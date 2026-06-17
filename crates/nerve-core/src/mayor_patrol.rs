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

use std::collections::{BTreeMap, BTreeSet};
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
    #[error("task already exists: {0}")]
    TaskAlreadyExists(String),
    #[error("budget ceiling exceeded for patrol {0}: requested {1} > ceiling {2}")]
    BudgetCeilingExceeded(String, u64, u64),
    #[error("invalid {0}: must be 1..=128 chars of [A-Za-z0-9_-]")]
    InvalidIdentifier(&'static str),
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

    /// S14: a coordinator bound to this Mayor's workspace + config.
    fn coordinator(&self) -> Coordinator {
        Coordinator::new(self.workspace_root.clone(), &self.config)
    }

    /// S14: snapshot of the coordination ledger. Observability ONLY — the queue
    /// directories + the deterministic `blocked` gate remain the sole authority.
    pub fn ledger(&self) -> Result<Ledger, MayorError> {
        self.coordinator().ledger()
    }

    /// H17: rebuild the coordination ledger from the authoritative queue dirs and
    /// return the reconciled snapshot. See [`Coordinator::reconcile`]: dirs win on
    /// every disagreement, it is a no-op when coordination is disabled, and the
    /// result is observability ONLY — never consulted by the apply gate.
    pub fn reconcile_ledger(&self) -> Result<Ledger, MayorError> {
        self.coordinator().reconcile()
    }

    /// S14: read and remove all coordination messages addressed to `recipient`.
    pub fn drain_mail(&self, recipient: &str) -> Result<Vec<MailMessage>, MayorError> {
        self.coordinator().drain_mail(recipient)
    }

    /// Write a task to `pending/<task_id>.json`.
    pub async fn enqueue(&self, task: PatrolTask) -> Result<(), MayorError> {
        validate_file_component("task_id", &task.task_id)?;
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
        if task_exists_anywhere(&layout, &task.task_id)? {
            return Err(MayorError::TaskAlreadyExists(task.task_id));
        }
        let path = layout.pending_path(&task.task_id);
        write_json_atomic(&path, &task)?;

        // S14: project the new pending task into the coordination ledger
        // (best-effort, non-authoritative — the pending file written above is
        // the authoritative state, so a ledger failure must not fail enqueue).
        if let Err(err) = self
            .coordinator()
            .record_enqueued(&task.task_id, task.created_at)
        {
            eprintln!("warning: coordination ledger record_enqueued failed: {err}");
        }

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
        let coord = self.coordinator();
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
                // S14: clear ownership in the ledger and drop a Reclaimed notice
                // into the patrol's mailbox (best-effort, non-authoritative — the
                // rename above is the authoritative recovery). The mail is purely
                // informational: the patrol's claim file is already gone.
                if let Err(err) = coord.record_recovered(&task_id) {
                    eprintln!("warning: coordination ledger record_recovered failed: {err}");
                }
                let _ = coord.send_mail(&MailMessage::new(
                    &task_id,
                    "mayor",
                    &patrol_id,
                    MailKind::Reclaimed,
                    format!("task `{task_id}` reclaimed from `{patrol_id}` (stale heartbeat)"),
                ));
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

    /// S14: a coordinator bound to this Patrol's workspace + config.
    fn coordinator(&self) -> Coordinator {
        Coordinator::new(self.workspace_root.clone(), &self.config)
    }

    /// Try to claim the lex-smallest task from `pending/`. Returns
    /// `Err(MayorError::QueueEmpty)` when the queue is empty.
    pub fn claim(&self) -> Result<PatrolTask, MayorError> {
        validate_file_component("patrol_id", &self.id)?;
        let layout = QueueLayout::new(&self.workspace_root, &self.config);
        layout.ensure()?;
        self.write_heartbeat(&layout)?;
        let _lock = FileLock::acquire(&self.workspace_root, "mayor.lock")?;

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
        validate_file_component("task_id", &task.task_id)?;
        let expected_task_id = candidate.trim_end_matches(".json");
        if task.task_id != expected_task_id {
            return Err(MayorError::ClaimFailed(format!(
                "pending filename `{expected_task_id}` did not match payload task_id `{}`",
                task.task_id
            )));
        }
        // S14: record the claim in the coordination ledger. This uses the
        // SEPARATE `ledger.lock`, so although the `mayor.lock` (`_lock`) is still
        // held here, there is no self-deadlock. Best-effort: the claimed file on
        // disk is the authoritative state.
        if let Err(err) = self.coordinator().record_claimed(&task.task_id, &self.id) {
            eprintln!("warning: coordination ledger record_claimed failed: {err}");
        }
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
        validate_file_component("task_id", &task_id)?;

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

        // S14: record completion in the coordination ledger (best-effort,
        // non-authoritative). Maps the verdict the SAME direction as the queue
        // move below, so the ledger never reports a failed task as done.
        if let Err(err) =
            self.coordinator()
                .record_finished(&task_id, result.verdict, result.cost_microusd)
        {
            eprintln!("warning: coordination ledger record_finished failed: {err}");
        }

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
        validate_file_component("patrol_id", &self.id)?;
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
    /// Queue root (`<workspace>/<queue_dir>`). The S14 ledger + mailbox live
    /// directly under it, OUTSIDE pending/claimed/done/failed, so they never
    /// affect `count_json_files`/`count_claimed_recursive` or the authoritative
    /// queue state.
    root: PathBuf,
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
            root: queue_root,
        }
    }

    fn ledger_path(&self) -> PathBuf {
        self.root.join("ledger.json")
    }

    fn mailbox_dir(&self) -> PathBuf {
        self.root.join("mailbox")
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

fn task_exists_anywhere(layout: &QueueLayout, task_id: &str) -> Result<bool, MayorError> {
    if layout.pending_path(task_id).exists()
        || layout.done_path(task_id).exists()
        || layout.failed_path(task_id).exists()
        || layout.result_path(task_id).exists()
    {
        return Ok(true);
    }
    if layout.claimed.exists() {
        for entry in fs::read_dir(&layout.claimed)? {
            let entry = entry?;
            if entry.file_type()?.is_dir() && entry.path().join(format!("{task_id}.json")).exists()
            {
                return Ok(true);
            }
        }
    }
    Ok(false)
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

/// The terminal [`LedgerState`] a [`PatrolVerdict`] maps to — the SAME direction
/// as the queue file move in `Patrol::run_task` (Success/Skipped → `done/`,
/// Failed → `failed/`). Routing `record_finished` and `reconcile` through one
/// mapping guarantees the ledger can never report a failed task as done.
fn terminal_state(verdict: PatrolVerdict) -> LedgerState {
    match verdict {
        PatrolVerdict::Success | PatrolVerdict::Skipped => LedgerState::Done,
        PatrolVerdict::Failed => LedgerState::Failed,
    }
}

/// One queue file as read by [`read_task_ids`]: its `task_id` (from the
/// filename) and the best-effort `created_at` from the payload.
type TaskIdRow = (String, Option<DateTime<Utc>>);

/// Read every valid `<task_id>.json` in a flat queue directory as a
/// `(task_id, created_at)` pair for ledger reconciliation. The id comes from the
/// FILENAME (the authoritative key the queue uses), and `created_at` is a
/// best-effort read of the `PatrolTask` payload — a file that will not parse
/// still contributes its id + state (with `None` timestamp), so reconcile never
/// drops a real on-disk task just because its payload is unreadable. Files whose
/// stem is not a valid queue id are skipped (defense-in-depth: such a file could
/// not have been written by `enqueue`/`claim`).
fn read_task_ids(dir: &Path) -> Result<Vec<TaskIdRow>, MayorError> {
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        let Some(task_id) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        if !is_valid_queue_id(task_id) {
            continue;
        }
        let created_at = read_json::<PatrolTask>(&path).ok().and_then(|t| t.created_at);
        out.push((task_id.to_string(), created_at));
    }
    Ok(out)
}

/// True iff `id` is a valid on-disk queue component: 1..=128 bytes of
/// `[A-Za-z0-9_-]`. This is the SAME predicate [`Mayor::enqueue`] enforces on
/// every `task_id` (via [`validate_file_component`]); it is exposed so callers
/// that DERIVE a task id (e.g. the plan dispatcher's `<plan_id>-step-NN`) can
/// fail closed with a precise, up-front error instead of partially dispatching
/// and surfacing Mayor's opaque `InvalidIdentifier` mid-loop. Routing both the
/// pre-check and the enqueue check through one predicate guarantees they can
/// never disagree.
pub fn is_valid_queue_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

fn validate_file_component(kind: &'static str, value: &str) -> Result<(), MayorError> {
    if is_valid_queue_id(value) {
        Ok(())
    } else {
        Err(MayorError::InvalidIdentifier(kind))
    }
}

// =====================================================================
// `.nerve/session-meta/*.lock` advisory lock
// =====================================================================

/// Advisory exclusive lock on a `.nerve/session-meta/<name>` file (`fs2` flock,
/// [`LOCK_TIMEOUT`] deadline). Serializes a short critical section across
/// processes and is released on drop.
///
/// The Mayor uses `mayor.lock` to serialize the claim scan + atomic rename so
/// two patrols can't double-claim. The S14 coordination ledger uses a SEPARATE
/// `ledger.lock`: a ledger write happens INSIDE `Patrol::claim`'s `mayor.lock`
/// critical section, so reusing `mayor.lock` for the ledger would self-deadlock
/// (a second exclusive flock on the same path from the same process blocks).
/// Distinct lock files keep the two critical sections independent.
#[derive(Debug)]
struct FileLock {
    file: File,
}

impl FileLock {
    fn acquire(workspace_root: &Path, name: &str) -> Result<Self, MayorError> {
        let dir = workspace_root.join(".nerve").join("session-meta");
        fs::create_dir_all(&dir)?;
        let path = dir.join(name);
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

impl Drop for FileLock {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.file);
    }
}

// =====================================================================
// S14 — coordination ledger + mailbox
// =====================================================================
//
// North star: the ledger and mailbox are COORDINATION / OBSERVABILITY ONLY.
// They are NEVER an acceptance or apply signal. The authoritative queue
// directories (pending/claimed/done/failed) plus the deterministic `blocked`
// gate in the synaptic loop remain the SOLE authority for what is accepted or
// applied — exactly as before S14. The ledger is a denormalized projection of
// queue state (so a TUI / `nv mayor --ledger` gets O(1) cross-patrol status
// instead of an O(dir-scan)); if it ever disagrees with the dirs, the DIRS win.
// All writes are best-effort: a ledger/mailbox failure WARNS at the call site
// and never aborts the authoritative enqueue/claim/finish/recover operation.

/// Schema version of the on-disk [`Ledger`].
const LEDGER_VERSION: u32 = 1;

/// Lifecycle state of a task as recorded in the coordination [`Ledger`]. Mirrors
/// the authoritative queue directory the task currently lives in.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LedgerState {
    Pending,
    Claimed,
    Done,
    Failed,
}

/// One task's row in the coordination [`Ledger`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LedgerEntry {
    pub task_id: String,
    pub state: LedgerState,
    /// Patrol that currently (or last) owned the task; cleared on recovery.
    #[serde(default)]
    pub owner: Option<String>,
    #[serde(default)]
    pub created_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub claimed_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub finished_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub cost_microusd: Option<u64>,
    #[serde(default)]
    pub verdict: Option<PatrolVerdict>,
}

