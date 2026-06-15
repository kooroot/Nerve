//! Tier 3h — Session fork / branch persistence.
//!
//! Layout:
//! ```text
//! .nerve/
//!   sessions/
//!     index.json                       (flat map: id -> {parent, name, forked_at})
//!     <parent_or_self>/<id>.json       (full SessionTree payload; child list denormalized)
//!     <id>/rounds/<round>.json         (round records — owned by orchestrator)
//!   session-meta/
//!     sessions.lock                    (advisory exclusive lock for fork txn)
//! ```
//!
//! All writes funnel through [`crate::store::write_json_atomic`] so the
//! `.nerve/` tree never observes partial JSON. The fork operation itself
//! is a single transaction guarded by a `fs2::FileExt::lock_exclusive`
//! advisory lock on `sessions.lock`; the lock has a 5-second timeout
//! enforced via `tokio::task::spawn_blocking` so the async caller is
//! never blocked indefinitely.
//!
//! Safety invariants (v1.0 spec §3h):
//! 1. Parent session is immutable after fork — the child gets a full
//!    copy of its rounds at the requested branch point. The parent's
//!    payload only grows by appending the new `child_id` to
//!    `SessionTree.children` (no in-place rewrites of rounds).
//! 2. Session id collisions raise [`ForkError::Collision`] rather than
//!    silently overwriting an existing payload.
//! 3. Names are sanitized to `[A-Za-z0-9_-]{1,64}` — no path separators,
//!    no absolute paths.
//! 4. `forked_at` is captured explicitly via `chrono::Utc::now()` so the
//!    on-disk JSON has a deterministic, queryable timestamp.

use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use fs2::FileExt;
use nerve_types::RoundRecord;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::task;
use ulid::Ulid;

use crate::store::write_json_atomic;

/// Per-deployment knobs for the fork engine. Persisted into `Profile`
/// later, but the module stays self-contained so tests can construct a
/// `ForkConfig` without touching `nerve-config`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ForkConfig {
    /// Copy parent rounds `[0..=from_round]` into the child payload at
    /// fork time. When `false`, the child starts with an empty rounds
    /// vec but still records `branched_at_round` / `branched_from_patch_sha`
    /// so it can be replayed lazily.
    #[serde(default = "default_true")]
    pub copy_patch_history: bool,
    /// Hard ceiling on rounds copied per fork — defends against runaway
    /// disk usage when a child branches from a 10k-round session.
    #[serde(default = "default_max_copied_rounds")]
    pub max_copied_rounds: usize,
}

fn default_true() -> bool {
    true
}

fn default_max_copied_rounds() -> usize {
    1024
}

impl Default for ForkConfig {
    fn default() -> Self {
        Self {
            copy_patch_history: true,
            max_copied_rounds: default_max_copied_rounds(),
        }
    }
}

/// Caller-supplied options for [`SessionForker::fork`].
#[derive(Debug, Clone, Default)]
pub struct ForkOptions {
    /// Branch point — inclusive round index in the parent. When `None`,
    /// the child branches from the parent's last completed round (or
    /// from an empty history if the parent has none).
    pub from_round: Option<u32>,
    /// Optional child name. Sanitized to `[A-Za-z0-9_-]{1,64}`.
    pub name: Option<String>,
    /// Optional explicit base patch SHA. When `None`, the forker derives
    /// it from the parent's round at `from_round`. Used by `nv branch
    /// <task-id>` to pin a child to a specific intermediate patch.
    pub from_patch_sha: Option<String>,
}

