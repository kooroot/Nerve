use chrono::{DateTime, Utc};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use thiserror::Error;

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
    /// sec-gap-12 hash chain: SHA-256 hex of the previous entry's JSON form
    /// (computed with `prev_hash` filled but no trailing hash field). The
    /// first entry in a log keeps this `None`. v0.2.0 entries also have no
    /// `prev_hash`, so loading them is backward-compatible.
    #[serde(default)]
    pub prev_hash: Option<String>,
}

#[derive(Debug, Error)]
pub enum AuditError {
    #[error("failed to read `{path}`: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("invalid budget audit JSON in `{path}`: {source}")]
    InvalidJson {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error(
        "budget audit hash chain broken at entry {at_entry}: expected prev_hash `{expected_prev}` but found `{actual_prev:?}`"
    )]
    ChainBroken {
        at_entry: usize,
        expected_prev: String,
        actual_prev: Option<String>,
    },
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// Snapshot of the chain head after loading or appending an entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChainStatus {
    /// No entries on disk yet.
    Empty,
    /// Chain links verify end-to-end.
    Intact { entries: usize, last_hash: String },
    /// At least one link is missing or tampered.
    Broken {
        at_entry: usize,
        expected_prev: String,
        actual_prev: Option<String>,
    },
}

/// Loads the existing chain head from disk, validating every link along the
/// way. Append calls re-use the cached head to avoid re-reading the whole
/// log under the advisory lock.
#[derive(Debug, Clone)]
pub struct AuditChainState {
    pub last_hash: Option<String>,
    pub entries: usize,
}

impl AuditChainState {
    pub fn load(audit_path: &Path) -> Result<Self, AuditError> {
        let entries = read_audit_log_raw(audit_path)?;
        let (last_hash, count) = verify_chain(&entries)?;
        Ok(Self {
            last_hash,
            entries: count,
        })
    }

    pub fn append(
        &mut self,
        audit_path: &Path,
        mut entry: BudgetAuditEntry,
    ) -> Result<(), AuditError> {
        let parent = audit_path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));
        fs::create_dir_all(&parent)?;

        let lock_path = parent.join("budget.lock");
        let lock_file = fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&lock_path)?;
        lock_file.lock_exclusive()?;

        let result = (|| -> Result<(), AuditError> {
            // Re-load under the lock to catch a concurrent writer who beat us.
            let mut entries = read_audit_log_raw(audit_path)?;
            let (current_head, _) = verify_chain(&entries)?;
            entry.prev_hash = current_head;
            entries.push(entry.clone());
            write_audit_log_atomic(audit_path, &entries)?;
            self.last_hash = Some(hash_entry(&entry)?);
            self.entries = entries.len();
            Ok(())
        })();

        let _ = FileExt::unlock(&lock_file);
        result
    }

    pub fn verify(audit_path: &Path) -> Result<ChainStatus, AuditError> {
        let entries = read_audit_log_raw(audit_path)?;
        if entries.is_empty() {
            return Ok(ChainStatus::Empty);
        }
        match verify_chain(&entries) {
            Ok((Some(last_hash), count)) => Ok(ChainStatus::Intact {
                entries: count,
                last_hash,
            }),
            Ok((None, _)) => Ok(ChainStatus::Empty),
            Err(AuditError::ChainBroken {
                at_entry,
                expected_prev,
                actual_prev,
            }) => Ok(ChainStatus::Broken {
                at_entry,
                expected_prev,
                actual_prev,
            }),
            Err(other) => Err(other),
        }
    }
}

/// Backwards-compatible append helper. Loads the chain head, links the new
/// entry, and appends under an advisory lock.
pub fn append_budget_audit_entry(
    audit_path: &Path,
    entry: BudgetAuditEntry,
) -> Result<(), AuditError> {
    let mut state = AuditChainState::load(audit_path)?;
    state.append(audit_path, entry)
}

pub fn read_audit_log(audit_path: &Path) -> Result<Vec<BudgetAuditEntry>, AuditError> {
    read_audit_log_raw(audit_path)
}

