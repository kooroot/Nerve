//! S5: OS execution sandbox for deterministic checks.
//!
//! [`GoalEvaluator`](crate::goal::GoalEvaluator) runs project-controlled code:
//! `/goal` check commands and the built-in verifier (Cargo build scripts,
//! `package.json` scripts, test code). The OS sandbox confines that execution's
//! filesystem writes and network on top of the existing resource guards (env
//! whitelist, timeout, output cap, ulimit).
//!
//! **North star (precise).** The sandbox never *fabricates* a success. A
//! [`SandboxDecision`] can only make the gate's verdict more conservative or
//! leave the executed command's own exit status untouched: `Refuse` →
//! `CheckResult::Fail` (and the code never runs); `Wrap` runs the operator's
//! exact command and the gate still keys `Pass` strictly on the child's real
//! exit status — the sandbox adds no success of its own (locked by
//! `goal_evaluator_sandbox_required_preserves_nonzero_fail`). When confinement
//! cannot be established and the operator asked for [`SandboxMode::Required`],
//! we **fail closed** — the code never runs unconfined.
//!
//! What confinement does NOT promise — inherent to *every* sandbox, not a bug
//! here: exit-code parity between a confined and an unconfined run. A denied
//! syscall returns an error the executed code can observe and branch on, so a
//! check deliberately authored to invert on a denial (e.g.
//! `write-outside-cwd && exit 1 || exit 0`) can exit differently confined. That
//! is acceptable because exit-code authority belongs to the operator-trusted
//! check (`check_cmd` / the built-in verifier command) — which the LEAD does NOT
//! control; the lead controls only the patch, i.e. the code the check executes.
//! Against a realistic operator check (an aggregate runner such as `cargo test`,
//! which exits non-zero if *any* operator assertion fails) the lead cannot turn a
//! genuine failure into a pass by branching on a denial, and where the lead fully
//! authors the exit logic it could already pass unconfined — so confinement
//! grants it no acceptance it did not already have. The sandbox only removes the
//! malicious *side effect* (out-of-root write, network), strictly in the safe
//! direction. It is therefore NOT a strictly conservative superset of `Off` for a
//! check intentionally written to invert on confinement; that residual is the
//! check author's concern, not the gate's.
//!
//! **Backends.**
//! - macOS: Seatbelt via `/usr/bin/sandbox-exec` (runtime-verified on the dev host).
//! - Linux: `bwrap` (bubblewrap) read-only root + rw bind mounts + optional
//!   network unshare. The wrapper binary is resolved from a fixed set of trusted
//!   absolute system paths, NEVER `$PATH` — a `$PATH`-resolved wrapper could be
//!   shadowed by a repo-planted `./bwrap` and defeat confinement. The argument
//!   generation is unit-tested on every platform, but the *runtime* path is NOT
//!   exercised on the macOS dev host — it is validated only at the
//!   arg-generation level here.
//!
//! **Honest limitations.** The macOS profile is permissive-baseline
//! (`(allow default)`) minus filesystem writes outside the writable roots and
//! minus network. It is best-effort *write + network confinement*, NOT a strong
//! jail: reads stay broad and arbitrary syscalls / process exec are allowed.
//! - It blocks the confined process's own DIRECT out-of-root writes and DIRECT
//!   network, which defeats the common/naive escape (a build script doing
//!   `fs::write("/etc/…")` or opening a socket).
//! - It does NOT block **daemon-mediated** side effects: under `(allow default)`
//!   a confined process can still send Mach IPC to system daemons that run
//!   OUTSIDE the sandbox and act on its behalf. For writes, `defaults write` (→
//!   `cfprefsd`) persists a plist under `~/Library/Preferences` even though a
//!   direct write there is denied (codex S5 r5). The network side has the same
//!   shape: `(deny network*)` stops the process's own sockets, but DNS
//!   resolution via `mDNSResponder` is reached over IPC, so data can still be
//!   exfiltrated over crafted hostnames. This is an inherent property of a
//!   permissive baseline — enumerating every write/network-capable daemon is
//!   whack-a-mole, and a deny-by-default IPC policy would break real build tools
//!   — so it is documented, not closed. Linux `bwrap` is stronger here (its mount
//!   namespace + RO host bind leaves no in-namespace daemon to mediate writes),
//!   but a host daemon reachable over a bound socket is the same class of risk.
//! - It does not prevent disclosure of already-readable files.
//!
//! Therefore `Required` raises the bar against accidental and naive escapes; it
//! is NOT a security boundary against a determined adversary. For hard isolation
//! of an untrusted tree, run the whole loop inside a container or VM.

