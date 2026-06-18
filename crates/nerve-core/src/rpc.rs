//! Tier 2e (v0.5.0) — RPC event-streaming bus for the synaptic runtime.
//!
//! The bus wraps a per-consumer bounded `tokio::sync::broadcast` channel and
//! enforces three orchestrator-wide guards from
//! `nerve-terminal-upgrade-proposal.md` §3 Tier 2e sec-4:
//!
//! 1. Per-consumer bounded queue (default 1024) so a single slow subscriber
//!    cannot starve the rest of the fan-out.
//! 2. Hard payload cap (default 64 KiB) — oversize payloads are truncated to
//!    a head/tail preview plus a `truncated: true` marker before being
//!    broadcast.
//! 3. 32-byte bearer token persisted at `RpcConfig::token_path` with mode
//!    `0600`. The token is created on startup, rotated on demand, and
//!    removed when the bus is shut down.
//!
//! Every emitted [`RpcEnvelope`] carries the workspace-wide
//! [`RPC_SCHEMA_VERSION`], a fresh ULID (`envelope_id`), and the emission
//! timestamp so consumers can deduplicate or order events.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::RwLock;
use std::sync::atomic::{AtomicUsize, Ordering};

use nerve_config::RpcConfig;
use nerve_types::RpcEnvelope;
use rand::RngCore;
use thiserror::Error;
use tokio::sync::broadcast;

/// Errors surfaced by [`RpcBus::new`], [`RpcBus::rotate_token`], and
/// [`RpcBus::shutdown`].
#[derive(Debug, Error)]
pub enum RpcError {
    /// Filesystem or permission error while reading, writing, or removing
    /// the bearer-token file.
    #[error("rpc token io error: {0}")]
    TokenIo(#[from] io::Error),
    /// Invalid [`RpcConfig`] value (e.g. zero-byte token size).
    #[error("invalid rpc config: {0}")]
    InvalidConfig(String),
}

/// Errors surfaced by [`RpcBus::emit`].
#[derive(Debug, Error)]
pub enum EmitError {
    /// JSON serialization of the payload failed before broadcast.
    #[error("rpc payload serialization failed: {0}")]
    Serialize(#[from] serde_json::Error),
}

/// Outcome of an [`RpcBus::emit`] call.
///
/// `dropped` reports the cumulative count of envelopes that broadcast
/// subscribers missed because their per-consumer queue lagged. The counter
/// is monotonic for the lifetime of the bus; consumers may sample it for
/// observability without resetting it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EmitOutcome {
    pub dropped: usize,
}

/// Per-runtime RPC bus.
///
/// Holds the broadcast sender, the bearer-token state, and the dropped-
/// envelope counter. Cloning is intentionally not implemented because
/// callers should share a single bus via `Arc<RpcBus>`.
#[derive(Debug)]
pub struct RpcBus {
    config: RpcConfig,
    token_path: PathBuf,
    bearer_token: RwLock<String>,
    sender: broadcast::Sender<RpcEnvelope>,
    dropped: AtomicUsize,
}

impl RpcBus {
    /// Construct a new bus, creating or reusing the bearer-token file under
    /// `workspace_dir` when `RpcConfig::token_path` is relative.
    ///
    /// `RpcConfig::token_path` is treated as relative when it is relative
    /// (the common case for the default `.nerve/session-meta/rpc-token`), and
    /// absolute paths are honoured verbatim — useful for tests that pin the
    /// token under a tempdir.
    pub fn new(config: RpcConfig, workspace_dir: &Path) -> Result<Self, RpcError> {
        config
            .validate()
            .map_err(|err| RpcError::InvalidConfig(err.to_string()))?;

        let token_path = resolve_token_path(&config, workspace_dir);
        ensure_parent_dir(&token_path)?;
        let bearer_token = read_or_create_token(&token_path, config.token_size_bytes)?;

        let (sender, _) = broadcast::channel(config.per_consumer_queue);

        Ok(Self {
            config,
            token_path,
            bearer_token: RwLock::new(bearer_token),
            sender,
            dropped: AtomicUsize::new(0),
        })
    }

    /// Current bearer token in `hex` form. Returned as a new `String`
    /// because the underlying storage is behind an `RwLock`.
    pub fn bearer_token(&self) -> String {
        self.bearer_token
            .read()
            .expect("rpc bearer token rwlock poisoned")
            .clone()
    }

    /// Subscribe a new consumer. The receiver has its own bounded queue of
    /// `RpcConfig::per_consumer_queue` envelopes.
    pub fn subscribe(&self) -> broadcast::Receiver<RpcEnvelope> {
        self.sender.subscribe()
    }

    /// Cumulative count of envelopes dropped because at least one
    /// subscriber lagged behind the broadcast channel.
    pub fn dropped(&self) -> usize {
        self.dropped.load(Ordering::Relaxed)
    }

