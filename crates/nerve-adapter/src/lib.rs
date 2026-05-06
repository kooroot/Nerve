use anyhow::{Context, Result};
use async_trait::async_trait;
use nerve_patch::NvPatch;
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
        let patch = NvPatch::single(
            ".nerve/mock-output.txt",
            "",
            format!("Task: {}\nStatus: initial\n", task.prompt),
        );
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
        let patch = NvPatch::single(
            ".nerve/mock-output.txt",
            "",
            format!(
                "Task: {}\nStatus: refined\nReviewer: {}\n",
                task.prompt, feedback.raw_text
            ),
        );
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
        Ok(AgentOutput::text(self.id(), raw))
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
        Ok(AgentOutput::text(self.id(), raw))
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
