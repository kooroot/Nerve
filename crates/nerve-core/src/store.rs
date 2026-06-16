use crate::RunReport;
use anyhow::{Context, Result};
use chrono::Utc;
use nerve_config::ProfileSelection;
use nerve_patch::{ApplyReport, NvPatch};
use nerve_types::{RoundRecord, Task, Verdict};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct NerveStore {
    cwd: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionSummary {
    pub id: String,
    #[serde(default)]
    pub name: Option<String>,
    pub prompt: String,
    pub started_at: String,
    #[serde(default)]
    pub updated_at: Option<String>,
    pub cwd: PathBuf,
    pub profile: Option<String>,
    pub verdict: Verdict,
    pub rounds: usize,
    pub patch_id: Option<String>,
    pub applied: bool,
    pub blocked: bool,
    #[serde(default)]
    pub parent_session_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SessionMetadata {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub parent_session_id: Option<String>,
    #[serde(default)]
    pub updated_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PatchIndex {
    pub patches: Vec<PatchRecord>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PatchRecord {
    pub id: String,
    pub session_id: String,
    pub prompt: String,
    pub created_at: String,
    pub file_count: usize,
    pub changed_files: Vec<PathBuf>,
    pub verdict: Verdict,
    pub applied: bool,
}

/// Lifecycle marker for a [`RunCheckpoint`]. A checkpoint is only ever written
/// while a loop is `Running`; the terminal `Finished` state is reserved for a
/// possible future "the run ended but I want the checkpoint to linger" use and
/// is never produced by the S8 write path (finalize *clears* the checkpoint
/// instead — see [`NerveStore::save_report`]).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    Running,
    Finished,
}

/// An explicitly IN-PROGRESS snapshot of a synaptic loop, written once per
/// completed round so a mid-loop crash leaves the rounds-so-far recoverable on
/// disk (substrate for S9's nonblocking daemon + live stream).
///
/// **Security invariant (S8 north star #2):** a checkpoint is *structurally*
/// distinct from a finalized [`RunReport`] — it deliberately carries NO
/// `applied` / `blocked` / `goal_satisfied` / `final_patch` fields. Those exist
/// only at finalize, so it is impossible for a crash-recovered checkpoint to be
/// mistaken for a completed/accepted run, and a checkpoint write (or its
/// failure) can never change which patch the deterministic gate accepts. The
/// presence of a checkpoint file after process exit means exactly "this run was
/// interrupted before finalize" — `save_report` removes it on a clean finish.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct RunCheckpoint {
    pub task: Task,
    pub selection: ProfileSelection,
    /// `Running` for every checkpoint the loop writes; see [`RunStatus`].
    pub status: RunStatus,
    /// Rounds completed so far, in order.
    pub rounds: Vec<RoundRecord>,
    /// rfc3339 timestamp of this snapshot (chrono::Utc, as elsewhere in store).
    pub updated_at: String,
}

impl NerveStore {
    pub fn new(cwd: impl Into<PathBuf>) -> Self {
        Self { cwd: cwd.into() }
    }

    pub fn save_report(&self, report: &RunReport) -> Result<()> {
        self.ensure_dirs()?;
        write_json(&self.session_path(&report.task.id), report)?;

        if !report.blocked
            && let Some(patch) = &report.final_patch
        {
            write_json(&self.patch_path(&patch.id), patch)?;
            self.upsert_patch_record(PatchRecord::from_report(report, patch))?;
        }

        // The finalized report supersedes any in-flight checkpoint for this run:
        // its presence means "still running / interrupted", so a clean finalize
        // must remove it (idempotent — Ok if it was never written).
        self.clear_checkpoint(&report.task.id)?;

        Ok(())
    }

    /// Persist an IN-PROGRESS round checkpoint (S8). Atomic write under
    /// `.nerve/checkpoints/{id}.json`. See [`RunCheckpoint`] for why this can
    /// never assert acceptance.
    pub fn save_checkpoint(&self, checkpoint: &RunCheckpoint) -> Result<()> {
        fs::create_dir_all(self.checkpoints_dir())
            .with_context(|| format!("failed to create `{}`", self.checkpoints_dir().display()))?;
        write_json(&self.checkpoint_path(&checkpoint.task.id), checkpoint)
    }

    /// Load the in-flight checkpoint for `id` (errors if absent — callers that
    /// tolerate absence should check [`Self::list_checkpoints`] instead).
    pub fn load_checkpoint(&self, id: &str) -> Result<RunCheckpoint> {
        read_json(&self.checkpoint_path(id))
    }

    /// All in-flight / interrupted checkpoints — the recovery + observability
    /// surface S9's daemon reads to resume or report on running loops.
    pub fn list_checkpoints(&self) -> Result<Vec<RunCheckpoint>> {
        let dir = self.checkpoints_dir();
        if !dir.exists() {
            return Ok(Vec::new());
        }
        let mut checkpoints = Vec::new();
        for entry in fs::read_dir(&dir)
            .with_context(|| format!("failed to read `{}`", dir.display()))?
        {
            let entry = entry?;
            if entry.path().extension().and_then(|value| value.to_str()) != Some("json") {
                continue;
            }
            checkpoints.push(read_json(&entry.path())?);
        }
        checkpoints.sort_by(|a: &RunCheckpoint, b| b.updated_at.cmp(&a.updated_at));
        Ok(checkpoints)
    }

    /// Remove the checkpoint for `id`. Idempotent: `Ok(())` if already absent,
    /// so it is safe to call unconditionally at finalize.
    pub fn clear_checkpoint(&self, id: &str) -> Result<()> {
        let path = self.checkpoint_path(id);
        match fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => {
                Err(error).with_context(|| format!("failed to remove `{}`", path.display()))
            }
        }
    }

    pub fn list_sessions(&self) -> Result<Vec<SessionSummary>> {
        let sessions_dir = self.sessions_dir();
        if !sessions_dir.exists() {
            return Ok(Vec::new());
        }

        let mut summaries = Vec::new();
        for entry in fs::read_dir(&sessions_dir)
            .with_context(|| format!("failed to read `{}`", sessions_dir.display()))?
        {
            let entry = entry?;
            if entry.path().extension().and_then(|value| value.to_str()) != Some("json") {
                continue;
            }
            let report = self.load_report_path(&entry.path())?;
            let metadata = self.load_session_metadata(&report.task.id)?;
            summaries.push(SessionSummary::from_report(&report, metadata));
        }

        summaries.sort_by(|a, b| b.started_at.cmp(&a.started_at));
        Ok(summaries)
    }

    pub fn load_report(&self, id: &str) -> Result<RunReport> {
        self.load_report_path(&self.session_path(id))
    }

    pub fn init(&self) -> Result<()> {
        self.ensure_dirs()
    }

    pub fn name_session(&self, id: &str, name: impl Into<String>) -> Result<()> {
        if !self.session_path(id).exists() {
            anyhow::bail!("session `{id}` does not exist");
        }
        let mut metadata = self.load_session_metadata(id)?;
        metadata.name = Some(name.into());
        metadata.updated_at = Some(Utc::now().to_rfc3339());
        self.save_session_metadata(id, &metadata)
    }

    pub fn link_child_session(&self, child_id: &str, parent_id: &str) -> Result<()> {
        if !self.session_path(child_id).exists() {
            anyhow::bail!("session `{child_id}` does not exist");
        }
        if !self.session_path(parent_id).exists() {
            anyhow::bail!("session `{parent_id}` does not exist");
        }
        let mut metadata = self.load_session_metadata(child_id)?;
        metadata.parent_session_id = Some(parent_id.to_string());
        metadata.updated_at = Some(Utc::now().to_rfc3339());
        self.save_session_metadata(child_id, &metadata)
    }

    pub fn list_patches(&self) -> Result<Vec<PatchRecord>> {
        let mut records = self.load_index()?.patches;
        records.sort_by(|a, b| b.created_at.cmp(&a.created_at));
        Ok(records)
    }

    pub fn apply_patch(&self, id: &str) -> Result<ApplyReport> {
        let patch = self.load_patch(id)?;
        let report = patch.apply(&self.cwd, false)?;
        self.set_patch_applied(id, true)?;
        Ok(report)
    }

    pub fn rollback_patch(&self, id: &str) -> Result<ApplyReport> {
        let patch = self.load_patch(id)?;
        let report = patch.rollback(&self.cwd, false)?;
        self.set_patch_applied(id, false)?;
        Ok(report)
    }

    fn ensure_dirs(&self) -> Result<()> {
        fs::create_dir_all(self.sessions_dir())
            .with_context(|| format!("failed to create `{}`", self.sessions_dir().display()))?;
        fs::create_dir_all(self.session_meta_dir())
            .with_context(|| format!("failed to create `{}`", self.session_meta_dir().display()))?;
        fs::create_dir_all(self.patches_dir())
            .with_context(|| format!("failed to create `{}`", self.patches_dir().display()))?;
        fs::create_dir_all(self.checkpoints_dir())
            .with_context(|| format!("failed to create `{}`", self.checkpoints_dir().display()))?;
        Ok(())
    }

    fn load_patch(&self, id: &str) -> Result<NvPatch> {
        read_json(&self.patch_path(id))
    }

    fn load_report_path(&self, path: &Path) -> Result<RunReport> {
        read_json(path)
    }

    fn load_session_metadata(&self, id: &str) -> Result<SessionMetadata> {
        let path = self.session_meta_path(id);
        if !path.exists() {
            return Ok(SessionMetadata::default());
        }
        read_json(&path)
    }

    fn save_session_metadata(&self, id: &str, metadata: &SessionMetadata) -> Result<()> {
        write_json(&self.session_meta_path(id), metadata)
    }

    fn upsert_patch_record(&self, record: PatchRecord) -> Result<()> {
        let _lock = self.lock_index()?;
        let mut index = self.load_index()?;
        index.patches.retain(|existing| existing.id != record.id);
        index.patches.push(record);
        write_json(&self.index_path(), &index)
    }

    fn set_patch_applied(&self, id: &str, applied: bool) -> Result<()> {
        let _lock = self.lock_index()?;
        let mut index = self.load_index()?;
        let Some(record) = index.patches.iter_mut().find(|record| record.id == id) else {
            anyhow::bail!("patch `{id}` is not indexed");
        };
        let session_id = record.session_id.clone();
        record.applied = applied;
        write_json(&self.index_path(), &index)?;
        self.set_session_applied(&session_id, applied)
    }

    fn set_session_applied(&self, id: &str, applied: bool) -> Result<()> {
        let path = self.session_path(id);
        if !path.exists() {
            return Ok(());
        }
        let mut report: RunReport = read_json(&path)?;
        report.applied = applied;
        write_json(&path, &report)
    }

    fn lock_index(&self) -> Result<StoreLock> {
        fs::create_dir_all(self.patches_dir())
            .with_context(|| format!("failed to create `{}`", self.patches_dir().display()))?;
        StoreLock::acquire(self.patches_dir().join("index.lock"))
    }

    fn load_index(&self) -> Result<PatchIndex> {
        let path = self.index_path();
        if !path.exists() {
            return Ok(PatchIndex::default());
        }
        read_json(&path)
    }

    fn root_dir(&self) -> PathBuf {
        self.cwd.join(".nerve")
    }

    fn sessions_dir(&self) -> PathBuf {
        self.root_dir().join("sessions")
    }

    fn session_meta_dir(&self) -> PathBuf {
        self.root_dir().join("session-meta")
    }

    fn patches_dir(&self) -> PathBuf {
        self.root_dir().join("patches")
    }

    fn checkpoints_dir(&self) -> PathBuf {
        self.root_dir().join("checkpoints")
    }

    fn session_path(&self, id: &str) -> PathBuf {
        self.sessions_dir().join(format!("{id}.json"))
    }

    fn checkpoint_path(&self, id: &str) -> PathBuf {
        self.checkpoints_dir().join(format!("{id}.json"))
    }

    fn session_meta_path(&self, id: &str) -> PathBuf {
        self.session_meta_dir().join(format!("{id}.json"))
    }

    fn patch_path(&self, id: &str) -> PathBuf {
        self.patches_dir().join(format!("{id}.json"))
    }

    fn index_path(&self) -> PathBuf {
        self.patches_dir().join("index.json")
    }
}

impl SessionSummary {
    fn from_report(report: &RunReport, metadata: SessionMetadata) -> Self {
        Self {
            id: report.task.id.clone(),
            name: metadata.name,
            prompt: report.task.prompt.clone(),
            started_at: report.task.started_at.to_rfc3339(),
            updated_at: metadata.updated_at,
            cwd: report.task.cwd.clone(),
            profile: report.selection.id.clone(),
            verdict: report.final_feedback.verdict.clone(),
            rounds: report.rounds.len(),
            patch_id: report.final_patch.as_ref().map(|patch| patch.id.clone()),
            applied: report.applied,
            blocked: report.blocked,
            parent_session_id: metadata.parent_session_id,
        }
    }
}

impl PatchRecord {
    fn from_report(report: &RunReport, patch: &NvPatch) -> Self {
        Self {
            id: patch.id.clone(),
            session_id: report.task.id.clone(),
            prompt: report.task.prompt.clone(),
            created_at: report.task.started_at.to_rfc3339(),
            file_count: patch.files.len(),
            changed_files: patch
                .files
                .iter()
                .flat_map(|file| match &file.operation {
                    nerve_patch::FileOperation::Rename { from } => {
                        vec![from.clone(), file.path.clone()]
                    }
                    nerve_patch::FileOperation::Modify
                    | nerve_patch::FileOperation::Create
                    | nerve_patch::FileOperation::Delete => vec![file.path.clone()],
                })
                .collect(),
            verdict: report.final_feedback.verdict.clone(),
            applied: report.applied,
        }
    }
}

struct StoreLock {
    path: PathBuf,
    _file: fs::File,
}

impl StoreLock {
    fn acquire(path: PathBuf) -> Result<Self> {
        const RETRIES: usize = 200;
        for _ in 0..RETRIES {
            match fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(file) => return Ok(Self { path, _file: file }),
                Err(source) if source.kind() == std::io::ErrorKind::AlreadyExists => {
                    std::thread::sleep(Duration::from_millis(25));
                }
                Err(source) => {
                    return Err(source)
                        .with_context(|| format!("failed to create `{}`", path.display()));
                }
            }
        }
        anyhow::bail!("timed out waiting for `{}`", path.display())
    }
}

