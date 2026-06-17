use crate::sandbox::{self, SandboxDecision};
#[cfg(unix)]
use crate::ulimit::apply_ulimit;
use crate::ulimit::{CheckUlimit, UlimitError};
use nerve_config::{GoalSpec, SandboxConfig};
use nerve_types::CheckResult;
use std::collections::BTreeSet;
use std::path::PathBuf;
use std::process::Stdio;
use tempfile::TempDir;
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
    // H2: minting the per-check private temp dir failed. Fail closed (→ Fail) —
    // we never fall back to a broader writable grant or run with a temp dir
    // outside the sandbox's writable set.
    #[error("failed to create per-check private temp dir: {0}")]
    PrivateTmpDir(std::io::Error),
}

/// H4: failure reason when the `Required` runtime confinement self-test (canary)
/// finds that an out-of-grant write was NOT denied — i.e. the sandbox wrap is
/// ineffective. The check is refused (fail closed) rather than run effectively
/// unconfined.
#[cfg_attr(
    not(any(target_os = "macos", target_os = "linux")),
    allow(dead_code)
)]
const CONFINEMENT_SELFTEST_FAILED: &str = "sandbox confinement self-test failed: an out-of-root write was NOT denied under sandbox.mode=required; refusing to run the check unconfined (fail closed)";

/// H13: the result of one deterministic goal check plus the additive sandbox
/// telemetry the run report surfaces. `ran_unconfined` is true ONLY when the
/// check command actually executed but ran WITHOUT OS confinement because
/// `sandbox.mode = auto` requested a sandbox and no backend was available on this
/// host (the documented "confine-if-possible, else run openly" degrade). It is
/// pure telemetry: it never changes `result`, never gates acceptance, and is
/// false for `Off` (intentionally unconfined — not a degrade), for `Required`
/// (which fails closed instead of degrading), whenever a backend confined the
/// run, and when the check never ran (setup/spawn error).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckOutcome {
    pub result: CheckResult,
    pub ran_unconfined: bool,
}

