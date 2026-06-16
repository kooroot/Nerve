use anyhow::{Context, Result};
use globset::{Glob, GlobSetBuilder};
use nerve_types::{McpServerSpec, Task};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

pub mod goal;
pub mod goal_intent;
pub use goal::{ConfigError, GoalSpec};
pub use goal_intent::GoalIntent;

const DEFAULT_CONFIG: &str = include_str!("../../../nerve.config.json");

/// Where an active [`Config`] was loaded from. Used to decide whether config
/// that enables an *executing* built-in verifier (S4 `Auto`/`Command` mode) is
/// operator-controlled (trusted) or project-controlled — a cloned repo could
/// ship `./nerve.config.json` to opt itself into running code.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ConfigSource {
    /// Repo-local `./nerve.config.json` — authored by whoever controls the
    /// working tree, not necessarily the operator. Untrusted for code execution.
    Project,
    /// Operator's `~/.config/nerve/config.json`. Trusted (operator-controlled).
    User,
    /// Embedded built-in default or in-memory construction (`from_json_str`).
    /// Trusted (the shipped default keeps the verifier `Off`).
    #[default]
    Default,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub orchestration: Orchestration,
    pub roles: Roles,
    #[serde(default)]
    pub profiles: Vec<Profile>,
    #[serde(default)]
    pub templates: Vec<PromptTemplate>,
    #[serde(default)]
    pub ui: UiConfig,
    #[serde(default)]
    pub daemon: DaemonConfig,
    // Tier 3g (v0.5.0): ratatui-based 3-pane TUI configuration.
    #[serde(default)]
    pub tui: TuiConfig,
    /// Provenance of this config (S4 trust boundary). Never serialized; defaults
    /// to [`ConfigSource::Default`] for in-memory construction and is stamped by
    /// [`Config::load_from`] based on which file supplied the config.
    #[serde(skip)]
    source: ConfigSource,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Orchestration {
    #[serde(default = "default_strategy")]
    pub default_strategy: Strategy,
    #[serde(default = "default_max_refinement_rounds")]
    pub max_refinement_rounds: u8,
    #[serde(default = "default_conflict_policy")]
    pub conflict_policy: ConflictPolicy,
    #[serde(default)]
    pub max_total_tokens: Option<u64>,
    #[serde(default)]
    pub max_estimated_cost_microusd: Option<u64>,
    // sec-1 #3: /goal evaluator env whitelist (names only; value "" = inherit from parent).
    #[serde(default)]
    pub check_env: Vec<String>,
    // sec-1 #7: streaming output cap for /goal check_cmd stdout/stderr.
    #[serde(default = "Orchestration::default_check_output_cap_bytes")]
    pub check_output_cap_bytes: usize,
    // Adapter spawn guard knobs; None falls back to nerve-adapter defaults.
    #[serde(default)]
    pub adapter_timeout_secs: Option<u64>,
    #[serde(default)]
    pub adapter_max_output_bytes: Option<usize>,
    // S2: additional spawn attempts on transient OS spawn failures
    // (EAGAIN/ENOMEM/ETXTBSY/EINTR). None falls back to the adapter default;
    // Some(0) disables retries.
    #[serde(default)]
    pub adapter_spawn_retries: Option<u32>,
    // sec-3 #1: hard ceiling that user `/budget raising` cannot exceed.
    #[serde(default)]
    pub budget_cost_microusd_ceiling: Option<u64>,
    #[serde(default)]
    pub budget_tokens_ceiling: Option<u64>,
    // sec-gap-5: optional parent-level resource limits applied before spawning
    // /goal check_cmd children. Linux honours all fields; macOS supports
    // RLIMIT_AS / RLIMIT_FSIZE / RLIMIT_CPU; nproc is best-effort.
    #[serde(default)]
    pub check_ulimit: Option<CheckUlimit>,
    // Tier 2d (v0.3.0): opt-in worktree-isolated /apply path.
    #[serde(default)]
    pub worktree_apply: bool,
    // Tier 3j (v1.0): mayor + patrol multi-instance dispatcher. None preserves
    // legacy byte-identical orchestration serialization.
    #[serde(default)]
    pub mayor_patrol: Option<MayorPatrolConfig>,
    // S4: built-in verification gate. When no explicit `/goal` is set, an
    // opted-in `auto`/`command` mode supplies the project's test/build command
    // as the deterministic check so acceptance never rests on reviewer opinion
    // alone. Defaults to `off` (never executes repo code without consent).
    #[serde(default)]
    pub builtin_verifier: BuiltinVerifierConfig,
    // S5: OS execution sandbox for deterministic checks (`/goal` and the
    // built-in verifier both run project-controlled code). Confines filesystem
    // writes and network on top of the existing resource guards. Defaults to
    // `off` (no behavior change). It never fabricates a success: it can only
    // refuse (→ Fail) or wrap, and the gate still keys Pass on the child's real
    // exit status. (It does not promise confined/unconfined exit-code parity —
    // confinement is observable; see `nerve_core::sandbox` module docs.)
    #[serde(default)]
    pub sandbox: SandboxConfig,
    // S10: how the live crossfire reviewer signal feeds back into the loop
    // (off=record-only / redirect=steer next refine / halt=redirect + block on a
    // live Block). Defaults to `off` → byte-identical to the prior advisory-only
    // behavior. Both non-off actions are rejection-direction only and never
    // weaken the deterministic acceptance gate.
    #[serde(default)]
    pub crossfire_action: CrossfireAction,
    // S12: auto-mode classifier gate. A DETERMINISTIC patch-risk classifier that
    // can only DOWNGRADE a would-be apply to dry-run (implement) when the final
    // patch looks risky — it never upgrades dry-run→apply and never weakens the
    // deterministic acceptance gate. Defaults to `off` → byte-identical to today
    // (the operator's `--apply`/grant alone decides). Rejection-direction only.
    #[serde(default)]
    pub apply_classifier: ApplyClassifierConfig,
}

/// S4: how the built-in verifier behaves when a run has no explicit `/goal`
/// deterministic check.
///
/// Defaults to [`Off`](BuiltinVerifierMode::Off): running a project's test/build
/// command executes project-controlled code (Cargo build scripts, `package.json`
/// scripts, …), and the existing guards (env whitelist, timeout, output cap,
/// optional ulimit) are *resource* limits, not filesystem/network isolation.
/// Executing repo code is an explicit operator opt-in, never a default (roadmap
/// anti-pattern #1: risky auto-execution must be loud and opt-in). The CLI
/// loudly warns whenever no gate is active so acceptance is never *silently*
/// reduced to reviewer opinion. For filesystem/network confinement of the
/// executed code, pair this with the OS execution sandbox ([`SandboxConfig`],
/// roadmap S5); `sandbox.mode=required` fails closed when no backend exists.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum BuiltinVerifierMode {
    /// Disable the built-in verifier; absent a `/goal`, the deterministic check
    /// is `Skipped` (acceptance = reviewer verdict only — surfaced loudly by the
    /// CLI). The default: Nerve never executes repo code without explicit
    /// consent.
    #[default]
    Off,
    /// Opt in to detecting the project's test/build command from marker files
    /// (Cargo.toml, go.mod, package.json) and running it as the deterministic
    /// gate. Executes project-controlled code — enable only when you trust the
    /// tree, or confine it with the OS sandbox ([`SandboxConfig`], roadmap S5).
    Auto,
    /// Use the operator-supplied `command` verbatim as the gate. Like `Auto`,
    /// this executes code; the operator names the exact argv they consent to.
    Command,
}

/// S4: configuration for the always-on built-in verifier.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct BuiltinVerifierConfig {
    #[serde(default)]
    pub mode: BuiltinVerifierMode,
    /// Explicit argv used when `mode == Command`. `command[0]` must be a
    /// PATH-searchable program name (same rule as `/goal check_cmd`).
    #[serde(default)]
    pub command: Vec<String>,
    /// Wall-clock timeout for one verifier invocation. Build/test commands are
    /// slower than a typical `/goal`, so this defaults higher.
    #[serde(default = "default_builtin_verifier_timeout_secs")]
    pub timeout_secs: u64,
}

impl Default for BuiltinVerifierConfig {
    fn default() -> Self {
        Self {
            mode: BuiltinVerifierMode::default(),
            command: Vec::new(),
            timeout_secs: default_builtin_verifier_timeout_secs(),
        }
    }
}

impl BuiltinVerifierConfig {
    pub fn validate(&self) -> Result<()> {
        if self.timeout_secs == 0 {
            anyhow::bail!("timeout_secs must be greater than 0");
        }
        if matches!(self.mode, BuiltinVerifierMode::Command) {
            let Some(program) = self.command.first() else {
                anyhow::bail!("command must be a non-empty argv when mode = command");
            };
            // Same PATH-safety rule as GoalSpec::check_cmd[0] (sec-1 #1: argv
            // only, no shell, PATH lookup only — no `/`, `\`, or `..`). Also
            // reject a leading `-` so a sandbox wrapper can't mis-parse it as one
            // of its own options (S5 sandbox-transparency).
            if program.is_empty()
                || program.starts_with('-')
                || program.contains('/')
                || program.contains('\\')
                || program.contains("..")
            {
                anyhow::bail!(
                    "command[0] `{program}` must be a PATH-searchable program name (no leading `-`, `/`, `\\`, or `..`)"
                );
            }
        }
        Ok(())
    }
}

fn default_builtin_verifier_timeout_secs() -> u64 {
    600
}

/// S5: how the OS execution sandbox confines a deterministic check.
///
/// Both `/goal` checks and the built-in verifier run project-controlled code.
/// The existing guards (env whitelist, timeout, output cap, ulimit) are
/// *resource* limits; the sandbox adds *filesystem/network confinement* via an
/// OS backend (macOS Seatbelt `sandbox-exec`, Linux `bwrap`). It never
/// *fabricates* a success: it can only refuse (→ `Fail`, fail-closed) or wrap,
/// and the gate still keys `Pass` strictly on the executed command's real exit
/// status. It does NOT promise that a confined run's exit code equals an
/// unconfined run's — confinement is observable to the executed code, inherent
/// to every sandbox; exit-code authority is the operator-trusted check's, which
/// the lead does not control. See the `nerve_core::sandbox` module docs.
///
/// Defaults to [`Off`](SandboxMode::Off) so existing runs are byte-for-byte
/// unchanged; confinement is an explicit opt-in (roadmap anti-pattern #1: risky
/// execution is loud and opt-in, never a silent default).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum SandboxMode {
    /// No OS confinement (current behavior, the default). When the built-in
    /// verifier executes repo code unconfined, the existing S4 loud warning
    /// still applies — acceptance is never *silently* changed.
    #[default]
    Off,
    /// Confine using the best available backend. If no backend is available on
    /// this platform, the check runs UNCONFINED with a loud warning
    /// (best-effort) — choose [`Required`](SandboxMode::Required) for a hard
    /// *fail-closed* guarantee (never runs unconfined).
    Auto,
    /// Confine, or refuse to run. If no backend is available or the sandbox
    /// cannot be established, the check returns `Fail` and the code never
    /// executes unconfined. FAIL CLOSED. NOTE: the confinement itself is
    /// best-effort write+network — it blocks direct out-of-root writes/network
    /// but NOT daemon-mediated side effects (e.g. `defaults`→`cfprefsd`); it
    /// raises the bar against naive escapes, it is not a hard jail for a
    /// determined adversary (use a container/VM for that). See
    /// `nerve_core::sandbox` module docs.
    Required,
}

