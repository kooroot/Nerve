use chrono::{DateTime, Utc};
use fs2::FileExt;
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use subtle::ConstantTimeEq;
use thiserror::Error;

type HmacSha256 = Hmac<Sha256>;

/// Operator environment variable holding the raw budget-audit HMAC key.
pub const AUDIT_KEY_ENV: &str = "NERVE_BUDGET_AUDIT_KEY";
/// Operator environment variable naming a file whose contents are the key.
pub const AUDIT_KEY_FILE_ENV: &str = "NERVE_BUDGET_AUDIT_KEY_FILE";

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
    /// H10 tail authentication. A backward `prev_hash` link authenticates an
    /// entry only via its SUCCESSOR, so the LAST entry's payload would otherwise
    /// be unauthenticated and a key-less writer could edit the most-recent budget
    /// change undetected. In KEYED mode each entry therefore also stores its own
    /// self-MAC = HMAC over its bare payload (this field excluded), which equals
    /// what the next entry's `prev_hash` would be; verification requires the
    /// TAIL's self-MAC to match. Present only in keyed mode and
    /// `skip_serializing_if` None, so unkeyed entries are byte-identical to the
    /// pre-H10 format (the unkeyed chain stays forgeable, as documented).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entry_mac: Option<String>,
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
    /// A link (`prev_hash`) or the tail entry's self-MAC failed to verify.
    /// `expected_prev`/`actual_prev` hold the expected vs stored authenticator at
    /// `at_entry` (a backward link for non-tail entries, the self-MAC for the
    /// tail).
    #[error(
        "budget audit hash chain broken at entry {at_entry}: expected authenticator `{expected_prev}` but found `{actual_prev:?}`"
    )]
    ChainBroken {
        at_entry: usize,
        expected_prev: String,
        actual_prev: Option<String>,
    },
    /// A keyed load/append was attempted over a log that is itself a valid
    /// UNKEYED chain. Fail-closed: we refuse rather than silently "upgrading"
    /// (tolerating the unkeyed prefix under a key would let a non-key-holder
    /// forge — see [`verify_chain`]). This is either a pre-key log (benign
    /// migration) or a keyed log rolled back to an unkeyed forgery; the two are
    /// indistinguishable from the file alone, so the message names both.
    #[error(
        "cannot append/load a keyed budget-audit entry: `{path}` is a valid UNKEYED chain ({unkeyed_entries} entries) but `{key_env}` is set. Either it predates the key (a pre-key log is NOT retroactively protected) or a keyed log was rolled back to an unkeyed forgery — Nerve cannot tell these apart from the file. If you just enabled the key, archive or re-key the old log; otherwise treat this as tampering."
    )]
    KeyOverUnkeyedLog {
        path: PathBuf,
        unkeyed_entries: usize,
        key_env: &'static str,
    },
    /// A budget-audit key was REQUESTED (an env var was set) but could not be
    /// resolved to usable bytes. Fail-closed: we refuse to silently fall back to
    /// the unkeyed (forgeable) chain when the operator clearly asked for keying.
    /// Covers a relative/non-absolute key-file path (which would resolve inside
    /// the repo working directory — a provenance hole), an unreadable key file,
    /// or an empty key file.
    #[error(
        "budget audit key from `{env}` is misconfigured: {reason}. Refusing to fall back to an unkeyed (forgeable) chain; fix the key or unset `{env}` to run unkeyed deliberately."
    )]
    KeyMisconfigured {
        env: &'static str,
        reason: String,
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
    /// Load and verify the chain, resolving the operator key from the
    /// environment ([`resolve_audit_key`]). A misconfigured key (set but
    /// unresolvable) fails closed rather than silently loading unkeyed.
    pub fn load(audit_path: &Path) -> Result<Self, AuditError> {
        let key = resolve_audit_key()?;
        Self::load_with_key(audit_path, key.as_deref())
    }

    /// Like [`load`](Self::load) but with an explicitly-supplied key (or `None`
    /// for the unkeyed/legacy chain). Lets callers and tests inject a key
    /// without mutating the process environment.
    pub fn load_with_key(audit_path: &Path, key: Option<&[u8]>) -> Result<Self, AuditError> {
        let entries = read_audit_log_raw(audit_path)?;
        let (last_hash, count) = verify_chain(&entries, key)
            .map_err(|e| diagnose_keyed_load(e, &entries, key, audit_path))?;
        Ok(Self {
            last_hash,
            entries: count,
        })
    }

    /// Append an entry, resolving the operator key from the environment. A
    /// misconfigured key fails closed rather than silently appending unkeyed.
    pub fn append(
        &mut self,
        audit_path: &Path,
        entry: BudgetAuditEntry,
    ) -> Result<(), AuditError> {
        let key = resolve_audit_key()?;
        self.append_with_key(audit_path, entry, key.as_deref())
    }

    /// Like [`append`](Self::append) but with an explicitly-supplied key. The new
    /// entry links to the KEYED hash of the current head, so once a key is
    /// configured every appended link is HMAC-protected.
    pub fn append_with_key(
        &mut self,
        audit_path: &Path,
        mut entry: BudgetAuditEntry,
        key: Option<&[u8]>,
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
            let (current_head, _) = verify_chain(&entries, key)
                .map_err(|e| diagnose_keyed_load(e, &entries, key, audit_path))?;
            entry.prev_hash = current_head;
            // H10 tail authentication: in keyed mode the entry carries its own
            // self-MAC (= what the next entry's `prev_hash` would be) so the tail
            // is authenticated even before a successor exists. `hash_entry`
            // excludes `entry_mac`, so this is well-defined.
            if key.is_some() {
                entry.entry_mac = Some(hash_entry(&entry, key)?);
            }
            entries.push(entry.clone());
            write_audit_log_atomic(audit_path, &entries)?;
            self.last_hash = Some(hash_entry(&entry, key)?);
            self.entries = entries.len();
            Ok(())
        })();

        let _ = FileExt::unlock(&lock_file);
        result
    }

    /// Verify the chain, resolving the operator key from the environment. A
    /// misconfigured key fails closed rather than silently verifying unkeyed.
    pub fn verify(audit_path: &Path) -> Result<ChainStatus, AuditError> {
        let key = resolve_audit_key()?;
        Self::verify_with_key(audit_path, key.as_deref())
    }

    /// Like [`verify`](Self::verify) but with an explicitly-supplied key.
    pub fn verify_with_key(
        audit_path: &Path,
        key: Option<&[u8]>,
    ) -> Result<ChainStatus, AuditError> {
        let entries = read_audit_log_raw(audit_path)?;
        if entries.is_empty() {
            return Ok(ChainStatus::Empty);
        }
        match verify_chain(&entries, key) {
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
/// entry, and appends under an advisory lock. Resolves the operator key from the
/// environment ([`resolve_audit_key`]).
pub fn append_budget_audit_entry(
    audit_path: &Path,
    entry: BudgetAuditEntry,
) -> Result<(), AuditError> {
    let key = resolve_audit_key()?;
    append_budget_audit_entry_with_key(audit_path, entry, key.as_deref())
}

/// Like [`append_budget_audit_entry`] but with an explicitly-supplied key.
pub fn append_budget_audit_entry_with_key(
    audit_path: &Path,
    entry: BudgetAuditEntry,
    key: Option<&[u8]>,
) -> Result<(), AuditError> {
    let mut state = AuditChainState::load_with_key(audit_path, key)?;
    state.append_with_key(audit_path, entry, key)
}

pub fn read_audit_log(audit_path: &Path) -> Result<Vec<BudgetAuditEntry>, AuditError> {
    read_audit_log_raw(audit_path)
}

/// Returns a human-readable warning describing a broken chain head.
///
/// Honest scope (H10): a broken chain proves the log was edited after it was
/// written, but an INTACT chain does NOT prove authenticity. Without a key the
/// chain is unkeyed SHA-256, so anyone who can write the file can recompute a
/// fully valid chain; with a key (`NERVE_BUDGET_AUDIT_KEY`) a non-key-holder
/// cannot forge or edit a keyed link (and a downgrade to an unkeyed/pre-key chain
/// is now DETECTED), but any writer can still TRUNCATE the log to an earlier keyed
/// prefix (rollback). So this detects accidental edits and naive tampering —
/// keep the key off the host you are defending against.
pub fn format_chain_broken(status: &ChainStatus) -> Option<String> {
    match status {
        ChainStatus::Broken {
            at_entry,
            expected_prev,
            actual_prev,
        } => Some(format!(
            "RED: budget audit hash chain broken at entry #{at_entry} (expected prev_hash `{expected_prev}`, found `{}`): the log no longer verifies — it was modified after it was written, OR (if you recently set `{AUDIT_KEY_ENV}`) it predates the key (a pre-key log is not retroactively protected; archive or re-key it). Note: an intact chain is NOT proof of authenticity — without `{AUDIT_KEY_ENV}` anyone who can write the file can forge a valid chain, and even keyed, a writer can roll the log back. Keep the key off the defended host.",
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

/// Walks the chain and returns (last_hash, count). Each entry's `prev_hash` must
/// equal the keyed hash of the previous entry's canonical JSON form: HMAC-SHA-256
/// under `key`, or plain SHA-256 when `key` is `None`.
///
/// Unkeyed mode (`key = None`) is byte-identical to the pre-H10 behavior: v0.2.0
/// entries (no `prev_hash`) are accepted only as a contiguous PREFIX, and once a
/// hashed link appears, a missing link is tampering.
///
/// Keyed mode (`key = Some`) is STRICT: every link (idx > 0) MUST equal the HMAC
/// of its predecessor — there is NO tolerance for unkeyed (SHA) or missing
/// links. This is deliberate and is the whole point of keying: if an unkeyed
/// prefix were tolerated under a key, a local file-writer who does not hold the
/// key could rewrite the entire log as an unkeyed chain and have it pass keyed
/// verification (silently re-deriving a "valid" forged chain). Strictness closes
/// that hole.
///
/// Keyed mode ALSO authenticates the TAIL. A backward `prev_hash` link
/// authenticates an entry only via its successor, so the last entry's payload
/// would otherwise be editable by a key-less writer (the latest budget change is
/// the most security-relevant record). Each keyed entry therefore stores a
/// self-MAC (`entry_mac`, = `hash_entry(self, key)`, which excludes the field
/// itself) and verification REQUIRES the tail's `entry_mac` to match. Non-tail
/// entries do not need one — their successor's `prev_hash` already authenticates
/// them.
///
/// The cost of strictness is that a pre-key unkeyed log (any non-empty log:
/// either a real SHA link, or a lone legacy entry whose tail self-MAC is absent)
/// does NOT verify once a key is configured — a deliberate migration boundary:
/// archive or re-key the old log (see [`format_chain_broken`] and the README).
/// Only an EMPTY log can begin a fresh keyed chain. The residual limitation — a
/// key-holder can rewrite history, and any writer can TRUNCATE the log to an
/// earlier keyed prefix (a shorter valid keyed chain is indistinguishable from
/// the real one without external knowledge of the expected head) — is disclosed
/// honestly; keep the key off the defended host.
fn verify_chain(
    entries: &[BudgetAuditEntry],
    key: Option<&[u8]>,
) -> Result<(Option<String>, usize), AuditError> {
    let keyed = key.is_some();
    let mut previous_hash: Option<String> = None;
    let mut seen_hashed_link = false;
    for (idx, entry) in entries.iter().enumerate() {
        if idx > 0 {
            match (&entry.prev_hash, &previous_hash) {
                (Some(claimed), Some(expected)) if hashes_equal(claimed, expected) => {
                    seen_hashed_link = true;
                }
                // v0.2.0 legacy prefix (no prev_hash) is tolerated ONLY in the
                // unkeyed path. Under a key, a missing link is not tolerated —
                // otherwise an all-`None` chain would forge past keyed verify.
                (None, Some(_)) if !keyed && !seen_hashed_link => {}
                (claimed, expected) => {
                    return Err(AuditError::ChainBroken {
                        at_entry: idx,
                        expected_prev: expected.clone().unwrap_or_default(),
                        actual_prev: claimed.clone(),
                    });
                }
            }
        }
        previous_hash = Some(hash_entry(entry, key)?);
    }

    // Tail authentication (keyed only): the last entry has no successor link, so
    // its self-MAC must equal what the next entry's `prev_hash` would be
    // (`previous_hash` already holds `hash_entry(tail, key)`). A missing or
    // mismatched tail `entry_mac` means the latest entry was edited or the log
    // predates the key.
    if keyed && let (Some(tail), Some(expected)) = (entries.last(), previous_hash.as_ref()) {
        let at_entry = entries.len() - 1;
        match &tail.entry_mac {
            Some(claimed) if hashes_equal(claimed, expected) => {}
            other => {
                return Err(AuditError::ChainBroken {
                    at_entry,
                    expected_prev: expected.clone(),
                    actual_prev: other.clone(),
                });
            }
        }
    }

    Ok((previous_hash, entries.len()))
}

/// Constant-time equality of two hex-encoded chain hashes. Every link comparison
/// goes through this so that a KEYED (HMAC) link — whose `expected` value is
/// secret-derived — cannot leak via comparison timing to a local writer who can
/// place a chosen `claimed` value and time repeated verifications (the textbook
/// MAC-verification side channel). Unkeyed (SHA) links are public so timing is
/// irrelevant there, but comparing uniformly keeps one code path and is harmless
/// on this cold path. A length mismatch (non-secret; hashes are fixed-width) is
/// simply unequal.
fn hashes_equal(claimed: &str, expected: &str) -> bool {
    claimed.as_bytes().ct_eq(expected.as_bytes()).into()
}

/// When a keyed verify fails, decide whether it is the migration boundary
/// ([`AuditError::KeyOverUnkeyedLog`]) or a genuine break. It is the migration
/// boundary iff a key is set, the failure is a chain break, AND the SAME bytes
/// form a valid UNKEYED chain (so the log is forgeable-but-internally-consistent
/// — a pre-key log or a rollback-to-unkeyed forgery, which are indistinguishable
/// from the file). A log that does not even verify unkeyed (genuine garble) keeps
/// its original [`AuditError::ChainBroken`]. Diagnostic only — the caller still
/// fails closed either way.
fn diagnose_keyed_load(
    err: AuditError,
    entries: &[BudgetAuditEntry],
    key: Option<&[u8]>,
    audit_path: &Path,
) -> AuditError {
    if key.is_some()
        && matches!(err, AuditError::ChainBroken { .. })
        && verify_chain(entries, None).is_ok()
    {
        AuditError::KeyOverUnkeyedLog {
            path: audit_path.to_path_buf(),
            unkeyed_entries: entries.len(),
            key_env: AUDIT_KEY_ENV,
        }
    } else {
        err
    }
}

/// Resolve the operator's budget-audit HMAC key from OUTSIDE the repository.
///
/// The key is read ONLY from the operator's process environment — never from
/// `nerve.config.json`, `.nerve/`, or any repo-local file — so a cloned or
/// hostile repository cannot supply or forge it (a clone cannot set the
/// operator's parent-process environment). Precedence: the raw key bytes in
/// `NERVE_BUDGET_AUDIT_KEY`, else the (trimmed) contents of the file named by
/// `NERVE_BUDGET_AUDIT_KEY_FILE` (which MUST be an absolute path).
///
/// Returns `Ok(None)` only when NEITHER env var requests a key (the legitimate
/// unkeyed default). When a key IS requested but cannot be resolved — a
/// set-but-non-UTF-8 env value, a relative/non-absolute key-file path (which
/// would resolve repo-locally), an unreadable key file, or an empty key file —
/// this returns `Err(AuditError::KeyMisconfigured)` and the caller fails closed
/// rather than silently downgrading to the unkeyed (forgeable) chain.
///
/// Note on UTF-8: `std::env::var` distinguishes `NotPresent` (the var is unset —
/// the legitimate unkeyed default) from `NotUnicode` (the var IS set but its
/// value is not valid UTF-8). The latter is a requested-but-unresolvable key, so
/// it must fail closed; using `std::env::var(..).ok()` would collapse it to
/// `None` and silently run unkeyed — exactly the fail-open downgrade this guards.
pub fn resolve_audit_key() -> Result<Option<Vec<u8>>, AuditError> {
    let raw = env_key_var(AUDIT_KEY_ENV)?;
    // Precedence: a usable raw key wins and short-circuits — the key-file env is
    // then NOT consulted (matching `audit_key_from`), so a stale/malformed
    // key-file var cannot make a valid raw key fail. Otherwise read the key file
    // (its own `NotUnicode` is likewise fail-closed, never silently unkeyed).
    let key_file = match raw {
        Some(ref r) if !r.trim().is_empty() => None,
        _ => env_key_var(AUDIT_KEY_FILE_ENV)?,
    };
    audit_key_from(raw, key_file)
}

/// Read one key-providing env var, failing closed on a set-but-non-UTF-8 value.
fn env_key_var(env: &'static str) -> Result<Option<String>, AuditError> {
    classify_key_var(env, std::env::var(env))
}

/// Pure classification of a [`std::env::var`] result for a key-providing env var,
/// factored out so the `NotUnicode` fail-closed branch is unit-testable without
/// mutating (or relying on) the process environment:
/// - `NotPresent` → `Ok(None)` (the var is unset — the legitimate unkeyed default);
/// - `NotUnicode` → `Err(KeyMisconfigured)` (the var IS set but unusable — a
///   requested key we cannot resolve, which must NOT silently fall back to unkeyed);
/// - `Ok(value)` → `Ok(Some(value))` (trimming / empty-is-unset / the absolute-path
///   and file rules are applied downstream by [`audit_key_from`]).
fn classify_key_var(
    env: &'static str,
    value: Result<String, std::env::VarError>,
) -> Result<Option<String>, AuditError> {
    match value {
        Ok(value) => Ok(Some(value)),
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(std::env::VarError::NotUnicode(_)) => Err(AuditError::KeyMisconfigured {
            env,
            reason: "the environment variable is set but its value is not valid UTF-8".to_string(),
        }),
    }
}

/// Pure-ish core of [`resolve_audit_key`], factored out so precedence, the
/// absolute-path rule, and file handling are unit-testable without mutating the
/// process environment.
///
/// - A non-empty (trimmed) raw key wins and short-circuits — the file is not
///   touched.
/// - A non-empty (trimmed) `key_file` is an EXPLICIT request to key from a file.
///   The path MUST be absolute (a relative path would resolve inside the repo
///   working directory — a provenance hole); the file must be readable; and its
///   trimmed contents must be non-empty. Any of these failing returns
///   `Err(KeyMisconfigured)` — fail-closed, NOT a silent unkeyed fallback.
/// - Empty/whitespace raw with no file, or neither var set, returns `Ok(None)`
///   (the unkeyed default; an empty env var is treated as "not set").
fn audit_key_from(
    raw: Option<String>,
    key_file: Option<String>,
) -> Result<Option<Vec<u8>>, AuditError> {
    if let Some(raw) = raw {
        let trimmed = raw.trim();
        if !trimmed.is_empty() {
            return Ok(Some(trimmed.as_bytes().to_vec()));
        }
    }
    if let Some(path) = key_file {
        let path = path.trim();
        if !path.is_empty() {
            // Provenance: only an absolute path is honored, so the key bytes can
            // never come from a file resolved against the (repo) working dir.
            if !Path::new(path).is_absolute() {
                return Err(AuditError::KeyMisconfigured {
                    env: AUDIT_KEY_FILE_ENV,
                    reason: format!(
                        "`{path}` is not an absolute path (a relative path would resolve inside the repository working directory)"
                    ),
                });
            }
            let contents = fs::read_to_string(path).map_err(|source| {
                AuditError::KeyMisconfigured {
                    env: AUDIT_KEY_FILE_ENV,
                    reason: format!("could not read key file `{path}`: {source}"),
                }
            })?;
            let trimmed = contents.trim();
            if trimmed.is_empty() {
                return Err(AuditError::KeyMisconfigured {
                    env: AUDIT_KEY_FILE_ENV,
                    reason: format!("key file `{path}` is empty"),
                });
            }
            return Ok(Some(trimmed.as_bytes().to_vec()));
        }
    }
    Ok(None)
}

/// Hash one entry for the chain. With `key = Some(..)` this is HMAC-SHA-256 (a
/// local file-writer who does not hold the key cannot forge a valid link); with
/// `key = None` it is plain SHA-256, byte-identical to the pre-H10 format so
/// existing unkeyed logs keep verifying.
///
/// The entry's own `entry_mac` is EXCLUDED from the hashed bytes: this makes the
/// self-MAC (`entry_mac = hash_entry(self, key)`) well-defined and non-circular,
/// and a successor's `prev_hash = hash_entry(predecessor, key)` therefore equals
/// the predecessor's `entry_mac`. For unkeyed entries `entry_mac` is always
/// `None` (omitted by `skip_serializing_if`), so this exclusion is a no-op and
/// the bytes stay identical to pre-H10.
fn hash_entry(entry: &BudgetAuditEntry, key: Option<&[u8]>) -> Result<String, AuditError> {
    // Exclude `entry_mac` without an allocation in the common (None) case.
    let raw = if entry.entry_mac.is_some() {
        let mut bare = entry.clone();
        bare.entry_mac = None;
        serde_json::to_vec(&bare)
    } else {
        serde_json::to_vec(entry)
    }
    .map_err(|source| AuditError::InvalidJson {
        path: PathBuf::from("<entry>"),
        source,
    })?;
    match key {
        Some(key) => {
            // `new_from_slice` accepts a key of any length and never errors.
            let mut mac =
                <HmacSha256 as Mac>::new_from_slice(key).expect("HMAC accepts any key length");
            mac.update(&raw);
            Ok(to_hex(&mac.finalize().into_bytes()))
        }
        None => {
            let mut hasher = Sha256::new();
            hasher.update(&raw);
            // Keep the exact pre-H10 formatting so on-disk SHA links still match.
            Ok(format!("{:x}", hasher.finalize()))
        }
    }
}

/// Lowercase, zero-padded hex with no separators — matches `format!("{:x}", _)`
/// over a digest's byte array, so keyed and unkeyed hashes share one encoding.
fn to_hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
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
            entry_mac: None,
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
        let first_hash = hash_entry(&entries[0], None).unwrap();
        assert_eq!(entries[1].prev_hash.as_deref(), Some(first_hash.as_str()));

        match AuditChainState::verify(&audit).unwrap() {
            ChainStatus::Intact { entries, last_hash } => {
                assert_eq!(entries, 2);
                assert_eq!(
                    last_hash,
                    hash_entry(&read_audit_log(&audit).unwrap()[1], None).unwrap()
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
        let legacy_hash = hash_entry(&entries[0], None).unwrap();
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

    // ----- H10: keyed (HMAC) chain -----

    /// `audit_key_from` precedence + provenance + fail-closed semantics:
    /// - a non-empty raw key wins and short-circuits (the file is never read);
    /// - neither var, or empty/whitespace raw with no file, resolves to `Ok(None)`
    ///   (the legitimate unkeyed default);
    /// - an EXPLICIT key-file request that cannot be satisfied (relative path,
    ///   unreadable, or empty) is `Err(KeyMisconfigured)` — NOT a silent unkeyed
    ///   fallback. No env mutation.
    #[test]
    fn audit_key_from_resolves_precedence_provenance_and_fails_closed() {
        // Raw key wins over the file (file path is irrelevant — and not read —
        // when raw is set), and is trimmed.
        assert_eq!(
            audit_key_from(Some("  secret  ".into()), Some("relative/nope".into())).unwrap(),
            Some(b"secret".to_vec()),
            "raw key takes precedence, is trimmed, and the file is never touched"
        );
        // Empty/whitespace raw with no file, and neither set → Ok(None) (unkeyed).
        assert_eq!(audit_key_from(Some("   ".into()), None).unwrap(), None);
        assert_eq!(audit_key_from(None, None).unwrap(), None);
        assert_eq!(audit_key_from(None, Some("   ".into())).unwrap(), None);

        // Provenance: a RELATIVE key-file path is rejected loudly (it would
        // resolve inside the repo working directory).
        let err = audit_key_from(None, Some("keys/audit.key".into())).unwrap_err();
        assert!(
            matches!(err, AuditError::KeyMisconfigured { env, .. } if env == AUDIT_KEY_FILE_ENV),
            "relative key-file path must be KeyMisconfigured, got {err:?}"
        );

        // Fail-closed: an absolute but UNREADABLE key file is an error, not None.
        let err = audit_key_from(None, Some("/definitely/not/here.key".into())).unwrap_err();
        assert!(
            matches!(err, AuditError::KeyMisconfigured { .. }),
            "unreadable key file must fail closed, got {err:?}"
        );

        // A readable, non-empty, ABSOLUTE key file resolves (and is trimmed).
        let dir = tempfile::tempdir().unwrap();
        let key_path = dir.path().join("k.key");
        std::fs::write(&key_path, "filekey\n").unwrap();
        assert!(key_path.is_absolute());
        assert_eq!(
            audit_key_from(None, Some(key_path.to_string_lossy().into_owned())).unwrap(),
            Some(b"filekey".to_vec())
        );

        // Fail-closed: an absolute but EMPTY key file is an error, not None.
        let empty_path = dir.path().join("empty.key");
        std::fs::write(&empty_path, "  \n").unwrap();
        let err =
            audit_key_from(None, Some(empty_path.to_string_lossy().into_owned())).unwrap_err();
        assert!(
            matches!(err, AuditError::KeyMisconfigured { .. }),
            "empty key file must fail closed, got {err:?}"
        );
    }

    /// `classify_key_var` fails closed on a set-but-non-UTF-8 key env var: an
    /// unset var is the unkeyed default (`None`), a valid value passes through,
    /// but `NotUnicode` is a REQUESTED-but-unresolvable key and must surface as
    /// `KeyMisconfigured` — NOT silently collapse to `None`/unkeyed (the fail-open
    /// hole that `std::env::var(..).ok()` would have left open).
    #[test]
    fn classify_key_var_fails_closed_on_non_utf8() {
        // Unset → unkeyed default.
        assert_eq!(
            classify_key_var(AUDIT_KEY_ENV, Err(std::env::VarError::NotPresent)).unwrap(),
            None
        );
        // Set & valid → passed through verbatim (trimming/empty handled downstream).
        assert_eq!(
            classify_key_var(AUDIT_KEY_ENV, Ok("secret".to_string())).unwrap(),
            Some("secret".to_string())
        );
        // Set but non-UTF-8 → KeyMisconfigured (fail-closed), naming the env var.
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStringExt;
            let bad = std::ffi::OsString::from_vec(vec![0x66, 0xff, 0x6f]); // "f\xffo"
            let err = classify_key_var(
                AUDIT_KEY_FILE_ENV,
                Err(std::env::VarError::NotUnicode(bad)),
            )
            .unwrap_err();
            assert!(
                matches!(err, AuditError::KeyMisconfigured { env, .. } if env == AUDIT_KEY_FILE_ENV),
                "non-UTF-8 key env var must be KeyMisconfigured, got {err:?}"
            );
        }
    }

    /// A keyed hash (HMAC) differs from the unkeyed SHA-256 hash and from the
    /// hash under a different key — so the key actually participates.
    #[test]
    fn hash_entry_keyed_differs_from_unkeyed_and_other_keys() {
        let e = entry(100, 200, "slash");
        let sha = hash_entry(&e, None).unwrap();
        let hmac_a = hash_entry(&e, Some(b"key-a")).unwrap();
        let hmac_b = hash_entry(&e, Some(b"key-b")).unwrap();
        assert_ne!(sha, hmac_a, "HMAC must differ from plain SHA-256");
        assert_ne!(hmac_a, hmac_b, "different keys must produce different hashes");
        // Both encodings are 64 lowercase hex chars (SHA-256 / HMAC-SHA-256).
        assert_eq!(sha.len(), 64);
        assert_eq!(hmac_a.len(), 64);
        assert!(hmac_a.bytes().all(|b| b.is_ascii_hexdigit()));
    }

    /// The unkeyed (`None`) path is byte-identical to the pre-H10 SHA format, so
    /// existing on-disk logs keep verifying.
    #[test]
    fn hash_entry_unkeyed_matches_plain_sha256() {
        let e = entry(1, 2, "slash");
        let raw = serde_json::to_vec(&e).unwrap();
        let mut hasher = Sha256::new();
        hasher.update(&raw);
        let expected = format!("{:x}", hasher.finalize());
        assert_eq!(hash_entry(&e, None).unwrap(), expected);
    }

    /// The constant-time link comparison decides accept/reject identically to
    /// `==` (equal hex accepts; any difference or length mismatch rejects). Only
    /// the timing differs — which we cannot assert here, but the decision must
    /// not change.
    #[test]
    fn hashes_equal_matches_plain_equality() {
        let h = "a".repeat(64);
        assert!(hashes_equal(&h, &h));
        let mut other = h.clone();
        other.replace_range(63..64, "b"); // differ in the last nibble only
        assert!(!hashes_equal(&h, &other));
        // Length mismatch is unequal (hashes are fixed-width; length is public).
        assert!(!hashes_equal("abc", "abc123"));
        assert!(!hashes_equal("", &h));
    }

    /// A keyed chain verifies under its key and is REJECTED under a wrong key or
    /// no key (a verifier without the key cannot confirm a keyed chain).
    #[test]
    fn keyed_chain_verifies_only_under_the_right_key() {
        let dir = tempfile::tempdir().unwrap();
        let audit = dir.path().join("budget-audit.json");
        let key: &[u8] = b"operator-secret";

        append_budget_audit_entry_with_key(&audit, entry(100, 200, "slash"), Some(key)).unwrap();
        append_budget_audit_entry_with_key(&audit, entry(200, 300, "slash"), Some(key)).unwrap();

        // Right key → intact.
        assert!(matches!(
            AuditChainState::verify_with_key(&audit, Some(key)).unwrap(),
            ChainStatus::Intact { entries: 2, .. }
        ));
        // Wrong key → broken at the first keyed link.
        assert!(matches!(
            AuditChainState::verify_with_key(&audit, Some(b"wrong")).unwrap(),
            ChainStatus::Broken { at_entry: 1, .. }
        ));
        // No key → broken too (cannot downgrade a keyed chain to SHA).
        assert!(matches!(
            AuditChainState::verify_with_key(&audit, None).unwrap(),
            ChainStatus::Broken { at_entry: 1, .. }
        ));
    }

    /// The core forgery the key defeats: an attacker who can write the file edits
    /// an entry and recomputes the successor's `prev_hash` with the UNKEYED SHA
    /// (they don't hold the key). Under the key this is detected as broken.
    #[test]
    fn keyed_chain_detects_sha_recomputed_forgery() {
        let dir = tempfile::tempdir().unwrap();
        let audit = dir.path().join("budget-audit.json");
        let key: &[u8] = b"operator-secret";

        append_budget_audit_entry_with_key(&audit, entry(100, 200, "slash"), Some(key)).unwrap();
        append_budget_audit_entry_with_key(&audit, entry(200, 300, "slash"), Some(key)).unwrap();

        // Forge: bump entry #0's ceiling and relink #1 with the SHA the attacker
        // CAN compute (no key) — a perfect forgery against an unkeyed chain.
        let mut entries = read_audit_log(&audit).unwrap();
        entries[0].next.max_total_tokens = Some(999_999);
        entries[1].prev_hash = Some(hash_entry(&entries[0], None).unwrap());
        std::fs::write(&audit, serde_json::to_vec_pretty(&entries).unwrap()).unwrap();

        // Unkeyed verify is fooled (this is exactly the pre-H10 weakness)...
        assert!(matches!(
            AuditChainState::verify_with_key(&audit, None).unwrap(),
            ChainStatus::Intact { .. }
        ));
        // ...but the keyed verify catches it: the SHA link is not the HMAC link.
        assert!(matches!(
            AuditChainState::verify_with_key(&audit, Some(key)).unwrap(),
            ChainStatus::Broken { at_entry: 1, .. }
        ));
    }

    /// Migration boundary (fail-closed): once an operator has run UNKEYED long
    /// enough to build a real SHA *link* (≥2 entries), configuring a key and
    /// appending FAILS LOUDLY instead of silently "upgrading". Tolerating the
    /// unkeyed SHA prefix under a key is exactly the forgery hole strict mode
    /// closes — a non-key-holder could otherwise rewrite the whole log as SHA
    /// links and pass keyed verify. The operator must archive or re-key the old
    /// log. (The single-entry case fails the same way — see the next test; only
    /// an EMPTY log can begin a fresh keyed chain.)
    #[test]
    fn append_with_key_over_unkeyed_log_fails_loudly() {
        let dir = tempfile::tempdir().unwrap();
        let audit = dir.path().join("budget-audit.json");

        // Operator ran unkeyed and built a real SHA-linked chain (≥2 entries).
        append_budget_audit_entry_with_key(&audit, entry(100, 200, "slash"), None).unwrap();
        append_budget_audit_entry_with_key(&audit, entry(200, 300, "slash"), None).unwrap();

        // Now they configure a key and try to append: the existing unkeyed chain
        // cannot pass keyed verification, so the append is rejected — loud, not
        // silent, with the actionable migration-boundary error that names BOTH
        // indistinguishable causes (pre-key log vs rollback-to-unkeyed forgery).
        let key: &[u8] = b"now-keyed";
        let err = append_budget_audit_entry_with_key(&audit, entry(300, 400, "slash"), Some(key))
            .expect_err("appending with a key over a multi-entry unkeyed log must fail loudly");
        assert!(
            matches!(
                err,
                AuditError::KeyOverUnkeyedLog {
                    unkeyed_entries: 2,
                    ..
                }
            ),
            "expected KeyOverUnkeyedLog over a 2-entry unkeyed log, got {err:?}"
        );

        // The on-disk log is untouched by the failed append: a keyed verify of it
        // is Broken (not silently accepted), while the unkeyed verify still passes.
        assert!(matches!(
            AuditChainState::verify_with_key(&audit, Some(key)).unwrap(),
            ChainStatus::Broken { at_entry: 1, .. }
        ));
        assert!(matches!(
            AuditChainState::verify_with_key(&audit, None).unwrap(),
            ChainStatus::Intact { entries: 2, .. }
        ));
    }

    /// Fail-closed migration boundary, single-entry case: even a 1-entry unkeyed
    /// log (a lone legacy entry — no SHA *link* yet) can NOT be silently adopted
    /// into a keyed chain. Tail authentication (R1) requires the adopted entry to
    /// carry a keyed self-MAC it does not have, so a keyed load/append over it
    /// fails loudly with `KeyOverUnkeyedLog { unkeyed_entries: 1 }`; the operator
    /// must re-key. Adopting it unMACed (the prior behavior) would have left the
    /// lone — and tail — payload forgeable by a key-less writer. Only an EMPTY log
    /// can begin a fresh keyed chain (asserted below).
    #[test]
    fn single_unkeyed_entry_keying_also_fails_loudly() {
        let dir = tempfile::tempdir().unwrap();
        let audit = dir.path().join("budget-audit.json");

        // One unkeyed entry (no prev_hash, no link, no self-MAC).
        append_budget_audit_entry_with_key(&audit, entry(100, 200, "slash"), None).unwrap();

        // Configuring a key and appending fails loudly: the lone legacy entry has
        // no keyed self-MAC, so it cannot be authenticated under the key.
        let key: &[u8] = b"now-keyed";
        let err = append_budget_audit_entry_with_key(&audit, entry(200, 300, "slash"), Some(key))
            .expect_err("keying over a 1-entry unkeyed log must fail loudly, not adopt it");
        assert!(
            matches!(
                err,
                AuditError::KeyOverUnkeyedLog {
                    unkeyed_entries: 1,
                    ..
                }
            ),
            "expected KeyOverUnkeyedLog over a 1-entry unkeyed log, got {err:?}"
        );

        // The on-disk log is untouched: still a valid UNKEYED single entry...
        assert!(matches!(
            AuditChainState::verify_with_key(&audit, None).unwrap(),
            ChainStatus::Intact { entries: 1, .. }
        ));
        // ...but it does NOT verify under the key — the lone entry has no self-MAC
        // to anchor its (tail) payload, so keyed verify is Broken at entry 0.
        assert!(matches!(
            AuditChainState::verify_with_key(&audit, Some(key)).unwrap(),
            ChainStatus::Broken { at_entry: 0, .. }
        ));

        // Only an EMPTY log can begin a fresh keyed chain: the first keyed append
        // succeeds, and the lone entry carries a self-MAC that verifies as the head.
        let fresh = dir.path().join("fresh-audit.json");
        append_budget_audit_entry_with_key(&fresh, entry(100, 200, "slash"), Some(key)).unwrap();
        match AuditChainState::verify_with_key(&fresh, Some(key)).unwrap() {
            ChainStatus::Intact { entries, last_hash } => {
                assert_eq!(entries, 1);
                let all = read_audit_log(&fresh).unwrap();
                // The lone keyed entry's self-MAC anchors its own payload and is the head.
                assert_eq!(all[0].entry_mac.as_deref(), Some(last_hash.as_str()));
            }
            other => panic!("expected intact keyed chain from an empty start, got {other:?}"),
        }
    }

    /// No silent downgrade: once a keyed link exists, a later link that only
    /// matches the unkeyed SHA (an attacker stripping the key from new entries)
    /// is rejected.
    #[test]
    fn keyed_chain_rejects_sha_link_after_first_keyed_link() {
        let dir = tempfile::tempdir().unwrap();
        let audit = dir.path().join("budget-audit.json");
        let key: &[u8] = b"operator-secret";

        append_budget_audit_entry_with_key(&audit, entry(100, 200, "slash"), Some(key)).unwrap();
        append_budget_audit_entry_with_key(&audit, entry(200, 300, "slash"), Some(key)).unwrap();
        append_budget_audit_entry_with_key(&audit, entry(300, 400, "slash"), Some(key)).unwrap();

        // Downgrade entry #2's link to the SHA of entry #1 (no key needed).
        let mut entries = read_audit_log(&audit).unwrap();
        entries[2].prev_hash = Some(hash_entry(&entries[1], None).unwrap());
        std::fs::write(&audit, serde_json::to_vec_pretty(&entries).unwrap()).unwrap();

        // Entry #1 is already a keyed link, so the SHA downgrade at #2 is broken.
        assert!(matches!(
            AuditChainState::verify_with_key(&audit, Some(key)).unwrap(),
            ChainStatus::Broken { at_entry: 2, .. }
        ));
    }

    /// The migration diagnosis must NOT swallow genuine corruption: a log that
    /// does not verify even UNKEYED (garbled prev_hash) keeps the plain
    /// `ChainBroken` error on the keyed load/append path — it is NOT relabeled as
    /// the benign-looking migration boundary. (Verify reports `Broken`, append
    /// errors `ChainBroken`, never `KeyOverUnkeyedLog`.)
    #[test]
    fn keyed_load_over_garbled_log_stays_chain_broken_not_migration() {
        let dir = tempfile::tempdir().unwrap();
        let audit = dir.path().join("budget-audit.json");
        let key: &[u8] = b"operator-secret";

        // Build a keyed chain, then garble entry #1's link to a value that is
        // neither the HMAC nor the SHA of entry #0 — genuine corruption.
        append_budget_audit_entry_with_key(&audit, entry(100, 200, "slash"), Some(key)).unwrap();
        append_budget_audit_entry_with_key(&audit, entry(200, 300, "slash"), Some(key)).unwrap();
        let mut entries = read_audit_log(&audit).unwrap();
        entries[1].prev_hash = Some("deadbeef".repeat(8));
        std::fs::write(&audit, serde_json::to_vec_pretty(&entries).unwrap()).unwrap();

        // Unkeyed verify ALSO fails (not a valid unkeyed chain), so the diagnosis
        // does not fire: the keyed load keeps the plain ChainBroken error.
        let err = AuditChainState::load_with_key(&audit, Some(key)).unwrap_err();
        assert!(
            matches!(err, AuditError::ChainBroken { at_entry: 1, .. }),
            "garbled log must stay ChainBroken, not be relabeled migration: {err:?}"
        );
        // And verify reports Broken (loud), never a softer status.
        assert!(matches!(
            AuditChainState::verify_with_key(&audit, Some(key)).unwrap(),
            ChainStatus::Broken { at_entry: 1, .. }
        ));
    }

    /// R1 regression: tail authentication. A backward `prev_hash` link
    /// authenticates an entry only via its SUCCESSOR, so without a tail self-MAC a
    /// key-less writer could edit the LAST (most recent, most security-relevant)
    /// budget change while leaving every `prev_hash` intact, and keyed verify would
    /// still report Intact. Editing only the tail payload (leaving its valid
    /// backward link and its now-stale `entry_mac` in place) must now be caught:
    /// the tail's stored self-MAC no longer equals the HMAC of its edited payload.
    #[test]
    fn keyed_chain_detects_tail_payload_edit() {
        let dir = tempfile::tempdir().unwrap();
        let audit = dir.path().join("budget-audit.json");
        let key: &[u8] = b"operator-secret";

        append_budget_audit_entry_with_key(&audit, entry(100, 200, "slash"), Some(key)).unwrap();
        append_budget_audit_entry_with_key(&audit, entry(200, 300, "slash"), Some(key)).unwrap();

        // Sanity: the intact keyed chain verifies before tampering.
        assert!(matches!(
            AuditChainState::verify_with_key(&audit, Some(key)).unwrap(),
            ChainStatus::Intact { entries: 2, .. }
        ));

        // Edit ONLY the tail entry's payload — bump its ceiling — and leave its
        // `prev_hash` (still the valid backward link to #0) and its now-stale
        // `entry_mac` exactly as written. A key-less attacker can do exactly this.
        let mut entries = read_audit_log(&audit).unwrap();
        entries[1].next.max_total_tokens = Some(999_999);
        // The backward link to #0 is untouched and still valid...
        assert_eq!(
            entries[1].prev_hash,
            Some(hash_entry(&entries[0], Some(key)).unwrap()),
            "the tail's backward link to entry #0 is left intact by this attack"
        );
        // ...and the stale self-MAC is left in place (the attacker cannot recompute
        // it without the key), so the only thing that changed is the payload.
        assert!(entries[1].entry_mac.is_some());
        std::fs::write(&audit, serde_json::to_vec_pretty(&entries).unwrap()).unwrap();

        // Tail authentication catches the edit: the tail's stored `entry_mac` no
        // longer equals the HMAC of its (edited) payload, so verify is Broken at #1.
        assert!(matches!(
            AuditChainState::verify_with_key(&audit, Some(key)).unwrap(),
            ChainStatus::Broken { at_entry: 1, .. }
        ));
    }
}
