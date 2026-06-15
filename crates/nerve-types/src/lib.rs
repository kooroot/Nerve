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
#[serde(rename_all = "snake_case")]
pub enum CheckResult {
    Pass,
    Fail { reason: String },
    Skipped,
}

impl CheckResult {
    pub fn is_pass(&self) -> bool {
        matches!(self, Self::Pass)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RoundRecord {
    pub round: u8,
    pub lead: AgentOutput,
    pub reviewer: ReviewerFeedback,
    #[serde(default)]
    pub check_result: Option<CheckResult>,
    #[serde(default)]
    pub patch_sha: Option<String>,
    #[serde(default)]
    pub envelope_id: Option<String>,
}

/// Semantic version of the RPC event-streaming envelope schema.
///
/// Bumped on breaking schema changes. Minor compatible: unknown fields in
/// payload or envelope must be silently ignored by older consumers.
pub const RPC_SCHEMA_VERSION: &str = "1.0.0";

/// Versioned wire envelope used for RPC event streaming between the core
/// runtime and external consumers (TUI, plugins, telemetry sinks).
///
/// `payload` is intentionally untyped (`serde_json::Value`) so that the
/// envelope crate stays decoupled from event-specific payload schemas.
/// The `kind` discriminant points consumers at the right payload shape;
/// see [`rpc_kinds`] for the catalog of stable event names.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RpcEnvelope {
    pub schema_version: String,
    pub kind: String,
    pub payload: serde_json::Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub envelope_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub emitted_at: Option<DateTime<Utc>>,
}

impl RpcEnvelope {
    /// Construct an envelope at the current [`RPC_SCHEMA_VERSION`].
    pub fn new(kind: impl Into<String>, payload: serde_json::Value) -> Self {
        Self {
            schema_version: RPC_SCHEMA_VERSION.to_string(),
            kind: kind.into(),
            payload,
            envelope_id: None,
            emitted_at: None,
        }
    }

    /// Attach an envelope id (typically a UUID) for tracing.
    pub fn with_envelope_id(mut self, envelope_id: impl Into<String>) -> Self {
        self.envelope_id = Some(envelope_id.into());
        self
    }

    /// Attach an emission timestamp.
    pub fn with_emitted_at(mut self, ts: DateTime<Utc>) -> Self {
        self.emitted_at = Some(ts);
        self
    }

    /// Attach fresh runtime metadata for a newly emitted envelope.
    pub fn with_fresh_metadata(self) -> Self {
        self.with_envelope_id(ulid::Ulid::new().to_string())
            .with_emitted_at(Utc::now())
    }
}

/// Stable catalog of RPC event kinds emitted by the core runtime.
///
/// These names are part of the public envelope contract; renaming any of
/// them constitutes a breaking change and requires bumping
/// [`RPC_SCHEMA_VERSION`].
pub mod rpc_kinds {
    pub const LEAD_STDOUT: &str = "lead.stdout_chunk";
    pub const REVIEWER_STDOUT: &str = "reviewer.stdout_chunk";
    pub const GOAL_CHECK_START: &str = "goal_check.start";
    pub const GOAL_CHECK_OUTPUT: &str = "goal_check.output";
    pub const GOAL_CHECK_DONE: &str = "goal_check.done";
    pub const ROUND_STARTED: &str = "round.started";
    pub const ROUND_ENDED: &str = "round.ended";
    pub const BUDGET_CHANGED: &str = "budget.changed";
    pub const PATCH_APPLIED: &str = "patch.applied";
    pub const PATCH_DISCARDED: &str = "patch.discarded";
    pub const SESSION_STARTED: &str = "session.started";
    pub const SESSION_ENDED: &str = "session.ended";
    pub const PLAN_PROPOSED: &str = "plan.proposed";
}

/// Output of a `/plan` (Plan mode, read-only analysis) run.
///
/// Plan mode is forbidden from emitting [`NvPatch`] artifacts; the result
/// is a structured Markdown document plus reviewer commentary.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PlanReport {
    pub task_id: String,
    pub plan_markdown: String,
    pub reviewer_feedback: String,
    #[serde(default)]
    pub estimated_loc: Option<u64>,
    #[serde(default)]
    pub estimated_files: Vec<PathBuf>,
    #[serde(default)]
    pub cost: Option<UsageStats>,
    pub finished_at: DateTime<Utc>,
}