impl LedgerEntry {
    fn pending(task_id: &str) -> Self {
        Self {
            task_id: task_id.to_string(),
            state: LedgerState::Pending,
            owner: None,
            created_at: None,
            claimed_at: None,
            finished_at: None,
            cost_microusd: None,
            verdict: None,
        }
    }
}

/// The shared coordination ledger — a projection of queue state keyed by task
/// id. `BTreeMap` keeps the serialized form deterministic (stable diffs/tests).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Ledger {
    pub version: u32,
    pub entries: BTreeMap<String, LedgerEntry>,
}

/// Kind of an inter-agent coordination message. A CLOSED enum of coordination
/// metadata ONLY: there is deliberately NO apply/consent variant, so a mailbox
/// message can never be parsed into an apply decision or used to relax `blocked`
/// (S11 north star: apply consent is unforgeable by the lead; the mailbox is not
/// a covert consent channel).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MailKind {
    /// Free-form coordination note.
    Note,
    /// Progress / status signal.
    Progress,
    /// The Mayor reclaimed a task from a patrol (stale heartbeat → orphan
    /// recovery). Informational: the patrol's claim file is already gone.
    Reclaimed,
}

/// A file-backed coordination message addressed to a recipient's inbox.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MailMessage {
    pub id: String,
    pub from: String,
    pub to: String,
    pub kind: MailKind,
    pub body: String,
    #[serde(default)]
    pub created_at: Option<DateTime<Utc>>,
}

