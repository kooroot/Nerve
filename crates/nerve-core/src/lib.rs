use anyhow::{Context, Result};
use nerve_adapter::ModelAdapter;
use nerve_config::{
    ApplyClassifierConfig, Config, ConflictPolicy, GoalSpec, Orchestration, ProfileSelection,
    ReviewStrictness, Strategy,
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
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::SystemTime;
use tokio::sync::{RwLock, broadcast, mpsc};
use tokio::time::{Duration, sleep};

pub mod budget_audit;
// H15: Linux cgroups v2 per-check resource enforcement. Linux-only; other
// platforms never reference it (`goal` gates its use behind cfg(linux)).
#[cfg(target_os = "linux")]
pub mod cgroup;
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
pub use goal::{CheckOutcome, GoalError, GoalEvaluator};
pub use goal_intent::{GOAL_INTENT_SYSTEM_PROMPT, GoalIntentConverter, GoalIntentError};
pub use mayor_patrol::{
    Coordinator, DispatchFuture, Ledger, LedgerEntry, LedgerState, MailKind, MailMessage, Mayor,
    MayorError, MayorStatus, Patrol, PatrolResult, PatrolTask, PatrolVerdict, is_valid_queue_id,
};
pub use plan::{
    PLAN_ONLY_SYSTEM_PROMPT, PLAN_REVIEW_SYSTEM_PROMPT, PlanError, PlanRunOptions, PlanSections,
    PlanStep, parse_plan_steps, parse_plan_steps_from_markdown, plan_step_to_patrol_task,
    run_plan_mode, validate_plan_markdown,
};
pub use rpc::{EmitError, EmitOutcome, RpcBus, RpcError};
pub use session_fork::{
    ForkConfig, ForkError, ForkOptions, SessionForker, SessionIndexEntry, SessionTree,
};
pub use store::{ApprovalGrant, RunCheckpoint, RunStatus};
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
    // S10: set when the run was short-circuited by a decisive live crossfire
    // `Block` (Halt action). Additive + rejection-direction (mirrors
    // `no_progress_exceeded`): it feeds `blocked` and forces `goal_satisfied`
    // false, never the reverse. `#[serde(default)]` keeps older persisted
    // reports deserializable.
    #[serde(default)]
    pub crossfire_halted: bool,
    // S15: set when the run was cancelled by the operator at a round seam.
    // Additive + rejection-direction (mirrors `crossfire_halted`): it feeds
    // `blocked` and forces `goal_satisfied` false, never the reverse, so a
    // cancelled run is never applied or marked goal-satisfied.
    // `#[serde(default)]` keeps older persisted reports deserializable.
    #[serde(default)]
    pub cancelled: bool,
    #[serde(default)]
    pub goal_satisfied: Option<bool>,
    pub applied: bool,
    pub blocked: bool,
    // S12: the deterministic auto-mode classification of the final patch, when
    // the classifier is enabled (Advisory/Enforce). `None` when the classifier is
    // Off, keeping older persisted reports byte-identical. Telemetry only — when
    // `downgraded` is true the run was kept as a dry-run despite operator consent
    // because the patch was High risk; this NEVER affects `blocked`/`goal_satisfied`.
    #[serde(default)]
    pub apply_classification: Option<ApplyClassification>,
    // H13: pure telemetry — true when a deterministic goal check actually executed
    // WITHOUT OS confinement because `sandbox.mode = auto` requested a sandbox but
    // no backend was available on this host (the documented
    // "confine-if-possible, else run openly" degrade). It is false for `off`
    // (intentionally unconfined — not a degrade), for `required` (which fails
    // closed instead of degrading), whenever a backend confined the run, and when
    // the check never ran. Like `apply_classification`, this NEVER affects
    // `blocked`/`goal_satisfied`; it is OR-accumulated across rounds.
    // `#[serde(default)]` keeps older persisted reports deserializable.
    #[serde(default)]
    pub ran_unconfined: bool,
}

/// S12: deterministic apply-risk level for the auto-mode classifier.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ApplyRisk {
    /// The patch is small/contained — no risk signal tripped.
    Low,
    /// At least one risk signal tripped (size / risky path / destructive op).
    High,
}

/// S12: the deterministic classification of a run's final patch (file/line size,
/// risky touched paths, destructive ops). Pure telemetry attached to the report;
/// in `Enforce` mode a `High` classification additionally DOWNGRADES a
/// would-be apply to a dry-run (recorded in `downgraded`), but it NEVER touches
/// the deterministic acceptance gate (`blocked` / `goal_satisfied`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ApplyClassification {
    pub risk: ApplyRisk,
    /// Human-readable reasons the patch was rated `High` (empty when `Low`).
    pub reasons: Vec<String>,
    /// Number of files the patch touches that effect a real change (non-noop).
    pub files_touched: usize,
    /// Total changed lines (added + removed) across the patch.
    pub lines_changed: usize,
    /// True when an operator-consented apply was kept as a dry-run because the
    /// patch was `High` risk and the classifier was in `Enforce` mode. Telemetry
    /// only — this is a *downgrade*, never an escalation.
    pub downgraded: bool,
}

impl ApplyClassification {
    pub fn is_high(&self) -> bool {
        matches!(self.risk, ApplyRisk::High)
    }
}

/// S12: count changed lines (added + removed) in one file patch, from its unified
/// diff, excluding the `+++`/`---` file headers. Deterministic churn measure.
fn changed_line_count(file: &FilePatch) -> usize {
    file.to_unified_diff()
        .lines()
        .filter(|line| {
            (line.starts_with('+') && !line.starts_with("+++"))
                || (line.starts_with('-') && !line.starts_with("---"))
        })
        .count()
}

/// S12: compile risky-path globs into a matcher, skipping any individual pattern
/// that fails to compile (a malformed operator glob disables only itself, never
/// the whole apply path). Recompiled per `classify_apply` call — classification
/// runs once per run at the single apply seam, so this is not a hot path.
fn build_risky_glob_set(globs: &[String]) -> globset::GlobSet {
    let mut builder = globset::GlobSetBuilder::new();
    for pattern in globs {
        if let Ok(glob) = globset::Glob::new(pattern) {
            builder.add(glob);
        }
    }
    builder
        .build()
        .unwrap_or_else(|_| globset::GlobSet::empty())
}

/// S12: deterministically classify a run's final patch for apply risk. PURE
/// function of the patch + config — no LLM, no side effects, no I/O. A `None`,
/// empty, or all-noop patch is `Low` (nothing meaningful to apply). The result's
/// `downgraded` is always false here; the caller sets it iff an apply is vetoed.
fn classify_apply(patch: Option<&NvPatch>, cfg: &ApplyClassifierConfig) -> ApplyClassification {
    let files: Vec<&FilePatch> = patch
        .map(|p| p.files.iter().filter(|f| !f.is_noop()).collect())
        .unwrap_or_default();
    let files_touched = files.len();
    let lines_changed: usize = files.iter().map(|f| changed_line_count(f)).sum();

    let mut reasons = Vec::new();
    if files_touched > cfg.max_files {
        reasons.push(format!(
            "touches {files_touched} files (> max_files {})",
            cfg.max_files
        ));
    }
    if lines_changed > cfg.max_lines {
        reasons.push(format!(
            "changes {lines_changed} lines (> max_lines {})",
            cfg.max_lines
        ));
    }
    if cfg.flag_destructive_ops {
        for file in &files {
            match &file.operation {
                FileOperation::Delete => {
                    reasons.push(format!("deletes {}", file.path.display()));
                }
                FileOperation::Rename { from } => {
                    reasons.push(format!(
                        "renames {} -> {}",
                        from.display(),
                        file.path.display()
                    ));
                }
                _ => {}
            }
        }
    }
    let matcher = build_risky_glob_set(&cfg.risky_path_globs);
    for file in &files {
        if matcher.is_match(&file.path) {
            reasons.push(format!("touches risky path {}", file.path.display()));
        }
    }

    let risk = if reasons.is_empty() {
        ApplyRisk::Low
    } else {
        ApplyRisk::High
    };
    ApplyClassification {
        risk,
        reasons,
        files_touched,
        lines_changed,
        downgraded: false,
    }
}