/// H13: the exact glue predicate threaded into [`CheckOutcome::ran_unconfined`].
/// A degrade-to-unconfined is `Unconfined { warning: Some(_) }` — the `Auto`
/// no-backend case (`Off` carries `warning: None`, so it is correctly NOT a
/// degrade; `Wrap`/`Refuse` are confined/refused). Pure function so the mapping
/// is unit-tested directly even on a host where a backend is always available and
/// `decide` can therefore never return the degrade variant.
fn decision_is_unconfined_degrade(decision: &SandboxDecision) -> bool {
    matches!(decision, SandboxDecision::Unconfined { warning: Some(_) })
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

    pub async fn evaluate(&self) -> CheckOutcome {
        match self.spawn_and_wait().await {
            Ok(outcome) => outcome,
            // The check never successfully ran (per-check tmpdir mint, ulimit, or
            // spawn failed). It did not execute unconfined, so the additive
            // `ran_unconfined` telemetry is false; the verdict is an unchanged Fail.
            Err(err) => CheckOutcome {
                result: CheckResult::Fail {
                    reason: err.to_string(),
                    progress: None,
                },
                ran_unconfined: false,
            },
        }
    }

    async fn spawn_and_wait(&self) -> Result<CheckOutcome, GoalError> {
        // S5: resolve the OS sandbox first. It confines side effects without
        // fabricating success — wrapping the command (Pass still keys on the
        // child's real exit status below), or (under `Required` with no backend)
        // refusing to run (→ Fail). It does not promise confined/unconfined
        // exit-code parity; see the `sandbox` module docs. The check runs in
        // `cwd`, which is always writable.
        //
        // H2: instead of granting the ENTIRE shared system temp (which let
        // gate-run code read/clobber sibling processes' temp artifacts inside the
        // jail), confine temp writes to a FRESH per-invocation private dir (0700,
        // RAII-cleaned). It is the SOLE extra writable root AND the child's
        // `TMPDIR`, so build tools (cargo/rustc) write their intermediates inside
        // the single grant. `private_tmp` lives until this fn returns — past the
        // child's exit/kill — then drops and removes the dir. When the sandbox is
        // Off none is minted and the child env is untouched (byte-identical).
        let private_tmp = private_check_tmpdir(&self.sandbox)?;
        let extra_writable: Vec<PathBuf> = private_tmp
            .as_ref()
            .map(|dir| vec![dir.path().to_path_buf()])
            .unwrap_or_default();
        let decision =
            sandbox::decide(&self.sandbox, &self.cwd, &self.goal.check_cmd, &extra_writable);
        // H13: capture the additive degrade signal BEFORE the match consumes the
        // decision. True ONLY for the `Auto` no-backend degrade
        // (`Unconfined { warning: Some(_) }`) — never for `Off` (intentionally
        // unconfined, `warning: None`), `Wrap` (confined), or `Refuse`. Pure
        // telemetry: it changes neither which command runs nor the verdict below.
        let ran_unconfined = decision_is_unconfined_degrade(&decision);
        let (program, args): (String, Vec<String>) =
            match decision {
                SandboxDecision::Unconfined { warning } => {
                    if let Some(message) = warning {
                        tracing::warn!(target: "nerve::sandbox", "{message}");
                    }
                    (
                        self.goal.check_cmd[0].clone(),
                        self.goal.check_cmd[1..].to_vec(),
                    )
                }
                SandboxDecision::Wrap { program, args } => {
                    // H4: `Required` promises code never runs unconfined, but
                    // fail-closed was enforced only at DECISION time (no backend
                    // -> Refuse). Once `decide` returns `Wrap`, the runtime
                    // trusted the wrap blindly: if the kernel/bwrap ran the inner
                    // command but the confinement profile was NOT actually in
                    // force (flag drift, missing userns, profile not applied),
                    // the real check would run effectively UNCONFINED while
                    // believed confined = fail-OPEN. Before trusting the wrap,
                    // run a canary that PROVES an out-of-grant write is denied.
                    // It can only turn a believed-confined run into a Fail; it
                    // never fabricates a Pass. `Required` only (Auto is
                    // best-effort by definition; no canary latency there); macOS
                    // /Linux only (the only Wrap-producing backends — elsewhere
                    // `Required` already Refuses at decision time).
                    #[cfg(any(target_os = "macos", target_os = "linux"))]
                    if self.sandbox.mode == nerve_config::SandboxMode::Required {
                        let confined = match &private_tmp {
                            Some(grant) => {
                                self.verify_confinement(&extra_writable, grant.path())
                                    .await?
                            }
                            // `Required` is always sandbox-enabled, so a private
                            // dir was minted and this is `Some`. If it somehow is
                            // not, we cannot run the self-test -> fail closed.
                            None => false,
                        };
                        if !confined {
                            return Ok(CheckOutcome {
                                result: CheckResult::Fail {
                                    reason: CONFINEMENT_SELFTEST_FAILED.to_string(),
                                    progress: None,
                                },
                                ran_unconfined,
                            });
                        }
                    }
                    (program, args)
                }
                SandboxDecision::Refuse { reason } => {
                    return Ok(CheckOutcome {
                        result: CheckResult::Fail {
                            reason,
                            progress: None,
                        },
                        ran_unconfined,
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

        // H2: point the child's `TMPDIR` at the per-check private dir LAST, so it
        // wins over any inherited/goal env. Under a confining sandbox a `TMPDIR`
        // outside the writable grant would have its writes denied; pinning it to
        // the granted private dir keeps build-tool temp writes inside the single
        // grant. Only set when a private dir was minted (sandbox enabled); Off
        // leaves the child env untouched.
        //
        // Compatibility (Linux): a tool that hardcodes `/tmp` and ignores
        // `TMPDIR` will now have those writes DENIED under a confining sandbox,
        // where the pre-H2 whole-system-temp grant (the parent's `temp_dir()`,
        // typically `/tmp` on Linux) allowed them. This is fail-safe confinement
        // — a denied write, never a fabricated pass — not a change to the accept
        // gate; tools that honor `TMPDIR` (cargo/rustc/go) write inside the grant
        // and are unaffected.
        if let Some(dir) = &private_tmp {
            command.env("TMPDIR", dir.path());
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
                return Ok(CheckOutcome {
                    result: CheckResult::Fail {
                        reason: format!("timeout after {}s", self.goal.timeout_secs),
                        progress: None,
                    },
                    ran_unconfined,
                });
            }
        };

        let status = match child.wait().await {
            Ok(status) => status,
            Err(err) => {
                return Ok(CheckOutcome {
                    result: CheckResult::Fail {
                        reason: format!("failed to await check_cmd: {err}"),
                        progress: None,
                    },
                    ran_unconfined,
                });
            }
        };

        let (stdout_res, stderr_res) = drained;
        if let Err(OutputCapExceeded(byte_cap)) = stdout_res {
            return Ok(CheckOutcome {
                result: CheckResult::Fail {
                    reason: format!("stdout exceeded {byte_cap} bytes"),
                    progress: None,
                },
                ran_unconfined,
            });
        }
        if let Err(OutputCapExceeded(byte_cap)) = stderr_res {
            return Ok(CheckOutcome {
                result: CheckResult::Fail {
                    reason: format!("stderr exceeded {byte_cap} bytes"),
                    progress: None,
                },
                ran_unconfined,
            });
        }

        let result = if status.success() {
            CheckResult::Pass
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
            CheckResult::Fail { reason, progress }
        };
        Ok(CheckOutcome {
            result,
            ran_unconfined,
        })
    }

    /// H4: prove the sandbox wrap actually confines before trusting it (`Required`
    /// only). Mint a probe dir OUTSIDE the writable grant and run a canary under
    /// the SAME policy as the real check; return whether an out-of-grant write is
    /// genuinely denied. `Ok(false)` (or a mint error via `?`) makes the caller
    /// fail closed. The probe dir is RAII-cleaned. macOS/Linux only — the only
    /// `Wrap`-producing backends.
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    async fn verify_confinement(
        &self,
        profile_writable: &[PathBuf],
        grant_dir: &std::path::Path,
    ) -> Result<bool, GoalError> {
        // The probe dir is a FRESH tempfile sibling, deliberately NOT added to
        // `profile_writable`, so the sandbox profile does not grant it. It must
        // EXIST so the only reason an in-probe write can fail is sandbox denial,
        // not a missing parent.
        //
        // Edge case (fail-safe direction): if the check's `cwd` is a broad
        // ANCESTOR of the system temp dir (e.g. `/`), the probe — minted under
        // the system temp — falls inside the always-writable `cwd` grant, so the
        // escape write succeeds and the canary reports NOT confined -> the check
        // is refused. This is a (theoretical, pathological-cwd) FALSE-NEGATIVE
        // that fails CLOSED; it never fails open. A normal check `cwd` (a repo /
        // worktree dir) is never an ancestor of the temp root.
        let probe = tempfile::Builder::new()
            .prefix("nerve-canary-")
            .tempdir()
            .map_err(GoalError::PrivateTmpDir)?;
        run_confinement_probe(
            &self.sandbox,
            &self.cwd,
            profile_writable,
            grant_dir,
            probe.path(),
            self.goal.timeout_secs,
        )
        .await
        // `probe` drops here -> the probe dir (and any escape file) is removed.
    }
}

/// H2: per-check writable-temp policy. When the sandbox confines writes, mint a
/// FRESH private temp dir (0700 via `tempfile`, RAII-cleaned on drop) to serve as
/// the SOLE extra writable root AND the child's `TMPDIR`, instead of granting the
/// entire shared system temp. When the sandbox is `Off`, `sandbox::decide`
/// returns `Unconfined` before it ever reads the extra-writable set, so we mint
/// nothing and leave the child env untouched — `Off` stays byte-identical to
/// pre-S5. Failure to create the dir is surfaced (→ `Fail`), never silently
/// downgraded to a broader grant.
fn private_check_tmpdir(sandbox: &SandboxConfig) -> Result<Option<TempDir>, GoalError> {
    if !sandbox.is_enabled() {
        return Ok(None);
    }
    let dir = tempfile::Builder::new()
        .prefix("nerve-check-")
        .tempdir()
        .map_err(GoalError::PrivateTmpDir)?;
    // `tempfile` creates directories with umask-based perms (commonly 0755), which
    // would let OTHER local users read/clobber the check's temp. Force owner-only
    // 0700 so the private grant is actually private. Fail closed on error.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700))
            .map_err(GoalError::PrivateTmpDir)?;
    }
    Ok(Some(dir))
}

/// H4 verdict (keyed on FILE PRESENCE, never an exit code — exit codes are
/// invertible, file presence is not). The wrap is confining iff the out-of-grant
/// ESCAPE write was DENIED (no file) AND the in-grant LIVE marker proves the
/// canary actually ran. `LIVE` absent => the canary never executed => ambiguous
/// => treat as NOT confined (fail closed), never as confined.
#[cfg_attr(
    not(any(target_os = "macos", target_os = "linux")),
    allow(dead_code)
)]
fn canary_confined(escape_exists: bool, live_exists: bool) -> bool {
    !escape_exists && live_exists
}