impl MailMessage {
    pub fn new(
        id: impl Into<String>,
        from: impl Into<String>,
        to: impl Into<String>,
        kind: MailKind,
        body: impl Into<String>,
    ) -> Self {
        Self {
            id: id.into(),
            from: from.into(),
            to: to.into(),
            kind,
            body: body.into(),
            created_at: Some(Utc::now()),
        }
    }
}

/// S14 coordinator — best-effort, non-authoritative shared ledger + mailbox,
/// built cheaply from a [`Mayor`]/[`Patrol`]'s config + workspace root.
///
/// Every mutation is a no-op when `coordination_enabled` is false (queue
/// behavior is then byte-identical to pre-S14). Ledger RMW is serialized by a
/// DEDICATED `ledger.lock` ([`FileLock`]), SEPARATE from `mayor.lock`, so a
/// ledger write inside [`Patrol::claim`]'s `mayor.lock` critical section can
/// never self-deadlock. All ids are forced through [`validate_file_component`]
/// (the S13 `is_valid_queue_id` predicate) so a crafted task/patrol/recipient id
/// can never escape the queue root.
#[derive(Debug, Clone)]
pub struct Coordinator {
    workspace_root: PathBuf,
    layout: QueueLayout,
    enabled: bool,
}

impl Coordinator {
    fn new(workspace_root: PathBuf, config: &MayorPatrolConfig) -> Self {
        let layout = QueueLayout::new(&workspace_root, config);
        let enabled = config.coordination_enabled;
        Self {
            workspace_root,
            layout,
            enabled,
        }
    }

    /// Whether coordination is enabled (config `coordination_enabled`).
    pub fn enabled(&self) -> bool {
        self.enabled
    }

    // ----- ledger -----

    /// Load the current ledger snapshot. Lock-free: writers replace the file
    /// atomically, so a reader always sees a complete version. A missing file is
    /// an empty ledger (not an error).
    pub fn ledger(&self) -> Result<Ledger, MayorError> {
        let path = self.layout.ledger_path();
        if !path.exists() {
            return Ok(Ledger {
                version: LEDGER_VERSION,
                entries: BTreeMap::new(),
            });
        }
        read_json(&path)
    }

    fn mutate_ledger<F: FnOnce(&mut Ledger)>(&self, f: F) -> Result<(), MayorError> {
        if !self.enabled {
            return Ok(());
        }
        let _lock = FileLock::acquire(&self.workspace_root, "ledger.lock")?;
        let path = self.layout.ledger_path();
        let mut ledger = if path.exists() {
            read_json(&path)?
        } else {
            Ledger {
                version: LEDGER_VERSION,
                entries: BTreeMap::new(),
            }
        };
        f(&mut ledger);
        ledger.version = LEDGER_VERSION;
        write_json_atomic(&path, &ledger)?;
        Ok(())
    }

    /// Record a task entering `pending/` (enqueue). Idempotent: re-enqueue keeps
    /// the original `created_at`.
    pub fn record_enqueued(
        &self,
        task_id: &str,
        created_at: Option<DateTime<Utc>>,
    ) -> Result<(), MayorError> {
        validate_file_component("ledger task_id", task_id)?;
        self.mutate_ledger(|ledger| {
            let entry = ledger
                .entries
                .entry(task_id.to_string())
                .or_insert_with(|| LedgerEntry::pending(task_id));
            entry.state = LedgerState::Pending;
            entry.owner = None;
            entry.claimed_at = None;
            if entry.created_at.is_none() {
                entry.created_at = created_at.or_else(|| Some(Utc::now()));
            }
        })
    }

