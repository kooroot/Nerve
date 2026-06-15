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

use chrono::Utc;
use nerve_config::RpcConfig;
use nerve_types::RpcEnvelope;
use rand::RngCore;
use thiserror::Error;
use tokio::sync::broadcast;
use ulid::Ulid;

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
    /// `session_meta_dir`.
    ///
    /// `RpcConfig::token_path` is treated as relative when it is relative
    /// (the common case for the default `.nerve/session-meta/rpc-token`),
    /// and absolute paths are honoured verbatim — useful for tests that
    /// pin the token under a tempdir.
    pub fn new(config: RpcConfig, session_meta_dir: &Path) -> Result<Self, RpcError> {
        config
            .validate()
            .map_err(|err| RpcError::InvalidConfig(err.to_string()))?;

        let token_path = resolve_token_path(&config, session_meta_dir);
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

        let envelope = RpcEnvelope::new(kind, final_payload)
            .with_envelope_id(Ulid::new().to_string())
            .with_emitted_at(Utc::now());

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
    pub fn rotate_token(&self, session_meta_dir: &Path) -> Result<String, RpcError> {
        let token_path = resolve_token_path(&self.config, session_meta_dir);
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

fn resolve_token_path(config: &RpcConfig, session_meta_dir: &Path) -> PathBuf {
    if config.token_path.is_absolute() {
        config.token_path.clone()
    } else {
        session_meta_dir.join(&config.token_path)
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
            if trimmed.is_empty() {
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

#[cfg(not(unix))]
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
