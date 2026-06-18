//! H15: Linux cgroups v2 per-check resource enforcement.
//!
//! This is **additive** to the per-process `setrlimit(2)` ceilings in
//! [`crate::ulimit`]. setrlimit caps each *individual* process; a fork bomb
//! whose children each stay under the per-process `RLIMIT_NPROC` evades it.
//! A cgroup v2 `pids.max` is an *aggregate* cap over the whole check subtree,
//! so it resists fork bombs; `memory.max` is an aggregate **memory-usage**
//! ceiling (anon + page cache) over the subtree that complements the per-process
//! virtual-address-space `RLIMIT_AS`. cgroup v2 governs *swap* through a separate
//! controller (`memory.swap.max`) which H15 does **not** set — so `memory.max`
//! bounds in-memory usage, not swap; we say so rather than overclaim a combined
//! RSS+swap cap.
//!
//! ## Honest scope / mapping
//! - `nproc`               → cgroup `pids.max`  (aggregate; the fork-bomb cap) **and** `RLIMIT_NPROC`.
//! - `address_space_bytes` → cgroup `memory.max` (aggregate memory usage, *not*
//!   swap) **and** `RLIMIT_AS`.
//! - `cpu_secs`            → `RLIMIT_CPU` only. cgroup `cpu.max` is a *rate*
//!   (quota/period), not a cumulative CPU-second budget, so mapping `cpu_secs`
//!   onto it would change its meaning — we do not, and say so rather than
//!   overclaim.
//! - `file_size_bytes`     → `RLIMIT_FSIZE` only (no cgroup equivalent).
//!
//! ## Opt-in & graceful degradation (never silent)
//! cgroup v2 enforcement requires a *delegated*, controller-enabled cgroup the
//! unprivileged `nv` process may create children under. We do **not** guess at
//! systemd slice paths; the operator opts in by pointing
//! [`CGROUP_PARENT_ENV`] at such a base. When it is unset we are inert (the
//! existing setrlimit path is unchanged — additive-when-off). When it is set
//! but unusable (not a cgroup2 dir, controllers not delegated, …) we DEGRADE to
//! setrlimit-only and the caller surfaces a `note` — we never silently pretend
//! the aggregate caps applied, and we never fail the check toward acceptance
//! over a resource-enforcement gap (that is fail-safe: a missing *limit* only
//! makes the check *less* constrained, it can never fabricate a pass).
//!
//! This module is compiled on Linux only; [`crate::goal`] gates its use behind
//! `#[cfg(target_os = "linux")]` and other platforms never reference it.

use std::fs;
use std::io;
use std::os::unix::io::{AsRawFd, RawFd};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::ulimit::CheckUlimit;

/// Operator opt-in: a delegated, controller-enabled cgroup v2 directory under
/// which `nv` may create one leaf cgroup per check. Empty/unset ⇒ inert.
pub const CGROUP_PARENT_ENV: &str = "NERVE_CGROUP_PARENT";

/// Monotonic suffix so concurrent checks (e.g. mayor patrol) never collide on a
/// cgroup name. Combined with the parent PID it is unique without needing a
/// clock or RNG.
static SEQ: AtomicU64 = AtomicU64::new(0);

/// Outcome of trying to set up a per-check cgroup before spawning the child.
pub enum CgroupSetup {
    /// A per-check cgroup was created and configured. The guard owns the open
    /// `cgroup.procs` fd (the child joins via `pre_exec`) and tears the cgroup
    /// down on drop.
    Active(CgroupGuard),
    /// No cgroup was set up. `note` is `Some` only when the operator opted in
    /// (env set) but the base was unusable — the caller MUST surface it. It is
    /// `None` when cgroup enforcement was simply not requested or the spec maps
    /// no cgroup limit, in which case there is nothing to say (inert).
    Inactive { note: Option<String> },
}

/// RAII handle for one per-check cgroup. Dropping it kills any lingering
/// processes in the cgroup (fork-bomb stragglers) and removes the directory, so
/// every spawn path — success, timeout, output-cap, error — cleans up.
pub struct CgroupGuard {
    dir: PathBuf,
    /// Held open so the raw fd handed to `pre_exec` stays valid through spawn.
    /// `CLOEXEC` is set so the exec'd program does not inherit it.
    procs: Option<fs::File>,
}