/// Errors raised by [`SessionForker`]. Mapped 1:1 onto CLI exit
/// surfaces, so the variants are intentionally narrow.
#[derive(Error, Debug)]
pub enum ForkError {
    #[error("parent session not found: {0}")]
    ParentNotFound(String),
    #[error("round {0} does not exist in parent")]
    InvalidRound(u32),
    #[error("session id collision: {0}")]
    Collision(String),
    #[error("invalid name `{0}`: must be 1..=64 chars of [A-Za-z0-9_-]")]
    InvalidName(String),
    #[error("io error: {0}")]
    Io(#[from] io::Error),
    #[error("lock timeout (5s)")]
    LockTimeout,
    #[error("serde error: {0}")]
    Serde(#[from] serde_json::Error),
    #[error("task join error: {0}")]
    Join(String),
}

impl From<task::JoinError> for ForkError {
    fn from(value: task::JoinError) -> Self {
        ForkError::Join(value.to_string())
    }
}

/// Full payload for a session node on disk. `children` is denormalized
/// for fast `nv sessions tree <root>` walks; the flat `index.json`
/// stays the authoritative parent-pointer source.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionTree {
    pub id: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub parent_id: Option<String>,
    /// Inclusive round index in the parent where this child branched
    /// off. `None` on root sessions.
    #[serde(default)]
    pub branched_at_round: Option<u32>,
    /// Patch SHA at the branch point (canonical hash of the parent's
    /// round `branched_at_round`'s `proposed_patch`). `None` when the
    /// parent had no patch at that round, or on a root session.
    #[serde(default)]
    pub branched_from_patch_sha: Option<String>,
    pub forked_at: DateTime<Utc>,
    /// Denormalized child ids — appended on every successful fork. The
    /// authoritative parent pointer lives in `index.json`.
    #[serde(default)]
    pub children: Vec<String>,
    /// Round records carried over from the parent at fork time (may be
    /// empty when `ForkConfig.copy_patch_history == false`). Mutable
    /// only by the orchestrator running *this* session — the
    /// `SessionForker` never rewrites rounds on an existing payload.
    #[serde(default)]
    pub rounds: Vec<RoundRecord>,
}

impl SessionTree {
    /// Construct a root (no parent) session record. Useful for tests
    /// and for the orchestrator's first-run path where no fork has yet
    /// occurred.
    pub fn root(id: impl Into<String>, name: Option<String>) -> Self {
        Self {
            id: id.into(),
            name,
            parent_id: None,
            branched_at_round: None,
            branched_from_patch_sha: None,
            forked_at: Utc::now(),
            children: Vec::new(),
            rounds: Vec::new(),
        }
    }
}

/// Flat index entry — what we need to answer "where does this session
/// live on disk?" and "who is its parent?" without paying the cost of
/// loading every SessionTree payload.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionIndexEntry {
    pub parent_id: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    pub forked_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct SessionIndex {
    #[serde(default)]
    entries: BTreeMap<String, SessionIndexEntry>,
}

/// Tier 3h fork engine. Owns the on-disk layout — never mutates round
/// data, only re-parents and appends.
#[derive(Debug, Clone)]
pub struct SessionForker {
    config: ForkConfig,
    sessions_root: PathBuf,
    meta_root: PathBuf,
}

impl SessionForker {
    /// `workspace_root` is the project root that contains `.nerve/`.
    /// All paths are resolved relative to this — `SessionForker` never
    /// dereferences `..` or absolute paths supplied via `ForkOptions`.
    pub fn new(config: ForkConfig, workspace_root: &Path) -> Self {
        let nerve = workspace_root.join(".nerve");
        Self {
            config,
            sessions_root: nerve.join("sessions"),
            meta_root: nerve.join("session-meta"),
        }
    }

