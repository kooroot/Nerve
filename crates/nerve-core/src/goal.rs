use crate::sandbox::{self, SandboxDecision};
#[cfg(unix)]
use crate::ulimit::apply_ulimit;
use crate::ulimit::{CheckUlimit, UlimitError};
use nerve_config::{GoalSpec, SandboxConfig};
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
    #[error("check_ulimit invalid: {0}")]
    Ulimit(#[from] UlimitError),
}

#[derive(Debug, Clone)]
pub struct GoalEvaluator {
    goal: GoalSpec,
    allowed_env: Vec<String>,
    output_cap: usize,
    cwd: PathBuf,
    ulimit: Option<CheckUlimit>,
    sandbox: SandboxConfig,
}

impl GoalEvaluator {
    pub fn new(
        goal: GoalSpec,
        allowed_env: Vec<String>,
        output_cap: usize,
        cwd: PathBuf,
    ) -> Result<Self, GoalError> {
        Self::with_ulimit(goal, allowed_env, output_cap, cwd, None)
    }

    pub fn with_ulimit(
        goal: GoalSpec,
        allowed_env: Vec<String>,
        output_cap: usize,
        cwd: PathBuf,
        ulimit: Option<CheckUlimit>,
    ) -> Result<Self, GoalError> {
        Self::with_options(goal, allowed_env, output_cap, cwd, ulimit, SandboxConfig::default())
    }

    /// Full constructor. S5: `sandbox` confines the check's filesystem writes and
    /// network via an OS backend (defaults to `Off` through the other
    /// constructors, preserving existing behavior).
    pub fn with_options(
        goal: GoalSpec,
        allowed_env: Vec<String>,
        output_cap: usize,
        cwd: PathBuf,
        ulimit: Option<CheckUlimit>,
        sandbox: SandboxConfig,
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
        if let Some(spec) = &ulimit {
            crate::ulimit::validate(spec)?;
        }
        Ok(Self {
            goal,
            allowed_env,
            output_cap,
            cwd,
            ulimit,
            sandbox,
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
                progress: None,
            },
        }
    }

    async fn spawn_and_wait(&self) -> Result<CheckResult, GoalError> {
        // S5: resolve the OS sandbox first. It confines side effects without
        // fabricating success — wrapping the command (Pass still keys on the
        // child's real exit status below), or (under `Required` with no backend)
        // refusing to run (→ Fail). It does not promise confined/unconfined
        // exit-code parity; see the `sandbox` module docs. The check runs in
        // `cwd`, which is always writable; the system temp dir is added so build
        // tools (cargo/rustc) can write their intermediates.
        let extra_writable = vec![std::env::temp_dir()];
        let (program, args): (String, Vec<String>) =
            match sandbox::decide(&self.sandbox, &self.cwd, &self.goal.check_cmd, &extra_writable) {
                SandboxDecision::Unconfined { warning } => {
                    if let Some(message) = warning {
                        tracing::warn!(target: "nerve::sandbox", "{message}");
                    }
                    (
                        self.goal.check_cmd[0].clone(),
                        self.goal.check_cmd[1..].to_vec(),
                    )
                }
                SandboxDecision::Wrap { program, args } => (program, args),
                SandboxDecision::Refuse { reason } => {
                    return Ok(CheckResult::Fail {
                        reason,
                        progress: None,
                    });
                }
            };

        let mut command = Command::new(&program);
        command
            .args(&args)
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

        #[cfg(not(unix))]
        if self.ulimit.is_some() {
            return Err(GoalError::Ulimit(UlimitError::Unsupported));
        }

        #[cfg(unix)]
        if let Some(spec) = self.ulimit.clone() {
            // SAFETY: pre_exec runs in the child after fork() and before the
            // image is replaced. apply_ulimit only invokes setrlimit, which
            // is async-signal-safe.
            unsafe {
                command.pre_exec(move || {
                    apply_ulimit(&spec).map_err(|e| match e {
                        UlimitError::SetRlimit { errno, .. } => {
                            std::io::Error::from_raw_os_error(errno)
                        }
                        _ => std::io::Error::other(e.to_string()),
                    })
                });
            }
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
                    progress: None,
                });
            }
        };

        let status = match child.wait().await {
            Ok(status) => status,
            Err(err) => {
                return Ok(CheckResult::Fail {
                    reason: format!("failed to await check_cmd: {err}"),
                    progress: None,
                });
            }
        };

        let (stdout_res, stderr_res) = drained;
        if let Err(OutputCapExceeded(byte_cap)) = stdout_res {
            return Ok(CheckResult::Fail {
                reason: format!("stdout exceeded {byte_cap} bytes"),
                progress: None,
            });
        }
        if let Err(OutputCapExceeded(byte_cap)) = stderr_res {
            return Ok(CheckResult::Fail {
                reason: format!("stderr exceeded {byte_cap} bytes"),
                progress: None,
            });
        }

        if status.success() {
            Ok(CheckResult::Pass)
        } else {
            // Both reads are `Ok` here — the cap checks above returned early on
            // `Err`. The pass-ratio (S7 progress) comes from whichever stream
            // carried the test summary; tools print it to stdout or stderr.
            let stdout_text = stdout_res.unwrap_or_default();
            let stderr_text = stderr_res.unwrap_or_default();
            let tail = tail_for_reason(&stderr_text);
            let reason = if tail.is_empty() {
                format!("check_cmd exited with status {status}")
            } else {
                format!("status {status}: {tail}")
            };
            let progress = parse_progress(&stdout_text, &stderr_text);
            Ok(CheckResult::Fail { reason, progress })
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

/// S7 best-effort distance-to-goal: the pass-ratio of a recognizable test
/// summary in the failed check's output, in PERMILLE (0..=1000). Recognizes
/// libtest (`N passed; M failed; ...`) and pytest (`M failed, N passed in ...`)
/// summaries in either stream, preferring the LAST such line (the final summary).
/// Returns `None` when nothing is recognized — progress is additive telemetry and
/// a stall hint, never an acceptance signal, so an unparsed check just lacks it.
///
/// When BOTH streams carry a recognizable summary, take the most pessimistic
/// (minimum) ratio rather than letting either stream win by position. The lead
/// controls the check's output, so a forged `1000 passed; 0 failed` on one stream
/// must not be able to mask a real failure summary on the other. Reporting the
/// worst ratio only ever feeds MORE stall pressure (toward abort), never less —
/// consistent with progress being a reject-only signal.
fn parse_progress(stdout: &str, stderr: &str) -> Option<u16> {
    match (parse_progress_in(stdout), parse_progress_in(stderr)) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (only, None) | (None, only) => only,
    }
}