/// S12: the auto-mode classifier gate. Given the pre-classifier apply decision
/// `want_apply` (which already ANDs operator consent with the deterministic
/// `!blocked`), return `(allow_apply, classification)`.
///
/// INVARIANT (the load-bearing safety property): `allow_apply <= want_apply`
/// ALWAYS. The classifier is monotone in the rejection direction — it can turn a
/// would-be apply OFF (downgrade to dry-run) but can NEVER turn a dry-run into an
/// apply, and it never reads or writes `blocked`/`goal_satisfied`. So:
/// - `Off`      → `(want_apply, None)` — byte-identical to pre-S12.
/// - `Advisory` → `(want_apply, Some(..))` — telemetry only; gate unchanged.
/// - `Enforce`  → vetoes (`allow_apply=false`, `downgraded=true`) ONLY when
///   `want_apply` was already true AND the patch is `High` risk; otherwise the
///   decision is unchanged.
fn apply_classifier_decision(
    want_apply: bool,
    patch: Option<&NvPatch>,
    cfg: &ApplyClassifierConfig,
) -> (bool, Option<ApplyClassification>) {
    if !cfg.mode.classifies() {
        return (want_apply, None);
    }
    let mut classification = classify_apply(patch, cfg);
    let veto = cfg.mode.enforces() && want_apply && classification.is_high();
    classification.downgraded = veto;
    (want_apply && !veto, Some(classification))
}

/// S11: a shared, daemon-controlled handle for escalating a SPECIFIC in-flight
/// run to apply-consent mid-run, honored at the run's apply point and sticky for
/// the run's whole life (all rounds/seams). It is the AUTHORITATIVE, forge-proof
/// consent signal: it lives in the daemon's memory and is flipped by the operator
/// (`approve <run-id>`), so — unlike a disk file under `.nerve/` — the lead
/// subprocess cannot reach or forge it. The `.nerve/approvals/` record (see
/// [`store::ApprovalGrant`]) is an audit trail only and is never read by the gate.
///
/// REJECTION-SAFE: this can only ever ENABLE apply from a real operator grant; it
/// feeds ONLY the apply trigger (`options.apply || granted`), never `blocked` or
/// `goal_satisfied`, so a blocked/rejected run is still never applied.
#[derive(Debug, Clone, Default)]
pub struct ApplyConsent(Arc<AtomicBool>);

impl ApplyConsent {
    /// A fresh, ungranted handle.
    pub fn new() -> Self {
        Self::default()
    }

