//! sec-gap-5 / H15: parent-level setrlimit helpers applied before spawning a
//! /goal `check_cmd` child. Unix-only; non-unix callers receive `Unsupported`.
//! Linux honours every limit; macOS supports AS / FSIZE / CPU and rejects
//! NPROC for unprivileged callers — that gap is now reported honestly via
//! [`unenforced_notes`] rather than silently returning `Ok`. On Linux these
//! per-process ceilings are COMPLEMENTED — not replaced — by aggregate
//! cgroup v2 caps (`pids.max`/`memory.max`); see [`crate::cgroup`].

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Configurable per-`check_cmd` resource ceilings. All fields are optional;
/// omitted fields fall through to the parent process limits. Lives in
/// `nerve-core` for v0.3.0 to avoid cross-crate churn while nerve-config is
/// being extended in parallel for `/goal` natural-language ingest.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(deny_unknown_fields)]
pub struct CheckUlimit {
    /// RLIMIT_NPROC — max user processes (Linux primary, macOS best-effort).
    #[serde(default)]
    pub nproc: Option<u64>,
    /// RLIMIT_AS — max virtual address space, bytes.
    #[serde(default)]
    pub address_space_bytes: Option<u64>,
    /// RLIMIT_FSIZE — max file size the process can create, bytes.
    #[serde(default)]
    pub file_size_bytes: Option<u64>,
    /// RLIMIT_CPU — max CPU seconds.
    #[serde(default)]
    pub cpu_secs: Option<u64>,
}

impl CheckUlimit {
    pub fn is_empty(&self) -> bool {
        self.nproc.is_none()
            && self.address_space_bytes.is_none()
            && self.file_size_bytes.is_none()
            && self.cpu_secs.is_none()
    }
}

#[derive(Debug, Error)]
pub enum UlimitError {
    #[error("check_ulimit field `{field}` must be > 0")]
    ZeroLimit { field: &'static str },
    #[error("setrlimit({resource}) failed: errno {errno}")]
    SetRlimit { resource: &'static str, errno: i32 },
    #[error("check_ulimit is not supported on this platform")]
    Unsupported,
}

/// Sanity check the spec before runtime: reject zero values (would forbid
/// everything) and otherwise accept. Called by both the constructor and
/// `apply_ulimit` directly so callers get the same error either way.
pub fn validate(spec: &CheckUlimit) -> Result<(), UlimitError> {
    if spec.nproc == Some(0) {
        return Err(UlimitError::ZeroLimit { field: "nproc" });
    }
    if spec.address_space_bytes == Some(0) {
        return Err(UlimitError::ZeroLimit {
            field: "address_space_bytes",
        });
    }
    if spec.file_size_bytes == Some(0) {
        return Err(UlimitError::ZeroLimit {
            field: "file_size_bytes",
        });
    }
    if spec.cpu_secs == Some(0) {
        return Err(UlimitError::ZeroLimit { field: "cpu_secs" });
    }
    Ok(())
}

/// Apply each configured limit via `setrlimit(2)`. Safe to call from a
/// `Command::pre_exec` hook: no allocator / lock acquisition is performed.
#[cfg(unix)]
pub fn apply_ulimit(spec: &CheckUlimit) -> Result<(), UlimitError> {
    validate(spec)?;

    if let Some(value) = spec.nproc {
        set_resource(
            libc::RLIMIT_NPROC,
            "nproc",
            value,
            /*macos_best_effort=*/ true,
        )?;
    }
    if let Some(value) = spec.address_space_bytes {
        set_resource(libc::RLIMIT_AS, "address_space_bytes", value, false)?;
    }
    if let Some(value) = spec.file_size_bytes {
        set_resource(libc::RLIMIT_FSIZE, "file_size_bytes", value, false)?;
    }
    if let Some(value) = spec.cpu_secs {
        set_resource(libc::RLIMIT_CPU, "cpu_secs", value, false)?;
    }

    Ok(())
}

#[cfg(not(unix))]
pub fn apply_ulimit(_spec: &CheckUlimit) -> Result<(), UlimitError> {
    Err(UlimitError::Unsupported)
}

/// Parent-side (pre-spawn) advisory — NOT for use inside `pre_exec`. Returns the
/// config field names of limits this platform will silently fail to enforce, so
/// the caller can surface them rather than let the child degrade unseen (H15).
///
/// macOS rejects `RLIMIT_NPROC` for unprivileged callers, so [`apply_ulimit`]
/// best-effort-ignores it (returns `Ok` instead of aborting the spawn); without
/// this report an operator would wrongly believe `nproc` is enforced.
/// `RLIMIT_AS`/`FSIZE`/`CPU` are honored on macOS and never listed. On Linux and
/// other unix every `setrlimit` is honored, so this is always empty.
#[cfg(unix)]
pub fn unenforced_notes(spec: &CheckUlimit) -> Vec<&'static str> {
    let mut notes = Vec::new();
    if cfg!(target_os = "macos") && spec.nproc.is_some() {
        notes.push("nproc");
    }
    notes
}

/// Non-unix has no setrlimit at all; the whole spec is unsupported (the caller
/// already errors via `apply_ulimit`), so there is nothing partial to report.
#[cfg(not(unix))]
pub fn unenforced_notes(_spec: &CheckUlimit) -> Vec<&'static str> {
    Vec::new()
}