impl CgroupGuard {
    /// The raw `cgroup.procs` fd the child writes its PID to in `pre_exec`.
    pub fn procs_fd(&self) -> RawFd {
        self.procs.as_ref().map_or(-1, |f| f.as_raw_fd())
    }

    fn teardown(&mut self) {
        // Drop our writable handle first so it is not itself counted/held.
        self.procs = None;

        // Kill the whole subtree. `cgroup.kill` (kernel ≥ 5.14) does it in one
        // write; the explicit SIGKILL loop is a belt-and-suspenders fallback for
        // older kernels and for any task that slipped in after the write.
        let _ = fs::write(self.dir.join("cgroup.kill"), "1");
        if let Ok(list) = fs::read_to_string(self.dir.join("cgroup.procs")) {
            for line in list.lines() {
                if let Ok(pid) = line.trim().parse::<i32>() {
                    // SAFETY: kill(2) with a parsed PID + SIGKILL is a plain
                    // syscall; an unknown/exited PID just yields ESRCH.
                    unsafe {
                        libc::kill(pid, libc::SIGKILL);
                    }
                }
            }
        }

        // rmdir refuses while the cgroup still has tasks; retry briefly while
        // the kernel reaps the killed processes. yield_now (not sleep) keeps us
        // from blocking the async worker; cleanup is best-effort by design.
        for _ in 0..100 {
            if fs::remove_dir(&self.dir).is_ok() {
                return;
            }
            std::thread::yield_now();
        }
        if let Err(e) = fs::remove_dir(&self.dir) {
            tracing::warn!(
                target: "nerve::cgroup",
                "failed to remove per-check cgroup {}: {e}",
                self.dir.display()
            );
        }
    }
}

impl Drop for CgroupGuard {
    fn drop(&mut self) {
        self.teardown();
    }
}

/// Production entry point: read the opt-in env and build a setup plan.
pub fn prepare(spec: &CheckUlimit) -> CgroupSetup {
    let base = std::env::var_os(CGROUP_PARENT_ENV).filter(|v| !v.is_empty());
    prepare_with_base(spec, base.as_deref())
}

/// Core logic with the base injected, so tests exercise every branch without
/// mutating the process-global environment (which would race parallel tests).
pub fn prepare_with_base(spec: &CheckUlimit, base: Option<&std::ffi::OsStr>) -> CgroupSetup {
    let pids = spec.nproc;
    let memory = spec.address_space_bytes;

    // Nothing the cgroup layer can enforce for this spec (cpu_secs / file_size
    // are setrlimit-only). Stay inert regardless of opt-in.
    if pids.is_none() && memory.is_none() {
        return CgroupSetup::Inactive { note: None };
    }

    // Not opted in ⇒ inert; the per-process setrlimit path still applies and we
    // do not nag operators who never asked for cgroup enforcement.
    let Some(base) = base else {
        return CgroupSetup::Inactive { note: None };
    };
    let base = PathBuf::from(base);

    // Opted in: from here, any failure DEGRADES with a surfaced note.
    match try_create(&base, pids, memory) {
        Ok(guard) => CgroupSetup::Active(guard),
        Err(e) => CgroupSetup::Inactive {
            note: Some(format!(
                "cgroup resource enforcement requested via {CGROUP_PARENT_ENV}={} but could not be set up ({e}); \
                 falling back to per-process setrlimit only — the aggregate pids/memory caps were NOT applied",
                base.display()
            )),
        },
    }
}