    /// Record a task being claimed by a patrol.
    pub fn record_claimed(&self, task_id: &str, patrol_id: &str) -> Result<(), MayorError> {
        validate_file_component("ledger task_id", task_id)?;
        validate_file_component("ledger patrol_id", patrol_id)?;
        self.mutate_ledger(|ledger| {
            let entry = ledger
                .entries
                .entry(task_id.to_string())
                .or_insert_with(|| LedgerEntry::pending(task_id));
            entry.state = LedgerState::Claimed;
            entry.owner = Some(patrol_id.to_string());
            entry.claimed_at = Some(Utc::now());
        })
    }

    /// Record a task finishing. `verdict` maps to `Done` (Success/Skipped) or
    /// `Failed` — the SAME direction as the queue file move, so the ledger never
    /// reports a failed task as done.
    pub fn record_finished(
        &self,
        task_id: &str,
        verdict: PatrolVerdict,
        cost_microusd: u64,
    ) -> Result<(), MayorError> {
        validate_file_component("ledger task_id", task_id)?;
        self.mutate_ledger(|ledger| {
            let entry = ledger
                .entries
                .entry(task_id.to_string())
                .or_insert_with(|| LedgerEntry::pending(task_id));
            entry.state = terminal_state(verdict);
            entry.verdict = Some(verdict);
            entry.cost_microusd = Some(cost_microusd);
            entry.finished_at = Some(Utc::now());
        })
    }

    /// Record a claim being recovered (moved back to `pending/`): clears owner.
    pub fn record_recovered(&self, task_id: &str) -> Result<(), MayorError> {
        validate_file_component("ledger task_id", task_id)?;
        self.mutate_ledger(|ledger| {
            let entry = ledger
                .entries
                .entry(task_id.to_string())
                .or_insert_with(|| LedgerEntry::pending(task_id));
            entry.state = LedgerState::Pending;
            entry.owner = None;
            entry.claimed_at = None;
        })
    }

    /// H17: REBUILD the ledger from scratch by scanning the AUTHORITATIVE queue
    /// directories, re-converging the projection on disk truth after any drift —
    /// a crashed writer, a manual file move, or a lost best-effort `record_*`. The
    /// DIRS WIN on every disagreement: an entry the ledger had but no directory
    /// backs is DROPPED, and a task present on disk but missing from the ledger is
    /// ADDED. Scan order pending → claimed → done/failed means a task that
    /// (transiently) appears under two directories resolves to the most-terminal
    /// state — the SAME direction as the queue file move — so reconcile can never
    /// downgrade a finished task back to pending nor report a failed task as done.
    ///
    /// Observability ONLY, exactly like every other ledger write: a no-op when
    /// coordination is disabled (returns the current snapshot and writes NOTHING,
    /// so queue behavior stays byte-identical to pre-S14), serialized by the
    /// dedicated `ledger.lock`, and written atomically. The ledger is NEVER
    /// consulted by the `blocked` gate, so reconciling cannot move a verdict
    /// toward acceptance.
    pub fn reconcile(&self) -> Result<Ledger, MayorError> {
        if !self.enabled {
            return self.ledger();
        }
        let _lock = FileLock::acquire(&self.workspace_root, "ledger.lock")?;
        let rebuilt = self.scan_queue_dirs()?;
        write_json_atomic(&self.layout.ledger_path(), &rebuilt)?;
        Ok(rebuilt)
    }

    /// Build a fresh [`Ledger`] purely from the authoritative queue directories.
    /// Read-only: takes no lock and writes nothing (the public [`Self::reconcile`]
    /// owns the `ledger.lock` + atomic write). Pending first, then claimed, then
    /// the terminal `done`/`failed` dirs, so a more-terminal directory overwrites
    /// any earlier transient appearance of the same id.
    fn scan_queue_dirs(&self) -> Result<Ledger, MayorError> {
        let mut entries: BTreeMap<String, LedgerEntry> = BTreeMap::new();

        // pending/<id>.json → Pending.
        for (task_id, created_at) in read_task_ids(&self.layout.pending)? {
            entries.insert(
                task_id.clone(),
                LedgerEntry {
                    state: LedgerState::Pending,
                    owner: None,
                    created_at,
                    claimed_at: None,
                    finished_at: None,
                    cost_microusd: None,
                    verdict: None,
                    task_id,
                },
            );
        }

        // claimed/<patrol>/<id>.json → Claimed, owner = patrol directory name.
        if self.layout.claimed.exists() {
            for patrol_dir in fs::read_dir(&self.layout.claimed)? {
                let patrol_dir = patrol_dir?;
                if !patrol_dir.file_type()?.is_dir() {
                    continue;
                }
                let patrol_id = patrol_dir.file_name().to_string_lossy().into_owned();
                for (task_id, created_at) in read_task_ids(&patrol_dir.path())? {
                    entries.insert(
                        task_id.clone(),
                        LedgerEntry {
                            state: LedgerState::Claimed,
                            owner: Some(patrol_id.clone()),
                            created_at,
                            claimed_at: None,
                            finished_at: None,
                            cost_microusd: None,
                            verdict: None,
                            task_id,
                        },
                    );
                }
            }
        }

        // done/ + failed/ → terminal. STATE comes from the DIRECTORY (dirs win);
        // verdict/cost/finished_at/owner are enriched from `results/<id>.json`
        // ONLY when the result's verdict agrees with the directory, so a tampered
        // result claiming success for a task sitting in `failed/` is ignored and
        // the row stays Failed with no rosy metadata.
        for (dir, dir_state) in [
            (&self.layout.done, LedgerState::Done),
            (&self.layout.failed, LedgerState::Failed),
        ] {
            for (task_id, created_at) in read_task_ids(dir)? {
                let result = self
                    .read_result(&task_id)
                    .filter(|r| terminal_state(r.verdict) == dir_state);
                entries.insert(
                    task_id.clone(),
                    LedgerEntry {
                        state: dir_state,
                        owner: result.as_ref().map(|r| r.patrol_id.clone()),
                        created_at,
                        claimed_at: None,
                        finished_at: result.as_ref().and_then(|r| r.finished_at),
                        cost_microusd: result.as_ref().map(|r| r.cost_microusd),
                        verdict: result.as_ref().map(|r| r.verdict),
                        task_id,
                    },
                );
            }
        }

        Ok(Ledger {
            version: LEDGER_VERSION,
            entries,
        })
    }

