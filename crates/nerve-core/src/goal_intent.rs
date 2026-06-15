//! §3 Tier 1b Phase 2 — natural-language `/goal` converter.
//!
//! Wraps a `ModelAdapter::dispatch_oneshot` call with deterministic prompting,
//! JSON extraction, and `GoalSpec::validate` so a user free-form sentence
//! turns into a vetted argv-form `GoalSpec` (plus rationale) ready for the
//! Phase 2 user confirmation prompt.

use chrono::Utc;
use nerve_adapter::{AdapterError, ModelAdapter};
use nerve_config::{ConfigError, GoalIntent, GoalSpec};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use thiserror::Error;
use uuid::Uuid;

/// Literal system prompt fed to the underlying LLM adapter. Kept verbatim per
/// the design doc so adapter behaviour stays auditable and reproducible
/// across sessions (sec-gap §1b.2 "보안 가드" #6).
pub const GOAL_INTENT_SYSTEM_PROMPT: &str = concat!(
    "You convert natural-language end conditions into deterministic shell commands. ",
    "Output a single JSON object with these fields:\n",
    "- \"check_cmd\": [\"argv\", ...] — Program name only (PATH-resolved, no shell metachars, no '/', no '..').\n",
    "- \"timeout_secs\": integer (1..=3600)\n",
    "- \"env\": object {name: value} (only if specific env vars need overriding)\n",
    "- \"rationale\": short string explaining why this command checks the condition.\n",
    "No prose outside the JSON object.",
);

#[derive(Debug, Error)]
pub enum GoalIntentError {
    #[error("free_form prompt must not be empty")]
    EmptyFreeForm,
    #[error("LLM adapter call failed: {0}")]
    AdapterFailed(#[from] AdapterError),
    #[error("LLM response did not contain a JSON object: {raw}")]
    UnparseableResponse { raw: String },
    #[error("LLM proposed an unsafe or incomplete GoalSpec: {reason}")]
    InvalidProposal { reason: String },
    #[error("LLM response failed GoalSpec validation: {0}")]
    ValidationFailed(#[from] ConfigError),
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawIntent {
    check_cmd: Vec<String>,
    #[serde(default)]
    timeout_secs: Option<u64>,
    #[serde(default)]
    env: BTreeMap<String, String>,
    rationale: String,
    #[serde(default)]
    cwd: Option<PathBuf>,
}

#[derive(Clone)]
pub struct GoalIntentConverter {
    adapter: Arc<dyn ModelAdapter>,
}

impl std::fmt::Debug for GoalIntentConverter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GoalIntentConverter")
            .field("adapter_id", &self.adapter.id())
            .finish()
    }
}

impl GoalIntentConverter {
    pub fn new(adapter: Arc<dyn ModelAdapter>) -> Self {
        Self { adapter }
    }

    pub fn adapter_id(&self) -> &str {
        self.adapter.id()
    }