/// S5: configuration for the OS execution sandbox.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(deny_unknown_fields)]
pub struct SandboxConfig {
    #[serde(default)]
    pub mode: SandboxMode,
    /// Allow the confined check to use the network. Defaults to `false`: a
    /// deterministic verifier should be hermetic. Denying network blocks the
    /// check's DIRECT outbound sockets — the common exfil path — but, like the
    /// write side, it does NOT close daemon-mediated channels (e.g. DNS via
    /// mDNSResponder, which the process reaches over IPC rather than a socket).
    /// It raises the bar against naive exfil; it is not a hard guarantee — see
    /// the `nerve_core::sandbox` module docs. Set `true` for checks that must
    /// fetch (e.g. first-build dependency download). Ignored when `mode = off`.
    #[serde(default)]
    pub allow_network: bool,
}

impl SandboxConfig {
    /// Whether this config requests any OS confinement.
    pub fn is_enabled(&self) -> bool {
        !matches!(self.mode, SandboxMode::Off)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(deny_unknown_fields)]
pub struct CheckUlimit {
    /// RLIMIT_NPROC — max user processes (Linux primary, macOS best-effort).
    #[serde(default)]
    pub nproc: Option<u64>,
    /// RLIMIT_AS — max virtual address space, bytes.
    #[serde(default)]
    pub memory_bytes: Option<u64>,
    /// RLIMIT_FSIZE — max file size the process can create, bytes.
    #[serde(default)]
    pub file_size_bytes: Option<u64>,
    /// RLIMIT_CPU — max CPU seconds.
    #[serde(default)]
    pub cpu_secs: Option<u64>,
}

impl CheckUlimit {
    pub fn is_empty(&self) -> bool {
        self.nproc.is_none()
            && self.memory_bytes.is_none()
            && self.file_size_bytes.is_none()
            && self.cpu_secs.is_none()
    }

    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.nproc == Some(0) {
            return Err(ConfigError::InvalidUlimitValue("nproc"));
        }
        if self.memory_bytes == Some(0) {
            return Err(ConfigError::InvalidUlimitValue("memory_bytes"));
        }
        if self.file_size_bytes == Some(0) {
            return Err(ConfigError::InvalidUlimitValue("file_size_bytes"));
        }
        if self.cpu_secs == Some(0) {
            return Err(ConfigError::InvalidUlimitValue("cpu_secs"));
        }
        Ok(())
    }
}

