use crate::RunReport;
use anyhow::{Context, Result};
use nerve_patch::{ApplyReport, NvPatch};
use nerve_types::Verdict;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct NerveStore {
    cwd: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionSummary {
    pub id: String,
    pub prompt: String,
    pub started_at: String,
    pub profile: Option<String>,
    pub verdict: Verdict,
    pub rounds: usize,
    pub patch_id: Option<String>,
    pub applied: bool,
    pub blocked: bool,
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
            summaries.push(SessionSummary::from(&report));
        }

        summaries.sort_by(|a, b| b.started_at.cmp(&a.started_at));
        Ok(summaries)
    }

    pub fn load_report(&self, id: &str) -> Result<RunReport> {
        self.load_report_path(&self.session_path(id))
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

    fn upsert_patch_record(&self, record: PatchRecord) -> Result<()> {
        let mut index = self.load_index()?;
        index.patches.retain(|existing| existing.id != record.id);
        index.patches.push(record);
        write_json(&self.index_path(), &index)
    }

    fn set_patch_applied(&self, id: &str, applied: bool) -> Result<()> {
        let mut index = self.load_index()?;
        let Some(record) = index.patches.iter_mut().find(|record| record.id == id) else {
            anyhow::bail!("patch `{id}` is not indexed");
        };
        record.applied = applied;
        write_json(&self.index_path(), &index)
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

    fn patches_dir(&self) -> PathBuf {
        self.root_dir().join("patches")
    }

    fn session_path(&self, id: &str) -> PathBuf {
        self.sessions_dir().join(format!("{id}.json"))
    }

    fn patch_path(&self, id: &str) -> PathBuf {
        self.patches_dir().join(format!("{id}.json"))
    }

    fn index_path(&self) -> PathBuf {
        self.patches_dir().join("index.json")
    }
}

impl SessionSummary {
    fn from(report: &RunReport) -> Self {
        Self {
            id: report.task.id.clone(),
            prompt: report.task.prompt.clone(),
            started_at: report.task.started_at.to_rfc3339(),
            profile: report.selection.id.clone(),
            verdict: report.final_feedback.verdict.clone(),
            rounds: report.rounds.len(),
            patch_id: report.final_patch.as_ref().map(|patch| patch.id.clone()),
            applied: report.applied,
            blocked: report.blocked,
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
    let tmp_path = path.with_extension("json.tmp");
    fs::write(&tmp_path, raw)
        .with_context(|| format!("failed to write `{}`", tmp_path.display()))?;
    fs::rename(&tmp_path, path).with_context(|| {
        format!(
            "failed to move `{}` to `{}`",
            tmp_path.display(),
            path.display()
        )
    })?;
    Ok(())
}