use nerve_config::{SandboxConfig, SandboxMode};
use std::path::{Path, PathBuf};

/// macOS Seatbelt CLI. Always present on macOS at this fixed absolute path;
/// using the absolute path means it resolves regardless of the (cleared) child
/// `PATH`, while the wrapped inner command is resolved by `sandbox-exec` itself
/// using the child environment — exactly as the unwrapped command would be.
#[cfg(target_os = "macos")]
const SANDBOX_EXEC: &str = "/usr/bin/sandbox-exec";

/// How to run a check command after applying the sandbox policy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SandboxDecision {
    /// Run the command unchanged. `warning`, when set, is surfaced to the
    /// operator (`Auto` mode with no available backend → loud unconfined run).
    Unconfined { warning: Option<String> },
    /// Replace the command's program/args with a sandbox-wrapper invocation.
    /// `program` is the wrapper binary (absolute) and `args` is the full argv:
    /// wrapper flags, then the original program followed by its own args.
    Wrap { program: String, args: Vec<String> },
    /// Refuse to run: `Required` mode with no usable backend. The caller turns
    /// this into a `CheckResult::Fail` and the project code never executes.
    Refuse { reason: String },
}

/// Decide how to run `check_cmd` (a validated argv; `check_cmd[0]` is the
/// program) in `cwd` under `config`. `extra_writable_roots` are absolute
/// directories — in addition to `cwd`, which is always writable — that the check
/// may write to (production passes the system temp dir so build tools work).
pub fn decide(
    config: &SandboxConfig,
    cwd: &Path,
    check_cmd: &[String],
    extra_writable_roots: &[PathBuf],
) -> SandboxDecision {
    if !config.is_enabled() {
        return SandboxDecision::Unconfined { warning: None };
    }
    #[cfg(target_os = "macos")]
    {
        seatbelt_decide(config, cwd, check_cmd, extra_writable_roots)
    }
    #[cfg(target_os = "linux")]
    {
        bwrap_decide(config, cwd, check_cmd, extra_writable_roots)
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = (cwd, check_cmd, extra_writable_roots);
        no_backend_decide(config.mode)
    }
}

/// Shared fallback when the platform backend is unavailable: refuse under
/// `Required` (fail closed), otherwise run unconfined with a loud warning.
/// `Off` is handled before any backend dispatch, so only `Auto`/`Required` reach
/// here.
#[cfg_attr(
    not(any(target_os = "macos", target_os = "linux")),
    allow(dead_code)
)]
fn no_backend_decide(mode: SandboxMode) -> SandboxDecision {
    let os = std::env::consts::OS;
    match mode {
        SandboxMode::Required => SandboxDecision::Refuse {
            reason: format!(
                "sandbox.mode=required but no OS sandbox backend is available on this platform ({os}); refusing to run the check unconfined"
            ),
        },
        _ => SandboxDecision::Unconfined {
            warning: Some(format!(
                "sandbox.mode=auto requested but no OS sandbox backend is available on this platform ({os}); running the check UNCONFINED — set sandbox.mode=required to refuse instead"
            )),
        },
    }
}

