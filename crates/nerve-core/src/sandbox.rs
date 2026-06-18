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
//! **H3 strict mode (opt-in, `SandboxConfig::strict`, macOS).** When enabled it
//! appends `(deny mach-lookup (global-name …))` for the two daemons named above —
//! `cfprefsd` (closing the `defaults write` persistence bypass) and, when
//! `allow_network = false`, `mDNSResponder` (closing the DNS-over-IPC exfil
//! bypass). This raises the bar on exactly those two KNOWN channels; it is NOT a
//! completeness claim — every other daemon reachable under `(allow default)`
//! (`launchd`, `distnoted`, `diagnosticd`, …) is still reachable, and a deny-all
//! IPC policy would break real build tools. strict is additive and inert when
//! off (the profile is byte-identical). It does not change the verdict below.
//!
//! **H8 strict read-scoping (opt-in, macOS).** strict ALSO denies `file-read*`
//! under the operator's well-known credential stores (`~/.ssh`, `~/.aws`,
//! `~/Library/Keychains`) so gate-run code cannot harvest those secrets before
//! exfil. This is best-effort, NOT a read jail: it is applied only when `$HOME`
//! resolves (absent `$HOME` ⇒ no read-scoping), every other path stays readable,
//! and a determined process can still reach secrets the operator made readable
//! elsewhere. Like the mach-lookup denies it is additive and inert when off
//! (byte-identical profile) and monotone (only adds `(deny …)`).
//!
//! **H8 enforcement canary.** Seatbelt silently DROPS denied operations, so an
//! OS change that broke enforcement would weaken `Required` INVISIBLY.
//! [`seatbelt_enforcement_canary`] (surfaced by `nv doctor`) is the only signal
//! that this happened: it proves the kernel still denies a known out-of-root
//! write. macOS confinement is permanently weaker than Linux (no syscall filter;
//! Seatbelt drops denied ops silently); App Sandbox / Endpoint Security are
//! explicitly out of scope.
//!
//! Therefore `Required` raises the bar against accidental and naive escapes; it
//! is NOT a security boundary against a determined adversary, with or without
//! strict mode. For hard isolation of an untrusted tree, run the whole loop
//! inside a container or VM.

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
///
/// H5: `confine_helper`, when `Some`, is the trusted absolute path to the running
/// Nerve binary, used ONLY on Linux when `config.landlock` is set — it is spliced
/// into the bwrap argv as the in-jail Landlock helper (`bwrap … -- <nv>
/// __nv-confine … -- <check>`). It is ignored on macOS and when Landlock is off.
pub fn decide(
    config: &SandboxConfig,
    cwd: &Path,
    check_cmd: &[String],
    extra_writable_roots: &[PathBuf],
    confine_helper: Option<&Path>,
) -> SandboxDecision {
    if !config.is_enabled() {
        return SandboxDecision::Unconfined { warning: None };
    }
    #[cfg(target_os = "macos")]
    {
        // Landlock is Linux-only; the macOS Seatbelt backend ignores the helper.
        let _ = confine_helper;
        seatbelt_decide(config, cwd, check_cmd, extra_writable_roots)
    }
    #[cfg(target_os = "linux")]
    {
        bwrap_decide(config, cwd, check_cmd, extra_writable_roots, confine_helper)
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = (cwd, check_cmd, extra_writable_roots, confine_helper);
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
    // H8: under strict, scope reads away from credential stores. Computed from
    // `$HOME` here (kept out of the pure profile builder) and canonicalized so
    // the deny subpaths match the kernel's symlink-resolved view, exactly like
    // the writable roots. A path that cannot be canonicalized (e.g. it does not
    // exist yet) falls back to itself — denying a not-yet-present path is
    // harmless and future-proof. Empty when off or when `$HOME` is unresolved,
    // which keeps the profile byte-identical to the non-strict build.
    let sensitive = if config.strict {
        home_sensitive_read_paths(std::env::var_os("HOME").map(PathBuf::from))
            .into_iter()
            .map(|p| std::fs::canonicalize(&p).unwrap_or(p))
            .collect::<Vec<_>>()
    } else {
        Vec::new()
    };
    let profile = seatbelt_profile(&writable, config.allow_network, config.strict, &sensitive);
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
///
/// `sensitive_read_paths` (H8) are credential-store directories whose reads are
/// denied — but ONLY under `strict`, and only when the slice is non-empty.
/// Off-strict the parameter is ignored entirely so the profile stays
/// byte-identical to the pre-H8 build (additive/inert when off; monotone).
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn seatbelt_profile(
    writable_roots: &[PathBuf],
    allow_network: bool,
    strict: bool,
    sensitive_read_paths: &[PathBuf],
) -> String {
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
    // H3: opt-in strict mode closes the two KNOWN daemon-mediated bypasses of the
    // permissive `(allow default)` baseline by denying `mach-lookup` to the
    // daemons that mediate them. SBPL is last-match-wins, so these denies override
    // the earlier `(allow default)` for exactly these Mach services and nothing
    // else. Purely additive: when `strict` is false the profile is byte-identical
    // to the non-strict build. NOT a completeness claim — other daemons remain
    // reachable; see the module-level "Honest limitations" docs.
    if strict {
        // `cfprefsd` mediates `defaults write`, which persists a plist under
        // ~/Library/Preferences despite `(deny file-write*)`. The bypass is a
        // WRITE, independent of network, so deny it whenever strict.
        p.push_str(
            "(deny mach-lookup\n    (global-name \"com.apple.cfprefsd.agent\")\n    (global-name \"com.apple.cfprefsd.daemon\"))\n",
        );
        // `mDNSResponder` resolves DNS over IPC, so data can exfil over crafted
        // hostnames despite `(deny network*)`. Only deny it when network is
        // ALREADY denied — if the operator allowed network, DNS resolution is
        // legitimate and denying the resolver would just break it (and the socket
        // path is open anyway, so the deny would buy nothing).
        if !allow_network {
            p.push_str(
                "(deny mach-lookup\n    (global-name \"com.apple.mDNSResponder\")\n    (global-name \"com.apple.mDNSResponder.dnsproxy\"))\n",
            );
        }
        // H8: opt-in read-scoping. Deny reads of well-known credential stores
        // (~/.ssh, ~/.aws, ~/Library/Keychains) so gate-run code cannot harvest
        // them before exfil. SBPL is last-match-wins, so this deny overrides the
        // earlier `(allow default)` for exactly these subpaths; it targets a
        // different operation (file-read*) than the write re-allow above, so it
        // does not interact with the writable-root grant. Emitted ONLY when the
        // caller supplied paths — empty when `$HOME` is unresolved, in which case
        // no read-scoping is applied (best-effort; NOT a hard read jail). When
        // `strict` is false this whole block is skipped, so the profile is
        // byte-identical to the pre-H8 build.
        if !sensitive_read_paths.is_empty() {
            p.push_str("(deny file-read*\n");
            for path in sensitive_read_paths {
                p.push_str("    (subpath ");
                p.push_str(&sbpl_string(&path.to_string_lossy()));
                p.push_str(")\n");
            }
            p.push_str(")\n");
        }
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

/// The well-known credential stores whose reads `strict` mode denies (H8),
/// derived from the operator's home directory. Pure (takes `home` explicitly)
/// so it is unit-testable without mutating the process environment. Returns
/// empty when `home` is absent or empty — read-scoping is then simply not
/// applied (best-effort; the mach-lookup denies still apply). macOS-shaped
/// paths; the caller canonicalizes them.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn home_sensitive_read_paths(home: Option<PathBuf>) -> Vec<PathBuf> {
    match home {
        Some(home) if !home.as_os_str().is_empty() => vec![
            home.join(".ssh"),
            home.join(".aws"),
            home.join("Library").join("Keychains"),
        ],
        _ => Vec::new(),
    }
}

/// Decide the [`seatbelt_enforcement_canary`] verdict from two observed facts:
/// whether the out-of-grant ESCAPE file exists (the canary's denied write leaked
/// through) and whether the in-grant LIVE marker exists (the canary actually
/// ran). `Some(false)` = the escape leaked ⇒ enforcement BROKEN; `Some(true)` =
/// no escape AND the canary ran ⇒ enforcement live; `None` = inconclusive (the
/// canary did not even complete its in-grant write, so "no escape" is vacuous —
/// the caller must fail loud, never read it as confined). The escape check wins
/// over the live check: a leaked escape is conclusive evidence of breakage
/// regardless of whether the in-grant write also landed. Pure; tested on every
/// platform.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn canary_enforced(escape_exists: bool, live_exists: bool) -> Option<bool> {
    if escape_exists {
        Some(false)
    } else if live_exists {
        Some(true)
    } else {
        None
    }
}

/// Runtime self-test that macOS Seatbelt still ENFORCES the write confinement
/// the `Required` guarantee depends on. Seatbelt silently DROPS denied
/// operations, so an OS change that broke enforcement would degrade `Required`
/// SILENTLY (H8). This spawns `/bin/sh` under a real `sandbox-exec` profile that
/// grants writes ONLY to a throwaway temp root, then has it (1) write a LIVE
/// marker INSIDE the grant and (2) attempt an ESCAPE write to a sibling temp dir
/// OUTSIDE every grant. Paths are passed via the environment, never interpolated
/// into the shell script, so a path with quotes/spaces/newlines cannot inject
/// shell syntax (same discipline as the H4 confinement canary). The two writes
/// are sequenced with `;` (not `&&`) so the escape is attempted even if the
/// in-grant write fails — a broken in-grant write cannot mask an escape.
///
/// `Ok(true)` = the escape write was DENIED and the canary ran (enforcement
/// live). `Ok(false)` = the escape write SUCCEEDED (Seatbelt is NOT enforcing —
/// `Required` would not actually confine writes on this host). `Err` = the probe
/// could not be run to a conclusion (sandbox-exec missing, temp mint/spawn
/// failed, or the in-grant write did not land so the result is vacuous) — the
/// caller must treat this as a loud failure, never as "confined".
///
/// This is a DIAGNOSTIC for `nv doctor`, NOT a per-run gate (H4 already provides
/// the per-run `Required` self-test); keep it off the hot path.
#[cfg(target_os = "macos")]
pub fn seatbelt_enforcement_canary() -> std::io::Result<bool> {
    use std::process::Command as StdCommand;

    if !Path::new(SANDBOX_EXEC).exists() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("{SANDBOX_EXEC} not found; cannot verify Seatbelt enforcement"),
        ));
    }

    let work = tempfile::tempdir()?;
    let probe = tempfile::tempdir()?;
    // Canonicalize so the grant matches the kernel's symlink-resolved view.
    let work = std::fs::canonicalize(work.path())?;
    // The probe dir is deliberately NOT a writable root, so a write into it must
    // be denied by the kernel when enforcement is live.
    let escape = std::fs::canonicalize(probe.path())?.join("escape.txt");
    let live = work.join("live.txt");

    let profile = seatbelt_profile(std::slice::from_ref(&work), false, false, &[]);

    // The shell expands `$NV_CANARY_*` (set by us to the real paths) as single
    // double-quoted words; the paths never enter the script text.
    let status = StdCommand::new(SANDBOX_EXEC)
        .arg("-p")
        .arg(&profile)
        .arg("/bin/sh")
        .arg("-c")
        .arg("echo live > \"$NV_CANARY_LIVE\" ; echo escaped > \"$NV_CANARY_ESCAPE\"")
        .env("NV_CANARY_LIVE", &live)
        .env("NV_CANARY_ESCAPE", &escape)
        .status()?;
    // Exit code is not authoritative (a denied write makes `sh` exit non-zero,
    // but so would an unrelated failure); file existence is the real signal.
    let _ = status;

    match canary_enforced(escape.exists(), live.exists()) {
        Some(verdict) => Ok(verdict),
        None => Err(std::io::Error::other(
            "Seatbelt enforcement canary inconclusive: the in-grant write did not land, so the absence of an escape cannot be trusted",
        )),
    }
    // `work` (shadowed) and `probe` TempDirs drop here, removing the probe dirs.
}