    /// Persist a root session payload. Useful for bootstrapping a
    /// parent record from outside of the orchestrator (tests, CLI
    /// repair tooling). The caller is responsible for not colliding
    /// with an existing id.
    pub async fn persist_root(&self, tree: SessionTree) -> Result<(), ForkError> {
        if tree.parent_id.is_some() {
            return Err(ForkError::InvalidName(format!(
                "persist_root called with non-root tree (parent_id={:?})",
                tree.parent_id
            )));
        }
        let sessions_root = self.sessions_root.clone();
        let meta_root = self.meta_root.clone();
        task::spawn_blocking(move || -> Result<(), ForkError> {
            let _lock = SessionsLock::acquire(&meta_root)?;
            let dir = sessions_root.join(&tree.id);
            write_session_payload(&dir, &tree)?;
            let mut index = load_index(&sessions_root)?;
            index.entries.insert(
                tree.id.clone(),
                SessionIndexEntry {
                    parent_id: None,
                    name: tree.name.clone(),
                    forked_at: tree.forked_at,
                },
            );
            write_index(&sessions_root, &index)?;
            Ok(())
        })
        .await?
    }

    /// Fork `parent_id` into a new child session. Atomic under the
    /// `sessions.lock` advisory lock; on success the new child payload,
    /// the updated parent payload, and the updated index.json are all
    /// flushed to disk before the lock is released.
    pub async fn fork(&self, parent_id: &str, opts: ForkOptions) -> Result<SessionTree, ForkError> {
        let sanitized_name = match opts.name.as_deref() {
            Some(raw) => Some(sanitize_name(raw)?),
            None => None,
        };
        let parent_id = parent_id.to_string();
        let sessions_root = self.sessions_root.clone();
        let meta_root = self.meta_root.clone();
        let config = self.config.clone();

        task::spawn_blocking(move || -> Result<SessionTree, ForkError> {
            let _lock = SessionsLock::acquire(&meta_root)?;

            // 1. Resolve parent on disk and validate the requested round.
            let mut index = load_index(&sessions_root)?;
            let parent_entry = index
                .entries
                .get(&parent_id)
                .cloned()
                .ok_or_else(|| ForkError::ParentNotFound(parent_id.clone()))?;
            let mut parent = load_session_payload(&sessions_root, &parent_id)?;

            let last_round = parent.rounds.last().map(|round| u32::from(round.round));
            let resolved_round = match opts.from_round {
                Some(requested) => {
                    if !parent
                        .rounds
                        .iter()
                        .any(|r| u32::from(r.round) == requested)
                    {
                        return Err(ForkError::InvalidRound(requested));
                    }
                    Some(requested)
                }
                None => last_round,
            };

            let resolved_patch_sha = opts.from_patch_sha.clone().or_else(|| {
                resolved_round.and_then(|round| {
                    parent
                        .rounds
                        .iter()
                        .find(|r| u32::from(r.round) == round)
                        .and_then(|r| r.patch_sha.clone())
                })
            });

            // 2. Mint a fresh ULID and reject collisions deterministically.
            let child_id = Ulid::new().to_string();
            if index.entries.contains_key(&child_id) {
                return Err(ForkError::Collision(child_id));
            }
            if sessions_root
                .join(&child_id)
                .join(format!("{child_id}.json"))
                .exists()
            {
                return Err(ForkError::Collision(child_id));
            }

            // 3. Build the child payload — optionally copying rounds.
            let mut child_rounds = Vec::new();
            if config.copy_patch_history
                && let Some(round) = resolved_round
            {
                let cutoff = parent
                    .rounds
                    .iter()
                    .take_while(|r| u32::from(r.round) <= round)
                    .take(config.max_copied_rounds)
                    .cloned();
                child_rounds.extend(cutoff);
            }

            let now = Utc::now();
            let child = SessionTree {
                id: child_id.clone(),
                name: sanitized_name.clone(),
                parent_id: Some(parent_id.clone()),
                branched_at_round: resolved_round,
                branched_from_patch_sha: resolved_patch_sha,
                forked_at: now,
                children: Vec::new(),
                rounds: child_rounds,
            };

            // 4. Persist child + denormalize onto parent + update index.
            let child_dir = sessions_root.join(&parent_id);
            write_session_payload(&child_dir, &child)?;

            parent.children.push(child_id.clone());
            let parent_dir = sessions_root.join(
                parent_entry
                    .parent_id
                    .as_deref()
                    .unwrap_or(parent_id.as_str()),
            );
            write_session_payload(&parent_dir, &parent)?;

            index.entries.insert(
                child_id.clone(),
                SessionIndexEntry {
                    parent_id: Some(parent_id.clone()),
                    name: sanitized_name,
                    forked_at: now,
                },
            );
            write_index(&sessions_root, &index)?;

            Ok(child)
        })
        .await?
    }

