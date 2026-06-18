use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;
use thiserror::Error;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct GoalSpec {
    pub id: String,
    pub check_cmd: Vec<String>,
    #[serde(default = "default_goal_timeout_secs")]
    pub timeout_secs: u64,
    #[serde(default)]
    pub cwd: Option<PathBuf>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub no_progress_max: Option<u8>,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ConfigError {
    #[error("goal id must not be empty")]
    EmptyGoalId,
    #[error("goal check_cmd must not be empty")]
    EmptyCheckCmd,
    // sec-1 #1: argv only, no sh -c. PATH lookup only for command name.
    #[error("goal check_cmd[0] `{0}` must be a PATH-searchable program name (no `/`, no `..`)")]
    InvalidCheckCmdProgram(String),
    #[error("goal timeout_secs must be greater than 0")]
    ZeroTimeout,
    #[error("goal env key `{0}` contains forbidden characters (`=`, NUL, or control)")]
    InvalidEnvKey(String),
    // H12: env keys that are process-level code-execution vectors are rejected
    // at the deterministic validation chokepoint so neither a `/goal` model
    // proposal nor a repo-local `active-goal.json` can smuggle one past the
    // operator. Trusted process env is supplied via `orchestration.check_env`.
    #[error(
        "goal env key `{0}` is a code-execution vector and may not be set (supply trusted process env via orchestration.check_env)"
    )]
    ForbiddenEnvKey(String),
    #[error("goal env value for key `{0}` contains a forbidden control character")]
    InvalidEnvValue(String),
    #[error("goal intent free_form must not be empty")]
    EmptyGoalIntentFreeForm,
    #[error("goal intent rationale must not be empty")]
    EmptyGoalIntentRationale,
    #[error("goal intent source_adapter must not be empty")]
    EmptyGoalIntentSourceAdapter,
    #[error("check_ulimit field `{0}` must be greater than 0 when set")]
    InvalidUlimitValue(&'static str),
    // Tier 2e (v0.5.0): rpc.* knobs must be non-zero when set.
    #[error("daemon.rpc.{0} must be greater than 0")]
    InvalidRpcValue(&'static str),
    // Tier 3i (v1.0): mcp.servers[].command must be a non-empty argv.
    #[error("mcp.servers[].name must not be empty")]
    EmptyMcpName,
    #[error("mcp.servers[`{0}`] is defined more than once")]
    DuplicateMcpServer(String),
    #[error("mcp.servers[`{0}`].command must be a non-empty argv vector")]
    EmptyMcpCommand(String),
    // Tier 3j (v1.0): mayor_patrol.* knobs must satisfy the documented ranges.
    #[error("orchestration.mayor_patrol.{0} must be greater than 0 when set")]
    InvalidMayorPatrolValue(&'static str),
    #[error("orchestration.mayor_patrol.max_patrols must be in 1..=64 (got {0})")]
    InvalidMayorPatrolMaxPatrols(u32),
    #[error(
        "orchestration.mayor_patrol.claim_ttl_secs ({ttl}) must be at least 2x heartbeat_secs ({hb})"
    )]
    InvalidMayorPatrolClaimTtl { ttl: u32, hb: u32 },
}

/// Env-key namespaces a `GoalSpec` may never carry — the dynamic-linker tunables
/// (`LD_PRELOAD`, `LD_AUDIT`, `LD_LIBRARY_PATH`, `DYLD_INSERT_LIBRARIES`, …),
/// which load attacker-chosen shared objects into the check child. A
/// whole-namespace prefix match is deliberate: every member is a code-execution
/// vector, while no legitimate check env begins `LD_`/`DYLD_` (`LDFLAGS` does NOT
/// — the underscore boundary keeps compiler-flag vars clear).
const FORBIDDEN_ENV_PREFIXES: &[&str] = &["LD_", "DYLD_"];

