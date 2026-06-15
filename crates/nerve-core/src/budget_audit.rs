use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct BudgetSnapshot {
    #[serde(default)]
    pub max_total_tokens: Option<u64>,
    #[serde(default)]
    pub max_estimated_cost_microusd: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BudgetAuditEntry {
    pub ts: DateTime<Utc>,
    pub prev: BudgetSnapshot,
    pub next: BudgetSnapshot,
    pub source: String,
    pub user_confirmed: bool,
    // v0.2.0: hash chain deferred (see §3 Tier 2g sec-3 #6).
}

/// Append one audit entry under an advisory lock, atomically rewriting the
/// audit file via a tempfile + rename. The lock file lives next to the audit
/// file and is shared with future `/budget` callers (sec-3 #7).
pub fn append_budget_audit_entry(audit_path: &Path, entry: BudgetAuditEntry) -> Result<()> {
    let parent = audit_path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    fs::create_dir_all(&parent)
        .with_context(|| format!("failed to create `{}`", parent.display()))?;

    let lock_path = parent.join("budget.lock");
    let lock_file = fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&lock_path)
        .with_context(|| format!("failed to open `{}`", lock_path.display()))?;
    lock_file
        .lock_exclusive()
        .with_context(|| format!("failed to acquire `{}`", lock_path.display()))?;

    let result = (|| -> Result<()> {
        let mut entries = read_audit_log(audit_path)?;
        entries.push(entry);
        write_audit_log_atomic(audit_path, &entries)
    })();

    let _ = FileExt::unlock(&lock_file);
    result
}

pub fn read_audit_log(audit_path: &Path) -> Result<Vec<BudgetAuditEntry>> {
    if !audit_path.exists() {
        return Ok(Vec::new());
    }
    let raw = fs::read_to_string(audit_path)
        .with_context(|| format!("failed to read `{}`", audit_path.display()))?;
    if raw.trim().is_empty() {
        return Ok(Vec::new());
    }
    serde_json::from_str(&raw)
        .with_context(|| format!("invalid budget audit JSON in `{}`", audit_path.display()))
}

fn write_audit_log_atomic(audit_path: &Path, entries: &[BudgetAuditEntry]) -> Result<()> {
    let parent = audit_path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let raw = serde_json::to_vec_pretty(entries)?;
    let mut tmp = tempfile::NamedTempFile::new_in(&parent)
        .with_context(|| format!("failed to create temp file in `{}`", parent.display()))?;
    tmp.write_all(&raw)
        .with_context(|| format!("failed to write temp JSON for `{}`", audit_path.display()))?;
    tmp.persist(audit_path)
        .map_err(|err| err.error)
        .with_context(|| format!("failed to move temp JSON to `{}`", audit_path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(prev: u64, next: u64, source: &str) -> BudgetAuditEntry {
        BudgetAuditEntry {
            ts: Utc::now(),
            prev: BudgetSnapshot {
                max_total_tokens: Some(prev),
                max_estimated_cost_microusd: None,
            },
            next: BudgetSnapshot {
                max_total_tokens: Some(next),
                max_estimated_cost_microusd: None,
            },
            source: source.to_string(),
            user_confirmed: true,
        }
    }

    #[test]
    fn append_creates_and_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let audit = dir.path().join("budget-audit.json");
        append_budget_audit_entry(&audit, entry(100, 200, "slash")).unwrap();
        append_budget_audit_entry(&audit, entry(200, 300, "slash")).unwrap();

        let entries = read_audit_log(&audit).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].next.max_total_tokens, Some(200));
        assert_eq!(entries[1].prev.max_total_tokens, Some(200));
    }

    #[test]
    fn empty_file_treated_as_empty_log() {
        let dir = tempfile::tempdir().unwrap();
        let audit = dir.path().join("budget-audit.json");
        std::fs::write(&audit, "").unwrap();
        let entries = read_audit_log(&audit).unwrap();
        assert!(entries.is_empty());
    }
}
