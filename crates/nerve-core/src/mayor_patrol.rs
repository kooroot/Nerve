//! Tier 3j (v1.0) — Mayor + Patrol multi-instance orchestrator.
//!
//! The Mayor is a long-lived process that owns a JSON-on-disk task queue
//! under `<workspace>/<queue_dir>/{pending,claimed,done,failed}/`. Patrols
//! are worker processes that claim a task by performing an atomic
//! `rename(2)` from `pending/<id>.json` to
//! `claimed/<patrol_id>/<id>.json`, run the task through a caller-supplied
//! dispatch closure, and write the [`PatrolResult`] to
//! `<results_dir>/<id>.json` plus moving the queue file to `done/` or
//! `failed/`.
//!
//! Key invariants — verbatim from the v1.0 proposal §3 Tier 3j:
//!
//! 1. **Atomic claim**: `rename(2)` is the only race-free claim
//!    mechanism. The mayor advisory lock at
//!    `.nerve/session-meta/mayor.lock` (`fs2`-backed flock with a 5s
//!    timeout) serializes the directory scan + rename so two patrols
//!    cannot pick the same lex-smallest file.
//! 2. **Crash safety**: queue and result files on disk are the source of
//!    truth. Mayor restart resumes from whatever lives under `pending/`
//!    and `claimed/`. Orphan recovery moves `claimed/<patrol>/<task>.json`
//!    back to `pending/` when the patrol heartbeat is older than
//!    `config.claim_ttl_secs`.
//! 3. **Per-patrol budget ceiling**: `PatrolTask.budget_microusd` is the
//!    hard cap. `Mayor::enqueue` refuses any task whose budget exceeds
//!    `total_budget_microusd` (if set). `Patrol::run_one` marks a result
//!    as failed when `dispatch` returns a `cost_microusd` above the
//!    task's ceiling — defense in depth against an orchestrator bug.
//! 4. **No partial writes**: every JSON file is written via tempfile +
//!    `rename(2)`. No reader ever observes a half-written file.

use std::collections::BTreeSet;
use std::fs::{self, File, OpenOptions};
use std::future::Future;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use chrono::{DateTime, Utc};
use fs2::FileExt;
use nerve_config::MayorPatrolConfig;
use nerve_types::rpc_kinds;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::watch;

use crate::rpc::RpcBus;

/// `mayor.lock` advisory-lock acquisition timeout. The mayor only holds
/// the lock for the duration of a directory scan + atomic claim, so 5s
/// is generous.
const LOCK_TIMEOUT: Duration = Duration::from_secs(5);
const LOCK_POLL: Duration = Duration::from_millis(25);

/// String form for orphan-recovery RPC envelopes. Defined here because
/// the catalog in `nerve_types::rpc_kinds` does not (yet) own this name.
const RPC_MAYOR_ORPHAN_RECOVERED: &str = "mayor.orphan_recovered";

/// A unit of work the Mayor hands to a Patrol.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PatrolTask {
    pub task_id: String,
    pub prompt: String,
    /// Per-task budget ceiling in micro-USD. The Mayor refuses to
    /// enqueue any task whose `budget_microusd > total_budget_microusd`,
    /// and the Patrol marks a result `failed` if the dispatch closure
    /// returns a cost above this ceiling.
    pub budget_microusd: u64,
    #[serde(default)]
    pub mcp_servers: Vec<String>,
    #[serde(default)]
    pub created_at: Option<DateTime<Utc>>,
}

impl PatrolTask {
    pub fn new(
        task_id: impl Into<String>,
        prompt: impl Into<String>,
        budget_microusd: u64,
    ) -> Self {
        Self {
            task_id: task_id.into(),
            prompt: prompt.into(),
            budget_microusd,
            mcp_servers: Vec::new(),
            created_at: Some(Utc::now()),
        }
    }
}

/// Final verdict of a single patrol task.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PatrolVerdict {
    Success,
    Failed,
    Skipped,
}

/// Persistent result of a patrol task. Written to
/// `<results_dir>/<task_id>.json` after the dispatch closure returns.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PatrolResult {
    pub task_id: String,
    pub patrol_id: String,
    pub verdict: PatrolVerdict,
    pub cost_microusd: u64,
    #[serde(default)]
    pub patch_sha: Option<String>,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub finished_at: Option<DateTime<Utc>>,
}

