use anyhow::{Context, Result};
use async_trait::async_trait;
use nerve_patch::{FilePatch, NvPatch};
use nerve_types::{
    AgentEvent, AgentOutput, Issue, IssueSeverity, ReviewerFeedback, Task, UsageStats, Verdict,
};
use serde_json::{Map, Value};
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;
use thiserror::Error;
use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc;
use tokio::time::timeout;

/// Default subprocess wall-clock timeout (5 minutes). Wraps both Claude and Codex
/// adapter invocations to prevent runaway LLM CLIs from hanging Nerve.
pub const DEFAULT_ADAPTER_TIMEOUT_SECS: u64 = 300;

/// Default per-stream output cap (16 MiB) applied to both stdout and stderr.
/// Exceeding this kills the child and surfaces `AdapterError::OutputTooLarge`.
pub const DEFAULT_MAX_OUTPUT_BYTES: usize = 16 * 1024 * 1024;

#[derive(Debug, Error)]
pub enum AdapterError {
    #[error("adapter timed out after {secs}s")]
    Timeout { secs: u64 },
    #[error("adapter output exceeded limit of {limit_bytes} bytes")]
    OutputTooLarge { limit_bytes: usize },
}

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

    async fn crossfire(
        &self,
        task: &Task,
        scratch_summary: &str,
        cwd: &Path,
        strictness: &str,
        tx: mpsc::Sender<AgentEvent>,
    ) -> Result<ReviewerFeedback> {
        let lead_output = AgentOutput::text("scratch", scratch_summary);
        self.review(task, &lead_output, cwd, strictness, tx).await
    }
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
            "mock-output.txt",
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
            cost: None,
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
            "mock-output.txt",
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
    timeout_secs: u64,
    max_output_bytes: usize,
}

impl SubprocessAdapter {
    pub fn new(id: impl Into<String>, command: impl Into<String>, args: Vec<String>) -> Self {
        Self {
            id: id.into(),
            command: command.into(),
            args,
            timeout_secs: DEFAULT_ADAPTER_TIMEOUT_SECS,
            max_output_bytes: DEFAULT_MAX_OUTPUT_BYTES,
        }
    }

    /// Override the wall-clock timeout applied to each adapter invocation.
    pub fn with_timeout_secs(mut self, secs: u64) -> Self {
        self.timeout_secs = secs;
        self
    }

    /// Override the per-stream output cap (bytes) before the child is killed.
    pub fn with_max_output_bytes(mut self, bytes: usize) -> Self {
        self.max_output_bytes = bytes;
        self
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
                "--skip-git-repo-check".to_string(),
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

        // Detach the child from the parent TTY: stdin null, stdout/stderr piped
        // so we can stream-cap them. Without Stdio::null on stdin, a raw-TTY
        // parent could leak keystrokes into the spawned LLM CLI.
        let mut child = Command::new(&self.command)
            .args(args)
            .current_dir(cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("failed to spawn `{}` adapter", self.id))?;

        let stdout_pipe = child
            .stdout
            .take()
            .context("subprocess stdout pipe unavailable")?;
        let stderr_pipe = child
            .stderr
            .take()
            .context("subprocess stderr pipe unavailable")?;

        let drain = async {
            // try_join short-circuits on the first error so a single overflowing
            // stream doesn't block forever on the still-live partner stream.
            let (stdout, stderr) = tokio::try_join!(
                drain_stream(
                    stdout_pipe,
                    self.id.clone(),
                    self.max_output_bytes,
                    tx.clone(),
                    StreamKind::Stdout,
                ),
                drain_stream(
                    stderr_pipe,
                    self.id.clone(),
                    self.max_output_bytes,
                    tx.clone(),
                    StreamKind::Stderr,
                ),
            )?;
            Ok::<_, anyhow::Error>((stdout, stderr))
        };

        let (stdout, stderr) = match timeout(Duration::from_secs(self.timeout_secs), drain).await {
            Ok(Ok(pair)) => pair,
            Ok(Err(drain_err)) => {
                // One of the drains failed (e.g., output cap exceeded). Kill the
                // child so the partner drain doesn't hang and so the LLM CLI
                // doesn't keep burning quota.
                let _ = child.start_kill();
                let _ = child.wait().await;
                return Err(drain_err);
            }
            Err(_) => {
                let _ = child.start_kill();
                let _ = child.wait().await;
                return Err(AdapterError::Timeout {
                    secs: self.timeout_secs,
                }
                .into());
            }
        };

        let status = child
            .wait()
            .await
            .with_context(|| format!("failed to await `{}` adapter exit status", self.id))?;

        if !status.success() {
            anyhow::bail!(
                "adapter `{}` exited with status {}: {}",
                self.id,
                status,
                stderr.trim()
            );
        }

        let _ = tx
            .send(AgentEvent::Done {
                agent_id: self.id.clone(),
            })
            .await;

        Ok(stdout)
    }
}

