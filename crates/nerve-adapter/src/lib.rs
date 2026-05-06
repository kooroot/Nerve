use anyhow::{Context, Result};
use async_trait::async_trait;
use nerve_patch::{FilePatch, NvPatch};
use nerve_types::{AgentEvent, AgentOutput, Issue, IssueSeverity, ReviewerFeedback, Task, Verdict};
use std::path::Path;
use tokio::process::Command;
use tokio::sync::mpsc;

#[async_trait]
pub trait ModelAdapter: Send + Sync {
    fn id(&self) -> &str;

    async fn implement(
        &self,
        task: &Task,
        cwd: &Path,
        tx: mpsc::Sender<AgentEvent>,
    ) -> Result<AgentOutput>;

    async fn review(
        &self,
        task: &Task,
        lead_output: &AgentOutput,
        cwd: &Path,
        strictness: &str,
        tx: mpsc::Sender<AgentEvent>,
    ) -> Result<ReviewerFeedback>;

    async fn refine(
        &self,
        task: &Task,
        previous_output: &AgentOutput,
        feedback: &ReviewerFeedback,
        cwd: &Path,
        tx: mpsc::Sender<AgentEvent>,
    ) -> Result<AgentOutput>;
}

#[derive(Debug, Clone)]
pub struct MockAdapter {
    id: String,
}

impl MockAdapter {
    pub fn new(id: impl Into<String>) -> Self {
        Self { id: id.into() }
    }

    pub fn lead() -> Self {
        Self::new("claude-code")
    }

    pub fn reviewer() -> Self {
        Self::new("codex")
    }
}

#[async_trait]
impl ModelAdapter for MockAdapter {
    fn id(&self) -> &str {
        &self.id
    }

    async fn implement(
        &self,
        task: &Task,
        _cwd: &Path,
        tx: mpsc::Sender<AgentEvent>,
    ) -> Result<AgentOutput> {
        send_stdout(&tx, self.id(), "mock lead produced initial patch").await;
        let patch = NvPatch::new(vec![FilePatch::create(
            ".nerve/mock-output.txt",
            format!("Task: {}\nStatus: initial\n", task.prompt),
        )]);
        Ok(AgentOutput::with_patch(
            self.id(),
            "Initial mock implementation",
            patch,
        ))
    }

    async fn review(
        &self,
        _task: &Task,
        lead_output: &AgentOutput,
        _cwd: &Path,
        strictness: &str,
        tx: mpsc::Sender<AgentEvent>,
    ) -> Result<ReviewerFeedback> {
        send_stdout(
            &tx,
            self.id(),
            &format!("mock reviewer checked patch with {strictness} strictness"),
        )
        .await;

        if lead_output.raw_text.contains("Refined") {
            return Ok(ReviewerFeedback::lgtm(self.id(), "LGTM"));
        }

        Ok(ReviewerFeedback {
            reviewer_id: self.id().to_string(),
            verdict: Verdict::RequestChanges,
            issues: vec![Issue {
                severity: IssueSeverity::Warning,
                message: "Add refinement marker before accepting the patch".to_string(),
            }],
            suggested_patch: None,
            raw_text: "Request changes: add refinement marker".to_string(),
        })
    }

    async fn refine(
        &self,
        task: &Task,
        _previous_output: &AgentOutput,
        feedback: &ReviewerFeedback,
        _cwd: &Path,
        tx: mpsc::Sender<AgentEvent>,
    ) -> Result<AgentOutput> {
        send_stdout(&tx, self.id(), "mock lead refined patch").await;
        let patch = NvPatch::new(vec![FilePatch::create(
            ".nerve/mock-output.txt",
            format!(
                "Task: {}\nStatus: refined\nReviewer: {}\n",
                task.prompt, feedback.raw_text
            ),
        )]);
        Ok(AgentOutput::with_patch(
            self.id(),
            "Refined mock implementation",
            patch,
        ))
    }
}

#[derive(Debug, Clone)]
pub struct SubprocessAdapter {
    id: String,
    command: String,
    args: Vec<String>,
}

impl SubprocessAdapter {
    pub fn new(id: impl Into<String>, command: impl Into<String>, args: Vec<String>) -> Self {
        Self {
            id: id.into(),
            command: command.into(),
            args,
        }
    }