/// Aggregated runtime state consumed by the ratatui TUI status pane.
///
/// All counters are monotonically updated by the core via the broadcast
/// channel feeding [`RpcEnvelope`]s; consumers should treat snapshots as
/// idempotent renders rather than diff inputs.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct TuiState {
    #[serde(default)]
    pub round: u8,
    #[serde(default)]
    pub max_rounds: u8,
    #[serde(default)]
    pub last_verdict: Option<Verdict>,
    #[serde(default)]
    pub last_check_result: Option<CheckResult>,
    #[serde(default)]
    pub cumulative_cost_microusd: u64,
    #[serde(default)]
    pub cumulative_tokens: u64,
    #[serde(default)]
    pub budget_cost_cap: Option<u64>,
    #[serde(default)]
    pub budget_tokens_cap: Option<u64>,
    #[serde(default)]
    pub active_goal: Option<String>,
    #[serde(default)]
    pub current_stage: String,
    #[serde(default)]
    pub elapsed_secs: u64,
    #[serde(default)]
    pub no_progress_count: u8,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn envelope_serde_round_trip() {
        let env = RpcEnvelope::new(
            rpc_kinds::LEAD_STDOUT,
            json!({ "agent_id": "lead", "chunk": "hello" }),
        )
        .with_envelope_id("evt-1");
        let wire = serde_json::to_string(&env).expect("serialize envelope");
        let decoded: RpcEnvelope = serde_json::from_str(&wire).expect("decode envelope");
        assert_eq!(decoded, env);
        assert_eq!(decoded.schema_version, RPC_SCHEMA_VERSION);
        assert_eq!(decoded.kind, rpc_kinds::LEAD_STDOUT);
        assert_eq!(decoded.envelope_id.as_deref(), Some("evt-1"));
    }

    #[test]
    fn envelope_fresh_metadata_sets_id_and_timestamp() {
        let env = RpcEnvelope::new(rpc_kinds::SESSION_STARTED, json!({})).with_fresh_metadata();

        assert!(env.envelope_id.is_some());
        assert!(env.emitted_at.is_some());
    }

    #[test]
    fn envelope_with_extra_unknown_field_ignored() {
        // Older consumers must accept envelopes that grew new optional
        // top-level fields in a minor revision (semver compat).
        let wire = json!({
            "schema_version": RPC_SCHEMA_VERSION,
            "kind": rpc_kinds::ROUND_STARTED,
            "payload": { "round": 1 },
            "future_field": "ignored-by-old-consumers",
            "another": { "nested": true }
        })
        .to_string();
        let env: RpcEnvelope = serde_json::from_str(&wire).expect("decode forward-compat envelope");
        assert_eq!(env.kind, rpc_kinds::ROUND_STARTED);
        assert_eq!(env.payload, json!({ "round": 1 }));
        assert!(env.envelope_id.is_none());
        assert!(env.emitted_at.is_none());
    }

    #[test]
    fn tui_state_default_empty() {
        let state = TuiState::default();
        assert_eq!(state.round, 0);
        assert_eq!(state.max_rounds, 0);
        assert!(state.last_verdict.is_none());
        assert!(state.last_check_result.is_none());
        assert_eq!(state.cumulative_cost_microusd, 0);
        assert_eq!(state.cumulative_tokens, 0);
        assert!(state.budget_cost_cap.is_none());
        assert!(state.budget_tokens_cap.is_none());
        assert!(state.active_goal.is_none());
        assert!(state.current_stage.is_empty());
        assert_eq!(state.elapsed_secs, 0);
        assert_eq!(state.no_progress_count, 0);
    }

    #[test]
    fn plan_report_serde() {
        let report = PlanReport {
            task_id: "task-7".to_string(),
            plan_markdown: "# Objective\n...".to_string(),
            reviewer_feedback: "LGTM".to_string(),
            estimated_loc: Some(120),
            estimated_files: vec![PathBuf::from("crates/nerve-core/src/lib.rs")],
            cost: Some(UsageStats {
                input_tokens: 10,
                output_tokens: 20,
                estimated_cost_microusd: Some(42),
            }),
            finished_at: Utc::now(),
        };
        let wire = serde_json::to_string(&report).expect("serialize plan report");
        let decoded: PlanReport = serde_json::from_str(&wire).expect("decode plan report");
        assert_eq!(decoded, report);
    }

    #[test]
    fn round_record_envelope_id_optional_default() {
        let wire = json!({
            "round": 1,
            "lead": {
                "agent_id": "lead",
                "raw_text": "",
                "proposed_patch": null,
                "tool_calls": [],
                "cost": null
            },
            "reviewer": {
                "reviewer_id": "rev",
                "verdict": "lgtm",
                "issues": [],
                "suggested_patch": null,
                "cost": null,
                "raw_text": ""
            }
        })
        .to_string();
        let rec: RoundRecord = serde_json::from_str(&wire).expect("decode legacy round");
        assert!(rec.envelope_id.is_none());
    }
}