fn try_create(base: &Path, pids: Option<u64>, memory: Option<u64>) -> io::Result<CgroupGuard> {
    if !base.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("{} is not a directory", base.display()),
        ));
    }
    // Confirm the base really sits on a cgroup v2 filesystem BEFORE we write
    // anything, using statfs(2)'s magic — NOT the mere presence of a file named
    // `cgroup.controllers`, which any directory on any filesystem could fake and
    // thereby lure `create_dir`/`subtree_control`/`pids.max` writes (and a leaked
    // leaf dir `remove_dir` would not clean up) under an arbitrary path. A
    // misconfigured or hostile NERVE_CGROUP_PARENT (e.g. `/etc`, or a tmpfs dir
    // containing a fake `cgroup.controllers`) degrades here instead.
    if !is_cgroup2_fs(base) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "{} is not on a cgroup v2 filesystem (statfs magic mismatch)",
                base.display()
            ),
        ));
    }

    // Ask the base to delegate the controllers we need into its subtree. This is
    // best-effort: it may already be enabled, or not be permitted — either way
    // the authoritative check is whether the child's interface files appear
    // below, so we do not fail on this write.
    let mut want = String::new();
    if pids.is_some() {
        want.push_str("+pids ");
    }
    if memory.is_some() {
        want.push_str("+memory");
    }
    let _ = fs::write(base.join("cgroup.subtree_control"), want.trim());

    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let dir = base.join(format!("nerve-check-{}-{seq}", std::process::id()));
    fs::create_dir(&dir)?;

    // Configure the leaf; on any failure remove the half-made cgroup so we never
    // leak it. Writing `pids.max`/`memory.max` also *verifies* the controller is
    // delegated — if it is not, the interface file is absent and the write fails
    // with ENOENT, which we report as a degrade reason.
    let configured = (|| -> io::Result<()> {
        if let Some(p) = pids {
            fs::write(dir.join("pids.max"), p.to_string())?;
        }
        if let Some(m) = memory {
            fs::write(dir.join("memory.max"), m.to_string())?;
        }
        Ok(())
    })();
    if let Err(e) = configured {
        let _ = fs::remove_dir(&dir);
        return Err(e);
    }

    let procs = match fs::OpenOptions::new().write(true).open(dir.join("cgroup.procs")) {
        Ok(f) => f,
        Err(e) => {
            let _ = fs::remove_dir(&dir);
            return Err(e);
        }
    };
    set_cloexec(procs.as_raw_fd());

    Ok(CgroupGuard {
        dir,
        procs: Some(procs),
    })
}

fn set_cloexec(fd: RawFd) {
    // SAFETY: fcntl on a valid owned fd; we only OR in FD_CLOEXEC. Failure is
    // non-fatal (the fd just stays inheritable), so we ignore the result.
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFD);
        if flags >= 0 {
            libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC);
        }
    }
}

/// True iff `path` resides on a cgroup v2 filesystem (`statfs(2)` reports
/// `CGROUP2_SUPER_MAGIC`). This is the authoritative "is this really a cgroup
/// base" check — unlike a filename probe it cannot be spoofed by a regular
/// directory on another filesystem, so it gates out writes to arbitrary paths.
// `f_type` and `CGROUP2_SUPER_MAGIC` have target-dependent widths/signedness
// (`c_long` vs `c_uint` across gnu/musl/32-bit), so we normalize both to i64;
// on a target where they already match, one cast is redundant — allow it rather
// than special-case per target.
#[allow(clippy::unnecessary_cast)]
fn is_cgroup2_fs(path: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;
    let Ok(c_path) = std::ffi::CString::new(path.as_os_str().as_bytes()) else {
        return false;
    };
    // SAFETY: statfs writes into a buffer we own; `c_path` is a valid C string
    // for the duration of the call.
    let mut buf: libc::statfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statfs(c_path.as_ptr(), &mut buf) } != 0 {
        return false;
    }
    // f_type is `c_long` on some targets and `c_uint`/`c_ulong` on others;
    // compare as i64 so the magic matches regardless of the field's width/sign.
    buf.f_type as i64 == libc::CGROUP2_SUPER_MAGIC as i64
}

/// Move the *calling* process into the cgroup by writing its own PID to the
/// pre-opened `cgroup.procs` `fd`. Intended to run inside a `Command::pre_exec`
/// hook (child, post-fork, pre-exec), so it MUST be async-signal-safe: it calls
/// only `getpid(2)` and `write(2)` and formats the PID into a stack buffer with
/// no allocation or locking. Joining before `exec` is what makes the cap
/// race-free — the target program and every descendant start already confined.
pub fn join_via_fd(fd: RawFd) -> io::Result<()> {
    // SAFETY: getpid is async-signal-safe and always succeeds.
    let pid = unsafe { libc::getpid() };
    let mut buf = [0u8; 24];
    let bytes = format_pid(pid, &mut buf);

    let mut off = 0;
    while off < bytes.len() {
        // SAFETY: write(2) is async-signal-safe; the slice pointer/len are valid
        // for the call.
        let n = unsafe {
            libc::write(
                fd,
                bytes[off..].as_ptr() as *const libc::c_void,
                bytes.len() - off,
            )
        };
        if n < 0 {
            let err = io::Error::last_os_error();
            // A signal can interrupt the write before any progress; retry rather
            // than fail the join. (last_os_error reads errno; no allocation.)
            if err.raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            return Err(err);
        }
        if n == 0 {
            // Zero progress before the PID is fully written: the child may NOT be
            // in the cgroup. Fail the join (fail-safe) — never report success and
            // run effectively unconfined. WriteZero needs no allocation.
            return Err(io::Error::from(io::ErrorKind::WriteZero));
        }
        off += n as usize;
    }
    Ok(())
}