/// H4: run one confinement canary under the SAME sandbox policy the real check
/// will use, and report whether an out-of-grant write is actually DENIED. The
/// canary writes a LIVE marker INSIDE the grant (positive control: proves the
/// canary ran at all, so "no escape file" cannot be confused with "canary never
/// executed") and attempts an ESCAPE write into `probe_dir`, which is OUTSIDE the
/// granted writable set. Returns `Ok(true)` only when the escape was denied AND
/// the canary demonstrably ran; otherwise `Ok(false)` so the caller fails closed.
/// It can only ever turn a believed-confined run into a Fail — it never
/// fabricates a Pass. macOS/Linux only.
#[cfg(any(target_os = "macos", target_os = "linux"))]
async fn run_confinement_probe(
    config: &SandboxConfig,
    cwd: &std::path::Path,
    profile_writable: &[PathBuf],
    grant_dir: &std::path::Path,
    probe_dir: &std::path::Path,
    timeout_secs: u64,
) -> Result<bool, GoalError> {
    let live = grant_dir.join(".nerve-canary-live");
    let escape = probe_dir.join("nerve-canary-escape");
    // Paths are passed via env (not interpolated into the script) so a path with
    // a quote/space/newline cannot break out of the shell command. `;` not `&&`:
    // attempt the ESCAPE write even if the LIVE write is somehow blocked, so a
    // broken in-grant write can never mask an ineffective wrap.
    let script = "printf x > \"$NERVE_CANARY_LIVE\"; printf x > \"$NERVE_CANARY_ESCAPE\"";
    let canary_cmd = [
        "sh".to_string(),
        "-c".to_string(),
        script.to_string(),
    ];
    let (program, args) = match sandbox::decide(config, cwd, &canary_cmd, profile_writable) {
        SandboxDecision::Wrap { program, args } => (program, args),
        // We are only called on the real check's `Wrap` path, and `decide` keys
        // its Wrap/Refuse choice on backend availability (not the inner command),
        // so the canary also decides `Wrap`. Any other decision means we cannot
        // prove confinement -> fail closed.
        _ => return Ok(false),
    };
    let mut command = Command::new(&program);
    command
        .args(&args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .env_clear()
        .env("NERVE_CANARY_LIVE", &live)
        .env("NERVE_CANARY_ESCAPE", &escape);
    let mut child = command.spawn()?;
    // The canary does two tiny writes; cap well under the check timeout. On
    // timeout/await-error we cannot conclude confinement -> fail closed.
    let dur = Duration::from_secs(timeout_secs.clamp(1, 10));
    match timeout(dur, child.wait()).await {
        // Exit status intentionally ignored — the authoritative signal is file
        // presence below, which a no-op'd wrap cannot fake.
        Ok(Ok(_status)) => {}
        Ok(Err(_)) | Err(_) => {
            let _ = child.start_kill();
            let _ = child.wait().await;
            return Ok(false);
        }
    }
    let verdict = canary_confined(escape.exists(), live.exists());
    // Keep the grant pristine for the real check that runs next (the LIVE marker
    // lives in the same dir that becomes the child's TMPDIR). The escape file, if
    // any, lives in the probe dir which the caller RAII-removes.
    let _ = std::fs::remove_file(&live);
    Ok(verdict)
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
/// libtest (`N passed; M failed; ...`), pytest (`M failed, N passed in ...`),
/// and jest (`Tests: M failed, N passed, T total`) summaries via the shared
/// `passed`/`failed` token scan, plus `go test` (`--- PASS:` / `--- FAIL:`
/// per-test markers) which prints no aggregate summary line. Returns `None`
/// when nothing is recognized — progress is additive telemetry and a stall hint,
/// never an acceptance signal, so an unparsed check (any other ecosystem) just
/// lacks it and the loop safely falls back to identical-output stall detection.
///
/// The hint takes the most pessimistic (minimum) ratio across stdout/stderr and
/// across the summary-line and go-marker recognizers, so it always biases toward
/// MORE stall pressure (toward abort), never less. It is best-effort and NOT
/// tamper-proof: the summary recognizer uses the LAST summary line in a stream
/// (the conventional final-result line), so a lead that appends a later forged
/// `1000 passed; 0 failed` summary in the same stream — or that omits/garbles the
/// real failure so nothing recognizable remains — can inflate this hint. That can
/// only DELAY a stall-driven abort; it can never satisfy a goal, shorten review,
/// or flip a verdict, because acceptance keys SOLELY on the deterministic check's
/// exit code, never on progress. Progress is a reject-only signal end to end.
fn parse_progress(stdout: &str, stderr: &str) -> Option<u16> {
    pessimistic(parse_progress_in(stdout), parse_progress_in(stderr))
}

/// The most pessimistic available pass-ratio: the minimum when both are present
/// (so neither recognizer/stream can mask the other's failures), else whichever
/// is present. Keeps progress strictly reject-only.
fn pessimistic(a: Option<u16>, b: Option<u16>) -> Option<u16> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (only, None) | (None, only) => only,
    }
}

