use anyhow::{Context, Result};
use globset::{Glob, GlobSetBuilder};
use nerve_types::Task;
use serde::{Deserialize, Serialize};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

pub mod goal;
pub mod goal_intent;
pub use goal::{ConfigError, GoalSpec};
pub use goal_intent::GoalIntent;

const DEFAULT_CONFIG: &str = include_str!("../../../nerve.config.json");

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

impl Config {
    pub fn load() -> Result<Self> {
        Self::load_from(env::current_dir().context("failed to read current directory")?)
    }

    pub fn load_from(cwd: impl AsRef<Path>) -> Result<Self> {
        let cwd_config = cwd.as_ref().join("nerve.config.json");
        if cwd_config.exists() {
            return Self::from_path(&cwd_config);
        }

        if let Some(home) = env::var_os("HOME") {
            let user_config = PathBuf::from(home).join(".config/nerve/config.json");
            if user_config.exists() {
                return Self::from_path(&user_config);
            }
        }

        Self::from_json_str(DEFAULT_CONFIG).context("embedded default config is invalid")
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
        if self.roles.architect.trim().is_empty() {
            anyhow::bail!("roles.architect must not be empty");
        }
        if self.roles.reviewer.trim().is_empty() {
            anyhow::bail!("roles.reviewer must not be empty");
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
    "1.0.0".to_string()
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
        assert_eq!(rpc.envelope_version, "1.0.0");

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
}
