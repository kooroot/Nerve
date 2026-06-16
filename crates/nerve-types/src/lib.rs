use chrono::{DateTime, Utc};
use nerve_patch::NvPatch;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
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

    pub fn accept_with_nits(
        reviewer_id: impl Into<String>,
        issues: Vec<Issue>,
        raw_text: impl Into<String>,
    ) -> Self {
        Self {
            reviewer_id: reviewer_id.into(),
            verdict: Verdict::AcceptWithNits,
            issues,
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
    AcceptWithNits,
    RequestChanges,
    Block,
}

impl Verdict {
    pub fn is_terminal_success(&self) -> bool {
        matches!(self, Self::Lgtm)
    }

    /// Whether this verdict counts as an acceptance, given whether the active
    /// review strictness permits cosmetic nits. `Lgtm` always accepts;
    /// `AcceptWithNits` accepts only when nits are permitted (High strictness
    /// degrades it to a change request). Strictness gating lives in the caller.
    pub fn accepts_under(&self, nits_permitted: bool) -> bool {
        match self {
            Self::Lgtm => true,
            Self::AcceptWithNits => nits_permitted,
            _ => false,
        }
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
    Fail {
        reason: String,
        /// S7 distance-to-goal signal: the deterministic check's pass-ratio in
        /// PERMILLE (0..=1000) when its output exposed a recognizable test
        /// summary, else absent. Permille (not `f64`) keeps `CheckResult: Eq`,
        /// which `RoundRecord`/`RpcEnvelope` depend on. Optional + skipped-when-
        /// `None` so older consumers stay wire-compatible (minor schema bump).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        progress: Option<u16>,
    },
    Skipped,
}

impl CheckResult {
    pub fn is_pass(&self) -> bool {
        matches!(self, Self::Pass)
    }

    /// Distance-to-goal progress in `[0.0, 1.0]` (S7): `Pass` is fully satisfied
    /// (`1.0`), `Skipped` carries no signal (`None`), and `Fail` reports the
    /// parsed pass-ratio when the check emitted a recognizable test summary.
    /// This is additive telemetry and a stall hint — it NEVER affects
    /// acceptance, which still requires a real [`CheckResult::Pass`].
    pub fn progress(&self) -> Option<f64> {
        match self {
            Self::Pass => Some(1.0),
            Self::Skipped => None,
            Self::Fail { progress, .. } => progress.map(|p| f64::from(p) / 1000.0),
        }
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
pub const RPC_SCHEMA_VERSION: &str = "1.2.0";

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
    // v1.0 — Tier 3h/3i/3j event kinds.
    pub const MAYOR_DISPATCHED: &str = "mayor.dispatched";
    pub const MAYOR_SHUTTING_DOWN: &str = "mayor.shutting_down";
    pub const PATROL_CLAIMED: &str = "patrol.claimed";
    pub const PATROL_FINISHED: &str = "patrol.finished";
    pub const PATROL_HEARTBEAT: &str = "patrol.heartbeat";
    pub const MCP_TOOL_INVOKED: &str = "mcp.tool_invoked";
    pub const MCP_TOOL_RESULT: &str = "mcp.tool_result";
    pub const SESSION_FORKED: &str = "session.forked";
    // S9 — daemon v2: in-flight run summary, served from the S8 round
    // checkpoints by the `status` command. Additive kind (minor-compatible:
    // older consumers ignore unknown kinds), NOT a schema-version bump.
    pub const SESSION_STATUS: &str = "session.status";
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

// ---------------------------------------------------------------------------
// v1.0 — Tier 3i: MCP (Model Context Protocol) client types
// ---------------------------------------------------------------------------

/// Wire transport used by an [`McpServerSpec`]. Only stdio (process spawn +
/// JSON-RPC 2.0 over stdin/stdout) is supported in v1.0; the `kind`-tagged
/// representation reserves room for future transports (http, websocket)
/// without breaking compatibility.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum McpTransport {
    #[default]
    Stdio,
}

/// Which orchestrator role(s) may dispatch tools from a given MCP server.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum McpRole {
    #[default]
    ReviewerOnly,
    LeadOnly,
    Both,
}

/// Default transport for newly-defined MCP server specs (stdio in v1.0).
pub fn default_mcp_transport() -> McpTransport {
    McpTransport::Stdio
}

/// Default role for newly-defined MCP server specs (reviewer-only).
pub fn default_mcp_role() -> McpRole {
    McpRole::ReviewerOnly
}

/// Serde default helper for boolean fields that should default to `true`.
pub fn default_true() -> bool {
    true
}

/// Tier 3i (v1.0): MCP server specification used by the per-profile
/// `mcp.servers` config block.
///
/// `command` is the argv vector launched via `Stdio::piped` by
/// `nerve-adapter::McpClient`; only the stdio transport is supported in v1.0.
/// `allowed_tools` is intersected with the global `mcp.allow_tools` allowlist
/// at dispatch time. `read_only` (default `true`) flips the orchestrator's
/// write-tool guard on, refusing tools whose name matches
/// `mcp.write_tool_patterns`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct McpServerSpec {
    pub name: String,
    pub command: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default = "default_mcp_transport")]
    pub transport: McpTransport,
    #[serde(default)]
    pub allowed_tools: Vec<String>,
    #[serde(default = "default_mcp_role")]
    pub role: McpRole,
    #[serde(default = "default_true")]
    pub read_only: bool,
}

