use anyhow::{Context, Result};
use nerve_adapter::ModelAdapter;
use nerve_config::{
    Config, ConflictPolicy, GoalSpec, Orchestration, ProfileSelection, ReviewStrictness, Strategy,
};
use nerve_patch::{FileOperation, FilePatch, NvPatch};
use nerve_types::{
    AgentEvent, AgentOutput, CheckResult, Issue, IssueSeverity, ReviewerFeedback, RoundRecord,
    Task, UsageStats, Verdict,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::future::Future;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::SystemTime;
use tokio::sync::{RwLock, broadcast, mpsc};
use tokio::time::{Duration, sleep};

pub mod budget_audit;
pub mod goal;
pub mod goal_intent;
pub mod mayor_patrol;
pub mod plan;
pub mod rpc;
pub mod sandbox;
pub mod session_fork;
pub mod store;
pub mod ulimit;
pub mod verifier;
pub mod worktree;

pub use budget_audit::{
    AuditChainState, AuditError, BudgetAuditEntry, BudgetSnapshot, ChainStatus,
    append_budget_audit_entry, format_chain_broken,
};
pub use goal::{GoalError, GoalEvaluator};
pub use goal_intent::{GOAL_INTENT_SYSTEM_PROMPT, GoalIntentConverter, GoalIntentError};
pub use mayor_patrol::{
    DispatchFuture, Mayor, MayorError, MayorStatus, Patrol, PatrolResult, PatrolTask, PatrolVerdict,
};
pub use plan::{
    PLAN_ONLY_SYSTEM_PROMPT, PLAN_REVIEW_SYSTEM_PROMPT, PlanError, PlanRunOptions, PlanSections,
    run_plan_mode, validate_plan_markdown,
};
pub use rpc::{EmitError, EmitOutcome, RpcBus, RpcError};
pub use session_fork::{
    ForkConfig, ForkError, ForkOptions, SessionForker, SessionIndexEntry, SessionTree,
};
pub use store::{RunCheckpoint, RunStatus};
pub use ulimit::{UlimitError, apply_ulimit};
pub use verifier::{
    BUILTIN_VERIFIER_GOAL_ID, DetectedVerifier, PROJECT_VERIFIER_CONSENT_ENV, ResolvedVerifier,
    detect_builtin_verifier, project_verifier_consent_from_env, resolve_builtin_verifier,
};
pub use worktree::{IsolatedRound, OrphanManifestEntry, WorktreeError, WorktreeIsolator};

#[derive(Debug, Clone)]
pub struct Synapse {
    inner: Arc<RwLock<SynapseState>>,
    events: broadcast::Sender<AgentEvent>,
    /// S8: when set, each completed round is snapshotted to `.nerve/checkpoints`
    /// so a mid-loop crash leaves the rounds-so-far recoverable. `None` ⇒ no
    /// checkpointing, so every existing caller/test keeps its exact prior
    /// behavior (`Synapse::new`).
    checkpoint: Option<CheckpointSink>,
    /// S9: when set, each completed round is forwarded LIVE to this observer so
    /// a daemon can stream round seams as they happen instead of replaying from
    /// the final `RunReport`. Best-effort + unbounded: a send error (receiver
    /// dropped) is ignored and the send never blocks, so it can NEVER stall or
    /// abort the loop nor influence acceptance (read-only telemetry, same
    /// contract as the S8 checkpoint and the S7 progress signal).
    round_observer: Option<mpsc::UnboundedSender<RoundRecord>>,
}

/// Turns a [`Synapse::record_round`] call into an on-disk [`store::RunCheckpoint`].
///
/// Purely additive telemetry (S8 north star): a checkpoint carries no
/// acceptance fields and a write failure is logged and swallowed, so it can
/// never change which patch the deterministic gate accepts nor abort a
/// run — exactly the S7 "additive telemetry, never weakens the gate" contract.
#[derive(Debug, Clone)]
struct CheckpointSink {
    store: store::NerveStore,
    task: Task,
    selection: ProfileSelection,
}

#[derive(Debug, Clone)]
pub struct SynapseState {
    pub task: Task,
    pub lead_output: Option<AgentOutput>,
    pub reviewer_feedback: Option<ReviewerFeedback>,
    pub rounds: Vec<RoundRecord>,
    pub crossfire_feedback: Vec<ReviewerFeedback>,
    pub events: Vec<AgentEvent>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunReport {
    pub task: Task,
    pub selection: ProfileSelection,
    pub rounds: Vec<RoundRecord>,
    #[serde(default)]
    pub crossfire_feedback: Vec<ReviewerFeedback>,
    pub final_output: AgentOutput,
    pub final_feedback: ReviewerFeedback,
    pub final_patch: Option<NvPatch>,
    pub events: Vec<AgentEvent>,
    #[serde(default)]
    pub usage: UsageStats,
    #[serde(default)]
    pub budget_exceeded: bool,
    #[serde(default)]
    pub no_progress_exceeded: bool,
    #[serde(default)]
    pub goal_satisfied: Option<bool>,
    pub applied: bool,
    pub blocked: bool,
}

#[derive(Debug, Clone, Default)]
pub struct RunOptions {
    pub apply: bool,
    /// Optional deterministic check_cmd evaluated each round. AND-combined with
    /// reviewer verdict per §3 Tier 1b ma-2 / ma-6 decision table.
    pub goal: Option<GoalSpec>,
    /// sec-gap-5: optional parent-level resource limits applied via
    /// setrlimit(2) before the `/goal check_cmd` child execs. CLI populates this
    /// from `Orchestration.check_ulimit`; `None` leaves child limits unchanged.
    pub ulimit: Option<ulimit::CheckUlimit>,
    /// Tier 2d (v0.3.0): per-run override for worktree-isolated `/apply`.
    /// `None` defers to `Config.orchestration.worktree_apply`. `Some(true)`
    /// forces the worktree path; `Some(false)` forces the legacy path even if
    /// the config has it enabled (useful for tests and recovery).
    pub worktree: Option<bool>,
}

impl RunOptions {
    pub fn new(apply: bool) -> Self {
        Self {
            apply,
            goal: None,
            ulimit: None,
            worktree: None,
        }
    }

    pub fn with_goal(mut self, goal: GoalSpec) -> Self {
        self.goal = Some(goal);
        self
    }

    pub fn with_ulimit(mut self, ulimit: ulimit::CheckUlimit) -> Self {
        self.ulimit = Some(ulimit);
        self
    }

    /// Tier 2d override. RunOptions value wins over
    /// `Orchestration.worktree_apply` when set.
    pub fn with_worktree(mut self, on: bool) -> Self {
        self.worktree = Some(on);
        self
    }
}

/// Resolve whether to use the worktree-isolated apply path.
///
/// `RunOptions::worktree` takes precedence; if unset, fall back to
/// `Config.orchestration.worktree_apply`.
fn resolve_worktree_apply(options: &RunOptions, orchestration: &Orchestration) -> bool {
    options.worktree.unwrap_or(orchestration.worktree_apply)
}

impl Synapse {
    pub fn new(task: Task) -> Self {
        Self::build(task, None, None)
    }

    /// S8: a [`Synapse`] that checkpoints each completed round to
    /// `.nerve/checkpoints` via `store`. Used by the non-streaming production
    /// loop entry point; tests use [`Synapse::new`] to opt out.
    pub fn with_checkpoint(task: Task, store: store::NerveStore, selection: ProfileSelection) -> Self {
        Self::with_checkpoint_and_observer(task, store, selection, None)
    }

    /// S9: like [`Synapse::with_checkpoint`] but ALSO forwards each completed
    /// round live to `round_observer` (best-effort, see the field doc). Used by
    /// the streaming daemon entry point.
    pub fn with_checkpoint_and_observer(
        task: Task,
        store: store::NerveStore,
        selection: ProfileSelection,
        round_observer: Option<mpsc::UnboundedSender<RoundRecord>>,
    ) -> Self {
        let sink = CheckpointSink {
            store,
            task: task.clone(),
            selection,
        };
        Self::build(task, Some(sink), round_observer)
    }

    fn build(
        task: Task,
        checkpoint: Option<CheckpointSink>,
        round_observer: Option<mpsc::UnboundedSender<RoundRecord>>,
    ) -> Self {
        let (events, _) = broadcast::channel(256);
        Self {
            inner: Arc::new(RwLock::new(SynapseState {
                task,
                lead_output: None,
                reviewer_feedback: None,
                rounds: Vec::new(),
                crossfire_feedback: Vec::new(),
                events: Vec::new(),
            })),
            events,
            checkpoint,
            round_observer,
        }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<AgentEvent> {
        self.events.subscribe()
    }

    pub async fn record_event(&self, event: AgentEvent) {
        let _ = self.events.send(event.clone());
        self.inner.write().await.events.push(event);
    }

    pub async fn record_round(&self, round: RoundRecord) {
        // S9: clone the round for the live observer BEFORE it is moved into the
        // in-memory state (only when an observer is attached, so the no-stream
        // path pays nothing).
        let observer_round = self.round_observer.as_ref().map(|_| round.clone());

        // Mutate in-memory state and snapshot the rounds UNDER the lock, then
        // drop the guard before any (synchronous) disk I/O so we never hold the
        // async RwLock across a blocking write.
        let rounds = {
            let mut inner = self.inner.write().await;
            inner.lead_output = Some(round.lead.clone());
            inner.reviewer_feedback = Some(round.reviewer.clone());
            inner.rounds.push(round);
            inner.rounds.clone()
        };

        // S8: persist the in-flight checkpoint. Additive telemetry only — a
        // write failure is logged and swallowed (never aborts the loop, never
        // touches acceptance). Cadence is once per completed round (model-call
        // bounded), and the payload is small + atomic, so the inline sync write
        // is acceptable here; a dedicated writer task is a future S9 concern.
        if let Some(sink) = &self.checkpoint {
            let checkpoint = store::RunCheckpoint {
                task: sink.task.clone(),
                selection: sink.selection.clone(),
                status: store::RunStatus::Running,
                rounds,
                updated_at: chrono::Utc::now().to_rfc3339(),
            };
            if let Err(error) = sink.store.save_checkpoint(&checkpoint) {
                tracing::warn!(
                    target: "nerve::checkpoint",
                    task = %sink.task.id,
                    "round checkpoint write failed: {error:#}"
                );
            }
        }

        // S9: forward the completed round to the live observer. Best-effort:
        // the unbounded send never blocks, and a send error (the daemon's
        // receiver was dropped) is intentionally ignored — streaming is
        // read-only telemetry and must never stall or abort the loop.
        if let (Some(observer), Some(round)) = (&self.round_observer, observer_round) {
            let _ = observer.send(round);
        }
    }

    pub async fn record_crossfire_feedback(&self, feedback: ReviewerFeedback) {
        self.inner.write().await.crossfire_feedback.push(feedback);
    }

    pub async fn rounds(&self) -> Vec<RoundRecord> {
        self.inner.read().await.rounds.clone()
    }

    pub async fn crossfire_feedback(&self) -> Vec<ReviewerFeedback> {
        self.inner.read().await.crossfire_feedback.clone()
    }

    pub async fn events(&self) -> Vec<AgentEvent> {
        self.inner.read().await.events.clone()
    }
}

pub async fn run_synaptic_loop(
    task: Task,
    config: &Config,
    adapters: &[Box<dyn ModelAdapter>],
    options: RunOptions,
) -> Result<RunReport> {
    run_synaptic_loop_inner(task, config, adapters, options, None).await
}

/// S9: like [`run_synaptic_loop`] but forwards each completed round LIVE to
/// `round_observer` so a daemon can stream round seams as they happen instead
/// of replaying them from the final `RunReport`. The observer is read-only
/// telemetry (see [`Synapse`]'s `round_observer` field) — a send failure is
/// ignored and the send never blocks, so it can NEVER affect the deterministic
/// acceptance gate.
pub async fn run_synaptic_loop_streaming(
    task: Task,
    config: &Config,
    adapters: &[Box<dyn ModelAdapter>],
    options: RunOptions,
    round_observer: mpsc::UnboundedSender<RoundRecord>,
) -> Result<RunReport> {
    run_synaptic_loop_inner(task, config, adapters, options, Some(round_observer)).await
}

async fn run_synaptic_loop_inner(
    task: Task,
    config: &Config,
    adapters: &[Box<dyn ModelAdapter>],
    mut options: RunOptions,
    round_observer: Option<mpsc::UnboundedSender<RoundRecord>>,
) -> Result<RunReport> {
    // S4: when no explicit `/goal` is set, activate the opted-in built-in
    // verifier so the deterministic gate produces a real Pass/Fail instead of
    // Skipped. Done before the strategy branch so consensus and tournament both
    // inherit it, and before any goal is read downstream.
    apply_builtin_verifier(&mut options, &task, config);

    let selection = config.select_profile(&task)?;
    let lead = find_adapter(adapters, &selection.lead)?;
    let reviewer = find_adapter(adapters, &selection.reviewer)?;
    if matches!(config.orchestration.default_strategy, Strategy::Tournament) {
        return run_tournament_strategy(
            task,
            config,
            selection,
            lead,
            reviewer,
            options,
            round_observer,
        )
        .await;
    }

    let synapse = Synapse::with_checkpoint_and_observer(
        task.clone(),
        store::NerveStore::new(&task.cwd),
        selection.clone(),
        round_observer,
    );
    let (tx, mut rx) = mpsc::channel(1024);
    let event_synapse = synapse.clone();

    let event_task = tokio::spawn(async move {
        while let Some(event) = rx.recv().await {
            event_synapse.record_event(event).await;
        }
    });

    let mut lead_output = collect_output_with_crossfire(
        lead.implement(&task, &task.cwd, tx.clone()),
        reviewer,
        &task,
        &selection,
        &synapse,
        tx.clone(),
    )
    .await
    .with_context(|| format!("lead adapter `{}` failed", lead.id()))?;
    let mut usage = UsageStats::default();
    accumulate_output_usage(&mut usage, &lead_output);

    let mut final_feedback = ReviewerFeedback::lgtm(reviewer.id(), "review not run");
    let mut budget_exceeded = exceeds_budget(&usage, &config.orchestration);

    if budget_exceeded {
        final_feedback = budget_exceeded_feedback(reviewer.id(), &usage);
    }

    let max_refinement_rounds = match config.orchestration.default_strategy {
        Strategy::Consensus => selection.max_refinement_rounds,
        Strategy::Pipeline => 0,
        Strategy::Tournament => unreachable!("tournament strategy returns before consensus loop"),
    };

    let goal_evaluator = build_goal_evaluator(&task, &options, &config.orchestration)?;
    let no_progress_max = options
        .goal
        .as_ref()
        .and_then(|spec| spec.no_progress_max)
        .unwrap_or(0);
    let mut previous_patch_sha: Option<String> = None;
    let mut no_progress_count: u8 = 0;
    let mut no_progress_exceeded = false;
    let mut last_check: Option<CheckResult> = None;
    // S7: best deterministic-check pass-ratio (permille) seen across rounds, so
    // the no-progress guard can also trip when the lead keeps producing DIFFERENT
    // patches that never get closer to green.
    let mut best_progress: Option<u16> = None;

    for round_index in 0..=max_refinement_rounds {
        if budget_exceeded {
            break;
        }

        let feedback = reviewer
            .review(
                &task,
                &lead_output,
                &task.cwd,
                strictness_label(&selection.review_strictness),
                tx.clone(),
            )
            .await
            .with_context(|| format!("reviewer adapter `{}` failed", reviewer.id()))?;
        accumulate_feedback_usage(&mut usage, &feedback);

        let check_result = run_goal_check(goal_evaluator.as_ref()).await;
        last_check = Some(check_result.clone());
        let patch_sha = lead_output
            .proposed_patch
            .as_ref()
            .map(NvPatch::canonical_hash);

        let round = RoundRecord {
            round: round_index,
            lead: lead_output.clone(),
            reviewer: feedback.clone(),
            check_result: Some(check_result.clone()),
            patch_sha: patch_sha.clone(),
            envelope_id: None,
        };
        synapse.record_round(round).await;
        final_feedback = feedback;
        budget_exceeded = exceeds_budget(&usage, &config.orchestration);

        if budget_exceeded {
            final_feedback = budget_exceeded_feedback(reviewer.id(), &usage);
            break;
        }

        // ma-6 decision table: stop iff the reviewer accepts AND the
        // deterministic check is green. `Lgtm` accepts on Pass OR Skipped
        // (unchanged behavior). `AcceptWithNits` is the weaker "accept the
        // known nits" verdict, so it requires a REAL green check (Pass, never
        // Skipped) — it must never accept on reviewer opinion alone — and it
        // only terminates when strictness permits nits (High forces a round).
        let terminal = match &final_feedback.verdict {
            Verdict::Lgtm => matches!(check_result, CheckResult::Pass | CheckResult::Skipped),
            Verdict::AcceptWithNits => {
                selection.review_strictness.permits_nits()
                    && matches!(check_result, CheckResult::Pass)
            }
            _ => false,
        };
        if terminal {
            break;
        }

        if matches!(final_feedback.verdict, Verdict::Block) {
            break;
        }

        // no-progress guard (ma-1 + S7): a refinement round that fails to move
        // toward the goal. `round_is_stalled` trips on an identical patch hash
        // (the original guard) for a RequestChanges / strict-mode-stuck
        // AcceptWithNits round, OR — when the check exposes a measurable
        // pass-ratio — on a DIFFERENT patch that still doesn't beat the best
        // progress seen so far (a lead churning without getting closer to green).
        // This only ever ABORTS the loop (a give-up that blocks acceptance) — it
        // can never turn a non-accepting round into an acceptance.
        let round_progress = check_result_progress(&check_result);
        if no_progress_max > 0
            && matches!(
                final_feedback.verdict,
                Verdict::RequestChanges | Verdict::AcceptWithNits
            )
            && round_is_stalled(
                previous_patch_sha.as_deref(),
                patch_sha.as_deref(),
                best_progress,
                round_progress,
            )
        {
            no_progress_count = no_progress_count.saturating_add(1);
            if no_progress_count >= no_progress_max {
                no_progress_exceeded = true;
                final_feedback = no_progress_feedback(reviewer.id(), no_progress_count);
                break;
            }
        } else {
            no_progress_count = 0;
        }
        if let Some(p) = round_progress {
            best_progress = Some(best_progress.map_or(p, |b| b.max(p)));
        }
        previous_patch_sha = patch_sha;

        if round_index == max_refinement_rounds {
            break;
        }

        lead_output = collect_output_with_crossfire(
            lead.refine(&task, &lead_output, &final_feedback, &task.cwd, tx.clone()),
            reviewer,
            &task,
            &selection,
            &synapse,
            tx.clone(),
        )
        .await
        .with_context(|| format!("lead adapter `{}` failed during refinement", lead.id()))?;
        accumulate_output_usage(&mut usage, &lead_output);
        budget_exceeded = exceeds_budget(&usage, &config.orchestration);
        if budget_exceeded {
            final_feedback = budget_exceeded_feedback(reviewer.id(), &usage);
        }
    }

    drop(tx);
    event_task.await.context("event collector task failed")?;

    let final_patch = select_final_patch(
        &lead_output,
        &final_feedback,
        &config.orchestration.conflict_policy,
    )?;
    let goal_check_failed = goal_check_failed(&options.goal, last_check.as_ref());
    // Policy-independent verification gate for the weaker AcceptWithNits
    // verdict: it is a genuine acceptance only when strictness permits nits AND
    // the deterministic check really passed (Pass, never Skipped). Otherwise it
    // is blocked under EVERY conflict policy — lead_priority included — so a
    // nits verdict can never be auto-applied or persisted as accepted on
    // reviewer opinion alone.
    let nits_unverified = matches!(final_feedback.verdict, Verdict::AcceptWithNits)
        && !(selection.review_strictness.permits_nits()
            && matches!(last_check, Some(CheckResult::Pass)));
    let blocked = budget_exceeded
        || no_progress_exceeded
        || goal_check_failed
        || nits_unverified
        || is_blocked(&final_feedback, &config.orchestration.conflict_policy);

    let applied = apply_final_patch(
        &task,
        final_patch.as_ref(),
        options.apply && !blocked,
        resolve_worktree_apply(&options, &config.orchestration),
    )
    .await?;

    let goal_satisfied = options.goal.as_ref().map(|_| {
        matches!(
            last_check,
            Some(CheckResult::Pass) | Some(CheckResult::Skipped)
        ) && final_feedback
            .verdict
            .accepts_under(selection.review_strictness.permits_nits())
            && !budget_exceeded
            && !no_progress_exceeded
    });

    Ok(RunReport {
        task,
        selection,
        rounds: synapse.rounds().await,
        crossfire_feedback: synapse.crossfire_feedback().await,
        final_output: lead_output,
        final_feedback,
        final_patch,
        events: synapse.events().await,
        usage,
        budget_exceeded,
        no_progress_exceeded,
        goal_satisfied,
        applied,
        blocked,
    })
}

async fn run_tournament_strategy(
    task: Task,
    config: &Config,
    selection: ProfileSelection,
    lead: &dyn ModelAdapter,
    reviewer: &dyn ModelAdapter,
    options: RunOptions,
    round_observer: Option<mpsc::UnboundedSender<RoundRecord>>,
) -> Result<RunReport> {
    let synapse = Synapse::with_checkpoint_and_observer(
        task.clone(),
        store::NerveStore::new(&task.cwd),
        selection.clone(),
        round_observer,
    );
    let (tx, mut rx) = mpsc::channel(1024);
    let event_synapse = synapse.clone();

    let event_task = tokio::spawn(async move {
        while let Some(event) = rx.recv().await {
            event_synapse.record_event(event).await;
        }
    });

    let (lead_output, reviewer_output) = tokio::try_join!(
        lead.implement(&task, &task.cwd, tx.clone()),
        reviewer.implement(&task, &task.cwd, tx.clone())
    )
    .with_context(|| "tournament candidate generation failed")?;
    let mut usage = UsageStats::default();
    accumulate_output_usage(&mut usage, &lead_output);
    accumulate_output_usage(&mut usage, &reviewer_output);

    let mut budget_exceeded = exceeds_budget(&usage, &config.orchestration);
    let mut final_output = lead_output.clone();
    let final_feedback = if budget_exceeded {
        budget_exceeded_feedback(reviewer.id(), &usage)
    } else {
        let lead_review = reviewer
            .review(
                &task,
                &lead_output,
                &task.cwd,
                strictness_label(&selection.review_strictness),
                tx.clone(),
            )
            .await
            .with_context(|| format!("reviewer adapter `{}` failed", reviewer.id()))?;
        accumulate_feedback_usage(&mut usage, &lead_review);

        let reviewer_review = lead
            .review(
                &task,
                &reviewer_output,
                &task.cwd,
                strictness_label(&selection.review_strictness),
                tx.clone(),
            )
            .await
            .with_context(|| {
                format!(
                    "lead adapter `{}` failed during tournament review",
                    lead.id()
                )
            })?;
        accumulate_feedback_usage(&mut usage, &reviewer_review);

        budget_exceeded = exceeds_budget(&usage, &config.orchestration);
        // S1: accept-with-nits is intentionally not auto-selected as the
        // tournament winner — only LGTM auto-wins here.
        if budget_exceeded {
            budget_exceeded_feedback(reviewer.id(), &usage)
        } else if lead_review.verdict.is_terminal_success() {
            lead_review
        } else if reviewer_review.verdict.is_terminal_success()
            || matches!(
                config.orchestration.conflict_policy,
                ConflictPolicy::ReviewerPriority
            )
        {
            final_output = reviewer_output.clone();
            reviewer_review
        } else {
            lead_review
        }
    };

    // Tournament strategy only runs one round; AND-combine the optional goal check.
    let goal_evaluator = build_goal_evaluator(&task, &options, &config.orchestration)?;
    let check_result = run_goal_check(goal_evaluator.as_ref()).await;
    let patch_sha = final_output
        .proposed_patch
        .as_ref()
        .map(NvPatch::canonical_hash);

    let round = RoundRecord {
        round: 0,
        lead: final_output.clone(),
        reviewer: final_feedback.clone(),
        check_result: Some(check_result.clone()),
        patch_sha,
        envelope_id: None,
    };
    synapse.record_round(round).await;

    drop(tx);
    event_task.await.context("event collector task failed")?;

    let final_patch = select_final_patch(
        &final_output,
        &final_feedback,
        &config.orchestration.conflict_policy,
    )?;
    let goal_check_failed = goal_check_failed(&options.goal, Some(&check_result));
    // See run_synaptic_loop: AcceptWithNits needs a real Pass + nits-permitting
    // strictness to be a genuine acceptance, enforced policy-independently so it
    // is never auto-applied on reviewer opinion alone under any conflict policy.
    let nits_unverified = matches!(final_feedback.verdict, Verdict::AcceptWithNits)
        && !(selection.review_strictness.permits_nits()
            && matches!(check_result, CheckResult::Pass));
    let blocked = budget_exceeded
        || goal_check_failed
        || nits_unverified
        || is_blocked(&final_feedback, &config.orchestration.conflict_policy);

    let applied = apply_final_patch(
        &task,
        final_patch.as_ref(),
        options.apply && !blocked,
        resolve_worktree_apply(&options, &config.orchestration),
    )
    .await?;

    let goal_satisfied = options.goal.as_ref().map(|_| {
        matches!(check_result, CheckResult::Pass | CheckResult::Skipped)
            && final_feedback
                .verdict
                .accepts_under(selection.review_strictness.permits_nits())
            && !budget_exceeded
    });

    Ok(RunReport {
        task,
        selection,
        rounds: synapse.rounds().await,
        crossfire_feedback: synapse.crossfire_feedback().await,
        final_output,
        final_feedback,
        final_patch,
        events: synapse.events().await,
        usage,
        budget_exceeded,
        no_progress_exceeded: false,
        goal_satisfied,
        applied,
        blocked,
    })
}

/// S4: if no explicit `/goal` is configured, resolve the always-on built-in
/// verifier and install it as `options.goal` so every downstream gate
/// (`build_goal_evaluator`, `goal_check_failed`, `goal_satisfied`,
/// no-progress) treats it as a first-class deterministic check. An explicit
/// user `/goal` always wins and is left untouched.
fn apply_builtin_verifier(options: &mut RunOptions, task: &Task, config: &Config) {
    if options.goal.is_some() {
        return;
    }
    // S4 trust boundary (codex BLOCK): a project-local `nerve.config.json` must
    // not silently opt the operator into running repo code. Only operator-
    // controlled config — or explicit out-of-band consent — may enable the
    // executing Auto/Command modes.
    let consent = verifier::project_verifier_consent_from_env();
    let exec_trusted = config.builtin_verifier_exec_trusted(consent);
    let orchestration = &config.orchestration;
    match verifier::resolve_builtin_verifier(orchestration, &task.cwd, exec_trusted) {
        Some(resolved) => {
            tracing::info!(
                verifier = %resolved.label,
                "no /goal set; activating built-in verification gate"
            );
            options.goal = Some(resolved.spec);
        }
        None if !exec_trusted
            && !matches!(
                orchestration.builtin_verifier.mode,
                nerve_config::BuiltinVerifierMode::Off
            ) =>
        {
            // Project config asked for an executing verifier but the operator
            // has not consented — refuse and say so loudly (never silent).
            tracing::warn!(
                "project nerve.config.json requested an executing built-in verifier; \
                 ignored without operator consent (set {} or move the setting to the \
                 user config) — acceptance rests on the reviewer verdict alone",
                verifier::PROJECT_VERIFIER_CONSENT_ENV
            );
        }
        None => {}
    }
}

fn build_goal_evaluator(
    task: &Task,
    options: &RunOptions,
    orchestration: &Orchestration,
) -> Result<Option<GoalEvaluator>> {
    let Some(spec) = options.goal.clone() else {
        return Ok(None);
    };
    let cwd = spec.cwd.clone().unwrap_or_else(|| task.cwd.clone());
    let evaluator = GoalEvaluator::with_options(
        spec,
        orchestration.check_env.clone(),
        orchestration.check_output_cap_bytes,
        cwd,
        options.ulimit.clone(),
        orchestration.sandbox,
    )
    .map_err(|err| anyhow::anyhow!("goal evaluator setup failed: {err}"))?;
    Ok(Some(evaluator))
}

async fn run_goal_check(evaluator: Option<&GoalEvaluator>) -> CheckResult {
    match evaluator {
        Some(eval) => eval.evaluate().await,
        None => CheckResult::Skipped,
    }
}

fn goal_check_failed(goal: &Option<GoalSpec>, check_result: Option<&CheckResult>) -> bool {
    goal.is_some() && !matches!(check_result, Some(CheckResult::Pass | CheckResult::Skipped))
}

/// The deterministic check's pass-ratio in permille for the no-progress guard:
/// `Pass` is fully satisfied (1000), `Skipped` carries no signal, and `Fail`
/// reports the parsed ratio when the check exposed a recognizable test summary
/// (S7). Used only for stall detection, never for the acceptance gate.
fn check_result_progress(check: &CheckResult) -> Option<u16> {
    match check {
        CheckResult::Pass => Some(1000),
        CheckResult::Skipped => None,
        CheckResult::Fail { progress, .. } => *progress,
    }
}

/// Whether a refinement round failed to move the loop toward its goal (S7).
///
/// Stalled when the lead re-submitted the SAME patch as the previous round (the
/// original ma-1 guard) OR — when the check exposes a measurable pass-ratio — a
/// DIFFERENT patch still did not beat the best progress seen so far (`best`
/// reflects rounds strictly before this one). The progress dimension is purely
/// additive: with no comparable ratio it reduces to the original identical-hash
/// behaviour, and a missing hash on either side never counts as a stall.
fn round_is_stalled(
    prev_sha: Option<&str>,
    cur_sha: Option<&str>,
    best: Option<u16>,
    current: Option<u16>,
) -> bool {
    match (prev_sha, cur_sha) {
        (Some(prev), Some(cur)) if prev == cur => true,
        (Some(_), Some(_)) => matches!((current, best), (Some(now), Some(b)) if now <= b),
        _ => false,
    }
}

fn no_progress_feedback(reviewer_id: &str, count: u8) -> ReviewerFeedback {
    ReviewerFeedback {
        reviewer_id: reviewer_id.to_string(),
        verdict: Verdict::Block,
        issues: vec![Issue {
            severity: IssueSeverity::Blocking,
            message: format!(
                "No progress for {count} consecutive rounds (no closer to a green check)"
            ),
        }],
        suggested_patch: None,
        cost: None,
        raw_text: format!("BLOCK: no-progress exceeded after {count} stalled rounds"),
    }
}

fn accumulate_output_usage(total: &mut UsageStats, output: &AgentOutput) {
    if let Some(cost) = &output.cost {
        total.add_assign(cost);
    }
}

fn accumulate_feedback_usage(total: &mut UsageStats, feedback: &ReviewerFeedback) {
    if let Some(cost) = &feedback.cost {
        total.add_assign(cost);
    }
}

fn exceeds_budget(usage: &UsageStats, orchestration: &Orchestration) -> bool {
    orchestration
        .max_total_tokens
        .is_some_and(|limit| usage.total_tokens() > limit)
        || orchestration
            .max_estimated_cost_microusd
            .is_some_and(|limit| {
                usage
                    .estimated_cost_microusd
                    .is_some_and(|cost| cost > limit)
            })
}

fn budget_exceeded_feedback(reviewer_id: &str, usage: &UsageStats) -> ReviewerFeedback {
    ReviewerFeedback {
        reviewer_id: reviewer_id.to_string(),
        verdict: Verdict::Block,
        issues: vec![Issue {
            severity: IssueSeverity::Blocking,
            message: format!(
                "Session budget exceeded: {} total tokens",
                usage.total_tokens()
            ),
        }],
        suggested_patch: None,
        cost: None,
        raw_text: format!(
            "BLOCK: session budget exceeded after {} total tokens",
            usage.total_tokens()
        ),
    }
}

async fn collect_output_with_crossfire<F>(
    output_future: F,
    reviewer: &dyn ModelAdapter,
    task: &Task,
    selection: &ProfileSelection,
    synapse: &Synapse,
    tx: mpsc::Sender<AgentEvent>,
) -> Result<AgentOutput>
where
    F: Future<Output = Result<AgentOutput>>,
{
    let mut watcher = ScratchWatcher::new(task.cwd.join(".nerve/scratch"))?;
    tokio::pin!(output_future);

    loop {
        tokio::select! {
            output = &mut output_future => return output,
            change = watcher.next_change() => {
                if let Some(summary) = change? {
                    let feedback = reviewer
                        .crossfire(
                            task,
                            &summary,
                            &task.cwd,
                            strictness_label(&selection.review_strictness),
                            tx.clone(),
                        )
                        .await?;
                    synapse.record_crossfire_feedback(feedback).await;
                }
            }
        }
    }
}

#[derive(Debug)]
struct ScratchWatcher {
    root: PathBuf,
    known: BTreeMap<PathBuf, SystemTime>,
}

impl ScratchWatcher {
    fn new(root: PathBuf) -> Result<Self> {
        std::fs::create_dir_all(&root)
            .with_context(|| format!("failed to create scratch dir `{}`", root.display()))?;
        let known = scan_scratch_files(&root)?;
        Ok(Self { root, known })
    }

    async fn next_change(&mut self) -> Result<Option<String>> {
        sleep(Duration::from_millis(100)).await;
        let current = scan_scratch_files(&self.root)?;
        let changed = current.iter().find_map(|(path, modified)| {
            let previous = self.known.get(path);
            if previous.is_none_or(|previous| previous < modified) {
                Some(path.clone())
            } else {
                None
            }
        });
        self.known = current;

        let Some(path) = changed else {
            return Ok(None);
        };
        let contents = std::fs::read_to_string(&path).unwrap_or_default();
        let relative = path.strip_prefix(&self.root).unwrap_or(&path);
        Ok(Some(format!(
            "scratch changed: {}\n{}",
            relative.display(),
            truncate_for_crossfire(&contents)
        )))
    }
}

fn scan_scratch_files(root: &Path) -> Result<BTreeMap<PathBuf, SystemTime>> {
    let mut files = BTreeMap::new();
    scan_scratch_files_inner(root, &mut files)?;
    Ok(files)
}

fn scan_scratch_files_inner(root: &Path, files: &mut BTreeMap<PathBuf, SystemTime>) -> Result<()> {
    if !root.exists() {
        return Ok(());
    }

    for entry in std::fs::read_dir(root)
        .with_context(|| format!("failed to read scratch dir `{}`", root.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        let metadata = entry.metadata()?;
        if metadata.is_dir() {
            scan_scratch_files_inner(&path, files)?;
        } else if metadata.is_file() {
            let modified = metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH);
            files.insert(path, modified);
        }
    }

    Ok(())
}

fn truncate_for_crossfire(contents: &str) -> String {
    const LIMIT: usize = 8192;
    if contents.len() <= LIMIT {
        return contents.to_string();
    }
    format!("{}...", truncate_at_char_boundary(contents, LIMIT))
}

fn truncate_at_char_boundary(value: &str, limit: usize) -> &str {
    let mut boundary = limit.min(value.len());
    while !value.is_char_boundary(boundary) {
        boundary -= 1;
    }
    &value[..boundary]
}

fn find_adapter<'a>(
    adapters: &'a [Box<dyn ModelAdapter>],
    id: &str,
) -> Result<&'a dyn ModelAdapter> {
    adapters
        .iter()
        .find(|adapter| adapter.id() == id)
        .map(|adapter| adapter.as_ref())
        .with_context(|| format!("no adapter registered for `{id}`"))
}

fn strictness_label(strictness: &ReviewStrictness) -> &'static str {
    match strictness {
        ReviewStrictness::Low => "low",
        ReviewStrictness::Normal => "normal",
        ReviewStrictness::High => "high",
    }
}

