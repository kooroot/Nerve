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
    #[error("goal intent free_form must not be empty")]
    EmptyGoalIntentFreeForm,
    #[error("goal intent rationale must not be empty")]
    EmptyGoalIntentRationale,
    #[error("goal intent source_adapter must not be empty")]
    EmptyGoalIntentSourceAdapter,
    #[error("check_ulimit field `{0}` must be greater than 0 when set")]
    InvalidUlimitValue(&'static str),
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
            || program.contains('/')
            || program.contains('\\')
            || program.split('/').any(|seg| seg == "..")
            || program.contains("..")
        {
            return Err(ConfigError::InvalidCheckCmdProgram(program.clone()));
        }
        if self.timeout_secs == 0 {
            return Err(ConfigError::ZeroTimeout);
        }
        for key in self.env.keys() {
            if key.is_empty()
                || key.contains('=')
                || key.contains('\0')
                || key.chars().any(|c| c.is_ascii_control())
            {
                return Err(ConfigError::InvalidEnvKey(key.clone()));
            }
        }
        Ok(())
    }
}

fn default_goal_timeout_secs() -> u64 {
    60
}