/// Catalog entry describing a single MCP tool advertised by a server.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct McpToolInfo {
    pub server: String,
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub input_schema: Option<serde_json::Value>,
}

/// A reviewer/lead-initiated MCP tool invocation, forwarded through the
/// orchestrator to `nerve-adapter::McpClient`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct McpToolCall {
    pub server: String,
    pub tool: String,
    pub arguments: serde_json::Value,
}

/// Result of an MCP tool invocation.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct McpToolResult {
    pub server: String,
    pub tool: String,
    pub content: serde_json::Value,
    #[serde(default)]
    pub is_error: bool,
    #[serde(default)]
    pub duration_ms: u64,
}

// ---------------------------------------------------------------------------
// v1.0 — Tier 3h: session fork / branch
// ---------------------------------------------------------------------------

/// Persistent representation of a session's place in a fork tree.
///
/// Persisted as `.nerve/sessions/<parent-id>/<child-id>.json` for forked
/// children; the root session uses `parent_id == None`. Parent sessions are
/// immutable after a fork — children carry the full history copy needed to
/// resume independently (proposal §3 Tier 3h).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionTree {
    pub session_id: String,
    #[serde(default)]
    pub parent_id: Option<String>,
    #[serde(default)]
    pub branched_at_round: Option<u32>,
    #[serde(default)]
    pub branched_from_patch_sha: Option<String>,
    pub forked_at: DateTime<Utc>,
    pub name: String,
    #[serde(default)]
    pub children: Vec<String>,
}

// NOTE: The patch index lives in `nerve-core::store::PatchRecord`, which
// already carries a `session_id` column. There is no `PatchIndexEntry`
// struct in `nerve-types` to extend here — keep this comment as a pointer
// for downstream readers wiring the Tier 3h "patch index scoped per session"
// behaviour at the storage layer.
// TODO(tier-3h): when the patch-index schema migrates into `nerve-types`,
// add `#[serde(default)] pub session_id: Option<String>` then.

// ---------------------------------------------------------------------------
// v1.0 — Tier 3j: Mayor / Patrol multi-instance
// ---------------------------------------------------------------------------

/// A unit of work dispatched by the Mayor to an idle Patrol worker.
///
/// Persisted in `.nerve/queue/pending/<ulid>.json`. Patrols transition the
/// state machine by atomic-renaming the file into per-worker `claimed/`
/// subdirs; results land in `.nerve/results/<ulid>.json`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PatrolTask {
    pub task_id: String,
    pub prompt: String,
    pub cwd: PathBuf,
    #[serde(default)]
    pub adapter_role_hint: Option<String>,
    #[serde(default)]
    pub budget_microusd: Option<u64>,
    #[serde(default)]
    pub max_rounds: Option<u32>,
    pub created_at: DateTime<Utc>,
}