#[cfg(unix)]
fn set_resource(
    resource: RlimitResource,
    label: &'static str,
    value: u64,
    macos_best_effort: bool,
) -> Result<(), UlimitError> {
    let limit = libc::rlimit {
        rlim_cur: clamp_to_rlim(value),
        rlim_max: clamp_to_rlim(value),
    };
    // SAFETY: libc::setrlimit needs a valid pointer for the duration of the
    // call; `&limit` is. The struct fields are POD.
    let rc = unsafe { libc::setrlimit(resource, &limit) };
    if rc == 0 {
        return Ok(());
    }
    if macos_best_effort && cfg!(target_os = "macos") {
        // macOS rejects RLIMIT_NPROC for unprivileged callers. We degrade to
        // best-effort here (rather than failing the whole spawn), but this is no
        // longer SILENT: the parent surfaces it pre-spawn via `unenforced_notes`
        // so the operator is not left believing nproc is enforced (H15).
        return Ok(());
    }
    Err(UlimitError::SetRlimit {
        resource: label,
        errno: errno(),
    })
}

#[cfg(all(
    unix,
    target_os = "linux",
    any(target_env = "gnu", target_env = "uclibc")
))]
type RlimitResource = libc::__rlimit_resource_t;

#[cfg(all(
    unix,
    not(all(target_os = "linux", any(target_env = "gnu", target_env = "uclibc")))
))]
type RlimitResource = libc::c_int;

#[cfg(unix)]
fn clamp_to_rlim(value: u64) -> libc::rlim_t {
    if (value as u128) > (libc::rlim_t::MAX as u128) {
        libc::rlim_t::MAX
    } else {
        value as libc::rlim_t
    }
}

#[cfg(unix)]
fn errno() -> i32 {
    // SAFETY: errno_location returns a pointer to the per-thread errno
    // variable, which is always valid to read.
    unsafe { *errno_location() }
}

#[cfg(all(unix, target_os = "linux"))]
unsafe fn errno_location() -> *mut i32 {
    unsafe { libc::__errno_location() }
}

#[cfg(all(unix, any(target_os = "macos", target_os = "ios")))]
unsafe fn errno_location() -> *mut i32 {
    unsafe { libc::__error() }
}

#[cfg(all(
    unix,
    not(any(target_os = "linux", target_os = "macos", target_os = "ios"))
))]
unsafe fn errno_location() -> *mut i32 {
    unsafe { libc::__error() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_rejects_zero_nproc() {
        let spec = CheckUlimit {
            nproc: Some(0),
            ..Default::default()
        };
        let err = validate(&spec).unwrap_err();
        assert!(matches!(err, UlimitError::ZeroLimit { field: "nproc" }));
    }

    #[test]
    fn validate_rejects_zero_address_space() {
        let spec = CheckUlimit {
            address_space_bytes: Some(0),
            ..Default::default()
        };
        let err = validate(&spec).unwrap_err();
        assert!(matches!(
            err,
            UlimitError::ZeroLimit {
                field: "address_space_bytes"
            }
        ));
    }

    #[test]
    fn validate_rejects_zero_file_size() {
        let spec = CheckUlimit {
            file_size_bytes: Some(0),
            ..Default::default()
        };
        let err = validate(&spec).unwrap_err();
        assert!(matches!(
            err,
            UlimitError::ZeroLimit {
                field: "file_size_bytes"
            }
        ));
    }

    #[test]
    fn validate_rejects_zero_cpu_secs() {
        let spec = CheckUlimit {
            cpu_secs: Some(0),
            ..Default::default()
        };
        let err = validate(&spec).unwrap_err();
        assert!(matches!(err, UlimitError::ZeroLimit { field: "cpu_secs" }));
    }

    #[test]
    fn validate_accepts_empty_spec() {
        validate(&CheckUlimit::default()).unwrap();
    }

    #[test]
    #[cfg(unix)]
    fn unenforced_notes_reports_macos_nproc_only() {
        // On macOS, a configured `nproc` is surfaced as unenforced (RLIMIT_NPROC
        // is rejected for unprivileged callers); on Linux/other unix it is
        // honored and nothing is reported.
        let with_nproc = CheckUlimit {
            nproc: Some(8),
            address_space_bytes: Some(1 << 30),
            ..Default::default()
        };
        let notes = unenforced_notes(&with_nproc);
        if cfg!(target_os = "macos") {
            assert_eq!(notes, vec!["nproc"]);
        } else {
            assert!(notes.is_empty(), "linux/unix honors nproc: {notes:?}");
        }

        // AS/FSIZE/CPU are honored everywhere on unix — never reported, even on
        // macOS, and an nproc-free spec yields no notes on any platform.
        let no_nproc = CheckUlimit {
            address_space_bytes: Some(1 << 30),
            file_size_bytes: Some(1024),
            cpu_secs: Some(10),
            ..Default::default()
        };
        assert!(unenforced_notes(&no_nproc).is_empty());
    }

    #[test]
    fn ulimit_validates_zero() {
        // Spec callout: nerve-config validate also rejects zero, but the
        // evaluator constructor calls into us as a second line of defence.
        let spec = CheckUlimit {
            nproc: Some(0),
            address_space_bytes: Some(1),
            file_size_bytes: Some(1),
            cpu_secs: Some(1),
        };
        assert!(matches!(
            validate(&spec),
            Err(UlimitError::ZeroLimit { field: "nproc" })
        ));
    }
}