// ---------------------------------------------------------------------------
// Linux bwrap backend (arg-generation unit-tested; runtime unverified on macOS)
// ---------------------------------------------------------------------------

/// Distinguished refusal reason when `Required` + `landlock` is requested but the
/// running Nerve binary path could not be resolved to insert the in-jail helper.
/// Refusing (rather than silently dropping the requested Landlock layer) keeps
/// `Required` from running under weaker-than-asked confinement.
#[cfg(target_os = "linux")]
const LANDLOCK_HELPER_UNRESOLVED: &str = "sandbox.mode=required with sandbox.landlock=true, but the Nerve binary path could not be resolved to insert the in-jail Landlock helper; refusing to run the check (fail closed)";

#[cfg(target_os = "linux")]
fn bwrap_decide(
    config: &SandboxConfig,
    cwd: &Path,
    check_cmd: &[String],
    extra_writable_roots: &[PathBuf],
    confine_helper: Option<&Path>,
) -> SandboxDecision {
    let Some(bwrap) = trusted_bwrap() else {
        // bwrap is not in a trusted system location — fail closed under
        // `Required` rather than risk executing a PATH-hijacked wrapper.
        return no_backend_decide(config.mode);
    };
    let writable = canonical_writable_roots(cwd, extra_writable_roots);
    // H5/H6: when Landlock and/or the seccomp denylist are requested, the check
    // does not run directly under bwrap — it runs under the in-jail helper
    // (`nv __nv-confine … -- check`) that bwrap execs, which applies the requested
    // layers (Landlock write-confinement matching `writable` plus bwrap's minimal
    // `/dev`, and/or the seccomp syscall denylist) then execs the check. These
    // layers MUST NOT be applied to bwrap itself: Landlock scoped to the writable
    // roots would deny bwrap's OWN unprivileged-userns setup writes (e.g.
    // `/proc/self/uid_map`), and a seccomp denylist of mount/unshare/etc. would
    // block bwrap's own namespace/mount setup — either breaks the jail. So they
    // are composed INSIDE the jail. When neither is requested the argv is
    // byte-identical to the pre-H5/H6 build (the helper is never spliced in).
    let inner: std::borrow::Cow<'_, [String]> = if config.landlock || config.seccomp {
        match confine_helper {
            Some(nv) => std::borrow::Cow::Owned(build_confine_helper_argv(
                nv,
                config.mode,
                config.landlock,
                config.seccomp,
                &writable,
                check_cmd,
            )),
            None => {
                // Only LANDLOCK under `Required` is a fail-closed basis: if the
                // helper can't be spliced, the requested Landlock layer is absent,
                // so `Required` refuses rather than run weaker-than-asked. Seccomp
                // is ALWAYS best-effort (never the fail-closed basis), so a
                // seccomp-only request never refuses here.
                if confine_unresolved_helper_refuses(config.mode, config.landlock) {
                    return SandboxDecision::Refuse {
                        reason: LANDLOCK_HELPER_UNRESOLVED.to_string(),
                    };
                }
                // Auto, or seccomp-only under Required: best-effort — fall back to
                // bwrap-only. The caller (`goal.rs`) surfaces the warning.
                std::borrow::Cow::Borrowed(check_cmd)
            }
        }
    } else {
        std::borrow::Cow::Borrowed(check_cmd)
    };
    let args = bwrap_args(&writable, cwd, inner.as_ref(), config.allow_network);
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

// ---------------------------------------------------------------------------
// H5: Linux Landlock filesystem layer (composed INSIDE the bwrap jail)
// ---------------------------------------------------------------------------
//
// Landlock is an LSM that lets a process irrevocably self-restrict its own
// filesystem access. bwrap already confines writes via a mount namespace; the
// Landlock layer is kernel-mediated defense-in-depth that an in-namespace daemon
// reachable over a bound socket cannot defeat.
//
// It CANNOT be applied to the bwrap process: a write-restriction scoped to the
// writable roots would deny bwrap's own unprivileged-userns setup writes (e.g.
// `/proc/self/uid_map`). So the check is run, inside the jail, through a tiny
// helper — the `nv` binary re-invoked as `nv __nv-confine … -- <check>`, which
// bwrap execs. The helper applies the Landlock ruleset on its single (pre-tokio)
// thread, then `execve`s the real check, which inherits the restriction.
//
// Only the ABI-V1 WRITE rights are handled, so reads/execs stay unrestricted (the
// check must read toolchains; bwrap already read-only-binds the host). Those
// rights are denied everywhere except beneath the granted roots (the same roots
// bwrap rw-binds: `cwd` + the H2 private temp dir) plus bwrap's minimal `/dev` (so
// `/dev/null` etc. work — bwrap's `--dev` exposes only safe pseudo-devices, no
// block devices).
//
// SCOPE (no overclaim): later-ABI rights — `Truncate`, `Refer` (cross-directory
// rename/link), `IoctlDev` — are intentionally NOT handled by this layer. They are
// backstopped by bwrap's read-only host bind, which already denies out-of-root
// host modification at the mount layer, so the COMPOSED bwrap+Landlock stack is
// the boundary (not Landlock alone). Handling `Refer` in particular would risk
// denying legitimate cross-directory renames a check makes, and the marginal
// Landlock-only gain over bwrap does not justify that on a CI-only-verifiable
// path. "Fully enforced" therefore means every *handled* (V1) right is enforced,
// not that every write vector is covered by Landlock alone.
//
// HONEST SCOPE: the argv-shape and fail-closed DECISION logic below is pure and
// unit-tested on every platform, but the actual `restrict_self()`/`execve` path
// runs ONLY on a real Linux kernel — it is exercised by the CI Linux real-kernel
// test (H7), never on the macOS/Windows dev host (there it is cross-compile- and
// clippy-checked only). The exact path-grant set (notably `/dev`) is provisional:
// real-kernel CI is where toolchain-specific write needs (e.g. extra device
// nodes) get shaken out.

/// The hidden subcommand token marking a confinement-helper invocation. Nerve's
/// own sandbox wrap argv inserts `<nv> __nv-confine …` between bwrap's `--` and
/// the real check; the `nv` binary detects this token at startup (before tokio)
/// and routes into the Landlock helper. NEVER a user-facing command — it is only
/// ever produced by [`build_confine_helper_argv`] and consumed by
/// [`parse_confine_args`]. Always compiled so the argv-shape invariant is
/// unit-testable on every platform.
pub const CONFINE_HELPER_TOKEN: &str = "__nv-confine";

/// Exit code the confinement helper uses when it REFUSES to run the check
/// (Landlock not fully enforced under `Required`, a malformed invocation, or an
/// exec failure). The gate sees a non-zero exit and reports the check as failed —
/// fail closed. Distinct from a project's own exit codes only by intent; the
/// helper also logs a distinguished message to stderr.
pub const CONFINE_EXIT_REFUSED: i32 = 70;

/// Machine-greppable marker the in-jail helper embeds in its stderr line when,
/// under `Auto`, Landlock could not be fully enforced and the check runs
/// bwrap-only (best-effort). The helper `execve`s the check and never reports
/// back to the parent, and the parent surfaces a child's captured stderr ONLY on
/// a failing check — so without this marker an Auto degradation on a *passing*
/// check would be silently swallowed. The parent re-scans the captured stderr for
/// this marker ([`stderr_signals_landlock_degraded`]) and re-emits an
/// operator-facing warning on every outcome WHERE THE CHECK'S STDERR WAS CAPTURED
/// (a normal pass or fail) — NOT on a timeout (child killed mid-drain) or a
/// stderr-cap abort (stderr unavailable to scan), which already fail the check
/// loudly. This makes the "surfaced warning" contract true on the captured-stderr
/// path rather than fail-path-only. Stable string (an internal contract between
/// [`confine_degraded_message`] and the parent scan); only ever emitted by the
/// `Auto` degradation path, never by the `Required` refusal, so it cannot fire a
/// spurious best-effort warning for a fail-closed refusal.
pub const CONFINE_DEGRADED_MARKER: &str = "[nv-confine:landlock-auto-degraded]";

/// Platform-independent classification of a Landlock enforcement attempt. Kept
/// free of the `landlock` crate's types so the fail-closed DECISION
/// ([`confine_should_refuse`]) is unit-testable on every platform; the Linux
/// backend maps `RulesetStatus`/errors onto it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LandlockEnforcement {
    /// Every requested write restriction is enforced by the kernel.
    Full,
    /// Only some requested restrictions are enforced (older ABI) — partial.
    Partial,
    /// The kernel does not support Landlock (or enforced nothing).
    None,
    /// Building or applying the ruleset errored.
    Error,
}

/// Fail-closed decision for the confinement helper. Under `Required`
/// (`required == true`) anything short of fully-enforced Landlock means the check
/// must NOT run (the helper exits non-zero → the gate fails closed). Under
/// best-effort/`Auto` (`required == false`) Landlock is never a reason to
/// refuse — the run continues under bwrap-only confinement. Pure; the SOLE
/// authority for whether the helper proceeds, tested on every platform.
pub fn confine_should_refuse(required: bool, enforcement: LandlockEnforcement) -> bool {
    match enforcement {
        LandlockEnforcement::Full => false,
        _ => required,
    }
}

/// Whether an UNRESOLVED in-jail helper (the running `nv` path could not be
/// found, so the helper cannot be spliced) must make the decision REFUSE (fail
/// closed) rather than fall back to bwrap-only. Only LANDLOCK under `Required` is
/// a fail-closed basis: if the helper is missing the requested Landlock layer is
/// simply absent, so `Required` must not run weaker-than-asked. The seccomp
/// denylist is ALWAYS best-effort and is NEVER a fail-closed basis, so a
/// seccomp-only request (Landlock off) never refuses here — it degrades to
/// bwrap-only with an operator warning. The single source of truth shared by the
/// decision layer ([`bwrap_decide`]) and the parent's warning gate (`goal.rs`), so
/// the two cannot drift. Pure; tested on every platform.
pub fn confine_unresolved_helper_refuses(mode: SandboxMode, landlock: bool) -> bool {
    matches!(mode, SandboxMode::Required) && landlock
}