impl Orchestration {
    pub fn default_check_output_cap_bytes() -> usize {
        1_048_576
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Roles {
    pub architect: String,
    pub reviewer: String,
    // Tier 2f (v0.5.0): default plan strategy for `/plan` when no profile matches.
    #[serde(default)]
    pub plan_strategy: PlanStrategy,
    // Tier 2f (v0.5.0): optional override for the plan-only system prompt.
    #[serde(default)]
    pub plan_system_prompt_override: Option<String>,
    // Tier 3h (v1.0): default fork/branch behaviour when no profile overrides it.
    #[serde(default)]
    pub fork: Option<ForkConfig>,
    // Tier 3i (v1.0): default MCP servers attached to the architect/reviewer
    // pair when no profile overrides it.
    #[serde(default)]
    pub mcp: Option<McpConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Profile {
    pub id: String,
    #[serde(default)]
    pub match_rules: MatchRules,
    pub lead: String,
    pub reviewer: String,
    #[serde(default)]
    pub review_strictness: ReviewStrictness,
    #[serde(default)]
    pub max_refinement_rounds: Option<u8>,
    // Tier 2f (v0.5.0): per-profile plan strategy override (defaults to PlanStrategy::Single).
    #[serde(default)]
    pub plan_strategy: PlanStrategy,
    // Tier 2f (v0.5.0): per-profile override of the plan-only system prompt.
    #[serde(default)]
    pub plan_system_prompt_override: Option<String>,
    // Tier 3h (v1.0): per-profile fork/branch behaviour override.
    #[serde(default)]
    pub fork: Option<ForkConfig>,
    // Tier 3i (v1.0): per-profile MCP server attachment override. When `Some`,
    // replaces the Roles-level default for both lead and reviewer in this
    // profile.
    #[serde(default)]
    pub mcp: Option<McpConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PromptTemplate {
    pub id: String,
    pub prompt: String,
    #[serde(default)]
    pub description: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct UiConfig {
    #[serde(default = "default_ui_mode")]
    pub default_mode: UiMode,
}

impl Default for UiConfig {
    fn default() -> Self {
        Self {
            default_mode: default_ui_mode(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum UiMode {
    Print,
    Interactive,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DaemonConfig {
    #[serde(default = "default_daemon_protocol")]
    pub protocol: DaemonProtocol,
    // Tier 2e (v0.5.0): RPC envelope/backpressure/token knobs. None falls back
    // to RpcConfig::default(); preserved as Option to keep existing serialized
    // daemon blobs round-trippable without injecting an `rpc` key.
    #[serde(default)]
    pub rpc: Option<RpcConfig>,
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            protocol: default_daemon_protocol(),
            rpc: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DaemonProtocol {
    Line,
    Rpc,
}

// Tier 3g (v0.5.0): ratatui 3-pane TUI runtime configuration.
//
// All knobs default to the values described in nerve-terminal-upgrade-proposal.md
// §3 Tier 3g. Honoured by the `nerve-tui` crate; ignored otherwise.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct TuiConfig {
    #[serde(default = "default_tui_enabled")]
    pub enabled: bool,
    #[serde(default = "default_tui_auto_in_cmux")]
    pub auto_in_cmux: bool,
    #[serde(default = "default_tui_refresh_ms")]
    pub refresh_ms: u64,
    #[serde(default = "default_tui_log_height_pct")]
    pub log_height_pct: u8,
}

impl Default for TuiConfig {
    fn default() -> Self {
        Self {
            enabled: default_tui_enabled(),
            auto_in_cmux: default_tui_auto_in_cmux(),
            refresh_ms: default_tui_refresh_ms(),
            log_height_pct: default_tui_log_height_pct(),
        }
    }
}

// Tier 2e (v0.5.0): RPC envelope / backpressure / token lifecycle knobs.
//
// Defaults match nerve-terminal-upgrade-proposal.md §3 Tier 2e sec-4 (per-consumer
// bounded channel 1024, 64 KiB payload cap, 32B bearer token stored 0600).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RpcConfig {
    #[serde(default = "default_rpc_per_consumer_queue")]
    pub per_consumer_queue: usize,
    #[serde(default = "default_rpc_payload_cap_kib")]
    pub payload_cap_kib: usize,
    #[serde(default = "default_rpc_token_path")]
    pub token_path: PathBuf,
    #[serde(default = "default_rpc_token_size_bytes")]
    pub token_size_bytes: usize,
    #[serde(default)]
    pub print_token: bool,
    #[serde(default = "default_rpc_envelope_version")]
    pub envelope_version: String,
}

impl Default for RpcConfig {
    fn default() -> Self {
        Self {
            per_consumer_queue: default_rpc_per_consumer_queue(),
            payload_cap_kib: default_rpc_payload_cap_kib(),
            token_path: default_rpc_token_path(),
            token_size_bytes: default_rpc_token_size_bytes(),
            print_token: false,
            envelope_version: default_rpc_envelope_version(),
        }
    }
}

impl RpcConfig {
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.per_consumer_queue == 0 {
            return Err(ConfigError::InvalidRpcValue("per_consumer_queue"));
        }
        if self.payload_cap_kib == 0 {
            return Err(ConfigError::InvalidRpcValue("payload_cap_kib"));
        }
        if self.token_size_bytes == 0 {
            return Err(ConfigError::InvalidRpcValue("token_size_bytes"));
        }
        Ok(())
    }
}

// Tier 3h (v1.0): session fork/branch behaviour.
//
// `copy_patch_history` controls whether `nv fork` snapshots the parent's
// round history into the child session file; `auto_name` controls whether
// the interactive `/fork` slash command synthesises a child name of the form
// `<parent>-fork-<n>` when the caller omits `--name`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ForkConfig {
    #[serde(default = "default_true")]
    pub copy_patch_history: bool,
    #[serde(default)]
    pub auto_name: bool,
}

impl Default for ForkConfig {
    fn default() -> Self {
        Self {
            copy_patch_history: default_true(),
            auto_name: false,
        }
    }
}

// Tier 3i (v1.0): MCP (Model Context Protocol) attachment.
//
// `servers` lists the MCP server processes the orchestrator will spawn for
// this profile/roles scope. `allow_tools` is a *global* allowlist that
// intersects with each server's per-spec `allowed_tools`. `write_tool_patterns`
// holds the read-only guard blacklist enforced when an `McpServerSpec.read_only`
// is true — substring matches against the tool name refuse dispatch.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct McpConfig {
    #[serde(default)]
    pub servers: Vec<McpServerSpec>,
    #[serde(default)]
    pub allow_tools: Vec<String>,
    #[serde(default = "default_mcp_write_patterns")]
    pub write_tool_patterns: Vec<String>,
}

impl Default for McpConfig {
    fn default() -> Self {
        Self {
            servers: Vec::new(),
            allow_tools: Vec::new(),
            write_tool_patterns: default_mcp_write_patterns(),
        }
    }
}

pub fn default_mcp_write_patterns() -> Vec<String> {
    vec![
        "shell".into(),
        "exec".into(),
        "fs.write".into(),
        "fs.delete".into(),
        "write_file".into(),
        "run_command".into(),
        "execute_command".into(),
    ]
}

// Tier 3j (v1.0): Mayor + Patrol multi-instance dispatcher.
//
// `queue_dir` and `results_dir` are the on-disk source of truth — Mayor and
// patrol agents communicate exclusively through atomic rename(2) into these
// directories so the system survives crash + restart. `max_patrols` bounds
// the worker fleet. `per_patrol_budget_microusd` carves Mayor's budget into a
// per-worker ceiling enforced by the budget-audit gate (sec-3 §3 Tier 2g).
// `heartbeat_secs` and `claim_ttl_secs` govern the doctor's orphaned-claim
// detection: a claim file older than `claim_ttl_secs` without a fresh
// heartbeat is considered abandoned and surfaced for reaping.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct MayorPatrolConfig {
    #[serde(default = "default_queue_dir")]
    pub queue_dir: PathBuf,
    #[serde(default = "default_results_dir")]
    pub results_dir: PathBuf,
    #[serde(default = "default_max_patrols")]
    pub max_patrols: u32,
    #[serde(default)]
    pub per_patrol_budget_microusd: Option<u64>,
    #[serde(default = "default_heartbeat_secs")]
    pub heartbeat_secs: u32,
    #[serde(default = "default_claim_ttl_secs")]
    pub claim_ttl_secs: u32,
    /// S14: enable the coordination ledger + mailbox (observability/coordination
    /// only — NEVER an acceptance or apply signal; the deterministic `blocked`
    /// gate stays the sole authority). When `false`, the Mayor/Patrol record
    /// nothing extra and queue behavior is byte-identical to before S14.
    /// Defaults `true`: the ledger/mailbox live under the queue root (not in
    /// pending/done/…), so even enabled they leave the authoritative queue state
    /// and all existing counts untouched.
    #[serde(default = "default_coordination_enabled")]
    pub coordination_enabled: bool,
}

impl Default for MayorPatrolConfig {
    fn default() -> Self {
        Self {
            queue_dir: default_queue_dir(),
            results_dir: default_results_dir(),
            max_patrols: default_max_patrols(),
            per_patrol_budget_microusd: None,
            heartbeat_secs: default_heartbeat_secs(),
            claim_ttl_secs: default_claim_ttl_secs(),
            coordination_enabled: default_coordination_enabled(),
        }
    }
}

impl MayorPatrolConfig {
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.max_patrols == 0 || self.max_patrols > 64 {
            return Err(ConfigError::InvalidMayorPatrolMaxPatrols(self.max_patrols));
        }
        if self.heartbeat_secs == 0 {
            return Err(ConfigError::InvalidMayorPatrolValue("heartbeat_secs"));
        }
        if self.claim_ttl_secs < self.heartbeat_secs.saturating_mul(2) {
            return Err(ConfigError::InvalidMayorPatrolClaimTtl {
                ttl: self.claim_ttl_secs,
                hb: self.heartbeat_secs,
            });
        }
        if self.per_patrol_budget_microusd == Some(0) {
            return Err(ConfigError::InvalidMayorPatrolValue(
                "per_patrol_budget_microusd",
            ));
        }
        Ok(())
    }
}

impl McpConfig {
    pub fn validate(&self) -> Result<(), ConfigError> {
        let mut names = BTreeSet::new();
        for server in &self.servers {
            if server.name.trim().is_empty() {
                return Err(ConfigError::EmptyMcpName);
            }
            if !names.insert(server.name.clone()) {
                return Err(ConfigError::DuplicateMcpServer(server.name.clone()));
            }
            if server.command.is_empty() {
                return Err(ConfigError::EmptyMcpCommand(server.name.clone()));
            }
            if server.command.iter().any(|arg| arg.trim().is_empty()) {
                return Err(ConfigError::EmptyMcpCommand(server.name.clone()));
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProfileSelection {
    pub id: Option<String>,
    pub lead: String,
    pub reviewer: String,
    pub review_strictness: ReviewStrictness,
    pub max_refinement_rounds: u8,
    #[serde(default)]
    pub plan_strategy: PlanStrategy,
    #[serde(default)]
    pub plan_system_prompt_override: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(untagged)]
pub enum MatchRules {
    Any(Vec<String>),
    Logic {
        #[serde(default)]
        all: Vec<String>,
        #[serde(default)]
        any: Vec<String>,
    },
}

impl Default for MatchRules {
    fn default() -> Self {
        Self::Any(Vec::new())
    }
}

impl MatchRules {
    fn is_empty(&self) -> bool {
        match self {
            Self::Any(rules) => rules.is_empty(),
            Self::Logic { all, any } => all.is_empty() && any.is_empty(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Strategy {
    Consensus,
    Pipeline,
    Tournament,
}

// Tier 2f (v0.5.0): /plan execution strategy.
//
// `Single` runs the lead adapter in plan-only mode (default). `DualReview`
// additionally pipes the lead's plan markdown to the reviewer for a structural
// review pass, without ever permitting patches.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum PlanStrategy {
    #[default]
    Single,
    DualReview,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ConflictPolicy {
    LeadPriority,
    ReviewerPriority,
    MergeAttempt,
    AbortOnConflict,
    ReviewerBlock,
    Manual,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum ReviewStrictness {
    Low,
    #[default]
    Normal,
    High,
}

impl ReviewStrictness {
    /// Whether `AcceptWithNits` may terminate the loop; `High` forces a
    /// refinement round even on cosmetic nits.
    pub fn permits_nits(&self) -> bool {
        matches!(self, Self::Low | Self::Normal)
    }
}

/// S10: how the live "crossfire" reviewer signal (gathered over-the-shoulder
/// while the lead generates, by watching `.nerve/scratch`) feeds back into the
/// loop. Defaults to [`Off`](CrossfireAction::Off): crossfire stays advisory —
/// recorded for the report but never steering the lead nor stopping a round.
///
/// Both non-`Off` actions are REJECTION-DIRECTION ONLY and act strictly at the
/// round seam (the lead's model subprocess is not `kill_on_drop`, and steering
/// is seam-only by design — roadmap directive (e) / anti-pattern #3). They can
/// only push the loop toward more scrutiny / refine / abort, NEVER toward
/// acceptance: the deterministic gate (verdict-accept AND check Pass) stays the
/// sole acceptance authority. A "looks good" crossfire never accelerates
/// anything; only a decisive crossfire `Block` may short-circuit, and it
/// short-circuits toward a BLOCKED run.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum CrossfireAction {
    /// Record-only (today's behavior, byte-identical): crossfire feedback is
    /// collected into the report but never changes loop control flow.
    #[default]
    Off,
    /// The crossfire hints from a generation enrich the NEXT refine round's
    /// prompt (the lead sees the live over-the-shoulder feedback in addition to
    /// the end-of-round review). Pure rejection-direction steering — it only
    /// changes the lead's INPUT; the gate still re-judges the resulting patch.
    Redirect,
    /// `Redirect` plus: a decisive live crossfire `Block` during a generation
    /// short-circuits the refinement loop and BLOCKS the run (단락). The
    /// terminal-accept check runs first every round, so this can only ever fire
    /// on a non-accepting round — it never overrides an acceptance.
    Halt,
}

impl CrossfireAction {
    /// Whether crossfire hints should steer the next refine (true for
    /// `Redirect` and `Halt`).
    pub fn redirects(&self) -> bool {
        matches!(self, Self::Redirect | Self::Halt)
    }

    /// Whether a decisive live crossfire `Block` may short-circuit + block the
    /// run (true only for `Halt`).
    pub fn halts(&self) -> bool {
        matches!(self, Self::Halt)
    }
}

/// S12: how the deterministic auto-mode classifier treats a run's final patch.
///
/// Defaults to [`Off`](ApplyClassifierMode::Off): no classification, and the
/// apply decision is exactly the operator's (`--apply` or an S11 grant) AND the
/// deterministic gate — byte-identical to pre-S12.
///
/// The classifier is REJECTION-DIRECTION ONLY and monotone: it can only ever
/// turn a would-be APPLY into a dry-run (implement) when the patch looks risky —
/// it NEVER turns a dry-run into an apply, and it NEVER touches `blocked` /
/// `goal_satisfied`. So even at its most aggressive it can only make Nerve *more*
/// conservative; a misclassification can over-protect (operator re-applies
/// manually) but can never fabricate an apply the operator did not request.
/// (Roadmap anti-pattern #1: risky auto-execution is a loud opt-in, never a
/// default; LLM opinion is never the gate — this classifier is deterministic.)
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum ApplyClassifierMode {
    /// Disabled (today's behavior, byte-identical): the patch is never inspected
    /// and the apply decision is unchanged.
    #[default]
    Off,
    /// Classify the final patch and record the risk + reasons in the run report
    /// (telemetry only, like the S7 progress signal). The apply gate is UNCHANGED
    /// — a High-risk classification is surfaced but does not veto apply.
    Advisory,
    /// Classify the final patch and, when it is High risk, DOWNGRADE a would-be
    /// apply to a dry-run (implement). Only ever removes apply permission; never
    /// grants it, never affects `blocked`/`goal_satisfied`.
    Enforce,
}

impl ApplyClassifierMode {
    /// Whether a classification should be computed at all (`Advisory` or
    /// `Enforce`). `Off` skips classification entirely.
    pub fn classifies(&self) -> bool {
        matches!(self, Self::Advisory | Self::Enforce)
    }

    /// Whether a High-risk classification may DOWNGRADE a would-be apply to a
    /// dry-run (true only for `Enforce`).
    pub fn enforces(&self) -> bool {
        matches!(self, Self::Enforce)
    }
}

/// S12: configuration for the deterministic auto-mode classifier gate.
///
/// All thresholds describe when a patch is "High risk" enough to warrant a
/// downgrade (in `Enforce`) or a warning (in `Advisory`). The classification is
/// computed from the final patch alone — file count, total changed lines, risky
/// touched paths, and inherently risky operations (delete/rename) — so it is
/// fully deterministic and adds no LLM call to the apply path.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ApplyClassifierConfig {
    #[serde(default)]
    pub mode: ApplyClassifierMode,
    /// A patch touching MORE than this many (non-noop) files is High risk.
    #[serde(default = "default_apply_classifier_max_files")]
    pub max_files: usize,
    /// A patch changing MORE than this many total lines (added + removed across
    /// all files) is High risk.
    #[serde(default = "default_apply_classifier_max_lines")]
    pub max_lines: usize,
    /// Glob patterns (matched against each touched path, relative to the task
    /// cwd) that mark a patch High risk regardless of size — e.g. lockfiles, CI
    /// config, build manifests, dotenv. Matched with `globset`.
    #[serde(default = "default_apply_classifier_risky_globs")]
    pub risky_path_globs: Vec<String>,
    /// When true, a patch containing a file delete or rename is High risk
    /// regardless of size (these are the most destructive operations).
    #[serde(default = "default_apply_classifier_flag_destructive_ops")]
    pub flag_destructive_ops: bool,
}

impl Default for ApplyClassifierConfig {
    fn default() -> Self {
        Self {
            mode: ApplyClassifierMode::default(),
            max_files: default_apply_classifier_max_files(),
            max_lines: default_apply_classifier_max_lines(),
            risky_path_globs: default_apply_classifier_risky_globs(),
            flag_destructive_ops: default_apply_classifier_flag_destructive_ops(),
        }
    }
}

impl ApplyClassifierConfig {
    pub fn validate(&self) -> Result<()> {
        if self.mode.classifies() && self.max_files == 0 && self.max_lines == 0 {
            anyhow::bail!(
                "apply_classifier: max_files and max_lines cannot both be 0 when the classifier is enabled"
            );
        }
        Ok(())
    }
}

fn default_apply_classifier_max_files() -> usize {
    25
}

fn default_apply_classifier_max_lines() -> usize {
    800
}

fn default_apply_classifier_flag_destructive_ops() -> bool {
    true
}

fn default_apply_classifier_risky_globs() -> Vec<String> {
    [
        "**/Cargo.lock",
        "**/package-lock.json",
        "**/pnpm-lock.yaml",
        "**/yarn.lock",
        "**/go.sum",
        "**/.github/**",
        "**/.env*",
        "**/Dockerfile",
        "**/Makefile",
    ]
    .into_iter()
    .map(String::from)
    .collect()
}

impl Config {
    pub fn load() -> Result<Self> {
        Self::load_from(env::current_dir().context("failed to read current directory")?)
    }

    pub fn load_from(cwd: impl AsRef<Path>) -> Result<Self> {
        let cwd_config = cwd.as_ref().join("nerve.config.json");
        if cwd_config.exists() {
            let mut config = Self::from_path(&cwd_config)?;
            // Repo-local: untrusted for enabling an executing built-in verifier.
            config.source = ConfigSource::Project;
            return Ok(config);
        }

        if let Some(home) = env::var_os("HOME") {
            let user_config = PathBuf::from(home).join(".config/nerve/config.json");
            if user_config.exists() {
                let mut config = Self::from_path(&user_config)?;
                config.source = ConfigSource::User;
                return Ok(config);
            }
        }

        let mut config =
            Self::from_json_str(DEFAULT_CONFIG).context("embedded default config is invalid")?;
        config.source = ConfigSource::Default;
        Ok(config)
    }

    /// Provenance of this config — see [`ConfigSource`]. The S4 trust boundary
    /// uses this to keep a project-local `nerve.config.json` from silently
    /// enabling code execution.
    pub fn source(&self) -> ConfigSource {
        self.source
    }

    /// Whether config that enables an *executing* built-in verifier (S4 `Auto`
    /// or `Command`) is trusted to run repo code. Operator-controlled sources
    /// ([`ConfigSource::User`]/[`Default`](ConfigSource::Default)) are trusted;
    /// a project-local config is NOT — a cloned repo could ship it — unless the
    /// operator passes explicit out-of-band consent (`operator_consent`, e.g. an
    /// env var or CLI flag a repo cannot forge).
    pub fn builtin_verifier_exec_trusted(&self, operator_consent: bool) -> bool {
        match self.source {
            ConfigSource::User | ConfigSource::Default => true,
            ConfigSource::Project => operator_consent,
        }
    }

    pub fn from_path(path: &Path) -> Result<Self> {
        let raw = fs::read_to_string(path)
            .with_context(|| format!("failed to read config `{}`", path.display()))?;
        Self::from_json_str(&raw).with_context(|| format!("invalid config `{}`", path.display()))
    }

    pub fn from_json_str(raw: &str) -> Result<Self> {
        let config: Self = serde_json::from_str(raw)?;
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<()> {
        if self.orchestration.max_refinement_rounds > 5 {
            anyhow::bail!("orchestration.max_refinement_rounds must be <= 5");
        }
        if self.orchestration.max_total_tokens == Some(0) {
            anyhow::bail!("orchestration.max_total_tokens must be greater than 0 when set");
        }
        if self.orchestration.max_estimated_cost_microusd == Some(0) {
            anyhow::bail!(
                "orchestration.max_estimated_cost_microusd must be greater than 0 when set"
            );
        }
        if self.orchestration.budget_cost_microusd_ceiling == Some(0) {
            anyhow::bail!(
                "orchestration.budget_cost_microusd_ceiling must be greater than 0 when set"
            );
        }
        if self.orchestration.budget_tokens_ceiling == Some(0) {
            anyhow::bail!("orchestration.budget_tokens_ceiling must be greater than 0 when set");
        }
        if self.orchestration.adapter_timeout_secs == Some(0) {
            anyhow::bail!("orchestration.adapter_timeout_secs must be greater than 0 when set");
        }
        if self.orchestration.adapter_max_output_bytes == Some(0) {
            anyhow::bail!("orchestration.adapter_max_output_bytes must be greater than 0 when set");
        }
        if let Some(ulimit) = &self.orchestration.check_ulimit {
            ulimit
                .validate()
                .map_err(|e| anyhow::anyhow!("orchestration.check_ulimit invalid: {e}"))?;
        }
        if self.orchestration.worktree_apply {
            eprintln!(
                "[nerve-config] orchestration.worktree_apply is enabled; run `nv doctor` to verify git worktree readiness."
            );
        }
        if let Some(rpc) = &self.daemon.rpc {
            rpc.validate()
                .map_err(|e| anyhow::anyhow!("daemon.rpc invalid: {e}"))?;
        }
        if self.tui.refresh_ms == 0 {
            anyhow::bail!("tui.refresh_ms must be greater than 0");
        }
        if self.tui.log_height_pct == 0 || self.tui.log_height_pct > 100 {
            anyhow::bail!("tui.log_height_pct must be in (0, 100]");
        }
        if let Some(mp) = &self.orchestration.mayor_patrol {
            mp.validate()
                .map_err(|e| anyhow::anyhow!("orchestration.mayor_patrol invalid: {e}"))?;
        }
        self.orchestration
            .builtin_verifier
            .validate()
            .map_err(|e| anyhow::anyhow!("orchestration.builtin_verifier invalid: {e}"))?;
        self.orchestration
            .apply_classifier
            .validate()
            .map_err(|e| anyhow::anyhow!("orchestration.apply_classifier invalid: {e}"))?;
        if self.roles.architect.trim().is_empty() {
            anyhow::bail!("roles.architect must not be empty");
        }
        if self.roles.reviewer.trim().is_empty() {
            anyhow::bail!("roles.reviewer must not be empty");
        }
        if let Some(mcp) = &self.roles.mcp {
            mcp.validate()
                .map_err(|e| anyhow::anyhow!("roles.mcp invalid: {e}"))?;
        }
        for profile in &self.profiles {
            if profile.id.trim().is_empty() {
                anyhow::bail!("profile id must not be empty");
            }
            if profile.lead.trim().is_empty() {
                anyhow::bail!("profile `{}` lead must not be empty", profile.id);
            }
            if profile.reviewer.trim().is_empty() {
                anyhow::bail!("profile `{}` reviewer must not be empty", profile.id);
            }
            if profile
                .max_refinement_rounds
                .is_some_and(|rounds| rounds > 5)
            {
                anyhow::bail!(
                    "profile `{}` max_refinement_rounds must be <= 5",
                    profile.id
                );
            }
            if let Some(mcp) = &profile.mcp {
                mcp.validate()
                    .map_err(|e| anyhow::anyhow!("profile `{}` mcp invalid: {}", profile.id, e))?;
            }
        }
        for template in &self.templates {
            if template.id.trim().is_empty() {
                anyhow::bail!("template id must not be empty");
            }
            if template.prompt.trim().is_empty() {
                anyhow::bail!("template `{}` prompt must not be empty", template.id);
            }
        }
        Ok(())
    }

    pub fn select_profile(&self, task: &Task) -> Result<ProfileSelection> {
        for profile in &self.profiles {
            if profile.matches(task)? {
                return Ok(ProfileSelection {
                    id: Some(profile.id.clone()),
                    lead: profile.lead.clone(),
                    reviewer: profile.reviewer.clone(),
                    review_strictness: profile.review_strictness.clone(),
                    max_refinement_rounds: profile
                        .max_refinement_rounds
                        .unwrap_or(self.orchestration.max_refinement_rounds),
                    plan_strategy: profile.plan_strategy.clone(),
                    plan_system_prompt_override: profile
                        .plan_system_prompt_override
                        .clone()
                        .or_else(|| self.roles.plan_system_prompt_override.clone()),
                });
            }
        }

        Ok(ProfileSelection {
            id: None,
            lead: self.roles.architect.clone(),
            reviewer: self.roles.reviewer.clone(),
            review_strictness: ReviewStrictness::Normal,
            max_refinement_rounds: self.orchestration.max_refinement_rounds,
            plan_strategy: self.roles.plan_strategy.clone(),
            plan_system_prompt_override: self.roles.plan_system_prompt_override.clone(),
        })
    }
}

impl Profile {
    pub fn matches(&self, task: &Task) -> Result<bool> {
        if self.match_rules.is_empty() {
            return Ok(false);
        }

        match &self.match_rules {
            MatchRules::Any(rules) => any_rule_matches(rules, task),
            MatchRules::Logic { all, any } => {
                let all_match = all_rules_match(all, task)?;
                let any_match = if any.is_empty() && !all.is_empty() {
                    true
                } else {
                    any_rule_matches(any, task)?
                };
                Ok(all_match && any_match)
            }
        }
    }
}

fn any_rule_matches(rules: &[String], task: &Task) -> Result<bool> {
    for rule in rules {
        if rule_matches(rule, task)? {
            return Ok(true);
        }
    }
    Ok(false)
}

fn all_rules_match(rules: &[String], task: &Task) -> Result<bool> {
    for rule in rules {
        if !rule_matches(rule, task)? {
            return Ok(false);
        }
    }
    Ok(true)
}

fn rule_matches(rule: &str, task: &Task) -> Result<bool> {
    let prompt = task.prompt.to_lowercase();
    if !looks_like_glob(rule) {
        return Ok(prompt.contains(&rule.to_lowercase()));
    }

    let mut glob_builder = GlobSetBuilder::new();
    glob_builder.add(Glob::new(rule).with_context(|| format!("invalid glob rule `{rule}`"))?);
    let glob_set = glob_builder.build()?;

    Ok(task
        .context_paths
        .iter()
        .any(|path| glob_set.is_match(path)))
}

fn looks_like_glob(rule: &str) -> bool {
    rule.contains('*') || rule.contains('?') || rule.contains('[')
}

fn default_strategy() -> Strategy {
    Strategy::Consensus
}

fn default_max_refinement_rounds() -> u8 {
    2
}

fn default_conflict_policy() -> ConflictPolicy {
    ConflictPolicy::LeadPriority
}

fn default_ui_mode() -> UiMode {
    UiMode::Print
}

fn default_daemon_protocol() -> DaemonProtocol {
    DaemonProtocol::Line
}

fn default_tui_enabled() -> bool {
    true
}

fn default_tui_auto_in_cmux() -> bool {
    true
}

fn default_tui_refresh_ms() -> u64 {
    100
}

fn default_tui_log_height_pct() -> u8 {
    60
}

fn default_rpc_per_consumer_queue() -> usize {
    1024
}

fn default_rpc_payload_cap_kib() -> usize {
    64
}

fn default_rpc_token_path() -> PathBuf {
    PathBuf::from(".nerve/session-meta/rpc-token")
}

fn default_rpc_token_size_bytes() -> usize {
    32
}

fn default_rpc_envelope_version() -> String {
    "1.1.0".to_string()
}

fn default_true() -> bool {
    true
}

fn default_queue_dir() -> PathBuf {
    PathBuf::from(".nerve/queue")
}

fn default_results_dir() -> PathBuf {
    PathBuf::from(".nerve/results")
}

fn default_max_patrols() -> u32 {
    8
}

fn default_heartbeat_secs() -> u32 {
    30
}

fn default_claim_ttl_secs() -> u32 {
    600
}

fn default_coordination_enabled() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loads_default_config() {
        let config = Config::from_json_str(DEFAULT_CONFIG).unwrap();
        assert_eq!(config.roles.architect, "claude-code");
        assert_eq!(config.profiles.len(), 2);
        assert_eq!(config.ui.default_mode, UiMode::Print);
        assert_eq!(config.daemon.protocol, DaemonProtocol::Line);
        assert!(!config.templates.is_empty());
    }

    #[test]
    fn matches_keyword_profile() {
        let config = Config::from_json_str(DEFAULT_CONFIG).unwrap();
        let task = Task::new("please fix ui spacing", ".");

        let selected = config.select_profile(&task).unwrap();

        assert_eq!(selected.id.as_deref(), Some("rapid_fix"));
        assert_eq!(selected.lead, "codex");
    }

    #[test]
    fn matches_glob_profile() {
        let config = Config::from_json_str(DEFAULT_CONFIG).unwrap();
        let mut task = Task::new("audit code", ".");
        task.context_paths.push(PathBuf::from("src/lib.rs"));

        let selected = config.select_profile(&task).unwrap();

        assert_eq!(selected.id.as_deref(), Some("blockchain_dev"));
    }

    #[test]
    fn matches_all_any_profile_rules() {
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
              "profiles": [
                {
                  "id": "contract_audit",
                  "match_rules": {
                    "all": ["*.rs", "contract"],
                    "any": ["audit", "security"]
                  },
                  "lead": "claude-code",
                  "reviewer": "codex"
                }
              ]
            }"#,
        )
        .unwrap();
        let mut task = Task::new("audit payment contract", ".");
        task.context_paths.push(PathBuf::from("src/lib.rs"));

        let selected = config.select_profile(&task).unwrap();

        assert_eq!(selected.id.as_deref(), Some("contract_audit"));
    }

    #[test]
    fn goal_spec_validate_rejects_empty_cmd() {
        let spec = GoalSpec {
            id: "g1".into(),
            check_cmd: Vec::new(),
            timeout_secs: 60,
            cwd: None,
            env: Default::default(),
            no_progress_max: None,
        };
        assert_eq!(spec.validate(), Err(ConfigError::EmptyCheckCmd));
    }

    #[test]
    fn goal_spec_validate_rejects_relative_path_with_dotdot() {
        let spec = GoalSpec {
            id: "g1".into(),
            check_cmd: vec!["../evil".into()],
            timeout_secs: 60,
            cwd: None,
            env: Default::default(),
            no_progress_max: None,
        };
        assert!(matches!(
            spec.validate(),
            Err(ConfigError::InvalidCheckCmdProgram(_))
        ));

        let abs = GoalSpec {
            id: "g2".into(),
            check_cmd: vec!["/bin/sh".into()],
            timeout_secs: 60,
            cwd: None,
            env: Default::default(),
            no_progress_max: None,
        };
        assert!(matches!(
            abs.validate(),
            Err(ConfigError::InvalidCheckCmdProgram(_))
        ));
    }

    #[test]
    fn goal_spec_validate_rejects_leading_dash_program() {
        // A leading `-` could be mis-parsed by a sandbox wrapper as one of its
        // own options, running a different command than the unwrapped gate would
        // (S5 sandbox-transparency). `["--", "true"]` must not validate.
        for argv in [vec!["--".to_string()], vec!["-c".to_string(), "x".into()]] {
            let spec = GoalSpec {
                id: "g".into(),
                check_cmd: argv.clone(),
                timeout_secs: 60,
                cwd: None,
                env: Default::default(),
                no_progress_max: None,
            };
            assert!(
                matches!(spec.validate(), Err(ConfigError::InvalidCheckCmdProgram(_))),
                "argv {argv:?} should be rejected"
            );
        }
    }

    #[test]
    fn goal_spec_serde_round_trip() {
        let mut env = std::collections::BTreeMap::new();
        env.insert("PATH".to_string(), String::new());
        env.insert("NERVE_RUN".to_string(), "1".to_string());
        let spec = GoalSpec {
            id: "g1".into(),
            check_cmd: vec!["cargo".into(), "test".into()],
            timeout_secs: 120,
            cwd: Some(PathBuf::from("/tmp/work")),
            env,
            no_progress_max: Some(3),
        };
        spec.validate().unwrap();
        let json = serde_json::to_string(&spec).unwrap();
        let back: GoalSpec = serde_json::from_str(&json).unwrap();
        assert_eq!(spec, back);
    }

    #[test]
    fn orchestration_check_env_default_empty() {
        let config = Config::from_json_str(DEFAULT_CONFIG).unwrap();
        assert!(config.orchestration.check_env.is_empty());
        assert_eq!(
            config.orchestration.check_output_cap_bytes,
            Orchestration::default_check_output_cap_bytes()
        );
        assert!(config.orchestration.adapter_timeout_secs.is_none());
        assert!(config.orchestration.adapter_max_output_bytes.is_none());
        assert!(config.orchestration.budget_cost_microusd_ceiling.is_none());
        assert!(config.orchestration.budget_tokens_ceiling.is_none());
    }

    #[test]
    fn loads_templates_from_config() {
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
              "templates": [
                {
                  "id": "security-audit",
                  "description": "Audit a target",
                  "prompt": "audit {{args}}"
                }
              ]
            }"#,
        )
        .unwrap();

        assert_eq!(config.templates[0].id, "security-audit");
        assert_eq!(
            config.templates[0].description.as_deref(),
            Some("Audit a target")
        );
    }

    #[test]
    fn goal_intent_validate_round_trip() {
        use chrono::Utc;
        let intent = GoalIntent {
            free_form: "run cargo tests until they pass".into(),
            proposed_spec: GoalSpec {
                id: "intent-1".into(),
                check_cmd: vec!["cargo".into(), "test".into()],
                timeout_secs: 60,
                cwd: None,
                env: Default::default(),
                no_progress_max: None,
            },
            rationale: "user asked for cargo test gate".into(),
            source_adapter: "claude-code".into(),
            created_at: Utc::now(),
        };
        intent.validate().unwrap();

        let json = serde_json::to_string(&intent).unwrap();
        let back: GoalIntent = serde_json::from_str(&json).unwrap();
        assert_eq!(intent, back);
        back.validate().unwrap();
    }

    #[test]
    fn check_ulimit_rejects_zero() {
        let zero_nproc = CheckUlimit {
            nproc: Some(0),
            ..Default::default()
        };
        assert_eq!(
            zero_nproc.validate(),
            Err(ConfigError::InvalidUlimitValue("nproc"))
        );

        let zero_mem = CheckUlimit {
            memory_bytes: Some(0),
            ..Default::default()
        };
        assert_eq!(
            zero_mem.validate(),
            Err(ConfigError::InvalidUlimitValue("memory_bytes"))
        );

        let zero_fsize = CheckUlimit {
            file_size_bytes: Some(0),
            ..Default::default()
        };
        assert_eq!(
            zero_fsize.validate(),
            Err(ConfigError::InvalidUlimitValue("file_size_bytes"))
        );

        let zero_cpu = CheckUlimit {
            cpu_secs: Some(0),
            ..Default::default()
        };
        assert_eq!(
            zero_cpu.validate(),
            Err(ConfigError::InvalidUlimitValue("cpu_secs"))
        );

        let ok = CheckUlimit {
            nproc: Some(64),
            memory_bytes: Some(2_147_483_648),
            file_size_bytes: Some(104_857_600),
            cpu_secs: Some(300),
        };
        ok.validate().unwrap();
    }

    #[test]
    fn orchestration_worktree_default_false() {
        let config = Config::from_json_str(DEFAULT_CONFIG).unwrap();
        assert!(!config.orchestration.worktree_apply);
        assert!(config.orchestration.check_ulimit.is_none());
    }

    #[test]
    fn plan_strategy_default_single() {
        // Defaulted enum.
        assert_eq!(PlanStrategy::default(), PlanStrategy::Single);

        // Embedded default config has no `plan_strategy` keys; field must default
        // to Single on both Roles and every Profile.
        let config = Config::from_json_str(DEFAULT_CONFIG).unwrap();
        assert_eq!(config.roles.plan_strategy, PlanStrategy::Single);
        assert!(config.roles.plan_system_prompt_override.is_none());
        for profile in &config.profiles {
            assert_eq!(
                profile.plan_strategy,
                PlanStrategy::Single,
                "profile `{}` should default to PlanStrategy::Single",
                profile.id
            );
            assert!(profile.plan_system_prompt_override.is_none());
        }

        // Snake-case serde round-trip for DualReview.
        let json = serde_json::to_string(&PlanStrategy::DualReview).unwrap();
        assert_eq!(json, "\"dual_review\"");
        let back: PlanStrategy = serde_json::from_str(&json).unwrap();
        assert_eq!(back, PlanStrategy::DualReview);
    }

    #[test]
    fn select_profile_carries_plan_settings() {
        let config = Config::from_json_str(
            r#"{
              "orchestration": {
                "default_strategy": "consensus",
                "max_refinement_rounds": 2,
                "conflict_policy": "lead_priority"
              },
              "roles": {
                "architect": "default-lead",
                "reviewer": "default-reviewer",
                "plan_strategy": "dual_review",
                "plan_system_prompt_override": "role prompt"
              },
              "profiles": [
                {
                  "id": "docs",
                  "match_rules": ["docs"],
                  "lead": "profile-lead",
                  "reviewer": "profile-reviewer",
                  "plan_strategy": "single",
                  "plan_system_prompt_override": "profile prompt"
                }
              ]
            }"#,
        )
        .unwrap();

        let selected = config
            .select_profile(&Task::new("update docs", "."))
            .expect("profile selection");
        assert_eq!(selected.id.as_deref(), Some("docs"));
        assert_eq!(selected.plan_strategy, PlanStrategy::Single);
        assert_eq!(
            selected.plan_system_prompt_override.as_deref(),
            Some("profile prompt")
        );

        let fallback = config
            .select_profile(&Task::new("update unrelated code", "."))
            .expect("default selection");
        assert_eq!(fallback.id, None);
        assert_eq!(fallback.plan_strategy, PlanStrategy::DualReview);
        assert_eq!(
            fallback.plan_system_prompt_override.as_deref(),
            Some("role prompt")
        );
    }

    #[test]
    fn tui_config_default_values() {
        let tui = TuiConfig::default();
        assert!(tui.enabled);
        assert!(tui.auto_in_cmux);
        assert_eq!(tui.refresh_ms, 100);
        assert_eq!(tui.log_height_pct, 60);

        // Embedded default config omits the `tui` key, so the Config-level
        // `#[serde(default)]` must materialise the same defaults.
        let config = Config::from_json_str(DEFAULT_CONFIG).unwrap();
        assert_eq!(config.tui, TuiConfig::default());
    }

    #[test]
    fn rpc_config_default_values() {
        let rpc = RpcConfig::default();
        assert_eq!(rpc.per_consumer_queue, 1024);
        assert_eq!(rpc.payload_cap_kib, 64);
        assert_eq!(
            rpc.token_path,
            PathBuf::from(".nerve/session-meta/rpc-token")
        );
        assert_eq!(rpc.token_size_bytes, 32);
        assert!(!rpc.print_token);
        assert_eq!(rpc.envelope_version, "1.1.0");

        // DaemonConfig leaves `rpc` as None to preserve legacy serialized blobs.
        let daemon = DaemonConfig::default();
        assert!(daemon.rpc.is_none());

        // Default Config keeps daemon.rpc unset.
        let config = Config::from_json_str(DEFAULT_CONFIG).unwrap();
        assert!(config.daemon.rpc.is_none());

        // Explicit empty RPC block round-trips into defaults.
        let with_rpc = Config::from_json_str(
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
              "daemon": {
                "protocol": "rpc",
                "rpc": {}
              }
            }"#,
        )
        .unwrap();
        let materialised = with_rpc.daemon.rpc.expect("rpc block was provided");
        assert_eq!(materialised, RpcConfig::default());
    }

    #[test]
    fn rpc_config_validates_nonzero_payload_cap() {
        let zero_payload = RpcConfig {
            payload_cap_kib: 0,
            ..RpcConfig::default()
        };
        assert_eq!(
            zero_payload.validate(),
            Err(ConfigError::InvalidRpcValue("payload_cap_kib"))
        );

        let zero_queue = RpcConfig {
            per_consumer_queue: 0,
            ..RpcConfig::default()
        };
        assert_eq!(
            zero_queue.validate(),
            Err(ConfigError::InvalidRpcValue("per_consumer_queue"))
        );

        let zero_token = RpcConfig {
            token_size_bytes: 0,
            ..RpcConfig::default()
        };
        assert_eq!(
            zero_token.validate(),
            Err(ConfigError::InvalidRpcValue("token_size_bytes"))
        );

        RpcConfig::default().validate().unwrap();

        // Surfaces through Config::validate as well.
        let bad_config_err = Config::from_json_str(
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
              "daemon": {
                "protocol": "rpc",
                "rpc": { "payload_cap_kib": 0 }
              }
            }"#,
        )
        .unwrap_err();
        let msg = format!("{bad_config_err:#}");
        assert!(
            msg.contains("payload_cap_kib"),
            "expected error to mention payload_cap_kib, got: {msg}"
        );
    }

    // -----------------------------------------------------------------------
    // v1.0 — Tier 3h / 3i / 3j unit tests
    // -----------------------------------------------------------------------

    #[test]
    fn fork_config_default_values() {
        let fork = ForkConfig::default();
        assert!(fork.copy_patch_history);
        assert!(!fork.auto_name);

        // Round-trip an empty `{}` block — both defaults must materialise.
        let parsed: ForkConfig = serde_json::from_str("{}").unwrap();
        assert_eq!(parsed, ForkConfig::default());

        // Legacy embedded default config has no `fork` keys; Roles + Profile
        // entries default to None to preserve byte-identical serialization.
        let config = Config::from_json_str(DEFAULT_CONFIG).unwrap();
        assert!(config.roles.fork.is_none());
        for profile in &config.profiles {
            assert!(
                profile.fork.is_none(),
                "profile `{}` fork must default to None",
                profile.id
            );
        }
    }

    #[test]
    fn mcp_config_default_write_patterns() {
        let mcp = McpConfig::default();
        assert!(mcp.servers.is_empty());
        assert!(mcp.allow_tools.is_empty());
        // Defaults documented in proposal §3 Tier 3i sec-3h.
        let expected: Vec<String> = [
            "shell",
            "exec",
            "fs.write",
            "fs.delete",
            "write_file",
            "run_command",
            "execute_command",
        ]
        .into_iter()
        .map(String::from)
        .collect();
        assert_eq!(mcp.write_tool_patterns, expected);

        // Empty `{}` block recreates the same defaults.
        let parsed: McpConfig = serde_json::from_str("{}").unwrap();
        assert_eq!(parsed, McpConfig::default());

        // Legacy embedded config keeps mcp = None on both Roles and Profile.
        let config = Config::from_json_str(DEFAULT_CONFIG).unwrap();
        assert!(config.roles.mcp.is_none());
        for profile in &config.profiles {
            assert!(
                profile.mcp.is_none(),
                "profile `{}` mcp must default to None",
                profile.id
            );
        }
    }

    #[test]
    fn mayor_patrol_default_values() {
        let mp = MayorPatrolConfig::default();
        assert_eq!(mp.queue_dir, PathBuf::from(".nerve/queue"));
        assert_eq!(mp.results_dir, PathBuf::from(".nerve/results"));
        assert_eq!(mp.max_patrols, 8);
        assert!(mp.per_patrol_budget_microusd.is_none());
        assert_eq!(mp.heartbeat_secs, 30);
        assert_eq!(mp.claim_ttl_secs, 600);
        // S14: coordination ledger/mailbox default on (observability only).
        assert!(mp.coordination_enabled);

        // Embedded default config keeps mayor_patrol unset to preserve legacy
        // byte-identical orchestration serialization.
        let config = Config::from_json_str(DEFAULT_CONFIG).unwrap();
        assert!(config.orchestration.mayor_patrol.is_none());

        // Empty `{}` block deserialises to all defaults.
        let parsed: MayorPatrolConfig = serde_json::from_str("{}").unwrap();
        assert_eq!(parsed, MayorPatrolConfig::default());

        // S14: an explicit `coordination_enabled: false` is honored (operator
        // opt-out → byte-identical legacy queue behavior).
        let off: MayorPatrolConfig =
            serde_json::from_str(r#"{"coordination_enabled": false}"#).unwrap();
        assert!(!off.coordination_enabled);
    }

    #[test]
    fn mcp_config_rejects_empty_command() {
        let mcp = McpConfig {
            servers: vec![nerve_types::McpServerSpec {
                name: "broken".into(),
                command: Vec::new(),
                env: Default::default(),
                transport: nerve_types::McpTransport::Stdio,
                allowed_tools: Vec::new(),
                role: nerve_types::McpRole::ReviewerOnly,
                read_only: true,
            }],
            allow_tools: Vec::new(),
            write_tool_patterns: default_mcp_write_patterns(),
        };
        assert!(matches!(
            mcp.validate(),
            Err(ConfigError::EmptyMcpCommand(name)) if name == "broken"
        ));

        // Surfaces through Config::validate when attached to roles.
        let mut config = Config::from_json_str(DEFAULT_CONFIG).unwrap();
        config.roles.mcp = Some(mcp.clone());
        let err = config.validate().unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("roles.mcp invalid"), "got: {msg}");
        assert!(msg.contains("broken"), "got: {msg}");
    }

    #[test]
    fn mcp_config_rejects_empty_and_duplicate_names() {
        let server = |name: &str| nerve_types::McpServerSpec {
            name: name.to_string(),
            command: vec!["mcp-server".to_string()],
            env: Default::default(),
            transport: nerve_types::McpTransport::Stdio,
            allowed_tools: Vec::new(),
            role: nerve_types::McpRole::ReviewerOnly,
            read_only: true,
        };

        let empty = McpConfig {
            servers: vec![server(" ")],
            allow_tools: Vec::new(),
            write_tool_patterns: default_mcp_write_patterns(),
        };
        assert!(matches!(empty.validate(), Err(ConfigError::EmptyMcpName)));

        let duplicate = McpConfig {
            servers: vec![server("docs"), server("docs")],
            allow_tools: Vec::new(),
            write_tool_patterns: default_mcp_write_patterns(),
        };
        assert!(matches!(
            duplicate.validate(),
            Err(ConfigError::DuplicateMcpServer(name)) if name == "docs"
        ));
    }

    #[test]
    fn mcp_config_rejects_empty_command_arg() {
        let mcp = McpConfig {
            servers: vec![nerve_types::McpServerSpec {
                name: "broken".into(),
                command: vec![" ".to_string()],
                env: Default::default(),
                transport: nerve_types::McpTransport::Stdio,
                allowed_tools: Vec::new(),
                role: nerve_types::McpRole::ReviewerOnly,
                read_only: true,
            }],
            allow_tools: Vec::new(),
            write_tool_patterns: default_mcp_write_patterns(),
        };

        assert!(matches!(
            mcp.validate(),
            Err(ConfigError::EmptyMcpCommand(name)) if name == "broken"
        ));
    }

    #[test]
    fn mayor_patrol_validation_rejects_bad_values() {
        // max_patrols == 0 rejected.
        let mp = MayorPatrolConfig {
            max_patrols: 0,
            ..Default::default()
        };
        assert!(matches!(
            mp.validate(),
            Err(ConfigError::InvalidMayorPatrolMaxPatrols(0))
        ));

        // max_patrols > 64 rejected.
        let mp = MayorPatrolConfig {
            max_patrols: 65,
            ..Default::default()
        };
        assert!(matches!(
            mp.validate(),
            Err(ConfigError::InvalidMayorPatrolMaxPatrols(65))
        ));

        // heartbeat_secs == 0 rejected.
        let mp = MayorPatrolConfig {
            heartbeat_secs: 0,
            claim_ttl_secs: 600,
            ..Default::default()
        };
        assert!(matches!(
            mp.validate(),
            Err(ConfigError::InvalidMayorPatrolValue("heartbeat_secs"))
        ));

        // claim_ttl_secs < 2x heartbeat_secs rejected.
        let mp = MayorPatrolConfig {
            heartbeat_secs: 30,
            claim_ttl_secs: 30,
            ..Default::default()
        };
        assert!(matches!(
            mp.validate(),
            Err(ConfigError::InvalidMayorPatrolClaimTtl { ttl: 30, hb: 30 })
        ));

        // per_patrol_budget_microusd == 0 rejected.
        let mp = MayorPatrolConfig {
            per_patrol_budget_microusd: Some(0),
            ..Default::default()
        };
        assert!(matches!(
            mp.validate(),
            Err(ConfigError::InvalidMayorPatrolValue(
                "per_patrol_budget_microusd"
            ))
        ));

        // Healthy config validates.
        let mp = MayorPatrolConfig {
            heartbeat_secs: 30,
            claim_ttl_secs: 60,
            per_patrol_budget_microusd: Some(100_000),
            ..Default::default()
        };
        mp.validate().unwrap();
    }

    #[test]
    fn mayor_patrol_validation_surfaces_through_config() {
        let bad = Config::from_json_str(
            r#"{
              "orchestration": {
                "default_strategy": "consensus",
                "max_refinement_rounds": 2,
                "conflict_policy": "lead_priority",
                "mayor_patrol": { "max_patrols": 0 }
              },
              "roles": {
                "architect": "claude-code",
                "reviewer": "codex"
              }
            }"#,
        )
        .unwrap_err();
        let msg = format!("{bad:#}");
        assert!(
            msg.contains("mayor_patrol"),
            "expected error to mention mayor_patrol, got: {msg}"
        );
        assert!(
            msg.contains("max_patrols"),
            "expected error to mention max_patrols, got: {msg}"
        );
    }

    #[test]
    fn legacy_config_without_v1_sections_round_trips() {
        // Legacy minimum config (no fork/mcp/mayor_patrol blocks anywhere)
        // must deserialise + re-serialise without injecting the new keys.
        let raw = r#"{
          "orchestration": {
            "default_strategy": "consensus",
            "max_refinement_rounds": 2,
            "conflict_policy": "lead_priority"
          },
          "roles": {
            "architect": "claude-code",
            "reviewer": "codex"
          }
        }"#;
        let config = Config::from_json_str(raw).unwrap();
        assert!(config.orchestration.mayor_patrol.is_none());
        assert!(config.roles.fork.is_none());
        assert!(config.roles.mcp.is_none());
        for profile in &config.profiles {
            assert!(profile.fork.is_none());
            assert!(profile.mcp.is_none());
        }

        // Re-serialised JSON must NOT contain v1.0 keys (Option<T> + None
        // would still emit `"key": null` without skip_serializing_if; the
        // contract is: legacy configs round-trip without these keys appearing
        // as non-null entries that change semantics).
        let json = serde_json::to_string(&config).unwrap();
        // Round-trip back to assert structural equality regardless of the
        // emitted shape — guarantees no spurious validation rejection.
        let back = Config::from_json_str(&json).unwrap();
        assert_eq!(back.orchestration.mayor_patrol, None);
        assert_eq!(back.roles.fork, None);
        assert_eq!(back.roles.mcp, None);
    }

    #[test]
    fn config_with_v1_sections_round_trips() {
        let raw = r#"{
          "orchestration": {
            "default_strategy": "consensus",
            "max_refinement_rounds": 2,
            "conflict_policy": "lead_priority",
            "mayor_patrol": {
              "max_patrols": 4,
              "heartbeat_secs": 10,
              "claim_ttl_secs": 120,
              "per_patrol_budget_microusd": 250000
            }
          },
          "roles": {
            "architect": "claude-code",
            "reviewer": "codex",
            "fork": { "copy_patch_history": false, "auto_name": true },
            "mcp": {
              "servers": [
                {
                  "name": "lsp-rust",
                  "command": ["rust-analyzer"],
                  "allowed_tools": ["hover", "definition"]
                }
              ],
              "allow_tools": ["hover"]
            }
          },
          "profiles": [
            {
              "id": "audit",
              "match_rules": ["audit"],
              "lead": "claude-code",
              "reviewer": "codex",
              "fork": { "auto_name": true },
              "mcp": {
                "servers": [
                  {
                    "name": "ripgrep",
                    "command": ["rg", "--json"],
                    "role": "lead_only",
                    "read_only": true
                  }
                ]
              }
            }
          ]
        }"#;
        let config = Config::from_json_str(raw).unwrap();

        let mp = config
            .orchestration
            .mayor_patrol
            .as_ref()
            .expect("mayor_patrol present");
        assert_eq!(mp.max_patrols, 4);
        assert_eq!(mp.heartbeat_secs, 10);
        assert_eq!(mp.claim_ttl_secs, 120);
        assert_eq!(mp.per_patrol_budget_microusd, Some(250_000));
        // Unspecified knobs fall back to defaults.
        assert_eq!(mp.queue_dir, PathBuf::from(".nerve/queue"));

        let roles_fork = config.roles.fork.as_ref().expect("roles.fork present");
        assert!(!roles_fork.copy_patch_history);
        assert!(roles_fork.auto_name);

        let roles_mcp = config.roles.mcp.as_ref().expect("roles.mcp present");
        assert_eq!(roles_mcp.servers.len(), 1);
        assert_eq!(roles_mcp.servers[0].name, "lsp-rust");
        assert_eq!(
            roles_mcp.servers[0].command,
            vec!["rust-analyzer".to_string()]
        );
        // Defaulted fields on the spec.
        assert!(roles_mcp.servers[0].read_only);
        assert_eq!(
            roles_mcp.servers[0].role,
            nerve_types::McpRole::ReviewerOnly
        );
        assert_eq!(roles_mcp.allow_tools, vec!["hover".to_string()]);
        // Default write_tool_patterns kicks in since the user did not override.
        assert_eq!(roles_mcp.write_tool_patterns, default_mcp_write_patterns());

        let profile = &config.profiles[0];
        let profile_fork = profile.fork.as_ref().expect("profile.fork present");
        assert!(profile_fork.copy_patch_history); // default true preserved
        assert!(profile_fork.auto_name);
        let profile_mcp = profile.mcp.as_ref().expect("profile.mcp present");
        assert_eq!(profile_mcp.servers[0].role, nerve_types::McpRole::LeadOnly);

        // Full round-trip through serde keeps the structure intact.
        let json = serde_json::to_string(&config).unwrap();
        let back = Config::from_json_str(&json).unwrap();
        assert_eq!(back, config);
    }

    #[test]
    fn mcp_server_spec_defaults_round_trip() {
        // Minimal MCP server spec — verify defaults bubble through.
        let raw = r#"{
          "name": "tiny",
          "command": ["echo", "hi"]
        }"#;
        let spec: nerve_types::McpServerSpec = serde_json::from_str(raw).unwrap();
        assert_eq!(spec.name, "tiny");
        assert_eq!(spec.command, vec!["echo".to_string(), "hi".to_string()]);
        assert!(spec.env.is_empty());
        assert_eq!(spec.transport, nerve_types::McpTransport::Stdio);
        assert!(spec.allowed_tools.is_empty());
        assert_eq!(spec.role, nerve_types::McpRole::ReviewerOnly);
        assert!(spec.read_only);

        // round-trip preserves equality.
        let json = serde_json::to_string(&spec).unwrap();
        let back: nerve_types::McpServerSpec = serde_json::from_str(&json).unwrap();
        assert_eq!(back, spec);
    }

