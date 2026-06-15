use crate::RunReport;
use anyhow::{Context, Result};
use chrono::Utc;
use nerve_patch::{ApplyReport, NvPatch};
use nerve_types::Verdict;
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

        Ok(())
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

    fn session_path(&self, id: &str) -> PathBuf {
        self.sessions_dir().join(format!("{id}.json"))
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
    use nerve_config::{ProfileSelection, ReviewStrictness};
    use nerve_patch::{FilePatch, NvPatch};
    use nerve_types::{AgentOutput, ReviewerFeedback, Task};

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