/// The stderr line the helper emits under `Required` when Landlock could not be
/// fully enforced and it REFUSES to run the check (fail closed). Pure so the
/// message contract is testable on every platform. Deliberately does NOT carry
/// [`CONFINE_DEGRADED_MARKER`] — a refusal is a fail-closed Fail, not a
/// best-effort degradation, so it must never trip the parent's best-effort
/// warning scan.
pub fn confine_refused_message(enforcement: LandlockEnforcement) -> String {
    format!(
        "nv {CONFINE_HELPER_TOKEN}: Landlock confinement could not be fully enforced ({enforcement:?}) under sandbox.mode=required; refusing to run the check unconfined (fail closed)"
    )
}

/// The stderr line the helper emits under `Auto` when Landlock could not be fully
/// enforced but the check still runs (bwrap-only, best-effort). Carries
/// [`CONFINE_DEGRADED_MARKER`] so the parent can re-surface the degradation on a
/// passing check. Pure so the marker contract is testable on every platform.
pub fn confine_degraded_message(enforcement: LandlockEnforcement) -> String {
    format!(
        "nv {CONFINE_HELPER_TOKEN}: {CONFINE_DEGRADED_MARKER} Landlock not fully enforced ({enforcement:?}); continuing best-effort under bwrap-only confinement"
    )
}

/// Whether `stderr` (the confined check's captured stderr) carries the in-jail
/// helper's `Auto` best-effort degradation marker. The parent uses this to
/// re-emit an operator-facing warning on every outcome where the check's stderr
/// was captured (a normal pass or fail) — the captured child stderr is otherwise
/// echoed only on a failing check, which would swallow the warning on a passing
/// one. A timeout or a stderr-cap abort (no captured stderr to scan) are the only
/// outcomes it is not re-emitted on; both already fail the check loudly. Pure;
/// tested on every platform.
pub fn stderr_signals_landlock_degraded(stderr: &str) -> bool {
    stderr.contains(CONFINE_DEGRADED_MARKER)
}

/// Machine-greppable marker the in-jail helper embeds in its stderr line when a
/// requested seccomp denylist could not be installed and the check runs without
/// it. Seccomp is ALWAYS best-effort (even under `Required` — it is never the
/// fail-closed basis), so unlike Landlock there is no "refused" counterpart: the
/// helper only ever degrades, never refuses, on a seccomp failure. The parent
/// re-scans the captured stderr for this marker ([`stderr_signals_seccomp_degraded`])
/// and re-emits an operator-facing warning on every outcome where the check's
/// stderr was captured (a normal pass or fail) — same captured-stderr contract as
/// [`CONFINE_DEGRADED_MARKER`]; the only outcomes it is not re-emitted on are a
/// timeout or a stderr-cap abort (no stderr to scan), which already fail loudly.
/// Distinct from the Landlock marker so the two layers' degradations are
/// independently observable. Stable string (an internal contract between
/// [`confine_seccomp_degraded_message`] and the parent scan).
pub const CONFINE_SECCOMP_DEGRADED_MARKER: &str = "[nv-confine:seccomp-degraded]";

/// The stderr line the helper emits when a requested seccomp denylist could not be
/// installed (old kernel, filter rejected) and the check runs without it. Carries
/// [`CONFINE_SECCOMP_DEGRADED_MARKER`] so the parent can re-surface the degradation
/// even on a passing check. Pure so the marker contract is testable on every
/// platform.
pub fn confine_seccomp_degraded_message() -> String {
    format!(
        "nv {CONFINE_HELPER_TOKEN}: {CONFINE_SECCOMP_DEGRADED_MARKER} seccomp denylist could not be installed; continuing best-effort without it (it is never the fail-closed basis)"
    )
}

/// Whether `stderr` (the confined check's captured stderr) carries the in-jail
/// helper's seccomp best-effort degradation marker. The parent uses this to
/// re-emit an operator-facing warning on every outcome where the check's stderr
/// was captured. Pure; tested on every platform.
pub fn stderr_signals_seccomp_degraded(stderr: &str) -> bool {
    stderr.contains(CONFINE_SECCOMP_DEGRADED_MARKER)
}

/// The fixed set of dangerous escape-primitive syscalls the opt-in H6 seccomp
/// DENYLIST blocks (by NAME — platform-independent, so the denylist's CONTENTS are
/// unit-testable on every platform; the Linux backend maps each name to its
/// syscall number via [`seccomp_syscall_number`]). Every entry is a syscall a
/// normal build/test workload never issues but that a hostile check could use to
/// escape or tamper:
///   - namespace / mount control: `mount`, `umount2`, `pivot_root`, `chroot`,
///     `unshare`, `setns`
///   - process tampering: `ptrace`, `process_vm_writev`
///   - kernel-object loading: `bpf`, `init_module`, `finit_module`,
///     `delete_module`, `kexec_load`, `kexec_file_load`
///   - kernel keyring: `add_key`, `request_key`, `keyctl`
///   - exotic exploit primitive: `userfaultfd`
///   - system control: `reboot`, `swapon`, `swapoff`
///
/// SCOPE (no overclaim): this is a DENYLIST, so it is inherently incomplete — it
/// blocks only these named syscalls and does NO argument-level filtering. In
/// particular `clone`/`clone3` are NOT denied (a build must spawn processes), so
/// namespace *creation* via `clone(CLONE_NEW*)` is contained by bwrap's own
/// namespace setup plus the `unshare`/`setns` denial, not by this list inspecting
/// clone flags. The list is defense-in-depth layered on bwrap + Landlock, never a
/// standalone boundary.
pub const SECCOMP_DENYLIST: &[&str] = &[
    "mount",
    "umount2",
    "pivot_root",
    "chroot",
    "unshare",
    "setns",
    "ptrace",
    "process_vm_writev",
    "bpf",
    "init_module",
    "finit_module",
    "delete_module",
    "kexec_load",
    "kexec_file_load",
    "add_key",
    "request_key",
    "keyctl",
    "userfaultfd",
    "reboot",
    "swapon",
    "swapoff",
];

/// Map a [`SandboxMode`] to the helper's `--mode` token. Only `Required` carries
/// the strict fail-closed contract; everything else (including `Off`, which never
/// reaches the helper) is best-effort `auto`. Pure.
fn confine_mode_token(mode: SandboxMode) -> &'static str {
    match mode {
        SandboxMode::Required => "required",
        SandboxMode::Auto | SandboxMode::Off => "auto",
    }
}

/// Parse the helper's `--mode` token back into the fail-closed flag. `required`
/// ⇒ `true`, `auto` ⇒ `false`, anything else ⇒ `None` (malformed → the helper
/// fails closed rather than guess). Pure.
fn confine_required_from_token(token: &str) -> Option<bool> {
    match token {
        "required" => Some(true),
        "auto" => Some(false),
        _ => None,
    }
}

/// Build the confinement-helper segment Nerve splices between bwrap's `--` and
/// the real check:
/// `<nv> __nv-confine --mode <m> [--landlock] [--seccomp] (--root <p>)* -- <check…>`.
///
/// `nv` is the trusted absolute path to the running Nerve binary (resolved by the
/// caller via `current_exe`); bwrap execs it INSIDE the jail, where it applies the
/// requested confinement layers then `execve`s the check. `--landlock` and
/// `--seccomp` are independent opt-in flags (at least one is set wherever this
/// argv is spliced, but the function does not assume that). `roots` — the same
/// writable roots bwrap rw-binds (operator/system-controlled: `cwd` + the private
/// temp dir, never lead-controlled) — are emitted ONLY when `landlock` is set,
/// since they configure the Landlock write grant and are meaningless without it.
/// The trailing `--` terminates the helper's own option parsing, so a `check[0]`
/// beginning with `-` is treated as the command, never a helper flag (same
/// discipline as the bwrap/sandbox-exec `--`). Pure; tested on every platform.
pub fn build_confine_helper_argv(
    nv: &Path,
    mode: SandboxMode,
    landlock: bool,
    seccomp: bool,
    roots: &[PathBuf],
    check_cmd: &[String],
) -> Vec<String> {
    let mut argv = vec![
        nv.to_string_lossy().into_owned(),
        CONFINE_HELPER_TOKEN.to_string(),
        "--mode".to_string(),
        confine_mode_token(mode).to_string(),
    ];
    if landlock {
        argv.push("--landlock".to_string());
    }
    if seccomp {
        argv.push("--seccomp".to_string());
    }
    if landlock {
        for root in roots {
            argv.push("--root".to_string());
            argv.push(root.to_string_lossy().into_owned());
        }
    }
    argv.push("--".to_string());
    argv.extend(check_cmd.iter().cloned());
    argv
}

/// A parsed confinement-helper invocation (the argv AFTER the `__nv-confine`
/// token).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfineSpec {
    /// `true` ⇒ `sandbox.mode=required`. Governs ONLY the Landlock fail-closed
    /// decision ([`confine_should_refuse`]): under `Required`, Landlock that is
    /// not fully enforced makes the helper refuse. Seccomp ignores this flag — it
    /// is always best-effort, never the fail-closed basis. `false` ⇒ best-effort
    /// (`Auto`) for every layer.
    pub required: bool,
    /// Apply the Landlock write-confinement (the `--landlock` flag was present).
    /// When `false` the helper applies no Landlock ruleset and `roots` is unused.
    pub landlock: bool,
    /// Apply the seccomp syscall denylist (the `--seccomp` flag was present).
    /// Always best-effort regardless of `required`.
    pub seccomp: bool,
    /// Writable roots to grant Landlock file-write beneath. Only consumed when
    /// `landlock` is `true`.
    pub roots: Vec<PathBuf>,
    /// The real check command to `execve` once confinement is applied.
    pub check_cmd: Vec<String>,
}

/// Parse the confinement-helper argv tail (everything after `__nv-confine`):
/// `--mode <required|auto> [--landlock] [--seccomp] (--root <path>)* -- <check> [args…]`.
/// Returns `None` on ANY malformed input — unknown flag, missing/duplicate
/// `--mode`, a duplicated `--landlock`/`--seccomp`, missing value, empty root, no
/// `--` terminator, or an empty check — so the helper fails closed rather than
/// guess. Note it does NOT require at least one of `--landlock`/`--seccomp`: the
/// SPLICE condition in [`bwrap_decide`] is the sole authority for when the helper
/// is inserted (only when a layer is requested), so a flagless helper would simply
/// apply nothing and `execve` the check under bwrap (already the confinement) — a
/// harmless no-op, never a fail-open. Pure; tested on every platform.
pub fn parse_confine_args(tail: &[String]) -> Option<ConfineSpec> {
    let mut required: Option<bool> = None;
    let mut landlock = false;
    let mut seccomp = false;
    let mut roots: Vec<PathBuf> = Vec::new();
    let mut i = 0;
    loop {
        let token = tail.get(i)?; // ran out before a `--` terminator ⇒ malformed
        match token.as_str() {
            "--" => {
                i += 1;
                break;
            }
            "--mode" => {
                let value = tail.get(i + 1)?;
                if required.is_some() {
                    return None; // duplicate --mode
                }
                required = Some(confine_required_from_token(value)?);
                i += 2;
            }
            "--landlock" => {
                if landlock {
                    return None; // duplicate --landlock
                }
                landlock = true;
                i += 1;
            }
            "--seccomp" => {
                if seccomp {
                    return None; // duplicate --seccomp
                }
                seccomp = true;
                i += 1;
            }
            "--root" => {
                let value = tail.get(i + 1)?;
                if value.is_empty() {
                    return None;
                }
                roots.push(PathBuf::from(value));
                i += 2;
            }
            _ => return None, // unknown flag
        }
    }
    let required = required?; // --mode is mandatory
    let check_cmd = tail[i..].to_vec();
    if check_cmd.is_empty() {
        return None;
    }
    Some(ConfineSpec {
        required,
        landlock,
        seccomp,
        roots,
        check_cmd,
    })
}