#[derive(Debug, Clone, Copy)]
enum StreamKind {
    Stdout,
    Stderr,
}

/// Read a child stream line-by-line, forwarding each line to the event channel
/// while enforcing `max_bytes`. Returns the buffered text. If the cap is
/// exceeded, surfaces `AdapterError::OutputTooLarge`; the caller is responsible
/// for killing the child after this future resolves with an error.
async fn drain_stream<R>(
    reader: R,
    agent_id: String,
    max_bytes: usize,
    tx: mpsc::Sender<AgentEvent>,
    kind: StreamKind,
) -> Result<String>
where
    R: AsyncRead + Unpin,
{
    let mut buffered = String::new();
    let mut lines = BufReader::new(reader).lines();

    while let Some(line) = lines
        .next_line()
        .await
        .with_context(|| format!("failed to read {:?} from `{}` adapter", kind, agent_id))?
    {
        // +1 accounts for the trailing newline the BufReader stripped.
        if buffered.len().saturating_add(line.len()).saturating_add(1) > max_bytes {
            return Err(AdapterError::OutputTooLarge {
                limit_bytes: max_bytes,
            }
            .into());
        }
        buffered.push_str(&line);
        buffered.push('\n');

        match kind {
            StreamKind::Stdout => send_stdout(&tx, &agent_id, &line).await,
            StreamKind::Stderr => send_stderr(&tx, &agent_id, &line).await,
        }
    }

    Ok(buffered)
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
        feedback_from_raw_text(self.id(), cwd, raw)
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
    let verdict = parse_verdict(&raw_text);

    let issues = if verdict == Verdict::Lgtm {
        Vec::new()
    } else {
        vec![Issue {
            severity: if verdict == Verdict::Block {
                IssueSeverity::Blocking
            } else {
                IssueSeverity::Warning
            },
            message: issue_summary_from_text(&raw_text),
        }]
    };

    ReviewerFeedback {
        reviewer_id: reviewer_id.to_string(),
        verdict,
        issues,
        suggested_patch: None,
        cost: usage_from_raw_text(&raw_text),
        raw_text,
    }
}

fn feedback_from_raw_text(
    reviewer_id: &str,
    cwd: &Path,
    raw_text: String,
) -> Result<ReviewerFeedback> {
    let mut feedback = feedback_from_text(reviewer_id, raw_text);
    let patch_input = extract_patch_candidate_text(&feedback.raw_text);
    if let Some(patch) = NvPatch::from_unified_diff(cwd, &patch_input)?
        && !patch.is_empty()
    {
        feedback.suggested_patch = Some(patch);
    }
    Ok(feedback)
}

fn parse_verdict(raw_text: &str) -> Verdict {
    let Some(first_line) = raw_text.lines().find(|line| !line.trim().is_empty()) else {
        return Verdict::RequestChanges;
    };
    let upper = first_line.trim_start().to_uppercase();
    if has_verdict_prefix(&upper, "LGTM") {
        Verdict::Lgtm
    } else if has_verdict_prefix(&upper, "REQUEST_CHANGES")
        || has_verdict_prefix(&upper, "REQUEST CHANGES")
    {
        Verdict::RequestChanges
    } else if has_verdict_prefix(&upper, "BLOCK") {
        Verdict::Block
    } else {
        Verdict::RequestChanges
    }
}

fn has_verdict_prefix(line: &str, prefix: &str) -> bool {
    let Some(rest) = line.strip_prefix(prefix) else {
        return false;
    };
    rest.chars()
        .next()
        .is_none_or(|ch| !ch.is_ascii_alphanumeric() && ch != '_')
}

fn issue_summary_from_text(raw_text: &str) -> String {
    let mut saw_verdict = false;
    for line in raw_text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        if !saw_verdict {
            saw_verdict = true;
            if let Some(summary) = strip_verdict_prefix(trimmed) {
                if !summary.is_empty() {
                    return summary.to_string();
                }
                continue;
            }
            return trimmed.to_string();
        }

        return trimmed.to_string();
    }

    "review requested changes".to_string()
}

fn strip_verdict_prefix(line: &str) -> Option<&str> {
    let trimmed = line.trim_start();
    for prefix in ["REQUEST_CHANGES", "REQUEST CHANGES", "BLOCK", "LGTM"] {
        if trimmed
            .get(..prefix.len())
            .is_some_and(|value| value.eq_ignore_ascii_case(prefix))
            && has_verdict_prefix(&trimmed.to_uppercase(), prefix)
        {
            return Some(trimmed[prefix.len()..].trim_start_matches([':', '-', ' ']));
        }
    }
    None
}