impl PatrolResult {
    pub fn success(task_id: impl Into<String>, patrol_id: impl Into<String>, cost: u64) -> Self {
        Self {
            task_id: task_id.into(),
            patrol_id: patrol_id.into(),
            verdict: PatrolVerdict::Success,
            cost_microusd: cost,
            patch_sha: None,
            error: None,
            finished_at: Some(Utc::now()),
        }
    }

    pub fn failed(
        task_id: impl Into<String>,
        patrol_id: impl Into<String>,
        cost: u64,
        err: impl Into<String>,
    ) -> Self {
        Self {
            task_id: task_id.into(),
            patrol_id: patrol_id.into(),
            verdict: PatrolVerdict::Failed,
            cost_microusd: cost,
            patch_sha: None,
            error: Some(err.into()),
            finished_at: Some(Utc::now()),
        }
    }
}

/// Snapshot of queue/worker state returned by [`Mayor::status`]. The
/// `*_count` fields are O(dir-listing) and intentionally cheap so a TUI
/// or `/mayor status` slash command can poll every second.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MayorStatus {
    pub pending_count: usize,
    pub claimed_count: usize,
    pub done_count: usize,
    pub failed_count: usize,
    pub active_patrols: Vec<String>,
    pub orphans_recovered: usize,
}

#[derive(Debug, Error)]
pub enum MayorError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("queue empty")]
    QueueEmpty,
    #[error("claim failed: {0}")]
    ClaimFailed(String),
    #[error("lock timeout")]
    LockTimeout,
    #[error("budget ceiling exceeded for patrol {0}: requested {1} > ceiling {2}")]
    BudgetCeilingExceeded(String, u64, u64),
    #[error("orphaned claim recovered: {0}")]
    OrphanRecovered(String),
    #[error("serde: {0}")]
    Serde(#[from] serde_json::Error),
}

/// Dispatch closure signature. Boxed-future variant so the trait object
/// can be stored without forcing a generic everywhere. Since
/// [`PatrolTask`] is owned and dispatch takes it by value, the returned
/// future is `'static`.
pub type DispatchFuture =
    Pin<Box<dyn Future<Output = Result<PatrolResult, MayorError>> + Send + 'static>>;

// =====================================================================
// Mayor
// =====================================================================

/// Mayor — long-lived dispatcher that owns the on-disk task queue.
#[derive(Debug)]
pub struct Mayor {
    config: MayorPatrolConfig,
    workspace_root: PathBuf,
    total_budget_microusd: Option<u64>,
    shutdown_tx: watch::Sender<bool>,
    rpc: Option<Arc<RpcBus>>,
}

impl Mayor {
    pub fn new(
        config: MayorPatrolConfig,
        workspace_root: PathBuf,
        total_budget_microusd: Option<u64>,
    ) -> Self {
        let (shutdown_tx, _) = watch::channel(false);
        Self {
            config,
            workspace_root,
            total_budget_microusd,
            shutdown_tx,
            rpc: None,
        }
    }

    pub fn with_rpc(mut self, rpc: Arc<RpcBus>) -> Self {
        self.rpc = Some(rpc);
        self
    }

    pub fn config(&self) -> &MayorPatrolConfig {
        &self.config
    }

    pub fn workspace_root(&self) -> &Path {
        &self.workspace_root
    }

    pub fn shutdown_signal(&self) -> watch::Receiver<bool> {
        self.shutdown_tx.subscribe()
    }

    /// Write a task to `pending/<task_id>.json`.
    pub async fn enqueue(&self, task: PatrolTask) -> Result<(), MayorError> {
        if let Some(total) = self.total_budget_microusd
            && task.budget_microusd > total
        {
            return Err(MayorError::BudgetCeilingExceeded(
                task.task_id.clone(),
                task.budget_microusd,
                total,
            ));
        }
        let layout = QueueLayout::new(&self.workspace_root, &self.config);
        layout.ensure()?;
        let path = layout.pending_path(&task.task_id);
        write_json_atomic(&path, &task)?;

        if let Some(rpc) = &self.rpc {
            let _ = rpc.emit(
                rpc_kinds::MAYOR_DISPATCHED,
                serde_json::json!({
                    "task_id": task.task_id,
                    "budget_microusd": task.budget_microusd,
                }),
            );
        }
        Ok(())
    }