    /// Serialize `payload`, truncate if it exceeds
    /// `RpcConfig::payload_cap_kib`, wrap it in an [`RpcEnvelope`] tagged
    /// with the current schema version and a fresh ULID, and broadcast it
    /// to all subscribers.
    ///
    /// Returns [`EmitOutcome`] with the running dropped counter. A `send`
    /// failure caused by *no* subscribers is **not** an error — the runtime
    /// continues to drive itself even when nothing is listening.
    pub fn emit(&self, kind: &str, payload: serde_json::Value) -> Result<EmitOutcome, EmitError> {
        let cap_bytes = self.config.payload_cap_kib.saturating_mul(1024);
        let final_payload = enforce_payload_cap(payload, cap_bytes)?;

        let envelope = RpcEnvelope::new(kind, final_payload).with_fresh_metadata();

        // broadcast::Sender::send fails only when there are no
        // subscribers; that is not a real failure in our model. The lagging
        // subscriber case is handled per-receiver via RecvError::Lagged and
        // tracked through `note_lag` from consumers if/when wired.
        match self.sender.send(envelope) {
            Ok(_count) => {}
            Err(_no_subscribers) => {}
        }

        Ok(EmitOutcome {
            dropped: self.dropped.load(Ordering::Relaxed),
        })
    }

    /// Account for a lag observed by a subscriber. Consumers that receive
    /// `broadcast::error::RecvError::Lagged(n)` should call this helper so
    /// the bus-wide drop counter reflects reality.
    pub fn note_lag(&self, lag: u64) {
        self.dropped.fetch_add(lag as usize, Ordering::Relaxed);
    }

    /// Rotate the bearer token in place, persisting the new value atomically
    /// with mode `0600`.
    pub fn rotate_token(&self, workspace_dir: &Path) -> Result<String, RpcError> {
        let token_path = resolve_token_path(&self.config, workspace_dir);
        ensure_parent_dir(&token_path)?;
        let new_token = generate_token_hex(self.config.token_size_bytes)?;
        atomic_write_token(&token_path, &new_token)?;
        *self
            .bearer_token
            .write()
            .expect("rpc bearer token rwlock poisoned") = new_token.clone();
        Ok(new_token)
    }

    /// Tear down the bus, removing the bearer-token file. Existing
    /// subscribers will observe `RecvError::Closed` on the next `recv`.
    pub fn shutdown(self) -> Result<(), RpcError> {
        match fs::remove_file(&self.token_path) {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(RpcError::TokenIo(err)),
        }
    }
}

fn resolve_token_path(config: &RpcConfig, workspace_dir: &Path) -> PathBuf {
    if config.token_path.is_absolute() {
        config.token_path.clone()
    } else {
        workspace_dir.join(&config.token_path)
    }
}

fn ensure_parent_dir(path: &Path) -> Result<(), RpcError> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)?;
    }
    Ok(())
}

fn read_or_create_token(path: &Path, size_bytes: usize) -> Result<String, RpcError> {
    match fs::read_to_string(path) {
        Ok(existing) => {
            let trimmed = existing.trim().to_string();
            // Reuse only a non-empty token we still trust. On Windows an existing
            // token whose on-disk DACL is not owner-only is rotated rather than
            // reused, so a token left by a pre-H9 build (which applied no ACL
            // hardening) — or otherwise readable by another local account — never
            // keeps authenticating the daemon. `||` short-circuits, so the DACL
            // read is skipped for the empty case (which rotates regardless).
            if trimmed.is_empty() || !existing_token_is_trusted(path) {
                let token = generate_token_hex(size_bytes)?;
                atomic_write_token(path, &token)?;
                Ok(token)
            } else {
                Ok(trimmed)
            }
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            let token = generate_token_hex(size_bytes)?;
            atomic_write_token(path, &token)?;
            Ok(token)
        }
        Err(err) => Err(RpcError::TokenIo(err)),
    }
}

/// Whether an existing, non-empty token file may be reused as-is.
///
/// On Windows a token whose on-disk DACL is not owner-only must not be reused:
/// the secret may already have been read by another local account (e.g. a token
/// written by a pre-H9 build, which applied no ACL hardening at all). Returning
/// `false` makes [`read_or_create_token`] rotate to a fresh, hardened token.
/// On Unix the token has always been created `0o600`, so there is no
/// unhardened-upgrade case to guard and the existing-reuse behaviour is
/// unchanged.
#[cfg(windows)]
fn existing_token_is_trusted(path: &Path) -> bool {
    win::verify_owner_only(path).is_ok()
}

#[cfg(not(windows))]
fn existing_token_is_trusted(_path: &Path) -> bool {
    true
}

fn generate_token_hex(size_bytes: usize) -> Result<String, RpcError> {
    if size_bytes == 0 {
        return Err(RpcError::InvalidConfig(
            "token_size_bytes must be greater than zero".to_string(),
        ));
    }
    let mut buf = vec![0u8; size_bytes];
    rand::thread_rng().fill_bytes(&mut buf);
    Ok(hex_encode(&buf))
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

#[cfg(unix)]
fn atomic_write_token(path: &Path, token: &str) -> Result<(), RpcError> {
    use std::io::Write as _;
    use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};

    let tmp = staging_path(path);
    {
        let mut file = fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .mode(0o600)
            .open(&tmp)?;
        file.write_all(token.as_bytes())?;
        file.sync_all()?;
    }
    // Re-assert permissions in case the umask / pre-existing file relaxed
    // them on platforms where O_CREAT honoured a wider mode.
    let perms = fs::Permissions::from_mode(0o600);
    fs::set_permissions(&tmp, perms)?;
    fs::rename(&tmp, path)?;
    Ok(())
}

