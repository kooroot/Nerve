use crate::RunReport;
use anyhow::{Context, Result};
use chrono::Utc;
use nerve_config::ProfileSelection;
use nerve_patch::{ApplyReport, NvPatch};
use nerve_types::{PlanReport, RoundRecord, Task, Verdict};
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

/// S11: an AUDIT record that an operator explicitly escalated approval (today:
/// apply-consent) for ONE run, keyed on the run's id. Written when the operator
/// sends an `approve` for an in-flight run so a reconnecting client can SEE the
/// standing grant; it is "sticky per run" because the run's authoritative,
/// forge-proof consent lives in the daemon's in-memory `ApplyConsent` handle for
/// the run's whole life.
///
/// **Security invariant (load-bearing):** this disk record is AUDIT-ONLY and is
/// NEVER consulted by the deterministic apply gate. The lead is an arbitrary CLI
/// subprocess with write access to `.nerve/` in `task.cwd`; if the gate trusted
/// this file the lead could forge operator consent and self-escalate dry-run →
/// apply. The gate instead reads the in-memory `ApplyConsent` (see nerve-core
/// `RunOptions::apply_grant`), which the lead cannot reach. A grant is also NOT
/// an acceptance: it is structurally distinct from [`RunReport`]/[`RunCheckpoint`]
/// — it carries ONLY consent + identity, never a verdict / `blocked` /
/// `goal_satisfied` / patch.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ApprovalGrant {
    /// The run this grant is scoped to (`Task::id`). A grant for one run-id is
    /// never consulted for another — no cross-run leak.
    pub run_id: String,
    /// Operator consented to APPLY this run's accepted patch.
    pub apply_consent: bool,
    /// rfc3339 timestamp the grant was recorded (chrono::Utc).
    pub granted_at: String,
}

impl ApprovalGrant {
    /// An apply-consent grant for `run_id`, stamped now.
    pub fn apply(run_id: impl Into<String>) -> Self {
        Self {
            run_id: run_id.into(),
            apply_consent: true,
            granted_at: Utc::now().to_rfc3339(),
        }
    }
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

    /// S11: persist a per-run approval grant as an AUDIT record under
    /// `.nerve/approvals/{run-id}.json`. See [`ApprovalGrant`]: this is written
    /// by the trusted daemon for observability/reconnect and is NEVER consulted
    /// by the apply gate (the gate reads the in-memory `ApplyConsent` handle).
    pub fn record_approval(&self, grant: &ApprovalGrant) -> Result<()> {
        write_json(&self.approval_path(&grant.run_id), grant)
    }

    /// S11: load the standing approval grant for `run_id`, if any. Absence (no
    /// file) is `Ok(None)`. Used only for observability (e.g. daemon `status`),
    /// never as a gate input.
    pub fn load_approval(&self, run_id: &str) -> Result<Option<ApprovalGrant>> {
        let path = self.approval_path(run_id);
        if !path.exists() {
            return Ok(None);
        }
        read_json(&path).map(Some)
    }

    /// S13: persist a [`PlanReport`] under `.nerve/plans/<task_id>.json` so a
    /// later `nv dispatch-plan <id>` can retrieve it and hand its steps off to
    /// the loop. Atomic write; the directory is created on demand. Returns the
    /// path written. Purely additive — plan output is unchanged for callers
    /// that never dispatch.
    pub fn save_plan(&self, report: &PlanReport) -> Result<PathBuf> {
        validate_store_id("plan id", &report.task_id)?;
        fs::create_dir_all(self.plans_dir())
            .with_context(|| format!("failed to create `{}`", self.plans_dir().display()))?;
        let path = self.plan_path(&report.task_id);
        write_json(&path, report)?;
        Ok(path)
    }

    /// S13: load a stored [`PlanReport`] by id. `plan_id` is operator-supplied
    /// (`nv dispatch-plan <plan-id>`), unlike the UUID `Task::id`s elsewhere, so
    /// it is validated as a safe file component first — a traversal id must
    /// never read JSON outside `.nerve/plans/`. Missing plan is an error.
    pub fn load_plan(&self, plan_id: &str) -> Result<PlanReport> {
        validate_store_id("plan id", plan_id)?;
        let path = self.plan_path(plan_id);
        if !path.exists() {
            anyhow::bail!("no stored plan `{plan_id}` at `{}`", path.display());
        }
        read_json(&path)
    }

