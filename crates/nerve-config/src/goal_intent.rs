use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::goal::{ConfigError, GoalSpec};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct GoalIntent {
    pub free_form: String,
    pub proposed_spec: GoalSpec,
    pub rationale: String,
    pub source_adapter: String,
    pub created_at: DateTime<Utc>,
}

impl GoalIntent {
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.free_form.trim().is_empty() {
            return Err(ConfigError::EmptyGoalIntentFreeForm);
        }
        if self.rationale.trim().is_empty() {
            return Err(ConfigError::EmptyGoalIntentRationale);
        }
        if self.source_adapter.trim().is_empty() {
            return Err(ConfigError::EmptyGoalIntentSourceAdapter);
        }
        self.proposed_spec.validate()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn sample_spec() -> GoalSpec {
        GoalSpec {
            id: "intent-1".into(),
            check_cmd: vec!["cargo".into(), "test".into()],
            timeout_secs: 60,
            cwd: None,
            env: BTreeMap::new(),
            no_progress_max: None,
        }
    }

    #[test]
    fn goal_intent_validate_round_trip() {
        let intent = GoalIntent {
            free_form: "run cargo tests until they pass".into(),
            proposed_spec: sample_spec(),
            rationale: "user asked for cargo test based exit".into(),
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
    fn goal_intent_validate_rejects_empty_free_form() {
        let intent = GoalIntent {
            free_form: "   ".into(),
            proposed_spec: sample_spec(),
            rationale: "ok".into(),
            source_adapter: "claude-code".into(),
            created_at: Utc::now(),
        };
        assert_eq!(intent.validate(), Err(ConfigError::EmptyGoalIntentFreeForm));
    }

    #[test]
    fn goal_intent_validate_rejects_empty_rationale() {
        let intent = GoalIntent {
            free_form: "do the thing".into(),
            proposed_spec: sample_spec(),
            rationale: "".into(),
            source_adapter: "claude-code".into(),
            created_at: Utc::now(),
        };
        assert_eq!(
            intent.validate(),
            Err(ConfigError::EmptyGoalIntentRationale)
        );
    }

    #[test]
    fn goal_intent_validate_rejects_empty_source_adapter() {
        let intent = GoalIntent {
            free_form: "do the thing".into(),
            proposed_spec: sample_spec(),
            rationale: "ok".into(),
            source_adapter: "".into(),
            created_at: Utc::now(),
        };
        assert_eq!(
            intent.validate(),
            Err(ConfigError::EmptyGoalIntentSourceAdapter)
        );
    }

    #[test]
    fn goal_intent_validate_propagates_spec_error() {
        let mut spec = sample_spec();
        spec.check_cmd.clear();
        let intent = GoalIntent {
            free_form: "noop".into(),
            proposed_spec: spec,
            rationale: "noop".into(),
            source_adapter: "mock".into(),
            created_at: Utc::now(),
        };
        assert_eq!(intent.validate(), Err(ConfigError::EmptyCheckCmd));
    }
}