/// Lifecycle states a [`PatrolTask`] traverses on the queue.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum PatrolTaskState {
    Pending,
    Claimed {
        patrol_id: String,
        claimed_at: DateTime<Utc>,
    },
    Done {
        patrol_id: String,
        finished_at: DateTime<Utc>,
    },
    Failed {
        patrol_id: String,
        error: String,
    },
}

/// Patrol-emitted result for a single dispatched task.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PatrolResult {
    pub task_id: String,
    pub patrol_id: String,
    pub verdict: Verdict,
    pub cost_microusd: u64,
    pub tokens: u64,
    #[serde(default)]
    pub patch_sha: Option<String>,
    #[serde(default)]
    pub error: Option<String>,
    pub finished_at: DateTime<Utc>,
}

/// Mayor-side aggregated view exposed via `/mayor status`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct MayorStatus {
    #[serde(default)]
    pub pending: u32,
    #[serde(default)]
    pub claimed: u32,
    #[serde(default)]
    pub done: u32,
    #[serde(default)]
    pub failed: u32,
    #[serde(default)]
    pub active_patrols: Vec<String>,
    #[serde(default)]
    pub total_cost_microusd: u64,
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
    fn session_tree_serde_round_trip() {
        let tree = SessionTree {
            session_id: "01HZZZULID".to_string(),
            parent_id: Some("01ROOTULID".to_string()),
            branched_at_round: Some(3),
            branched_from_patch_sha: Some("deadbeef".to_string()),
            forked_at: Utc::now(),
            name: "child-fork".to_string(),
            children: vec!["01CHILDA".to_string(), "01CHILDB".to_string()],
        };
        let wire = serde_json::to_string(&tree).expect("serialize session tree");
        let decoded: SessionTree = serde_json::from_str(&wire).expect("decode session tree");
        assert_eq!(decoded, tree);
    }

    #[test]
    fn session_tree_defaults_root_with_missing_fields() {
        // Root sessions must decode from a minimal payload with only the
        // required `session_id`, `forked_at`, and `name` fields.
        let now = Utc::now();
        let wire = json!({
            "session_id": "root",
            "forked_at": now,
            "name": "root",
        })
        .to_string();
        let tree: SessionTree = serde_json::from_str(&wire).expect("decode minimal session tree");
        assert!(tree.parent_id.is_none());
        assert!(tree.branched_at_round.is_none());
        assert!(tree.branched_from_patch_sha.is_none());
        assert!(tree.children.is_empty());
    }

    #[test]
    fn mcp_server_spec_minimal_uses_defaults() {
        let wire = json!({
            "name": "lsp",
            "command": ["/usr/bin/rust-analyzer"],
        })
        .to_string();
        let spec: McpServerSpec =
            serde_json::from_str(&wire).expect("decode minimal mcp server spec");
        assert_eq!(spec.name, "lsp");
        assert_eq!(spec.command, vec!["/usr/bin/rust-analyzer".to_string()]);
        assert!(spec.env.is_empty());
        assert_eq!(spec.transport, McpTransport::Stdio);
        assert_eq!(spec.role, McpRole::ReviewerOnly);
        assert!(spec.allowed_tools.is_empty());
        assert!(spec.read_only);
    }

    #[test]
    fn mcp_server_spec_round_trip_explicit_fields() {
        let mut env = BTreeMap::new();
        env.insert("RUST_LOG".to_string(), "info".to_string());
        let spec = McpServerSpec {
            name: "ra".to_string(),
            command: vec!["rust-analyzer".to_string(), "--stdio".to_string()],
            env,
            transport: McpTransport::Stdio,
            allowed_tools: vec!["definition".to_string(), "references".to_string()],
            role: McpRole::Both,
            read_only: false,
        };
        let wire = serde_json::to_string(&spec).expect("serialize mcp server spec");
        let decoded: McpServerSpec = serde_json::from_str(&wire).expect("decode mcp server spec");
        assert_eq!(decoded, spec);
    }

    #[test]
    fn mcp_transport_serializes_with_kind_tag() {
        let value = serde_json::to_value(McpTransport::Stdio).expect("serialize transport");
        assert_eq!(value, json!({ "kind": "stdio" }));
        let decoded: McpTransport = serde_json::from_value(value).expect("decode tagged transport");
        assert_eq!(decoded, McpTransport::Stdio);
    }

    #[test]
    fn mcp_role_serializes_snake_case() {
        let value = serde_json::to_value(McpRole::ReviewerOnly).expect("serialize role");
        assert_eq!(value, json!("reviewer_only"));
    }

    #[test]
    fn mcp_default_helpers_match_spec() {
        assert_eq!(default_mcp_transport(), McpTransport::Stdio);
        assert_eq!(default_mcp_role(), McpRole::ReviewerOnly);
        assert!(default_true());
    }

    #[test]
    fn mcp_tool_info_minimal_decodes() {
        let wire = json!({
            "server": "lsp",
            "name": "find_references",
        })
        .to_string();
        let info: McpToolInfo = serde_json::from_str(&wire).expect("decode tool info");
        assert_eq!(info.server, "lsp");
        assert_eq!(info.name, "find_references");
        assert!(info.description.is_none());
        assert!(info.input_schema.is_none());
    }

    #[test]
    fn mcp_tool_call_and_result_round_trip() {
        let call = McpToolCall {
            server: "lsp".to_string(),
            tool: "find_references".to_string(),
            arguments: json!({ "uri": "file:///main.rs", "position": [10, 4] }),
        };
        let wire = serde_json::to_string(&call).expect("serialize tool call");
        let decoded: McpToolCall = serde_json::from_str(&wire).expect("decode tool call");
        assert_eq!(decoded, call);

        let result = McpToolResult {
            server: "lsp".to_string(),
            tool: "find_references".to_string(),
            content: json!([{ "uri": "file:///main.rs", "range": [10, 4, 10, 12] }]),
            is_error: false,
            duration_ms: 42,
        };
        let wire = serde_json::to_string(&result).expect("serialize tool result");
        let decoded: McpToolResult = serde_json::from_str(&wire).expect("decode tool result");
        assert_eq!(decoded, result);
    }

    #[test]
    fn mcp_tool_result_defaults_on_missing_fields() {
        let wire = json!({
            "server": "lsp",
            "tool": "hover",
            "content": null,
        })
        .to_string();
        let result: McpToolResult =
            serde_json::from_str(&wire).expect("decode minimal tool result");
        assert!(!result.is_error);
        assert_eq!(result.duration_ms, 0);
    }

    #[test]
    fn patrol_task_minimal_decodes_with_defaults() {
        let now = Utc::now();
        let wire = json!({
            "task_id": "01HZ",
            "prompt": "fix the lint",
            "cwd": "/tmp/work",
            "created_at": now,
        })
        .to_string();
        let task: PatrolTask = serde_json::from_str(&wire).expect("decode minimal patrol task");
        assert_eq!(task.task_id, "01HZ");
        assert_eq!(task.prompt, "fix the lint");
        assert_eq!(task.cwd, PathBuf::from("/tmp/work"));
        assert!(task.adapter_role_hint.is_none());
        assert!(task.budget_microusd.is_none());
        assert!(task.max_rounds.is_none());
    }

    #[test]
    fn patrol_task_state_round_trip_each_variant() {
        for state in [
            PatrolTaskState::Pending,
            PatrolTaskState::Claimed {
                patrol_id: "p-1".to_string(),
                claimed_at: Utc::now(),
            },
            PatrolTaskState::Done {
                patrol_id: "p-1".to_string(),
                finished_at: Utc::now(),
            },
            PatrolTaskState::Failed {
                patrol_id: "p-2".to_string(),
                error: "boom".to_string(),
            },
        ] {
            let wire = serde_json::to_string(&state).expect("serialize patrol task state");
            let decoded: PatrolTaskState =
                serde_json::from_str(&wire).expect("decode patrol task state");
            assert_eq!(decoded, state);
        }
    }

    #[test]
    fn patrol_result_round_trip() {
        let result = PatrolResult {
            task_id: "01HZ".to_string(),
            patrol_id: "patrol-3".to_string(),
            verdict: Verdict::Lgtm,
            cost_microusd: 1_234,
            tokens: 5_678,
            patch_sha: Some("cafebabe".to_string()),
            error: None,
            finished_at: Utc::now(),
        };
        let wire = serde_json::to_string(&result).expect("serialize patrol result");
        let decoded: PatrolResult = serde_json::from_str(&wire).expect("decode patrol result");
        assert_eq!(decoded, result);
    }

    #[test]
    fn mayor_status_default_is_empty() {
        let status = MayorStatus::default();
        assert_eq!(status.pending, 0);
        assert_eq!(status.claimed, 0);
        assert_eq!(status.done, 0);
        assert_eq!(status.failed, 0);
        assert!(status.active_patrols.is_empty());
        assert_eq!(status.total_cost_microusd, 0);
    }

    #[test]
    fn mayor_status_decodes_from_empty_object() {
        let status: MayorStatus = serde_json::from_str("{}").expect("decode empty mayor status");
        assert_eq!(status, MayorStatus::default());
    }

    #[test]
    fn v1_rpc_kinds_are_distinct_and_well_formed() {
        let kinds = [
            rpc_kinds::MAYOR_DISPATCHED,
            rpc_kinds::MAYOR_SHUTTING_DOWN,
            rpc_kinds::PATROL_CLAIMED,
            rpc_kinds::PATROL_FINISHED,
            rpc_kinds::PATROL_HEARTBEAT,
            rpc_kinds::MCP_TOOL_INVOKED,
            rpc_kinds::MCP_TOOL_RESULT,
            rpc_kinds::SESSION_FORKED,
        ];
        // No duplicates, each contains a namespace prefix.
        let mut seen = std::collections::HashSet::new();
        for k in kinds {
            assert!(k.contains('.'), "kind `{k}` must be namespaced");
            assert!(seen.insert(k), "kind `{k}` listed twice");
        }
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

    #[test]
    fn verdict_accept_with_nits_serde_round_trip() {
        let wire = serde_json::to_string(&Verdict::AcceptWithNits).expect("serialize verdict");
        assert_eq!(wire, "\"accept_with_nits\"");
        let decoded: Verdict = serde_json::from_str(&wire).expect("decode verdict");
        assert_eq!(decoded, Verdict::AcceptWithNits);
    }

    #[test]
    fn accept_with_nits_is_not_terminal_success_but_accepts() {
        assert!(!Verdict::AcceptWithNits.is_terminal_success());
        // Strictness-aware acceptance: nits accept only when permitted.
        assert!(Verdict::AcceptWithNits.accepts_under(true));
        assert!(!Verdict::AcceptWithNits.accepts_under(false));
        // Lgtm accepts regardless of strictness; failures never accept.
        assert!(Verdict::Lgtm.accepts_under(true));
        assert!(Verdict::Lgtm.accepts_under(false));
        assert!(!Verdict::RequestChanges.accepts_under(true));
        assert!(!Verdict::Block.accepts_under(true));
    }

    #[test]
    fn round_record_with_accept_with_nits_round_trips() {
        let rec = RoundRecord {
            round: 2,
            lead: AgentOutput::text("lead", "patch"),
            reviewer: ReviewerFeedback::accept_with_nits(
                "rev",
                vec![Issue {
                    severity: IssueSeverity::Info,
                    message: "tighten naming".to_string(),
                }],
                "ACCEPT_WITH_NITS: tighten naming",
            ),
            check_result: Some(CheckResult::Pass),
            patch_sha: Some("abc123".to_string()),
            envelope_id: None,
        };
        let wire = serde_json::to_string(&rec).expect("serialize round");
        let decoded: RoundRecord = serde_json::from_str(&wire).expect("decode round");
        assert_eq!(decoded, rec);
        assert_eq!(decoded.reviewer.verdict, Verdict::AcceptWithNits);
        assert_eq!(decoded.reviewer.issues[0].severity, IssueSeverity::Info);
    }
}
