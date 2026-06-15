use nerve_config::GoalSpec;
use nerve_types::CheckResult;
use std::collections::BTreeSet;
use std::path::PathBuf;
use std::process::Stdio;
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;
use tokio::time::{Duration, timeout};

#[derive(Debug, Error)]
pub enum GoalError {
    #[error("goal spec is invalid: {0}")]
    InvalidSpec(String),
    #[error("failed to spawn goal check command: {0}")]
    SpawnFailed(#[from] std::io::Error),
    // v0.2.0: evaluator does not persist goal spec; CLI handles .nerve/goals/<id>.json
    // path traversal in a follow-up. PathInjection is reserved for that surface.
    #[error("goal path injection rejected: {0}")]
    PathInjection(String),
}

#[derive(Debug, Clone)]
pub struct GoalEvaluator {
    goal: GoalSpec,
    allowed_env: Vec<String>,
    output_cap: usize,
    cwd: PathBuf,
}

impl GoalEvaluator {
    pub fn new(
        goal: GoalSpec,
        allowed_env: Vec<String>,
        output_cap: usize,
        cwd: PathBuf,
    ) -> Result<Self, GoalError> {
        goal.validate()
            .map_err(|e| GoalError::InvalidSpec(e.to_string()))?;
        if output_cap == 0 {
            return Err(GoalError::InvalidSpec(
                "output_cap must be greater than 0".to_string(),
            ));
        }
        if !cwd.is_absolute() {
            return Err(GoalError::InvalidSpec(format!(
                "evaluator cwd `{}` must be absolute",
                cwd.display()
            )));
        }
        Ok(Self {
            goal,
            allowed_env,
            output_cap,
            cwd,
        })
    }

    pub fn goal(&self) -> &GoalSpec {
        &self.goal
    }

    pub async fn evaluate(&self) -> CheckResult {
        match self.spawn_and_wait().await {
            Ok(result) => result,
            Err(err) => CheckResult::Fail {
                reason: err.to_string(),
            },
        }
    }