/// Build the owner-only DACL in SDDL form for `sid_str` (e.g. `S-1-5-21-…`).
///
/// `D:P(A;;FA;;;<sid>)`:
/// - `D` — this is a DACL.
/// - `P` — **PROTECTED**: inherited ACEs are stripped, so no broader principal
///   can be granted access via inheritance from the parent directory.
/// - `(A;;FA;;;<sid>)` — a single **A**llow ACE granting **F**ile **A**ll access
///   to that SID and to no one else.
///
/// Pure string construction so it is unit-testable on every platform (the
/// Win32 application of it is `#[cfg(windows)]`).
#[cfg(any(windows, test))]
fn owner_only_sddl(sid_str: &str) -> String {
    format!("D:P(A;;FA;;;{sid_str})")
}

// Windows parity for the Unix `0o600` guard (H9). The token authenticates RPC
// clients to the daemon, so a token readable by another local account would let
// that account drive the daemon. We stage into a **freshly created**
// (`CREATE_NEW`) file with an **unpredictable** sibling name and an owner-only
// DACL applied **at creation time** (via `SECURITY_ATTRIBUTES`), so the token
// bytes never exist under a default/inherited ACL — mirroring the Unix
// `O_CREAT … mode(0o600)`. `CREATE_NEW` (not `CREATE_ALWAYS`) is load-bearing:
// `CreateFileW` ignores the security descriptor when it opens an *existing*
// file, so under `CREATE_ALWAYS` a stale or attacker-planted staging file could
// receive the token bytes under a wider ACL before the verifier rejects it; the
// unpredictable name plus `CREATE_NEW` denies that. We then **fail-closed
// self-verify** the on-disk DACL (exactly one Allow ACE for our own SID on a
// PROTECTED DACL) and refuse to publish otherwise, and on ANY error remove the
// staging file so a misapplied ACL fails the write rather than silently shipping
// a readable secret.
#[cfg(windows)]
fn atomic_write_token(path: &Path, token: &str) -> Result<(), RpcError> {
    use std::io::Write as _;

    let (mut file, tmp) = win::create_staging(path)?;

    // One closure so a single cleanup arm covers every failure — write, fsync,
    // verify, or a failed atomic replace — and a half-written, wrongly-
    // permissioned, or unpublished token is never left behind.
    let publish = || -> io::Result<()> {
        file.write_all(token.as_bytes())?;
        file.sync_all()?;
        drop(file);
        // Fail-closed: only publish if the on-disk DACL really is owner-only.
        win::verify_owner_only(&tmp)?;
        // Atomic publish that also replaces a prior token (rotation), with the
        // owner-only DACL already in place on the source.
        win::replace_atomic(&tmp, path)
    };
    if let Err(e) = publish() {
        let _ = fs::remove_file(&tmp);
        return Err(e.into());
    }
    Ok(())
}

// Other non-Unix, non-Windows targets (e.g. wasm): keep the previous
// best-effort behaviour. No owner-only guarantee is claimed for these
// platforms (none ship the daemon today).
#[cfg(all(not(unix), not(windows)))]
fn atomic_write_token(path: &Path, token: &str) -> Result<(), RpcError> {
    use std::io::Write as _;

    let tmp = staging_path(path);
    {
        let mut file = fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&tmp)?;
        file.write_all(token.as_bytes())?;
        file.sync_all()?;
    }
    fs::rename(&tmp, path)?;
    Ok(())
}

/// Win32 owner-only DACL primitives for [`atomic_write_token`] (H9). Each
/// function is a thin, fail-closed wrapper over the documented Win32 sequence;
/// the security-relevant decision (the SDDL) is the platform-independent
/// [`owner_only_sddl`]. Pointers handed to us by Win32 (`GetAce`,
/// `GetNamedSecurityInfoW`) point into OS-aligned structures, so the
/// `cast_ptr_alignment` lint is a false positive on this path.
#[cfg(windows)]
#[allow(clippy::cast_ptr_alignment)]
mod win {
    use std::ffi::{OsStr, c_void};
    use std::fs::File;
    use std::io;
    use std::os::windows::ffi::OsStrExt as _;
    use std::os::windows::io::{FromRawHandle as _, RawHandle};
    use std::path::{Path, PathBuf};
    use std::ptr::{null_mut, read_unaligned};