    /// Compute a queue snapshot. Side effect: scans `claimed/` and moves
    /// any task whose patrol heartbeat is older than
    /// `config.claim_ttl_secs` back to `pending/`.
    pub async fn status(&self) -> Result<MayorStatus, MayorError> {
        let layout = QueueLayout::new(&self.workspace_root, &self.config);
        layout.ensure()?;

        let orphans_recovered = self.recover_orphans(&layout)?;

        Ok(MayorStatus {
            pending_count: count_json_files(&layout.pending)?,
            claimed_count: count_claimed_recursive(&layout.claimed)?,
            done_count: count_json_files(&layout.done)?,
            failed_count: count_json_files(&layout.failed)?,
            active_patrols: list_heartbeats(&layout.heartbeat)?,
            orphans_recovered,
        })
    }

    fn recover_orphans(&self, layout: &QueueLayout) -> Result<usize, MayorError> {
        let mut recovered = 0usize;
        if !layout.claimed.exists() {
            return Ok(0);
        }
        let ttl = Duration::from_secs(u64::from(self.config.claim_ttl_secs));
        let now = SystemTime::now();
        for patrol_dir in fs::read_dir(&layout.claimed)? {
            let patrol_dir = patrol_dir?;
            if !patrol_dir.file_type()?.is_dir() {
                continue;
            }
            let patrol_id = patrol_dir.file_name().to_string_lossy().into_owned();
            let heartbeat = layout.heartbeat_path(&patrol_id);
            let alive = match fs::metadata(&heartbeat) {
                Ok(meta) => {
                    let mtime = meta.modified().unwrap_or(UNIX_EPOCH);
                    now.duration_since(mtime).unwrap_or(Duration::ZERO) < ttl
                }
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => false,
                Err(err) => return Err(err.into()),
            };
            if alive {
                continue;
            }
            for entry in fs::read_dir(patrol_dir.path())? {
                let entry = entry?;
                let path = entry.path();
                if path.extension().and_then(|s| s.to_str()) != Some("json") {
                    continue;
                }
                let task_id = entry
                    .path()
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("unknown")
                    .to_string();
                let target = layout.pending_path(&task_id);
                fs::rename(&path, &target)?;
                recovered += 1;
                if let Some(rpc) = &self.rpc {
                    let _ = rpc.emit(
                        RPC_MAYOR_ORPHAN_RECOVERED,
                        serde_json::json!({
                            "task_id": task_id,
                            "patrol_id": patrol_id,
                        }),
                    );
                }
            }
            // Remove the (now empty) per-patrol claimed dir so doctor
            // surfaces zero orphans on the next poll.
            let _ = fs::remove_dir(patrol_dir.path());
        }
        Ok(recovered)
    }