/// Exact env keys a `GoalSpec` may never carry — well-known process-level
/// code-execution vectors that turn a benign check command into arbitrary
/// execution inside the (possibly sandboxed) child. Kept uppercase so
/// [`GoalSpec::validate`] can fold case.
///
/// NOT exhaustive: this is a deterministic tripwire for the unambiguous,
/// universal vectors, not a full capability model — any tool may define its own
/// exec hook. The complete env boundary is the executor's `env_clear` + the
/// operator `orchestration.check_env` allowlist (for inherited env) plus, on the
/// converter path, the interactive confirmation. The list intentionally omits
/// generic, commonly-legitimate names (e.g. `ENV`, `GIT_PAGER`) to avoid
/// over-rejecting real checks. It only ever REJECTS and reads no config, so a
/// repo-local file cannot loosen it — hence, like the H3/H11 monotone layers, it
/// needs no operator-consent provenance gate.
const FORBIDDEN_ENV_EXACT: &[&str] = &[
    // argv[0] resolution hijack — PATH must come from the operator's
    // `orchestration.check_env` allowlist, never the model (a model-set value
    // would override the allowlisted PATH at execution and redirect lookups).
    "PATH",
    // cargo executes these as the compiler / wrapper => arbitrary code.
    "RUSTC_WRAPPER",
    "RUSTC_WORKSPACE_WRAPPER",
    // shell startup / parsing hijacks for any check that spawns sh/bash.
    "BASH_ENV",
    "SHELLOPTS",
    "BASHOPTS",
    "IFS",
    "PROMPT_COMMAND",
    // git runs these as external commands.
    "GIT_SSH_COMMAND",
    "GIT_EXTERNAL_DIFF",
    // interpreter option / module-path injection.
    "PYTHONPATH",
    "PYTHONSTARTUP",
    "NODE_OPTIONS",
    "PERL5OPT",
    "PERL5LIB",
    "RUBYOPT",
    "RUBYLIB",
];

/// Whether `key` (any case) names a process-level code-execution vector that a
/// `GoalSpec` may not carry. See [`FORBIDDEN_ENV_PREFIXES`] /
/// [`FORBIDDEN_ENV_EXACT`]. ASCII-case-insensitive: env names are case-sensitive
/// on Unix (a lowercase `ld_preload` is inert there) but case-INsensitive on
/// Windows, so folding is strictly more restrictive — fail-safe on both.
fn is_forbidden_goal_env_key(key: &str) -> bool {
    let folded = key.to_ascii_uppercase();
    FORBIDDEN_ENV_PREFIXES.iter().any(|p| folded.starts_with(p))
        || FORBIDDEN_ENV_EXACT.contains(&folded.as_str())
}

impl GoalSpec {
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.id.trim().is_empty() {
            return Err(ConfigError::EmptyGoalId);
        }
        if self.check_cmd.is_empty() {
            return Err(ConfigError::EmptyCheckCmd);
        }
        let program = &self.check_cmd[0];
        if program.is_empty()
            || program.starts_with('-')
            || program.contains('/')
            || program.contains('\\')
            || program.split('/').any(|seg| seg == "..")
            || program.contains("..")
        {
            // Reject a leading `-`: a program name never starts with one, and a
            // sandbox wrapper (`sandbox-exec`/`bwrap`) could otherwise mis-parse
            // `check_cmd[0]` as one of ITS own options, executing a different
            // command than the unwrapped gate would (S5 sandbox-transparency).
            return Err(ConfigError::InvalidCheckCmdProgram(program.clone()));
        }
        if self.timeout_secs == 0 {
            return Err(ConfigError::ZeroTimeout);
        }
        for (key, value) in &self.env {
            if key.is_empty()
                || key.contains('=')
                || key.contains('\0')
                || key.chars().any(|c| c.is_ascii_control())
            {
                return Err(ConfigError::InvalidEnvKey(key.clone()));
            }
            // H12: reject code-execution-vector keys at this deterministic
            // chokepoint — it is shared by the `/goal` converter
            // (goal_intent.rs), the argv path, AND the persisted active-goal
            // reload, so a model proposal OR a repo-local `active-goal.json`
            // cannot inject `LD_PRELOAD`/`RUSTC_WRAPPER`/`PATH`/… past the gate.
            if is_forbidden_goal_env_key(key) {
                return Err(ConfigError::ForbiddenEnvKey(key.clone()));
            }
            // Values are forwarded verbatim into the child env; a NUL would
            // break `Command::env` and newlines/tabs are injection-adjacent.
            if value.chars().any(|c| c.is_ascii_control()) {
                return Err(ConfigError::InvalidEnvValue(key.clone()));
            }
        }
        Ok(())
    }
}

fn default_goal_timeout_secs() -> u64 {
    60
}