    use windows_sys::Win32::Foundation::{
        CloseHandle, GetLastError, HANDLE, INVALID_HANDLE_VALUE, LocalFree,
    };
    use windows_sys::Win32::Security::Authorization::{
        ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
        GetNamedSecurityInfoW,
    };
    use windows_sys::Win32::Security::{
        ACCESS_ALLOWED_ACE, ACL, GetAce, GetSecurityDescriptorControl, GetTokenInformation,
        SECURITY_ATTRIBUTES, TOKEN_USER,
    };
    use windows_sys::Win32::Storage::FileSystem::{CreateFileW, MoveFileExW};
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    // Win32 constants, named locally to their documented values so the import
    // surface stays small and stable across windows-sys point releases.
    const GENERIC_WRITE: u32 = 0x4000_0000;
    // CREATE_NEW (not CREATE_ALWAYS): fail if the staging file already exists, so
    // the owner-only SECURITY_ATTRIBUTES is always applied to a brand-new file —
    // CreateFileW ignores the security descriptor when it opens an existing one.
    const CREATE_NEW: u32 = 1;
    const FILE_ATTRIBUTE_NORMAL: u32 = 0x80;
    const MOVEFILE_REPLACE_EXISTING: u32 = 0x1;
    const MOVEFILE_WRITE_THROUGH: u32 = 0x8;
    const TOKEN_QUERY: u32 = 0x0008;
    const TOKEN_USER_CLASS: i32 = 1; // TokenUser
    const SDDL_REVISION_1: u32 = 1;
    const SE_FILE_OBJECT: i32 = 1;
    const DACL_SECURITY_INFORMATION: u32 = 0x0000_0004;
    const ACCESS_ALLOWED_ACE_TYPE: u8 = 0;
    const SE_DACL_PROTECTED: u16 = 0x1000;
    // CreateFileW(CREATE_NEW) sets last-error to ERROR_FILE_EXISTS on a name
    // collision; create_staging retries with a fresh random name.
    const ERROR_FILE_EXISTS: i32 = 80;
    // Bound on random staging-name attempts before giving up (collisions are
    // astronomically unlikely at 96 bits of entropy; this is only a safety net).
    const STAGING_ATTEMPTS: usize = 8;

    fn last_err() -> io::Error {
        // SAFETY: GetLastError reads a thread-local error code; always sound.
        io::Error::from_raw_os_error(unsafe { GetLastError() } as i32)
    }

    fn perm_denied(msg: impl Into<String>) -> io::Error {
        io::Error::new(io::ErrorKind::PermissionDenied, msg.into())
    }

    fn to_wide(s: &OsStr) -> Vec<u16> {
        s.encode_wide().chain(std::iter::once(0)).collect()
    }

    fn wide_str(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// SAFETY: `p` must be a valid pointer to a NUL-terminated UTF-16 string.
    unsafe fn from_wide_ptr(p: *const u16) -> String {
        let mut len = 0usize;
        // SAFETY: caller guarantees a NUL terminator; we stop at it.
        while unsafe { *p.add(len) } != 0 {
            len += 1;
        }
        let slice = unsafe { std::slice::from_raw_parts(p, len) };
        String::from_utf16_lossy(slice)
    }

    /// Closes a token handle on drop.
    struct HandleGuard(HANDLE);
    impl Drop for HandleGuard {
        fn drop(&mut self) {
            // SAFETY: we own the handle for the guard's lifetime.
            unsafe {
                CloseHandle(self.0);
            }
        }
    }

    /// The current process user's SID rendered as an `S-1-…` string.
    pub(super) fn current_user_sid_string() -> io::Result<String> {
        // SAFETY: the standard OpenProcessToken → GetTokenInformation(TokenUser)
        // → ConvertSidToStringSidW sequence; every out-param is an owned local,
        // the token handle is closed by `HandleGuard`, and the SID string is
        // LocalFree'd. `buf` outlives the SID pointer read from it.
        unsafe {
            let mut token: HANDLE = null_mut();
            if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
                return Err(last_err());
            }
            let _guard = HandleGuard(token);

            let mut len: u32 = 0;
            // First call sizes the buffer (returns 0 / ERROR_INSUFFICIENT_BUFFER).
            GetTokenInformation(token, TOKEN_USER_CLASS, null_mut(), 0, &mut len);
            if len == 0 {
                return Err(last_err());
            }
            let mut buf = vec![0u8; len as usize];
            let mut got: u32 = 0;
            if GetTokenInformation(
                token,
                TOKEN_USER_CLASS,
                buf.as_mut_ptr() as *mut c_void,
                len,
                &mut got,
            ) == 0
            {
                return Err(last_err());
            }
            // `buf` may be 1-aligned; read the struct unaligned to avoid forming
            // a misaligned reference. The SID it points at lives inside `buf`.
            let token_user = read_unaligned(buf.as_ptr() as *const TOKEN_USER);
            let mut wsid: *mut u16 = null_mut();
            if ConvertSidToStringSidW(token_user.User.Sid, &mut wsid) == 0 {
                return Err(last_err());
            }
            let s = from_wide_ptr(wsid);
            LocalFree(wsid as *mut c_void);
            Ok(s)
        }
    }