    pub fn claude_code() -> Self {
        Self::new(
            "claude-code",
            "claude",
            vec![
                "-p".to_string(),
                "{prompt}".to_string(),
                "--output-format".to_string(),
                "stream-json".to_string(),
                "--verbose".to_string(),
            ],
        )
    }

    pub fn codex() -> Self {
        Self::new(
            "codex",
            "codex",
            vec![
                "exec".to_string(),
                "--json".to_string(),
                "{prompt}".to_string(),
            ],
        )
    }

    async fn run_prompt(
        &self,
        prompt: String,
        cwd: &Path,
        tx: &mpsc::Sender<AgentEvent>,
    ) -> Result<String> {
        let args = self
            .args
            .iter()
            .map(|arg| arg.replace("{prompt}", &prompt))
            .collect::<Vec<_>>();

        let output = Command::new(&self.command)
            .args(args)
            .current_dir(cwd)
            .output()
            .await
            .with_context(|| format!("failed to spawn `{}` adapter", self.id))?;

        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();

        for line in stdout.lines() {
            send_stdout(tx, self.id(), line).await;
        }
        for line in stderr.lines() {
            send_stderr(tx, self.id(), line).await;
        }

        if !output.status.success() {
            anyhow::bail!(
                "adapter `{}` exited with status {}: {}",
                self.id,
                output.status,
                stderr.trim()
            );
        }

        Ok(stdout)
    }
}

#[async_trait]
impl ModelAdapter for SubprocessAdapter {
    fn id(&self) -> &str {
        &self.id
    }

    async fn implement(
        &self,
        task: &Task,
        cwd: &Path,
        tx: mpsc::Sender<AgentEvent>,
    ) -> Result<AgentOutput> {
        let prompt = format!(
            "You are the Lead Agent for Nerve. Produce a concise implementation patch for this task. Return a unified diff when possible.\n\nTask:\n{}",
            task.prompt
        );
        let raw = self.run_prompt(prompt, cwd, &tx).await?;
        output_from_raw_text(self.id(), cwd, raw)
    }

    async fn review(
        &self,
        task: &Task,
        lead_output: &AgentOutput,
        cwd: &Path,
        strictness: &str,
        tx: mpsc::Sender<AgentEvent>,
    ) -> Result<ReviewerFeedback> {
        let prompt = format!(
            "You are the Reviewer Agent for Nerve. Review the Lead output with {strictness} strictness. Start with LGTM, REQUEST_CHANGES, or BLOCK.\n\nTask:\n{}\n\nLead output:\n{}",
            task.prompt, lead_output.raw_text
        );
        let raw = self.run_prompt(prompt, cwd, &tx).await?;
        Ok(feedback_from_text(self.id(), raw))
    }

    async fn refine(
        &self,
        task: &Task,
        previous_output: &AgentOutput,
        feedback: &ReviewerFeedback,
        cwd: &Path,
        tx: mpsc::Sender<AgentEvent>,
    ) -> Result<AgentOutput> {
        let prompt = format!(
            "You are the Lead Agent for Nerve. Refine your previous implementation using the reviewer feedback. Return the final unified diff when possible.\n\nTask:\n{}\n\nPrevious output:\n{}\n\nReviewer feedback:\n{}",
            task.prompt, previous_output.raw_text, feedback.raw_text
        );
        let raw = self.run_prompt(prompt, cwd, &tx).await?;
        output_from_raw_text(self.id(), cwd, raw)
    }
}

pub fn default_adapters(mock: bool) -> Vec<Box<dyn ModelAdapter>> {
    if mock {
        vec![
            Box::new(MockAdapter::lead()),
            Box::new(MockAdapter::reviewer()),
        ]
    } else {
        vec![
            Box::new(SubprocessAdapter::claude_code()),
            Box::new(SubprocessAdapter::codex()),
        ]
    }
}

fn feedback_from_text(reviewer_id: &str, raw_text: String) -> ReviewerFeedback {
    let upper = raw_text.to_uppercase();
    let verdict = if upper.contains("BLOCK") {
        Verdict::Block
    } else if upper.contains("REQUEST_CHANGES") || upper.contains("REQUEST CHANGES") {
        Verdict::RequestChanges
    } else if upper.contains("LGTM") {
        Verdict::Lgtm
    } else {
        Verdict::RequestChanges
    };

    let issues = if verdict == Verdict::Lgtm {
        Vec::new()
    } else {
        vec![Issue {
            severity: if verdict == Verdict::Block {
                IssueSeverity::Blocking
            } else {
                IssueSeverity::Warning
            },
            message: raw_text
                .lines()
                .next()
                .unwrap_or("review requested changes")
                .to_string(),
        }]
    };

    ReviewerFeedback {
        reviewer_id: reviewer_id.to_string(),
        verdict,
        issues,
        suggested_patch: None,
        raw_text,
    }
}