/// H5 entry point: if this process was started as the confinement helper
/// (`nv __nv-confine …`, inserted by Nerve's own bwrap wrap argv), apply the
/// Landlock ruleset and `execve` the real check; otherwise return so normal CLI
/// startup proceeds.
///
/// MUST be called at the very top of `main`, BEFORE any tokio worker thread
/// exists, so Landlock is applied on the sole (main) thread and the `execve`
/// replaces a single-threaded image. On success it never returns (it becomes the
/// check); on refusal or exec failure it exits the process non-zero (fail
/// closed). Linux only — the helper is injected only on Linux.
#[cfg(target_os = "linux")]
pub fn maybe_run_confine_helper() {
    let mut argv = std::env::args();
    let _bin = argv.next();
    match argv.next() {
        Some(token) if token == CONFINE_HELPER_TOKEN => {}
        _ => return,
    }
    let tail: Vec<String> = argv.collect();
    let Some(spec) = parse_confine_args(&tail) else {
        eprintln!(
            "nv {CONFINE_HELPER_TOKEN}: malformed confinement invocation; refusing to run (fail closed)"
        );
        std::process::exit(CONFINE_EXIT_REFUSED);
    };
    run_confine_helper(&spec);
}

/// Apply the requested confinement layers for `spec` (Landlock and/or the seccomp
/// denylist), then `execve` the real check. Diverges: it either becomes the check
/// (success) or exits the process.
///
/// Ordering matters: Landlock is applied FIRST, then seccomp. This is forward-safe
/// because the denylist never blocks `prctl`/`seccomp`/`landlock_*`, so installing
/// seccomp second cannot have blocked any syscall Landlock needs (and Landlock,
/// being filesystem-only, never blocks the seccomp syscall). Both restrictions
/// survive the final `execve`.
#[cfg(target_os = "linux")]
fn run_confine_helper(spec: &ConfineSpec) -> ! {
    use std::os::unix::process::CommandExt;
    // --- Landlock (fail-closed basis under Required) ---
    if spec.landlock {
        let enforcement = apply_landlock_write_confinement(&spec.roots);
        if confine_should_refuse(spec.required, enforcement) {
            eprintln!("{}", confine_refused_message(enforcement));
            std::process::exit(CONFINE_EXIT_REFUSED);
        }
        if enforcement != LandlockEnforcement::Full {
            // Auto best-effort: carries CONFINE_DEGRADED_MARKER so the parent
            // re-surfaces this through tracing even when the check passes.
            eprintln!("{}", confine_degraded_message(enforcement));
        }
    }
    // --- Seccomp denylist (ALWAYS best-effort, never the fail-closed basis) ---
    if spec.seccomp && apply_seccomp_denylist().is_err() {
        // A kernel that cannot install the filter (too old, filter rejected)
        // degrades to running without it. Seccomp is secondary hardening and is
        // never the basis for `Required`, so this NEVER refuses — it carries
        // CONFINE_SECCOMP_DEGRADED_MARKER so the parent re-surfaces it through
        // tracing even when the check passes.
        eprintln!("{}", confine_seccomp_degraded_message());
    }
    // `exec` replaces this process image with the check, preserving the Landlock
    // and seccomp restrictions (both survive execve). It only returns on failure.
    let error = std::process::Command::new(&spec.check_cmd[0])
        .args(&spec.check_cmd[1..])
        .exec();
    eprintln!(
        "nv {CONFINE_HELPER_TOKEN}: failed to exec check `{}`: {error}",
        spec.check_cmd[0]
    );
    std::process::exit(CONFINE_EXIT_REFUSED);
}

/// Build and `restrict_self()` a Landlock ruleset that denies the ABI-V1 file
/// WRITE rights everywhere except beneath `roots` (plus bwrap's minimal `/dev`,
/// best-effort). Reads/execs and later-ABI rights (`Truncate`/`Refer`/`IoctlDev`)
/// are NOT handled, so they stay unrestricted at this layer — bwrap's read-only
/// host bind backstops them (the composed stack is the boundary, not Landlock
/// alone). Returns how fully the
/// kernel enforced it. Runs on a real Linux kernel only (CI / the calling helper
/// inside bwrap); never invoked on the macOS/Windows dev host.
#[cfg(target_os = "linux")]
fn apply_landlock_write_confinement(roots: &[PathBuf]) -> LandlockEnforcement {
    use landlock::{
        ABI, AccessFs, CompatLevel, Compatible, PathBeneath, PathFd, Ruleset, RulesetAttr,
        RulesetCreatedAttr, RulesetStatus,
    };
    // ABI V1 (Linux 5.13+) basic filesystem write rights. Later-ABI write rights
    // (Refer=V2, Truncate=V3, IoctlDev=V5) are deliberately NOT requested: they
    // are backstopped by bwrap's read-only host bind, and requesting them would
    // both raise the kernel-version floor under Required and risk denying
    // legitimate cross-directory renames (Refer) on a path only CI can verify.
    let abi = ABI::V1;
    let write = AccessFs::from_write(abi);
    let created = Ruleset::default()
        .set_compatibility(CompatLevel::BestEffort)
        .handle_access(write)
        .and_then(|ruleset| ruleset.create());
    let mut created = match created {
        Ok(created) => created,
        Err(_) => return LandlockEnforcement::Error,
    };
    // Mandatory roots (the writable roots bwrap rw-binds). A missing one is an
    // error → fail closed under Required.
    for root in roots {
        let path_fd = match PathFd::new(root) {
            Ok(path_fd) => path_fd,
            Err(_) => return LandlockEnforcement::Error,
        };
        created = match created.add_rule(PathBeneath::new(path_fd, write)) {
            Ok(created) => created,
            Err(_) => return LandlockEnforcement::Error,
        };
    }
    // Grant writes to bwrap's minimal `/dev` (only safe pseudo-devices) so the
    // check can write `/dev/null` etc. The grant is SKIPPED only if `/dev` is
    // absent (e.g. a unit context outside bwrap); if `/dev` exists but the rule
    // cannot be added, that is an error → fail closed under Required.
    if let Ok(dev_fd) = PathFd::new("/dev") {
        created = match created.add_rule(PathBeneath::new(dev_fd, write)) {
            Ok(with_dev) => with_dev,
            Err(_) => return LandlockEnforcement::Error,
        };
    }
    match created.restrict_self() {
        Ok(status) => match status.ruleset {
            RulesetStatus::FullyEnforced => LandlockEnforcement::Full,
            RulesetStatus::PartiallyEnforced => LandlockEnforcement::Partial,
            RulesetStatus::NotEnforced => LandlockEnforcement::None,
        },
        Err(_) => LandlockEnforcement::Error,
    }
}

// ---------------------------------------------------------------------------
// H6: Linux seccomp-bpf syscall denylist (composed INSIDE the bwrap jail)
// ---------------------------------------------------------------------------
//
// Like Landlock, the denylist is applied INSIDE the jail by the same helper, not
// to bwrap: denying mount/unshare/setns/etc. on bwrap itself would block bwrap's
// own namespace and mount setup. The helper installs the filter on its single
// (pre-tokio) thread, after Landlock, then `execve`s the check, which inherits the
// filter (seccomp survives execve once `no_new_privs` is set — which `seccompiler`
// does for us). It is ALWAYS best-effort: a failure to install degrades to running
// without it, never refuses, and is never the basis for `Required`.

/// Map a denylist syscall NAME to its number on this Linux build. Explicit and
/// auditable (no name-table magic); a name with no `libc::SYS_*` constant returns
/// `None`. The numbers are arch-specific but `libc` resolves the right ones for
/// the target, so the same name list works on every Linux arch. A Linux-only test
/// asserts every [`SECCOMP_DENYLIST`] entry resolves (catches a typo or a renamed
/// constant at build time).
#[cfg(target_os = "linux")]
fn seccomp_syscall_number(name: &str) -> Option<i64> {
    let nr: libc::c_long = match name {
        "mount" => libc::SYS_mount,
        "umount2" => libc::SYS_umount2,
        "pivot_root" => libc::SYS_pivot_root,
        "chroot" => libc::SYS_chroot,
        "unshare" => libc::SYS_unshare,
        "setns" => libc::SYS_setns,
        "ptrace" => libc::SYS_ptrace,
        "process_vm_writev" => libc::SYS_process_vm_writev,
        "bpf" => libc::SYS_bpf,
        "init_module" => libc::SYS_init_module,
        "finit_module" => libc::SYS_finit_module,
        "delete_module" => libc::SYS_delete_module,
        "kexec_load" => libc::SYS_kexec_load,
        "kexec_file_load" => libc::SYS_kexec_file_load,
        "add_key" => libc::SYS_add_key,
        "request_key" => libc::SYS_request_key,
        "keyctl" => libc::SYS_keyctl,
        "userfaultfd" => libc::SYS_userfaultfd,
        "reboot" => libc::SYS_reboot,
        "swapon" => libc::SYS_swapon,
        "swapoff" => libc::SYS_swapoff,
        _ => return None,
    };
    Some(nr as i64)
}

/// Compile the [`SECCOMP_DENYLIST`] into a BPF program: every listed syscall →
/// `KillProcess` (the process is killed with `SIGSYS` if it issues one), every
/// other syscall → `Allow`. Returns `Err(())` if a name fails to resolve or the
/// filter cannot be built/compiled. Kept SEPARATE from [`apply_seccomp_denylist`]
/// so the (allocating) compile step can run in a parent process and the
/// (syscall-only) install step can run in a forked child — the real-kernel test
/// relies on exactly that split to apply the filter without killing the test
/// runner.
#[cfg(target_os = "linux")]
fn build_seccomp_denylist_program() -> Result<seccompiler::BpfProgram, ()> {
    use seccompiler::{SeccompAction, SeccompFilter};
    use std::collections::BTreeMap;
    let mut rules: BTreeMap<i64, Vec<seccompiler::SeccompRule>> = BTreeMap::new();
    for name in SECCOMP_DENYLIST {
        let nr = seccomp_syscall_number(name).ok_or(())?;
        // Empty rule vec ⇒ match the syscall unconditionally (no arg filtering).
        rules.insert(nr, Vec::new());
    }
    let arch = std::env::consts::ARCH.try_into().map_err(|_| ())?;
    // new(rules, mismatch_action, match_action, arch): non-listed syscalls Allow,
    // listed syscalls KillProcess. The two actions must differ (they do).
    let filter = SeccompFilter::new(
        rules,
        SeccompAction::Allow,
        SeccompAction::KillProcess,
        arch,
    )
    .map_err(|_| ())?;
    filter.try_into().map_err(|_| ())
}