fn output_from_raw_text(agent_id: &str, cwd: &Path, raw_text: String) -> Result<AgentOutput> {
    let usage = usage_from_raw_text(&raw_text);
    let patch_input = extract_patch_candidate_text(&raw_text);
    let mut output = match NvPatch::from_unified_diff(cwd, &patch_input)? {
        Some(patch) if !patch.is_empty() => AgentOutput::with_patch(agent_id, raw_text, patch),
        _ => AgentOutput::text(agent_id, raw_text),
    };
    output.cost = usage;
    Ok(output)
}

fn extract_patch_candidate_text(raw_text: &str) -> String {
    let mut out = String::new();
    for line in raw_text.lines() {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };

        collect_adapter_message_text(&value, &mut out);
    }

    if out.is_empty() {
        raw_text.to_string()
    } else {
        out
    }
}

fn collect_adapter_message_text(value: &serde_json::Value, out: &mut String) {
    if value.get("type").and_then(serde_json::Value::as_str) == Some("assistant") {
        collect_content_text(&value["message"]["content"], out);
        return;
    }

    if value.get("type").and_then(serde_json::Value::as_str) == Some("item.completed") {
        if let Some(text) = value["item"]["text"].as_str() {
            push_candidate_text(out, text);
        }
        collect_content_text(&value["item"]["content"], out);
        return;
    }

    if value.get("role").and_then(serde_json::Value::as_str) == Some("assistant") {
        if let Some(text) = value.get("text").and_then(serde_json::Value::as_str) {
            push_candidate_text(out, text);
        }
        collect_content_text(&value["content"], out);
    }

    if value
        .get("item")
        .and_then(|item| item.get("role"))
        .and_then(serde_json::Value::as_str)
        == Some("assistant")
    {
        if let Some(text) = value["item"]["text"].as_str() {
            push_candidate_text(out, text);
        }
        collect_content_text(&value["item"]["content"], out);
    }
}

fn collect_content_text(value: &serde_json::Value, out: &mut String) {
    match value {
        serde_json::Value::String(text) => push_candidate_text(out, text),
        serde_json::Value::Array(items) => {
            for item in items {
                if item.get("type").and_then(serde_json::Value::as_str) == Some("text")
                    && let Some(text) = item.get("text").and_then(serde_json::Value::as_str)
                {
                    push_candidate_text(out, text);
                }
            }
        }
        serde_json::Value::Object(item) => {
            if item.get("type").and_then(serde_json::Value::as_str) == Some("text")
                && let Some(text) = item.get("text").and_then(serde_json::Value::as_str)
            {
                push_candidate_text(out, text);
            }
        }
        serde_json::Value::Null | serde_json::Value::Bool(_) | serde_json::Value::Number(_) => {}
    }
}

fn push_candidate_text(out: &mut String, text: &str) {
    if !out.is_empty() {
        out.push('\n');
    }
    out.push_str(text);
}

fn usage_from_raw_text(raw_text: &str) -> Option<UsageStats> {
    let mut latest = None;
    for line in raw_text.lines() {
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        collect_latest_usage(&value, &mut latest);
    }
    latest.filter(|usage: &UsageStats| {
        usage.total_tokens() > 0 || usage.estimated_cost_microusd.is_some()
    })
}