fn output_from_raw_text(agent_id: &str, cwd: &Path, raw_text: String) -> Result<AgentOutput> {
    let patch_input = extract_patch_candidate_text(&raw_text);
    match NvPatch::from_unified_diff(cwd, &patch_input)? {
        Some(patch) if !patch.is_empty() => Ok(AgentOutput::with_patch(agent_id, raw_text, patch)),
        _ => Ok(AgentOutput::text(agent_id, raw_text)),
    }
}

fn extract_patch_candidate_text(raw_text: &str) -> String {
    let mut out = String::from(raw_text);
    for line in raw_text.lines() {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };

        collect_json_strings(&value, &mut out);
    }
    out
}

fn collect_json_strings(value: &serde_json::Value, out: &mut String) {
    match value {
        serde_json::Value::String(value) => {
            out.push('\n');
            out.push_str(value);
        }
        serde_json::Value::Array(values) => {
            for value in values {
                collect_json_strings(value, out);
            }
        }
        serde_json::Value::Object(values) => {
            for value in values.values() {
                collect_json_strings(value, out);
            }
        }
        serde_json::Value::Null | serde_json::Value::Bool(_) | serde_json::Value::Number(_) => {}
    }
}

async fn send_stdout(tx: &mpsc::Sender<AgentEvent>, agent_id: &str, line: &str) {
    let _ = tx
        .send(AgentEvent::Stdout {
            agent_id: agent_id.to_string(),
            line: line.to_string(),
        })
        .await;
}

async fn send_stderr(tx: &mpsc::Sender<AgentEvent>, agent_id: &str, line: &str) {
    let _ = tx
        .send(AgentEvent::Stderr {
            agent_id: agent_id.to_string(),
            line: line.to_string(),
        })
        .await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_raw_unified_diff_to_structured_agent_output() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("file.txt"), "before\n").unwrap();
        let raw = "\
Lead notes before the patch.

--- a/file.txt
+++ b/file.txt
@@ -1 +1 @@
-before
+after
"
        .to_string();

        let output = output_from_raw_text("claude-code", dir.path(), raw).unwrap();

        let patch = output.proposed_patch.unwrap();
        assert_eq!(patch.files.len(), 1);
        assert_eq!(patch.files[0].modified, "after\n");
    }

    #[test]
    fn leaves_plain_text_output_unstructured() {
        let dir = tempfile::tempdir().unwrap();
        let output =
            output_from_raw_text("claude-code", dir.path(), "no diff here".to_string()).unwrap();

        assert!(output.proposed_patch.is_none());
    }

    #[test]
    fn extracts_structured_patch_from_jsonl_strings() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("file.txt"), "before\n").unwrap();
        let raw = r#"{"type":"assistant","message":{"content":[{"type":"text","text":"--- a/file.txt\n+++ b/file.txt\n@@ -1 +1 @@\n-before\n+after\n"}]}}"#
            .to_string();

        let output = output_from_raw_text("claude-code", dir.path(), raw).unwrap();

        let patch = output.proposed_patch.unwrap();
        assert_eq!(patch.files[0].modified, "after\n");
    }

    #[test]
    fn extracts_structured_patch_from_fenced_claude_jsonl_strings() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("file.txt"), "before\n").unwrap();
        let raw = r#"{"type":"assistant","message":{"content":[{"type":"text","text":"```diff\n--- a/file.txt\n+++ b/file.txt\n@@ -1 +1 @@\n-before\n+after\n```"}]}}"#
            .to_string();

        let output = output_from_raw_text("claude-code", dir.path(), raw).unwrap();

        let patch = output.proposed_patch.unwrap();
        assert_eq!(patch.files[0].modified, "after\n");
    }

    #[test]
    fn claude_adapter_uses_verbose_stream_json() {
        let adapter = SubprocessAdapter::claude_code();

        assert!(adapter.args.contains(&"--output-format".to_string()));
        assert!(adapter.args.contains(&"stream-json".to_string()));
        assert!(adapter.args.contains(&"--verbose".to_string()));
    }
}