impl Drop for StoreLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn read_json<T>(path: &Path) -> Result<T>
where
    T: for<'de> Deserialize<'de>,
{
    let raw =
        fs::read_to_string(path).with_context(|| format!("failed to read `{}`", path.display()))?;
    serde_json::from_str(&raw).with_context(|| format!("invalid JSON in `{}`", path.display()))
}

fn write_json<T>(path: &Path, value: &T) -> Result<()>
where
    T: Serialize,
{
    write_json_atomic(path, value)
}

/// Atomically serialize `value` to JSON at `path`.
///
/// Implementation guarantees:
/// 1. The parent directory is created if missing.
/// 2. Bytes are first written to a `NamedTempFile` inside the *same*
///    directory so that the subsequent `persist` is a `rename(2)` on the
///    same filesystem — never a cross-mount copy.
/// 3. On success the destination is replaced atomically; on any failure
///    the temp file is cleaned up and the destination is untouched.
///
/// This is the canonical path for persisting `.nerve/` state — fork
/// session payloads, queue tasks, results, audit chain entries, etc.
pub fn write_json_atomic<T>(path: &Path, value: &T) -> Result<()>
where
    T: Serialize,
{
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create `{}`", parent.display()))?;
    }

    let raw = serde_json::to_vec_pretty(value)?;
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let mut tmp = tempfile::NamedTempFile::new_in(parent)
        .with_context(|| format!("failed to create temp file in `{}`", parent.display()))?;
    tmp.write_all(&raw)
        .with_context(|| format!("failed to write temp JSON for `{}`", path.display()))?;
    tmp.persist(path)
        .map_err(|error| error.error)
        .with_context(|| format!("failed to move temp JSON to `{}`", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::RunReport;
    use nerve_config::{PlanStrategy, ProfileSelection, ReviewStrictness};
    use nerve_patch::{FilePatch, NvPatch};
    use nerve_types::{AgentOutput, ReviewerFeedback, RoundRecord, Task};

    fn sample_selection() -> ProfileSelection {
        ProfileSelection {
            id: None,
            lead: "lead".to_string(),
            reviewer: "reviewer".to_string(),
            review_strictness: ReviewStrictness::Normal,
            max_refinement_rounds: 1,
            plan_strategy: PlanStrategy::Single,
            plan_system_prompt_override: None,
        }
    }

    fn sample_round(round: u8) -> RoundRecord {
        let patch = NvPatch::new(vec![FilePatch::create("created.txt", "created\n")]);
        RoundRecord {
            round,
            lead: AgentOutput::with_patch("lead", "patch", patch),
            reviewer: ReviewerFeedback::lgtm("reviewer", "LGTM"),
            check_result: None,
            patch_sha: None,
            envelope_id: None,
        }
    }

    fn sample_checkpoint(task: &Task, rounds: usize) -> RunCheckpoint {
        RunCheckpoint {
            task: task.clone(),
            selection: sample_selection(),
            status: RunStatus::Running,
            rounds: (0..rounds).map(|i| sample_round(i as u8)).collect(),
            updated_at: Utc::now().to_rfc3339(),
        }
    }

    #[test]
    fn checkpoint_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let store = NerveStore::new(dir.path());
        let task = Task::new("do work", dir.path());
        let checkpoint = sample_checkpoint(&task, 2);

        store.save_checkpoint(&checkpoint).unwrap();
        let loaded = store.load_checkpoint(&task.id).unwrap();

        assert_eq!(loaded, checkpoint);
        assert_eq!(loaded.status, RunStatus::Running);
        assert_eq!(loaded.rounds.len(), 2);
    }

    #[test]
    fn save_report_clears_checkpoint() {
        let dir = tempfile::tempdir().unwrap();
        let store = NerveStore::new(dir.path());
        let patch = NvPatch::new(vec![FilePatch::create("created.txt", "created\n")]);
        let task = Task::new("create file", dir.path());
        store.save_checkpoint(&sample_checkpoint(&task, 1)).unwrap();
        assert!(store.load_checkpoint(&task.id).is_ok());

        let report = RunReport {
            task: task.clone(),
            selection: sample_selection(),
            rounds: Vec::new(),
            crossfire_feedback: Vec::new(),
            final_output: AgentOutput::with_patch("lead", "patch", patch.clone()),
            final_feedback: ReviewerFeedback::lgtm("reviewer", "LGTM"),
            final_patch: Some(patch),
            events: Vec::new(),
            usage: Default::default(),
            budget_exceeded: false,
            no_progress_exceeded: false,
            crossfire_halted: false,
            goal_satisfied: None,
            applied: false,
            blocked: false,
        };
        store.save_report(&report).unwrap();

        // The finalized report supersedes the in-flight checkpoint.
        assert!(store.load_checkpoint(&task.id).is_err());
        assert!(store.list_checkpoints().unwrap().is_empty());
    }

    #[test]
    fn clear_checkpoint_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let store = NerveStore::new(dir.path());
        // Clearing a never-written checkpoint is a no-op success.
        store.clear_checkpoint("nonexistent").unwrap();

        let task = Task::new("do work", dir.path());
        store.save_checkpoint(&sample_checkpoint(&task, 1)).unwrap();
        store.clear_checkpoint(&task.id).unwrap();
        store.clear_checkpoint(&task.id).unwrap();
        assert!(store.load_checkpoint(&task.id).is_err());
    }

    #[test]
    fn list_checkpoints_returns_in_flight_runs() {
        let dir = tempfile::tempdir().unwrap();
        let store = NerveStore::new(dir.path());
        // No directory yet ⇒ empty, not an error.
        assert!(store.list_checkpoints().unwrap().is_empty());

        let task_a = Task::new("task a", dir.path());
        let task_b = Task::new("task b", dir.path());
        store.save_checkpoint(&sample_checkpoint(&task_a, 1)).unwrap();
        store.save_checkpoint(&sample_checkpoint(&task_b, 3)).unwrap();

        let listed = store.list_checkpoints().unwrap();
        assert_eq!(listed.len(), 2);
        let ids: std::collections::HashSet<_> =
            listed.iter().map(|c| c.task.id.clone()).collect();
        assert!(ids.contains(&task_a.id));
        assert!(ids.contains(&task_b.id));
    }

    #[test]
    fn apply_and_rollback_keep_session_history_in_sync() {
        let dir = tempfile::tempdir().unwrap();
        let patch = NvPatch::new(vec![FilePatch::create("created.txt", "created\n")]);
        let task = Task::new("create file", dir.path());
        let report = RunReport {
            task: task.clone(),
            selection: ProfileSelection {
                id: None,
                lead: "lead".to_string(),
                reviewer: "reviewer".to_string(),
                review_strictness: ReviewStrictness::Normal,
                max_refinement_rounds: 1,
                plan_strategy: PlanStrategy::Single,
                plan_system_prompt_override: None,
            },
            rounds: Vec::new(),
            crossfire_feedback: Vec::new(),
            final_output: AgentOutput::with_patch("lead", "patch", patch.clone()),
            final_feedback: ReviewerFeedback::lgtm("reviewer", "LGTM"),
            final_patch: Some(patch.clone()),
            events: Vec::new(),
            usage: Default::default(),
            budget_exceeded: false,
            no_progress_exceeded: false,
            crossfire_halted: false,
            goal_satisfied: None,
            applied: false,
            blocked: false,
        };
        let store = NerveStore::new(dir.path());
        store.save_report(&report).unwrap();

        store.apply_patch(&patch.id).unwrap();

        assert!(store.list_patches().unwrap()[0].applied);
        assert!(store.load_report(&task.id).unwrap().applied);

        store.rollback_patch(&patch.id).unwrap();

        assert!(!store.list_patches().unwrap()[0].applied);
        assert!(!store.load_report(&task.id).unwrap().applied);
    }

    #[test]
    fn session_name_is_included_in_history() {
        let dir = tempfile::tempdir().unwrap();
        let patch = NvPatch::new(vec![FilePatch::create("created.txt", "created\n")]);
        let task = Task::new("create file", dir.path());
        let report = RunReport {
            task: task.clone(),
            selection: ProfileSelection {
                id: None,
                lead: "lead".to_string(),
                reviewer: "reviewer".to_string(),
                review_strictness: ReviewStrictness::Normal,
                max_refinement_rounds: 1,
                plan_strategy: PlanStrategy::Single,
                plan_system_prompt_override: None,
            },
            rounds: Vec::new(),
            crossfire_feedback: Vec::new(),
            final_output: AgentOutput::with_patch("lead", "patch", patch.clone()),
            final_feedback: ReviewerFeedback::lgtm("reviewer", "LGTM"),
            final_patch: Some(patch),
            events: Vec::new(),
            usage: Default::default(),
            budget_exceeded: false,
            no_progress_exceeded: false,
            crossfire_halted: false,
            goal_satisfied: None,
            applied: false,
            blocked: false,
        };
        let store = NerveStore::new(dir.path());
        store.save_report(&report).unwrap();

        store.name_session(&task.id, "first pass").unwrap();

        let summary = store.list_sessions().unwrap().remove(0);
        assert_eq!(summary.name.as_deref(), Some("first pass"));
        assert_eq!(summary.cwd, dir.path());
    }
}