    /// S13: enumerate stored plan ids (sorted). Empty when no plan has been
    /// saved yet.
    pub fn list_plans(&self) -> Result<Vec<String>> {
        let dir = self.plans_dir();
        if !dir.exists() {
            return Ok(Vec::new());
        }
        let mut ids = Vec::new();
        for entry in
            fs::read_dir(&dir).with_context(|| format!("failed to read `{}`", dir.display()))?
        {
            let path = entry?.path();
            if path.extension().and_then(|value| value.to_str()) != Some("json") {
                continue;
            }
            if let Some(stem) = path.file_stem().and_then(|value| value.to_str()) {
                ids.push(stem.to_string());
            }
        }
        ids.sort();
        Ok(ids)
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
        fs::create_dir_all(self.approvals_dir())
            .with_context(|| format!("failed to create `{}`", self.approvals_dir().display()))?;
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

    /// S11: per-run approval grant audit records (see [`ApprovalGrant`]).
    fn approvals_dir(&self) -> PathBuf {
        self.root_dir().join("approvals")
    }

    /// S13: persisted [`PlanReport`]s, the source for `nv dispatch-plan`.
    fn plans_dir(&self) -> PathBuf {
        self.root_dir().join("plans")
    }

    fn session_path(&self, id: &str) -> PathBuf {
        self.sessions_dir().join(format!("{id}.json"))
    }

    fn checkpoint_path(&self, id: &str) -> PathBuf {
        self.checkpoints_dir().join(format!("{id}.json"))
    }

    fn approval_path(&self, id: &str) -> PathBuf {
        self.approvals_dir().join(format!("{id}.json"))
    }

    fn session_meta_path(&self, id: &str) -> PathBuf {
        self.session_meta_dir().join(format!("{id}.json"))
    }

    fn plan_path(&self, id: &str) -> PathBuf {
        self.plans_dir().join(format!("{id}.json"))
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

/// S13: reject an operator-supplied id that could escape the store directory.
/// Used for `plan_id` (from `nv dispatch-plan <plan-id>`), which—unlike the
/// UUID `Task::id`s the rest of the store keys on—is untrusted input. Mirrors
/// the strict allowlist contract of `mayor_patrol::validate_file_component`
/// (1..=128 chars of `[A-Za-z0-9_-]`), so `/`, `\`, `..`, and control chars all
/// fail closed. UUID task ids (hex + hyphen) pass.
fn validate_store_id(kind: &str, id: &str) -> Result<()> {
    let ok = (1..=128).contains(&id.len())
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    if !ok {
        anyhow::bail!("invalid {kind} `{id}`: must be 1..=128 chars of [A-Za-z0-9_-]");
    }
    Ok(())
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

    fn sample_plan_report(task_id: &str) -> PlanReport {
        PlanReport {
            task_id: task_id.to_string(),
            plan_markdown: "## Objective\nx\n\n## Steps\n1. a\n2. b\n".to_string(),
            reviewer_feedback: String::new(),
            estimated_loc: Some(10),
            estimated_files: vec![PathBuf::from("a.rs")],
            cost: None,
            finished_at: Utc::now(),
        }
    }

    #[test]
    fn plan_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let store = NerveStore::new(dir.path());
        let report = sample_plan_report("plan-001");

        let path = store.save_plan(&report).unwrap();
        assert!(path.ends_with("plans/plan-001.json"));
        let loaded = store.load_plan("plan-001").unwrap();
        assert_eq!(loaded, report);
        assert_eq!(store.list_plans().unwrap(), vec!["plan-001".to_string()]);
    }

    #[test]
    fn load_plan_missing_errors() {
        let dir = tempfile::tempdir().unwrap();
        let store = NerveStore::new(dir.path());
        assert!(store.load_plan("nope").is_err());
        // No plans dir yet → empty list, not an error.
        assert!(store.list_plans().unwrap().is_empty());
    }

    #[test]
    fn load_plan_rejects_traversal_id() {
        let dir = tempfile::tempdir().unwrap();
        let store = NerveStore::new(dir.path());
        for bad in ["../escape", "a/b", "..", "with space", "tab\tid", ""] {
            assert!(
                store.load_plan(bad).is_err(),
                "traversal/invalid id must be rejected: {bad:?}"
            );
        }
    }

    #[test]
    fn save_plan_rejects_traversal_id() {
        let dir = tempfile::tempdir().unwrap();
        let store = NerveStore::new(dir.path());
        let report = sample_plan_report("../escape");
        assert!(store.save_plan(&report).is_err());
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
            cancelled: false,
            goal_satisfied: None,
            applied: false,
            blocked: false,
            apply_classification: None,
            ran_unconfined: false,
        };
        store.save_report(&report).unwrap();

        // The finalized report supersedes the in-flight checkpoint.
        assert!(store.load_checkpoint(&task.id).is_err());
        assert!(store.list_checkpoints().unwrap().is_empty());
    }

    /// H13: the additive `ran_unconfined` telemetry is exposed in the serialized
    /// JSON report when a run degraded to unconfined, and older reports that
    /// predate the field load as `false` (serde default) — never erroring. Pure
    /// telemetry: it rides alongside the verdict; it is not the verdict.
    #[test]
    fn run_report_exposes_ran_unconfined_and_defaults_when_absent() {
        let dir = tempfile::tempdir().unwrap();
        let patch = NvPatch::new(vec![FilePatch::create("created.txt", "created\n")]);
        let task = Task::new("create file", dir.path());
        let report = RunReport {
            task,
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
            cancelled: false,
            goal_satisfied: None,
            applied: false,
            blocked: false,
            apply_classification: None,
            // A run that degraded to an unconfined `Auto` check.
            ran_unconfined: true,
        };

        // The JSON report a degraded run produces carries the signal verbatim.
        let mut value = serde_json::to_value(&report).unwrap();
        assert_eq!(value["ran_unconfined"], serde_json::json!(true));

        // A pre-H13 persisted report has no such key: it must load as `false`,
        // never error — and the deterministic gate fields are untouched.
        value.as_object_mut().unwrap().remove("ran_unconfined");
        let legacy: RunReport = serde_json::from_value(value).unwrap();
        assert!(!legacy.ran_unconfined);
        assert!(!legacy.blocked);
        assert_eq!(legacy.goal_satisfied, None);
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
            cancelled: false,
            goal_satisfied: None,
            applied: false,
            blocked: false,
            apply_classification: None,
            ran_unconfined: false,
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
            cancelled: false,
            goal_satisfied: None,
            applied: false,
            blocked: false,
            apply_classification: None,
            ran_unconfined: false,
        };
        let store = NerveStore::new(dir.path());
        store.save_report(&report).unwrap();

        store.name_session(&task.id, "first pass").unwrap();

        let summary = store.list_sessions().unwrap().remove(0);
        assert_eq!(summary.name.as_deref(), Some("first pass"));
        assert_eq!(summary.cwd, dir.path());
    }