/// Apply the final selected patch.
///
/// v0.3.0 Tier 2d simplification: only the *final* patch is funneled through
/// the worktree isolator. Round-by-round isolation is deferred to v0.5.0.
///
/// Behaviour:
/// - `should_apply == false` → returns `Ok(false)`; legacy.
/// - `should_apply && patch.is_none()` → returns `Ok(false)`; nothing to do.
/// - `should_apply && !use_worktree` → legacy in-place apply against
///   `task.cwd`.
/// - `should_apply && use_worktree` → prepare a worktree round, apply the
///   patch inside, merge on success. On any worktree error, discard the
///   round and propagate the error so the caller (`run_synaptic_loop`) can
///   surface it.
async fn apply_final_patch(
    task: &Task,
    final_patch: Option<&NvPatch>,
    should_apply: bool,
    use_worktree: bool,
) -> Result<bool> {
    if !should_apply {
        return Ok(false);
    }
    let Some(patch) = final_patch else {
        return Ok(false);
    };
    if !use_worktree {
        patch.apply(&task.cwd, false)?;
        return Ok(true);
    }

    let isolator = WorktreeIsolator::new(task.cwd.clone())
        .map_err(|err| anyhow::anyhow!("worktree isolator setup failed: {err}"))?;
    let round = isolator
        .prepare_round(0)
        .map_err(|err| anyhow::anyhow!("worktree prepare failed: {err}"))?;
    if let Err(err) = isolator.apply_patch_in_worktree(&round, patch).await {
        // discard returns Ok unless rewind fails catastrophically; we still
        // surface the original apply error.
        let _ = isolator.discard_round(round);
        return Err(anyhow::anyhow!("worktree patch apply failed: {err}"));
    }

    // Commit the patch inside the worktree so the merge target has a tip
    // to fast-forward onto. We use `-q` + a generated message and rely on
    // the per-worktree HEAD; falling back gracefully if `git commit`
    // reports "nothing to commit" (no-op patch).
    if let Err(err) = commit_worktree_round(&round.worktree_path) {
        let _ = isolator.discard_round(round);
        return Err(anyhow::anyhow!("worktree commit failed: {err}"));
    }

    match isolator.merge_round(round.clone()) {
        Ok(()) => Ok(true),
        Err(err) => {
            // merge_round already rewinds main; nothing else to do.
            Err(anyhow::anyhow!("worktree merge failed: {err}"))
        }
    }
}