/// Returns a human-readable warning describing a broken chain head.
pub fn format_chain_broken(status: &ChainStatus) -> Option<String> {
    match status {
        ChainStatus::Broken {
            at_entry,
            expected_prev,
            actual_prev,
        } => Some(format!(
            "RED: budget audit hash chain broken at entry #{at_entry}. expected prev_hash `{expected_prev}`, found `{}`. The audit log may have been tampered with.",
            actual_prev.as_deref().unwrap_or("<missing>")
        )),
        _ => None,
    }
}

fn read_audit_log_raw(audit_path: &Path) -> Result<Vec<BudgetAuditEntry>, AuditError> {
    if !audit_path.exists() {
        return Ok(Vec::new());
    }
    let raw = fs::read_to_string(audit_path).map_err(|source| AuditError::Read {
        path: audit_path.to_path_buf(),
        source,
    })?;
    if raw.trim().is_empty() {
        return Ok(Vec::new());
    }
    serde_json::from_str(&raw).map_err(|source| AuditError::InvalidJson {
        path: audit_path.to_path_buf(),
        source,
    })
}

fn write_audit_log_atomic(
    audit_path: &Path,
    entries: &[BudgetAuditEntry],
) -> Result<(), AuditError> {
    let parent = audit_path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let raw = serde_json::to_vec_pretty(entries).map_err(|source| AuditError::InvalidJson {
        path: audit_path.to_path_buf(),
        source,
    })?;
    let mut tmp = tempfile::NamedTempFile::new_in(&parent)?;
    tmp.write_all(&raw)?;
    tmp.persist(audit_path).map_err(|err| err.error)?;
    Ok(())
}

/// Walks the chain and returns (last_hash, count). Each entry's `prev_hash`
/// must equal the SHA-256 of the previous entry's canonical JSON form.
/// v0.2.0 entries (no `prev_hash`) are accepted only as a contiguous prefix to
/// keep the upgrade path lossless. Once a hashed link appears, missing links are
/// treated as tampering.
fn verify_chain(entries: &[BudgetAuditEntry]) -> Result<(Option<String>, usize), AuditError> {
    let mut previous_hash: Option<String> = None;
    let mut seen_hashed_link = false;
    for (idx, entry) in entries.iter().enumerate() {
        if idx > 0 {
            match (&entry.prev_hash, &previous_hash) {
                (Some(claimed), Some(expected)) if claimed == expected => {
                    seen_hashed_link = true;
                }
                (None, Some(_)) if !seen_hashed_link => {
                    // Legacy prefix entry: tolerated; successor links to its
                    // hash regardless.
                }
                (claimed, expected) => {
                    return Err(AuditError::ChainBroken {
                        at_entry: idx,
                        expected_prev: expected.clone().unwrap_or_default(),
                        actual_prev: claimed.clone(),
                    });
                }
            }
        }
        previous_hash = Some(hash_entry(entry)?);
    }
    Ok((previous_hash, entries.len()))
}