    /// Best-effort read of `results/<task_id>.json`. `None` when absent or
    /// unparseable (reconcile then leaves the terminal row's metadata empty).
    fn read_result(&self, task_id: &str) -> Option<PatrolResult> {
        let path = self.layout.result_path(task_id);
        if !path.exists() {
            return None;
        }
        read_json(&path).ok()
    }

    // ----- mailbox -----

    /// Deliver a coordination message to `msg.to`'s inbox
    /// (`<queue>/mailbox/<to>/<id>.json`, atomic). No-op when disabled. All of
    /// `to`/`from`/`id` are path-traversal-guarded, so a sender can only write a
    /// validated recipient dir inside the queue root.
    pub fn send_mail(&self, msg: &MailMessage) -> Result<(), MayorError> {
        if !self.enabled {
            return Ok(());
        }
        validate_file_component("mail recipient", &msg.to)?;
        validate_file_component("mail sender", &msg.from)?;
        validate_file_component("mail id", &msg.id)?;
        let dir = self.layout.mailbox_dir().join(&msg.to);
        fs::create_dir_all(&dir)?;
        let path = dir.join(format!("{}.json", msg.id));
        write_json_atomic(&path, msg)?;
        Ok(())
    }

    /// Read and REMOVE all messages in `recipient`'s inbox (deterministic order).
    /// Empty when disabled or no inbox. Malformed files are left in place (not
    /// returned) for operator inspection rather than silently deleted.
    ///
    /// When `coordination_enabled` is false this is a pure no-op returning empty
    /// (the SAME `!self.enabled` early-return as [`send_mail`]/`mutate_ledger`):
    /// a drain must NEVER read or delete pre-existing mailbox files while
    /// disabled, so the disabled queue is byte-identical / inert (N4). The guard
    /// comes BEFORE any filesystem access so no message is consumed or removed.
    pub fn drain_mail(&self, recipient: &str) -> Result<Vec<MailMessage>, MayorError> {
        if !self.enabled {
            return Ok(Vec::new());
        }
        validate_file_component("mail recipient", recipient)?;
        let dir = self.layout.mailbox_dir().join(recipient);
        if !dir.exists() {
            return Ok(Vec::new());
        }
        let mut out = Vec::new();
        for name in sorted_json_filenames(&dir)? {
            let path = dir.join(&name);
            match read_json::<MailMessage>(&path) {
                Ok(msg) => {
                    out.push(msg);
                    let _ = fs::remove_file(&path);
                }
                Err(_) => { /* leave malformed message for inspection */ }
            }
        }
        Ok(out)
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
            coordination_enabled: true,
        }
    }

    /// Like [`cfg`] but with the S14 coordination ledger/mailbox disabled.
    fn cfg_no_coordination() -> MayorPatrolConfig {
        MayorPatrolConfig {
            coordination_enabled: false,
            ..cfg()
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
    async fn enqueue_rejects_path_traversal_task_id() {
        let dir = tempfile::tempdir().unwrap();
        let mayor = Mayor::new(cfg(), dir.path().to_path_buf(), Some(1_000_000));
        let task = PatrolTask::new("../escape", "prompt", 1000);

        let err = mayor.enqueue(task).await.unwrap_err();

        assert!(matches!(err, MayorError::InvalidIdentifier("task_id")));
        assert!(
            !dir.path()
                .join(".nerve/queue/pending/../escape.json")
                .exists()
        );
    }

    #[test]
    fn is_valid_queue_id_matches_enqueue_contract() {
        // The public predicate must agree byte-for-byte with what
        // `Mayor::enqueue`'s `validate_file_component` accepts: 1..=128 bytes
        // of [A-Za-z0-9_-]. Dispatch pre-checks derived task ids against this.
        assert!(is_valid_queue_id("plan-xyz-step-01"));
        assert!(is_valid_queue_id(&"p".repeat(128)), "128 bytes is the limit");
        assert!(!is_valid_queue_id(&"p".repeat(129)), "129 bytes overflows");
        assert!(!is_valid_queue_id(""), "empty is rejected");
        assert!(!is_valid_queue_id("../escape"));
        assert!(!is_valid_queue_id("with space"));
        assert!(!is_valid_queue_id("nested/id"));
        // Cross-check: anything the predicate rejects, enqueue must also reject.
        assert!(matches!(
            validate_file_component("task_id", &"p".repeat(129)),
            Err(MayorError::InvalidIdentifier("task_id"))
        ));
    }

    #[tokio::test]
    async fn enqueue_rejects_duplicate_task_id() {
        let dir = tempfile::tempdir().unwrap();
        let mayor = Mayor::new(cfg(), dir.path().to_path_buf(), Some(1_000_000));
        mayor
            .enqueue(PatrolTask::new("t-dupe", "first", 1000))
            .await
            .unwrap();

        let err = mayor
            .enqueue(PatrolTask::new("t-dupe", "second", 1000))
            .await
            .unwrap_err();

        assert!(matches!(err, MayorError::TaskAlreadyExists(id) if id == "t-dupe"));
        let raw = fs::read_to_string(dir.path().join(".nerve/queue/pending/t-dupe.json")).unwrap();
        assert!(raw.contains("first"));
        assert!(!raw.contains("second"));
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
    async fn patrol_rejects_path_traversal_patrol_id() {
        let dir = tempfile::tempdir().unwrap();
        let mayor = Mayor::new(cfg(), dir.path().to_path_buf(), Some(1_000_000));
        mayor
            .enqueue(PatrolTask::new("t-claim", "prompt", 1000))
            .await
            .unwrap();
        let patrol = Patrol::new("../escape".to_string(), cfg(), dir.path().to_path_buf());

        let err = patrol.claim().unwrap_err();

        assert!(matches!(err, MayorError::InvalidIdentifier("patrol_id")));
        assert!(!dir.path().join(".nerve/queue/claimed/../escape").exists());
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

    // ----- S14: coordination ledger + mailbox -----

    #[tokio::test]
    async fn ledger_records_pending_then_claimed() {
        let dir = tempfile::tempdir().unwrap();
        let mayor = Mayor::new(cfg(), dir.path().to_path_buf(), Some(1_000_000));
        mayor
            .enqueue(PatrolTask::new("t-led", "prompt", 1000))
            .await
            .unwrap();

        let entry = mayor.ledger().unwrap().entries.remove("t-led").unwrap();
        assert_eq!(entry.state, LedgerState::Pending);
        assert!(entry.owner.is_none());
        assert!(entry.created_at.is_some());

        let patrol = Patrol::new("p-led".to_string(), cfg(), dir.path().to_path_buf());
        let claimed = patrol.claim().unwrap();
        assert_eq!(claimed.task_id, "t-led");

        let entry = mayor.ledger().unwrap().entries.remove("t-led").unwrap();
        assert_eq!(entry.state, LedgerState::Claimed);
        assert_eq!(entry.owner.as_deref(), Some("p-led"));
        assert!(entry.claimed_at.is_some());
    }

    #[tokio::test]
    async fn ledger_records_finished_done_and_failed() {
        let dir = tempfile::tempdir().unwrap();
        let mayor = Mayor::new(cfg(), dir.path().to_path_buf(), Some(10_000_000));
        mayor
            .enqueue(PatrolTask::new("t-ok", "prompt", 1_000_000))
            .await
            .unwrap();
        mayor
            .enqueue(PatrolTask::new("t-bad", "prompt", 1_000_000))
            .await
            .unwrap();

        // Claim+run each. Patrols claim the lex-smallest first (t-bad < t-ok).
        let p = Patrol::new("p-fin".to_string(), cfg(), dir.path().to_path_buf());
        p.run_one(stub_dispatch(false, 100)).await.unwrap();
        p.run_one(stub_dispatch(true, 4242)).await.unwrap();

        let ledger = mayor.ledger().unwrap();
        let ok = ledger.entries.get("t-ok").unwrap();
        assert_eq!(ok.state, LedgerState::Done);
        assert_eq!(ok.verdict, Some(PatrolVerdict::Success));
        assert_eq!(ok.cost_microusd, Some(4242));
        assert!(ok.finished_at.is_some());

        let bad = ledger.entries.get("t-bad").unwrap();
        assert_eq!(bad.state, LedgerState::Failed);
        assert_eq!(bad.verdict, Some(PatrolVerdict::Failed));
    }

    #[tokio::test]
    async fn recover_orphans_updates_ledger_and_sends_reclaim_mail() {
        let dir = tempfile::tempdir().unwrap();
        let mayor = Mayor::new(cfg(), dir.path().to_path_buf(), None);
        let queue_root = dir.path().join(".nerve").join("queue");
        let claimed_dir = queue_root.join("claimed").join("p-dead");
        fs::create_dir_all(&claimed_dir).unwrap();
        let stale = PatrolTask::new("t-orph", "prompt", 500);
        write_json_atomic(&claimed_dir.join("t-orph.json"), &stale).unwrap();
        // Seed the ledger as if the dead patrol had claimed it.
        mayor.coordinator().record_claimed("t-orph", "p-dead").unwrap();

        let heartbeat_dir = queue_root.join("heartbeat");
        fs::create_dir_all(&heartbeat_dir).unwrap();
        let heartbeat_path = heartbeat_dir.join("p-dead");
        File::create(&heartbeat_path).unwrap();
        backdate_to_seconds_ago(&heartbeat_path, 120);

        let status = mayor.status().await.unwrap();
        assert_eq!(status.orphans_recovered, 1);

        // Ledger: ownership cleared, back to Pending.
        let entry = mayor.ledger().unwrap().entries.remove("t-orph").unwrap();
        assert_eq!(entry.state, LedgerState::Pending);
        assert!(entry.owner.is_none());

        // Mailbox: the dead patrol got a Reclaimed notice (non-authoritative).
        let mail = mayor.drain_mail("p-dead").unwrap();
        assert_eq!(mail.len(), 1);
        assert_eq!(mail[0].kind, MailKind::Reclaimed);
        assert_eq!(mail[0].from, "mayor");
        // Drained: a second drain is empty.
        assert!(mayor.drain_mail("p-dead").unwrap().is_empty());
    }

    #[tokio::test]
    async fn ledger_disabled_writes_nothing_and_queue_is_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let mayor = Mayor::new(cfg_no_coordination(), dir.path().to_path_buf(), Some(1_000_000));
        mayor
            .enqueue(PatrolTask::new("t-off", "prompt", 1000))
            .await
            .unwrap();

        // Authoritative queue state is identical; no ledger file is created.
        assert!(dir.path().join(".nerve/queue/pending/t-off.json").exists());
        assert!(!dir.path().join(".nerve/queue/ledger.json").exists());
        assert!(mayor.ledger().unwrap().entries.is_empty());
        // Mailbox send is a no-op when disabled.
        mayor
            .coordinator()
            .send_mail(&MailMessage::new("m1", "mayor", "p1", MailKind::Note, "hi"))
            .unwrap();
        assert!(!dir.path().join(".nerve/queue/mailbox").exists());

        // Disabled drain is ALSO a pure no-op: a pre-existing mailbox file (e.g.
        // left from when coordination was enabled) must be neither returned nor
        // DELETED while disabled — the disabled queue is byte-identical / inert.
        let inbox = dir.path().join(".nerve/queue/mailbox/p1");
        fs::create_dir_all(&inbox).unwrap();
        let preexisting = inbox.join("leftover.json");
        write_json_atomic(
            &preexisting,
            &MailMessage::new("leftover", "mayor", "p1", MailKind::Note, "old"),
        )
        .unwrap();
        let drained = mayor.drain_mail("p1").unwrap();
        assert!(drained.is_empty(), "disabled drain must return nothing");
        assert!(
            preexisting.exists(),
            "disabled drain must NOT delete pre-existing mailbox files"
        );
    }

    #[tokio::test]
    async fn ledger_is_non_authoritative_status_comes_from_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let mayor = Mayor::new(cfg(), dir.path().to_path_buf(), Some(1_000_000));
        mayor
            .enqueue(PatrolTask::new("t-a", "p", 1000))
            .await
            .unwrap();
        mayor
            .enqueue(PatrolTask::new("t-b", "p", 1000))
            .await
            .unwrap();

        // Nuke the ledger entirely. The authoritative status must be unaffected:
        // it is computed from the queue directories, never from the ledger.
        fs::remove_file(dir.path().join(".nerve/queue/ledger.json")).unwrap();
        let status = mayor.status().await.unwrap();
        assert_eq!(status.pending_count, 2);
        assert!(mayor.ledger().unwrap().entries.is_empty());
    }

    #[test]
    fn reconcile_rebuilds_ledger_from_dirs_and_dirs_win() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let coord = Coordinator::new(root.clone(), &cfg());

        let queue = root.join(".nerve/queue");
        let results = root.join(".nerve/results");

        // Lay out the AUTHORITATIVE queue dirs by hand: one task in each lifecycle
        // directory, terminal ones backed by a result file.
        write_json_atomic(
            &queue.join("pending/t-pending.json"),
            &PatrolTask::new("t-pending", "p", 1000),
        )
        .unwrap();
        write_json_atomic(
            &queue.join("claimed/p-worker/t-claimed.json"),
            &PatrolTask::new("t-claimed", "p", 1000),
        )
        .unwrap();
        write_json_atomic(
            &queue.join("done/t-done.json"),
            &PatrolTask::new("t-done", "p", 1000),
        )
        .unwrap();
        write_json_atomic(
            &results.join("t-done.json"),
            &PatrolResult::success("t-done", "p-worker", 4242),
        )
        .unwrap();
        write_json_atomic(
            &queue.join("failed/t-failed.json"),
            &PatrolTask::new("t-failed", "p", 1000),
        )
        .unwrap();
        // A TAMPERED result claiming SUCCESS for a task sitting in `failed/` must
        // be ignored — dirs win, so the row stays Failed with NO rosy metadata.
        write_json_atomic(
            &results.join("t-failed.json"),
            &PatrolResult::success("t-failed", "p-worker", 9999),
        )
        .unwrap();

        // Seed a DIVERGENT stale ledger: a wrong state for a real task, plus a
        // ghost entry no directory backs. (t-done / t-failed are deliberately
        // ABSENT from the seed — dir-only — and must be ADDED by reconcile.)
        coord.record_claimed("t-pending", "p-stale").unwrap(); // dir actually says Pending
        coord.record_enqueued("t-ghost", None).unwrap(); // no dir backs this
        assert!(coord.ledger().unwrap().entries.contains_key("t-ghost"));

        let ledger = coord.reconcile().unwrap();

        // Exactly the four on-disk tasks survive; the stale ghost is DROPPED.
        assert_eq!(ledger.entries.len(), 4);
        assert!(
            !ledger.entries.contains_key("t-ghost"),
            "stale ghost (no backing dir) must be dropped"
        );

        let pending = ledger.entries.get("t-pending").unwrap();
        assert_eq!(
            pending.state,
            LedgerState::Pending,
            "dirs win: stale Claimed corrected back to Pending"
        );
        assert!(pending.owner.is_none());
        assert!(pending.created_at.is_some(), "created_at read from task file");

        let claimed = ledger.entries.get("t-claimed").unwrap();
        assert_eq!(claimed.state, LedgerState::Claimed);
        assert_eq!(claimed.owner.as_deref(), Some("p-worker"));

        let done = ledger.entries.get("t-done").unwrap();
        assert_eq!(done.state, LedgerState::Done);
        assert_eq!(done.verdict, Some(PatrolVerdict::Success));
        assert_eq!(done.cost_microusd, Some(4242));
        assert!(done.finished_at.is_some());

        let failed = ledger.entries.get("t-failed").unwrap();
        assert_eq!(
            failed.state,
            LedgerState::Failed,
            "dirs win: a task in failed/ stays Failed"
        );
        // The tampered success result was rejected (verdict ≠ dir), so no metadata.
        assert_eq!(failed.verdict, None);
        assert_eq!(failed.cost_microusd, None);

        // The reconciled ledger was PERSISTED atomically — a fresh reader agrees.
        let reread = Coordinator::new(root, &cfg()).ledger().unwrap();
        assert_eq!(reread, ledger);
    }

    #[test]
    fn reconcile_is_noop_when_coordination_disabled() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let queue = root.join(".nerve/queue");
        // A real pending task exists on disk...
        write_json_atomic(
            &queue.join("pending/t-x.json"),
            &PatrolTask::new("t-x", "p", 1000),
        )
        .unwrap();

        // ...but with coordination disabled, reconcile writes NOTHING and returns
        // the current (empty) snapshot — a disabled queue stays byte-identical to
        // pre-S14, exactly like `mutate_ledger`'s `!enabled` no-op.
        let coord = Coordinator::new(root.clone(), &cfg_no_coordination());
        let ledger = coord.reconcile().unwrap();
        assert!(
            ledger.entries.is_empty(),
            "disabled reconcile must not project the dirs"
        );
        assert!(
            !queue.join("ledger.json").exists(),
            "disabled reconcile must not create the ledger file"
        );
    }

    #[test]
    fn ledger_and_mailbox_reject_traversal_ids() {
        let dir = tempfile::tempdir().unwrap();
        let coord = Coordinator::new(dir.path().to_path_buf(), &cfg());
        // The full reject set of `is_valid_queue_id`: traversal, separators
        // (`/` and `\`), whitespace/control (space, tab, NUL), empty, and the
        // 129-byte over-limit boundary.
        let over_limit = "x".repeat(129);
        let bad_ids: Vec<&str> = vec![
            "../escape",
            "a/b",
            "a\\b",
            "..",
            "with space",
            "tab\tx",
            "nul\0x",
            "",
            &over_limit,
        ];
        for bad in bad_ids {
            assert!(coord.record_enqueued(bad, None).is_err(), "id {bad:?}");
            assert!(
                coord
                    .send_mail(&MailMessage::new("m", "mayor", bad, MailKind::Note, "x"))
                    .is_err(),
                "recipient {bad:?}"
            );
            assert!(coord.drain_mail(bad).is_err(), "drain {bad:?}");
        }
        // No traversal write escaped the queue root.
        assert!(!dir.path().join(".nerve/queue/escape.json").exists());
        assert!(!dir.path().join(".nerve/escape.json").exists());
    }

    #[test]
    fn ledger_concurrent_enqueue_no_lost_update() {
        // N threads racing distinct ids must all land: the dedicated ledger.lock
        // serializes the read-modify-write so no update is lost.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let n = 12usize;
        let handles: Vec<_> = (0..n)
            .map(|i| {
                let root = root.clone();
                std::thread::spawn(move || {
                    let coord = Coordinator::new(root, &cfg());
                    coord
                        .record_enqueued(&format!("task-{i:02}"), None)
                        .unwrap();
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        let ledger = Coordinator::new(root, &cfg()).ledger().unwrap();
        assert_eq!(ledger.entries.len(), n);
    }

    #[test]
    fn mailbox_send_drain_roundtrip_and_isolation() {
        let dir = tempfile::tempdir().unwrap();
        let coord = Coordinator::new(dir.path().to_path_buf(), &cfg());
        coord
            .send_mail(&MailMessage::new("m1", "mayor", "p1", MailKind::Note, "one"))
            .unwrap();
        coord
            .send_mail(&MailMessage::new("m2", "p2", "p1", MailKind::Progress, "two"))
            .unwrap();
        coord
            .send_mail(&MailMessage::new("m3", "mayor", "p2", MailKind::Note, "other"))
            .unwrap();

        // p1 gets exactly its two messages (deterministic order by id).
        let p1 = coord.drain_mail("p1").unwrap();
        assert_eq!(p1.len(), 2);
        assert_eq!(p1[0].id, "m1");
        assert_eq!(p1[1].id, "m2");
        // Isolation: p2's inbox is independent and untouched by p1's drain.
        let p2 = coord.drain_mail("p2").unwrap();
        assert_eq!(p2.len(), 1);
        assert_eq!(p2[0].id, "m3");
        // Drain consumes: a second drain is empty.
        assert!(coord.drain_mail("p1").unwrap().is_empty());
    }

    // ===== H18: standing thesis-invariant guard ==============================
    #[test]
    fn h18_invariant_mailkind_is_a_closed_set_with_no_consent_variant() {
        // Regression guarded: "MailKind widened with a consent variant". The
        // mailbox is coordination metadata ONLY; it must never become a covert
        // channel by which the lead signals apply/consent (S11: apply consent is
        // unforgeable in-memory state, never a disk/message signal). The match
        // below is deliberately EXHAUSTIVE with no wildcard arm — adding any
        // variant to `MailKind` (e.g. an `Approve`/`Consent` kind) makes this
        // fail to COMPILE, which is the guard. We also pin the on-disk serde
        // tags so the set cannot be widened by an alias.
        fn tag_of(kind: MailKind) -> &'static str {
            match kind {
                MailKind::Note => "note",
                MailKind::Progress => "progress",
                MailKind::Reclaimed => "reclaimed",
            }
        }
        for (kind, tag) in [
            (MailKind::Note, "note"),
            (MailKind::Progress, "progress"),
            (MailKind::Reclaimed, "reclaimed"),
        ] {
            assert_eq!(tag_of(kind), tag);
            let json = serde_json::to_string(&kind).unwrap();
            assert_eq!(json, format!("\"{tag}\""));
            assert_eq!(serde_json::from_str::<MailKind>(&json).unwrap(), kind);
        }
        // Nothing outside the closed set parses — no covert consent variant.
        assert!(serde_json::from_str::<MailKind>("\"approve\"").is_err());
        assert!(serde_json::from_str::<MailKind>("\"consent\"").is_err());
    }
}