fn commit_worktree_round(worktree_path: &Path) -> Result<()> {
    let add = Command::new("git")
        .args(["add", "-A"])
        .current_dir(worktree_path)
        .output()
        .context("failed to spawn `git add` in worktree")?;
    if !add.status.success() {
        anyhow::bail!(
            "`git add` in worktree failed: {}",
            String::from_utf8_lossy(&add.stderr).trim()
        );
    }

    let commit = Command::new("git")
        .args([
            "commit",
            "-q",
            "-m",
            "nerve worktree round commit",
            "--allow-empty",
        ])
        .current_dir(worktree_path)
        .output()
        .context("failed to spawn `git commit` in worktree")?;
    if !commit.status.success() {
        anyhow::bail!(
            "`git commit` in worktree failed: {}",
            String::from_utf8_lossy(&commit.stderr).trim()
        );
    }
    Ok(())
}

fn select_final_patch(
    lead_output: &AgentOutput,
    feedback: &ReviewerFeedback,
    policy: &ConflictPolicy,
) -> Result<Option<NvPatch>> {
    match policy {
        ConflictPolicy::LeadPriority
        | ConflictPolicy::AbortOnConflict
        | ConflictPolicy::ReviewerBlock
        | ConflictPolicy::Manual => Ok(lead_output.proposed_patch.clone()),
        ConflictPolicy::ReviewerPriority => Ok(feedback
            .suggested_patch
            .clone()
            .or_else(|| lead_output.proposed_patch.clone())),
        ConflictPolicy::MergeAttempt => merge_patches(
            lead_output.proposed_patch.as_ref(),
            feedback.suggested_patch.as_ref(),
        )
        .context("failed to merge lead and reviewer patches"),
    }
}