    /// Polling loop — returns once the queue is empty AND no claims are
    /// outstanding. Useful for the foreground `nv mayor` CLI in test
    /// scenarios and for graceful shutdown drains.
    pub async fn run_until_idle(&self) -> Result<(), MayorError> {
        loop {
            if *self.shutdown_tx.borrow() {
                return Ok(());
            }
            let status = self.status().await?;
            if status.pending_count == 0 && status.claimed_count == 0 {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    /// Signal patrols to stop. The mayor watch channel flips to `true`
    /// and any subscriber (including [`Patrol::run_loop`]) observes the
    /// transition through [`Self::shutdown_signal`].
    pub async fn shutdown(&self) -> Result<(), MayorError> {
        let _ = self.shutdown_tx.send(true);
        if let Some(rpc) = &self.rpc {
            let _ = rpc.emit(
                rpc_kinds::MAYOR_SHUTTING_DOWN,
                serde_json::json!({"reason": "shutdown_requested"}),
            );
        }
        Ok(())
    }
}

// =====================================================================
// Patrol
// =====================================================================

/// Patrol — worker process that claims a task and runs it through a
/// caller-supplied dispatch closure.
#[derive(Debug)]
pub struct Patrol {
    id: String,
    config: MayorPatrolConfig,
    workspace_root: PathBuf,
    rpc: Option<Arc<RpcBus>>,
}

impl Patrol {
    pub fn new(id: String, config: MayorPatrolConfig, workspace_root: PathBuf) -> Self {
        Self {
            id,
            config,
            workspace_root,
            rpc: None,
        }
    }

    pub fn with_rpc(mut self, rpc: Arc<RpcBus>) -> Self {
        self.rpc = Some(rpc);
        self
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    /// Try to claim the lex-smallest task from `pending/`. Returns
    /// `Err(MayorError::QueueEmpty)` when the queue is empty.
    pub fn claim(&self) -> Result<PatrolTask, MayorError> {
        let layout = QueueLayout::new(&self.workspace_root, &self.config);
        layout.ensure()?;
        let _lock = MayorLock::acquire(&self.workspace_root)?;

        let pending = sorted_json_filenames(&layout.pending)?;
        let candidate = pending.first().cloned().ok_or(MayorError::QueueEmpty)?;
        let src = layout.pending.join(&candidate);
        let patrol_claimed_dir = layout.claimed.join(&self.id);
        fs::create_dir_all(&patrol_claimed_dir)?;
        let dst = patrol_claimed_dir.join(&candidate);

        match fs::rename(&src, &dst) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                // Lost the race — another patrol picked it after our scan.
                return Err(MayorError::ClaimFailed(format!(
                    "lost race for `{}`",
                    candidate
                )));
            }
            Err(err) => return Err(MayorError::Io(err)),
        }

        let task: PatrolTask = read_json(&dst)?;
        self.write_heartbeat(&layout)?;

        if let Some(rpc) = &self.rpc {
            let _ = rpc.emit(
                rpc_kinds::PATROL_CLAIMED,
                serde_json::json!({
                    "patrol_id": self.id,
                    "task_id": task.task_id,
                }),
            );
        }
        Ok(task)
    }

    /// Claim one task and run it through `dispatch`. Persists the
    /// result, moves the queue file to `done/` or `failed/`, and emits
    /// the matching RPC envelopes.
    pub async fn run_one<F>(&self, dispatch: F) -> Result<PatrolResult, MayorError>
    where
        F: FnOnce(PatrolTask) -> DispatchFuture + Send,
    {
        let task = self.claim()?;
        self.run_task(task, dispatch).await
    }

    async fn run_task<F>(&self, task: PatrolTask, dispatch: F) -> Result<PatrolResult, MayorError>
    where
        F: FnOnce(PatrolTask) -> DispatchFuture + Send,
    {
        let layout = QueueLayout::new(&self.workspace_root, &self.config);
        let ceiling = task.budget_microusd;
        let task_id = task.task_id.clone();

        let dispatch_future = dispatch(task.clone());
        let outcome = dispatch_future.await;

        let mut result = match outcome {
            Ok(mut r) => {
                if r.cost_microusd > ceiling {
                    // Defense in depth — primary enforcement is in the
                    // orchestrator. Mark as failed if cost overran.
                    r.verdict = PatrolVerdict::Failed;
                    r.error = Some(format!(
                        "cost {} exceeded ceiling {}",
                        r.cost_microusd, ceiling
                    ));
                }
                r
            }
            Err(err) => PatrolResult::failed(&task_id, &self.id, 0, err.to_string()),
        };
        if result.finished_at.is_none() {
            result.finished_at = Some(Utc::now());
        }
        result.task_id.clone_from(&task_id);
        result.patrol_id.clone_from(&self.id);

        let result_path = layout.result_path(&task_id);
        write_json_atomic(&result_path, &result)?;

        // Move claimed/<patrol>/<task>.json into done/ or failed/.
        let claimed_path = layout
            .claimed
            .join(&self.id)
            .join(format!("{task_id}.json"));
        let destination = match result.verdict {
            PatrolVerdict::Success | PatrolVerdict::Skipped => layout.done_path(&task_id),
            PatrolVerdict::Failed => layout.failed_path(&task_id),
        };
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent)?;
        }
        match fs::rename(&claimed_path, &destination) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                // claimed file might have been swept by orphan recovery
                // — that is a no-op from the patrol's POV.
            }
            Err(err) => return Err(MayorError::Io(err)),
        }
        // Best-effort cleanup of the per-patrol claimed dir if empty.
        let _ = fs::remove_dir(layout.claimed.join(&self.id));

        self.write_heartbeat(&layout)?;
        if let Some(rpc) = &self.rpc {
            let _ = rpc.emit(
                rpc_kinds::PATROL_FINISHED,
                serde_json::json!({
                    "patrol_id": self.id,
                    "task_id": result.task_id,
                    "verdict": result.verdict,
                    "cost_microusd": result.cost_microusd,
                }),
            );
        }
        Ok(result)
    }

    /// Long-running claim/run loop. Stops cleanly when `shutdown` flips
    /// to `true`, after the current task finishes.
    pub async fn run_loop<F>(
        &self,
        mut dispatch_factory: F,
        mut shutdown: watch::Receiver<bool>,
    ) -> Result<(), MayorError>
    where
        F: FnMut(PatrolTask) -> DispatchFuture + Send,
    {
        loop {
            if *shutdown.borrow() {
                return Ok(());
            }
            match self.claim() {
                Ok(task) => {
                    let result = self.run_task(task.clone(), |t| dispatch_factory(t)).await?;
                    let _ = result; // exposed via on-disk result file
                }
                Err(MayorError::QueueEmpty) => {
                    tokio::select! {
                        _ = shutdown.changed() => {
                            if *shutdown.borrow() {
                                return Ok(());
                            }
                        }
                        _ = tokio::time::sleep(Duration::from_millis(
                            (u64::from(self.config.heartbeat_secs).saturating_mul(1000)).max(100),
                        )) => {
                            let layout = QueueLayout::new(&self.workspace_root, &self.config);
                            self.write_heartbeat(&layout)?;
                            if let Some(rpc) = &self.rpc {
                                let _ = rpc.emit(
                                    rpc_kinds::PATROL_HEARTBEAT,
                                    serde_json::json!({
                                        "patrol_id": self.id,
                                    }),
                                );
                            }
                        }
                    }
                }
                Err(err) => return Err(err),
            }
        }
    }

    fn write_heartbeat(&self, layout: &QueueLayout) -> Result<(), MayorError> {
        fs::create_dir_all(&layout.heartbeat)?;
        let path = layout.heartbeat_path(&self.id);
        // Touch the file — open with create + truncate so mtime updates
        // even if the heartbeat already exists.
        let _ = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&path)?;
        Ok(())
    }
}