    /// Convert `free_form` into a vetted `GoalIntent`. `cwd` is always recorded
    /// from the call site so the orchestrator freezes the workspace root instead
    /// of trusting model-proposed paths (sec-1 #2).
    pub async fn convert(
        &self,
        free_form: &str,
        cwd: &Path,
    ) -> Result<GoalIntent, GoalIntentError> {
        let trimmed = free_form.trim();
        if trimmed.is_empty() {
            return Err(GoalIntentError::EmptyFreeForm);
        }

        let raw = self
            .adapter
            .dispatch_oneshot(GOAL_INTENT_SYSTEM_PROMPT, trimmed)
            .await?;

        let json =
            extract_json_object(&raw).ok_or_else(|| GoalIntentError::UnparseableResponse {
                raw: truncate_raw(&raw),
            })?;

        let parsed: RawIntent =
            serde_json::from_str(&json).map_err(|err| GoalIntentError::InvalidProposal {
                reason: format!("JSON decode failed: {err}"),
            })?;

        if parsed.rationale.trim().is_empty() {
            return Err(GoalIntentError::InvalidProposal {
                reason: "rationale field was empty".into(),
            });
        }
        if parsed.check_cmd.is_empty() {
            return Err(GoalIntentError::InvalidProposal {
                reason: "check_cmd field was empty".into(),
            });
        }

        let timeout_secs = match parsed.timeout_secs {
            Some(secs) if (1..=3600).contains(&secs) => secs,
            Some(secs) => {
                return Err(GoalIntentError::InvalidProposal {
                    reason: format!("timeout_secs {secs} must satisfy 1..=3600"),
                });
            }
            None => 60,
        };

        if let Some(proposed_cwd) = parsed.cwd.as_ref()
            && proposed_cwd != cwd
        {
            return Err(GoalIntentError::InvalidProposal {
                reason: "cwd field is controlled by caller".into(),
            });
        }

        let resolved_cwd = cwd.to_path_buf();
        let spec = GoalSpec {
            id: Uuid::new_v4().to_string(),
            check_cmd: parsed.check_cmd,
            timeout_secs,
            cwd: Some(resolved_cwd),
            env: parsed.env,
            no_progress_max: None,
        };
        spec.validate()?;

        let intent = GoalIntent {
            free_form: trimmed.to_string(),
            proposed_spec: spec,
            rationale: parsed.rationale.trim().to_string(),
            source_adapter: self.adapter.id().to_string(),
            created_at: Utc::now(),
        };
        intent.validate()?;
        Ok(intent)
    }
}

/// Pull the first balanced JSON object out of `raw`. Tolerates fenced
/// ```json``` blocks and surrounding prose so we don't depend on the LLM
/// emitting a perfectly clean response.
fn extract_json_object(raw: &str) -> Option<String> {
    let candidate = strip_fenced_block(raw).unwrap_or_else(|| raw.to_string());
    let bytes = candidate.as_bytes();
    let mut start = None;
    let mut depth: i32 = 0;
    let mut in_string = false;
    let mut escaped = false;

    for (idx, byte) in bytes.iter().enumerate() {
        let ch = *byte as char;
        if in_string {
            if escaped {
                escaped = false;
                continue;
            }
            if ch == '\\' {
                escaped = true;
                continue;
            }
            if ch == '"' {
                in_string = false;
            }
            continue;
        }

        match ch {
            '"' => in_string = true,
            '{' => {
                if depth == 0 {
                    start = Some(idx);
                }
                depth += 1;
            }
            '}' => {
                depth -= 1;
                if depth == 0
                    && let Some(begin) = start
                {
                    return Some(candidate[begin..=idx].to_string());
                }
            }
            _ => {}
        }
    }
    None
}

fn strip_fenced_block(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    let lower = trimmed.to_ascii_lowercase();
    let fence_idx = lower.find("```json").or_else(|| lower.find("```"))?;
    let after_fence = &trimmed[fence_idx + 3..];
    let after_fence = after_fence.trim_start_matches("json").trim_start();
    let end = after_fence.find("```")?;
    Some(after_fence[..end].to_string())
}

fn truncate_raw(raw: &str) -> String {
    const LIMIT: usize = 512;
    if raw.len() <= LIMIT {
        return raw.to_string();
    }
    let mut boundary = LIMIT.min(raw.len());
    while !raw.is_char_boundary(boundary) {
        boundary -= 1;
    }
    format!("{}...", &raw[..boundary])
}

#[cfg(test)]
mod tests {
    use super::*;
    use nerve_adapter::MockAdapter;
    use std::sync::Arc;

    fn cwd() -> PathBuf {
        std::env::current_dir().expect("cwd")
    }

