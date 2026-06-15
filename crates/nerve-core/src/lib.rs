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
pub mod store;

pub use budget_audit::{BudgetAuditEntry, BudgetSnapshot, append_budget_audit_entry};
pub use goal::{GoalError, GoalEvaluator};

#[derive(Debug, Clone)]
pub struct Synapse {
    inner: Arc<RwLock<SynapseState>>,
    events: broadcast::Sender<AgentEvent>,
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
}

impl RunOptions {
    pub fn new(apply: bool) -> Self {
        Self { apply, goal: None }
    }

    pub fn with_goal(mut self, goal: GoalSpec) -> Self {
        self.goal = Some(goal);
        self
    }
}

impl Synapse {
    pub fn new(task: Task) -> Self {
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
        let mut inner = self.inner.write().await;
        inner.lead_output = Some(round.lead.clone());
        inner.reviewer_feedback = Some(round.reviewer.clone());
        inner.rounds.push(round);
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
    let selection = config.select_profile(&task)?;
    let lead = find_adapter(adapters, &selection.lead)?;
    let reviewer = find_adapter(adapters, &selection.reviewer)?;
    if matches!(config.orchestration.default_strategy, Strategy::Tournament) {
        return run_tournament_strategy(task, config, selection, lead, reviewer, options).await;
    }

    let synapse = Synapse::new(task.clone());
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
        };
        synapse.record_round(round).await;
        final_feedback = feedback;
        budget_exceeded = exceeds_budget(&usage, &config.orchestration);

        if budget_exceeded {
            final_feedback = budget_exceeded_feedback(reviewer.id(), &usage);
            break;
        }

        // ma-6 decision table: stop iff reviewer says LGTM AND check_result is Pass/Skipped.
        let reviewer_success = final_feedback.verdict.is_terminal_success();
        let check_success = matches!(check_result, CheckResult::Pass | CheckResult::Skipped);
        if reviewer_success && check_success {
            break;
        }

        if matches!(final_feedback.verdict, Verdict::Block) {
            break;
        }

        // ma-1 no-progress guard: identical patch hash + RequestChanges in a row.
        if no_progress_max > 0
            && matches!(final_feedback.verdict, Verdict::RequestChanges)
            && let Some(current) = patch_sha.as_ref()
            && previous_patch_sha.as_deref() == Some(current.as_str())
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
    let blocked = budget_exceeded
        || no_progress_exceeded
        || goal_check_failed
        || is_blocked(&final_feedback, &config.orchestration.conflict_policy);

    let applied = if options.apply && !blocked {
        if let Some(patch) = &final_patch {
            patch.apply(&task.cwd, false)?;
            true
        } else {
            false
        }
    } else {
        false
    };

    let goal_satisfied = options.goal.as_ref().map(|_| {
        matches!(
            last_check,
            Some(CheckResult::Pass) | Some(CheckResult::Skipped)
        ) && final_feedback.verdict.is_terminal_success()
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
) -> Result<RunReport> {
    let synapse = Synapse::new(task.clone());
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
    let blocked = budget_exceeded
        || goal_check_failed
        || is_blocked(&final_feedback, &config.orchestration.conflict_policy);

    let applied = if options.apply && !blocked {
        if let Some(patch) = &final_patch {
            patch.apply(&task.cwd, false)?;
            true
        } else {
            false
        }
    } else {
        false
    };

    let goal_satisfied = options.goal.as_ref().map(|_| {
        matches!(check_result, CheckResult::Pass | CheckResult::Skipped)
            && final_feedback.verdict.is_terminal_success()
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

fn build_goal_evaluator(
    task: &Task,
    options: &RunOptions,
    orchestration: &Orchestration,
) -> Result<Option<GoalEvaluator>> {
    let Some(spec) = options.goal.clone() else {
        return Ok(None);
    };
    let cwd = spec.cwd.clone().unwrap_or_else(|| task.cwd.clone());
    let evaluator = GoalEvaluator::new(
        spec,
        orchestration.check_env.clone(),
        orchestration.check_output_cap_bytes,
        cwd,
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

fn no_progress_feedback(reviewer_id: &str, count: u8) -> ReviewerFeedback {
    ReviewerFeedback {
        reviewer_id: reviewer_id.to_string(),
        verdict: Verdict::Block,
        issues: vec![Issue {
            severity: IssueSeverity::Blocking,
            message: format!("No progress for {count} consecutive rounds (identical patch hash)"),
        }],
        suggested_patch: None,
        cost: None,
        raw_text: format!("BLOCK: no-progress exceeded after {count} identical rounds"),
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
        ConflictPolicy::AbortOnConflict => feedback.verdict != Verdict::Lgtm,
        ConflictPolicy::MergeAttempt | ConflictPolicy::ReviewerBlock => {
            feedback.verdict == Verdict::Block
        }
        ConflictPolicy::Manual => true,
    }
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
}