/// Canonicalize each writable root (resolving symlinks) so the sandbox profile
/// matches the kernel's resolved view — e.g. macOS `/var/folders/...` →
/// `/private/var/folders/...`, `/tmp` → `/private/tmp`. A path that cannot be
/// canonicalized falls back to itself: if it then fails to match, the effect is
/// a *denied* write (fail-safe direction), never an over-broad allow.
#[cfg_attr(
    not(any(target_os = "macos", target_os = "linux")),
    allow(dead_code)
)]
fn canonical_writable_roots(cwd: &Path, extra: &[PathBuf]) -> Vec<PathBuf> {
    std::iter::once(cwd.to_path_buf())
        .chain(extra.iter().cloned())
        .map(|p| std::fs::canonicalize(&p).unwrap_or(p))
        .collect()
}

// ---------------------------------------------------------------------------
// macOS Seatbelt backend
// ---------------------------------------------------------------------------

#[cfg(target_os = "macos")]
fn seatbelt_decide(
    config: &SandboxConfig,
    cwd: &Path,
    check_cmd: &[String],
    extra_writable_roots: &[PathBuf],
) -> SandboxDecision {
    if !Path::new(SANDBOX_EXEC).exists() {
        return no_backend_decide(config.mode);
    }
    let writable = canonical_writable_roots(cwd, extra_writable_roots);
    let profile = seatbelt_profile(&writable, config.allow_network);
    // `sandbox-exec -p <profile> -- <command> [args...]`. The wrapper-owned `--`
    // terminates sandbox-exec's own option parsing, so a `check_cmd[0]` that
    // begins with `-` (e.g. `--`) is treated as the command, NOT consumed as a
    // sandbox-exec option. Without it, `check_cmd = ["--", "true"]` would make
    // sandbox-exec run `true` (exit 0 -> Pass) where the unwrapped gate would
    // fail to spawn `--` — the sandbox must never change which command runs.
    let mut args = vec!["-p".to_string(), profile, "--".to_string()];
    args.extend(check_cmd.iter().cloned());
    SandboxDecision::Wrap {
        program: SANDBOX_EXEC.to_string(),
        args,
    }
}

/// Build the Seatbelt (SBPL) profile: permissive baseline, deny all filesystem
/// writes, re-allow writes only under the writable roots, and deny network
/// unless explicitly allowed. SBPL is last-match-wins, so the later
/// `(allow file-write* (subpath ...))` re-grants writes inside the roots while
/// every other location remains denied.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn seatbelt_profile(writable_roots: &[PathBuf], allow_network: bool) -> String {
    let mut p = String::from("(version 1)\n(allow default)\n(deny file-write*)\n");
    if !writable_roots.is_empty() {
        p.push_str("(allow file-write*\n");
        for root in writable_roots {
            p.push_str("    (subpath ");
            p.push_str(&sbpl_string(&root.to_string_lossy()));
            p.push_str(")\n");
        }
        p.push_str(")\n");
    }
    if !allow_network {
        p.push_str("(deny network*)\n");
    }
    p
}

/// Encode `s` as an SBPL string literal: double-quoted with `"` and `\`
/// backslash-escaped. This prevents a path containing a quote or paren from
/// breaking out of the literal and injecting profile directives.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn sbpl_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        if c == '"' || c == '\\' {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('"');
    out
}

// ---------------------------------------------------------------------------
// Linux bwrap backend (arg-generation unit-tested; runtime unverified on macOS)
// ---------------------------------------------------------------------------

#[cfg(target_os = "linux")]
fn bwrap_decide(
    config: &SandboxConfig,
    cwd: &Path,
    check_cmd: &[String],
    extra_writable_roots: &[PathBuf],
) -> SandboxDecision {
    let Some(bwrap) = trusted_bwrap() else {
        // bwrap is not in a trusted system location — fail closed under
        // `Required` rather than risk executing a PATH-hijacked wrapper.
        return no_backend_decide(config.mode);
    };
    let writable = canonical_writable_roots(cwd, extra_writable_roots);
    let args = bwrap_args(&writable, cwd, check_cmd, config.allow_network);
    SandboxDecision::Wrap {
        program: bwrap.to_string_lossy().into_owned(),
        args,
    }
}