    /// A random hex suffix for the staging file name, so an attacker cannot
    /// pre-create the staging path (and `CREATE_NEW` refuses any collision).
    fn random_suffix() -> String {
        use rand::RngCore as _;
        let mut buf = [0u8; 12];
        rand::thread_rng().fill_bytes(&mut buf);
        super::hex_encode(&buf)
    }

    /// Create an owner-only staging file as a sibling of `final_path` with an
    /// unpredictable name and `CREATE_NEW`, retrying on the (vanishingly rare)
    /// name collision. Returns the open file and the path chosen. Because the
    /// file is always brand-new, the owner-only `SECURITY_ATTRIBUTES` is honoured
    /// rather than ignored (as it would be when opening an existing file), so the
    /// token bytes can never land in a pre-existing or attacker-planted file
    /// under a wider ACL.
    pub(super) fn create_staging(final_path: &Path) -> io::Result<(File, PathBuf)> {
        let dir = final_path.parent().filter(|p| !p.as_os_str().is_empty());
        let base = final_path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("rpc-token");
        for _ in 0..STAGING_ATTEMPTS {
            let name = format!(".{base}.{}.tmp", random_suffix());
            let tmp = match dir {
                Some(d) => d.join(&name),
                None => PathBuf::from(&name),
            };
            match create_new_owner_only(&tmp) {
                Ok(file) => return Ok((file, tmp)),
                Err(e) if e.raw_os_error() == Some(ERROR_FILE_EXISTS) => continue,
                Err(e) => return Err(e),
            }
        }
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "could not create a unique owner-only RPC token staging file",
        ))
    }

    /// Create `path` with `CREATE_NEW` (fails if it already exists) and a DACL
    /// granting only the current user, established atomically at creation so the
    /// file never exists under a wider ACL. Returns the open file for writing.
    pub(super) fn create_new_owner_only(path: &Path) -> io::Result<File> {
        let sid = current_user_sid_string()?;
        let sddl = wide_str(&super::owner_only_sddl(&sid));
        let path_w = to_wide(path.as_os_str());

        // SAFETY: `sddl` is a valid NUL-terminated UTF-16 SDDL string; on
        // success `psd` is a LocalAlloc'd security descriptor we free below.
        let mut psd: *mut c_void = null_mut();
        if unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl.as_ptr(),
                SDDL_REVISION_1,
                &mut psd,
                null_mut(),
            )
        } == 0
        {
            return Err(last_err());
        }

        let sa = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: psd,
            bInheritHandle: 0,
        };
        // SAFETY: valid path + security attributes; no-sharing, CREATE_NEW so the
        // call fails (ERROR_FILE_EXISTS) rather than opening an existing file —
        // which guarantees the security descriptor is applied to a new file.
        let handle = unsafe {
            CreateFileW(
                path_w.as_ptr(),
                GENERIC_WRITE,
                0,
                &sa,
                CREATE_NEW,
                FILE_ATTRIBUTE_NORMAL,
                null_mut(),
            )
        };
        // The SD was copied into the new file; free our copy regardless of
        // whether CreateFileW succeeded.
        // SAFETY: `psd` came from ConvertString…W and is freed exactly once.
        unsafe {
            LocalFree(psd);
        }
        if handle == INVALID_HANDLE_VALUE || handle.is_null() {
            return Err(last_err());
        }
        // SAFETY: we own `handle`; File closes it on drop.
        Ok(unsafe { File::from_raw_handle(handle as RawHandle) })
    }

    /// Fail-closed check that `path`'s DACL grants access to exactly the current
    /// user and nobody else, on a PROTECTED (non-inheriting) DACL.
    pub(super) fn verify_owner_only(path: &Path) -> io::Result<()> {
        let expected = current_user_sid_string()?;
        let path_w = to_wide(path.as_os_str());

        let mut dacl: *mut ACL = null_mut();
        let mut psd: *mut c_void = null_mut();
        // SAFETY: out-params written by the call; `psd` is LocalAlloc'd and
        // freed below (once, after we finish reading `dacl`, which it owns).
        let rc = unsafe {
            GetNamedSecurityInfoW(
                path_w.as_ptr(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION,
                null_mut(),
                null_mut(),
                &mut dacl,
                null_mut(),
                &mut psd,
            )
        };
        if rc != 0 {
            return Err(io::Error::from_raw_os_error(rc as i32));
        }

        let verdict = inspect_dacl(dacl, psd, &expected);
        // SAFETY: `psd` was allocated by GetNamedSecurityInfoW; free once.
        unsafe {
            LocalFree(psd);
        }
        verdict
    }

    /// The pointer-walking core of [`verify_owner_only`], split out so the
    /// LocalFree of `psd` happens on every return path.
    fn inspect_dacl(dacl: *mut ACL, psd: *mut c_void, expected_sid: &str) -> io::Result<()> {
        if dacl.is_null() {
            // A NULL DACL grants everyone full access — the opposite of what we
            // want. Fail closed.
            return Err(perm_denied("token DACL is null (would grant all users)"));
        }
        // SAFETY: `dacl` is a valid ACL owned by `psd` for this scope.
        let ace_count = unsafe { (*dacl).AceCount };
        if ace_count != 1 {
            return Err(perm_denied(format!(
                "token DACL has {ace_count} ACEs, expected exactly 1 (owner-only)"
            )));
        }

        let mut ace: *mut c_void = null_mut();
        // SAFETY: index 0 is in range (AceCount == 1); `ace` is set on success.
        if unsafe { GetAce(dacl, 0, &mut ace) } == 0 {
            return Err(last_err());
        }
        let ace = ace as *const ACCESS_ALLOWED_ACE;
        // SAFETY: GetAce yielded a valid, OS-aligned ACE pointer.
        let ace_type = unsafe { (*ace).Header.AceType };
        if ace_type != ACCESS_ALLOWED_ACE_TYPE {
            return Err(perm_denied(format!(
                "token DACL ACE type is {ace_type}, expected ACCESS_ALLOWED"
            )));
        }

        // The ACE's SID begins at its `SidStart` field.
        // SAFETY: `ace` is valid; `&(*ace).SidStart` is a pointer into it.
        let ace_sid = unsafe { std::ptr::addr_of!((*ace).SidStart) as *mut c_void };
        let mut wsid: *mut u16 = null_mut();
        // SAFETY: `ace_sid` is a valid SID; `wsid` is LocalFree'd after read.
        if unsafe { ConvertSidToStringSidW(ace_sid, &mut wsid) } == 0 {
            return Err(last_err());
        }
        // SAFETY: ConvertSidToStringSidW produced a NUL-terminated UTF-16 string.
        let ace_sid_str = unsafe { from_wide_ptr(wsid) };
        // SAFETY: `wsid` came from ConvertSidToStringSidW; free once.
        unsafe {
            LocalFree(wsid as *mut c_void);
        }
        if ace_sid_str != expected_sid {
            return Err(perm_denied(format!(
                "token DACL grants {ace_sid_str}, not the owner {expected_sid}"
            )));
        }

        // Confirm the DACL is PROTECTED so a future inherited ACE cannot widen
        // access behind our back.
        let mut control: u16 = 0;
        let mut revision: u32 = 0;
        // SAFETY: `psd` is a valid security descriptor; out-params are locals.
        if unsafe { GetSecurityDescriptorControl(psd, &mut control, &mut revision) } == 0 {
            return Err(last_err());
        }
        if control & SE_DACL_PROTECTED == 0 {
            return Err(perm_denied(
                "token DACL is not PROTECTED (inherited ACEs could widen access)",
            ));
        }
        Ok(())
    }

    /// Atomically move `from` onto `to`, replacing any existing token, with
    /// write-through so the rename is durable.
    pub(super) fn replace_atomic(from: &Path, to: &Path) -> io::Result<()> {
        let from_w = to_wide(from.as_os_str());
        let to_w = to_wide(to.as_os_str());
        // SAFETY: both are valid NUL-terminated path strings.
        if unsafe {
            MoveFileExW(
                from_w.as_ptr(),
                to_w.as_ptr(),
                MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
            )
        } == 0
        {
            return Err(last_err());
        }
        Ok(())
    }
}