// =====================================================================
// Filesystem helpers
// =====================================================================

/// Resolves the on-disk directories used by [`Mayor`] and [`Patrol`].
#[derive(Debug, Clone)]
struct QueueLayout {
    pending: PathBuf,
    claimed: PathBuf,
    done: PathBuf,
    failed: PathBuf,
    results: PathBuf,
    heartbeat: PathBuf,
}

impl QueueLayout {
    fn new(workspace_root: &Path, config: &MayorPatrolConfig) -> Self {
        let queue_root = resolve_relative(workspace_root, &config.queue_dir);
        let results_root = resolve_relative(workspace_root, &config.results_dir);
        Self {
            pending: queue_root.join("pending"),
            claimed: queue_root.join("claimed"),
            done: queue_root.join("done"),
            failed: queue_root.join("failed"),
            heartbeat: queue_root.join("heartbeat"),
            results: results_root,
        }
    }

    fn ensure(&self) -> Result<(), MayorError> {
        for dir in [
            &self.pending,
            &self.claimed,
            &self.done,
            &self.failed,
            &self.heartbeat,
            &self.results,
        ] {
            fs::create_dir_all(dir)?;
        }
        Ok(())
    }

    fn pending_path(&self, task_id: &str) -> PathBuf {
        self.pending.join(format!("{task_id}.json"))
    }

    fn done_path(&self, task_id: &str) -> PathBuf {
        self.done.join(format!("{task_id}.json"))
    }

    fn failed_path(&self, task_id: &str) -> PathBuf {
        self.failed.join(format!("{task_id}.json"))
    }

    fn result_path(&self, task_id: &str) -> PathBuf {
        self.results.join(format!("{task_id}.json"))
    }

    fn heartbeat_path(&self, patrol_id: &str) -> PathBuf {
        self.heartbeat.join(patrol_id)
    }
}

fn resolve_relative(root: &Path, candidate: &Path) -> PathBuf {
    if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        root.join(candidate)
    }
}

fn write_json_atomic<T: Serialize>(path: &Path, value: &T) -> Result<(), MayorError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let raw = serde_json::to_vec_pretty(value)?;
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let mut tmp = tempfile::NamedTempFile::new_in(parent)?;
    tmp.write_all(&raw)?;
    tmp.as_file().sync_all()?;
    tmp.persist(path).map_err(|e| MayorError::Io(e.error))?;
    Ok(())
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<T, MayorError> {
    let raw = fs::read_to_string(path)?;
    let value = serde_json::from_str(&raw)?;
    Ok(value)
}