fn hash_entry(entry: &BudgetAuditEntry) -> Result<String, AuditError> {
    let raw = serde_json::to_vec(entry).map_err(|source| AuditError::InvalidJson {
        path: PathBuf::from("<entry>"),
        source,
    })?;
    let mut hasher = Sha256::new();
    hasher.update(&raw);
    Ok(format!("{:x}", hasher.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(prev: u64, next: u64, source: &str) -> BudgetAuditEntry {
        BudgetAuditEntry {
            ts: chrono::TimeZone::with_ymd_and_hms(&Utc, 2026, 1, 1, 0, 0, 0)
                .single()
                .unwrap(),
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
            prev_hash: None,
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

    #[test]
    fn hash_chain_basic_append() {
        let dir = tempfile::tempdir().unwrap();
        let audit = dir.path().join("budget-audit.json");

        append_budget_audit_entry(&audit, entry(100, 200, "slash")).unwrap();
        append_budget_audit_entry(&audit, entry(200, 300, "slash")).unwrap();

        let entries = read_audit_log(&audit).unwrap();
        assert_eq!(entries.len(), 2);
        // First entry: no predecessor → prev_hash stays None.
        assert!(entries[0].prev_hash.is_none());
        // Second entry: prev_hash should equal hash of the first entry.
        let first_hash = hash_entry(&entries[0]).unwrap();
        assert_eq!(entries[1].prev_hash.as_deref(), Some(first_hash.as_str()));

        match AuditChainState::verify(&audit).unwrap() {
            ChainStatus::Intact { entries, last_hash } => {
                assert_eq!(entries, 2);
                assert_eq!(
                    last_hash,
                    hash_entry(&read_audit_log(&audit).unwrap()[1]).unwrap()
                );
            }
            other => panic!("expected intact chain, got {other:?}"),
        }
    }

    #[test]
    fn hash_chain_detects_tamper() {
        let dir = tempfile::tempdir().unwrap();
        let audit = dir.path().join("budget-audit.json");
        append_budget_audit_entry(&audit, entry(100, 200, "slash")).unwrap();
        append_budget_audit_entry(&audit, entry(200, 300, "slash")).unwrap();

        // Manually rewrite the file with a broken prev_hash for entry #1.
        let mut entries = read_audit_log(&audit).unwrap();
        entries[1].prev_hash = Some("deadbeef".repeat(8));
        let raw = serde_json::to_vec_pretty(&entries).unwrap();
        std::fs::write(&audit, raw).unwrap();

        match AuditChainState::verify(&audit).unwrap() {
            ChainStatus::Broken {
                at_entry,
                expected_prev: _,
                actual_prev,
            } => {
                assert_eq!(at_entry, 1);
                assert!(actual_prev.unwrap().starts_with("deadbeef"));
            }
            other => panic!("expected broken chain, got {other:?}"),
        }
    }

    #[test]
    fn hash_chain_detects_removed_link_after_chain_started() {
        let dir = tempfile::tempdir().unwrap();
        let audit = dir.path().join("budget-audit.json");
        append_budget_audit_entry(&audit, entry(100, 200, "slash")).unwrap();
        append_budget_audit_entry(&audit, entry(200, 300, "slash")).unwrap();
        append_budget_audit_entry(&audit, entry(300, 400, "slash")).unwrap();

        let mut entries = read_audit_log(&audit).unwrap();
        entries[2].prev_hash = None;
        let raw = serde_json::to_vec_pretty(&entries).unwrap();
        std::fs::write(&audit, raw).unwrap();

        match AuditChainState::verify(&audit).unwrap() {
            ChainStatus::Broken {
                at_entry,
                actual_prev,
                ..
            } => {
                assert_eq!(at_entry, 2);
                assert_eq!(actual_prev, None);
            }
            other => panic!("expected broken chain, got {other:?}"),
        }
    }

    #[test]
    fn hash_chain_v0_2_0_backfill_links_to_legacy_hash() {
        let dir = tempfile::tempdir().unwrap();
        let audit = dir.path().join("budget-audit.json");

        // Write a legacy (v0.2.0) entry that has no `prev_hash` field.
        let legacy = entry(50, 100, "legacy");
        let raw = serde_json::to_vec_pretty(&vec![legacy.clone()]).unwrap();
        std::fs::write(&audit, raw).unwrap();

        // Append a new entry — it should link to the legacy entry's hash.
        append_budget_audit_entry(&audit, entry(100, 150, "slash")).unwrap();

        let entries = read_audit_log(&audit).unwrap();
        assert_eq!(entries.len(), 2);
        let legacy_hash = hash_entry(&entries[0]).unwrap();
        assert_eq!(entries[1].prev_hash.as_deref(), Some(legacy_hash.as_str()));

        // And verification accepts the chain end-to-end.
        match AuditChainState::verify(&audit).unwrap() {
            ChainStatus::Intact { entries: n, .. } => assert_eq!(n, 2),
            other => panic!("expected intact, got {other:?}"),
        }
    }

    #[test]
    fn format_chain_broken_returns_red_warning() {
        let status = ChainStatus::Broken {
            at_entry: 3,
            expected_prev: "aa".into(),
            actual_prev: Some("bb".into()),
        };
        let msg = format_chain_broken(&status).unwrap();
        assert!(msg.starts_with("RED:"));
        assert!(msg.contains("entry #3"));
        assert!(msg.contains("aa"));
        assert!(msg.contains("bb"));
    }
}
