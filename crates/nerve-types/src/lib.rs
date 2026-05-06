use chrono::{DateTime, Utc};
use nerve_patch::NvPatch;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Task {
    pub id: String,
    pub prompt: String,
    pub cwd: PathBuf,
    pub context_paths: Vec<PathBuf>,
    pub started_at: DateTime<Utc>,
}

impl Task {
    pub fn new(prompt: impl Into<String>, cwd: impl Into<PathBuf>) -> Self {
        Self {
            id: Uuid::new_v4().to_string(),
            prompt: prompt.into(),
            cwd: cwd.into(),
            context_paths: Vec::new(),
            started_at: Utc::now(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentOutput {
    pub agent_id: String,
    pub raw_text: String,
    pub proposed_patch: Option<NvPatch>,
    pub tool_calls: Vec<ToolCall>,
    pub cost: Option<UsageStats>,
}

impl AgentOutput {
    pub fn text(agent_id: impl Into<String>, raw_text: impl Into<String>) -> Self {
        Self {
            agent_id: agent_id.into(),
            raw_text: raw_text.into(),
            proposed_patch: None,
            tool_calls: Vec::new(),
            cost: None,
        }
    }

    pub fn with_patch(
        agent_id: impl Into<String>,
        raw_text: impl Into<String>,
        proposed_patch: NvPatch,
    ) -> Self {
        Self {
            agent_id: agent_id.into(),
            raw_text: raw_text.into(),
            proposed_patch: Some(proposed_patch),
            tool_calls: Vec::new(),
            cost: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ToolCall {
    pub name: String,
    pub arguments: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct UsageStats {
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    #[serde(default)]
    pub estimated_cost_microusd: Option<u64>,
}

impl UsageStats {
    pub fn total_tokens(&self) -> u64 {
        self.input_tokens.saturating_add(self.output_tokens)
    }

    pub fn add_assign(&mut self, other: &Self) {
        self.input_tokens = self.input_tokens.saturating_add(other.input_tokens);
        self.output_tokens = self.output_tokens.saturating_add(other.output_tokens);
        self.estimated_cost_microusd =
            match (self.estimated_cost_microusd, other.estimated_cost_microusd) {
                (Some(current), Some(next)) => Some(current.saturating_add(next)),
                (Some(current), None) => Some(current),
                (None, Some(next)) => Some(next),
                (None, None) => None,
            };
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReviewerFeedback {
    pub reviewer_id: String,
    pub verdict: Verdict,
    pub issues: Vec<Issue>,
    pub suggested_patch: Option<NvPatch>,
    #[serde(default)]
    pub cost: Option<UsageStats>,
    pub raw_text: String,
}

impl ReviewerFeedback {
    pub fn lgtm(reviewer_id: impl Into<String>, raw_text: impl Into<String>) -> Self {
        Self {
            reviewer_id: reviewer_id.into(),
            verdict: Verdict::Lgtm,
            issues: Vec::new(),
            suggested_patch: None,
            cost: None,
            raw_text: raw_text.into(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    Lgtm,
    RequestChanges,
    Block,
}

impl Verdict {
    pub fn is_terminal_success(&self) -> bool {
        matches!(self, Self::Lgtm)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Issue {
    pub severity: IssueSeverity,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum IssueSeverity {
    Info,
    Warning,
    Blocking,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum AgentEvent {
    Stdout { agent_id: String, line: String },
    Stderr { agent_id: String, line: String },
    Tool { agent_id: String, call: ToolCall },
    Done { agent_id: String },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RoundRecord {
    pub round: u8,
    pub lead: AgentOutput,
    pub reviewer: ReviewerFeedback,
}