/// Recognizable pass-ratio (permille) in one stream: the last summary-line
/// ratio combined pessimistically with the `go test` marker ratio.
fn parse_progress_in(text: &str) -> Option<u16> {
    let summary = text.lines().filter_map(progress_from_line).next_back();
    pessimistic(summary, go_test_progress(text))
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
    ratio_permille(passed, failed)
}

/// `go test` / `gotestsum` print no aggregate `N passed` summary line, only
/// per-test `--- PASS:` / `--- FAIL:` markers (one per test, at any indent under
/// `-v`). Count them across the whole stream: `passed` = `--- PASS:` count,
/// `failed` = `--- FAIL:` count. Reject-only by construction — every `FAIL`
/// marker only lowers the ratio, so this can feed stall pressure toward abort
/// but never fabricate acceptance (the exit-code gate stays authoritative).
/// Returns `None` when no markers are present, so non-go output is unaffected.
fn go_test_progress(text: &str) -> Option<u16> {
    let mut passed = 0u64;
    let mut failed = 0u64;
    for line in text.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("--- PASS:") {
            passed += 1;
        } else if trimmed.starts_with("--- FAIL:") {
            failed += 1;
        }
    }
    ratio_permille(passed, failed)
}

/// `passed / (passed + failed)` in permille (0..=1000), or `None` when the total
/// is zero. `passed <= total`, so the result is `<= 1000` and fits in `u16`;
/// `.min` is belt-and-braces.
fn ratio_permille(passed: u64, failed: u64) -> Option<u16> {
    let total = passed.checked_add(failed)?;
    if total == 0 {
        return None;
    }
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
        assert_eq!(evaluator.evaluate().await.result, CheckResult::Pass);
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
        let result = evaluator.evaluate().await.result;
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
        let result = evaluator.evaluate().await.result;
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
        let result = evaluator.evaluate().await.result;
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
    fn parse_progress_jest_summary() {
        // jest writes `Tests: M failed, N passed, T total` (often padded). The
        // shared token scan finds 11 before `passed` and 1 before `failed`:
        // 11/12 = 916 permille.
        let out = "Tests:       1 failed, 11 passed, 12 total\n";
        assert_eq!(parse_progress(out, ""), Some(916));
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
    fn parse_progress_go_test_markers() {
        // `go test -v` prints no aggregate `N passed` summary, only per-test
        // `--- PASS:` / `--- FAIL:` markers (indented under the subtest, and the
        // package `FAIL`/`ok` footer carries no counts). 3/4 PASS = 750 permille.
        let out = "=== RUN   TestAlpha\n--- PASS: TestAlpha (0.00s)\n\
            === RUN   TestBeta\n    --- FAIL: TestBeta/case (0.00s)\n\
            === RUN   TestGamma\n--- PASS: TestGamma (0.00s)\n\
            === RUN   TestDelta\n--- PASS: TestDelta (0.00s)\n\
            FAIL\nexit status 1\nFAIL\texample/pkg\t0.012s\n";
        assert_eq!(parse_progress(out, ""), Some(750));
    }

    #[test]
    fn parse_progress_go_test_all_fail_is_zero() {
        let out = "--- FAIL: TestA (0.00s)\n--- FAIL: TestB (0.00s)\nFAIL\n";
        assert_eq!(parse_progress(out, ""), Some(0));
    }

    #[test]
    fn parse_progress_go_markers_combine_pessimistically_with_summary() {
        // Within ONE stream, a forged all-pass libtest summary must not mask a real
        // go `--- FAIL:` marker on the same stream — the pessimistic (min) ratio of
        // the two recognizers wins, so this only ever raises stall pressure.
        let out = "--- FAIL: TestReal (0.00s)\n\
            test result: ok. 10 passed; 0 failed\n";
        assert_eq!(parse_progress(out, ""), Some(0));
    }

    #[test]
    fn parse_progress_go_markers_worst_across_streams() {
        // Go markers participate in the same cross-stream pessimism as summaries.
        let stdout = "--- PASS: TestA (0.00s)\n--- PASS: TestB (0.00s)\n";
        let stderr = "===== 1 failed, 0 passed in 0.01s =====\n";
        assert_eq!(parse_progress(stdout, stderr), Some(0));
        assert_eq!(parse_progress(stderr, stdout), Some(0));
    }

    #[test]
    fn parse_progress_go_prose_without_markers_is_none() {
        // Prose mentioning PASS/FAIL without the leading `--- PASS:`/`--- FAIL:`
        // marker must not be parsed as go-test progress; output stays unrecognized.
        assert_eq!(
            parse_progress("all checks PASS and nothing did FAIL here\n", ""),
            None
        );
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
        let result = evaluator.evaluate().await.result;
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
            ..Default::default()
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
        assert_eq!(evaluator.evaluate().await.result, CheckResult::Pass);
        assert!(cwd.join("marker.txt").exists(), "in-cwd write should persist");
    }

    /// The sandbox never *fabricates* a success: a wrapped check that exits
    /// non-zero still reports `Fail`. This is the precise S5 guarantee (the gate
    /// keys `Pass` strictly on the child's real exit status; the sandbox adds no
    /// success of its own) — distinct from confined/unconfined exit-code parity,
    /// which is inherently unachievable since confinement is observable (see the
    /// `sandbox` module docs / codex S5 r4). The evaluator grants `cwd` + a fresh
    /// per-check private temp dir as writable (H2), so the nonzero exit from this
    /// non-writing failing check wraps cleanly and the `progress` path is unaffected.
    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn goal_evaluator_sandbox_required_preserves_nonzero_fail() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = std::fs::canonicalize(dir.path()).unwrap();
        let sandbox = SandboxConfig {
            mode: nerve_config::SandboxMode::Required,
            allow_network: false,
            ..Default::default()
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
            matches!(evaluator.evaluate().await.result, CheckResult::Fail { .. }),
            "a wrapped failing check must still Fail"
        );
    }

    // --- H2: per-check private temp dir ---

    /// The per-check temp policy mints NOTHING when the sandbox is `Off` (so the
    /// child env stays byte-identical to pre-S5), and a fresh 0700 dir when
    /// confinement is requested — removed by RAII on drop.
    #[test]
    fn private_check_tmpdir_minted_only_when_sandbox_enabled() {
        use nerve_config::SandboxMode;
        let off = SandboxConfig {
            mode: SandboxMode::Off,
            allow_network: false,
            ..Default::default()
        };
        assert!(
            private_check_tmpdir(&off).unwrap().is_none(),
            "Off must mint no private temp dir"
        );
        for mode in [SandboxMode::Auto, SandboxMode::Required] {
            let cfg = SandboxConfig {
                mode,
                allow_network: false,
                ..Default::default()
            };
            let dir = private_check_tmpdir(&cfg)
                .unwrap()
                .expect("an enabled sandbox must mint a private temp dir");
            assert!(dir.path().is_dir(), "minted dir must exist");
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let bits = std::fs::metadata(dir.path()).unwrap().permissions().mode() & 0o777;
                assert_eq!(bits, 0o700, "private temp dir must be 0700, got {bits:o}");
            }
            let path = dir.path().to_path_buf();
            drop(dir);
            assert!(!path.exists(), "RAII drop must remove the private temp dir");
        }
    }

    /// H2 end-to-end (macOS real kernel): under a confining sandbox the per-check
    /// private dir is the child's `TMPDIR` and the SOLE temp grant. A write to
    /// `$TMPDIR` succeeds; a DIRECT write to a sibling dir outside the grant (what
    /// the old whole-system-temp grant would have allowed) is denied by the
    /// kernel and leaves no file — proving the writable surface shrank.
    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn goal_evaluator_sandbox_confines_temp_to_private_dir() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = std::fs::canonicalize(dir.path()).unwrap();
        let sandbox = SandboxConfig {
            mode: nerve_config::SandboxMode::Required,
            allow_network: false,
            ..Default::default()
        };
        // Inside $TMPDIR (the granted private dir): allowed.
        let ok = GoalEvaluator::with_options(
            spec_with_cmd(
                vec![
                    "sh",
                    "-c",
                    "echo hi > \"$TMPDIR/probe\" && test -f \"$TMPDIR/probe\"",
                ],
                30,
            ),
            Vec::new(),
            4096,
            cwd.clone(),
            None,
            sandbox,
        )
        .unwrap();
        let ok_outcome = ok.evaluate().await;
        assert_eq!(
            ok_outcome.result,
            CheckResult::Pass,
            "a write inside $TMPDIR must pass under confinement"
        );
        // H13: a real backend confined the run, so the additive telemetry stays
        // false — `ran_unconfined` is reserved for the `Auto` no-backend degrade.
        assert!(
            !ok_outcome.ran_unconfined,
            "a confined Required run must never report ran_unconfined"
        );
        // A sibling temp dir, outside cwd and outside the private grant: denied.
        let sibling = tempfile::tempdir().unwrap();
        let sibling = std::fs::canonicalize(sibling.path()).unwrap();
        let escape = sibling.join("escape.txt");
        let denied = GoalEvaluator::with_options(
            spec_with_cmd(
                vec!["sh", "-c", &format!("echo hi > {}", escape.display())],
                30,
            ),
            Vec::new(),
            4096,
            cwd,
            None,
            sandbox,
        )
        .unwrap();
        assert!(
            matches!(denied.evaluate().await.result, CheckResult::Fail { .. }),
            "a direct write outside the private temp grant must be denied"
        );
        assert!(!escape.exists(), "denied write must leave no file");
    }

    // --- H4: Required runtime confinement self-test (canary) ---

    /// The canary verdict keys on FILE PRESENCE only (exit codes are invertible):
    /// confined iff the out-of-grant escape write was denied (absent) AND the
    /// in-grant live marker proves the canary actually ran. A missing live marker
    /// (canary never executed) is NEVER reported as confined — fail closed.
    #[test]
    fn canary_confined_truth_table() {
        assert!(
            canary_confined(false, true),
            "escape denied + canary ran => confined"
        );
        assert!(
            !canary_confined(true, true),
            "escape SUCCEEDED => not confined (fail closed)"
        );
        assert!(
            !canary_confined(false, false),
            "canary never ran (no live marker) => not confined (fail closed)"
        );
        assert!(
            !canary_confined(true, false),
            "escape succeeded + no live => not confined"
        );
    }

    /// macOS real kernel: under an EFFECTIVE wrap the canary's out-of-grant write
    /// is denied, so `run_confinement_probe` reports confined and leaves no escape
    /// file. The profile grants only `cwd` + `grant` (the probe dir is excluded).
    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn confinement_probe_reports_confined_when_escape_denied() {
        let cwd_t = tempfile::tempdir().unwrap();
        let grant_t = tempfile::tempdir().unwrap();
        let probe_t = tempfile::tempdir().unwrap();
        let cwd = std::fs::canonicalize(cwd_t.path()).unwrap();
        let grant = std::fs::canonicalize(grant_t.path()).unwrap();
        let probe = std::fs::canonicalize(probe_t.path()).unwrap();
        let sandbox = SandboxConfig {
            mode: nerve_config::SandboxMode::Required,
            allow_network: false,
            ..Default::default()
        };
        let confined =
            run_confinement_probe(&sandbox, &cwd, std::slice::from_ref(&grant), &grant, &probe, 10)
                .await
                .unwrap();
        assert!(confined, "an out-of-grant write must be denied -> confined");
        assert!(
            !probe.join("nerve-canary-escape").exists(),
            "the denied escape write must leave no file"
        );
    }

    /// macOS real kernel, FAIL-CLOSED: simulate an INEFFECTIVE wrap by letting the
    /// canary's own profile ALSO grant the probe dir, so the out-of-grant escape
    /// write SUCCEEDS (as it would under a silently no-op'd wrap).
    /// `run_confinement_probe` must then report NOT confined — proving that when a
    /// wrap fails to block an out-of-root write, the canary catches it.
    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn confinement_probe_reports_unconfined_when_escape_succeeds() {
        let cwd_t = tempfile::tempdir().unwrap();
        let grant_t = tempfile::tempdir().unwrap();
        let probe_t = tempfile::tempdir().unwrap();
        let cwd = std::fs::canonicalize(cwd_t.path()).unwrap();
        let grant = std::fs::canonicalize(grant_t.path()).unwrap();
        let probe = std::fs::canonicalize(probe_t.path()).unwrap();
        let sandbox = SandboxConfig {
            mode: nerve_config::SandboxMode::Required,
            allow_network: false,
            ..Default::default()
        };
        let confined = run_confinement_probe(
            &sandbox,
            &cwd,
            &[grant.clone(), probe.clone()],
            &grant,
            &probe,
            10,
        )
        .await
        .unwrap();
        assert!(
            !confined,
            "when an out-of-grant write SUCCEEDS, the canary must report NOT confined (fail closed)"
        );
    }

    /// H4 end-to-end (macOS real kernel): a healthy `Required` run is NOT broken
    /// by the canary — the self-test finds confinement effective and the real
    /// check proceeds to Pass on its own exit status.
    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn goal_evaluator_required_canary_allows_healthy_run() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = std::fs::canonicalize(dir.path()).unwrap();
        let sandbox = SandboxConfig {
            mode: nerve_config::SandboxMode::Required,
            allow_network: false,
            ..Default::default()
        };
        let evaluator = GoalEvaluator::with_options(
            spec_with_cmd(vec!["true"], 30),
            Vec::new(),
            4096,
            cwd,
            None,
            sandbox,
        )
        .unwrap();
        assert_eq!(
            evaluator.evaluate().await.result,
            CheckResult::Pass,
            "the canary must not break a healthy Required run"
        );
    }

    /// H13: the glue predicate maps the sandbox decision to the additive
    /// `ran_unconfined` telemetry. Host-independent (constructs decisions directly)
    /// so the degrade case is proven even where a backend is always available and
    /// `decide` therefore never returns it. Combined with the sandbox-layer test
    /// `no_backend_auto_runs_unconfined_with_warning` (Auto+no-backend yields
    /// exactly `Unconfined { warning: Some(_) }`), this closes the chain end-to-end.
    #[test]
    fn unconfined_degrade_predicate_only_fires_on_auto_no_backend() {
        // The ONE security-relevant case: a confinement was requested (`Auto`) but
        // no backend ran it — the check executed unconfined.
        assert!(decision_is_unconfined_degrade(
            &SandboxDecision::Unconfined {
                warning: Some("running the check UNCONFINED".to_string()),
            }
        ));
        // `Off` is intentionally unconfined (no warning): NOT a degrade.
        assert!(!decision_is_unconfined_degrade(
            &SandboxDecision::Unconfined { warning: None }
        ));
        // A confined wrap and a fail-closed refusal never "ran unconfined".
        assert!(!decision_is_unconfined_degrade(&SandboxDecision::Wrap {
            program: "/usr/bin/sandbox-exec".to_string(),
            args: Vec::new(),
        }));
        assert!(!decision_is_unconfined_degrade(&SandboxDecision::Refuse {
            reason: "no backend".to_string(),
        }));
    }

    /// H13 end-to-end (host-independent): `Off` runs the check unconfined BY
    /// DESIGN, which is NOT a degrade — so the additive telemetry must stay false.
    /// Only the `Auto` no-backend fallback sets it. Proves the signal is plumbed
    /// onto the real run outcome, not just the predicate.
    #[tokio::test]
    async fn evaluate_off_mode_is_not_reported_as_unconfined_degrade() {
        let dir = tempfile::tempdir().unwrap();
        let evaluator = GoalEvaluator::with_options(
            spec_with_cmd(vec!["true"], 5),
            Vec::new(),
            1024,
            dir.path().to_path_buf(),
            None,
            SandboxConfig {
                mode: nerve_config::SandboxMode::Off,
                ..Default::default()
            },
        )
        .unwrap();
        let outcome = evaluator.evaluate().await;
        assert_eq!(outcome.result, CheckResult::Pass);
        assert!(
            !outcome.ran_unconfined,
            "sandbox.mode=off is intentionally unconfined, not a degrade"
        );
    }
}