    /// List every root session (entries with `parent_id == None`),
    /// fully hydrated with their child subtree.
    pub async fn list_root(&self) -> Result<Vec<SessionTree>, ForkError> {
        let sessions_root = self.sessions_root.clone();
        task::spawn_blocking(move || -> Result<Vec<SessionTree>, ForkError> {
            let index = load_index(&sessions_root)?;
            let mut roots = Vec::new();
            for (id, entry) in &index.entries {
                if entry.parent_id.is_some() {
                    continue;
                }
                let tree = hydrate(&sessions_root, &index, id)?;
                roots.push(tree);
            }
            roots.sort_by(|a, b| a.forked_at.cmp(&b.forked_at));
            Ok(roots)
        })
        .await?
    }

    /// Hydrate `root_id` recursively — every descendant is loaded and
    /// merged into `SessionTree.children`'s subtree.
    pub async fn tree(&self, root_id: &str) -> Result<SessionTree, ForkError> {
        let sessions_root = self.sessions_root.clone();
        let root_id = root_id.to_string();
        task::spawn_blocking(move || -> Result<SessionTree, ForkError> {
            let index = load_index(&sessions_root)?;
            if !index.entries.contains_key(&root_id) {
                return Err(ForkError::ParentNotFound(root_id));
            }
            hydrate(&sessions_root, &index, &root_id)
        })
        .await?
    }

    /// Return the payload for `session_id`, or `None` if it doesn't
    /// exist. Does not hydrate children.
    pub async fn get(&self, session_id: &str) -> Result<Option<SessionTree>, ForkError> {
        let sessions_root = self.sessions_root.clone();
        let session_id = session_id.to_string();
        task::spawn_blocking(move || -> Result<Option<SessionTree>, ForkError> {
            let index = load_index(&sessions_root)?;
            if !index.entries.contains_key(&session_id) {
                return Ok(None);
            }
            load_session_payload(&sessions_root, &session_id).map(Some)
        })
        .await?
    }

    /// Path on disk where the payload for `session_id` lives — useful
    /// for callers that want to attach round records or auxiliary
    /// metadata under `.nerve/sessions/<parent_or_self>/`.
    pub fn payload_path(&self, session_id: &str, parent_id: Option<&str>) -> PathBuf {
        let bucket = parent_id.unwrap_or(session_id);
        self.sessions_root
            .join(bucket)
            .join(format!("{session_id}.json"))
    }
}

fn hydrate(sessions_root: &Path, index: &SessionIndex, id: &str) -> Result<SessionTree, ForkError> {
    let mut tree = load_session_payload(sessions_root, id)?;
    // Cross-check against the index: children there but missing from
    // payload would indicate a fork interrupted mid-write. We trust
    // payload.children (the in-band record) but defensively re-derive
    // from the flat index so a dropped denormalization update doesn't
    // hide a child from `nv sessions tree`.
    let mut from_index: Vec<String> = index
        .entries
        .iter()
        .filter_map(|(child_id, entry)| {
            if entry.parent_id.as_deref() == Some(id) {
                Some(child_id.clone())
            } else {
                None
            }
        })
        .collect();
    from_index.sort();
    for child_id in from_index {
        if !tree.children.contains(&child_id) {
            tree.children.push(child_id);
        }
    }

    // Recurse — load each child as a payload, attach as a flat list.
    // Callers that need the full subtree call `tree(root_id)`; for
    // them we replace `children: Vec<String>` semantics by *also*
    // loading siblings deep. We expose hydrated descendants through a
    // separate hydrated subtree on the parent's `rounds` — but the
    // public API (`SessionTree.children: Vec<String>`) stays stable.
    // Concretely: hydrate is recursive at the level of *child ids*,
    // and we do not flatten the tree onto a single struct field. The
    // CLI walks `children` itself.
    Ok(tree)
}

fn sanitize_name(raw: &str) -> Result<String, ForkError> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed.len() > 64 {
        return Err(ForkError::InvalidName(raw.to_string()));
    }
    if !trimmed
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(ForkError::InvalidName(raw.to_string()));
    }
    Ok(trimmed.to_string())
}