fn count_json_files(dir: &Path) -> Result<usize, MayorError> {
    if !dir.exists() {
        return Ok(0);
    }
    let mut count = 0usize;
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        if entry.path().extension().and_then(|s| s.to_str()) == Some("json") {
            count += 1;
        }
    }
    Ok(count)
}

fn count_claimed_recursive(dir: &Path) -> Result<usize, MayorError> {
    if !dir.exists() {
        return Ok(0);
    }
    let mut count = 0usize;
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            count += count_json_files(&entry.path())?;
        }
    }
    Ok(count)
}

fn sorted_json_filenames(dir: &Path) -> Result<Vec<String>, MayorError> {
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut entries: BTreeSet<String> = BTreeSet::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        if let Some(name) = path.file_name().and_then(|s| s.to_str()) {
            entries.insert(name.to_string());
        }
    }
    Ok(entries.into_iter().collect())
}

fn list_heartbeats(dir: &Path) -> Result<Vec<String>, MayorError> {
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut entries = Vec::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        if entry.file_type()?.is_file()
            && let Some(name) = entry.file_name().to_str()
        {
            entries.push(name.to_string());
        }
    }
    entries.sort();
    Ok(entries)
}

// =====================================================================
// `mayor.lock` advisory lock
// =====================================================================

#[derive(Debug)]
struct MayorLock {
    file: File,
}

impl MayorLock {
    fn acquire(workspace_root: &Path) -> Result<Self, MayorError> {
        let dir = workspace_root.join(".nerve").join("session-meta");
        fs::create_dir_all(&dir)?;
        let path = dir.join("mayor.lock");
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&path)?;

        let deadline = std::time::Instant::now() + LOCK_TIMEOUT;
        loop {
            match FileExt::try_lock_exclusive(&file) {
                Ok(()) => return Ok(Self { file }),
                Err(_) => {
                    if std::time::Instant::now() >= deadline {
                        return Err(MayorError::LockTimeout);
                    }
                    std::thread::sleep(LOCK_POLL);
                }
            }
        }
    }
}

impl Drop for MayorLock {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.file);
    }
}