/// Install the [`SECCOMP_DENYLIST`] filter on the current process (all threads via
/// TSYNC). `seccompiler::apply_filter*` sets `PR_SET_NO_NEW_PRIVS` for us, so the
/// filter is honored unprivileged and survives the helper's subsequent `execve`.
/// Best-effort: returns `Err(())` on any failure so the caller degrades rather
/// than refuses. Runs on a real Linux kernel only (CI / the calling helper inside
/// bwrap); exercised by the H7 real-kernel test, never on the macOS/Windows host.
#[cfg(target_os = "linux")]
fn apply_seccomp_denylist() -> Result<(), ()> {
    let program = build_seccomp_denylist_program()?;
    seccompiler::apply_filter_all_threads(&program).map_err(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(mode: SandboxMode, allow_network: bool) -> SandboxConfig {
        SandboxConfig {
            mode,
            allow_network,
            ..Default::default()
        }
    }

    #[test]
    fn off_mode_is_always_unconfined_without_warning() {
        let d = decide(
            &cfg(SandboxMode::Off, false),
            Path::new("/tmp/work"),
            &["true".to_string()],
            &[],
            None,
        );
        assert_eq!(d, SandboxDecision::Unconfined { warning: None });
    }

    // --- Seatbelt profile (pure; tested on every platform) ---

    #[test]
    fn seatbelt_profile_denies_writes_and_network_by_default() {
        let p = seatbelt_profile(&[PathBuf::from("/tmp/work")], false, false, &[]);
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
        let p = seatbelt_profile(&[PathBuf::from("/tmp/work")], true, false, &[]);
        assert!(!p.contains("(deny network*)"));
    }

    #[test]
    fn seatbelt_profile_escapes_quotes_and_backslashes_in_paths() {
        // A path with a quote/paren must not break out of the string literal.
        let evil = PathBuf::from("/tmp/a\"b)\\c");
        let p = seatbelt_profile(&[evil], false, false, &[]);
        assert!(p.contains("(subpath \"/tmp/a\\\"b)\\\\c\")"), "profile = {p}");
    }

    // --- H3: opt-in strict daemon-mediated-bypass mitigation (pure) ---

    /// strict=false is ADDITIVE-inert: the profile carries NO `mach-lookup` deny,
    /// so it is byte-identical to the pre-H3 profile for the same inputs.
    #[test]
    fn seatbelt_profile_strict_off_emits_no_mach_lookup_deny() {
        let roots = [PathBuf::from("/tmp/work")];
        for allow_network in [false, true] {
            let p = seatbelt_profile(&roots, allow_network, false, &[]);
            assert!(
                !p.contains("mach-lookup"),
                "strict=off must not emit any mach-lookup deny (allow_network={allow_network}); profile = {p}"
            );
        }
    }

    /// strict=true (hermetic) denies BOTH the cfprefsd write-persistence channel
    /// and the mDNSResponder DNS-exfil channel, and the denies come AFTER
    /// `(allow default)` so last-match-wins actually denies them.
    #[test]
    fn seatbelt_profile_strict_denies_cfprefsd_and_mdns_when_hermetic() {
        let p = seatbelt_profile(&[PathBuf::from("/tmp/work")], false, true, &[]);
        assert!(p.contains("(deny mach-lookup"), "profile = {p}");
        assert!(p.contains("com.apple.cfprefsd.agent"), "profile = {p}");
        assert!(p.contains("com.apple.cfprefsd.daemon"), "profile = {p}");
        assert!(p.contains("com.apple.mDNSResponder"), "profile = {p}");
        let allow_default = p.find("(allow default)").unwrap();
        let deny_lookup = p.find("(deny mach-lookup").unwrap();
        assert!(
            allow_default < deny_lookup,
            "(allow default) must precede the mach-lookup deny (last-match-wins)"
        );
    }

    /// strict=true with network ALLOWED keeps the cfprefsd write deny (a write
    /// bypass, independent of network) but DROPS the mDNSResponder deny — denying
    /// the resolver when network is legitimately allowed would only break DNS and
    /// buy nothing (the socket path is open anyway).
    #[test]
    fn seatbelt_profile_strict_with_network_keeps_cfprefsd_drops_mdns() {
        let p = seatbelt_profile(&[PathBuf::from("/tmp/work")], true, true, &[]);
        assert!(p.contains("com.apple.cfprefsd.agent"), "profile = {p}");
        assert!(
            !p.contains("mDNSResponder"),
            "mDNSResponder deny is pointless (and harmful) when network is allowed; profile = {p}"
        );
        assert!(!p.contains("(deny network*)"), "network is allowed; profile = {p}");
    }

    // --- H8 read-scoping (pure) ---

    /// strict=true WITH sensitive paths emits a `(deny file-read* (subpath …))`
    /// block for each path, and the deny comes AFTER `(allow default)` so
    /// last-match-wins actually denies the reads.
    #[test]
    fn seatbelt_profile_strict_denies_reads_of_sensitive_paths() {
        let secrets = [
            PathBuf::from("/Users/x/.ssh"),
            PathBuf::from("/Users/x/.aws"),
            PathBuf::from("/Users/x/Library/Keychains"),
        ];
        let p = seatbelt_profile(&[PathBuf::from("/tmp/work")], false, true, &secrets);
        assert!(p.contains("(deny file-read*"), "profile = {p}");
        for s in &secrets {
            assert!(
                p.contains(&sbpl_string(&s.to_string_lossy())),
                "missing deny subpath for {}; profile = {p}",
                s.display()
            );
        }
        let allow_default = p.find("(allow default)").unwrap();
        let deny_read = p.find("(deny file-read*").unwrap();
        assert!(
            allow_default < deny_read,
            "(allow default) must precede the file-read deny (last-match-wins)"
        );
    }

    /// strict=FALSE never emits a file-read deny, even when sensitive paths are
    /// passed: read-scoping is gated behind strict, and the profile must be
    /// byte-identical to the same call with no sensitive paths.
    #[test]
    fn seatbelt_profile_read_scoping_is_inert_when_strict_off() {
        let secrets = [PathBuf::from("/Users/x/.ssh")];
        let with = seatbelt_profile(&[PathBuf::from("/tmp/work")], false, false, &secrets);
        let without = seatbelt_profile(&[PathBuf::from("/tmp/work")], false, false, &[]);
        assert!(!with.contains("(deny file-read*"), "profile = {with}");
        assert_eq!(
            with, without,
            "off-strict profile must ignore sensitive paths (byte-identical)"
        );
    }

    /// strict=true with NO sensitive paths (e.g. `$HOME` unresolved) emits no
    /// file-read deny — read-scoping is best-effort, not a hard read jail.
    #[test]
    fn seatbelt_profile_strict_without_sensitive_paths_emits_no_read_deny() {
        let p = seatbelt_profile(&[PathBuf::from("/tmp/work")], false, true, &[]);
        assert!(!p.contains("(deny file-read*"), "profile = {p}");
        // The mach-lookup denies still apply (strict is otherwise unchanged).
        assert!(p.contains("(deny mach-lookup"), "profile = {p}");
    }

    /// `home_sensitive_read_paths` derives the three credential stores from a
    /// present home dir, and returns empty when home is absent or empty (so the
    /// caller applies no read-scoping rather than denying bogus paths).
    #[test]
    fn home_sensitive_read_paths_derives_from_home_and_fails_open() {
        let paths = home_sensitive_read_paths(Some(PathBuf::from("/Users/x")));
        assert_eq!(
            paths,
            vec![
                PathBuf::from("/Users/x/.ssh"),
                PathBuf::from("/Users/x/.aws"),
                PathBuf::from("/Users/x/Library/Keychains"),
            ]
        );
        assert!(home_sensitive_read_paths(None).is_empty());
        assert!(home_sensitive_read_paths(Some(PathBuf::new())).is_empty());
    }

    /// `canary_enforced` truth table: a leaked escape is BROKEN regardless of the
    /// live marker; no escape + live ran = enforced; nothing ran = inconclusive.
    #[test]
    fn canary_enforced_truth_table() {
        assert_eq!(canary_enforced(true, true), Some(false)); // escaped => broken
        assert_eq!(canary_enforced(true, false), Some(false)); // escaped => broken
        assert_eq!(canary_enforced(false, true), Some(true)); // confined + ran
        assert_eq!(canary_enforced(false, false), None); // inconclusive
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
        let profile = seatbelt_profile(std::slice::from_ref(&work), false, false, &[]);

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

    /// H3 end-to-end (macOS real kernel): the cfprefsd-mediated write bypass that
    /// the direct-write proof above explicitly does NOT cover is CLOSED under
    /// strict mode. `defaults write` persists a value via `cfprefsd` (a daemon
    /// outside the sandbox) even though a direct write to `~/Library/Preferences`
    /// is denied. We prove the bypass is REAL under the non-strict profile (the
    /// value persists) and is DENIED under the strict profile (it does not) — by
    /// denying `mach-lookup` to cfprefsd. Two distinct per-pid domains so the
    /// control's persisted value cannot mask the strict result; cleanup runs
    /// BEFORE any assertion so a failure cannot leak the test plists. A positive
    /// control (`/usr/bin/true` under the strict profile, exit 0) proves
    /// `sandbox-exec` ACCEPTED the strict SBPL, so the strict non-persistence is
    /// attributable to the mach-lookup deny rather than a rejected/invalid profile.
    #[cfg(target_os = "macos")]
    #[test]
    fn strict_profile_denies_cfprefsd_mediated_write() {
        use std::process::Command as StdCommand;

        let work = tempfile::tempdir().unwrap();
        let work = std::fs::canonicalize(work.path()).unwrap();
        let lax = seatbelt_profile(std::slice::from_ref(&work), false, false, &[]);
        let strict = seatbelt_profile(std::slice::from_ref(&work), false, true, &[]);

        let pid = std::process::id();
        let domain_ctrl = format!("com.nerve.h3test.ctrl.{pid}");
        let domain_strict = format!("com.nerve.h3test.strict.{pid}");

        // `defaults write <domain> probe -int 1` under `sandbox-exec -p <profile>`.
        let write_under = |profile: &str, domain: &str| {
            StdCommand::new(SANDBOX_EXEC)
                .arg("-p")
                .arg(profile)
                .arg("/usr/bin/defaults")
                .arg("write")
                .arg(domain)
                .arg("probe")
                .arg("-int")
                .arg("1")
                .status()
                .map(|s| s.success())
                .unwrap_or(false)
        };
        // Did the value actually PERSIST? Read it back UNSANDBOXED (`defaults read`
        // exits 0 iff the key exists). This reads through the same cfprefsd, so it
        // is the authoritative persistence signal regardless of the write's exit.
        let persisted = |domain: &str| {
            StdCommand::new("/usr/bin/defaults")
                .arg("read")
                .arg(domain)
                .arg("probe")
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false)
        };

        let _ = write_under(&lax, &domain_ctrl);
        let _ = write_under(&strict, &domain_strict);
        let persisted_ctrl = persisted(&domain_ctrl);
        let persisted_strict = persisted(&domain_strict);

        // Positive control: prove `sandbox-exec` ACCEPTS the strict profile (compiles
        // it and launches the child) by running `/usr/bin/true` under it. `sandbox-exec`
        // parses `-p <profile>` BEFORE spawning the child, so exit 0 here means the
        // strict SBPL is syntactically valid and was applied. Without this, a future
        // strict-SBPL edit that broke the syntax would make `sandbox-exec` reject the
        // profile and run nothing — silently turning `!persisted_strict` into a VACUOUS
        // pass. Exit 0 attributes the non-persistence below to the cfprefsd mach-lookup
        // deny, not to a rejected profile. (`true` creates no state, so no cleanup.)
        let strict_profile_accepted = StdCommand::new(SANDBOX_EXEC)
            .arg("-p")
            .arg(&strict)
            .arg("/usr/bin/true")
            .status()
            .map(|s| s.success())
            .unwrap_or(false);

        // Cleanup BEFORE asserting (no RAII for `defaults`; a panic must not leak).
        let _ = StdCommand::new("/usr/bin/defaults").arg("delete").arg(&domain_ctrl).status();
        let _ = StdCommand::new("/usr/bin/defaults").arg("delete").arg(&domain_strict).status();

        // The strict profile is valid and accepted: `!persisted_strict` below reflects
        // the mach-lookup deny, not a rejected profile that silently ran nothing.
        assert!(
            strict_profile_accepted,
            "positive control: sandbox-exec must accept the strict profile (else !persisted_strict is vacuous)"
        );
        // The bypass is real: under the non-strict profile cfprefsd persisted it.
        assert!(
            persisted_ctrl,
            "control: cfprefsd-mediated write should persist under the non-strict profile (else the test is vacuous)"
        );
        // strict closes it: the cfprefsd mach-lookup deny stopped the persistence.
        assert!(
            !persisted_strict,
            "strict: cfprefsd-mediated write must NOT persist (mach-lookup deny should block it)"
        );
    }

    /// H8 read-scoping (macOS real kernel): under the strict profile a read of a
    /// file UNDER a listed sensitive path is DENIED, while a read of a file NOT
    /// under any sensitive path still succeeds. We pass a throwaway temp dir as
    /// the "sensitive" path (the same machinery production points at ~/.ssh etc.)
    /// so the test never touches the operator's real credentials. The successful
    /// public read is the positive control: it proves `sandbox-exec` ACCEPTED the
    /// strict profile and ran the child, so the denied secret read is attributable
    /// to the file-read deny, not a rejected/invalid profile or a missing file.
    #[cfg(target_os = "macos")]
    #[test]
    fn strict_profile_denies_reads_under_sensitive_path() {
        use std::process::Command as StdCommand;

        let work = tempfile::tempdir().unwrap();
        let secret_dir = tempfile::tempdir().unwrap();
        let work = std::fs::canonicalize(work.path()).unwrap();
        let secret_dir = std::fs::canonicalize(secret_dir.path()).unwrap();

        // A readable file outside any sensitive path, and a "secret" inside one.
        let public = work.join("public.txt");
        std::fs::write(&public, "PUBLIC").unwrap();
        let secret = secret_dir.join("key.txt");
        std::fs::write(&secret, "TOPSECRET").unwrap();

        // strict profile grants writes to `work` and scopes reads away from
        // `secret_dir`.
        let strict = seatbelt_profile(
            std::slice::from_ref(&work),
            false,
            true,
            std::slice::from_ref(&secret_dir),
        );

        // `cat "$NV_TARGET"` — path via env, never interpolated into the script.
        let read_under = |target: &std::path::Path| {
            StdCommand::new(SANDBOX_EXEC)
                .arg("-p")
                .arg(&strict)
                .arg("/bin/sh")
                .arg("-c")
                .arg("cat \"$NV_TARGET\"")
                .env("NV_TARGET", target)
                .status()
                .map(|s| s.success())
                .unwrap_or(false)
        };

        // Positive control: the public read succeeds, proving the strict profile
        // was accepted and the child ran (so the secret denial below is real).
        assert!(
            read_under(&public),
            "positive control: reading a non-sensitive file must succeed under the strict profile"
        );
        // The read-scoping denies the secret read.
        assert!(
            !read_under(&secret),
            "strict read-scoping: a read under the sensitive path must be denied"
        );
    }

    /// H8 enforcement canary (macOS real kernel, healthy host): Seatbelt enforces
    /// on this dev host, so the canary reports `Ok(true)` — a known out-of-root
    /// write was denied while the in-grant write landed. This is the signal
    /// `nv doctor` surfaces; if a future OS broke enforcement it would flip to
    /// `Ok(false)` and the doctor check would fail loudly.
    #[cfg(target_os = "macos")]
    #[test]
    fn seatbelt_enforcement_canary_reports_enforced_on_healthy_host() {
        assert!(
            seatbelt_enforcement_canary().unwrap(),
            "Seatbelt must deny a known out-of-root write on a healthy macOS host"
        );
    }

    /// H8 canary fail-detection (macOS real kernel): if the probe profile were
    /// to GRANT the escape target (simulating broken/ineffective enforcement),
    /// the escape write would land and `canary_enforced` must read it as BROKEN
    /// (`Some(false)`), never as confined. This mirrors the H4 fail-closed proof:
    /// it shows the canary's verdict tracks real kernel behavior, so it cannot
    /// silently pass when enforcement is absent.
    #[cfg(target_os = "macos")]
    #[test]
    fn seatbelt_canary_detects_ineffective_confinement() {
        use std::process::Command as StdCommand;

        let work = tempfile::tempdir().unwrap();
        let probe = tempfile::tempdir().unwrap();
        let work = std::fs::canonicalize(work.path()).unwrap();
        let probe = std::fs::canonicalize(probe.path()).unwrap();
        let escape = probe.join("escape.txt");
        let live = work.join("live.txt");

        // INEFFECTIVE profile: grant writes to BOTH roots, so the "escape" write
        // is actually permitted — exactly what a broken Seatbelt would allow.
        let granted = [work.clone(), probe.clone()];
        let profile = seatbelt_profile(&granted, false, false, &[]);

        let _ = StdCommand::new(SANDBOX_EXEC)
            .arg("-p")
            .arg(&profile)
            .arg("/bin/sh")
            .arg("-c")
            .arg("echo live > \"$NV_CANARY_LIVE\" ; echo escaped > \"$NV_CANARY_ESCAPE\"")
            .env("NV_CANARY_LIVE", &live)
            .env("NV_CANARY_ESCAPE", &escape)
            .status()
            .unwrap();

        // The escape landed (enforcement ineffective) → verdict must be BROKEN.
        assert!(escape.exists(), "escape write should land under the granting profile");
        assert_eq!(
            canary_enforced(escape.exists(), live.exists()),
            Some(false),
            "canary must report BROKEN when the escape write is not confined"
        );
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
            None,
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
        let p = seatbelt_profile(&[], false, false, &[]);
        assert!(p.contains("(deny file-write*)"));
        assert!(!p.contains("(allow file-write*"), "no roots => no re-allow block");
    }

    // --- H5: Landlock confinement helper (pure logic; tested on every platform) ---

    #[test]
    fn confine_mode_token_maps_required_strict_else_auto() {
        assert_eq!(confine_mode_token(SandboxMode::Required), "required");
        assert_eq!(confine_mode_token(SandboxMode::Auto), "auto");
        // `Off` never reaches the helper but must not map to the strict token.
        assert_eq!(confine_mode_token(SandboxMode::Off), "auto");
    }

    #[test]
    fn confine_should_refuse_truth_table() {
        use LandlockEnforcement::*;
        // Required: only fully-enforced Landlock may proceed; everything else
        // refuses (fail closed).
        assert!(!confine_should_refuse(true, Full));
        assert!(confine_should_refuse(true, Partial));
        assert!(confine_should_refuse(true, None));
        assert!(confine_should_refuse(true, Error));
        // Auto/best-effort: Landlock is never a reason to refuse.
        for e in [Full, Partial, None, Error] {
            assert!(!confine_should_refuse(false, e), "auto must never refuse on landlock grounds: {e:?}");
        }
    }

    #[test]
    fn confine_unresolved_helper_refuses_only_for_required_landlock() {
        use SandboxMode::*;
        // The ONLY unresolved-helper case that refuses is Required + Landlock.
        assert!(confine_unresolved_helper_refuses(Required, true));
        // Required with seccomp-only (Landlock off) must NOT refuse — seccomp is
        // never the fail-closed basis; it degrades to bwrap-only best-effort.
        assert!(!confine_unresolved_helper_refuses(Required, false));
        // Auto never refuses regardless of which layer was requested.
        assert!(!confine_unresolved_helper_refuses(Auto, true));
        assert!(!confine_unresolved_helper_refuses(Auto, false));
        // Off never reaches a backend, but the predicate must still be benign.
        assert!(!confine_unresolved_helper_refuses(Off, true));
        assert!(!confine_unresolved_helper_refuses(Off, false));
    }

    #[test]
    fn landlock_degradation_marker_contract() {
        use LandlockEnforcement::*;
        // Every Auto-degradation message carries the marker, so the parent
        // (`goal.rs`) re-surfaces it on a passing check.
        for e in [Partial, None, Error] {
            let msg = confine_degraded_message(e);
            assert!(
                stderr_signals_landlock_degraded(&msg),
                "degraded message must carry the marker: {msg}"
            );
            let level = format!("{e:?}");
            assert!(msg.contains(level.as_str()), "degraded message must name the level: {msg}");
        }
        // The Required REFUSAL message must NOT carry the marker — a fail-closed
        // refusal is a Fail, never a best-effort degradation, so it must never
        // trip the parent's best-effort warning even though both mention Landlock.
        for e in [Partial, None, Error] {
            let msg = confine_refused_message(e);
            assert!(
                !stderr_signals_landlock_degraded(&msg),
                "refusal message must NOT carry the degradation marker: {msg}"
            );
        }
        // Arbitrary check output never trips it.
        assert!(!stderr_signals_landlock_degraded(""));
        assert!(!stderr_signals_landlock_degraded("error[E0382]: borrow of moved value"));
    }

    #[test]
    fn confine_helper_argv_has_exact_shape() {
        let nv = Path::new("/usr/local/bin/nv");
        let roots = [PathBuf::from("/work"), PathBuf::from("/tmp/priv")];
        let check = ["cargo".to_string(), "test".to_string()];
        // Landlock only: `--landlock` flag, then roots (roots ride Landlock).
        let argv = build_confine_helper_argv(nv, SandboxMode::Required, true, false, &roots, &check);
        assert_eq!(
            argv,
            vec![
                "/usr/local/bin/nv",
                "__nv-confine",
                "--mode",
                "required",
                "--landlock",
                "--root",
                "/work",
                "--root",
                "/tmp/priv",
                "--",
                "cargo",
                "test",
            ]
        );
    }

    #[test]
    fn confine_helper_argv_seccomp_only_omits_roots() {
        // Seccomp-only: `--seccomp` flag and NO `--root`/`--landlock` (roots are a
        // Landlock concept and are meaningless — so not emitted — without it).
        let nv = Path::new("/nv");
        let roots = [PathBuf::from("/work")];
        let check = ["true".to_string()];
        let argv = build_confine_helper_argv(nv, SandboxMode::Auto, false, true, &roots, &check);
        assert_eq!(
            argv,
            vec!["/nv", "__nv-confine", "--mode", "auto", "--seccomp", "--", "true"]
        );
        // Both layers: both flags, then roots.
        let both = build_confine_helper_argv(nv, SandboxMode::Required, true, true, &roots, &check);
        assert_eq!(
            both,
            vec![
                "/nv",
                "__nv-confine",
                "--mode",
                "required",
                "--landlock",
                "--seccomp",
                "--root",
                "/work",
                "--",
                "true",
            ]
        );
    }

    #[test]
    fn confine_args_round_trip_all_layer_combinations() {
        let nv = Path::new("/nv");
        for (mode, required) in [(SandboxMode::Required, true), (SandboxMode::Auto, false)] {
            for landlock in [false, true] {
                for seccomp in [false, true] {
                    let roots = [PathBuf::from("/a"), PathBuf::from("/b")];
                    let check = ["sh".to_string(), "-c".to_string(), "true".to_string()];
                    let argv =
                        build_confine_helper_argv(nv, mode, landlock, seccomp, &roots, &check);
                    // The helper consumes argv AFTER `nv __nv-confine`.
                    assert_eq!(argv[0], "/nv");
                    assert_eq!(argv[1], CONFINE_HELPER_TOKEN);
                    let spec = parse_confine_args(&argv[2..]).expect("round-trip must parse");
                    // Roots only round-trip when Landlock is on (they're not
                    // emitted otherwise).
                    let expected_roots = if landlock { roots.to_vec() } else { Vec::new() };
                    assert_eq!(
                        spec,
                        ConfineSpec {
                            required,
                            landlock,
                            seccomp,
                            roots: expected_roots,
                            check_cmd: check.to_vec(),
                        },
                        "mode={mode:?} landlock={landlock} seccomp={seccomp}"
                    );
                }
            }
        }
    }

    #[test]
    fn confine_args_preserve_dash_prefixed_check_program() {
        // Everything after `--` is the verbatim check, including a leading `-`,
        // so a `check[0]` like `--` is the command, never a helper flag.
        let nv = Path::new("/nv");
        let check = ["--".to_string(), "weird".to_string()];
        let argv = build_confine_helper_argv(nv, SandboxMode::Auto, true, true, &[], &check);
        let spec = parse_confine_args(&argv[2..]).unwrap();
        assert_eq!(spec.check_cmd, check.to_vec());
        assert!(!spec.required);
        assert!(spec.landlock);
        assert!(spec.seccomp);
        assert!(spec.roots.is_empty());
    }

    #[test]
    fn parse_confine_args_rejects_malformed() {
        let s = |xs: &[&str]| xs.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        // Missing `--mode`.
        assert!(parse_confine_args(&s(&["--root", "/a", "--", "true"])).is_none());
        // Unknown mode token.
        assert!(parse_confine_args(&s(&["--mode", "yolo", "--", "true"])).is_none());
        // Missing value for `--mode`.
        assert!(parse_confine_args(&s(&["--mode"])).is_none());
        // Missing value for `--root`.
        assert!(parse_confine_args(&s(&["--mode", "auto", "--root"])).is_none());
        // Empty root.
        assert!(parse_confine_args(&s(&["--mode", "auto", "--root", "", "--", "true"])).is_none());
        // Unknown flag.
        assert!(parse_confine_args(&s(&["--mode", "auto", "--bogus", "--", "true"])).is_none());
        // No `--` terminator.
        assert!(parse_confine_args(&s(&["--mode", "auto", "--root", "/a"])).is_none());
        // Empty check after `--`.
        assert!(parse_confine_args(&s(&["--mode", "auto", "--"])).is_none());
        // Duplicate `--mode`.
        assert!(
            parse_confine_args(&s(&["--mode", "auto", "--mode", "required", "--", "true"]))
                .is_none()
        );
        // Duplicate `--landlock` / `--seccomp` (fail closed rather than guess).
        assert!(
            parse_confine_args(&s(&["--mode", "auto", "--landlock", "--landlock", "--", "true"]))
                .is_none()
        );
        assert!(
            parse_confine_args(&s(&["--mode", "auto", "--seccomp", "--seccomp", "--", "true"]))
                .is_none()
        );
        // `--landlock` / `--seccomp` take NO value: a following token is parsed as
        // the next flag, and an unknown one still fails closed.
        assert!(
            parse_confine_args(&s(&["--mode", "auto", "--seccomp", "--bogus", "--", "true"]))
                .is_none()
        );
        // Well-formed with both layer flags parses (sanity floor for the negatives).
        assert!(
            parse_confine_args(&s(&["--mode", "required", "--landlock", "--seccomp", "--", "true"]))
                .is_some()
        );
    }

    /// On Linux, with Landlock requested and a helper path available, the bwrap
    /// argv must run the check THROUGH the helper (`… -- <nv> __nv-confine …
    /// -- <check>`); with Landlock off it must be byte-identical to the pre-H5
    /// argv (helper never spliced in). Verified at the arg-composition level
    /// (`bwrap_args` over the inner command), which needs no installed bwrap.
    #[cfg(target_os = "linux")]
    #[test]
    fn landlock_injection_wraps_check_only_when_enabled() {
        let cwd = Path::new("/work");
        let roots = [PathBuf::from("/work")];
        let check = ["cargo".to_string(), "test".to_string()];
        let nv = Path::new("/usr/bin/nv");

        // Landlock OFF: inner == check, byte-identical to the historical argv.
        let off = bwrap_args(&roots, cwd, &check, false);
        let off_tail = &off[off.iter().position(|a| a == "--").unwrap() + 1..];
        assert_eq!(off_tail, &check, "landlock off must not alter the inner argv");

        // Landlock ON: inner == the helper segment wrapping the check.
        let inner = build_confine_helper_argv(nv, SandboxMode::Required, true, false, &roots, &check);
        let on = bwrap_args(&roots, cwd, &inner, false);
        let on_tail = &on[on.iter().position(|a| a == "--").unwrap() + 1..];
        assert_eq!(on_tail[0], "/usr/bin/nv");
        assert_eq!(on_tail[1], CONFINE_HELPER_TOKEN);
        // The real check still terminates the argv, after the helper's own `--`.
        assert_eq!(&on_tail[on_tail.len() - 2..], &check);
    }

    /// H5 real-kernel proof (CI Linux; ties to H7). When Landlock is fully
    /// enforced on this kernel, an out-of-grant write must be DENIED while an
    /// in-grant write succeeds. Applied on a dedicated thread (Landlock restricts
    /// the calling thread + its children only, so it cannot leak into the test
    /// harness). SKIPPED, not failed, where Landlock is unavailable, so it never
    /// breaks a non-Landlock dev host or CI runner.
    #[cfg(target_os = "linux")]
    #[test]
    fn landlock_denies_out_of_grant_write_real_kernel() {
        let grant = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let grant_path = grant.path().to_path_buf();
        let outside_path = outside.path().to_path_buf();

        let observed = std::thread::spawn(move || {
            let enforcement = apply_landlock_write_confinement(std::slice::from_ref(&grant_path));
            if enforcement != LandlockEnforcement::Full {
                return None; // Landlock unavailable on this kernel -> skip.
            }
            let in_ok = std::fs::write(grant_path.join("in.txt"), b"x").is_ok();
            let out_denied = std::fs::write(outside_path.join("out.txt"), b"x").is_err();
            Some((in_ok, out_denied))
        })
        .join()
        .unwrap();

        match observed {
            None => skip_or_require_real_kernel(
                "landlock_denies_out_of_grant_write_real_kernel",
                "Landlock not fully enforced on this kernel",
            ),
            Some((in_ok, out_denied)) => {
                assert!(in_ok, "in-grant write must succeed under Landlock");
                assert!(out_denied, "out-of-grant write must be DENIED by Landlock");
            }
        }
    }

    #[test]
    fn seccomp_denylist_contents_are_sane() {
        // Non-empty, no duplicates, and contains the canonical escape primitives.
        assert!(!SECCOMP_DENYLIST.is_empty());
        let mut seen = std::collections::HashSet::new();
        for name in SECCOMP_DENYLIST {
            assert!(seen.insert(*name), "duplicate entry in SECCOMP_DENYLIST: {name}");
        }
        for must in ["mount", "ptrace", "bpf", "unshare", "setns", "keyctl"] {
            assert!(SECCOMP_DENYLIST.contains(&must), "denylist must include {must}");
        }
        // It must NOT deny syscalls a normal build/test workload depends on —
        // denying these would break every check (false positives that defeat the
        // opt-in) and is exactly the over-reach the denylist is documented to
        // avoid (no clone-flag filtering, processes must spawn).
        for must_not in [
            "clone", "clone3", "execve", "execveat", "openat", "read", "write", "mmap", "futex",
            "fork", "vfork",
        ] {
            assert!(
                !SECCOMP_DENYLIST.contains(&must_not),
                "denylist must NOT include the normal-workload syscall {must_not}"
            );
        }
    }

    #[test]
    fn seccomp_degraded_marker_contract() {
        // The seccomp degradation message carries its marker, so the parent
        // (`goal.rs`) re-surfaces it even on a passing check. Seccomp is always
        // best-effort, so — unlike Landlock — there is no refusal counterpart.
        let msg = confine_seccomp_degraded_message();
        assert!(
            stderr_signals_seccomp_degraded(&msg),
            "seccomp degraded message must carry its marker: {msg}"
        );
        // The two layers' markers are independent channels: a seccomp message must
        // not trip the Landlock scan and vice-versa, so the parent attributes the
        // right warning to the right layer.
        assert!(
            !stderr_signals_landlock_degraded(&msg),
            "seccomp message must not trip the Landlock scan: {msg}"
        );
        use LandlockEnforcement::*;
        for e in [Partial, None, Error] {
            assert!(!stderr_signals_seccomp_degraded(&confine_degraded_message(e)));
            assert!(!stderr_signals_seccomp_degraded(&confine_refused_message(e)));
        }
        // Arbitrary check output never trips it.
        assert!(!stderr_signals_seccomp_degraded(""));
        assert!(!stderr_signals_seccomp_degraded("error[E0382]: borrow of moved value"));
    }

    /// Every name in the platform-independent denylist must map to a real syscall
    /// number on this Linux build — catches a typo or a renamed `libc::SYS_*`
    /// constant at build time, and proves the filter actually compiles.
    #[cfg(target_os = "linux")]
    #[test]
    fn seccomp_denylist_all_names_resolve_and_compile() {
        for name in SECCOMP_DENYLIST {
            assert!(
                seccomp_syscall_number(name).is_some(),
                "denylist name has no libc::SYS_* on this arch: {name}"
            );
        }
        // A clearly-bogus name does NOT resolve (guards the match fall-through).
        assert!(seccomp_syscall_number("definitely_not_a_syscall").is_none());
        // The compiled BPF program builds (numbers + arch valid).
        assert!(
            build_seccomp_denylist_program().is_ok(),
            "the denylist must compile into a BPF program"
        );
    }

    /// H6 real-kernel proof (CI Linux; ties to H7). With the denylist installed, a
    /// DENIED syscall (`mount`) must terminate the process by `SIGSYS` while a
    /// workload using only ALLOWED syscalls runs to a clean exit. The filter is
    /// built in the parent (that step allocates) and installed in forked children
    /// that then issue only async-signal-safe syscalls — so applying a
    /// process-killing filter never takes down the test runner. SKIPPED (not
    /// failed) where seccomp filtering is unavailable, so it never breaks an
    /// incapable runner.
    #[cfg(target_os = "linux")]
    #[test]
    fn seccomp_denylist_kills_denied_syscall_but_allows_normal_real_kernel() {
        let program = match build_seccomp_denylist_program() {
            Ok(program) => program,
            Err(()) => {
                return skip_or_require_real_kernel(
                    "seccomp_denylist_kills_denied_syscall_but_allows_normal_real_kernel",
                    "the seccomp denylist did not compile on this build",
                );
            }
        };

        // --- Child A: install the filter, then attempt a DENIED syscall (mount).
        // Exit 101 = filter could not be installed (skip); exit 102 = reached only
        // if mount was NOT blocked (it must instead be killed by SIGSYS).
        let pid_a = unsafe { libc::fork() };
        assert!(pid_a >= 0, "fork failed");
        if pid_a == 0 {
            // CHILD — async-signal-safe only: no allocation, no panic, no stdio.
            if seccompiler::apply_filter_all_threads(&program).is_err() {
                unsafe { libc::_exit(101) };
            }
            unsafe {
                libc::syscall(
                    libc::SYS_mount,
                    0 as libc::c_long,
                    0 as libc::c_long,
                    0 as libc::c_long,
                    0 as libc::c_long,
                    0 as libc::c_long,
                );
                libc::_exit(102);
            }
        }
        let mut status_a: libc::c_int = 0;
        unsafe { libc::waitpid(pid_a, &mut status_a, 0) };

        // --- Child B: install the filter, then issue only ALLOWED syscalls.
        let pid_b = unsafe { libc::fork() };
        assert!(pid_b >= 0, "fork failed");
        if pid_b == 0 {
            if seccompiler::apply_filter_all_threads(&program).is_err() {
                unsafe { libc::_exit(101) };
            }
            unsafe {
                libc::syscall(libc::SYS_getpid);
                // close a bogus fd: returns EBADF, but the syscall is ALLOWED so
                // the process is not killed.
                libc::syscall(libc::SYS_close, 999 as libc::c_long);
                libc::_exit(0);
            }
        }
        let mut status_b: libc::c_int = 0;
        unsafe { libc::waitpid(pid_b, &mut status_b, 0) };

        // If either child could not install the filter, this kernel lacks usable
        // seccomp filtering -> skip rather than fail.
        let install_failed = |status: libc::c_int| {
            libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 101
        };
        if install_failed(status_a) || install_failed(status_b) {
            return skip_or_require_real_kernel(
                "seccomp_denylist_kills_denied_syscall_but_allows_normal_real_kernel",
                "the seccomp filter could not be installed on this kernel",
            );
        }

        // The denied syscall must have KILLED the process via SIGSYS (KILL_PROCESS,
        // or KILL_THREAD on pre-4.14 kernels — both terminate this single-threaded
        // child with SIGSYS). It must NOT have exited 102 (mount having run).
        assert!(
            libc::WIFSIGNALED(status_a),
            "a denied syscall must terminate the process by signal; status={status_a:#x}"
        );
        assert_eq!(
            libc::WTERMSIG(status_a),
            libc::SIGSYS,
            "a denied syscall must be killed with SIGSYS"
        );
        // The normal workload must run unaffected.
        assert!(
            libc::WIFEXITED(status_b) && libc::WEXITSTATUS(status_b) == 0,
            "a workload using only allowed syscalls must exit cleanly; status={status_b:#x}"
        );
    }

    /// Skip/require gate shared by ALL the Linux real-kernel proofs (H5 Landlock,
    /// H6 seccomp, H7 bwrap). On a dev host (env unset) a missing-support condition
    /// SKIPS (prints, returns) so it never false-fails a workstation lacking the
    /// relevant kernel feature (bwrap/user-namespaces, Landlock, or seccomp
    /// filtering). In CI we set `NERVE_CI_REAL_KERNEL=1` on a runner where those
    /// features are present, which turns the same condition into a PANIC —
    /// otherwise a confinement proof could silently green-skip and prove nothing.
    /// Routing every proof's skip through this one gate is what makes the CI claim
    /// ("the Linux real-kernel proofs actually run") true for all of them, not just
    /// the bwrap proof.
    #[cfg(target_os = "linux")]
    fn skip_or_require_real_kernel(test: &str, reason: &str) {
        if std::env::var_os("NERVE_CI_REAL_KERNEL").is_some_and(|v| !v.is_empty()) {
            panic!(
                "NERVE_CI_REAL_KERNEL is set but the real-kernel confinement proof `{test}` could not run: {reason}"
            );
        }
        eprintln!("SKIP {test}: {reason}");
    }

    /// H7 real-kernel proof (CI Linux; the Linux mirror of
    /// [`seatbelt_profile_denies_direct_out_of_root_write`]). Runs a canary
    /// through the PRODUCTION decision path (`decide` → `bwrap_decide` →
    /// `bwrap_args`, resolving a trusted `bwrap`) under `Required` and asserts the
    /// jail actually confines on a real kernel:
    ///   * a write INSIDE the only writable root succeeds (positive control — also
    ///     proves bwrap launched the child at all),
    ///   * a write OUTSIDE every writable root is DENIED (the host `/` is bound
    ///     read-only),
    ///   * the child's network namespace is UNSHARED — a different `net:[inode]`
    ///     than the host (`--unshare-net`).
    ///
    /// This is the bwrap-LEVEL proof (no Landlock/seccomp helper spliced in; H5/H6
    /// prove their own layers' real-kernel behaviour). Skip-vs-fail discipline: if
    /// `decide` does not return `Wrap` (no trusted bwrap) or the positive-control
    /// write does not land (bwrap could not create an unprivileged user
    /// namespace), the proof SKIPS — it never false-fails a host without
    /// bwrap/userns. But if bwrap DID launch the child and the out-of-root write
    /// nonetheless succeeds, that is a real confinement regression and the test
    /// FAILS loudly. In CI, `NERVE_CI_REAL_KERNEL` converts the "support missing"
    /// skip into a hard failure so the CI proof can never silently green-skip.
    #[cfg(target_os = "linux")]
    #[test]
    fn bwrap_confines_writes_and_unshares_network_real_kernel() {
        use std::process::Command as StdCommand;

        const TEST: &str = "bwrap_confines_writes_and_unshares_network_real_kernel";

        let work = tempfile::tempdir().unwrap();
        let sibling = tempfile::tempdir().unwrap();
        // Canonicalize so the rw-bind path matches the kernel's symlink-resolved
        // view (mirrors the macOS proof).
        let work = std::fs::canonicalize(work.path()).unwrap();
        let sibling = std::fs::canonicalize(sibling.path()).unwrap();

        // Production decision path: Required, no network, no Landlock/seccomp
        // helper — the bwrap-level proof.
        let config = cfg(SandboxMode::Required, false);

        // Build the bwrap argv for `inner` via `decide`: the wrapper program+args,
        // or `None` when no trusted bwrap is resolvable.
        let wrap = |inner: &[String]| -> Option<(String, Vec<String>)> {
            match decide(&config, &work, inner, &[], None) {
                SandboxDecision::Wrap { program, args } => Some((program, args)),
                _ => None,
            }
        };

        if wrap(&["true".to_string()]).is_none() {
            return skip_or_require_real_kernel(
                TEST,
                "no trusted bwrap binary is installed (decide did not return Wrap)",
            );
        }

        let run_status = |inner: &[String]| {
            let (program, args) = wrap(inner).expect("wrap resolved above");
            let _ = StdCommand::new(program).args(args).status();
        };

        // Positive control: a write inside the writable root. If it does not land,
        // bwrap could not set up the namespace on this kernel -> skip (or, in CI,
        // fail because the proof did not actually run).
        let ok = work.join("ok.txt");
        run_status(&[
            "/bin/sh".to_string(),
            "-c".to_string(),
            format!("echo hi > {}", ok.display()),
        ]);
        if !ok.exists() {
            return skip_or_require_real_kernel(
                TEST,
                "bwrap could not launch a confined child (unprivileged user namespaces disabled?)",
            );
        }

        // Confinement proof #1: a write OUTSIDE every writable root must be denied
        // by the read-only host bind.
        let escape = sibling.join("escape.txt");
        run_status(&[
            "/bin/sh".to_string(),
            "-c".to_string(),
            format!("echo hi > {}", escape.display()),
        ]);
        assert!(
            !escape.exists(),
            "out-of-root write must be DENIED inside the bwrap jail; the host filesystem is bound read-only"
        );

        // Confinement proof #2: the network namespace must be unshared. The child
        // sees a different `net:[inode]` than the host (`--unshare-net`). /proc is
        // freshly mounted in the jail, so /proc/self/ns/net is the child's. If the
        // HOST's own `/proc/self/ns/net` is unreadable (a kernel without namespace
        // support — never a normal CI runner), route that support gap through the
        // shared gate too, so this sub-proof can't silently no-op under
        // `NERVE_CI_REAL_KERNEL` (it would otherwise quietly skip the netns check).
        let host_ns = match std::fs::read_link("/proc/self/ns/net") {
            Ok(ns) => ns,
            Err(_) => {
                return skip_or_require_real_kernel(
                    TEST,
                    "host /proc/self/ns/net is unreadable (kernel without namespace support?)",
                );
            }
        };
        let (program, args) = wrap(&[
            "/bin/sh".to_string(),
            "-c".to_string(),
            "readlink /proc/self/ns/net".to_string(),
        ])
        .expect("wrap resolved above");
        let out = StdCommand::new(program).args(args).output().unwrap();
        let child_ns = String::from_utf8_lossy(&out.stdout);
        let child_ns = child_ns.trim();
        assert!(
            !child_ns.is_empty(),
            "could not read the child's network namespace; stderr={}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_ne!(
            child_ns,
            host_ns.to_string_lossy().trim(),
            "the child's network namespace must be UNSHARED (a different inode from the host)"
        );
    }
}