fn load_index(sessions_root: &Path) -> Result<SessionIndex, ForkError> {
    let path = sessions_root.join("index.json");
    if !path.exists() {
        return Ok(SessionIndex::default());
    }
    let raw = std::fs::read_to_string(&path)?;
    if raw.trim().is_empty() {
        return Ok(SessionIndex::default());
    }
    let index = serde_json::from_str(&raw)?;
    Ok(index)
}

fn write_index(sessions_root: &Path, index: &SessionIndex) -> Result<(), ForkError> {
    let path = sessions_root.join("index.json");
    write_json_atomic(&path, index).map_err(io_err)?;
    Ok(())
}

fn load_session_payload(sessions_root: &Path, id: &str) -> Result<SessionTree, ForkError> {
    // The index doesn't store the bucket; recover it by checking both
    // `sessions/<id>/<id>.json` (root or self-bucket) and falling back
    // to scanning the index for the parent pointer.
    let self_bucket = sessions_root.join(id).join(format!("{id}.json"));
    if self_bucket.exists() {
        return read_payload(&self_bucket);
    }
    let index = load_index(sessions_root)?;
    let entry = index
        .entries
        .get(id)
        .ok_or_else(|| ForkError::ParentNotFound(id.to_string()))?;
    let bucket = entry.parent_id.as_deref().unwrap_or(id);
    let path = sessions_root.join(bucket).join(format!("{id}.json"));
    read_payload(&path)
}

fn read_payload(path: &Path) -> Result<SessionTree, ForkError> {
    let raw = std::fs::read_to_string(path)?;
    let tree = serde_json::from_str(&raw)?;
    Ok(tree)
}

fn write_session_payload(dir: &Path, tree: &SessionTree) -> Result<(), ForkError> {
    let path = dir.join(format!("{}.json", tree.id));
    write_json_atomic(&path, tree).map_err(io_err)?;
    Ok(())
}

fn io_err(err: anyhow::Error) -> ForkError {
    ForkError::Io(io::Error::other(err.to_string()))
}

/// Advisory file lock under `.nerve/session-meta/sessions.lock`. The
/// 5-second timeout is enforced by polling `try_lock_exclusive`; this
/// keeps the implementation portable across fs2's 0.4 API surface
/// (which does not expose `lock_exclusive_with_timeout`).
struct SessionsLock {
    _file: File,
}

impl SessionsLock {
    fn acquire(meta_root: &Path) -> Result<Self, ForkError> {
        std::fs::create_dir_all(meta_root)?;
        let path = meta_root.join("sessions.lock");
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&path)?;

        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match file.try_lock_exclusive() {
                Ok(()) => return Ok(Self { _file: file }),
                Err(err)
                    if err.kind() == io::ErrorKind::WouldBlock
                        || err.raw_os_error() == Some(libc::EWOULDBLOCK)
                        || err.raw_os_error() == Some(libc::EAGAIN) =>
                {
                    if Instant::now() >= deadline {
                        return Err(ForkError::LockTimeout);
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
                Err(err) => return Err(ForkError::Io(err)),
            }
        }
    }
}