fn collect_latest_usage(value: &Value, latest: &mut Option<UsageStats>) {
    match value {
        Value::Object(map) => {
            if let Some(usage) = parse_usage_object(map) {
                *latest = Some(usage);
            }
            for child in map.values() {
                collect_latest_usage(child, latest);
            }
        }
        Value::Array(items) => {
            for item in items {
                collect_latest_usage(item, latest);
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {}
    }
}

fn parse_usage_object(map: &Map<String, Value>) -> Option<UsageStats> {
    let input_tokens = first_u64(
        map,
        &[
            "input_tokens",
            "prompt_tokens",
            "total_input_tokens",
            "cached_input_tokens",
        ],
    )
    .unwrap_or_default();
    let output_tokens = first_u64(
        map,
        &[
            "output_tokens",
            "completion_tokens",
            "total_output_tokens",
            "response_tokens",
        ],
    )
    .unwrap_or_default();
    let total_tokens = first_u64(map, &["total_tokens"]);
    let estimated_cost_microusd = first_u64(
        map,
        &["estimated_cost_microusd", "cost_microusd", "microusd"],
    );

    if input_tokens == 0
        && output_tokens == 0
        && total_tokens.is_none()
        && estimated_cost_microusd.is_none()
    {
        return None;
    }

    let (input_tokens, output_tokens) = if input_tokens == 0 && output_tokens == 0 {
        (total_tokens.unwrap_or_default(), 0)
    } else {
        (input_tokens, output_tokens)
    };

    Some(UsageStats {
        input_tokens,
        output_tokens,
        estimated_cost_microusd,
    })
}

fn first_u64(map: &Map<String, Value>, keys: &[&str]) -> Option<u64> {
    keys.iter().find_map(|key| {
        map.get(*key).and_then(|value| {
            value.as_u64().or_else(|| {
                value
                    .as_f64()
                    .filter(|number| *number >= 0.0)
                    .map(|number| number as u64)
            })
        })
    })
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
    fn parses_verdict_from_leading_token_only() {
        let feedback = feedback_from_text("codex", "LGTM: no blockers remain".to_string());

        assert_eq!(feedback.verdict, Verdict::Lgtm);
        assert!(feedback.issues.is_empty());
    }

    #[test]
    fn issue_message_skips_bare_verdict_line() {
        let feedback = feedback_from_text(
            "codex",
            "REQUEST_CHANGES\n\nThe patch leaks a file descriptor.\nAdd a regression test."
                .to_string(),
        );

        assert_eq!(feedback.verdict, Verdict::RequestChanges);
        assert_eq!(
            feedback.issues[0].message,
            "The patch leaks a file descriptor."
        );
    }

    #[test]
    fn issue_message_keeps_first_line_without_explicit_verdict() {
        let feedback = feedback_from_text("codex", "Please fix the missing test.".to_string());

        assert_eq!(feedback.verdict, Verdict::RequestChanges);
        assert_eq!(feedback.issues[0].message, "Please fix the missing test.");
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
    fn codex_item_completed_with_role_is_not_collected_twice() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("file.txt"), "before\n").unwrap();
        let raw = r#"{"type":"item.completed","item":{"role":"assistant","text":"--- a/file.txt\n+++ b/file.txt\n@@ -1 +1 @@\n-before\n+after\n"}}"#
            .to_string();

        let output = output_from_raw_text("codex", dir.path(), raw).unwrap();

        let patch = output.proposed_patch.unwrap();
        assert_eq!(patch.files.len(), 1);
        assert_eq!(patch.files[0].modified, "after\n");
    }

    #[test]
    fn extracts_suggested_patch_from_reviewer_jsonl() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("file.txt"), "before\n").unwrap();
        let raw = r#"{"type":"assistant","message":{"content":[{"type":"text","text":"REQUEST_CHANGES: use this exact patch\n\n--- a/file.txt\n+++ b/file.txt\n@@ -1 +1 @@\n-before\n+reviewed\n"}]}}"#
            .to_string();

        let feedback = feedback_from_raw_text("codex", dir.path(), raw).unwrap();

        assert_eq!(feedback.verdict, Verdict::RequestChanges);
        let patch = feedback.suggested_patch.unwrap();
        assert_eq!(patch.files[0].modified, "reviewed\n");
    }

    #[test]
    fn ignores_tool_result_diffs_when_extracting_jsonl_patch() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("file.txt"), "before\n").unwrap();
        let raw = r#"{"type":"tool_result","content":"--- /dev/null\n+++ b/tool.txt\n@@ -0,0 +1 @@\n+wrong\n"}
{"type":"assistant","message":{"content":[{"type":"text","text":"--- a/file.txt\n+++ b/file.txt\n@@ -1 +1 @@\n-before\n+after\n"}]}}"#
            .to_string();

        let output = output_from_raw_text("claude-code", dir.path(), raw).unwrap();

        let patch = output.proposed_patch.unwrap();
        assert_eq!(patch.files.len(), 1);
        assert_eq!(patch.files[0].path, Path::new("file.txt"));
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

    #[test]
    fn codex_adapter_skips_repo_trust_check_for_non_interactive_exec() {
        let adapter = SubprocessAdapter::codex();

        assert!(adapter.args.contains(&"--skip-git-repo-check".to_string()));
        assert!(adapter.args.contains(&"--json".to_string()));
    }

    #[test]
    fn extracts_latest_usage_from_jsonl() {
        let raw = r#"{"type":"usage","usage":{"input_tokens":10,"output_tokens":5}}
{"type":"result","usage":{"input_tokens":14,"output_tokens":9,"estimated_cost_microusd":120}}"#;

        let usage = usage_from_raw_text(raw).unwrap();

        assert_eq!(usage.input_tokens, 14);
        assert_eq!(usage.output_tokens, 9);
        assert_eq!(usage.estimated_cost_microusd, Some(120));
    }
}