    #[tokio::test]
    async fn convert_with_mock_adapter_valid_response() {
        let mock = MockAdapter::new("mock-converter");
        mock.set_oneshot_response(
            r#"{"check_cmd":["cargo","test"],"timeout_secs":60,"rationale":"runs tests until they pass"}"#,
        );
        let converter = GoalIntentConverter::new(Arc::new(mock));

        let intent = converter
            .convert("run cargo test until it passes", &cwd())
            .await
            .expect("convert ok");

        assert_eq!(intent.source_adapter, "mock-converter");
        assert_eq!(intent.free_form, "run cargo test until it passes");
        assert_eq!(intent.proposed_spec.check_cmd, vec!["cargo", "test"]);
        assert_eq!(intent.proposed_spec.timeout_secs, 60);
        assert!(intent.proposed_spec.cwd.is_some());
        intent.validate().unwrap();
    }

    #[tokio::test]
    async fn convert_with_fenced_response_strips_code_fence() {
        let mock = MockAdapter::new("mock-fenced");
        mock.set_oneshot_response(
            "Sure, here is the JSON:\n```json\n{\"check_cmd\":[\"cargo\",\"check\"],\"timeout_secs\":120,\"rationale\":\"static type check covers the request\"}\n```\n",
        );
        let converter = GoalIntentConverter::new(Arc::new(mock));

        let intent = converter
            .convert("make sure it compiles", &cwd())
            .await
            .expect("convert ok");

        assert_eq!(intent.proposed_spec.check_cmd, vec!["cargo", "check"]);
        assert_eq!(intent.proposed_spec.timeout_secs, 120);
    }

