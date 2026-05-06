use anyhow::{Context, Result};
use nerve_adapter::ModelAdapter;
use nerve_config::{Config, ConflictPolicy, ProfileSelection, ReviewStrictness};
use nerve_patch::NvPatch;
use nerve_types::{AgentEvent, AgentOutput, ReviewerFeedback, RoundRecord, Task, Verdict};
use std::sync::Arc;
use tokio::sync::{RwLock, broadcast, mpsc};

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

#[derive(Debug, Clone)]
pub struct RunReport {
    pub task: Task,
    pub selection: ProfileSelection,
    pub rounds: Vec<RoundRecord>,
    pub final_output: AgentOutput,
    pub final_feedback: ReviewerFeedback,
    pub final_patch: Option<NvPatch>,
    pub events: Vec<AgentEvent>,
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

    let mut final_feedback = ReviewerFeedback::lgtm(reviewer.id(), "review not run");

    for round_index in 0..=selection.max_refinement_rounds {
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

        let round = RoundRecord {
            round: round_index,
            lead: lead_output.clone(),
            reviewer: feedback.clone(),
        };
        synapse.record_round(round).await;
        final_feedback = feedback;

        if final_feedback.verdict.is_terminal_success() {
            break;
        }

        if round_index == selection.max_refinement_rounds {
            break;
        }

        lead_output = lead
            .refine(&task, &lead_output, &final_feedback, &task.cwd, tx.clone())
            .await
            .with_context(|| format!("lead adapter `{}` failed during refinement", lead.id()))?;
    }

    drop(tx);
    event_task.await.context("event collector task failed")?;

    let final_patch = select_final_patch(
        &lead_output,
        &final_feedback,
        &config.orchestration.conflict_policy,
    );
    let blocked = is_blocked(&final_feedback, &config.orchestration.conflict_policy);

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
        applied,
        blocked,
    })
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
) -> Option<NvPatch> {
    match policy {
        ConflictPolicy::ReviewerPriority => feedback
            .suggested_patch
            .clone()
            .or_else(|| lead_output.proposed_patch.clone()),
        _ => lead_output.proposed_patch.clone(),
    }
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
}