// =====================================================================
// Tests
// =====================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// Test helper: rewind `path`'s mtime + atime by `seconds`. We hit
    /// `utimes(2)` directly so the test suite does not need the
    /// `filetime` crate as a workspace dep.
    #[cfg(unix)]
    fn backdate_to_seconds_ago(path: &Path, seconds: u64) {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock before epoch")
            .as_secs();
        let target = now.saturating_sub(seconds) as libc::time_t;
        let times = [
            libc::timeval {
                tv_sec: target,
                tv_usec: 0,
            },
            libc::timeval {
                tv_sec: target,
                tv_usec: 0,
            },
        ];
        let c_path = CString::new(path.as_os_str().as_bytes()).unwrap();
        // SAFETY: `c_path` is a valid C string; `times` is a 2-element
        // timeval array of the expected length.
        let rc = unsafe { libc::utimes(c_path.as_ptr(), times.as_ptr()) };
        assert_eq!(rc, 0, "utimes failed: {}", std::io::Error::last_os_error());
    }

    fn cfg() -> MayorPatrolConfig {
        MayorPatrolConfig {
            queue_dir: PathBuf::from(".nerve/queue"),
            results_dir: PathBuf::from(".nerve/results"),
            max_patrols: 2,
            per_patrol_budget_microusd: None,
            heartbeat_secs: 1,
            claim_ttl_secs: 2,
        }
    }

    fn stub_dispatch(ok: bool, cost: u64) -> Box<dyn FnOnce(PatrolTask) -> DispatchFuture + Send> {
        Box::new(move |task: PatrolTask| -> DispatchFuture {
            Box::pin(async move {
                if ok {
                    Ok(PatrolResult::success(&task.task_id, "stub", cost))
                } else {
                    Ok(PatrolResult::failed(
                        &task.task_id,
                        "stub",
                        cost,
                        "stub failure",
                    ))
                }
            })
        })
    }

    #[tokio::test]
    async fn enqueue_writes_pending_file() {
        let dir = tempfile::tempdir().unwrap();
        let mayor = Mayor::new(cfg(), dir.path().to_path_buf(), Some(1_000_000));
        let task = PatrolTask::new("t-1", "prompt", 1000);
        mayor.enqueue(task).await.unwrap();

        let status = mayor.status().await.unwrap();
        assert_eq!(status.pending_count, 1);
        assert_eq!(status.claimed_count, 0);
        assert!(dir.path().join(".nerve/queue/pending/t-1.json").exists());
    }

    #[tokio::test]
    async fn enqueue_rejects_over_total_budget() {
        let dir = tempfile::tempdir().unwrap();
        let mayor = Mayor::new(cfg(), dir.path().to_path_buf(), Some(500));
        let task = PatrolTask::new("t-over", "prompt", 1000);
        let err = mayor.enqueue(task).await.unwrap_err();
        match err {
            MayorError::BudgetCeilingExceeded(id, requested, ceiling) => {
                assert_eq!(id, "t-over");
                assert_eq!(requested, 1000);
                assert_eq!(ceiling, 500);
            }
            other => panic!("expected BudgetCeilingExceeded, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn patrol_claims_pending_into_claimed() {
        let dir = tempfile::tempdir().unwrap();
        let mayor = Mayor::new(cfg(), dir.path().to_path_buf(), None);
        let task = PatrolTask::new("t-claim", "prompt", 1000);
        mayor.enqueue(task).await.unwrap();

        let patrol = Patrol::new("p-1".to_string(), cfg(), dir.path().to_path_buf());
        let claimed = patrol.claim().unwrap();
        assert_eq!(claimed.task_id, "t-claim");

        let status = mayor.status().await.unwrap();
        assert_eq!(status.pending_count, 0);
        assert_eq!(status.claimed_count, 1);
        assert!(
            dir.path()
                .join(".nerve/queue/claimed/p-1/t-claim.json")
                .exists()
        );
        // Heartbeat was written when claim completed.
        assert!(dir.path().join(".nerve/queue/heartbeat/p-1").exists());
    }

    #[tokio::test]
    async fn patrol_run_one_writes_result_and_moves_to_done() {
        let dir = tempfile::tempdir().unwrap();
        let mayor = Mayor::new(cfg(), dir.path().to_path_buf(), None);
        let task = PatrolTask::new("t-run", "prompt", 1000);
        mayor.enqueue(task).await.unwrap();

        let patrol = Patrol::new("p-1".to_string(), cfg(), dir.path().to_path_buf());
        let result = patrol.run_one(stub_dispatch(true, 250)).await.unwrap();
        assert_eq!(result.verdict, PatrolVerdict::Success);
        assert_eq!(result.cost_microusd, 250);

        let status = mayor.status().await.unwrap();
        assert_eq!(status.pending_count, 0);
        assert_eq!(status.claimed_count, 0);
        assert_eq!(status.done_count, 1);
        assert!(dir.path().join(".nerve/results/t-run.json").exists());
        assert!(dir.path().join(".nerve/queue/done/t-run.json").exists());
    }

    #[tokio::test]
    async fn patrol_run_one_marks_failed_when_cost_exceeds_ceiling() {
        let dir = tempfile::tempdir().unwrap();
        let mayor = Mayor::new(cfg(), dir.path().to_path_buf(), None);
        // Ceiling = 100, dispatch returns cost 250 → must be Failed.
        let task = PatrolTask::new("t-over", "prompt", 100);
        mayor.enqueue(task).await.unwrap();

        let patrol = Patrol::new("p-1".to_string(), cfg(), dir.path().to_path_buf());
        let result = patrol.run_one(stub_dispatch(true, 250)).await.unwrap();

        assert_eq!(result.verdict, PatrolVerdict::Failed);
        assert!(
            result
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("exceeded ceiling")
        );

        let status = mayor.status().await.unwrap();
        assert_eq!(status.failed_count, 1);
        assert!(dir.path().join(".nerve/queue/failed/t-over.json").exists());
    }

    #[tokio::test]
    async fn patrol_failed_dispatch_lands_in_failed_dir() {
        let dir = tempfile::tempdir().unwrap();
        let mayor = Mayor::new(cfg(), dir.path().to_path_buf(), None);
        let task = PatrolTask::new("t-fail", "prompt", 1000);
        mayor.enqueue(task).await.unwrap();

        let patrol = Patrol::new("p-1".to_string(), cfg(), dir.path().to_path_buf());
        let result = patrol.run_one(stub_dispatch(false, 0)).await.unwrap();
        assert_eq!(result.verdict, PatrolVerdict::Failed);

        let status = mayor.status().await.unwrap();
        assert_eq!(status.failed_count, 1);
        assert_eq!(status.done_count, 0);
    }

    #[tokio::test]
    async fn status_recovers_orphaned_claim() {
        let dir = tempfile::tempdir().unwrap();
        let config = cfg();
        let mayor = Mayor::new(config.clone(), dir.path().to_path_buf(), None);
        // Pre-create a stale claimed entry + an old heartbeat.
        let queue_root = dir.path().join(".nerve").join("queue");
        let claimed_dir = queue_root.join("claimed").join("p-dead");
        fs::create_dir_all(&claimed_dir).unwrap();
        let stale_task = PatrolTask::new("t-stale", "prompt", 500);
        write_json_atomic(&claimed_dir.join("t-stale.json"), &stale_task).unwrap();

        let heartbeat_dir = queue_root.join("heartbeat");
        fs::create_dir_all(&heartbeat_dir).unwrap();
        let heartbeat_path = heartbeat_dir.join("p-dead");
        File::create(&heartbeat_path).unwrap();
        // Backdate the heartbeat well beyond claim_ttl_secs (2s) by
        // poking utimes(2) directly. We avoid pulling in the `filetime`
        // crate to keep the workspace dep set tight.
        backdate_to_seconds_ago(&heartbeat_path, 120);

        let status = mayor.status().await.unwrap();
        assert_eq!(status.orphans_recovered, 1);
        assert_eq!(status.pending_count, 1);
        assert_eq!(status.claimed_count, 0);
        assert!(queue_root.join("pending/t-stale.json").exists());
        assert!(!queue_root.join("claimed/p-dead/t-stale.json").exists());
    }

    #[tokio::test]
    async fn two_patrols_share_lock_no_double_claim() {
        let dir = tempfile::tempdir().unwrap();
        let mayor = Mayor::new(cfg(), dir.path().to_path_buf(), None);
        mayor
            .enqueue(PatrolTask::new("t-shared", "prompt", 100))
            .await
            .unwrap();

        let p1 = Patrol::new("p-1".to_string(), cfg(), dir.path().to_path_buf());
        let p2 = Patrol::new("p-2".to_string(), cfg(), dir.path().to_path_buf());

        let one = p1.claim().unwrap();
        // Second patrol must observe an empty queue — the file already
        // moved into claimed/p-1/.
        let err = p2.claim().unwrap_err();
        assert!(
            matches!(err, MayorError::QueueEmpty),
            "expected QueueEmpty, got {err:?}"
        );
        assert_eq!(one.task_id, "t-shared");
    }

    #[tokio::test]
    async fn shutdown_signals_subscribers() {
        let dir = tempfile::tempdir().unwrap();
        let mayor = Mayor::new(cfg(), dir.path().to_path_buf(), None);
        let mut rx = mayor.shutdown_signal();
        assert!(!*rx.borrow());
        mayor.shutdown().await.unwrap();
        rx.changed().await.unwrap();
        assert!(*rx.borrow());
    }

    #[tokio::test]
    async fn dispatch_closure_observes_task_fields() {
        // Make sure the closure receives the same PatrolTask we enqueued.
        let dir = tempfile::tempdir().unwrap();
        let mayor = Mayor::new(cfg(), dir.path().to_path_buf(), None);
        let task = PatrolTask::new("t-obs", "the prompt", 777);
        mayor.enqueue(task).await.unwrap();

        let seen = Arc::new(AtomicU64::new(0));
        let seen_ref = Arc::clone(&seen);
        let patrol = Patrol::new("p-1".to_string(), cfg(), dir.path().to_path_buf());
        let result = patrol
            .run_one(Box::new(move |task: PatrolTask| -> DispatchFuture {
                let seen = Arc::clone(&seen_ref);
                Box::pin(async move {
                    seen.store(task.budget_microusd, Ordering::SeqCst);
                    Ok(PatrolResult::success(&task.task_id, "p-1", 1))
                })
            }))
            .await
            .unwrap();

        assert_eq!(result.verdict, PatrolVerdict::Success);
        assert_eq!(seen.load(Ordering::SeqCst), 777);
    }
}