    async fn spawn_and_wait(&self) -> Result<CheckResult, GoalError> {
        let program = &self.goal.check_cmd[0];
        let mut command = Command::new(program);
        command
            .args(&self.goal.check_cmd[1..])
            .current_dir(&self.cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .env_clear();

        let mut seen: BTreeSet<&str> = BTreeSet::new();
        for name in &self.allowed_env {
            if seen.insert(name.as_str())
                && let Ok(value) = std::env::var(name)
            {
                command.env(name, value);
            }
        }
        for (key, value) in &self.goal.env {
            command.env(key, value);
        }

        let mut child = command.spawn()?;
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let cap = self.output_cap;

        let drain = async move {
            let (out, err) = tokio::join!(read_capped(stdout, cap), read_capped(stderr, cap),);
            (out, err)
        };

        let timeout_duration = Duration::from_secs(self.goal.timeout_secs);
        let drained = match timeout(timeout_duration, drain).await {
            Ok(pair) => pair,
            Err(_) => {
                let _ = child.start_kill();
                let _ = child.wait().await;
                return Ok(CheckResult::Fail {
                    reason: format!("timeout after {}s", self.goal.timeout_secs),
                });
            }
        };

        let status = match child.wait().await {
            Ok(status) => status,
            Err(err) => {
                return Ok(CheckResult::Fail {
                    reason: format!("failed to await check_cmd: {err}"),
                });
            }
        };

        let (stdout_res, stderr_res) = drained;
        if let Err(OutputCapExceeded(byte_cap)) = stdout_res {
            return Ok(CheckResult::Fail {
                reason: format!("stdout exceeded {byte_cap} bytes"),
            });
        }
        if let Err(OutputCapExceeded(byte_cap)) = stderr_res {
            return Ok(CheckResult::Fail {
                reason: format!("stderr exceeded {byte_cap} bytes"),
            });
        }

        if status.success() {
            Ok(CheckResult::Pass)
        } else {
            let stderr_text = stderr_res.unwrap_or_default();
            let tail = tail_for_reason(&stderr_text);
            let reason = if tail.is_empty() {
                format!("check_cmd exited with status {status}")
            } else {
                format!("status {status}: {tail}")
            };
            Ok(CheckResult::Fail { reason })
        }
    }
}

struct OutputCapExceeded(usize);

async fn read_capped<R>(reader: Option<R>, cap: usize) -> Result<String, OutputCapExceeded>
where
    R: AsyncRead + Unpin,
{
    let Some(mut reader) = reader else {
        return Ok(String::new());
    };
    // +1 sentinel byte detects overflow: read cap+1 bytes; if filled past cap, cap exceeded.
    let mut buf = Vec::with_capacity(cap.min(8192));
    let mut chunk = [0u8; 8192];
    loop {
        let n = match reader.read(&mut chunk).await {
            Ok(0) => break,
            Ok(n) => n,
            Err(_) => break,
        };
        if buf.len().saturating_add(n) > cap {
            return Err(OutputCapExceeded(cap));
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

fn tail_for_reason(text: &str) -> String {
    const TAIL_LIMIT: usize = 512;
    let trimmed = text.trim_end_matches(['\n', '\r']);
    if trimmed.len() <= TAIL_LIMIT {
        return trimmed.to_string();
    }
    let mut start = trimmed.len() - TAIL_LIMIT;
    while !trimmed.is_char_boundary(start) {
        start += 1;
    }
    trimmed[start..].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn spec_with_cmd(cmd: Vec<&str>, timeout_secs: u64) -> GoalSpec {
        GoalSpec {
            id: "g".into(),
            check_cmd: cmd.into_iter().map(String::from).collect(),
            timeout_secs,
            cwd: None,
            env: BTreeMap::new(),
            no_progress_max: None,
        }
    }

    #[tokio::test]
    async fn goal_evaluator_pass_on_zero_exit() {
        let dir = tempfile::tempdir().unwrap();
        let evaluator = GoalEvaluator::new(
            spec_with_cmd(vec!["true"], 5),
            Vec::new(),
            1024,
            dir.path().to_path_buf(),
        )
        .unwrap();
        assert_eq!(evaluator.evaluate().await, CheckResult::Pass);
    }

    #[tokio::test]
    async fn goal_evaluator_fail_on_nonzero() {
        let dir = tempfile::tempdir().unwrap();
        let evaluator = GoalEvaluator::new(
            spec_with_cmd(vec!["false"], 5),
            Vec::new(),
            1024,
            dir.path().to_path_buf(),
        )
        .unwrap();
        let result = evaluator.evaluate().await;
        assert!(matches!(result, CheckResult::Fail { .. }), "got {result:?}");
    }

    #[tokio::test]
    async fn goal_evaluator_timeout() {
        let dir = tempfile::tempdir().unwrap();
        let evaluator = GoalEvaluator::new(
            spec_with_cmd(vec!["sleep", "10"], 1),
            Vec::new(),
            1024,
            dir.path().to_path_buf(),
        )
        .unwrap();
        let result = evaluator.evaluate().await;
        match result {
            CheckResult::Fail { reason } => {
                assert!(reason.contains("timeout"), "reason = {reason}")
            }
            other => panic!("expected timeout fail, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn goal_evaluator_output_cap() {
        let dir = tempfile::tempdir().unwrap();
        let evaluator = GoalEvaluator::new(
            spec_with_cmd(vec!["yes"], 5),
            Vec::new(),
            128,
            dir.path().to_path_buf(),
        )
        .unwrap();
        let result = evaluator.evaluate().await;
        match result {
            CheckResult::Fail { reason } => {
                assert!(reason.contains("exceeded"), "reason = {reason}")
            }
            other => panic!("expected output cap fail, got {other:?}"),
        }
    }

    #[test]
    fn goal_evaluator_rejects_relative_cwd() {
        let err = GoalEvaluator::new(
            spec_with_cmd(vec!["true"], 5),
            Vec::new(),
            1024,
            PathBuf::from("relative/path"),
        )
        .unwrap_err();
        assert!(matches!(err, GoalError::InvalidSpec(_)));
    }

    #[test]
    fn goal_evaluator_rejects_invalid_spec() {
        let err = GoalEvaluator::new(
            spec_with_cmd(vec!["../evil"], 5),
            Vec::new(),
            1024,
            PathBuf::from("/tmp"),
        )
        .unwrap_err();
        assert!(matches!(err, GoalError::InvalidSpec(_)));
    }
}