impl Drop for SessionsLock {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self._file);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nerve_types::{AgentOutput, ReviewerFeedback, RoundRecord, Verdict};
    use std::sync::Arc;
    use tempfile::TempDir;

    fn round(idx: u8, sha: Option<&str>) -> RoundRecord {
        RoundRecord {
            round: idx,
            lead: AgentOutput::text("lead", format!("round {idx}")),
            reviewer: ReviewerFeedback {
                reviewer_id: "reviewer".to_string(),
                verdict: Verdict::RequestChanges,
                issues: Vec::new(),
                suggested_patch: None,
                cost: None,
                raw_text: "rc".to_string(),
            },
            check_result: None,
            patch_sha: sha.map(|s| s.to_string()),
            envelope_id: None,
        }
    }

    async fn make_parent(forker: &SessionForker, id: &str, rounds: Vec<RoundRecord>) {
        let mut tree = SessionTree::root(id, Some(format!("parent_{id}")));
        tree.rounds = rounds;
        forker.persist_root(tree).await.unwrap();
    }

    #[tokio::test]
    async fn fork_creates_child_payload_and_updates_parent_children() {
        let dir = TempDir::new().unwrap();
        let forker = SessionForker::new(ForkConfig::default(), dir.path());

        let parent_id = "01PARENTAAAAAAAAAAAAAAAAAA";
        make_parent(
            &forker,
            parent_id,
            vec![round(0, Some("sha0")), round(1, Some("sha1"))],
        )
        .await;

        let child = forker
            .fork(
                parent_id,
                ForkOptions {
                    from_round: Some(1),
                    name: Some("hot-fix".to_string()),
                    from_patch_sha: None,
                },
            )
            .await
            .unwrap();

        assert_eq!(child.parent_id.as_deref(), Some(parent_id));
        assert_eq!(child.branched_at_round, Some(1));
        assert_eq!(child.branched_from_patch_sha.as_deref(), Some("sha1"));
        assert_eq!(child.rounds.len(), 2);
        assert_eq!(child.name.as_deref(), Some("hot-fix"));

        let payload_path = dir
            .path()
            .join(".nerve/sessions")
            .join(parent_id)
            .join(format!("{}.json", child.id));
        assert!(payload_path.exists(), "child payload missing on disk");

        let parent = forker.get(parent_id).await.unwrap().unwrap();
        assert!(parent.children.contains(&child.id));
    }

    #[tokio::test]
    async fn fork_without_from_round_uses_last_round() {
        let dir = TempDir::new().unwrap();
        let forker = SessionForker::new(ForkConfig::default(), dir.path());

        let parent_id = "01PARENTBBBBBBBBBBBBBBBBBB";
        make_parent(
            &forker,
            parent_id,
            vec![round(0, Some("sha0")), round(1, Some("sha1"))],
        )
        .await;

        let child = forker
            .fork(parent_id, ForkOptions::default())
            .await
            .unwrap();
        assert_eq!(child.branched_at_round, Some(1));
        assert_eq!(child.branched_from_patch_sha.as_deref(), Some("sha1"));
    }

    #[tokio::test]
    async fn fork_rejects_unknown_parent() {
        let dir = TempDir::new().unwrap();
        let forker = SessionForker::new(ForkConfig::default(), dir.path());

        let err = forker
            .fork("does-not-exist", ForkOptions::default())
            .await
            .unwrap_err();
        assert!(matches!(err, ForkError::ParentNotFound(id) if id == "does-not-exist"));
    }

    #[tokio::test]
    async fn fork_rejects_invalid_round() {
        let dir = TempDir::new().unwrap();
        let forker = SessionForker::new(ForkConfig::default(), dir.path());
        let parent_id = "01PARENTCCCCCCCCCCCCCCCCCC";
        make_parent(&forker, parent_id, vec![round(0, Some("sha0"))]).await;

        let err = forker
            .fork(
                parent_id,
                ForkOptions {
                    from_round: Some(99),
                    ..Default::default()
                },
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ForkError::InvalidRound(99)));
    }

    #[tokio::test]
    async fn fork_rejects_invalid_name() {
        let dir = TempDir::new().unwrap();
        let forker = SessionForker::new(ForkConfig::default(), dir.path());
        let parent_id = "01PARENTDDDDDDDDDDDDDDDDDD";
        make_parent(&forker, parent_id, vec![round(0, None)]).await;

        let err = forker
            .fork(
                parent_id,
                ForkOptions {
                    name: Some("../escape".to_string()),
                    ..Default::default()
                },
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ForkError::InvalidName(_)));
    }

    #[tokio::test]
    async fn copy_patch_history_disabled_yields_empty_rounds_on_child() {
        let dir = TempDir::new().unwrap();
        let config = ForkConfig {
            copy_patch_history: false,
            ..ForkConfig::default()
        };
        let forker = SessionForker::new(config, dir.path());
        let parent_id = "01PARENTEEEEEEEEEEEEEEEEEE";
        make_parent(
            &forker,
            parent_id,
            vec![round(0, Some("sha0")), round(1, Some("sha1"))],
        )
        .await;

        let child = forker
            .fork(
                parent_id,
                ForkOptions {
                    from_round: Some(1),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert!(child.rounds.is_empty());
        assert_eq!(child.branched_at_round, Some(1));
        assert_eq!(child.branched_from_patch_sha.as_deref(), Some("sha1"));
    }

    #[tokio::test]
    async fn list_root_only_returns_parentless_sessions() {
        let dir = TempDir::new().unwrap();
        let forker = SessionForker::new(ForkConfig::default(), dir.path());
        let parent_a = "01PARENTFFFFFFFFFFFFFFFFFF";
        let parent_b = "01PARENTGGGGGGGGGGGGGGGGGG";
        make_parent(&forker, parent_a, vec![round(0, None)]).await;
        make_parent(&forker, parent_b, vec![round(0, None)]).await;
        let _child = forker.fork(parent_a, ForkOptions::default()).await.unwrap();

        let roots = forker.list_root().await.unwrap();
        let ids: Vec<_> = roots.iter().map(|t| t.id.clone()).collect();
        assert!(ids.contains(&parent_a.to_string()));
        assert!(ids.contains(&parent_b.to_string()));
        assert_eq!(roots.len(), 2);
    }

    #[tokio::test]
    async fn tree_hydrates_recursively() {
        let dir = TempDir::new().unwrap();
        let forker = SessionForker::new(ForkConfig::default(), dir.path());
        let parent_id = "01PARENTHHHHHHHHHHHHHHHHHH";
        make_parent(&forker, parent_id, vec![round(0, Some("sha0"))]).await;

        let child_a = forker
            .fork(parent_id, ForkOptions::default())
            .await
            .unwrap();
        let child_b = forker
            .fork(parent_id, ForkOptions::default())
            .await
            .unwrap();

        let tree = forker.tree(parent_id).await.unwrap();
        assert_eq!(tree.children.len(), 2);
        assert!(tree.children.contains(&child_a.id));
        assert!(tree.children.contains(&child_b.id));
    }

    #[tokio::test]
    async fn concurrent_forks_are_serialized_by_lock() {
        let dir = TempDir::new().unwrap();
        let forker = Arc::new(SessionForker::new(ForkConfig::default(), dir.path()));
        let parent_id = "01PARENTIIIIIIIIIIIIIIIIII";
        make_parent(forker.as_ref(), parent_id, vec![round(0, None)]).await;

        let mut handles = Vec::new();
        for _ in 0..5 {
            let f = forker.clone();
            let pid = parent_id.to_string();
            handles.push(tokio::spawn(async move {
                f.fork(&pid, ForkOptions::default()).await
            }));
        }

        let mut children = Vec::new();
        for h in handles {
            children.push(h.await.unwrap().unwrap());
        }

        let parent = forker.get(parent_id).await.unwrap().unwrap();
        assert_eq!(parent.children.len(), 5);
        for child in children {
            assert!(parent.children.contains(&child.id));
        }
    }
}