// Deterministic staging name for the Unix and best-effort fallback writers. The
// Windows writer instead uses an unpredictable `CREATE_NEW` staging file (see
// `win::create_staging`), so this helper is not compiled there.
#[cfg(not(windows))]
fn staging_path(path: &Path) -> PathBuf {
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    PathBuf::from(tmp)
}

fn enforce_payload_cap(
    payload: serde_json::Value,
    cap_bytes: usize,
) -> Result<serde_json::Value, EmitError> {
    let encoded = serde_json::to_vec(&payload)?;
    if cap_bytes == 0 || encoded.len() <= cap_bytes {
        return Ok(payload);
    }

    const PREVIEW: usize = 256;
    let original_size = encoded.len();
    let head = preview_bytes(&encoded, 0, PREVIEW);
    let tail_start = encoded.len().saturating_sub(PREVIEW);
    let tail = preview_bytes(&encoded, tail_start, PREVIEW);

    Ok(serde_json::json!({
        "truncated": true,
        "original_size": original_size,
        "head": head,
        "tail": tail,
    }))
}

fn preview_bytes(buf: &[u8], start: usize, len: usize) -> String {
    let end = start.saturating_add(len).min(buf.len());
    let slice = &buf[start..end];
    String::from_utf8_lossy(slice).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use nerve_types::RPC_SCHEMA_VERSION;
    use serde_json::json;
    use tempfile::tempdir;
    use tokio::sync::broadcast::error::TryRecvError;

    fn test_config(tmp: &Path) -> RpcConfig {
        RpcConfig {
            per_consumer_queue: 16,
            payload_cap_kib: 1, // 1 KiB to make truncation easy in tests
            token_path: tmp.join("rpc-token"),
            token_size_bytes: 32,
            print_token: false,
            envelope_version: RPC_SCHEMA_VERSION.to_string(),
        }
    }

    #[test]
    fn emit_truncates_oversize_payload() {
        let dir = tempdir().unwrap();
        let mut config = test_config(dir.path());
        config.payload_cap_kib = 64;
        let bus = RpcBus::new(config, dir.path()).unwrap();
        let mut rx = bus.subscribe();

        // 1 MiB of payload data — well above the 64 KiB cap.
        let large = "x".repeat(1_048_576);
        let outcome = bus.emit("plan.proposed", json!({ "body": large })).unwrap();
        assert_eq!(outcome.dropped, 0);

        let envelope = rx.try_recv().unwrap();
        let payload = envelope.payload;
        assert_eq!(payload["truncated"], json!(true));
        let original_size = payload["original_size"].as_u64().unwrap() as usize;
        assert!(original_size > 64 * 1024);
        assert!(payload["head"].is_string());
        assert!(payload["tail"].is_string());
        assert!(payload["head"].as_str().unwrap().len() <= 256);
        assert!(payload["tail"].as_str().unwrap().len() <= 256);

        // Re-encoded envelope payload must be at least an order of
        // magnitude smaller than the original encoded blob.
        let re_encoded = serde_json::to_vec(&payload).unwrap();
        assert!(re_encoded.len() < 64 * 1024);
    }

    #[test]
    fn emit_includes_schema_version() {
        let dir = tempdir().unwrap();
        let bus = RpcBus::new(test_config(dir.path()), dir.path()).unwrap();
        let mut rx = bus.subscribe();

        bus.emit("round.started", json!({ "round": 1 })).unwrap();
        bus.emit("round.ended", json!({ "round": 1, "ok": true }))
            .unwrap();

        for _ in 0..2 {
            let env = rx.try_recv().unwrap();
            assert_eq!(env.schema_version, RPC_SCHEMA_VERSION);
            assert!(env.envelope_id.is_some(), "envelope_id must be set");
            assert!(env.emitted_at.is_some(), "emitted_at must be set");
        }
        assert!(matches!(rx.try_recv(), Err(TryRecvError::Empty)));
    }

    #[cfg(unix)]
    #[test]
    fn new_creates_token_file_0600() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = tempdir().unwrap();
        let bus = RpcBus::new(test_config(dir.path()), dir.path()).unwrap();
        let token_path = dir.path().join("rpc-token");
        assert!(token_path.exists());

        let metadata = fs::metadata(&token_path).unwrap();
        let mode = metadata.permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "token file must be 0600, got {mode:o}");

        // 32 bytes -> 64 hex characters.
        let token = bus.bearer_token();
        assert_eq!(token.len(), 64);
        assert!(token.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn owner_only_sddl_grants_single_full_access_ace_to_the_sid() {
        // A PROTECTED DACL (`P`) with exactly one Allow / Full-Access (`FA`) ACE
        // for the SID and nobody else — the owner-only shape applied on Windows.
        // Pure logic, asserted on every platform.
        let sddl = owner_only_sddl("S-1-5-21-100-200-300-1001");
        assert_eq!(sddl, "D:P(A;;FA;;;S-1-5-21-100-200-300-1001)");
    }

    // H9 runtime proof — Windows only (runs on the Windows CI lane). Exercises
    // the production token-write path and asserts the on-disk DACL is owner-only,
    // with a negative control so the assertion is not vacuous.
    #[cfg(windows)]
    #[test]
    fn windows_token_file_is_owner_only_real() {
        let dir = tempdir().unwrap();
        // Production path: RpcBus::new writes the token via atomic_write_token,
        // which creates it owner-only and self-verifies — a wrong ACL would make
        // this fail rather than ship a readable token.
        let bus = RpcBus::new(test_config(dir.path()), dir.path()).unwrap();
        let token_path = dir.path().join("rpc-token");
        assert!(token_path.exists());

        // The on-disk DACL really is owner-only (exactly one Allow ACE for the
        // current user, on a PROTECTED DACL) — the same check the write uses.
        win::verify_owner_only(&token_path).expect("token DACL must be owner-only");

        // Negative control: a normally-created file inherits a wider,
        // non-PROTECTED DACL, so the verifier MUST reject it — proving the
        // positive check above actually discriminates.
        let plain = dir.path().join("plain.txt");
        fs::write(&plain, b"not hardened").unwrap();
        assert!(
            win::verify_owner_only(&plain).is_err(),
            "verifier must reject a default-ACL (non-owner-only) file"
        );

        // Token content is intact and well-formed.
        let token = bus.bearer_token();
        assert_eq!(token.len(), 64);
        assert!(token.chars().all(|c| c.is_ascii_hexdigit()));
    }

    // H9 regression (BLOCKER, both reviewers): an existing token whose DACL is
    // NOT owner-only — e.g. one written by a pre-H9 build with no ACL hardening,
    // readable by another local account — must be ROTATED on startup, never
    // reused. Windows only (the rotation guard is `#[cfg(windows)]`).
    #[cfg(windows)]
    #[test]
    fn windows_rotates_a_preexisting_non_owner_only_token() {
        let dir = tempdir().unwrap();
        let token_path = dir.path().join("rpc-token");
        // A plain write inherits a wider, non-PROTECTED DACL — exactly the pre-H9
        // exposure. Sanity-check it is not already owner-only.
        let weak = "deadbeef".repeat(8); // 64 hex chars: well-formed but weak
        fs::write(&token_path, weak.as_bytes()).unwrap();
        assert!(
            win::verify_owner_only(&token_path).is_err(),
            "a plain fs::write token must not already be owner-only"
        );

        let bus = RpcBus::new(test_config(dir.path()), dir.path()).unwrap();
        assert_ne!(
            bus.bearer_token(),
            weak,
            "a non-owner-only existing token must be rotated, never reused"
        );
        win::verify_owner_only(&token_path).expect("the rotated token must be owner-only on disk");
    }

    // H9 regression (BLOCKER, r2): the staging create must REFUSE a pre-existing
    // file (CREATE_NEW) so token bytes never land in a stale or attacker-planted
    // staging file under a wider ACL. Windows only.
    #[cfg(windows)]
    #[test]
    fn windows_staging_create_refuses_a_preexisting_file() {
        let dir = tempdir().unwrap();
        let planted = dir.path().join(".rpc-token.planted.tmp");
        fs::write(&planted, b"planted").unwrap();
        assert!(
            win::create_new_owner_only(&planted).is_err(),
            "create_new_owner_only must refuse an existing file (CREATE_NEW)"
        );
        // The pre-existing file must be left untouched (never truncated/written).
        assert_eq!(
            fs::read(&planted).unwrap(),
            b"planted".to_vec(),
            "an existing staging file must not be truncated or written"
        );
    }

    #[test]
    fn default_token_path_is_workspace_relative() {
        let dir = tempdir().unwrap();
        let _bus = RpcBus::new(RpcConfig::default(), dir.path()).unwrap();

        assert!(dir.path().join(".nerve/session-meta/rpc-token").exists());
        assert!(
            !dir.path()
                .join(".nerve/session-meta/.nerve/session-meta/rpc-token")
                .exists()
        );
    }

    #[test]
    fn shutdown_removes_token_file() {
        let dir = tempdir().unwrap();
        let bus = RpcBus::new(test_config(dir.path()), dir.path()).unwrap();
        let token_path = dir.path().join("rpc-token");
        assert!(token_path.exists());

        bus.shutdown().unwrap();
        assert!(
            !token_path.exists(),
            "shutdown must delete the bearer-token file"
        );
    }

    #[test]
    fn rotate_token_changes_value() {
        let dir = tempdir().unwrap();
        let bus = RpcBus::new(test_config(dir.path()), dir.path()).unwrap();

        let before = bus.bearer_token();
        let rotated = bus.rotate_token(dir.path()).unwrap();
        let after = bus.bearer_token();

        assert_ne!(before, after, "rotate must produce a new token");
        assert_eq!(rotated, after, "rotate must return the new token");

        let on_disk = fs::read_to_string(dir.path().join("rpc-token")).unwrap();
        assert_eq!(on_disk.trim(), after);
    }

    #[test]
    fn subscribe_receives_emitted() {
        let dir = tempdir().unwrap();
        let bus = RpcBus::new(test_config(dir.path()), dir.path()).unwrap();

        let mut rx_a = bus.subscribe();
        let mut rx_b = bus.subscribe();

        bus.emit("session.started", json!({ "task_id": "t-1" }))
            .unwrap();

        let env_a = rx_a.try_recv().unwrap();
        let env_b = rx_b.try_recv().unwrap();
        assert_eq!(env_a.kind, "session.started");
        assert_eq!(env_b.kind, "session.started");
        assert_eq!(env_a.envelope_id, env_b.envelope_id);
        assert_eq!(env_a.payload, json!({ "task_id": "t-1" }));
    }

    #[test]
    fn new_reuses_existing_token() {
        let dir = tempdir().unwrap();
        let config = test_config(dir.path());
        let first = RpcBus::new(config.clone(), dir.path()).unwrap();
        let first_token = first.bearer_token();
        drop(first);

        let second = RpcBus::new(config, dir.path()).unwrap();
        assert_eq!(
            second.bearer_token(),
            first_token,
            "second open must reuse persisted token"
        );
    }

    #[test]
    fn emit_no_subscribers_is_ok() {
        let dir = tempdir().unwrap();
        let bus = RpcBus::new(test_config(dir.path()), dir.path()).unwrap();
        // No subscribers — emit must not error.
        bus.emit("budget.changed", json!({ "tokens": 10 })).unwrap();
    }

    #[test]
    fn note_lag_accumulates() {
        let dir = tempdir().unwrap();
        let bus = RpcBus::new(test_config(dir.path()), dir.path()).unwrap();
        assert_eq!(bus.dropped(), 0);
        bus.note_lag(3);
        bus.note_lag(2);
        assert_eq!(bus.dropped(), 5);
    }
}