    #[tokio::test]
    async fn convert_with_unsafe_argv_rejected() {
        let mock = MockAdapter::new("mock-unsafe");
        // GoalSpec::validate rejects argv[0] with '/'.
        mock.set_oneshot_response(
            r#"{"check_cmd":["/bin/sh","-c","rm -rf /"],"timeout_secs":5,"rationale":"unsafe"}"#,
        );
        let converter = GoalIntentConverter::new(Arc::new(mock));

        let err = converter
            .convert("delete everything", &cwd())
            .await
            .expect_err("must reject unsafe argv");

        match err {
            GoalIntentError::ValidationFailed(ConfigError::InvalidCheckCmdProgram(prog)) => {
                assert_eq!(prog, "/bin/sh");
            }
            other => panic!("expected InvalidCheckCmdProgram, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn convert_with_dotdot_argv_rejected() {
        let mock = MockAdapter::new("mock-dotdot");
        mock.set_oneshot_response(
            r#"{"check_cmd":["../evil","arg"],"timeout_secs":5,"rationale":"path escape"}"#,
        );
        let converter = GoalIntentConverter::new(Arc::new(mock));

        let err = converter
            .convert("escape", &cwd())
            .await
            .expect_err("must reject ..");

        assert!(matches!(
            err,
            GoalIntentError::ValidationFailed(ConfigError::InvalidCheckCmdProgram(_))
        ));
    }

    #[tokio::test]
    async fn convert_rejects_model_controlled_cwd() {
        let mock = MockAdapter::new("mock-cwd");
        mock.set_oneshot_response(
            r#"{"check_cmd":["cargo","test"],"timeout_secs":60,"cwd":"/tmp/elsewhere","rationale":"runs tests"}"#,
        );
        let converter = GoalIntentConverter::new(Arc::new(mock));

        let err = converter
            .convert("run tests", &cwd())
            .await
            .expect_err("model must not move the goal cwd");

        match err {
            GoalIntentError::InvalidProposal { reason } => {
                assert!(reason.contains("cwd"));
            }
            other => panic!("expected InvalidProposal, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn convert_with_garbled_response_unparseable() {
        let mock = MockAdapter::new("mock-garbled");
        mock.set_oneshot_response("not json at all");
        let converter = GoalIntentConverter::new(Arc::new(mock));

        let err = converter
            .convert("garbage in", &cwd())
            .await
            .expect_err("must be unparseable");

        match err {
            GoalIntentError::UnparseableResponse { raw } => {
                assert!(raw.contains("not json"));
            }
            other => panic!("expected UnparseableResponse, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn convert_rejects_zero_timeout() {
        let mock = MockAdapter::new("mock-zero");
        mock.set_oneshot_response(
            r#"{"check_cmd":["cargo","test"],"timeout_secs":0,"rationale":"bad timeout"}"#,
        );
        let converter = GoalIntentConverter::new(Arc::new(mock));

        let err = converter
            .convert("timeout zero", &cwd())
            .await
            .expect_err("must reject 0");

        match err {
            GoalIntentError::InvalidProposal { reason } => {
                assert!(reason.contains("timeout_secs"));
            }
            other => panic!("expected InvalidProposal, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn convert_propagates_empty_rationale() {
        let mock = MockAdapter::new("mock-no-rationale");
        mock.set_oneshot_response(
            r#"{"check_cmd":["cargo","test"],"timeout_secs":30,"rationale":""}"#,
        );
        let converter = GoalIntentConverter::new(Arc::new(mock));

        let err = converter
            .convert("no rationale", &cwd())
            .await
            .expect_err("must reject empty rationale");

        match err {
            GoalIntentError::InvalidProposal { reason } => {
                assert!(reason.contains("rationale"));
            }
            other => panic!("expected InvalidProposal, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn convert_propagates_adapter_failure() {
        let mock = MockAdapter::new("mock-fail");
        mock.set_oneshot_error("network down");
        let converter = GoalIntentConverter::new(Arc::new(mock));

        let err = converter
            .convert("anything", &cwd())
            .await
            .expect_err("adapter failure must surface");

        match err {
            GoalIntentError::AdapterFailed(AdapterError::OneshotFailed { reason, .. }) => {
                assert_eq!(reason, "network down");
            }
            other => panic!("expected AdapterFailed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn convert_rejects_empty_free_form() {
        let mock = MockAdapter::new("mock-empty");
        let converter = GoalIntentConverter::new(Arc::new(mock));

        let err = converter
            .convert("   ", &cwd())
            .await
            .expect_err("must reject blank input");

        assert!(matches!(err, GoalIntentError::EmptyFreeForm));
    }

    #[tokio::test]
    async fn default_dispatch_oneshot_is_unsupported() {
        // Demonstrates the default trait impl path: if an adapter doesn't
        // override dispatch_oneshot, the converter surfaces it as
        // AdapterFailed(OneshotNotSupported). Used by the doctor surface.
        struct NoOneshot;
        #[async_trait::async_trait]
        impl ModelAdapter for NoOneshot {
            fn id(&self) -> &str {
                "noop-adapter"
            }
            async fn implement(
                &self,
                _task: &nerve_types::Task,
                _cwd: &Path,
                _tx: tokio::sync::mpsc::Sender<nerve_types::AgentEvent>,
            ) -> anyhow::Result<nerve_types::AgentOutput> {
                unimplemented!()
            }
            async fn review(
                &self,
                _task: &nerve_types::Task,
                _lead_output: &nerve_types::AgentOutput,
                _cwd: &Path,
                _strictness: &str,
                _tx: tokio::sync::mpsc::Sender<nerve_types::AgentEvent>,
            ) -> anyhow::Result<nerve_types::ReviewerFeedback> {
                unimplemented!()
            }
            async fn refine(
                &self,
                _task: &nerve_types::Task,
                _previous_output: &nerve_types::AgentOutput,
                _feedback: &nerve_types::ReviewerFeedback,
                _cwd: &Path,
                _tx: tokio::sync::mpsc::Sender<nerve_types::AgentEvent>,
            ) -> anyhow::Result<nerve_types::AgentOutput> {
                unimplemented!()
            }
        }

        let converter = GoalIntentConverter::new(Arc::new(NoOneshot));
        let err = converter
            .convert("anything", &cwd())
            .await
            .expect_err("oneshot is not supported");

        match err {
            GoalIntentError::AdapterFailed(AdapterError::OneshotNotSupported { adapter }) => {
                assert_eq!(adapter, "noop-adapter");
            }
            other => panic!("expected OneshotNotSupported, got {other:?}"),
        }
    }
}