    // --- S11: per-run approval grant audit records ---------------------------

    #[test]
    fn approval_grant_round_trips_and_is_per_run() {
        let dir = tempfile::tempdir().unwrap();
        let store = NerveStore::new(dir.path());

        // Absent grant reads as None (the dry-run-safe default).
        assert!(store.load_approval("run-a").unwrap().is_none());

        let grant = ApprovalGrant::apply("run-a");
        assert!(grant.apply_consent);
        store.record_approval(&grant).unwrap();

        let loaded = store.load_approval("run-a").unwrap().unwrap();
        assert_eq!(loaded, grant);
        // Per-run isolation: a grant for run-a is never visible for run-b.
        assert!(store.load_approval("run-b").unwrap().is_none());
    }

    #[test]
    fn approval_grant_is_structurally_distinct_from_checkpoint() {
        // A grant must never be deserializable as a checkpoint (or vice-versa):
        // `deny_unknown_fields` on both keeps a consent record from ever being
        // read as in-flight run state, and vice-versa.
        let grant = ApprovalGrant::apply("run-a");
        let grant_json = serde_json::to_string(&grant).unwrap();
        assert!(serde_json::from_str::<RunCheckpoint>(&grant_json).is_err());

        let dir = tempfile::tempdir().unwrap();
        let task = Task::new("t", dir.path());
        let checkpoint = sample_checkpoint(&task, 1);
        let checkpoint_json = serde_json::to_string(&checkpoint).unwrap();
        assert!(serde_json::from_str::<ApprovalGrant>(&checkpoint_json).is_err());
    }
}
