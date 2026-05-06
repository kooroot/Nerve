use anyhow::{Context, Result};
use nerve_adapter::ModelAdapter;
use nerve_config::{
    Config, ConflictPolicy, Orchestration, ProfileSelection, ReviewStrictness, Strategy,
};
use nerve_patch::{FileOperation, FilePatch, NvPatch};
use nerve_types::{
    AgentEvent, AgentOutput, Issue, IssueSeverity, ReviewerFeedback, RoundRecord, Task, UsageStats,
    Verdict,
};
use serde::Serialize;
use std::io::Write as _;
use std::process::Command;
use std::sync::Arc;
use tokio::sync::{RwLock, broadcast, mpsc};

pub mod store;

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
    pub events: Vec<AgentEvent>,
}

#[derive(Debug, Clone, Serialize, serde::Deserialize)]
pub struct RunReport {
    pub task: Task,
    pub selection: ProfileSelection,
    pub rounds: Vec<RoundRecord>,
    pub final_output: AgentOutput,
    pub final_feedback: ReviewerFeedback,
    pub final_patch: Option<NvPatch>,
    pub events: Vec<AgentEvent>,
    #[serde(default)]
    pub usage: UsageStats,
    #[serde(default)]
    pub budget_exceeded: bool,
    pub applied: bool,
    pub blocked: bool,
}

#[derive(Debug, Clone, Copy)]
pub struct RunOptions {
    pub apply: bool,
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

    pub async fn rounds(&self) -> Vec<RoundRecord> {
        self.inner.read().await.rounds.clone()
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

    let mut lead_output = lead
        .implement(&task, &task.cwd, tx.clone())
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

        let round = RoundRecord {
            round: round_index,
            lead: lead_output.clone(),
            reviewer: feedback.clone(),
        };
        synapse.record_round(round).await;
        final_feedback = feedback;
        budget_exceeded = exceeds_budget(&usage, &config.orchestration);

        if budget_exceeded {
            final_feedback = budget_exceeded_feedback(reviewer.id(), &usage);
            break;
        }

        if final_feedback.verdict.is_terminal_success() {
            break;
        }

        if round_index == max_refinement_rounds {
            break;
        }

        lead_output = lead
            .refine(&task, &lead_output, &final_feedback, &task.cwd, tx.clone())
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
    let blocked =
        budget_exceeded || is_blocked(&final_feedback, &config.orchestration.conflict_policy);

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

    Ok(RunReport {
        task,
        selection,
        rounds: synapse.rounds().await,
        final_output: lead_output,
        final_feedback,
        final_patch,
        events: synapse.events().await,
        usage,
        budget_exceeded,
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

    let round = RoundRecord {
        round: 0,
        lead: final_output.clone(),
        reviewer: final_feedback.clone(),
    };
    synapse.record_round(round).await;

    drop(tx);
    event_task.await.context("event collector task failed")?;

    let final_patch = select_final_patch(
        &final_output,
        &final_feedback,
        &config.orchestration.conflict_policy,
    )?;
    let blocked =
        budget_exceeded || is_blocked(&final_feedback, &config.orchestration.conflict_policy);

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

    Ok(RunReport {
        task,
        selection,
        rounds: synapse.rounds().await,
        final_output,
        final_feedback,
        final_patch,
        events: synapse.events().await,
        usage,
        budget_exceeded,
        applied,
        blocked,
    })
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
        ConflictPolicy::ReviewerPriority => Ok(feedback
            .suggested_patch
            .clone()
            .or_else(|| lead_output.proposed_patch.clone())),
        ConflictPolicy::MergeAttempt => merge_patches(
            lead_output.proposed_patch.as_ref(),
            feedback.suggested_patch.as_ref(),
        )
        .context("failed to merge lead and reviewer patches"),
        _ => Ok(lead_output.proposed_patch.clone()),
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

    if !output.status.success() {
        anyhow::bail!("{}", String::from_utf8_lossy(&output.stderr).trim());
    }

    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

fn is_blocked(feedback: &ReviewerFeedback, policy: &ConflictPolicy) -> bool {
    if feedback.verdict != Verdict::Block {
        return false;
    }

    !matches!(
        policy,
        ConflictPolicy::LeadPriority | ConflictPolicy::ReviewerPriority
    )
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

        let report = run_synaptic_loop(task, &config, &adapters, RunOptions { apply: false })
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

        let report = run_synaptic_loop(task, &config, &adapters, RunOptions { apply: true })
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

        let report = run_synaptic_loop(task, &config, &adapters, RunOptions { apply: false })
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

        let report = run_synaptic_loop(task, &config, &adapters, RunOptions { apply: false })
            .await
            .unwrap();

        assert_eq!(report.final_feedback.verdict, Verdict::Lgtm);
        assert_eq!(report.final_output.agent_id, "candidate-b");
        assert_eq!(report.rounds.len(), 1);
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
}
