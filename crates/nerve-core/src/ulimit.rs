//! sec-gap-5: parent-level setrlimit helpers applied before spawning a /goal
//! `check_cmd` child. v0.3.0 ships a unix-only implementation; non-unix
//! callers receive `Unsupported`. Linux honours every limit; macOS supports
//! AS / FSIZE / CPU and tolerates NPROC best-effort. v1.0 will replace this
//! with cgroups (Linux) per §3 Tier 2g.

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

#[cfg(unix)]
fn set_resource(
    resource: impl Into<libc::c_int>,
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
    let rc = unsafe { libc::setrlimit(resource.into() as _, &limit) };
    if rc == 0 {
        return Ok(());
    }
    if macos_best_effort && cfg!(target_os = "macos") {
        // macOS rejects RLIMIT_NPROC for unprivileged callers; degrade to
        // best-effort instead of failing the whole spawn.
        return Ok(());
    }
    Err(UlimitError::SetRlimit {
        resource: label,
        errno: errno(),
    })
}

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