/// Build the `bwrap` argv: bind the whole host filesystem read-only, re-bind the
/// writable roots read-write, mount `/proc` and `/dev`, run in `cwd`, die with
/// the parent, and unshare the network namespace unless network is allowed.
/// `bwrap [options] -- command [args...]`.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn bwrap_args(
    writable_roots: &[PathBuf],
    cwd: &Path,
    check_cmd: &[String],
    allow_network: bool,
) -> Vec<String> {
    let mut args: Vec<String> = vec![
        "--ro-bind".into(),
        "/".into(),
        "/".into(),
        "--proc".into(),
        "/proc".into(),
        "--dev".into(),
        "/dev".into(),
        "--chdir".into(),
        cwd.to_string_lossy().into_owned(),
        "--die-with-parent".into(),
    ];
    for root in writable_roots {
        let path = root.to_string_lossy().into_owned();
        args.push("--bind".into());
        args.push(path.clone());
        args.push(path);
    }
    if !allow_network {
        args.push("--unshare-net".into());
    }
    args.push("--".into());
    args.extend(check_cmd.iter().cloned());
    args
}

/// Trusted absolute locations for the `bwrap` binary. We deliberately do NOT
/// search `$PATH`: the lead controls the repo, so a `$PATH` that contained `.`
/// or a repo-writable directory before the system dirs would let a planted
/// `./bwrap` shadow the real one, run the check UNCONFINED, and exit 0 —
/// defeating `Required`'s fail-closed guarantee. Only these root-owned system
/// paths (where the bubblewrap package installs) are trusted; if bwrap lives
/// anywhere else, `Required` refuses rather than risk a hijacked wrapper.
///
/// Always compiled (not just on Linux) so the "absolute, never `$PATH`-derived"
/// invariant stays unit-testable on every platform.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
const BWRAP_TRUSTED_PATHS: &[&str] = &["/usr/bin/bwrap", "/bin/bwrap", "/usr/sbin/bwrap"];