    #[test]
    fn review_strictness_permits_nits() {
        assert!(ReviewStrictness::Low.permits_nits());
        assert!(ReviewStrictness::Normal.permits_nits());
        assert!(!ReviewStrictness::High.permits_nits());
    }

    #[test]
    fn builtin_verifier_defaults_to_off_with_generous_timeout() {
        // Safe by default: Nerve must not execute project-controlled code
        // (cargo/npm test) without an explicit operator opt-in (S5 OS sandbox
        // is S4's not-yet-built safety dependency).
        let cfg = BuiltinVerifierConfig::default();
        assert_eq!(cfg.mode, BuiltinVerifierMode::Off);
        assert!(cfg.command.is_empty());
        assert_eq!(cfg.timeout_secs, 600);
        cfg.validate().unwrap();
    }

    #[test]
    fn default_config_leaves_builtin_verifier_off() {
        // The shipped default config opts nobody into repo-code execution.
        let config = Config::from_json_str(DEFAULT_CONFIG).unwrap();
        assert_eq!(
            config.orchestration.builtin_verifier.mode,
            BuiltinVerifierMode::Off
        );
    }

    const MINIMAL_CONFIG_JSON: &str = r#"{
      "orchestration": {
        "default_strategy": "consensus",
        "max_refinement_rounds": 2,
        "conflict_policy": "lead_priority",
        "builtin_verifier": { "mode": "auto" }
      },
      "roles": { "architect": "claude-code", "reviewer": "codex" },
      "profiles": []
    }"#;

    #[test]
    fn in_memory_config_source_is_default_and_trusted() {
        // from_json_str / embedded default are operator-controlled (the shipped
        // default keeps the verifier Off), so they are trusted.
        let config = Config::from_json_str(DEFAULT_CONFIG).unwrap();
        assert_eq!(config.source(), ConfigSource::Default);
        assert!(config.builtin_verifier_exec_trusted(false));
    }

    #[test]
    fn load_from_stamps_repo_local_config_as_project() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("nerve.config.json"), MINIMAL_CONFIG_JSON).unwrap();
        let config = Config::load_from(dir.path()).unwrap();
        assert_eq!(config.source(), ConfigSource::Project);
    }

    #[test]
    fn project_config_is_untrusted_for_exec_without_consent() {
        // The codex BLOCK: a cloned repo's nerve.config.json enabling `auto`
        // must NOT be trusted to run code unless the operator consents
        // out-of-band (a signal the repo cannot forge).
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("nerve.config.json"), MINIMAL_CONFIG_JSON).unwrap();
        let project = Config::load_from(dir.path()).unwrap();

        assert_eq!(project.source(), ConfigSource::Project);
        assert!(!project.builtin_verifier_exec_trusted(false));
        assert!(project.builtin_verifier_exec_trusted(true));
    }

    #[test]
    fn builtin_verifier_command_mode_requires_nonempty_safe_argv() {
        // Empty argv in command mode is rejected.
        let cfg = BuiltinVerifierConfig {
            mode: BuiltinVerifierMode::Command,
            command: Vec::new(),
            timeout_secs: 60,
        };
        assert!(cfg.validate().is_err());

        // A `/`-bearing program is rejected (no shell, PATH lookup only).
        let cfg = BuiltinVerifierConfig {
            mode: BuiltinVerifierMode::Command,
            command: vec!["./run.sh".into()],
            timeout_secs: 60,
        };
        assert!(cfg.validate().is_err());

        // A leading `-` is rejected so a sandbox wrapper can't mis-parse it as
        // one of its own options (S5 sandbox-transparency).
        let cfg = BuiltinVerifierConfig {
            mode: BuiltinVerifierMode::Command,
            command: vec!["--".into(), "true".into()],
            timeout_secs: 60,
        };
        assert!(cfg.validate().is_err());

        // A bare program name is accepted.
        let cfg = BuiltinVerifierConfig {
            mode: BuiltinVerifierMode::Command,
            command: vec!["make".into(), "test".into()],
            timeout_secs: 60,
        };
        cfg.validate().unwrap();
    }

    #[test]
    fn builtin_verifier_zero_timeout_rejected() {
        let cfg = BuiltinVerifierConfig {
            mode: BuiltinVerifierMode::Auto,
            command: Vec::new(),
            timeout_secs: 0,
        };
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn builtin_verifier_round_trips_through_config_json() {
        let config = Config::from_json_str(
            r#"{
              "orchestration": {
                "default_strategy": "consensus",
                "max_refinement_rounds": 2,
                "conflict_policy": "lead_priority",
                "builtin_verifier": { "mode": "command", "command": ["make", "ci"], "timeout_secs": 90 }
              },
              "roles": { "architect": "claude-code", "reviewer": "codex" },
              "profiles": []
            }"#,
        )
        .unwrap();
        let bv = &config.orchestration.builtin_verifier;
        assert_eq!(bv.mode, BuiltinVerifierMode::Command);
        assert_eq!(bv.command, vec!["make", "ci"]);
        assert_eq!(bv.timeout_secs, 90);
    }

    #[test]
    fn sandbox_defaults_to_off_and_no_network() {
        // The shipped/default config never confines silently and never changes
        // existing execution: sandbox is opt-in, network denied when enabled.
        let cfg = SandboxConfig::default();
        assert_eq!(cfg.mode, SandboxMode::Off);
        assert!(!cfg.allow_network);
        assert!(!cfg.is_enabled());
    }

    #[test]
    fn sandbox_round_trips_through_config_json() {
        let config = Config::from_json_str(
            r#"{
              "orchestration": {
                "default_strategy": "consensus",
                "max_refinement_rounds": 2,
                "conflict_policy": "lead_priority",
                "sandbox": { "mode": "required", "allow_network": true }
              },
              "roles": { "architect": "claude-code", "reviewer": "codex" },
              "profiles": []
            }"#,
        )
        .unwrap();
        let sb = config.orchestration.sandbox;
        assert_eq!(sb.mode, SandboxMode::Required);
        assert!(sb.allow_network);
        assert!(sb.is_enabled());
    }

    #[test]
    fn sandbox_absent_section_defaults_off() {
        // A legacy config without a `sandbox` section parses with confinement off.
        let config = Config::from_json_str(
            r#"{
              "orchestration": {
                "default_strategy": "consensus",
                "max_refinement_rounds": 2,
                "conflict_policy": "lead_priority"
              },
              "roles": { "architect": "claude-code", "reviewer": "codex" },
              "profiles": []
            }"#,
        )
        .unwrap();
        assert_eq!(config.orchestration.sandbox.mode, SandboxMode::Off);
        assert!(!config.orchestration.sandbox.is_enabled());
    }

    #[test]
    fn sandbox_rejects_unknown_fields() {
        // deny_unknown_fields guards against typo'd keys silently disabling
        // confinement (e.g. "allownetwork" leaving allow_network at its default).
        let err = Config::from_json_str(
            r#"{
              "orchestration": {
                "default_strategy": "consensus",
                "max_refinement_rounds": 2,
                "conflict_policy": "lead_priority",
                "sandbox": { "mode": "required", "allownetwork": true }
              },
              "roles": { "architect": "claude-code", "reviewer": "codex" },
              "profiles": []
            }"#,
        );
        assert!(err.is_err());
    }

    #[test]
    fn crossfire_action_defaults_off() {
        // A legacy config without a `crossfire_action` key parses as record-only,
        // so existing runs keep their byte-identical advisory-only behavior.
        let config = Config::from_json_str(
            r#"{
              "orchestration": {
                "default_strategy": "consensus",
                "max_refinement_rounds": 2,
                "conflict_policy": "lead_priority"
              },
              "roles": { "architect": "claude-code", "reviewer": "codex" },
              "profiles": []
            }"#,
        )
        .unwrap();
        assert_eq!(config.orchestration.crossfire_action, CrossfireAction::Off);
        assert!(!config.orchestration.crossfire_action.redirects());
        assert!(!config.orchestration.crossfire_action.halts());
    }

    #[test]
    fn crossfire_action_round_trips_each_variant() {
        for (token, expected, redirects, halts) in [
            ("off", CrossfireAction::Off, false, false),
            ("redirect", CrossfireAction::Redirect, true, false),
            ("halt", CrossfireAction::Halt, true, true),
        ] {
            let config = Config::from_json_str(&format!(
                r#"{{
                  "orchestration": {{
                    "default_strategy": "consensus",
                    "max_refinement_rounds": 2,
                    "conflict_policy": "lead_priority",
                    "crossfire_action": "{token}"
                  }},
                  "roles": {{ "architect": "claude-code", "reviewer": "codex" }},
                  "profiles": []
                }}"#
            ))
            .unwrap();
            assert_eq!(config.orchestration.crossfire_action, expected);
            assert_eq!(config.orchestration.crossfire_action.redirects(), redirects);
            assert_eq!(config.orchestration.crossfire_action.halts(), halts);
        }
    }

    #[test]
    fn apply_classifier_defaults_off() {
        // A legacy config without an `apply_classifier` key parses with the
        // classifier disabled, so the apply decision stays byte-identical.
        let config = Config::from_json_str(
            r#"{
              "orchestration": {
                "default_strategy": "consensus",
                "max_refinement_rounds": 2,
                "conflict_policy": "lead_priority"
              },
              "roles": { "architect": "claude-code", "reviewer": "codex" },
              "profiles": []
            }"#,
        )
        .unwrap();
        let ac = &config.orchestration.apply_classifier;
        assert_eq!(ac.mode, ApplyClassifierMode::Off);
        assert!(!ac.mode.classifies());
        assert!(!ac.mode.enforces());
        // Defaults are populated even when the key is absent.
        assert_eq!(ac.max_files, 25);
        assert_eq!(ac.max_lines, 800);
        assert!(ac.flag_destructive_ops);
        assert!(!ac.risky_path_globs.is_empty());
    }

    #[test]
    fn apply_classifier_round_trips_each_variant() {
        for (token, expected, classifies, enforces) in [
            ("off", ApplyClassifierMode::Off, false, false),
            ("advisory", ApplyClassifierMode::Advisory, true, false),
            ("enforce", ApplyClassifierMode::Enforce, true, true),
        ] {
            let config = Config::from_json_str(&format!(
                r#"{{
                  "orchestration": {{
                    "default_strategy": "consensus",
                    "max_refinement_rounds": 2,
                    "conflict_policy": "lead_priority",
                    "apply_classifier": {{ "mode": "{token}", "max_files": 10, "max_lines": 200 }}
                  }},
                  "roles": {{ "architect": "claude-code", "reviewer": "codex" }},
                  "profiles": []
                }}"#
            ))
            .unwrap();
            let ac = &config.orchestration.apply_classifier;
            assert_eq!(ac.mode, expected);
            assert_eq!(ac.mode.classifies(), classifies);
            assert_eq!(ac.mode.enforces(), enforces);
            assert_eq!(ac.max_files, 10);
            assert_eq!(ac.max_lines, 200);
        }
    }

    #[test]
    fn apply_classifier_rejects_unknown_fields() {
        // deny_unknown_fields guards against a typo'd key silently leaving the
        // classifier mis-tuned (e.g. "maxfiles" leaving max_files at its default).
        let err = Config::from_json_str(
            r#"{
              "orchestration": {
                "default_strategy": "consensus",
                "max_refinement_rounds": 2,
                "conflict_policy": "lead_priority",
                "apply_classifier": { "mode": "enforce", "maxfiles": 3 }
              },
              "roles": { "architect": "claude-code", "reviewer": "codex" },
              "profiles": []
            }"#,
        );
        assert!(err.is_err());
    }

    #[test]
    fn apply_classifier_validate_rejects_both_thresholds_zero_when_enabled() {
        // An enabled classifier with both size thresholds at 0 could never flag a
        // patch on size — a likely misconfiguration; validate() rejects it.
        let err = Config::from_json_str(
            r#"{
              "orchestration": {
                "default_strategy": "consensus",
                "max_refinement_rounds": 2,
                "conflict_policy": "lead_priority",
                "apply_classifier": { "mode": "enforce", "max_files": 0, "max_lines": 0 }
              },
              "roles": { "architect": "claude-code", "reviewer": "codex" },
              "profiles": []
            }"#,
        );
        assert!(err.is_err());

        // The same all-zero thresholds are fine when the classifier is Off.
        let ok = Config::from_json_str(
            r#"{
              "orchestration": {
                "default_strategy": "consensus",
                "max_refinement_rounds": 2,
                "conflict_policy": "lead_priority",
                "apply_classifier": { "mode": "off", "max_files": 0, "max_lines": 0 }
              },
              "roles": { "architect": "claude-code", "reviewer": "codex" },
              "profiles": []
            }"#,
        );
        assert!(ok.is_ok());
    }
}