/// Async-signal-safe decimal formatting of a PID (with trailing newline) into a
/// caller-provided buffer; returns the populated tail slice. No allocation.
fn format_pid(pid: libc::pid_t, buf: &mut [u8; 24]) -> &[u8] {
    let mut n = if pid < 0 { 0u64 } else { pid as u64 };
    let mut i = buf.len();
    i -= 1;
    buf[i] = b'\n';
    if n == 0 {
        i -= 1;
        buf[i] = b'0';
        return &buf[i..];
    }
    while n > 0 {
        i -= 1;
        buf[i] = b'0' + (n % 10) as u8;
        n /= 10;
    }
    &buf[i..]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;

    #[test]
    fn format_pid_renders_decimal_with_newline() {
        let mut buf = [0u8; 24];
        assert_eq!(format_pid(0, &mut buf), b"0\n");
        let mut buf = [0u8; 24];
        assert_eq!(format_pid(1, &mut buf), b"1\n");
        let mut buf = [0u8; 24];
        assert_eq!(format_pid(123_456, &mut buf), b"123456\n");
        // Defensive: a negative PID is clamped to 0 rather than corrupting the
        // buffer (PIDs are positive in practice).
        let mut buf = [0u8; 24];
        assert_eq!(format_pid(-5, &mut buf), b"0\n");
    }

    #[test]
    fn inert_when_spec_maps_no_cgroup_limit() {
        // cpu_secs / file_size are setrlimit-only: even opted-in, the cgroup
        // layer has nothing to do and must not create anything or nag.
        let spec = CheckUlimit {
            cpu_secs: Some(10),
            file_size_bytes: Some(1024),
            ..Default::default()
        };
        match prepare_with_base(&spec, Some(OsStr::new("/sys/fs/cgroup"))) {
            CgroupSetup::Inactive { note: None } => {}
            CgroupSetup::Inactive { note: Some(n) } => panic!("expected silent inert, got note: {n}"),
            CgroupSetup::Active(_) => panic!("must not create a cgroup when no cgroup limit is set"),
        }
    }

    #[test]
    fn inert_when_not_opted_in() {
        // A cgroup-mappable spec but no NERVE_CGROUP_PARENT ⇒ inert, no note
        // (setrlimit still applies; we never nag operators who did not opt in).
        let spec = CheckUlimit {
            nproc: Some(8),
            ..Default::default()
        };
        match prepare_with_base(&spec, None) {
            CgroupSetup::Inactive { note: None } => {}
            other => panic!("expected silent inert when base is None, got {:?}", DebugSetup(&other)),
        }
    }

    #[test]
    fn degrades_with_surfaced_note_when_base_unusable() {
        // Opted in to a real directory that is NOT on a cgroup2 filesystem — and
        // which even contains a FAKE file literally named `cgroup.controllers` to
        // prove the guard is statfs(2) magic, not a spoofable filename. prepare
        // must DEGRADE with a surfaced note before writing/creating anything, and
        // must not leave any artifact behind under the bogus base.
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::write(tmp.path().join("cgroup.controllers"), "memory pids\n")
            .expect("write fake cgroup.controllers");
        let spec = CheckUlimit {
            nproc: Some(8),
            ..Default::default()
        };
        match prepare_with_base(&spec, Some(tmp.path().as_os_str())) {
            CgroupSetup::Inactive { note: Some(note) } => {
                assert!(
                    note.contains(CGROUP_PARENT_ENV) && note.contains("setrlimit"),
                    "degrade note must name the env var and the setrlimit fallback: {note}"
                );
                // Nothing must have been created or written: the only entry is the
                // fake controllers file we planted (no nerve-check-* leaf, and no
                // cgroup.subtree_control written by us).
                let mut entries: Vec<String> = std::fs::read_dir(tmp.path())
                    .expect("read tmp")
                    .filter_map(|e| e.ok())
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .collect();
                entries.sort();
                assert_eq!(
                    entries,
                    vec!["cgroup.controllers".to_string()],
                    "statfs guard must reject before any write/create — only the planted fake file should remain"
                );
            }
            other => panic!(
                "expected degrade-with-note for a non-cgroup base, got {:?}",
                DebugSetup(&other)
            ),
        }
    }

    /// Minimal debug shim so test panics can describe a `CgroupSetup` without
    /// requiring `Debug` on the (fd-owning) guard.
    struct DebugSetup<'a>(&'a CgroupSetup);
    impl std::fmt::Debug for DebugSetup<'_> {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            match self.0 {
                CgroupSetup::Active(_) => write!(f, "Active(..)"),
                CgroupSetup::Inactive { note } => write!(f, "Inactive {{ note: {note:?} }}"),
            }
        }
    }

    /// Real-kernel proof: under a delegated cgroup base, `pids.max` actually
    /// caps an aggregate fork attempt (the fork-bomb defense), proving the
    /// production `prepare → pre_exec join → exec` path confines on a live
    /// kernel.
    ///
    /// Gate: cgroup enforcement is opt-in, so the proof keys on the SAME signal
    /// production does — `NERVE_CGROUP_PARENT`. Unset (a dev host that did not
    /// arrange delegation) ⇒ SKIP, never a false failure. SET (CI arranges a
    /// delegated base and exports it) ⇒ the cgroup MUST be creatable: if
    /// `prepare` degrades we PANIC, because that means the opted-in delegation is
    /// misconfigured — exactly the silent-green-skip this gate exists to prevent.
    #[test]
    fn cgroup_pids_max_caps_fork_real_kernel() {
        let base = std::env::var_os(CGROUP_PARENT_ENV).filter(|v| !v.is_empty());
        let Some(base) = base else {
            eprintln!(
                "SKIP cgroup_pids_max_caps_fork_real_kernel: {CGROUP_PARENT_ENV} is not set \
                 (cgroup enforcement is opt-in; no delegated cgroup v2 base to test against)"
            );
            return;
        };

        // Cap the subtree at a small number of PIDs.
        const MAX_PIDS: u64 = 6;
        let spec = CheckUlimit {
            nproc: Some(MAX_PIDS),
            ..Default::default()
        };
        let guard = match prepare_with_base(&spec, Some(&base)) {
            CgroupSetup::Active(g) => g,
            CgroupSetup::Inactive { note } => panic!(
                "{CGROUP_PARENT_ENV}={base:?} is set but a per-check cgroup could not be created \
                 ({note:?}); cgroup delegation is misconfigured — the fork-bomb cap could not be proven"
            ),
        };

        // Spawn a child that joins the cgroup (pre_exec) then forks as many
        // long-lived children as it can, printing the count it achieved before
        // hitting EAGAIN. With pids.max = MAX_PIDS the aggregate count is capped
        // well below the attempted number — that is the fork-bomb defense.
        let fd = guard.procs_fd();
        let script = "n=0; while [ $n -lt 200 ]; do sleep 30 & n=$((n+1)); done; echo done";
        let mut cmd = std::process::Command::new("/bin/sh");
        cmd.args(["-c", script])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null());
        // SAFETY: join_via_fd is async-signal-safe (getpid + write only).
        unsafe {
            use std::os::unix::process::CommandExt;
            cmd.pre_exec(move || join_via_fd(fd));
        }
        let mut child = cmd.spawn().expect("spawn fork-bomb canary");

        // Give the shell a moment to fork up to the cap, then read pids.current:
        // it must never exceed pids.max. We poll the cgroup's own accounting
        // rather than trusting the child's self-report.
        let pids_current = guard.dir.join("pids.current");
        let pids_max = guard.dir.join("pids.max");
        assert_eq!(
            std::fs::read_to_string(&pids_max).unwrap().trim(),
            MAX_PIDS.to_string(),
            "pids.max should be written to the configured value"
        );
        let mut peak = 0u64;
        for _ in 0..200 {
            if let Ok(cur) = std::fs::read_to_string(&pids_current)
                && let Ok(v) = cur.trim().parse::<u64>()
            {
                peak = peak.max(v);
                assert!(
                    v <= MAX_PIDS,
                    "pids.current {v} exceeded pids.max {MAX_PIDS} — the aggregate cap did not hold"
                );
            }
            std::thread::yield_now();
        }
        assert!(
            peak >= 1,
            "the canary never appeared in the cgroup — pre_exec join did not take effect"
        );

        // Clean up the canary tree; the guard's Drop also kills+rmdirs.
        let _ = child.kill();
        let _ = child.wait();
        drop(guard);
    }
}