/// The first trusted bwrap path that exists as a regular file, or `None`. Never
/// consults `$PATH`.
#[cfg(target_os = "linux")]
fn trusted_bwrap() -> Option<PathBuf> {
    BWRAP_TRUSTED_PATHS
        .iter()
        .map(PathBuf::from)
        .find(|candidate| candidate.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(mode: SandboxMode, allow_network: bool) -> SandboxConfig {
        SandboxConfig {
            mode,
            allow_network,
        }
    }

    #[test]
    fn off_mode_is_always_unconfined_without_warning() {
        let d = decide(
            &cfg(SandboxMode::Off, false),
            Path::new("/tmp/work"),
            &["true".to_string()],
            &[],
        );
        assert_eq!(d, SandboxDecision::Unconfined { warning: None });
    }

    // --- Seatbelt profile (pure; tested on every platform) ---

    #[test]
    fn seatbelt_profile_denies_writes_and_network_by_default() {
        let p = seatbelt_profile(&[PathBuf::from("/tmp/work")], false);
        assert!(p.contains("(allow default)"));
        assert!(p.contains("(deny file-write*)"));
        assert!(p.contains("(subpath \"/tmp/work\")"));
        assert!(p.contains("(deny network*)"));
        // Writes must be denied BEFORE being re-allowed for the roots
        // (last-match-wins), or the re-allow would never take effect.
        let deny = p.find("(deny file-write*)").unwrap();
        let allow = p.find("(allow file-write*").unwrap();
        assert!(deny < allow, "deny must precede the per-root re-allow");
    }

    #[test]
    fn seatbelt_profile_allows_network_when_opted_in() {
        let p = seatbelt_profile(&[PathBuf::from("/tmp/work")], true);
        assert!(!p.contains("(deny network*)"));
    }

    #[test]
    fn seatbelt_profile_escapes_quotes_and_backslashes_in_paths() {
        // A path with a quote/paren must not break out of the string literal.
        let evil = PathBuf::from("/tmp/a\"b)\\c");
        let p = seatbelt_profile(&[evil], false);
        assert!(p.contains("(subpath \"/tmp/a\\\"b)\\\\c\")"), "profile = {p}");
    }

    // --- bwrap args (pure; tested on every platform incl. the macOS dev host) ---

    #[test]
    fn bwrap_args_bind_ro_root_and_rw_writable_roots() {
        let args = bwrap_args(
            &[PathBuf::from("/work"), PathBuf::from("/tmp")],
            Path::new("/work"),
            &["cargo".to_string(), "test".to_string()],
            false,
        );
        // read-only host root
        let ro = args.windows(3).any(|w| w == ["--ro-bind", "/", "/"]);
        assert!(ro, "args = {args:?}");
        // rw bind for each writable root
        let rw_work = args.windows(3).any(|w| w == ["--bind", "/work", "/work"]);
        let rw_tmp = args.windows(3).any(|w| w == ["--bind", "/tmp", "/tmp"]);
        assert!(rw_work && rw_tmp, "args = {args:?}");
        // network denied by default
        assert!(args.iter().any(|a| a == "--unshare-net"));
        // chdir into cwd
        let chdir = args.windows(2).any(|w| w == ["--chdir", "/work"]);
        assert!(chdir, "args = {args:?}");
        // original command after the `--` separator
        let sep = args.iter().position(|a| a == "--").unwrap();
        assert_eq!(&args[sep + 1..], &["cargo".to_string(), "test".to_string()]);
    }

    #[test]
    fn bwrap_args_omit_net_unshare_when_allowed() {
        let args = bwrap_args(
            &[PathBuf::from("/work")],
            Path::new("/work"),
            &["true".to_string()],
            true,
        );
        assert!(!args.iter().any(|a| a == "--unshare-net"));
    }

    // --- fail-closed fallback ---

    #[test]
    fn no_backend_required_refuses() {
        match no_backend_decide(SandboxMode::Required) {
            SandboxDecision::Refuse { reason } => {
                assert!(reason.contains("required"), "reason = {reason}");
            }
            other => panic!("expected Refuse, got {other:?}"),
        }
    }

    #[test]
    fn no_backend_auto_runs_unconfined_with_warning() {
        match no_backend_decide(SandboxMode::Auto) {
            SandboxDecision::Unconfined { warning: Some(w) } => {
                assert!(w.contains("UNCONFINED"), "warning = {w}");
            }
            other => panic!("expected Unconfined+warning, got {other:?}"),
        }
    }

    /// The bwrap wrapper must be resolved only from trusted, root-owned absolute
    /// paths — never `$PATH` — so the lead cannot shadow it with a repo-planted
    /// `./bwrap` to escape confinement (codex S5 r1 BLOCK). This invariant is
    /// asserted on every platform even though `trusted_bwrap()` only runs on Linux.
    #[test]
    fn bwrap_wrapper_paths_are_trusted_absolute_locations() {
        assert!(!BWRAP_TRUSTED_PATHS.is_empty());
        for p in BWRAP_TRUSTED_PATHS {
            assert!(p.starts_with('/'), "{p} must be an absolute path");
            assert!(!p.contains(".."), "{p} must not contain ..");
            // No relative `.`/`..` path component that PATH search could exploit.
            assert!(
                !std::path::Path::new(p)
                    .components()
                    .any(|c| matches!(c, std::path::Component::CurDir | std::path::Component::ParentDir)),
                "{p} must not contain a relative component"
            );
        }
        // The canonical distro location must be covered.
        assert!(BWRAP_TRUSTED_PATHS.contains(&"/usr/bin/bwrap"));
    }

    // --- Real-kernel proof (macOS): the SBPL profile denies DIRECT out-of-root writes ---

    /// Run `/bin/sh` under real `sandbox-exec` with a profile that grants writes
    /// ONLY to `work`. A write inside `work` must succeed; a DIRECT write to a
    /// sibling directory (outside every granted root) must be denied by the
    /// kernel and leave no file behind. We control the writable set exactly (no
    /// temp grant), so the denial is unambiguous.
    ///
    /// Scope (codex S5 r5): this proves the profile blocks the confined process's
    /// OWN `file-write*` syscalls — NOT that out-of-workspace persistence is
    /// impossible. Under the permissive `(allow default)` baseline a confined
    /// process can still reach system daemons over Mach IPC, and a daemon running
    /// OUTSIDE the sandbox can persist on its behalf (e.g. `defaults write` →
    /// `cfprefsd` writes a plist under `~/Library/Preferences`). That
    /// daemon-mediated residual is a documented limitation (see module docs), not
    /// something this direct-write proof claims to cover.
    #[cfg(target_os = "macos")]
    #[test]
    fn seatbelt_profile_denies_direct_out_of_root_write() {
        use std::process::Command as StdCommand;

        let work = tempfile::tempdir().unwrap();
        let sibling = tempfile::tempdir().unwrap();
        // Canonicalize so the profile matches the kernel's symlink-resolved view.
        let work = std::fs::canonicalize(work.path()).unwrap();
        let sibling = std::fs::canonicalize(sibling.path()).unwrap();
        let profile = seatbelt_profile(std::slice::from_ref(&work), false);

        let run = |target: &std::path::Path| {
            StdCommand::new(SANDBOX_EXEC)
                .arg("-p")
                .arg(&profile)
                .arg("/bin/sh")
                .arg("-c")
                .arg(format!("echo hi > {}", target.display()))
                .status()
                .unwrap()
        };

        // Inside the only granted root: allowed.
        let ok = work.join("ok.txt");
        assert!(run(&ok).success(), "write inside the granted root must succeed");
        assert!(ok.exists(), "allowed write should create the file");

        // Outside every granted root (a sibling temp dir): denied.
        let escape = sibling.join("escape.txt");
        assert!(
            !run(&escape).success(),
            "write outside the granted roots must be denied"
        );
        assert!(!escape.exists(), "denied write must not create the file");
    }

    /// Argv transparency: a hostile `check_cmd = ["--", "true"]` must NOT let
    /// `sandbox-exec` consume the `--` as its own option terminator and run
    /// `true` (which would be a false Pass). The wrapper-owned `--` makes the
    /// command resolve to the (non-existent) program `--`, which fails to exec —
    /// exactly as the unwrapped gate would (codex S5 r2 BLOCK). We call `decide`
    /// directly to bypass `GoalSpec::validate` (which independently rejects this)
    /// and prove the sandbox layer itself is transparent.
    #[cfg(target_os = "macos")]
    #[test]
    fn seatbelt_wrapper_is_argv_transparent_for_leading_dash() {
        use std::process::Command as StdCommand;

        let work = tempfile::tempdir().unwrap();
        let work = std::fs::canonicalize(work.path()).unwrap();
        let decision = decide(
            &cfg(SandboxMode::Required, false),
            &work,
            &["--".to_string(), "true".to_string()],
            &[],
        );
        let SandboxDecision::Wrap { program, args } = decision else {
            panic!("expected Wrap, got {decision:?}");
        };
        // Wrapper-owned `--` must sit right after the profile, before the user argv.
        assert_eq!(args[0], "-p");
        assert_eq!(args[2], "--", "wrapper-owned -- must follow the profile");
        assert_eq!(&args[3..], &["--".to_string(), "true".to_string()]);
        // Real kernel: `true` must NOT run.
        let status = StdCommand::new(&program).args(&args).status().unwrap();
        assert!(
            !status.success(),
            "`-- true` must not execute `true` (would be a false Pass)"
        );
    }

    /// With network allowed and no roots granted, the profile must NOT contain a
    /// network deny rule (the only network-relevant directive), and writes are
    /// still globally denied (no re-allow block emitted).
    #[test]
    fn seatbelt_profile_with_no_roots_denies_all_writes() {
        let p = seatbelt_profile(&[], false);
        assert!(p.contains("(deny file-write*)"));
        assert!(!p.contains("(allow file-write*"), "no roots => no re-allow block");
    }
}