fn merge_patches(lead: Option<&NvPatch>, reviewer: Option<&NvPatch>) -> Result<Option<NvPatch>> {
    let Some(lead) = lead else {
        return Ok(reviewer.cloned());
    };
    let Some(reviewer) = reviewer else {
        return Ok(Some(lead.clone()));
    };

    let mut files = lead.files.clone();
    for reviewer_file in &reviewer.files {
        let Some(index) = files
            .iter()
            .position(|lead_file| lead_file.path == reviewer_file.path)
        else {
            files.push(reviewer_file.clone());
            continue;
        };

        if files[index] == *reviewer_file {
            continue;
        }

        files[index] = merge_file_patch(&files[index], reviewer_file)?;
    }

    Ok(Some(NvPatch::new(files)))
}

fn merge_file_patch(lead: &FilePatch, reviewer: &FilePatch) -> Result<FilePatch> {
    if lead.operation != reviewer.operation {
        anyhow::bail!(
            "cannot merge different operations for `{}`",
            lead.path.display()
        );
    }
    if lead.original_sha256 != reviewer.original_sha256 {
        anyhow::bail!(
            "cannot merge patches with different bases for `{}`",
            lead.path.display()
        );
    }
    if !matches!(
        lead.operation,
        FileOperation::Modify | FileOperation::Create
    ) {
        anyhow::bail!(
            "merge_attempt only supports modify/create conflicts, got {:?} for `{}`",
            lead.operation,
            lead.path.display()
        );
    }

    let merged = git_merge_file(&lead.original, &lead.modified, &reviewer.modified)
        .with_context(|| format!("git merge-file failed for `{}`", lead.path.display()))?;
    Ok(FilePatch::with_operation(
        lead.path.clone(),
        lead.operation.clone(),
        lead.original.clone(),
        merged,
    ))
}

fn git_merge_file(base: &str, lead: &str, reviewer: &str) -> Result<String> {
    let mut base_file = tempfile::NamedTempFile::new()?;
    let mut lead_file = tempfile::NamedTempFile::new()?;
    let mut reviewer_file = tempfile::NamedTempFile::new()?;
    base_file.write_all(base.as_bytes())?;
    lead_file.write_all(lead.as_bytes())?;
    reviewer_file.write_all(reviewer.as_bytes())?;

    let output = Command::new("git")
        .arg("merge-file")
        .arg("-p")
        .arg(lead_file.path())
        .arg(base_file.path())
        .arg(reviewer_file.path())
        .output()
        .context("failed to run `git merge-file`")?;

    match output.status.code() {
        Some(0) => {}
        Some(code) if (1..128).contains(&code) => {}
        _ => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!(
                "`git merge-file` failed with status {}: {}",
                output.status,
                stderr.trim()
            );
        }
    }

    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

fn is_blocked(feedback: &ReviewerFeedback, policy: &ConflictPolicy) -> bool {
    match policy {
        ConflictPolicy::LeadPriority | ConflictPolicy::ReviewerPriority => false,
        // Conflict-resolution axis only. `AcceptWithNits` is treated as an
        // accept-class verdict here; whether it is a GENUINE acceptance (real
        // Pass check + nits-permitting strictness) is enforced separately and
        // policy-independently via the `nits_unverified` gate in the blocked
        // chain, so it can never be auto-applied on opinion alone under ANY
        // policy (including the reviewer-advisory lead_priority).
        ConflictPolicy::AbortOnConflict => {
            !matches!(feedback.verdict, Verdict::Lgtm | Verdict::AcceptWithNits)
        }
        ConflictPolicy::MergeAttempt | ConflictPolicy::ReviewerBlock => {
            feedback.verdict == Verdict::Block
        }
        ConflictPolicy::Manual => true,
    }
}

/// Tier 2d / sec-gap-12 readiness signal returned by [`doctor_checks`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DoctorCheck {
    pub name: String,
    pub status: DoctorStatus,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DoctorStatus {
    Ok,
    Warn(String),
    Fail(String),
}