/// Last recognizable test-summary pass-ratio (permille) in one stream.
fn parse_progress_in(text: &str) -> Option<u16> {
    text.lines().filter_map(progress_from_line).next_back()
}

/// Pass-ratio (permille) of a single line, if it carries `passed`/`failed`
/// counts. Either count may be absent (treated as 0); a line with neither, or a
/// zero total, yields `None`.
fn progress_from_line(line: &str) -> Option<u16> {
    let passed = count_before_word(line, "passed");
    let failed = count_before_word(line, "failed");
    let (passed, failed) = match (passed, failed) {
        (None, None) => return None,
        (p, f) => (p.unwrap_or(0), f.unwrap_or(0)),
    };
    let total = passed.checked_add(failed)?;
    if total == 0 {
        return None;
    }
    // passed <= total, so this is <= 1000 and fits in u16; `.min` is belt-and-braces.
    Some((passed.saturating_mul(1000) / total).min(1000) as u16)
}

/// The integer token immediately preceding a whole-word `word` on the line (the
/// LAST such pair if several), e.g. `3` in `3 passed`. Splitting on whitespace
/// and `;,:` isolates libtest/pytest count tokens while ignoring prose, so a bare
/// word like "passed" with no leading number is not miscounted.
fn count_before_word(line: &str, word: &str) -> Option<u64> {
    let tokens: Vec<&str> = line
        .split(|c: char| c.is_whitespace() || matches!(c, ';' | ',' | ':'))
        .filter(|t| !t.is_empty())
        .collect();
    let mut found = None;
    for pair in tokens.windows(2) {
        if pair[1].trim_end_matches('.') == word
            && let Ok(n) = pair[0].parse::<u64>()
        {
            found = Some(n);
        }
    }
    found
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
            CheckResult::Fail { reason, .. } => {
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
            CheckResult::Fail { reason, .. } => {
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

    #[test]
    fn parse_progress_libtest_summary() {
        // libtest separates counts with `;`; 3/4 passed = 750 permille.
        let out = "running 4 tests\n....\n\
            test result: FAILED. 3 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s\n";
        assert_eq!(parse_progress(out, ""), Some(750));
    }

    #[test]
    fn parse_progress_pytest_summary_in_stderr() {
        // pytest writes `M failed, N passed` (failed first); may land in stderr.
        let err = "===== 1 failed, 3 passed in 0.12s =====\n";
        assert_eq!(parse_progress("", err), Some(750));
    }

    #[test]
    fn parse_progress_all_failed_is_zero() {
        assert_eq!(
            parse_progress("test result: FAILED. 0 passed; 2 failed; 0 ignored\n", ""),
            Some(0)
        );
    }

    #[test]
    fn parse_progress_unrecognized_is_none() {
        assert_eq!(parse_progress("error[E0382]: borrow of moved value\n", ""), None);
        // Prose that mentions the words without leading counts must not be parsed.
        assert_eq!(
            parse_progress("the lint check passed but the build failed\n", ""),
            None
        );
    }

    #[test]
    fn parse_progress_prefers_last_summary_line() {
        // With several test binaries, the final summary wins (1/4 = 250 permille).
        let out = "test result: ok. 2 passed; 0 failed\n\
            test result: FAILED. 1 passed; 3 failed\n";
        assert_eq!(parse_progress(out, ""), Some(250));
    }

    #[test]
    fn parse_progress_takes_worst_across_streams() {
        // The lead controls both streams; a forged all-pass summary on one must not
        // mask a real failure summary on the other. The pessimistic (min) ratio wins,
        // so this only ever feeds MORE stall pressure, never a rosier signal.
        let stdout = "test result: ok. 10 passed; 0 failed\n";
        let stderr = "===== 1 failed, 0 passed in 0.01s =====\n";
        assert_eq!(parse_progress(stdout, stderr), Some(0));
        // Order-independent: same worst-case regardless of which stream is rosy.
        assert_eq!(parse_progress(stderr, stdout), Some(0));
    }

    #[test]
    fn check_result_progress_accessor() {
        assert_eq!(CheckResult::Pass.progress(), Some(1.0));
        assert_eq!(CheckResult::Skipped.progress(), None);
        assert_eq!(
            CheckResult::Fail {
                reason: "x".into(),
                progress: Some(750),
            }
            .progress(),
            Some(0.75)
        );
        assert_eq!(
            CheckResult::Fail {
                reason: "x".into(),
                progress: None,
            }
            .progress(),
            None
        );
    }

    #[tokio::test]
    async fn goal_evaluator_fail_carries_parsed_progress() {
        let dir = tempfile::tempdir().unwrap();
        let evaluator = GoalEvaluator::new(
            spec_with_cmd(
                vec![
                    "sh",
                    "-c",
                    "echo 'test result: FAILED. 3 passed; 1 failed; 0 ignored'; exit 1",
                ],
                5,
            ),
            Vec::new(),
            4096,
            dir.path().to_path_buf(),
        )
        .unwrap();
        let result = evaluator.evaluate().await;
        assert_eq!(result.progress(), Some(0.75), "got {result:?}");
        match result {
            CheckResult::Fail { progress, .. } => assert_eq!(progress, Some(750)),
            other => panic!("expected fail with progress, got {other:?}"),
        }
    }

    // --- S5: OS sandbox end-to-end wiring (macOS Seatbelt, real kernel) ---

    /// A `Required` sandbox must not break a legitimate check that writes inside
    /// its working directory: the wrapper runs, the in-cwd write is allowed, and
    /// the check still reports `Pass`.
    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn goal_evaluator_sandbox_required_allows_write_in_cwd() {
        let dir = tempfile::tempdir().unwrap();
        // Canonicalize: the kernel resolves /var/folders/... -> /private/... and
        // the profile must match that resolved view.
        let cwd = std::fs::canonicalize(dir.path()).unwrap();
        let sandbox = SandboxConfig {
            mode: nerve_config::SandboxMode::Required,
            allow_network: false,
        };
        let evaluator = GoalEvaluator::with_options(
            spec_with_cmd(vec!["sh", "-c", "echo ok > marker.txt"], 30),
            Vec::new(),
            4096,
            cwd.clone(),
            None,
            sandbox,
        )
        .unwrap();
        assert_eq!(evaluator.evaluate().await, CheckResult::Pass);
        assert!(cwd.join("marker.txt").exists(), "in-cwd write should persist");
    }

    /// The sandbox never *fabricates* a success: a wrapped check that exits
    /// non-zero still reports `Fail`. This is the precise S5 guarantee (the gate
    /// keys `Pass` strictly on the child's real exit status; the sandbox adds no
    /// success of its own) — distinct from confined/unconfined exit-code parity,
    /// which is inherently unachievable since confinement is observable (see the
    /// `sandbox` module docs / codex S5 r4). The evaluator always grants `cwd` +
    /// the system temp dir as writable, so the nonzero exit from this non-writing
    /// failing check wraps cleanly and the `progress` parsing path is unaffected.
    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn goal_evaluator_sandbox_required_preserves_nonzero_fail() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = std::fs::canonicalize(dir.path()).unwrap();
        let sandbox = SandboxConfig {
            mode: nerve_config::SandboxMode::Required,
            allow_network: false,
        };
        let evaluator = GoalEvaluator::with_options(
            spec_with_cmd(vec!["false"], 30),
            Vec::new(),
            4096,
            cwd,
            None,
            sandbox,
        )
        .unwrap();
        assert!(
            matches!(evaluator.evaluate().await, CheckResult::Fail { .. }),
            "a wrapped failing check must still Fail"
        );
    }
}