    /// Record the operator's apply-consent for this run (idempotent).
    pub fn grant(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    /// Whether the operator has granted apply-consent for this run.
    pub fn is_granted(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

/// S15: a forge-proof, shared CANCELLATION handle for a single run. Like
/// [`ApplyConsent`] it is owned by the daemon and clone-shared with the run task;
/// the lead subprocess can never reach it, so the lead cannot cancel-then-claim
/// nor forge a cancel.
///
/// REJECTION-SAFE: this can only ever STOP a run. It feeds `cancelled` ->
/// `blocked` and forces `goal_satisfied=false` at the round seam — never the
/// reverse — so a cancelled run is always reported blocked and is NEVER applied
/// or marked goal-satisfied. It mirrors the S10 `crossfire_halted` path exactly
/// and adds no new accept/apply surface.
#[derive(Debug, Clone, Default)]
pub struct CancelToken(Arc<AtomicBool>);

impl CancelToken {
    /// A fresh, un-cancelled handle.
    pub fn new() -> Self {
        Self::default()
    }

    /// Request cancellation of this run (idempotent). Honored at the next round
    /// seam; it never interrupts an in-flight model subprocess mid-generation.
    pub fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    /// Whether cancellation has been requested for this run.
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
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
    /// S11: optional shared handle for mid-run operator apply-consent (see
    /// [`ApplyConsent`]). `None` (the default) leaves apply == `apply` exactly.
    /// When set, an operator can escalate THIS run to apply at a round seam; the
    /// gate independently re-judges the patch, so this never weakens acceptance.
    pub apply_grant: Option<ApplyConsent>,
    /// S15: optional shared handle for mid-run operator CANCELLATION (see
    /// [`CancelToken`]). `None` (the default) is byte-identical to pre-S15 — the
    /// seam check is a no-op and `cancelled` is always false. When set, an
    /// operator can cancel THIS run at a round seam; cancellation is
    /// rejection-direction only (feeds `blocked`, forces `goal_satisfied=false`).
    pub cancel_token: Option<CancelToken>,
}

impl RunOptions {
    pub fn new(apply: bool) -> Self {
        Self {
            apply,
            goal: None,
            ulimit: None,
            worktree: None,
            apply_grant: None,
            cancel_token: None,
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

    /// S11: attach a shared apply-consent handle so the daemon can escalate this
    /// run to apply mid-flight. See [`ApplyConsent`].
    pub fn with_apply_grant(mut self, grant: ApplyConsent) -> Self {
        self.apply_grant = Some(grant);
        self
    }

    /// S15: attach a shared cancel handle so the daemon can cancel this run
    /// mid-flight at a round seam. See [`CancelToken`].
    pub fn with_cancel_token(mut self, token: CancelToken) -> Self {
        self.cancel_token = Some(token);
        self
    }

    /// S15: whether this run has been CANCELLED by the operator. REJECTION-SAFE:
    /// only ever stops a run — the caller feeds it into `cancelled`/`blocked` and
    /// forces `goal_satisfied=false`, so it can never fabricate an acceptance.
    fn is_cancelled(&self) -> bool {
        self.cancel_token
            .as_ref()
            .is_some_and(CancelToken::is_cancelled)
    }

    /// S11: whether this run may APPLY its accepted patch — the invocation-time
    /// `apply` OR a granted in-memory escalation handle. REJECTION-SAFE: only
    /// ever enables from a real operator grant; the caller still ANDs this with
    /// `!blocked`, so it never applies a blocked/rejected run.
    fn apply_consented(&self) -> bool {
        self.apply
            || self
                .apply_grant
                .as_ref()
                .is_some_and(ApplyConsent::is_granted)
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

        // S8: persist the in-flight checkpoint. Additive recovery telemetry only
        // — a write failure is logged and swallowed (never aborts the loop, never
        // touches acceptance), and a checkpoint is never read as an authority on
        // whether a run succeeded. Cadence is once per completed round (model-call
        // bounded). The write goes through `write_json_atomic` (unique per-call
        // temp file + `rename(2)`) onto a per-run-id `{id}.json`, so even
        // concurrent multi-instance writers hit DIFFERENT files atomically, or race
        // renames onto one path where each swaps in a COMPLETE file — corruption is
        // structurally prevented and a reader never sees a torn checkpoint
        // (`store::concurrent_checkpoint_writes_never_corrupt_under_multi_instance`
        // proves this). A dedicated async writer task is therefore a deferred
        // latency optimization (S9), NOT a correctness requirement; the inline sync
        // write is safe here.
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

    // S10: `current_crossfire` holds the live crossfire feedback gathered during
    // the generation that produced the CURRENT `lead_output` (round 0 here; the
    // refine at the loop tail updates it for each subsequent round). The loop
    // acts on it at the seam — redirect (steer the next refine) and, under Halt,
    // short-circuit on a decisive live Block.
    let crossfire_action = config.orchestration.crossfire_action;
    let (mut lead_output, mut current_crossfire) = collect_output_with_crossfire(
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
    // S10 Halt: set when a decisive live crossfire `Block` short-circuits the
    // loop. Rejection-direction only — it feeds `blocked` and forces
    // `goal_satisfied=false`, mirroring `no_progress_exceeded`.
    let mut crossfire_halted = false;
    // S15: set when the operator cancels this run at a round seam. Rejection-
    // direction only — like `crossfire_halted` it feeds `blocked` and forces
    // `goal_satisfied=false`; it never overrides an acceptance.
    let mut cancelled = false;
    let mut last_check: Option<CheckResult> = None;
    // H13: OR-accumulated additive telemetry — set once any round's check actually
    // ran unconfined because `sandbox.mode = auto` found no backend. Never gates;
    // surfaced in the JSON report so an operator can see a degraded run.
    let mut ran_unconfined = false;
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

        let check_outcome = run_goal_check(goal_evaluator.as_ref()).await;
        ran_unconfined |= check_outcome.ran_unconfined;
        let check_result = check_outcome.result;
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

        // S15: operator cancellation, honored at the round seam. Placed AFTER the
        // terminal-accept check (like `crossfire_halted` below) so it can ONLY
        // fire on a non-accepting round and never overrides an acceptance already
        // returned. Rejection-direction: it sets `cancelled` -> `blocked` and
        // forces `goal_satisfied=false`, so a cancelled run is never applied.
        if options.is_cancelled() {
            cancelled = true;
            final_feedback = cancelled_feedback(reviewer.id());
            break;
        }

        if matches!(final_feedback.verdict, Verdict::Block) {
            break;
        }

        // S10 Halt: a decisive LIVE crossfire `Block` gathered while the lead
        // generated THIS round's output short-circuits the loop and blocks the
        // run. The terminal-accept check above returns FIRST for any acceptance,
        // so this can only ever fire on a NON-accepting round — it never overrides
        // an acceptance, and (like the `Block` break above) only ever stops the
        // loop earlier toward rejection. `crossfire_halted` feeds `blocked` and
        // forces `goal_satisfied=false` below — it can never fabricate acceptance.
        if crossfire_action.halts()
            && matches!(most_severe_crossfire(&current_crossfire), Some(Verdict::Block))
        {
            crossfire_halted = true;
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

        // S10 Redirect: when enabled, the live crossfire hints from THIS
        // generation enrich the refine prompt so the lead sees the
        // over-the-shoulder feedback in addition to the end-of-round review.
        // Rejection-biased and gate-safe: the gate-bearing `final_feedback` is
        // never mutated (only this refine-only copy), and the deterministic gate
        // independently re-judges the resulting patch.
        let refine_feedback = if crossfire_action.redirects() {
            merge_crossfire_into_feedback(&final_feedback, &current_crossfire)
        } else {
            final_feedback.clone()
        };
        let (next_output, next_crossfire) = collect_output_with_crossfire(
            lead.refine(&task, &lead_output, &refine_feedback, &task.cwd, tx.clone()),
            reviewer,
            &task,
            &selection,
            &synapse,
            tx.clone(),
        )
        .await
        .with_context(|| format!("lead adapter `{}` failed during refinement", lead.id()))?;
        lead_output = next_output;
        current_crossfire = next_crossfire;
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
        || crossfire_halted
        || cancelled
        || goal_check_failed
        || nits_unverified
        || is_blocked(&final_feedback, &config.orchestration.conflict_policy);

    // S11: apply IFF the operator consented (invocation `--apply` OR a mid-run
    // escalation grant) AND the run is not blocked. `apply_consented()` is
    // rejection-direction only; `!blocked` is the unchanged deterministic gate,
    // so a granted-but-blocked run still never applies. S15: a cancelled run is
    // blocked, so it is never applied.
    let want_apply = options.apply_consented() && !blocked;
    // S12: the auto-mode classifier may DOWNGRADE a would-be apply to a dry-run
    // when the final patch is High risk (Enforce mode). It is monotone —
    // `allow_apply <= want_apply` — so it never fabricates an apply and never
    // reads or writes `blocked`/`goal_satisfied`. Off/Advisory leave `allow_apply
    // == want_apply` (byte-identical apply decision).
    let (allow_apply, apply_classification) = apply_classifier_decision(
        want_apply,
        final_patch.as_ref(),
        &config.orchestration.apply_classifier,
    );
    let applied = apply_final_patch(
        &task,
        final_patch.as_ref(),
        allow_apply,
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
            && !crossfire_halted
            && !cancelled
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
        crossfire_halted,
        cancelled,
        goal_satisfied,
        applied,
        blocked,
        apply_classification,
        ran_unconfined,
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
    let check_outcome = run_goal_check(goal_evaluator.as_ref()).await;
    // H13: single round, so the run's unconfined-degrade telemetry is this check's.
    let ran_unconfined = check_outcome.ran_unconfined;
    let check_result = check_outcome.result;
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
    // S15: operator cancellation. The tournament runs a single round whose
    // generation has already completed by here, so this cannot interrupt the
    // round mid-flight; it is apply-GATING — a cancelled tournament run is
    // blocked and therefore never applied or marked goal-satisfied.
    let cancelled = options.is_cancelled();
    let blocked = budget_exceeded
        || cancelled
        || goal_check_failed
        || nits_unverified
        || is_blocked(&final_feedback, &config.orchestration.conflict_policy);

    // S11: apply IFF the operator consented (invocation `--apply` OR a mid-run
    // escalation grant) AND the run is not blocked. `apply_consented()` is
    // rejection-direction only; `!blocked` is the unchanged deterministic gate,
    // so a granted-but-blocked run still never applies. S15: a cancelled run is
    // blocked, so it is never applied.
    let want_apply = options.apply_consented() && !blocked;
    // S12: the auto-mode classifier may DOWNGRADE a would-be apply to a dry-run
    // when the final patch is High risk (Enforce mode). It is monotone —
    // `allow_apply <= want_apply` — so it never fabricates an apply and never
    // reads or writes `blocked`/`goal_satisfied`. Off/Advisory leave `allow_apply
    // == want_apply` (byte-identical apply decision).
    let (allow_apply, apply_classification) = apply_classifier_decision(
        want_apply,
        final_patch.as_ref(),
        &config.orchestration.apply_classifier,
    );
    let applied = apply_final_patch(
        &task,
        final_patch.as_ref(),
        allow_apply,
        resolve_worktree_apply(&options, &config.orchestration),
    )
    .await?;

    let goal_satisfied = options.goal.as_ref().map(|_| {
        matches!(check_result, CheckResult::Pass | CheckResult::Skipped)
            && final_feedback
                .verdict
                .accepts_under(selection.review_strictness.permits_nits())
            && !budget_exceeded
            && !cancelled
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
        // Tournament has no crossfire (single round, no scratch watcher), so it
        // can never be halted by a live crossfire Block.
        crossfire_halted: false,
        cancelled,
        goal_satisfied,
        applied,
        blocked,
        apply_classification,
        ran_unconfined,
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

async fn run_goal_check(evaluator: Option<&GoalEvaluator>) -> CheckOutcome {
    match evaluator {
        Some(eval) => eval.evaluate().await,
        // No goal configured: nothing ran, so it never ran unconfined.
        None => CheckOutcome {
            result: CheckResult::Skipped,
            ran_unconfined: false,
        },
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

/// S15: the rejection feedback recorded when an operator cancels a run at a round
/// seam. A `Block` verdict (like `no_progress_feedback`/`budget_exceeded_feedback`)
/// so a cancelled run is reported blocked and is never applied or goal-satisfied.
fn cancelled_feedback(reviewer_id: &str) -> ReviewerFeedback {
    ReviewerFeedback {
        reviewer_id: reviewer_id.to_string(),
        verdict: Verdict::Block,
        issues: vec![Issue {
            severity: IssueSeverity::Blocking,
            message: "Run cancelled by operator".to_string(),
        }],
        suggested_patch: None,
        cost: None,
        raw_text: "BLOCK: run cancelled by operator".to_string(),
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

/// Drive a lead generation while watching `.nerve/scratch` for over-the-shoulder
/// crossfire review. Returns the lead output AND the crossfire feedback gathered
/// during THIS generation (each item is also recorded to the synapse for the
/// report). S10 uses the returned batch to optionally redirect the next refine
/// or short-circuit the loop — strictly at the round seam, never mid-generation
/// (steering is seam-only by design; H14's `kill_on_drop` on the generation
/// subprocess only reaps an abandoned generation future, it is not a
/// mid-generation steering or cancel hook).
async fn collect_output_with_crossfire<F>(
    output_future: F,
    reviewer: &dyn ModelAdapter,
    task: &Task,
    selection: &ProfileSelection,
    synapse: &Synapse,
    tx: mpsc::Sender<AgentEvent>,
) -> Result<(AgentOutput, Vec<ReviewerFeedback>)>
where
    F: Future<Output = Result<AgentOutput>>,
{
    let mut watcher = ScratchWatcher::new(task.cwd.join(".nerve/scratch"))?;
    tokio::pin!(output_future);

    let mut collected: Vec<ReviewerFeedback> = Vec::new();
    loop {
        tokio::select! {
            output = &mut output_future => return output.map(|output| (output, collected)),
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
                    synapse.record_crossfire_feedback(feedback.clone()).await;
                    collected.push(feedback);
                }
            }
        }
    }
}

/// S10: severity rank for the crossfire helpers. Rejection-monotonic
/// (`Lgtm` < `AcceptWithNits` < `RequestChanges` < `Block`) — the same ordering
/// idea the S6 verdict scan relies on.
fn verdict_severity_rank(verdict: &Verdict) -> u8 {
    match verdict {
        Verdict::Lgtm => 0,
        Verdict::AcceptWithNits => 1,
        Verdict::RequestChanges => 2,
        Verdict::Block => 3,
    }
}

/// S10: canonical uppercase verdict token (the same wire format the reviewer
/// emits and `strip_verdict_prefix` parses), so a rendered crossfire hint reads
/// to the lead exactly like an ordinary reviewer verdict line.
fn verdict_token(verdict: &Verdict) -> &'static str {
    match verdict {
        Verdict::Lgtm => "LGTM",
        Verdict::AcceptWithNits => "ACCEPT_WITH_NITS",
        Verdict::RequestChanges => "REQUEST_CHANGES",
        Verdict::Block => "BLOCK",
    }
}

/// S10: uppercase label for an issue severity when rendering crossfire hints.
fn issue_severity_token(severity: &IssueSeverity) -> &'static str {
    match severity {
        IssueSeverity::Info => "INFO",
        IssueSeverity::Warning => "WARNING",
        IssueSeverity::Blocking => "BLOCKING",
    }
}

/// S10: the most severe verdict among a batch of live crossfire feedback, or
/// `None` if the batch is empty. Used to decide whether a decisive live `Block`
/// should short-circuit the loop (Halt action).
fn most_severe_crossfire(feedback: &[ReviewerFeedback]) -> Option<Verdict> {
    feedback
        .iter()
        .max_by_key(|item| verdict_severity_rank(&item.verdict))
        .map(|item| item.verdict.clone())
}

/// S10 redirect: produce a refine-only feedback that augments the end-of-round
/// review with the live crossfire hints. REJECTION-BIASED: the verdict is only
/// ever raised toward rejection (never lowered) and crossfire issues are
/// appended. This feeds ONLY the lead's refine prompt — the gate-bearing
/// `final_feedback` is never mutated, so crossfire can never fabricate
/// acceptance (the deterministic gate independently re-judges the next patch).
/// An empty crossfire batch yields a clone of `base` (no-op steering).
///
/// The hints are rendered into `raw_text` as well as `issues`/`verdict`: the
/// SHIPPED lead adapters build their refine prompt from `feedback.raw_text`
/// ONLY (see nerve-adapter `refine`), never the structured `verdict`/`issues`,
/// so without the `raw_text` rendering the redirect would never reach a real
/// lead. The rendered section is clearly labeled and uses the reviewer's
/// canonical verdict tokens so it reads like ordinary additional change
/// requests. This only affects the refine prompt — the deterministic gate never
/// reads this `raw_text`.
fn merge_crossfire_into_feedback(
    base: &ReviewerFeedback,
    crossfire: &[ReviewerFeedback],
) -> ReviewerFeedback {
    let mut merged = base.clone();
    if crossfire.is_empty() {
        return merged; // no-op steering — byte-identical clone of base
    }

    // Structured channel (report/telemetry + any adapter that reads it):
    // raise the verdict toward rejection, append the crossfire issues.
    for item in crossfire {
        if verdict_severity_rank(&item.verdict) > verdict_severity_rank(&merged.verdict) {
            merged.verdict = item.verdict.clone();
        }
        merged.issues.extend(item.issues.iter().cloned());
    }

    // Prose channel (what shipped adapters actually feed the lead): render the
    // crossfire hints into raw_text so the refine prompt conveys them.
    let mut section = String::from(
        "--- Live crossfire feedback (over-the-shoulder review captured during generation) ---\n\
         Treat the following as ADDITIONAL change requests for this refine:",
    );
    for item in crossfire {
        section.push_str(&format!("\n[CROSSFIRE {}]", verdict_token(&item.verdict)));
        let body = item.raw_text.trim();
        if !body.is_empty() {
            section.push(' ');
            section.push_str(body);
        }
        for issue in &item.issues {
            section.push_str(&format!(
                "\n  - {}: {}",
                issue_severity_token(&issue.severity),
                issue.message
            ));
        }
    }

    if merged.raw_text.trim().is_empty() {
        merged.raw_text = section;
    } else {
        merged.raw_text = format!("{}\n\n{}", merged.raw_text, section);
    }

    merged
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
        // A misconfigured key is a requested-but-broken integrity feature the
        // operator opted into (a set-but-non-UTF-8 env value, a relative/repo-local
        // or unreadable/empty key file): fail closed and loudly rather than
        // silently running unkeyed. Its message is already actionable.
        Err(err @ AuditError::KeyMisconfigured { .. }) => DoctorStatus::Fail(err.to_string()),
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

    // 6. (macOS, H8) Seatbelt enforcement canary. Seatbelt silently DROPS denied
    //    operations, so an OS change that broke enforcement would weaken the
    //    `sandbox.mode=required` write confinement INVISIBLY. Prove the kernel
    //    still denies a known out-of-root write and surface a loud Fail if it
    //    does not (or if the probe is inconclusive). Diagnostic only — this is
    //    NOT on the per-run hot path (H4 provides the per-run Required self-test).
    #[cfg(target_os = "macos")]
    {
        let status = match sandbox::seatbelt_enforcement_canary() {
            Ok(true) => DoctorStatus::Ok,
            Ok(false) => DoctorStatus::Fail(
                "macOS Seatbelt did NOT deny a known out-of-root write — sandbox enforcement appears BROKEN; `sandbox.mode=required` would not actually confine writes on this host. Use a container/VM for hard isolation."
                    .to_string(),
            ),
            Err(err) => DoctorStatus::Fail(format!(
                "could not verify macOS Seatbelt enforcement (canary inconclusive): {err}"
            )),
        };
        checks.push(DoctorCheck {
            name: "sandbox_enforcement".to_string(),
            status,
        });
    }

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

    // --- S10: crossfire redirect / short-circuit ------------------------------

    /// A distinctive message only ever emitted by the test reviewer's crossfire
    /// channel, so a test can prove whether a crossfire hint reached the refine.
    const CROSSFIRE_MARKER: &str = "CROSSFIRE_HINT_XYZ";

    fn feedback_with_verdict(id: &str, verdict: Verdict, message: &str) -> ReviewerFeedback {
        let severity = match verdict {
            Verdict::Block => IssueSeverity::Blocking,
            Verdict::RequestChanges => IssueSeverity::Warning,
            _ => IssueSeverity::Info,
        };
        ReviewerFeedback {
            reviewer_id: id.to_string(),
            verdict,
            issues: vec![Issue {
                severity,
                message: message.to_string(),
            }],
            suggested_patch: None,
            cost: None,
            raw_text: message.to_string(),
        }
    }

    /// A lead that writes to `.nerve/scratch` on every generation (so the live
    /// crossfire watcher fires) and CAPTURES the feedback it is refined with, so
    /// a test can assert whether crossfire hints were merged into the refine.
    #[derive(Debug, Clone)]
    struct CrossfireLeadAdapter {
        refine_feedback: std::sync::Arc<std::sync::Mutex<Vec<ReviewerFeedback>>>,
    }

    impl CrossfireLeadAdapter {
        fn new() -> Self {
            Self {
                refine_feedback: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            }
        }

        fn captured(&self) -> Vec<ReviewerFeedback> {
            self.refine_feedback.lock().unwrap().clone()
        }
    }

    #[async_trait::async_trait]
    impl ModelAdapter for CrossfireLeadAdapter {
        fn id(&self) -> &str {
            "crossfire-lead"
        }

        async fn implement(
            &self,
            _task: &Task,
            cwd: &Path,
            _tx: mpsc::Sender<AgentEvent>,
        ) -> Result<AgentOutput> {
            let scratch = cwd.join(".nerve/scratch/lead");
            std::fs::create_dir_all(&scratch)?;
            std::fs::write(scratch.join("note.txt"), "implement progress\n")?;
            tokio::time::sleep(Duration::from_millis(250)).await;
            Ok(AgentOutput::with_patch(
                self.id(),
                "lead v0",
                NvPatch::new(vec![FilePatch::new("a.txt", "old\n", "v0\n")]),
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
            Ok(ReviewerFeedback::lgtm(self.id(), "unused"))
        }

        async fn refine(
            &self,
            _task: &Task,
            _previous_output: &AgentOutput,
            feedback: &ReviewerFeedback,
            cwd: &Path,
            _tx: mpsc::Sender<AgentEvent>,
        ) -> Result<AgentOutput> {
            let n = {
                let mut guard = self.refine_feedback.lock().unwrap();
                guard.push(feedback.clone());
                guard.len()
            };
            let scratch = cwd.join(".nerve/scratch/lead");
            std::fs::create_dir_all(&scratch)?;
            std::fs::write(scratch.join("note.txt"), format!("refine progress {n}\n"))?;
            tokio::time::sleep(Duration::from_millis(250)).await;
            Ok(AgentOutput::with_patch(
                self.id(),
                "lead refined",
                NvPatch::new(vec![FilePatch::new("a.txt", "old\n", format!("v{n}\n"))]),
            ))
        }
    }

    /// A reviewer with a fixed end-of-round `review` verdict and a fixed live
    /// `crossfire` verdict. The crossfire feedback carries [`CROSSFIRE_MARKER`]
    /// so redirect/record-only can be distinguished.
    #[derive(Debug, Clone)]
    struct CrossfireReviewerAdapter {
        review_verdict: Verdict,
        crossfire_verdict: Verdict,
    }

    #[async_trait::async_trait]
    impl ModelAdapter for CrossfireReviewerAdapter {
        fn id(&self) -> &str {
            "crossfire-reviewer"
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
            Ok(feedback_with_verdict(
                self.id(),
                self.review_verdict.clone(),
                "review verdict",
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
            Ok(AgentOutput::text(self.id(), "unused"))
        }

        async fn crossfire(
            &self,
            _task: &Task,
            _scratch_summary: &str,
            _cwd: &Path,
            _strictness: &str,
            _tx: mpsc::Sender<AgentEvent>,
        ) -> Result<ReviewerFeedback> {
            Ok(feedback_with_verdict(
                self.id(),
                self.crossfire_verdict.clone(),
                CROSSFIRE_MARKER,
            ))
        }
    }

    fn crossfire_config(action: &str) -> Config {
        Config::from_json_str(&format!(
            r#"{{
              "orchestration": {{
                "default_strategy": "consensus",
                "max_refinement_rounds": 2,
                "conflict_policy": "lead_priority",
                "crossfire_action": "{action}"
              }},
              "roles": {{ "architect": "crossfire-lead", "reviewer": "crossfire-reviewer" }},
              "profiles": []
            }}"#
        ))
        .unwrap()
    }

    #[test]
    fn most_severe_crossfire_picks_block() {
        let batch = vec![
            ReviewerFeedback::lgtm("r", "ok"),
            feedback_with_verdict("r", Verdict::RequestChanges, "changes"),
            feedback_with_verdict("r", Verdict::Block, "block"),
            feedback_with_verdict("r", Verdict::AcceptWithNits, "nit"),
        ];
        assert_eq!(most_severe_crossfire(&batch), Some(Verdict::Block));
        assert_eq!(most_severe_crossfire(&[]), None);
    }

    #[test]
    fn merge_crossfire_into_feedback_is_rejection_biased() {
        let base = feedback_with_verdict("r", Verdict::RequestChanges, "base issue");
        let crossfire = vec![
            // A "looks good" hint must NEVER lower the verdict ...
            feedback_with_verdict("r", Verdict::Lgtm, "lgtm hint"),
            // ... and a Block hint raises it toward rejection.
            feedback_with_verdict("r", Verdict::Block, CROSSFIRE_MARKER),
        ];
        let merged = merge_crossfire_into_feedback(&base, &crossfire);
        assert_eq!(merged.verdict, Verdict::Block);
        assert!(merged.issues.iter().any(|i| i.message.contains("base issue")));
        assert!(merged.issues.iter().any(|i| i.message.contains(CROSSFIRE_MARKER)));
        // The hint must also reach `raw_text` — the ONLY channel the shipped
        // lead adapters read for the refine prompt. The end-of-round review's
        // own text is preserved alongside it.
        assert!(merged.raw_text.contains(CROSSFIRE_MARKER));
        assert!(merged.raw_text.contains("base issue"));

        // An empty crossfire batch is a no-op (byte-identical clone of base).
        let noop = merge_crossfire_into_feedback(&base, &[]);
        assert_eq!(noop.verdict, base.verdict);
        assert_eq!(noop.issues.len(), base.issues.len());
        assert_eq!(noop.raw_text, base.raw_text);
    }

    #[tokio::test]
    async fn crossfire_off_is_record_only() {
        let dir = tempfile::tempdir().unwrap();
        let task = Task::new("off path", dir.path());
        let config = crossfire_config("off");
        let lead = CrossfireLeadAdapter::new();
        let adapters: Vec<Box<dyn ModelAdapter>> = vec![
            Box::new(lead.clone()),
            Box::new(CrossfireReviewerAdapter {
                review_verdict: Verdict::RequestChanges,
                crossfire_verdict: Verdict::Block,
            }),
        ];

        let report = run_synaptic_loop(task, &config, &adapters, RunOptions::new(false))
            .await
            .unwrap();

        // Crossfire is still RECORDED (advisory) ...
        assert!(!report.crossfire_feedback.is_empty());
        // ... but a live Block never halts the run under `off`,
        assert!(!report.crossfire_halted);
        // ... and it never steers the refine: no captured refine feedback
        // carries the crossfire marker — in neither the structured `issues`
        // nor the `raw_text` the shipped lead adapters actually read.
        let captured = lead.captured();
        assert!(!captured.is_empty());
        assert!(captured.iter().all(|fb| {
            !fb.raw_text.contains(CROSSFIRE_MARKER)
                && fb
                    .issues
                    .iter()
                    .all(|i| !i.message.contains(CROSSFIRE_MARKER))
        }));
    }

    #[tokio::test]
    async fn crossfire_redirect_merges_into_refine() {
        let dir = tempfile::tempdir().unwrap();
        let task = Task::new("redirect path", dir.path());
        let config = crossfire_config("redirect");
        let lead = CrossfireLeadAdapter::new();
        let adapters: Vec<Box<dyn ModelAdapter>> = vec![
            Box::new(lead.clone()),
            Box::new(CrossfireReviewerAdapter {
                review_verdict: Verdict::RequestChanges,
                crossfire_verdict: Verdict::RequestChanges,
            }),
        ];

        let report = run_synaptic_loop(task, &config, &adapters, RunOptions::new(false))
            .await
            .unwrap();

        // Redirect steered the refine: the first refine feedback carries the
        // live crossfire marker merged in (the end-of-round review never emits
        // it, so its presence proves the crossfire was the source). It must
        // reach `raw_text` — the channel shipped lead adapters build the refine
        // prompt from — not just the structured `issues`.
        let captured = lead.captured();
        assert!(!captured.is_empty());
        assert!(captured[0].raw_text.contains(CROSSFIRE_MARKER));
        assert!(
            captured[0]
                .issues
                .iter()
                .any(|i| i.message.contains(CROSSFIRE_MARKER))
        );
        // Redirect never halts.
        assert!(!report.crossfire_halted);
    }

    #[tokio::test]
    async fn crossfire_halt_blocks_on_live_block() {
        let dir = tempfile::tempdir().unwrap();
        let task = Task::new("halt path", dir.path());
        let config = crossfire_config("halt");
        let lead = CrossfireLeadAdapter::new();
        let adapters: Vec<Box<dyn ModelAdapter>> = vec![
            Box::new(lead.clone()),
            // review is RequestChanges (non-accepting AND not Block, so the
            // existing Block break does not fire); crossfire is a decisive
            // live Block.
            Box::new(CrossfireReviewerAdapter {
                review_verdict: Verdict::RequestChanges,
                crossfire_verdict: Verdict::Block,
            }),
        ];

        // apply=true: prove the halt's `blocked` gate prevents application.
        let report = run_synaptic_loop(task, &config, &adapters, RunOptions::new(true))
            .await
            .unwrap();

        assert!(report.crossfire_halted);
        assert!(report.blocked);
        assert!(!report.applied);
        // The halt short-circuits at round 0 → no refine ever runs.
        assert!(lead.captured().is_empty());
    }

    #[tokio::test]
    async fn crossfire_halt_never_overrides_acceptance() {
        let dir = tempfile::tempdir().unwrap();
        let task = Task::new("accept path", dir.path());
        let config = crossfire_config("halt");
        let lead = CrossfireLeadAdapter::new();
        let adapters: Vec<Box<dyn ModelAdapter>> = vec![
            Box::new(lead.clone()),
            // review ACCEPTS (Lgtm) → round 0 is terminal (Lgtm + Skipped check).
            // The terminal-accept check precedes the Halt check, so a live Block
            // crossfire can NEVER override the acceptance (rejection-direction
            // only — this is the load-bearing north-star guard).
            Box::new(CrossfireReviewerAdapter {
                review_verdict: Verdict::Lgtm,
                crossfire_verdict: Verdict::Block,
            }),
        ];

        let report = run_synaptic_loop(task, &config, &adapters, RunOptions::new(false))
            .await
            .unwrap();

        assert_eq!(report.final_feedback.verdict, Verdict::Lgtm);
        assert!(!report.crossfire_halted);
        assert!(!report.blocked);
    }

    #[tokio::test]
    async fn crossfire_non_block_never_halts() {
        let dir = tempfile::tempdir().unwrap();
        let task = Task::new("non-block crossfire", dir.path());
        let config = crossfire_config("halt");
        let lead = CrossfireLeadAdapter::new();
        let adapters: Vec<Box<dyn ModelAdapter>> = vec![
            Box::new(lead.clone()),
            // Crossfire is only ever RequestChanges (never Block) → never halts,
            // even under the Halt action.
            Box::new(CrossfireReviewerAdapter {
                review_verdict: Verdict::RequestChanges,
                crossfire_verdict: Verdict::RequestChanges,
            }),
        ];

        let report = run_synaptic_loop(task, &config, &adapters, RunOptions::new(false))
            .await
            .unwrap();

        assert!(!report.crossfire_halted);
    }

    // --- S15: operator cancellation (round-seam, rejection-direction) ---------

    #[tokio::test]
    async fn cancel_at_seam_blocks_and_never_applies() {
        let dir = tempfile::tempdir().unwrap();
        let task = Task::new("cancel path", dir.path());
        let config = crossfire_config("redirect");
        let lead = CrossfireLeadAdapter::new();
        let adapters: Vec<Box<dyn ModelAdapter>> = vec![
            Box::new(lead.clone()),
            // review is RequestChanges (non-accepting AND not Block), and crossfire
            // never blocks — so the ONLY thing that can stop the loop at round 0 is
            // the operator cancel. This isolates the cancel as the cause.
            Box::new(CrossfireReviewerAdapter {
                review_verdict: Verdict::RequestChanges,
                crossfire_verdict: Verdict::RequestChanges,
            }),
        ];

        // Operator cancelled before the first seam. apply=true proves the cancel's
        // `blocked` gate prevents application (the load-bearing N1 guard).
        let token = CancelToken::new();
        token.cancel();
        let options = RunOptions::new(true).with_cancel_token(token);
        let report = run_synaptic_loop(task, &config, &adapters, options)
            .await
            .unwrap();

        assert!(report.cancelled, "run must be marked cancelled");
        assert!(report.blocked, "cancelled run must be blocked");
        assert!(!report.applied, "cancelled run must NEVER apply");
        assert_ne!(
            report.goal_satisfied,
            Some(true),
            "cancelled run must never be goal-satisfied"
        );
        assert_eq!(report.final_feedback.verdict, Verdict::Block);
        // Cancel short-circuits at round 0 → no refine ever runs.
        assert!(lead.captured().is_empty());
    }

    #[tokio::test]
    async fn cancel_never_overrides_acceptance() {
        let dir = tempfile::tempdir().unwrap();
        let task = Task::new("accept path", dir.path());
        let config = crossfire_config("redirect");
        let lead = CrossfireLeadAdapter::new();
        let adapters: Vec<Box<dyn ModelAdapter>> = vec![
            Box::new(lead.clone()),
            // review ACCEPTS (Lgtm) → round 0 is terminal. The terminal-accept
            // check precedes the cancel check, so a cancel can NEVER override an
            // acceptance already returned (rejection-direction only — N2 guard).
            Box::new(CrossfireReviewerAdapter {
                review_verdict: Verdict::Lgtm,
                crossfire_verdict: Verdict::RequestChanges,
            }),
        ];

        // Cancel is SET, but round 0 already accepts → acceptance wins.
        let token = CancelToken::new();
        token.cancel();
        let options = RunOptions::new(false).with_cancel_token(token);
        let report = run_synaptic_loop(task, &config, &adapters, options)
            .await
            .unwrap();

        assert_eq!(report.final_feedback.verdict, Verdict::Lgtm);
        assert!(!report.cancelled);
        assert!(!report.blocked);
    }

    #[tokio::test]
    async fn cancel_token_uncancelled_is_inert() {
        // An ATTACHED but un-cancelled token must change nothing: the run reaches
        // its normal acceptance and is not marked cancelled (N4 byte-identical).
        let dir = tempfile::tempdir().unwrap();
        let task = Task::new("inert token", dir.path());
        let config = crossfire_config("redirect");
        let adapters: Vec<Box<dyn ModelAdapter>> = vec![
            Box::new(CrossfireLeadAdapter::new()),
            Box::new(CrossfireReviewerAdapter {
                review_verdict: Verdict::Lgtm,
                crossfire_verdict: Verdict::RequestChanges,
            }),
        ];

        let token = CancelToken::new(); // never cancelled
        let options = RunOptions::new(false).with_cancel_token(token);
        let report = run_synaptic_loop(task, &config, &adapters, options)
            .await
            .unwrap();

        assert!(!report.cancelled);
        assert_eq!(report.final_feedback.verdict, Verdict::Lgtm);
    }

    #[tokio::test]
    async fn tournament_cancel_blocks_and_never_applies() {
        let dir = tempfile::tempdir().unwrap();
        let task = Task::new("cancel tournament", dir.path());
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

        // The tournament's single round completes (a winner is chosen), but the
        // operator cancelled — so the run is blocked and never applied even though
        // a candidate would otherwise have been accepted (apply-gating).
        let token = CancelToken::new();
        token.cancel();
        let options = RunOptions::new(true).with_cancel_token(token);
        let report = run_synaptic_loop(task, &config, &adapters, options)
            .await
            .unwrap();

        assert!(report.cancelled, "tournament run must be marked cancelled");
        assert!(report.blocked, "cancelled tournament run must be blocked");
        assert!(!report.applied, "cancelled tournament run must NEVER apply");
        assert_ne!(report.goal_satisfied, Some(true));
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

    // --- S11: per-run apply-consent escalation -------------------------------

    #[test]
    fn apply_consent_handle_is_shared_and_starts_ungranted() {
        let consent = ApplyConsent::new();
        assert!(!consent.is_granted());
        // A grant on one handle is visible through every clone — this shared
        // semantics is what lets the daemon flip a live run's consent mid-flight
        // while the run holds its own clone.
        let clone = consent.clone();
        consent.grant();
        assert!(clone.is_granted());
    }

    #[test]
    fn apply_consented_only_enables_from_a_real_grant() {
        // `--apply` alone consents.
        assert!(RunOptions::new(true).apply_consented());
        // Dry-run with no handle never consents.
        assert!(!RunOptions::new(false).apply_consented());
        // An ATTACHED but ungranted handle is byte-identical to dry-run.
        assert!(
            !RunOptions::new(false)
                .with_apply_grant(ApplyConsent::new())
                .apply_consented()
        );
        // A granted handle consents even when `--apply` was not passed.
        let granted = ApplyConsent::new();
        granted.grant();
        assert!(
            RunOptions::new(false)
                .with_apply_grant(granted)
                .apply_consented()
        );
    }

    #[tokio::test]
    async fn apply_grant_enables_apply_on_accepted_run() {
        // An operator escalation (granted mid-run) makes an ACCEPTED dry-run
        // actually apply — the daemon's mid-flight `approve` path.
        let dir = tempfile::tempdir().unwrap();
        let task = Task::new("granted apply", dir.path());
        let config = consensus_config();
        let adapters: Vec<Box<dyn ModelAdapter>> = vec![
            Box::new(MockAdapter::lead()),
            Box::new(MockAdapter::reviewer()),
        ];
        let consent = ApplyConsent::new();
        consent.grant(); // operator approved THIS run
        let options = RunOptions::new(false).with_apply_grant(consent);

        let report = run_synaptic_loop(task, &config, &adapters, options)
            .await
            .unwrap();

        assert_eq!(report.final_feedback.verdict, Verdict::Lgtm);
        assert!(report.applied);
        assert!(dir.path().join("mock-output.txt").exists());
    }

    #[tokio::test]
    async fn ungranted_apply_handle_is_dry_run() {
        // Attaching an UNGRANTED handle must be byte-identical to dry-run: an
        // accepted run does NOT apply unless the operator actually granted.
        let dir = tempfile::tempdir().unwrap();
        let task = Task::new("ungranted dry-run", dir.path());
        let config = consensus_config();
        let adapters: Vec<Box<dyn ModelAdapter>> = vec![
            Box::new(MockAdapter::lead()),
            Box::new(MockAdapter::reviewer()),
        ];
        let options = RunOptions::new(false).with_apply_grant(ApplyConsent::new());

        let report = run_synaptic_loop(task, &config, &adapters, options)
            .await
            .unwrap();

        assert_eq!(report.final_feedback.verdict, Verdict::Lgtm);
        assert!(!report.applied);
        assert!(!dir.path().join("mock-output.txt").exists());
    }

    #[tokio::test]
    async fn apply_grant_never_applies_a_blocked_run() {
        // North star: the grant feeds ONLY the apply trigger, never `!blocked`.
        // A failing deterministic goal check blocks the run; even a granted
        // escalation must NOT apply it.
        let dir = tempfile::tempdir().unwrap();
        let task = Task::new("granted but blocked", dir.path());
        let config = consensus_config();
        let adapters: Vec<Box<dyn ModelAdapter>> = vec![
            Box::new(MockAdapter::lead()),
            Box::new(MockAdapter::reviewer()),
        ];
        let consent = ApplyConsent::new();
        consent.grant();
        let options = RunOptions::new(false)
            .with_apply_grant(consent)
            .with_goal(goal_spec(&["false"]));

        let report = run_synaptic_loop(task, &config, &adapters, options)
            .await
            .unwrap();

        assert!(report.blocked);
        assert!(!report.applied);
        assert!(!dir.path().join("mock-output.txt").exists());
    }

    // ===== H18: standing thesis-invariant guards =============================
    // Mechanize two facets of the safety thesis's negative space that were not
    // previously pinned by a dedicated test (see `nerve_config::ConfigSource`'s
    // doc comment for the full checklist). Each maps to a named regression and
    // turns red if that regression is introduced.

    #[test]
    fn h18_invariant_run_options_default_is_dry_run() {
        // Regression guarded: "apply defaulted true". A run built for dry-run
        // never consents to apply, and attaching an ungranted consent handle is
        // byte-identical to dry-run. (Complements `apply_consented_only_enables_
        // from_a_real_grant`; kept as the H18 entry point for the thesis.)
        let dry = RunOptions::new(false);
        assert!(!dry.apply);
        assert!(!dry.apply_consented());
        assert!(
            !RunOptions::new(false)
                .with_apply_grant(ApplyConsent::new())
                .apply_consented()
        );
    }

    // A tournament candidate that BOTH emits a patch (so an apply would write
    // `mock-output.txt`) and LGTMs its opponent (so the tournament reaches a
    // terminal-success verdict). This makes the tournament apply seam reachable
    // with a real patch, so the "not applied" assertion below is non-vacuous.
    #[derive(Debug)]
    struct PatchTournamentAdapter {
        id: &'static str,
    }
    #[async_trait::async_trait]
    impl ModelAdapter for PatchTournamentAdapter {
        fn id(&self) -> &str {
            self.id
        }
        async fn implement(
            &self,
            _task: &Task,
            _cwd: &Path,
            _tx: mpsc::Sender<AgentEvent>,
        ) -> Result<AgentOutput> {
            let patch = NvPatch::new(vec![FilePatch::create(
                "mock-output.txt",
                "tournament patch\n",
            )]);
            Ok(AgentOutput::with_patch(self.id, "tournament patch", patch))
        }
        async fn review(
            &self,
            _task: &Task,
            _lead_output: &AgentOutput,
            _cwd: &Path,
            _strictness: &str,
            _tx: mpsc::Sender<AgentEvent>,
        ) -> Result<ReviewerFeedback> {
            Ok(ReviewerFeedback::lgtm(self.id, "LGTM"))
        }
        async fn refine(
            &self,
            _task: &Task,
            _previous_output: &AgentOutput,
            _feedback: &ReviewerFeedback,
            _cwd: &Path,
            _tx: mpsc::Sender<AgentEvent>,
        ) -> Result<AgentOutput> {
            let patch = NvPatch::new(vec![FilePatch::create(
                "mock-output.txt",
                "tournament refined\n",
            )]);
            Ok(AgentOutput::with_patch(self.id, "tournament refine", patch))
        }
    }

    // Shared body for the disk-approval invariant: forge an on-disk
    // `ApprovalGrant` claiming apply-consent for the run, drive the run to a
    // terminal-success verdict with apply=false and NO in-memory grant, and
    // assert it still does not apply. Used for EVERY production apply seam so a
    // regression wiring any one gate to read disk approvals is caught.
    async fn assert_forged_disk_approval_never_applies(
        config: &Config,
        adapters: &[Box<dyn ModelAdapter>],
    ) {
        let dir = tempfile::tempdir().unwrap();
        let task = Task::new("disk approval must be ignored", dir.path());
        // Forge an on-disk approval record for this run id (apply_consent=true).
        crate::store::NerveStore::new(dir.path())
            .record_approval(&ApprovalGrant::apply(task.id.clone()))
            .unwrap();

        // Dry-run, NO in-memory grant — the only authoritative consent surface.
        let report = run_synaptic_loop(task, config, adapters, RunOptions::new(false))
            .await
            .unwrap();

        // Accepted, yet NOT applied: the disk grant is inert. The LGTM assertion
        // keeps the no-apply assertion meaningful (the run really reached the
        // apply seam with a patch present).
        assert_eq!(
            report.final_feedback.verdict,
            Verdict::Lgtm,
            "run must reach acceptance so the no-apply assertion is non-vacuous"
        );
        assert!(
            !report.applied,
            "a forged on-disk approval must never make an accepted dry-run apply"
        );
        assert!(!dir.path().join("mock-output.txt").exists());
    }

    #[tokio::test]
    async fn h18_invariant_disk_approval_record_is_never_read_by_apply_gate() {
        // Regression guarded: "approvals read by the gate". The lead is an
        // arbitrary subprocess with write access to `.nerve/` in `task.cwd`; if
        // any apply gate ever consulted the on-disk `ApprovalGrant` the lead
        // could forge operator consent and self-escalate dry-run -> apply. There
        // are TWO production apply seams (consensus `run_synaptic_loop_inner` and
        // `run_tournament_strategy`); exercise BOTH so a regression in either is
        // caught (R2 of the H18 review found a single-seam test missed tournament).

        // Consensus seam.
        let consensus_adapters: Vec<Box<dyn ModelAdapter>> = vec![
            Box::new(MockAdapter::lead()),
            Box::new(MockAdapter::reviewer()),
        ];
        assert_forged_disk_approval_never_applies(&consensus_config(), &consensus_adapters).await;

        // Tournament seam (a SEPARATE apply gate).
        let tournament_config = Config::from_json_str(
            r#"{
              "orchestration": {
                "default_strategy": "tournament",
                "max_refinement_rounds": 2,
                "conflict_policy": "lead_priority"
              },
              "roles": { "architect": "cand-a", "reviewer": "cand-b" },
              "profiles": []
            }"#,
        )
        .unwrap();
        let tournament_adapters: Vec<Box<dyn ModelAdapter>> = vec![
            Box::new(PatchTournamentAdapter { id: "cand-a" }),
            Box::new(PatchTournamentAdapter { id: "cand-b" }),
        ];
        assert_forged_disk_approval_never_applies(&tournament_config, &tournament_adapters).await;
    }

    // --- S12: auto-mode classifier gate (implement↔apply) --------------------

    fn classifier_cfg(
        mode: nerve_config::ApplyClassifierMode,
        max_files: usize,
        max_lines: usize,
    ) -> ApplyClassifierConfig {
        ApplyClassifierConfig {
            mode,
            max_files,
            max_lines,
            risky_path_globs: vec!["**/Cargo.lock".to_string(), "**/.github/**".to_string()],
            flag_destructive_ops: true,
        }
    }

    #[test]
    fn classify_apply_none_or_noop_patch_is_low() {
        let cfg = classifier_cfg(nerve_config::ApplyClassifierMode::Enforce, 25, 800);
        let none = classify_apply(None, &cfg);
        assert_eq!(none.risk, ApplyRisk::Low);
        assert_eq!(none.files_touched, 0);
        assert_eq!(none.lines_changed, 0);
        assert!(none.reasons.is_empty());
        assert!(!none.downgraded);
        // A patch whose only file is a no-op (identical content) is also Low.
        let noop = NvPatch::single("a.txt", "same\n", "same\n");
        let c = classify_apply(Some(&noop), &cfg);
        assert_eq!(c.risk, ApplyRisk::Low);
        assert_eq!(c.files_touched, 0);
    }

    #[test]
    fn classify_apply_low_for_small_contained_patch() {
        let cfg = classifier_cfg(nerve_config::ApplyClassifierMode::Enforce, 25, 800);
        let patch = NvPatch::single("src/small.rs", "fn a() {}\n", "fn a() { b(); }\n");
        let c = classify_apply(Some(&patch), &cfg);
        assert_eq!(c.risk, ApplyRisk::Low);
        assert_eq!(c.files_touched, 1);
        assert!(c.reasons.is_empty());
    }

    #[test]
    fn classify_apply_high_on_each_risk_signal() {
        // Too many files.
        let cfg_files = classifier_cfg(nerve_config::ApplyClassifierMode::Enforce, 1, 800);
        let many = NvPatch::new(vec![
            FilePatch::new("a.rs", "x\n", "y\n"),
            FilePatch::new("b.rs", "x\n", "y\n"),
        ]);
        let c = classify_apply(Some(&many), &cfg_files);
        assert_eq!(c.risk, ApplyRisk::High);
        assert_eq!(c.files_touched, 2);
        assert!(c.reasons.iter().any(|r| r.contains("max_files")));

        // Too many lines.
        let cfg_lines = classifier_cfg(nerve_config::ApplyClassifierMode::Enforce, 25, 1);
        let big = NvPatch::single("a.rs", "1\n2\n3\n", "1\n2x\n3x\n");
        let c = classify_apply(Some(&big), &cfg_lines);
        assert_eq!(c.risk, ApplyRisk::High);
        assert!(c.reasons.iter().any(|r| r.contains("max_lines")));

        // Risky path via glob — `**/Cargo.lock` must match a bare `Cargo.lock`.
        let cfg = classifier_cfg(nerve_config::ApplyClassifierMode::Enforce, 25, 800);
        let lock = NvPatch::single("Cargo.lock", "a = 1\n", "a = 2\n");
        let c = classify_apply(Some(&lock), &cfg);
        assert_eq!(c.risk, ApplyRisk::High);
        assert!(c.reasons.iter().any(|r| r.contains("risky path")));
        // Nested risky path via `**/.github/**`.
        let ci = NvPatch::single(".github/workflows/ci.yml", "on: push\n", "on: pull\n");
        assert!(classify_apply(Some(&ci), &cfg).is_high());

        // Destructive op (delete) regardless of small size.
        let del = NvPatch::new(vec![FilePatch::delete("src/gone.rs", "old\n")]);
        let c = classify_apply(Some(&del), &cfg);
        assert_eq!(c.risk, ApplyRisk::High);
        assert!(c.reasons.iter().any(|r| r.contains("deletes")));

        // Destructive ops can be disabled — then a small delete is Low.
        let mut cfg_no_destruct = cfg.clone();
        cfg_no_destruct.flag_destructive_ops = false;
        assert_eq!(
            classify_apply(Some(&del), &cfg_no_destruct).risk,
            ApplyRisk::Low
        );
    }

    #[test]
    fn apply_classifier_decision_off_is_byte_identical() {
        // Off never classifies and never changes the decision, for any patch/want.
        let high = NvPatch::new(vec![
            FilePatch::new("a", "x\n", "y\n"),
            FilePatch::new("b", "x\n", "y\n"),
        ]);
        let cfg = classifier_cfg(nerve_config::ApplyClassifierMode::Off, 1, 1);
        for want in [true, false] {
            let (allow, cls) = apply_classifier_decision(want, Some(&high), &cfg);
            assert_eq!(allow, want);
            assert!(cls.is_none());
        }
    }

    #[test]
    fn apply_classifier_decision_advisory_classifies_but_never_vetoes() {
        let high = NvPatch::new(vec![
            FilePatch::new("a", "x\n", "y\n"),
            FilePatch::new("b", "x\n", "y\n"),
        ]);
        let cfg = classifier_cfg(nerve_config::ApplyClassifierMode::Advisory, 1, 800);
        for want in [true, false] {
            let (allow, cls) = apply_classifier_decision(want, Some(&high), &cfg);
            assert_eq!(allow, want); // gate unchanged
            let cls = cls.expect("advisory records a classification");
            assert!(cls.is_high());
            assert!(!cls.downgraded);
        }
    }

    #[test]
    fn apply_classifier_decision_enforce_downgrades_only_a_would_be_apply() {
        let high = NvPatch::new(vec![
            FilePatch::new("a", "x\n", "y\n"),
            FilePatch::new("b", "x\n", "y\n"),
        ]);
        let cfg = classifier_cfg(nerve_config::ApplyClassifierMode::Enforce, 1, 800);
        // want_apply=true + High ⇒ vetoed (downgraded to dry-run).
        let (allow, cls) = apply_classifier_decision(true, Some(&high), &cfg);
        assert!(!allow);
        assert!(cls.unwrap().downgraded);
        // want_apply=false + High ⇒ stays false; NEVER upgraded, nothing downgraded.
        let (allow, cls) = apply_classifier_decision(false, Some(&high), &cfg);
        assert!(!allow);
        assert!(!cls.unwrap().downgraded);

        // Enforce + Low ⇒ never vetoes; allow == want.
        let low = NvPatch::single("ok.rs", "a\n", "a b\n");
        let cfg_low = classifier_cfg(nerve_config::ApplyClassifierMode::Enforce, 25, 800);
        for want in [true, false] {
            let (allow, cls) = apply_classifier_decision(want, Some(&low), &cfg_low);
            assert_eq!(allow, want);
            let cls = cls.unwrap();
            assert!(!cls.is_high());
            assert!(!cls.downgraded);
        }
    }

    #[test]
    fn apply_classifier_decision_never_exceeds_want_apply() {
        // THE load-bearing invariant: allow_apply <= want_apply for EVERY mode,
        // patch, threshold, and want value (allow implies want — never an upgrade).
        let patches: [Option<NvPatch>; 3] = [
            None,
            Some(NvPatch::single("ok.rs", "a\n", "b\n")),
            Some(NvPatch::new(vec![FilePatch::delete("x", "y\n")])),
        ];
        for mode in [
            nerve_config::ApplyClassifierMode::Off,
            nerve_config::ApplyClassifierMode::Advisory,
            nerve_config::ApplyClassifierMode::Enforce,
        ] {
            for max_files in [0usize, 1, 25] {
                for want in [true, false] {
                    for patch in &patches {
                        let cfg = classifier_cfg(mode, max_files, 800);
                        let (allow, _) = apply_classifier_decision(want, patch.as_ref(), &cfg);
                        assert!(!allow || want, "allow_apply must imply want_apply");
                    }
                }
            }
        }
    }

    #[tokio::test]
    async fn enforce_downgrades_a_high_risk_apply() {
        // Operator passed --apply and the run is accepted, but Enforce + a High-
        // risk patch (max_files 0 ⇒ any non-empty patch is High) keeps it a
        // dry-run. The deterministic gate is untouched; this is a pure downgrade.
        let dir = tempfile::tempdir().unwrap();
        let task = Task::new("enforce downgrade", dir.path());
        let mut config = consensus_config();
        config.orchestration.apply_classifier =
            classifier_cfg(nerve_config::ApplyClassifierMode::Enforce, 0, 100_000);
        let adapters: Vec<Box<dyn ModelAdapter>> = vec![
            Box::new(MockAdapter::lead()),
            Box::new(MockAdapter::reviewer()),
        ];

        let report = run_synaptic_loop(task, &config, &adapters, RunOptions::new(true))
            .await
            .unwrap();

        assert_eq!(report.final_feedback.verdict, Verdict::Lgtm);
        assert!(!report.blocked); // NOT blocked — the gate accepted it
        assert!(!report.applied); // ...but the classifier downgraded the apply
        let cls = report.apply_classification.expect("classification recorded");
        assert!(cls.is_high());
        assert!(cls.downgraded);
        assert!(!dir.path().join("mock-output.txt").exists());
    }

    #[tokio::test]
    async fn off_classifier_keeps_apply_byte_identical() {
        // With the classifier Off (the default), an accepted --apply run applies
        // exactly as before and records no classification.
        let dir = tempfile::tempdir().unwrap();
        let task = Task::new("classifier off", dir.path());
        let config = consensus_config(); // apply_classifier defaults to Off
        let adapters: Vec<Box<dyn ModelAdapter>> = vec![
            Box::new(MockAdapter::lead()),
            Box::new(MockAdapter::reviewer()),
        ];

        let report = run_synaptic_loop(task, &config, &adapters, RunOptions::new(true))
            .await
            .unwrap();

        assert!(report.applied);
        assert!(report.apply_classification.is_none());
        assert!(dir.path().join("mock-output.txt").exists());
    }

    #[tokio::test]
    async fn advisory_classifies_but_does_not_block_apply() {
        // Advisory surfaces the High-risk classification but never vetoes — the
        // accepted --apply run still applies.
        let dir = tempfile::tempdir().unwrap();
        let task = Task::new("classifier advisory", dir.path());
        let mut config = consensus_config();
        config.orchestration.apply_classifier =
            classifier_cfg(nerve_config::ApplyClassifierMode::Advisory, 0, 100_000);
        let adapters: Vec<Box<dyn ModelAdapter>> = vec![
            Box::new(MockAdapter::lead()),
            Box::new(MockAdapter::reviewer()),
        ];

        let report = run_synaptic_loop(task, &config, &adapters, RunOptions::new(true))
            .await
            .unwrap();

        assert!(report.applied); // advisory never vetoes
        let cls = report.apply_classification.expect("classification recorded");
        assert!(cls.is_high());
        assert!(!cls.downgraded);
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

        // Five cross-platform checks in documented order; macOS appends a sixth
        // `sandbox_enforcement` canary (H8), asserted separately below.
        let names: Vec<&str> = checks.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(
            &names[..5],
            &[
                "git",
                "disk_space",
                "orphaned_worktrees",
                "budget_audit_chain",
                "active_goal",
            ]
        );
        #[cfg(target_os = "macos")]
        {
            assert_eq!(names.len(), 6, "macOS emits the sandbox_enforcement canary");
            assert_eq!(names[5], "sandbox_enforcement");
            // Seatbelt enforces on a healthy dev host, so the canary passes.
            assert_eq!(
                checks[5].status,
                DoctorStatus::Ok,
                "sandbox enforcement canary: {:?}",
                checks[5].status
            );
        }
        #[cfg(not(target_os = "macos"))]
        assert_eq!(names.len(), 5);

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