/// Compute the v0.3.0 doctor signal set:
///
/// 1. `git` executable on PATH (Tier 2d guard #4)
/// 2. statvfs of `cwd/.nerve` above 100 MiB (Tier 2d guard #6)
/// 3. orphaned-worktrees count (sec-5 #7 quarantine bin)
/// 4. budget-audit hash chain integrity (sec-gap-12)
/// 5. active-goal.json validity (Phase 2 /goal)
///
/// The CLI doctor command is wired in a follow-up phase; this helper exists
/// so the doctor signal set is owned by `nerve-core` and stays consistent
/// with the runtime guards above.
pub fn doctor_checks(_config: &Config, cwd: &Path) -> Vec<DoctorCheck> {
    let mut checks = Vec::new();

    // 1. git --version
    let status = match Command::new("git").arg("--version").output() {
        Ok(out) if out.status.success() => DoctorStatus::Ok,
        Ok(out) => DoctorStatus::Fail(format!(
            "`git --version` exited with status {}: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        )),
        Err(err) => DoctorStatus::Fail(format!("git not found on PATH: {err}")),
    };
    checks.push(DoctorCheck {
        name: "git".to_string(),
        status,
    });

    // 2. statvfs of cwd/.nerve (100 MiB threshold = worktree default)
    let nerve_root = cwd.join(".nerve");
    let probe = if nerve_root.exists() {
        nerve_root.clone()
    } else {
        cwd.to_path_buf()
    };
    let status = match worktree::available_mib_for_doctor(&probe) {
        Ok(available) if available >= 100 => DoctorStatus::Ok,
        Ok(available) => DoctorStatus::Warn(format!(
            "only {available} MiB free under `{}` (threshold 100 MiB)",
            probe.display()
        )),
        Err(err) => DoctorStatus::Warn(format!("statvfs failed: {err}")),
    };
    checks.push(DoctorCheck {
        name: "disk_space".to_string(),
        status,
    });

    // 3. orphaned-worktrees count
    let orphan_dir = cwd
        .join(".nerve")
        .join("scratch")
        .join("orphaned-worktrees");
    let status = match orphan_entry_count(&orphan_dir) {
        Ok(0) => DoctorStatus::Ok,
        Ok(n) => DoctorStatus::Warn(format!(
            "{n} quarantined worktree(s) under `{}`",
            orphan_dir.display()
        )),
        Err(err) => DoctorStatus::Warn(format!("could not read `{}`: {err}", orphan_dir.display())),
    };
    checks.push(DoctorCheck {
        name: "orphaned_worktrees".to_string(),
        status,
    });

    // 4. budget-audit chain integrity
    let audit_path = cwd
        .join(".nerve")
        .join("session-meta")
        .join("budget-audit.json");
    let status = match AuditChainState::verify(&audit_path) {
        Ok(ChainStatus::Empty) => DoctorStatus::Ok,
        Ok(ChainStatus::Intact { entries, .. }) => {
            tracing::debug!(entries, "audit chain intact");
            DoctorStatus::Ok
        }
        Ok(status @ ChainStatus::Broken { .. }) => {
            let msg = format_chain_broken(&status).unwrap_or_else(|| "chain broken".to_string());
            DoctorStatus::Fail(msg)
        }
        Err(err) => DoctorStatus::Warn(format!("failed to verify audit chain: {err}")),
    };
    checks.push(DoctorCheck {
        name: "budget_audit_chain".to_string(),
        status,
    });

    // 5. active-goal.json validity
    let goal_path = cwd
        .join(".nerve")
        .join("session-meta")
        .join("active-goal.json");
    let status = if !goal_path.exists() {
        DoctorStatus::Ok
    } else {
        match std::fs::read_to_string(&goal_path) {
            Ok(raw) if raw.trim().is_empty() => DoctorStatus::Ok,
            Ok(raw) => match serde_json::from_str::<GoalSpec>(&raw) {
                Ok(spec) => match spec.validate() {
                    Ok(()) => DoctorStatus::Ok,
                    Err(err) => DoctorStatus::Fail(format!("invalid active goal: {err}")),
                },
                Err(err) => DoctorStatus::Fail(format!("active-goal.json parse error: {err}")),
            },
            Err(err) => {
                DoctorStatus::Warn(format!("failed to read `{}`: {err}", goal_path.display()))
            }
        }
    };
    checks.push(DoctorCheck {
        name: "active_goal".to_string(),
        status,
    });

    checks
}

fn orphan_entry_count(dir: &Path) -> std::io::Result<usize> {
    if !dir.exists() {
        return Ok(0);
    }
    let mut count = 0usize;
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        // Skip the manifest.jsonl bookkeeping file itself.
        if entry.file_name() == "manifest.jsonl" {
            continue;
        }
        count += 1;
    }
    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;
    use nerve_adapter::MockAdapter;
    use nerve_config::Config;
    use std::path::Path;
    use tokio::sync::mpsc;

    #[tokio::test]
    async fn mock_loop_refines_until_lgtm() {
        let dir = tempfile::tempdir().unwrap();
        let task = Task::new("add a health endpoint", dir.path());
        let config = Config::from_json_str(
            r#"{
              "orchestration": {
                "default_strategy": "consensus",
                "max_refinement_rounds": 2,
                "conflict_policy": "lead_priority"
              },
              "roles": {
                "architect": "claude-code",
                "reviewer": "codex"
              },
              "profiles": []
            }"#,
        )
        .unwrap();
        let adapters = vec![
            Box::new(MockAdapter::lead()) as Box<dyn ModelAdapter>,
            Box::new(MockAdapter::reviewer()) as Box<dyn ModelAdapter>,
        ];

        let report = run_synaptic_loop(task, &config, &adapters, RunOptions::new(false))
            .await
            .unwrap();

        assert_eq!(report.final_feedback.verdict, Verdict::Lgtm);
        assert_eq!(report.rounds.len(), 2);
        assert!(report.final_patch.is_some());
        assert!(!report.applied);
    }

    #[tokio::test]
    async fn token_budget_blocks_before_review_when_lead_exceeds_limit() {
        let dir = tempfile::tempdir().unwrap();
        let task = Task::new("expensive task", dir.path());
        let config = Config::from_json_str(
            r#"{
              "orchestration": {
                "default_strategy": "consensus",
                "max_refinement_rounds": 2,
                "conflict_policy": "lead_priority",
                "max_total_tokens": 10
              },
              "roles": {
                "architect": "budget-lead",
                "reviewer": "budget-reviewer"
              },
              "profiles": []
            }"#,
        )
        .unwrap();
        let adapters = vec![
            Box::new(BudgetAdapter::new("budget-lead", 7, 4)) as Box<dyn ModelAdapter>,
            Box::new(BudgetAdapter::new("budget-reviewer", 1, 1)) as Box<dyn ModelAdapter>,
        ];

        let report = run_synaptic_loop(task, &config, &adapters, RunOptions::new(true))
            .await
            .unwrap();

        assert!(report.budget_exceeded);
        assert!(report.blocked);
        assert!(!report.applied);
        assert_eq!(report.final_feedback.verdict, Verdict::Block);
        assert_eq!(report.usage.total_tokens(), 11);
        assert!(report.rounds.is_empty());
    }

    #[tokio::test]
    async fn pipeline_strategy_runs_single_review_without_refinement() {
        let dir = tempfile::tempdir().unwrap();
        let task = Task::new("add a health endpoint", dir.path());
        let config = Config::from_json_str(
            r#"{
              "orchestration": {
                "default_strategy": "pipeline",
                "max_refinement_rounds": 2,
                "conflict_policy": "lead_priority"
              },
              "roles": {
                "architect": "claude-code",
                "reviewer": "codex"
              },
              "profiles": []
            }"#,
        )
        .unwrap();
        let adapters = vec![
            Box::new(MockAdapter::lead()) as Box<dyn ModelAdapter>,
            Box::new(MockAdapter::reviewer()) as Box<dyn ModelAdapter>,
        ];

        let report = run_synaptic_loop(task, &config, &adapters, RunOptions::new(false))
            .await
            .unwrap();

        assert_eq!(report.final_feedback.verdict, Verdict::RequestChanges);
        assert_eq!(report.rounds.len(), 1);
        assert_eq!(report.final_output.raw_text, "Initial mock implementation");
    }

    #[tokio::test]
    async fn tournament_strategy_can_select_reviewer_candidate() {
        let dir = tempfile::tempdir().unwrap();
        let task = Task::new("choose best candidate", dir.path());
        let config = Config::from_json_str(
            r#"{
              "orchestration": {
                "default_strategy": "tournament",
                "max_refinement_rounds": 2,
                "conflict_policy": "lead_priority"
              },
              "roles": {
                "architect": "candidate-a",
                "reviewer": "candidate-b"
              },
              "profiles": []
            }"#,
        )
        .unwrap();
        let adapters = vec![
            Box::new(TournamentAdapter::new("candidate-a")) as Box<dyn ModelAdapter>,
            Box::new(TournamentAdapter::new("candidate-b")) as Box<dyn ModelAdapter>,
        ];

        let report = run_synaptic_loop(task, &config, &adapters, RunOptions::new(false))
            .await
            .unwrap();

        assert_eq!(report.final_feedback.verdict, Verdict::Lgtm);
        assert_eq!(report.final_output.agent_id, "candidate-b");
        assert_eq!(report.rounds.len(), 1);
    }

    #[tokio::test]
    async fn crossfire_records_scratch_feedback_during_lead_run() {
        let dir = tempfile::tempdir().unwrap();
        let task = Task::new("watch scratch", dir.path());
        let config = Config::from_json_str(
            r#"{
              "orchestration": {
                "default_strategy": "consensus",
                "max_refinement_rounds": 1,
                "conflict_policy": "lead_priority"
              },
              "roles": {
                "architect": "scratch-lead",
                "reviewer": "scratch-reviewer"
              },
              "profiles": []
            }"#,
        )
        .unwrap();
        let adapters = vec![
            Box::new(ScratchLeadAdapter) as Box<dyn ModelAdapter>,
            Box::new(ScratchReviewerAdapter) as Box<dyn ModelAdapter>,
        ];

        let report = run_synaptic_loop(task, &config, &adapters, RunOptions::new(false))
            .await
            .unwrap();

        assert_eq!(report.final_feedback.verdict, Verdict::Lgtm);
        assert_eq!(report.crossfire_feedback.len(), 1);
        assert!(
            report.crossfire_feedback[0]
                .raw_text
                .contains("scratch changed")
        );
    }

    #[test]
    fn merge_attempt_combines_non_overlapping_patch_files() {
        let lead = AgentOutput::with_patch(
            "lead",
            "lead",
            NvPatch::new(vec![FilePatch::new("a.txt", "old\n", "lead\n")]),
        );
        let feedback = ReviewerFeedback {
            reviewer_id: "reviewer".to_string(),
            verdict: Verdict::Lgtm,
            issues: Vec::new(),
            suggested_patch: Some(NvPatch::new(vec![FilePatch::new(
                "b.txt",
                "old\n",
                "reviewer\n",
            )])),
            cost: None,
            raw_text: "LGTM".to_string(),
        };

        let patch = select_final_patch(&lead, &feedback, &ConflictPolicy::MergeAttempt)
            .unwrap()
            .unwrap();

        assert_eq!(patch.files.len(), 2);
        assert_eq!(patch.files[0].path, Path::new("a.txt"));
        assert_eq!(patch.files[1].path, Path::new("b.txt"));
    }

    #[test]
    fn merge_attempt_uses_git_merge_file_for_same_file_changes() {
        let lead = AgentOutput::with_patch(
            "lead",
            "lead",
            NvPatch::new(vec![FilePatch::new(
                "file.txt",
                "one\ntwo\nthree\n",
                "ONE\ntwo\nthree\n",
            )]),
        );
        let feedback = ReviewerFeedback {
            reviewer_id: "reviewer".to_string(),
            verdict: Verdict::Lgtm,
            issues: Vec::new(),
            suggested_patch: Some(NvPatch::new(vec![FilePatch::new(
                "file.txt",
                "one\ntwo\nthree\n",
                "one\ntwo\nTHREE\n",
            )])),
            cost: None,
            raw_text: "LGTM".to_string(),
        };

        let patch = select_final_patch(&lead, &feedback, &ConflictPolicy::MergeAttempt)
            .unwrap()
            .unwrap();

        assert_eq!(patch.files.len(), 1);
        assert_eq!(patch.files[0].modified, "ONE\ntwo\nTHREE\n");
    }

    #[test]
    fn merge_attempt_preserves_git_conflict_markers() {
        let lead = AgentOutput::with_patch(
            "lead",
            "lead",
            NvPatch::new(vec![FilePatch::new("file.txt", "one\ntwo\n", "ONE\ntwo\n")]),
        );
        let feedback = ReviewerFeedback {
            reviewer_id: "reviewer".to_string(),
            verdict: Verdict::Lgtm,
            issues: Vec::new(),
            suggested_patch: Some(NvPatch::new(vec![FilePatch::new(
                "file.txt",
                "one\ntwo\n",
                "TWO\ntwo\n",
            )])),
            cost: None,
            raw_text: "LGTM".to_string(),
        };

        let patch = select_final_patch(&lead, &feedback, &ConflictPolicy::MergeAttempt)
            .unwrap()
            .unwrap();

        assert!(patch.files[0].modified.contains("<<<<<<<"));
        assert!(patch.files[0].modified.contains(">>>>>>>"));
    }

    #[test]
    fn crossfire_truncation_preserves_utf8_boundaries() {
        let mut value = "a".repeat(8191);
        value.push_str("한글");

        let truncated = truncate_for_crossfire(&value);

        assert!(truncated.ends_with("..."));
        assert!(truncated.is_char_boundary(truncated.len()));
    }

    #[tokio::test]
    async fn max_refinement_rounds_counts_lead_refinements_not_reviews() {
        let dir = tempfile::tempdir().unwrap();
        let task = Task::new("never accepted", dir.path());
        let config = Config::from_json_str(
            r#"{
              "orchestration": {
                "default_strategy": "consensus",
                "max_refinement_rounds": 2,
                "conflict_policy": "lead_priority"
              },
              "roles": {
                "architect": "stubborn-lead",
                "reviewer": "stubborn-reviewer"
              },
              "profiles": []
            }"#,
        )
        .unwrap();
        let adapters = vec![
            Box::new(StubbornAdapter::new("stubborn-lead")) as Box<dyn ModelAdapter>,
            Box::new(StubbornAdapter::new("stubborn-reviewer")) as Box<dyn ModelAdapter>,
        ];

        let report = run_synaptic_loop(task, &config, &adapters, RunOptions::new(false))
            .await
            .unwrap();

        assert_eq!(report.rounds.len(), 3);
        assert_eq!(report.final_output.raw_text, "refinement 2");
    }

    #[tokio::test]
    async fn abort_on_conflict_blocks_request_changes_apply() {
        let dir = tempfile::tempdir().unwrap();
        let task = Task::new("add a health endpoint", dir.path());
        let config = Config::from_json_str(
            r#"{
              "orchestration": {
                "default_strategy": "pipeline",
                "max_refinement_rounds": 2,
                "conflict_policy": "abort_on_conflict"
              },
              "roles": {
                "architect": "claude-code",
                "reviewer": "codex"
              },
              "profiles": []
            }"#,
        )
        .unwrap();
        let adapters = vec![
            Box::new(MockAdapter::lead()) as Box<dyn ModelAdapter>,
            Box::new(MockAdapter::reviewer()) as Box<dyn ModelAdapter>,
        ];

        let report = run_synaptic_loop(task, &config, &adapters, RunOptions::new(true))
            .await
            .unwrap();

        assert!(report.blocked);
        assert!(!report.applied);
        assert!(!dir.path().join("mock-output.txt").exists());
    }

    #[tokio::test]
    async fn manual_policy_never_auto_applies() {
        let dir = tempfile::tempdir().unwrap();
        let task = Task::new("add a health endpoint", dir.path());
        let config = Config::from_json_str(
            r#"{
              "orchestration": {
                "default_strategy": "consensus",
                "max_refinement_rounds": 2,
                "conflict_policy": "manual"
              },
              "roles": {
                "architect": "claude-code",
                "reviewer": "codex"
              },
              "profiles": []
            }"#,
        )
        .unwrap();
        let adapters = vec![
            Box::new(MockAdapter::lead()) as Box<dyn ModelAdapter>,
            Box::new(MockAdapter::reviewer()) as Box<dyn ModelAdapter>,
        ];

        let report = run_synaptic_loop(task, &config, &adapters, RunOptions::new(true))
            .await
            .unwrap();

        assert_eq!(report.final_feedback.verdict, Verdict::Lgtm);
        assert!(report.blocked);
        assert!(!report.applied);
        assert!(!dir.path().join("mock-output.txt").exists());
    }

    #[derive(Debug)]
    struct BudgetAdapter {
        id: &'static str,
        input_tokens: u64,
        output_tokens: u64,
    }

    impl BudgetAdapter {
        fn new(id: &'static str, input_tokens: u64, output_tokens: u64) -> Self {
            Self {
                id,
                input_tokens,
                output_tokens,
            }
        }

        fn output(&self) -> AgentOutput {
            let mut output = AgentOutput::text(self.id, "budgeted output");
            output.cost = Some(UsageStats {
                input_tokens: self.input_tokens,
                output_tokens: self.output_tokens,
                estimated_cost_microusd: Some(5),
            });
            output
        }
    }

    #[async_trait::async_trait]
    impl ModelAdapter for BudgetAdapter {
        fn id(&self) -> &str {
            self.id
        }

        async fn implement(
            &self,
            _task: &Task,
            _cwd: &Path,
            _tx: mpsc::Sender<AgentEvent>,
        ) -> Result<AgentOutput> {
            Ok(self.output())
        }

        async fn review(
            &self,
            _task: &Task,
            _lead_output: &AgentOutput,
            _cwd: &Path,
            _strictness: &str,
            _tx: mpsc::Sender<AgentEvent>,
        ) -> Result<ReviewerFeedback> {
            panic!("review should be skipped after budget is exceeded")
        }

        async fn refine(
            &self,
            _task: &Task,
            _previous_output: &AgentOutput,
            _feedback: &ReviewerFeedback,
            _cwd: &Path,
            _tx: mpsc::Sender<AgentEvent>,
        ) -> Result<AgentOutput> {
            Ok(self.output())
        }
    }

    #[derive(Debug)]
    struct StubbornAdapter {
        id: &'static str,
    }

    impl StubbornAdapter {
        fn new(id: &'static str) -> Self {
            Self { id }
        }
    }

    #[async_trait::async_trait]
    impl ModelAdapter for StubbornAdapter {
        fn id(&self) -> &str {
            self.id
        }

        async fn implement(
            &self,
            _task: &Task,
            _cwd: &Path,
            _tx: mpsc::Sender<AgentEvent>,
        ) -> Result<AgentOutput> {
            Ok(AgentOutput::text(self.id, "implementation 0"))
        }

        async fn review(
            &self,
            _task: &Task,
            _lead_output: &AgentOutput,
            _cwd: &Path,
            _strictness: &str,
            _tx: mpsc::Sender<AgentEvent>,
        ) -> Result<ReviewerFeedback> {
            Ok(ReviewerFeedback {
                reviewer_id: self.id.to_string(),
                verdict: Verdict::RequestChanges,
                issues: vec![Issue {
                    severity: IssueSeverity::Warning,
                    message: "still not accepted".to_string(),
                }],
                suggested_patch: None,
                cost: None,
                raw_text: "REQUEST_CHANGES".to_string(),
            })
        }

        async fn refine(
            &self,
            _task: &Task,
            previous_output: &AgentOutput,
            _feedback: &ReviewerFeedback,
            _cwd: &Path,
            _tx: mpsc::Sender<AgentEvent>,
        ) -> Result<AgentOutput> {
            let next = if previous_output.raw_text.ends_with('0') {
                1
            } else {
                2
            };
            Ok(AgentOutput::text(self.id, format!("refinement {next}")))
        }
    }

    #[derive(Debug)]
    struct TournamentAdapter {
        id: &'static str,
    }

    impl TournamentAdapter {
        fn new(id: &'static str) -> Self {
            Self { id }
        }
    }

    #[async_trait::async_trait]
    impl ModelAdapter for TournamentAdapter {
        fn id(&self) -> &str {
            self.id
        }

        async fn implement(
            &self,
            _task: &Task,
            _cwd: &Path,
            _tx: mpsc::Sender<AgentEvent>,
        ) -> Result<AgentOutput> {
            Ok(AgentOutput::text(self.id, format!("{} patch", self.id)))
        }

        async fn review(
            &self,
            _task: &Task,
            lead_output: &AgentOutput,
            _cwd: &Path,
            _strictness: &str,
            _tx: mpsc::Sender<AgentEvent>,
        ) -> Result<ReviewerFeedback> {
            if lead_output.agent_id == "candidate-b" {
                Ok(ReviewerFeedback::lgtm(self.id, "LGTM candidate-b"))
            } else {
                Ok(ReviewerFeedback {
                    reviewer_id: self.id.to_string(),
                    verdict: Verdict::RequestChanges,
                    issues: vec![Issue {
                        severity: IssueSeverity::Warning,
                        message: "candidate-a loses tournament".to_string(),
                    }],
                    suggested_patch: None,
                    cost: None,
                    raw_text: "REQUEST_CHANGES: candidate-a loses tournament".to_string(),
                })
            }
        }

        async fn refine(
            &self,
            _task: &Task,
            _previous_output: &AgentOutput,
            _feedback: &ReviewerFeedback,
            _cwd: &Path,
            _tx: mpsc::Sender<AgentEvent>,
        ) -> Result<AgentOutput> {
            Ok(AgentOutput::text(self.id, "unused refinement"))
        }
    }

    #[derive(Debug)]
    struct ScratchLeadAdapter;

    #[async_trait::async_trait]
    impl ModelAdapter for ScratchLeadAdapter {
        fn id(&self) -> &str {
            "scratch-lead"
        }

        async fn implement(
            &self,
            _task: &Task,
            cwd: &Path,
            _tx: mpsc::Sender<AgentEvent>,
        ) -> Result<AgentOutput> {
            let scratch = cwd.join(".nerve/scratch/lead");
            std::fs::create_dir_all(&scratch)?;
            std::fs::write(scratch.join("note.txt"), "partial implementation\n")?;
            tokio::time::sleep(Duration::from_millis(250)).await;
            Ok(AgentOutput::text(self.id(), "lead done"))
        }

        async fn review(
            &self,
            _task: &Task,
            _lead_output: &AgentOutput,
            _cwd: &Path,
            _strictness: &str,
            _tx: mpsc::Sender<AgentEvent>,
        ) -> Result<ReviewerFeedback> {
            Ok(ReviewerFeedback::lgtm(self.id(), "LGTM"))
        }

        async fn refine(
            &self,
            _task: &Task,
            _previous_output: &AgentOutput,
            _feedback: &ReviewerFeedback,
            _cwd: &Path,
            _tx: mpsc::Sender<AgentEvent>,
        ) -> Result<AgentOutput> {
            Ok(AgentOutput::text(self.id(), "unused"))
        }
    }

    #[derive(Debug)]
    struct ScratchReviewerAdapter;

    #[async_trait::async_trait]
    impl ModelAdapter for ScratchReviewerAdapter {
        fn id(&self) -> &str {
            "scratch-reviewer"
        }

        async fn implement(
            &self,
            _task: &Task,
            _cwd: &Path,
            _tx: mpsc::Sender<AgentEvent>,
        ) -> Result<AgentOutput> {
            Ok(AgentOutput::text(self.id(), "unused"))
        }

        async fn review(
            &self,
            _task: &Task,
            _lead_output: &AgentOutput,
            _cwd: &Path,
            _strictness: &str,
            _tx: mpsc::Sender<AgentEvent>,
        ) -> Result<ReviewerFeedback> {
            Ok(ReviewerFeedback::lgtm(self.id(), "LGTM"))
        }

        async fn refine(
            &self,
            _task: &Task,
            _previous_output: &AgentOutput,
            _feedback: &ReviewerFeedback,
            _cwd: &Path,
            _tx: mpsc::Sender<AgentEvent>,
        ) -> Result<AgentOutput> {
            Ok(AgentOutput::text(self.id(), "unused"))
        }

        async fn crossfire(
            &self,
            _task: &Task,
            scratch_summary: &str,
            _cwd: &Path,
            _strictness: &str,
            _tx: mpsc::Sender<AgentEvent>,
        ) -> Result<ReviewerFeedback> {
            Ok(ReviewerFeedback::lgtm(self.id(), scratch_summary))
        }
    }

    fn consensus_config() -> Config {
        Config::from_json_str(
            r#"{
              "orchestration": {
                "default_strategy": "consensus",
                "max_refinement_rounds": 2,
                "conflict_policy": "lead_priority"
              },
              "roles": {
                "architect": "claude-code",
                "reviewer": "codex"
              },
              "profiles": []
            }"#,
        )
        .unwrap()
    }

    fn goal_spec(cmd: &[&str]) -> GoalSpec {
        GoalSpec {
            id: "g1".into(),
            check_cmd: cmd.iter().map(|s| (*s).to_string()).collect(),
            timeout_secs: 5,
            cwd: None,
            env: std::collections::BTreeMap::new(),
            no_progress_max: None,
        }
    }

    #[tokio::test]
    async fn orchestrator_and_combines_lgtm_with_check_pass() {
        let dir = tempfile::tempdir().unwrap();
        let task = Task::new("watch and pass", dir.path());
        let config = consensus_config();
        let adapters: Vec<Box<dyn ModelAdapter>> = vec![
            Box::new(MockAdapter::lead()),
            Box::new(MockAdapter::reviewer()),
        ];

        let options = RunOptions::new(false).with_goal(goal_spec(&["true"]));
        let report = run_synaptic_loop(task, &config, &adapters, options)
            .await
            .unwrap();

        assert_eq!(report.final_feedback.verdict, Verdict::Lgtm);
        assert_eq!(report.goal_satisfied, Some(true));
        assert!(!report.no_progress_exceeded);
        // mock_loop_refines_until_lgtm normally takes 2 rounds before LGTM; check Pass keeps it the same.
        assert!(!report.rounds.is_empty());
        assert!(report.rounds.iter().all(|r| r.check_result.is_some()));
    }

    #[tokio::test]
    async fn orchestrator_and_keeps_running_when_check_fails() {
        let dir = tempfile::tempdir().unwrap();
        let task = Task::new("check always fails", dir.path());
        let config = consensus_config();
        let adapters: Vec<Box<dyn ModelAdapter>> = vec![
            Box::new(MockAdapter::lead()),
            Box::new(MockAdapter::reviewer()),
        ];

        let options = RunOptions::new(false).with_goal(goal_spec(&["false"]));
        let report = run_synaptic_loop(task, &config, &adapters, options)
            .await
            .unwrap();

        // Even after reviewer LGTM, check Fail prevents early termination.
        // Loop runs the full max_refinement_rounds + 1 reviews and ends with goal_satisfied=false.
        assert_eq!(report.goal_satisfied, Some(false));
        assert_eq!(report.rounds.len(), 3);
        let last = report.rounds.last().unwrap();
        match &last.check_result {
            Some(CheckResult::Fail { .. }) => {}
            other => panic!("expected last round check to be Fail, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn orchestrator_blocks_apply_when_goal_check_fails() {
        let dir = tempfile::tempdir().unwrap();
        let task = Task::new("check blocks apply", dir.path());
        let config = consensus_config();
        let adapters: Vec<Box<dyn ModelAdapter>> = vec![
            Box::new(MockAdapter::lead()),
            Box::new(MockAdapter::reviewer()),
        ];

        let options = RunOptions::new(true).with_goal(goal_spec(&["false"]));
        let report = run_synaptic_loop(task, &config, &adapters, options)
            .await
            .unwrap();

        assert_eq!(report.goal_satisfied, Some(false));
        assert!(report.blocked);
        assert!(!report.applied);
        assert!(!dir.path().join("mock-output.txt").exists());
    }

    #[tokio::test]
    async fn orchestrator_records_skipped_check_when_no_goal() {
        let dir = tempfile::tempdir().unwrap();
        let task = Task::new("no goal", dir.path());
        let config = consensus_config();
        let adapters: Vec<Box<dyn ModelAdapter>> = vec![
            Box::new(MockAdapter::lead()),
            Box::new(MockAdapter::reviewer()),
        ];

        let report = run_synaptic_loop(task, &config, &adapters, RunOptions::new(false))
            .await
            .unwrap();

        assert_eq!(report.goal_satisfied, None);
        for round in &report.rounds {
            assert_eq!(round.check_result.as_ref(), Some(&CheckResult::Skipped));
        }
    }

    // ----- Tier 2d (v0.3.0) worktree integration tests -----

    fn run_git(dir: &Path, args: &[&str]) {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .expect("git invocation");
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn init_git_main(cwd: &Path) {
        run_git(cwd, &["init", "-q", "--initial-branch=main"]);
        run_git(cwd, &["config", "user.email", "nerve@example.com"]);
        run_git(cwd, &["config", "user.name", "Nerve Tester"]);
        std::fs::write(cwd.join("seed.txt"), "seed\n").unwrap();
        run_git(cwd, &["add", "seed.txt"]);
        run_git(cwd, &["commit", "-q", "-m", "seed"]);
    }

    fn head_oid(cwd: &Path) -> String {
        let out = std::process::Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(cwd)
            .output()
            .expect("git rev-parse");
        assert!(out.status.success(), "git rev-parse failed");
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    #[tokio::test]
    async fn worktree_apply_off_unchanged_behavior() {
        // Regression: when worktree_apply is off, the legacy in-place apply
        // path runs and a patched file appears under task.cwd as before.
        let dir = tempfile::tempdir().unwrap();
        let task = Task::new("legacy apply", dir.path());
        let config = consensus_config();
        let adapters: Vec<Box<dyn ModelAdapter>> = vec![
            Box::new(MockAdapter::lead()),
            Box::new(MockAdapter::reviewer()),
        ];

        let report = run_synaptic_loop(task, &config, &adapters, RunOptions::new(true))
            .await
            .unwrap();

        assert_eq!(report.final_feedback.verdict, Verdict::Lgtm);
        assert!(report.applied);
        assert!(dir.path().join("mock-output.txt").exists());
    }

    #[tokio::test]
    async fn worktree_apply_on_merge_on_success() {
        // When worktree is forced on, the final patch merges back into main
        // and main HEAD advances. The merged file lives at the original cwd.
        let dir = tempfile::tempdir().unwrap();
        init_git_main(dir.path());
        let pre = head_oid(dir.path());

        let task = Task::new("worktree apply", dir.path());
        let config = consensus_config();
        let adapters: Vec<Box<dyn ModelAdapter>> = vec![
            Box::new(MockAdapter::lead()),
            Box::new(MockAdapter::reviewer()),
        ];

        let report = run_synaptic_loop(
            task,
            &config,
            &adapters,
            RunOptions::new(true).with_worktree(true),
        )
        .await
        .unwrap();

        assert_eq!(report.final_feedback.verdict, Verdict::Lgtm);
        assert!(report.applied);
        let post = head_oid(dir.path());
        assert_ne!(
            pre, post,
            "main HEAD should advance after a successful worktree merge"
        );
        assert!(dir.path().join("mock-output.txt").exists());
    }

    #[tokio::test]
    async fn worktree_apply_on_discard_on_failure() {
        // With a goal check that always fails, blocked=true and the worktree
        // path is never entered. main HEAD stays put, and no orphan worktree
        // remains under .nerve/scratch.
        let dir = tempfile::tempdir().unwrap();
        init_git_main(dir.path());
        let pre = head_oid(dir.path());

        let task = Task::new("worktree discard", dir.path());
        let config = consensus_config();
        let adapters: Vec<Box<dyn ModelAdapter>> = vec![
            Box::new(MockAdapter::lead()),
            Box::new(MockAdapter::reviewer()),
        ];

        let options = RunOptions::new(true)
            .with_worktree(true)
            .with_goal(goal_spec(&["false"]));
        let report = run_synaptic_loop(task, &config, &adapters, options)
            .await
            .unwrap();

        assert!(report.blocked);
        assert!(!report.applied);
        assert_eq!(head_oid(dir.path()), pre);
        assert!(!dir.path().join("mock-output.txt").exists());

        // No orphan worktree should have been quarantined.
        let orphan = dir
            .path()
            .join(".nerve")
            .join("scratch")
            .join("orphaned-worktrees");
        if orphan.exists() {
            let count = std::fs::read_dir(&orphan)
                .unwrap()
                .filter(|e| {
                    e.as_ref()
                        .map(|x| x.file_name() != "manifest.jsonl")
                        .unwrap_or(false)
                })
                .count();
            assert_eq!(count, 0, "no orphan worktree should be quarantined");
        }
    }

    #[test]
    fn doctor_checks_basic() {
        let dir = tempfile::tempdir().unwrap();
        let config = consensus_config();
        let checks = doctor_checks(&config, dir.path());

        // Five checks emitted in the documented order.
        let names: Vec<&str> = checks.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "git",
                "disk_space",
                "orphaned_worktrees",
                "budget_audit_chain",
                "active_goal",
            ]
        );

        // git should be present in the test environment.
        assert_eq!(
            checks[0].status,
            DoctorStatus::Ok,
            "git --version must succeed: {:?}",
            checks[0].status
        );

        // No audit log yet → empty chain reported as Ok.
        assert_eq!(checks[3].status, DoctorStatus::Ok);

        // No active-goal.json yet → Ok.
        assert_eq!(checks[4].status, DoctorStatus::Ok);

        // No orphan dir yet → Ok (count == 0).
        assert_eq!(checks[2].status, DoctorStatus::Ok);
    }

    // ----- S1: accept-with-nits graduated verdict tests -----

    /// Test-double reviewer/lead that always returns `AcceptWithNits` with a
    /// single low-severity `Info` issue. As a lead it produces a stable patch
    /// (identical content on implement and refine) so the no-progress guard
    /// can observe an unchanged `patch_sha` across rounds.
    #[derive(Debug)]
    struct NitsAdapter {
        id: &'static str,
    }

    impl NitsAdapter {
        fn new(id: &'static str) -> Self {
            Self { id }
        }

        fn stable_patch() -> NvPatch {
            NvPatch::new(vec![FilePatch::create(
                "nits-output.txt",
                "Status: accepted-with-nits\n".to_string(),
            )])
        }
    }

    #[async_trait::async_trait]
    impl ModelAdapter for NitsAdapter {
        fn id(&self) -> &str {
            self.id
        }

        async fn implement(
            &self,
            _task: &Task,
            _cwd: &Path,
            _tx: mpsc::Sender<AgentEvent>,
        ) -> Result<AgentOutput> {
            Ok(AgentOutput::with_patch(
                self.id,
                "initial nits implementation",
                Self::stable_patch(),
            ))
        }

        async fn review(
            &self,
            _task: &Task,
            _lead_output: &AgentOutput,
            _cwd: &Path,
            _strictness: &str,
            _tx: mpsc::Sender<AgentEvent>,
        ) -> Result<ReviewerFeedback> {
            Ok(ReviewerFeedback::accept_with_nits(
                self.id,
                vec![Issue {
                    severity: IssueSeverity::Info,
                    message: "cosmetic: tighten variable naming".to_string(),
                }],
                "ACCEPT_WITH_NITS: cosmetic naming only",
            ))
        }

        async fn refine(
            &self,
            _task: &Task,
            _previous_output: &AgentOutput,
            _feedback: &ReviewerFeedback,
            _cwd: &Path,
            _tx: mpsc::Sender<AgentEvent>,
        ) -> Result<AgentOutput> {
            // Identical patch each round so patch_sha never changes.
            Ok(AgentOutput::with_patch(
                self.id,
                "refined nits implementation",
                Self::stable_patch(),
            ))
        }
    }

    /// Consensus config whose matched profile pins `review_strictness` to the
    /// supplied value. The task prompt must contain "nits" for the profile to
    /// match (see `nits_task`).
    fn nits_config(strictness: &str, conflict_policy: &str) -> Config {
        Config::from_json_str(&format!(
            r#"{{
              "orchestration": {{
                "default_strategy": "consensus",
                "max_refinement_rounds": 2,
                "conflict_policy": "{conflict_policy}"
              }},
              "roles": {{
                "architect": "nits-lead",
                "reviewer": "nits-reviewer"
              }},
              "profiles": [
                {{
                  "id": "nits-profile",
                  "match_rules": ["nits"],
                  "lead": "nits-lead",
                  "reviewer": "nits-reviewer",
                  "review_strictness": "{strictness}"
                }}
              ]
            }}"#
        ))
        .unwrap()
    }

    fn nits_task(dir: &Path) -> Task {
        Task::new("polish the nits in this patch", dir)
    }

    fn nits_adapters() -> Vec<Box<dyn ModelAdapter>> {
        vec![
            Box::new(NitsAdapter::new("nits-lead")) as Box<dyn ModelAdapter>,
            Box::new(NitsAdapter::new("nits-reviewer")) as Box<dyn ModelAdapter>,
        ]
    }

    fn nits_goal_spec(cmd: &[&str], no_progress_max: Option<u8>) -> GoalSpec {
        GoalSpec {
            id: "g-nits".into(),
            check_cmd: cmd.iter().map(|s| (*s).to_string()).collect(),
            timeout_secs: 5,
            cwd: None,
            env: std::collections::BTreeMap::new(),
            no_progress_max,
        }
    }

    #[tokio::test]
    async fn accept_with_nits_terminates_under_normal_strictness_with_green_check() {
        let dir = tempfile::tempdir().unwrap();
        let task = nits_task(dir.path());
        let config = nits_config("normal", "lead_priority");
        let adapters = nits_adapters();

        let options = RunOptions::new(false).with_goal(nits_goal_spec(&["true"], None));
        let report = run_synaptic_loop(task, &config, &adapters, options)
            .await
            .unwrap();

        // Round 0 accept-with-nits + green check terminates without a refine round.
        assert_eq!(report.rounds.len(), 1);
        assert_eq!(report.final_feedback.verdict, Verdict::AcceptWithNits);
        assert_eq!(report.goal_satisfied, Some(true));
        assert!(!report.blocked);
        assert!(!report.no_progress_exceeded);
    }

    #[tokio::test]
    async fn accept_with_nits_degrades_to_refine_under_high_strictness() {
        let dir = tempfile::tempdir().unwrap();
        let task = nits_task(dir.path());
        let config = nits_config("high", "lead_priority");
        let adapters = nits_adapters();

        // No no_progress guard so the loop runs the full refinement budget.
        let options = RunOptions::new(false).with_goal(nits_goal_spec(&["true"], None));
        let report = run_synaptic_loop(task, &config, &adapters, options)
            .await
            .unwrap();

        // High strictness does not terminate on round 0; full budget runs
        // (initial review + 2 refinement reviews = 3 rounds).
        assert_eq!(report.rounds.len(), 3);
        assert_eq!(report.final_feedback.verdict, Verdict::AcceptWithNits);
    }

    #[tokio::test]
    async fn accept_with_nits_does_not_terminate_when_check_fails() {
        let dir = tempfile::tempdir().unwrap();
        let task = nits_task(dir.path());
        let config = nits_config("normal", "lead_priority");
        let adapters = nits_adapters();

        // Green check is false → accept-with-nits cannot terminate the loop.
        let options = RunOptions::new(false).with_goal(nits_goal_spec(&["false"], None));
        let report = run_synaptic_loop(task, &config, &adapters, options)
            .await
            .unwrap();

        assert_eq!(report.rounds.len(), 3);
        assert_eq!(report.goal_satisfied, Some(false));
        assert!(report.blocked);
    }

    #[tokio::test]
    async fn accept_with_nits_no_progress_guard_reaps_strict_stuck_loop() {
        let dir = tempfile::tempdir().unwrap();
        let task = nits_task(dir.path());
        // High strictness keeps refining; identical patch each round trips the
        // no-progress guard at the first repeat (no_progress_max = 1).
        let config = nits_config("high", "lead_priority");
        let adapters = nits_adapters();

        let options = RunOptions::new(false).with_goal(nits_goal_spec(&["true"], Some(1)));
        let report = run_synaptic_loop(task, &config, &adapters, options)
            .await
            .unwrap();

        assert!(report.no_progress_exceeded);
        assert_eq!(report.final_feedback.verdict, Verdict::Block);
    }

    #[test]
    fn round_is_stalled_legacy_and_progress_dimensions() {
        // Legacy ma-1: identical patch hash is a stall regardless of progress.
        assert!(round_is_stalled(Some("a"), Some("a"), None, None));
        assert!(round_is_stalled(Some("a"), Some("a"), Some(900), Some(900)));

        // Different patch, no measurable progress → not stalled (legacy reset).
        assert!(!round_is_stalled(Some("a"), Some("b"), None, None));

        // S7: different patch but progress flat or regressed vs best → stalled.
        assert!(round_is_stalled(Some("a"), Some("b"), Some(700), Some(700)));
        assert!(round_is_stalled(Some("a"), Some("b"), Some(700), Some(500)));

        // S7: different patch and progress improved over best → not stalled.
        assert!(!round_is_stalled(Some("a"), Some("b"), Some(700), Some(800)));

        // A missing hash on either side never counts as a stall (legacy).
        assert!(!round_is_stalled(None, Some("b"), Some(700), Some(700)));
        assert!(!round_is_stalled(Some("a"), None, Some(700), Some(700)));
    }

    #[test]
    fn check_result_progress_maps_variants() {
        assert_eq!(check_result_progress(&CheckResult::Pass), Some(1000));
        assert_eq!(check_result_progress(&CheckResult::Skipped), None);
        assert_eq!(
            check_result_progress(&CheckResult::Fail {
                reason: "x".into(),
                progress: Some(420),
            }),
            Some(420)
        );
        assert_eq!(
            check_result_progress(&CheckResult::Fail {
                reason: "x".into(),
                progress: None,
            }),
            None
        );
    }

    #[tokio::test]
    async fn accept_with_nits_abort_on_conflict_not_blocked() {
        let dir = tempfile::tempdir().unwrap();
        let task = nits_task(dir.path());
        let config = nits_config("normal", "abort_on_conflict");
        let adapters = nits_adapters();

        let options = RunOptions::new(true).with_goal(nits_goal_spec(&["true"], None));
        let report = run_synaptic_loop(task, &config, &adapters, options)
            .await
            .unwrap();

        // accept-with-nits counts as an accept → abort_on_conflict does not block.
        assert_eq!(report.final_feedback.verdict, Verdict::AcceptWithNits);
        assert!(!report.blocked);
        assert!(report.applied);
        assert!(dir.path().join("nits-output.txt").exists());
    }

    /// Regression (codex BLOCKING #1): with NO goal the deterministic check is
    /// `Skipped`, which must NOT let `AcceptWithNits` accept on reviewer
    /// opinion alone. The loop must keep refining to the round budget instead
    /// of breaking on the first review.
    #[tokio::test]
    async fn accept_with_nits_skipped_check_does_not_shortcut() {
        let dir = tempfile::tempdir().unwrap();
        let task = nits_task(dir.path());
        let config = nits_config("normal", "lead_priority");
        let adapters = nits_adapters();

        // No goal → CheckResult::Skipped every round.
        let options = RunOptions::new(false);
        let report = run_synaptic_loop(task, &config, &adapters, options)
            .await
            .unwrap();

        // Ran the full budget (max_refinement_rounds = 2 → 3 rounds); did NOT
        // shortcut on a nits-accept with no real check.
        assert_eq!(report.rounds.len(), 3);
        assert_eq!(report.final_feedback.verdict, Verdict::AcceptWithNits);
    }

    /// Regression (codex BLOCKING #2): under High strictness an
    /// `AcceptWithNits` that exhausts the round budget must NOT be treated as
    /// an acceptance at finalization — it is neither goal-satisfied nor applied
    /// under `abort_on_conflict` (High degrades nits to a change request
    /// end-to-end, not just at the stop edge).
    #[tokio::test]
    async fn accept_with_nits_high_strictness_not_satisfied_or_applied() {
        let dir = tempfile::tempdir().unwrap();
        let task = nits_task(dir.path());
        let config = nits_config("high", "abort_on_conflict");
        let adapters = nits_adapters();

        // Green check + apply requested; High strictness must still refuse the nits.
        let options = RunOptions::new(true).with_goal(nits_goal_spec(&["true"], None));
        let report = run_synaptic_loop(task, &config, &adapters, options)
            .await
            .unwrap();

        assert_eq!(report.final_feedback.verdict, Verdict::AcceptWithNits);
        assert_eq!(report.rounds.len(), 3); // ran full budget, never accepted early
        assert_eq!(report.goal_satisfied, Some(false));
        assert!(report.blocked);
        assert!(!report.applied);
    }

    /// Regression (codex round-2 BLOCKING): the apply gate must also require a
    /// real check. With NO goal the check is `Skipped`, so even under Normal
    /// strictness with `abort_on_conflict` and `--apply`, an `AcceptWithNits`
    /// patch must be BLOCKED and never written — acceptance cannot rest on the
    /// reviewer verdict alone.
    #[tokio::test]
    async fn accept_with_nits_skipped_check_blocks_abort_on_conflict_apply() {
        let dir = tempfile::tempdir().unwrap();
        let task = nits_task(dir.path());
        let config = nits_config("normal", "abort_on_conflict");
        let adapters = nits_adapters();

        // No goal → Skipped check; apply requested.
        let options = RunOptions::new(true);
        let report = run_synaptic_loop(task, &config, &adapters, options)
            .await
            .unwrap();

        assert_eq!(report.final_feedback.verdict, Verdict::AcceptWithNits);
        assert!(report.blocked);
        assert!(!report.applied);
        assert!(!dir.path().join("nits-output.txt").exists());
    }

    /// Regression (codex round-3 BLOCKING): the verification gate is
    /// policy-independent. Under the DEFAULT `lead_priority` (reviewer
    /// advisory) a no-goal (Skipped) `AcceptWithNits` must still be blocked and
    /// never applied — a permissive conflict policy must not let a nits verdict
    /// be accepted on reviewer opinion alone.
    #[tokio::test]
    async fn accept_with_nits_skipped_check_blocks_lead_priority_apply() {
        let dir = tempfile::tempdir().unwrap();
        let task = nits_task(dir.path());
        let config = nits_config("normal", "lead_priority");
        let adapters = nits_adapters();

        // No goal → Skipped check; apply requested; permissive policy.
        let options = RunOptions::new(true);
        let report = run_synaptic_loop(task, &config, &adapters, options)
            .await
            .unwrap();

        assert_eq!(report.final_feedback.verdict, Verdict::AcceptWithNits);
        assert!(report.blocked);
        assert!(!report.applied);
        assert!(!dir.path().join("nits-output.txt").exists());
    }

    /// Positive guard: a GENUINE accept-with-nits (real Pass check + permissive
    /// strictness) IS applied under lead_priority — the verification gate must
    /// not over-block legitimately accepted work.
    #[tokio::test]
    async fn accept_with_nits_pass_check_applies_under_lead_priority() {
        let dir = tempfile::tempdir().unwrap();
        let task = nits_task(dir.path());
        let config = nits_config("normal", "lead_priority");
        let adapters = nits_adapters();

        let options = RunOptions::new(true).with_goal(nits_goal_spec(&["true"], None));
        let report = run_synaptic_loop(task, &config, &adapters, options)
            .await
            .unwrap();

        assert_eq!(report.final_feedback.verdict, Verdict::AcceptWithNits);
        assert!(!report.blocked);
        assert!(report.applied);
        assert!(dir.path().join("nits-output.txt").exists());
    }

    // ----- S4: always-on built-in verifier gate -----

    use nerve_config::{BuiltinVerifierConfig, BuiltinVerifierMode};

    /// consensus config whose built-in verifier runs `cmd` in `Command` mode
    /// (fast, deterministic — avoids invoking a real toolchain in tests).
    fn builtin_verifier_config(cmd: &[&str]) -> Config {
        let mut config = consensus_config();
        config.orchestration.builtin_verifier = BuiltinVerifierConfig {
            mode: BuiltinVerifierMode::Command,
            command: cmd.iter().map(|s| (*s).to_string()).collect(),
            timeout_secs: 5,
        };
        config
    }

    #[tokio::test]
    async fn builtin_verifier_supplies_real_pass_without_explicit_goal() {
        // The whole point of S4: with NO `/goal` the deterministic check is a
        // real Pass (never Skipped), so acceptance is verification-gated.
        let dir = tempfile::tempdir().unwrap();
        let task = Task::new("add a health endpoint", dir.path());
        let config = builtin_verifier_config(&["true"]);
        let adapters: Vec<Box<dyn ModelAdapter>> = vec![
            Box::new(MockAdapter::lead()),
            Box::new(MockAdapter::reviewer()),
        ];

        let report = run_synaptic_loop(task, &config, &adapters, RunOptions::new(false))
            .await
            .unwrap();

        assert_eq!(report.goal_satisfied, Some(true));
        assert!(!report.rounds.is_empty());
        for round in &report.rounds {
            assert_eq!(round.check_result.as_ref(), Some(&CheckResult::Pass));
        }
    }

    #[tokio::test]
    async fn builtin_verifier_fail_blocks_apply_without_explicit_goal() {
        // A failing built-in verifier must block apply even on reviewer LGTM —
        // this is the gap S1 noted, now closed: acceptance can't rest on the
        // reviewer's opinion alone.
        let dir = tempfile::tempdir().unwrap();
        let task = Task::new("add a health endpoint", dir.path());
        let config = builtin_verifier_config(&["false"]);
        let adapters: Vec<Box<dyn ModelAdapter>> = vec![
            Box::new(MockAdapter::lead()),
            Box::new(MockAdapter::reviewer()),
        ];

        let report = run_synaptic_loop(task, &config, &adapters, RunOptions::new(true))
            .await
            .unwrap();

        assert_eq!(report.goal_satisfied, Some(false));
        assert!(report.blocked);
        assert!(!report.applied);
        assert!(!dir.path().join("mock-output.txt").exists());
    }

    #[tokio::test]
    async fn builtin_verifier_off_preserves_skipped_check() {
        // Opt-out: with mode=off and no `/goal`, the check stays Skipped and
        // goal_satisfied is None (legacy behavior, reviewer verdict only).
        let dir = tempfile::tempdir().unwrap();
        // A Cargo.toml marker is present, proving Off wins over auto-detection.
        std::fs::write(dir.path().join("Cargo.toml"), "[package]\n").unwrap();
        let task = Task::new("add a health endpoint", dir.path());
        let mut config = consensus_config();
        config.orchestration.builtin_verifier = BuiltinVerifierConfig {
            mode: BuiltinVerifierMode::Off,
            ..Default::default()
        };
        let adapters: Vec<Box<dyn ModelAdapter>> = vec![
            Box::new(MockAdapter::lead()),
            Box::new(MockAdapter::reviewer()),
        ];

        let report = run_synaptic_loop(task, &config, &adapters, RunOptions::new(false))
            .await
            .unwrap();

        assert_eq!(report.goal_satisfied, None);
        for round in &report.rounds {
            assert_eq!(round.check_result.as_ref(), Some(&CheckResult::Skipped));
        }
    }

    #[tokio::test]
    async fn explicit_goal_wins_over_builtin_verifier() {
        // An explicit `/goal` (here, a failing one) must take precedence over
        // the built-in verifier (here, a passing one) — the user is in control.
        let dir = tempfile::tempdir().unwrap();
        let task = Task::new("add a health endpoint", dir.path());
        let config = builtin_verifier_config(&["true"]);
        let adapters: Vec<Box<dyn ModelAdapter>> = vec![
            Box::new(MockAdapter::lead()),
            Box::new(MockAdapter::reviewer()),
        ];

        let options = RunOptions::new(true).with_goal(goal_spec(&["false"]));
        let report = run_synaptic_loop(task, &config, &adapters, options)
            .await
            .unwrap();

        // The user's failing goal ran (not the passing built-in) → blocked.
        assert_eq!(report.goal_satisfied, Some(false));
        assert!(report.blocked);
        assert!(!report.applied);
    }

    // --- S8: round-incremental checkpoint -------------------------------------

    fn checkpoint_round(n: u8) -> RoundRecord {
        let patch = NvPatch::new(vec![FilePatch::create("created.txt", "created\n")]);
        RoundRecord {
            round: n,
            lead: AgentOutput::with_patch("lead", "patch", patch),
            reviewer: ReviewerFeedback::lgtm("reviewer", "LGTM"),
            check_result: None,
            patch_sha: None,
            envelope_id: None,
        }
    }

    #[tokio::test]
    async fn record_round_writes_incremental_checkpoint() {
        let dir = tempfile::tempdir().unwrap();
        let task = Task::new("checkpoint me", dir.path());
        let config = consensus_config();
        let selection = config.select_profile(&task).unwrap();
        let store = store::NerveStore::new(dir.path());
        let synapse = Synapse::with_checkpoint(task.clone(), store.clone(), selection);

        synapse.record_round(checkpoint_round(0)).await;
        synapse.record_round(checkpoint_round(1)).await;

        let checkpoint = store.load_checkpoint(&task.id).unwrap();
        assert_eq!(checkpoint.status, RunStatus::Running);
        assert_eq!(checkpoint.rounds.len(), 2);
        assert_eq!(checkpoint.rounds[1].round, 1);
        // A checkpoint is structurally incapable of asserting acceptance: it has
        // no applied/blocked/goal_satisfied/final_patch fields at all.
    }

    #[tokio::test]
    async fn synapse_without_checkpoint_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let task = Task::new("no checkpoint", dir.path());
        let synapse = Synapse::new(task);

        synapse.record_round(checkpoint_round(0)).await;

        // Existing-behavior preservation: `Synapse::new` never touches disk.
        assert!(!dir.path().join(".nerve").join("checkpoints").exists());
    }

    #[tokio::test]
    async fn interrupted_run_leaves_recoverable_checkpoint() {
        // The MockAdapter pair drives RequestChanges -> LGTM = 2 rounds. The loop
        // checkpoints each round but never finalizes (only the CLI calls
        // save_report), so this models a crash before finalize: all completed
        // rounds must be recoverable from the on-disk checkpoint.
        let dir = tempfile::tempdir().unwrap();
        let task = Task::new("add a health endpoint", dir.path());
        let config = consensus_config();
        let adapters: Vec<Box<dyn ModelAdapter>> = vec![
            Box::new(MockAdapter::lead()),
            Box::new(MockAdapter::reviewer()),
        ];

        let report = run_synaptic_loop(task, &config, &adapters, RunOptions::new(false))
            .await
            .unwrap();
        assert_eq!(report.rounds.len(), 2);

        let store = store::NerveStore::new(dir.path());
        let checkpoint = store.load_checkpoint(&report.task.id).unwrap();
        assert_eq!(checkpoint.status, RunStatus::Running);
        assert_eq!(checkpoint.rounds.len(), report.rounds.len());

        // Finalizing supersedes (clears) the in-flight checkpoint.
        store.save_report(&report).unwrap();
        assert!(store.load_checkpoint(&report.task.id).is_err());
        assert!(store.list_checkpoints().unwrap().is_empty());
    }

    // --- S9: live round-seam stream -------------------------------------------

    #[tokio::test]
    async fn streaming_loop_emits_each_round_live() {
        // MockAdapter drives RequestChanges -> LGTM = 2 rounds; the streaming
        // entry point must forward each completed round to the observer (the
        // sender is dropped when the loop ends, so draining terminates).
        let dir = tempfile::tempdir().unwrap();
        let task = Task::new("add a health endpoint", dir.path());
        let config = consensus_config();
        let adapters: Vec<Box<dyn ModelAdapter>> = vec![
            Box::new(MockAdapter::lead()),
            Box::new(MockAdapter::reviewer()),
        ];
        let (tx, mut rx) = mpsc::unbounded_channel::<RoundRecord>();

        let report =
            run_synaptic_loop_streaming(task, &config, &adapters, RunOptions::new(false), tx)
                .await
                .unwrap();

        let mut streamed = Vec::new();
        while let Some(round) = rx.recv().await {
            streamed.push(round);
        }
        assert_eq!(streamed.len(), report.rounds.len());
        assert_eq!(streamed.len(), 2);
        assert_eq!(streamed[0].round, 0);
        assert_eq!(streamed[1].round, 1);
    }

    #[tokio::test]
    async fn record_round_observer_send_error_is_ignored() {
        // A dropped receiver makes every observer send fail — the loop must
        // treat that as a no-op (read-only telemetry): in-memory state and the
        // S8 checkpoint still update, and record_round does not panic/abort.
        let dir = tempfile::tempdir().unwrap();
        let task = Task::new("observer dropped", dir.path());
        let config = consensus_config();
        let selection = config.select_profile(&task).unwrap();
        let store = store::NerveStore::new(dir.path());
        let (tx, rx) = mpsc::unbounded_channel::<RoundRecord>();
        drop(rx);
        let synapse =
            Synapse::with_checkpoint_and_observer(task.clone(), store.clone(), selection, Some(tx));

        synapse.record_round(checkpoint_round(0)).await;

        assert_eq!(synapse.rounds().await.len(), 1);
        assert_eq!(store.load_checkpoint(&task.id).unwrap().rounds.len(), 1);
    }
}
