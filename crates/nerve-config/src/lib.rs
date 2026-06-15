use anyhow::{Context, Result};
use globset::{Glob, GlobSetBuilder};
use nerve_types::Task;
use serde::{Deserialize, Serialize};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

pub mod goal;
pub use goal::{ConfigError, GoalSpec};

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
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            protocol: default_daemon_protocol(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DaemonProtocol {
    Line,
    Rpc,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProfileSelection {
    pub id: Option<String>,
    pub lead: String,
    pub reviewer: String,
    pub review_strictness: ReviewStrictness,
    pub max_refinement_rounds: u8,
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
                });
            }
        }

        Ok(ProfileSelection {
            id: None,
            lead: self.roles.architect.clone(),
            reviewer: self.roles.reviewer.clone(),
            review_strictness: ReviewStrictness::Normal,
            max_refinement_rounds: self.orchestration.max_refinement_rounds,
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
}
