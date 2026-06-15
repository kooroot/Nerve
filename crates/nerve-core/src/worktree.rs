//! Tier 2d worktree isolator (§3 Tier 2d sec-5).
//!
//! Provides the round-scoped `git worktree`-based isolation used by
//! `/apply --worktree`. The orchestrator integration ships in a later phase;
//! this module is responsible for the transactional guards 1..=7 enumerated
//! in the design doc:
//!
//! 1. main-pre.ref backup + `git bundle create` snapshot
//! 2. dirty main tree refusal
//! 3. advisory file lock serialising concurrent rounds
//! 4. doctor pre-check (`git --version`)
//! 5. symlink escape detection before merge
//! 6. disk-space gate before each round
//! 7. reset --hard → reset --merge fallback + orphaned manifest

use chrono::Utc;
use fs2::FileExt;
use nerve_patch::NvPatch;
use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use thiserror::Error;
use tokio::task;

const DEFAULT_STATVFS_THRESHOLD_MIB: u64 = 100;
const NERVE_DIR: &str = ".nerve";
const SCRATCH_SUBDIR: &str = ".nerve/scratch/worktrees";
const ORPHAN_SUBDIR: &str = ".nerve/scratch/orphaned-worktrees";
const RECOVERY_SUBDIR: &str = ".nerve/scratch/main-recovery";

#[derive(Debug, Error)]
pub enum WorktreeError {
    #[error("main worktree is dirty; commit or stash before /apply --worktree")]
    DirtyTree,
    #[error("`git` executable not found on PATH")]
    GitMissing,
    #[error(
        "insufficient disk space under .nerve/: {available_mib} MiB available, threshold {threshold_mib} MiB"
    )]
    DiskFull {
        available_mib: u64,
        threshold_mib: u64,
    },
    #[error("symlink or path escapes main repository: `{path}`")]
    SymlinkEscape { path: PathBuf },
    #[error("git merge failed: {stderr}")]
    MergeFailed { stderr: String },
    #[error("git reset failed even after --merge fallback: {stderr}")]
    ResetFailed { stderr: String },
    #[error("main HEAD moved during isolated round: expected `{expected}`, found `{actual}`")]
    MainMoved { expected: String, actual: String },
    #[error("failed to acquire worktree lock: {source}")]
    Lock {
        #[source]
        source: io::Error,
    },
    #[error("git bundle create failed: {stderr}")]
    Bundle { stderr: String },
    #[error("git command `{cmd}` failed: {stderr}")]
    Git { cmd: String, stderr: String },
    #[error(transparent)]
    Io(#[from] io::Error),
}

/// Round-scoped worktree handle returned by [`WorktreeIsolator::prepare_round`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct IsolatedRound {
    pub worktree_path: PathBuf,
    pub branch_name: String,
    /// Backup HEAD oid recorded before any mutation, used to rewind main on
    /// merge failure (sec-5 #1 / #7).
    pub main_pre_ref: String,
    /// `.bundle` archive of the recorded HEAD for sec-5 #7 fallback.
    pub bundle_path: PathBuf,
}

/// Append-only entry written to `orphaned-worktrees/manifest.jsonl` when a
/// worktree directory cannot be relocated out of the main repository (sec-5 #7).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct OrphanManifestEntry {
    pub ts: chrono::DateTime<Utc>,
    pub original_path: PathBuf,
    pub reason: String,
    pub bundle: Option<PathBuf>,
}

/// Tier 2d isolator. Owns the layout under `.nerve/scratch/worktrees/`.
#[derive(Debug, Clone)]
pub struct WorktreeIsolator {
    main_cwd: PathBuf,
    scratch_root: PathBuf,
    statvfs_threshold_mib: u64,
}

impl WorktreeIsolator {
    /// Construct a new isolator anchored at `main_cwd`.
    ///
    /// Verifies that `main_cwd` is a git repository and that the `git`
    /// executable is reachable on PATH (sec-5 #4 doctor pre-check).
    pub fn new(main_cwd: PathBuf) -> Result<Self, WorktreeError> {
        Self::with_threshold(main_cwd, DEFAULT_STATVFS_THRESHOLD_MIB)
    }

    pub fn with_threshold(
        main_cwd: PathBuf,
        statvfs_threshold_mib: u64,
    ) -> Result<Self, WorktreeError> {
        ensure_git_available()?;
        let scratch_root = main_cwd.join(SCRATCH_SUBDIR);
        fs::create_dir_all(&scratch_root)?;
        fs::create_dir_all(main_cwd.join(ORPHAN_SUBDIR))?;
        fs::create_dir_all(main_cwd.join(RECOVERY_SUBDIR))?;
        Ok(Self {
            main_cwd,
            scratch_root,
            statvfs_threshold_mib,
        })
    }

    pub fn main_cwd(&self) -> &Path {
        &self.main_cwd
    }

    pub fn scratch_root(&self) -> &Path {
        &self.scratch_root
    }

    pub fn statvfs_threshold_mib(&self) -> u64 {
        self.statvfs_threshold_mib
    }

    /// Prepare an isolated worktree for `round`. Runs guards #2/#3/#6/#1.
    pub fn prepare_round(&self, round: u8) -> Result<IsolatedRound, WorktreeError> {
        let _lock = WorktreeLock::acquire(&self.scratch_root)?;
        self.check_disk_space()?;
        self.ensure_clean_main()?;

        let ts = Utc::now().format("%Y%m%dT%H%M%S%f").to_string();
        let branch_name = format!("nerve/round-{round}-{ts}");
        let worktree_path = self.scratch_root.join(format!("round-{round}-{ts}"));
        let head_ref = self.head_oid()?;

        let bundle_path = self
            .main_cwd
            .join(RECOVERY_SUBDIR)
            .join(format!("round-{round}-{ts}.bundle"));
        self.write_bundle(&bundle_path, &head_ref)?;

        let pre_ref_path = worktree_path.join("main-pre.ref");
        // git worktree add creates `worktree_path` itself; place the ref *next* to it
        // until the directory exists, then move it inside on success.
        self.run_git(
            &self.main_cwd,
            &[
                "worktree",
                "add",
                "-b",
                &branch_name,
                worktree_path
                    .to_str()
                    .ok_or_else(|| invalid_path(&worktree_path))?,
                &head_ref,
            ],
        )?;

        write_atomic(&pre_ref_path, head_ref.as_bytes())?;

        Ok(IsolatedRound {
            worktree_path,
            branch_name,
            main_pre_ref: head_ref,
            bundle_path,
        })
    }

    /// Apply `patch` inside the isolated worktree. The patch is applied to the
    /// worktree directory rather than `main_cwd`. Returns the patch's apply
    /// report fields by way of `NvPatch::apply` errors (propagated as IO).
    pub async fn apply_patch_in_worktree(
        &self,
        round: &IsolatedRound,
        patch: &NvPatch,
    ) -> Result<(), WorktreeError> {
        let patch = patch.clone();
        let target = round.worktree_path.clone();
        task::spawn_blocking(move || -> Result<(), WorktreeError> {
            patch
                .apply(&target, false)
                .map_err(|err| io::Error::other(err.to_string()))?;
            Ok(())
        })
        .await
        .map_err(|err| io::Error::other(err.to_string()))??;
        Ok(())
    }

    /// Merge the round branch back into the main worktree. Runs guards
    /// #2 (dirty main), #5 (symlink escape), #6 (statvfs), then attempts a
    /// fast-forward merge. On failure, rewinds via guard #7.
    pub fn merge_round(&self, round: IsolatedRound) -> Result<(), WorktreeError> {
        let _lock = WorktreeLock::acquire(&self.scratch_root)?;
        self.check_disk_space()?;
        self.ensure_clean_main()?;
        self.ensure_main_head_unchanged(&round)?;

        // Guard #5: collect every file touched in the round branch and reject
        // anything that resolves to a symlink or escapes main_cwd.
        let changed = self.changed_paths(&round.main_pre_ref, &round.branch_name)?;
        for rel in &changed {
            self.verify_no_escape(&round.worktree_path, rel, &round)?;
        }

        // Attempt the merge by fast-forwarding to the round branch tip.
        let tip = self.rev_parse(&round.branch_name)?;
        let merge = self.run_git_capture(&self.main_cwd, &["merge", "--ff-only", &tip])?;
        if !merge.status.success() {
            let stderr = String::from_utf8_lossy(&merge.stderr).to_string();
            self.rewind_main(&round)?;
            return Err(WorktreeError::MergeFailed { stderr });
        }

        // Successful merge — clean up the worktree directory and branch.
        self.cleanup_worktree(&round, "merged");
        Ok(())
    }

    /// Discard a prepared round without merging. Refuses to touch main if HEAD
    /// moved after the round was prepared, then cleans up worktree storage.
    pub fn discard_round(&self, round: IsolatedRound) -> Result<(), WorktreeError> {
        let _lock = WorktreeLock::acquire(&self.scratch_root)?;
        self.ensure_main_head_unchanged(&round)?;
        self.cleanup_worktree(&round, "discarded");
        Ok(())
    }

    // ----- internal helpers -----

    fn ensure_clean_main(&self) -> Result<(), WorktreeError> {
        let out = self.run_git_capture(&self.main_cwd, &["status", "--porcelain"])?;
        if !out.status.success() {
            return Err(WorktreeError::Git {
                cmd: "git status".into(),
                stderr: String::from_utf8_lossy(&out.stderr).into(),
            });
        }
        // Treat anything inside `.nerve/` as nerve-managed scratch; those entries
        // are expected (the isolator creates them on construction) and must not
        // be counted as user-visible dirt.
        let stdout = String::from_utf8_lossy(&out.stdout);
        let dirty = stdout.lines().any(|line| {
            // Porcelain v1 format: two status chars + space + path. Anything
            // shorter is a defensive skip.
            let trimmed = line.get(3..).unwrap_or("");
            !trimmed.starts_with(".nerve/") && !trimmed.is_empty()
        });
        if dirty {
            return Err(WorktreeError::DirtyTree);
        }
        Ok(())
    }

    fn head_oid(&self) -> Result<String, WorktreeError> {
        self.rev_parse("HEAD")
    }

    fn rev_parse(&self, refname: &str) -> Result<String, WorktreeError> {
        let out = self.run_git_capture(&self.main_cwd, &["rev-parse", refname])?;
        if !out.status.success() {
            return Err(WorktreeError::Git {
                cmd: format!("git rev-parse {refname}"),
                stderr: String::from_utf8_lossy(&out.stderr).into(),
            });
        }
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    }

    fn ensure_main_head_unchanged(&self, round: &IsolatedRound) -> Result<(), WorktreeError> {
        let current = self.head_oid()?;
        if current != round.main_pre_ref {
            self.cleanup_worktree(round, "main-head-moved");
            return Err(WorktreeError::MainMoved {
                expected: round.main_pre_ref.clone(),
                actual: current,
            });
        }
        Ok(())
    }

    fn write_bundle(&self, bundle_path: &Path, _head_ref: &str) -> Result<(), WorktreeError> {
        // `git bundle create <file> HEAD` is the most portable form: it writes
        // the current commit reachable from HEAD without requiring a named
        // branch ref. We fall back to `--all` if HEAD is detached and HEAD
        // alone produces an empty bundle.
        let out = self.run_git_capture(
            &self.main_cwd,
            &[
                "bundle",
                "create",
                bundle_path
                    .to_str()
                    .ok_or_else(|| invalid_path(bundle_path))?,
                "HEAD",
            ],
        )?;
        if out.status.success() {
            return Ok(());
        }
        let first_err = String::from_utf8_lossy(&out.stderr).to_string();
        // Retry with --all (covers initial-commit/empty-bundle edge cases).
        let retry = self.run_git_capture(
            &self.main_cwd,
            &[
                "bundle",
                "create",
                bundle_path
                    .to_str()
                    .ok_or_else(|| invalid_path(bundle_path))?,
                "--all",
            ],
        )?;
        if retry.status.success() {
            return Ok(());
        }
        let second_err = String::from_utf8_lossy(&retry.stderr).to_string();
        Err(WorktreeError::Bundle {
            stderr: format!("HEAD: {} | --all: {}", first_err.trim(), second_err.trim()),
        })
    }

    fn changed_paths(&self, base: &str, branch: &str) -> Result<Vec<PathBuf>, WorktreeError> {
        let out = self.run_git_capture(&self.main_cwd, &["diff", "--name-only", base, branch])?;
        if !out.status.success() {
            return Err(WorktreeError::Git {
                cmd: format!("git diff {base}..{branch}"),
                stderr: String::from_utf8_lossy(&out.stderr).into(),
            });
        }
        let mut paths = Vec::new();
        for line in String::from_utf8_lossy(&out.stdout).lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            paths.push(PathBuf::from(trimmed));
        }
        Ok(paths)
    }

    fn verify_no_escape(
        &self,
        worktree_path: &Path,
        rel: &Path,
        round: &IsolatedRound,
    ) -> Result<(), WorktreeError> {
        // Reject absolute paths or paths containing parent components.
        if rel.is_absolute() || rel.components().any(|c| matches!(c, Component::ParentDir)) {
            self.quarantine(round, "path-escape");
            return Err(WorktreeError::SymlinkEscape {
                path: rel.to_path_buf(),
            });
        }

        let candidate = worktree_path.join(rel);
        match fs::symlink_metadata(&candidate) {
            Ok(meta) => {
                if meta.file_type().is_symlink() {
                    self.quarantine(round, "symlink");
                    return Err(WorktreeError::SymlinkEscape { path: candidate });
                }
                // For tracked files that still exist, ensure canonical path stays
                // inside main_cwd. Deleted files (errors above) skip this check.
                if let Ok(canonical) = fs::canonicalize(&candidate) {
                    let main_canonical =
                        fs::canonicalize(&self.main_cwd).unwrap_or_else(|_| self.main_cwd.clone());
                    if !canonical.starts_with(&main_canonical) {
                        self.quarantine(round, "escape");
                        return Err(WorktreeError::SymlinkEscape { path: canonical });
                    }
                }
            }
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                // Deleted file: nothing to validate at the filesystem level.
            }
            Err(err) => return Err(err.into()),
        }
        Ok(())
    }

    fn rewind_main(&self, round: &IsolatedRound) -> Result<(), WorktreeError> {
        let hard =
            self.run_git_capture(&self.main_cwd, &["reset", "--hard", &round.main_pre_ref])?;
        if hard.status.success() {
            return Ok(());
        }
        let hard_err = String::from_utf8_lossy(&hard.stderr).to_string();

        // Fallback to `reset --merge` (sec-5 #7).
        let merge =
            self.run_git_capture(&self.main_cwd, &["reset", "--merge", &round.main_pre_ref])?;
        if merge.status.success() {
            return Ok(());
        }
        let merge_err = String::from_utf8_lossy(&merge.stderr).to_string();
        Err(WorktreeError::ResetFailed {
            stderr: format!(
                "hard: {} | merge: {} | bundle backup at {}",
                hard_err.trim(),
                merge_err.trim(),
                round.bundle_path.display()
            ),
        })
    }

    fn cleanup_worktree(&self, round: &IsolatedRound, reason: &str) {
        // Best-effort `git worktree remove`. Falls through to `--force` then to
        // quarantine if neither succeeds.
        let path_str = match round.worktree_path.to_str() {
            Some(p) => p.to_string(),
            None => {
                self.quarantine(round, "non-utf8-path");
                return;
            }
        };
        let first = self.run_git_capture(&self.main_cwd, &["worktree", "remove", &path_str]);
        if matches!(&first, Ok(out) if out.status.success()) {
            let _ = self.delete_branch(&round.branch_name);
            tracing::debug!(reason, branch = round.branch_name, "worktree removed");
            return;
        }
        let forced = self.run_git_capture(
            &self.main_cwd,
            &["worktree", "remove", "--force", &path_str],
        );
        if matches!(&forced, Ok(out) if out.status.success()) {
            let _ = self.delete_branch(&round.branch_name);
            tracing::debug!(reason, branch = round.branch_name, "worktree force-removed");
            return;
        }
        tracing::warn!(
            reason,
            branch = round.branch_name,
            "worktree cleanup failed, quarantining"
        );
        self.quarantine(round, reason);
    }

    fn delete_branch(&self, branch: &str) -> Result<(), WorktreeError> {
        let out = self.run_git_capture(&self.main_cwd, &["branch", "-D", branch])?;
        if !out.status.success() {
            return Err(WorktreeError::Git {
                cmd: format!("git branch -D {branch}"),
                stderr: String::from_utf8_lossy(&out.stderr).into(),
            });
        }
        Ok(())
    }

    fn quarantine(&self, round: &IsolatedRound, reason: &str) {
        let orphan_root = self.main_cwd.join(ORPHAN_SUBDIR);
        if let Err(err) = fs::create_dir_all(&orphan_root) {
            tracing::error!(error=%err, "failed to ensure orphan dir");
        }
        let dest = orphan_root.join(
            round
                .worktree_path
                .file_name()
                .map(|s| s.to_owned())
                .unwrap_or_else(|| std::ffi::OsString::from("unknown")),
        );

        let mv_ok = fs::rename(&round.worktree_path, &dest).is_ok();
        let final_path = if mv_ok {
            dest
        } else {
            round.worktree_path.clone()
        };

        if !mv_ok {
            // sec-5 #7: chmod 0600 in-place and record location.
            if let Err(err) = chmod_recursive(&final_path, 0o600) {
                tracing::error!(error=%err, "in-place chmod failed");
            }
        } else {
            // Tighten perms on the quarantined dir (auth caches).
            let _ = chmod_recursive(&final_path, 0o600);
        }

        let manifest = orphan_root.join("manifest.jsonl");
        let entry = OrphanManifestEntry {
            ts: Utc::now(),
            original_path: final_path,
            reason: reason.to_string(),
            bundle: Some(round.bundle_path.clone()),
        };
        if let Err(err) = append_manifest(&manifest, &entry) {
            tracing::error!(error=%err, "failed to append orphan manifest");
        }
    }

    fn check_disk_space(&self) -> Result<(), WorktreeError> {
        let nerve_root = self.main_cwd.join(NERVE_DIR);
        let probe = if nerve_root.exists() {
            nerve_root
        } else {
            self.main_cwd.clone()
        };
        let available = available_mib(&probe)?;
        if available < self.statvfs_threshold_mib {
            return Err(WorktreeError::DiskFull {
                available_mib: available,
                threshold_mib: self.statvfs_threshold_mib,
            });
        }
        Ok(())
    }

    fn run_git(&self, dir: &Path, args: &[&str]) -> Result<(), WorktreeError> {
        let out = self.run_git_capture(dir, args)?;
        if !out.status.success() {
            return Err(WorktreeError::Git {
                cmd: format!("git {}", args.join(" ")),
                stderr: String::from_utf8_lossy(&out.stderr).into(),
            });
        }
        Ok(())
    }

    fn run_git_capture(
        &self,
        dir: &Path,
        args: &[&str],
    ) -> Result<std::process::Output, WorktreeError> {
        Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .map_err(WorktreeError::Io)
    }
}

fn ensure_git_available() -> Result<(), WorktreeError> {
    Command::new("git")
        .arg("--version")
        .output()
        .map_err(|_| WorktreeError::GitMissing)?;
    Ok(())
}

fn invalid_path(path: &Path) -> WorktreeError {
    WorktreeError::Io(io::Error::new(
        io::ErrorKind::InvalidInput,
        format!("non-UTF8 path: {}", path.display()),
    ))
}

fn write_atomic(target: &Path, bytes: &[u8]) -> Result<(), WorktreeError> {
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut tmp = tempfile::NamedTempFile::new_in(target.parent().unwrap_or(Path::new(".")))?;
    tmp.write_all(bytes)?;
    tmp.persist(target)
        .map_err(|err| WorktreeError::Io(err.error))?;
    Ok(())
}

fn append_manifest(path: &Path, entry: &OrphanManifestEntry) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    let line = serde_json::to_string(entry).map_err(io::Error::other)?;
    file.write_all(line.as_bytes())?;
    file.write_all(b"\n")?;
    Ok(())
}

#[cfg(unix)]
fn chmod_recursive(root: &Path, mode: u32) -> io::Result<()> {
    if !root.exists() {
        return Ok(());
    }
    let meta = fs::symlink_metadata(root)?;
    if meta.file_type().is_symlink() {
        return Ok(());
    }
    if meta.file_type().is_dir() {
        for entry in fs::read_dir(root)? {
            let entry = entry?;
            chmod_recursive(&entry.path(), mode)?;
        }
        fs::set_permissions(root, fs::Permissions::from_mode(mode | 0o100))?;
    } else {
        fs::set_permissions(root, fs::Permissions::from_mode(mode))?;
    }
    Ok(())
}

#[cfg(not(unix))]
fn chmod_recursive(_root: &Path, _mode: u32) -> io::Result<()> {
    Ok(())
}

/// Doctor-facing accessor for the MiB disk-space probe used by
/// `prepare_round`/`merge_round` (Tier 2d guard #6). Wraps the private helper
/// so `doctor_checks` doesn't need to construct an isolator just to read
/// available disk space.
pub fn available_mib_for_doctor(probe: &Path) -> Result<u64, WorktreeError> {
    available_mib(probe)
}

fn available_mib(probe: &Path) -> Result<u64, WorktreeError> {
    let available = fs2::available_space(probe).map_err(WorktreeError::Io)?;
    Ok(available / (1024 * 1024))
}

struct WorktreeLock {
    file: File,
}

impl WorktreeLock {
    fn acquire(scratch_root: &Path) -> Result<Self, WorktreeError> {
        fs::create_dir_all(scratch_root)?;
        let path = scratch_root.join(".lock");
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&path)
            .map_err(|source| WorktreeError::Lock { source })?;
        FileExt::lock_exclusive(&file).map_err(|source| WorktreeError::Lock { source })?;
        Ok(Self { file })
    }
}

impl Drop for WorktreeLock {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.file);
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use nerve_patch::{FilePatch, NvPatch};
    use std::os::unix::fs::symlink;

    fn run(dir: &Path, args: &[&str]) {
        let out = Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .expect("git invocation");
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn init_main_repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        run(dir.path(), &["init", "-q", "--initial-branch=main"]);
        run(dir.path(), &["config", "user.email", "nerve@example.com"]);
        run(dir.path(), &["config", "user.name", "Nerve Tester"]);
        fs::write(dir.path().join("seed.txt"), "seed\n").unwrap();
        run(dir.path(), &["add", "seed.txt"]);
        run(dir.path(), &["commit", "-q", "-m", "seed"]);
        dir
    }

    #[test]
    fn prepare_round_creates_isolated_worktree() {
        let dir = init_main_repo();
        let isolator = WorktreeIsolator::with_threshold(dir.path().to_path_buf(), 0).unwrap();
        let round = isolator.prepare_round(1).unwrap();

        assert!(round.worktree_path.exists());
        assert!(round.worktree_path.starts_with(dir.path()));
        assert!(round.bundle_path.exists());
        assert!(round.branch_name.starts_with("nerve/round-1-"));
        assert!(round.worktree_path.join("main-pre.ref").exists());

        // worktree should have its own checkout of seed.txt.
        assert!(round.worktree_path.join("seed.txt").exists());

        // cleanup
        isolator.discard_round(round).unwrap();
    }

    #[test]
    fn dirty_tree_rejected() {
        let dir = init_main_repo();
        // Stage an uncommitted file in main.
        fs::write(dir.path().join("dirty.txt"), "dirty\n").unwrap();
        run(dir.path(), &["add", "dirty.txt"]);

        let isolator = WorktreeIsolator::with_threshold(dir.path().to_path_buf(), 0).unwrap();
        match isolator.prepare_round(2) {
            Err(WorktreeError::DirtyTree) => {}
            other => panic!("expected DirtyTree, got {other:?}"),
        }
    }

    #[test]
    fn symlink_escape_rejected() {
        let dir = init_main_repo();
        let isolator = WorktreeIsolator::with_threshold(dir.path().to_path_buf(), 0).unwrap();
        let round = isolator.prepare_round(3).unwrap();

        // Commit a symlink inside the worktree branch.
        let link_path = round.worktree_path.join("escape");
        symlink("/etc/passwd", &link_path).unwrap();
        run(&round.worktree_path, &["add", "escape"]);
        run(
            &round.worktree_path,
            &["commit", "-q", "-m", "add escape symlink"],
        );

        let merged = isolator.merge_round(round.clone());
        match merged {
            Err(WorktreeError::SymlinkEscape { .. }) => {}
            other => panic!("expected SymlinkEscape, got {other:?}"),
        }

        // Quarantine should have produced a manifest entry.
        let manifest = dir.path().join(ORPHAN_SUBDIR).join("manifest.jsonl");
        assert!(manifest.exists(), "orphan manifest should exist");
        let body = fs::read_to_string(&manifest).unwrap();
        assert!(body.contains("symlink") || body.contains("escape"));
    }

    #[test]
    fn merge_refuses_if_main_head_moved() {
        let dir = init_main_repo();
        let isolator = WorktreeIsolator::with_threshold(dir.path().to_path_buf(), 0).unwrap();
        let round = isolator.prepare_round(6).unwrap();
        let original_head = round.main_pre_ref.clone();

        fs::write(dir.path().join("main-change.txt"), "main moved\n").unwrap();
        run(dir.path(), &["add", "main-change.txt"]);
        run(dir.path(), &["commit", "-q", "-m", "main moved"]);
        let moved_head = isolator.head_oid().unwrap();
        assert_ne!(moved_head, original_head);

        fs::write(round.worktree_path.join("seed.txt"), "seed from worktree\n").unwrap();
        run(&round.worktree_path, &["add", "seed.txt"]);
        run(
            &round.worktree_path,
            &["commit", "-q", "-m", "worktree edit"],
        );

        let err = isolator.merge_round(round).unwrap_err();
        match err {
            WorktreeError::MainMoved { expected, actual } => {
                assert_eq!(expected, original_head);
                assert_eq!(actual, moved_head);
            }
            other => panic!("expected MainMoved, got {other:?}"),
        }
        assert_eq!(isolator.head_oid().unwrap(), moved_head);
        assert!(dir.path().join("main-change.txt").exists());
    }

    #[test]
    fn chmod_recursive_skips_symlink_targets() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        let target = dir.path().join("outside.txt");
        fs::create_dir(&root).unwrap();
        fs::write(&target, "outside\n").unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o644)).unwrap();
        symlink(&target, root.join("link")).unwrap();

        chmod_recursive(&root, 0o600).unwrap();

        let mode = fs::metadata(&target).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o644);
    }

    #[test]
    fn reset_fallback_invoked_when_hard_reset_fails() {
        // Direct unit test for rewind_main: build a synthetic round whose
        // main_pre_ref is a non-existent oid so `git reset --hard` fails.
        let dir = init_main_repo();
        let isolator = WorktreeIsolator::with_threshold(dir.path().to_path_buf(), 0).unwrap();
        let bogus = IsolatedRound {
            worktree_path: dir.path().join("does-not-exist"),
            branch_name: "nerve/round-bogus".into(),
            main_pre_ref: "deadbeefcafebabe0000000000000000deadbeef".into(),
            bundle_path: dir.path().join("missing.bundle"),
        };
        let err = isolator.rewind_main(&bogus).unwrap_err();
        match err {
            WorktreeError::ResetFailed { stderr } => {
                assert!(stderr.contains("hard:"));
                assert!(stderr.contains("merge:"));
            }
            other => panic!("expected ResetFailed, got {other:?}"),
        }
    }

    #[test]
    fn disk_full_simulated() {
        let dir = init_main_repo();
        // Set threshold absurdly high so guard #6 fires.
        let isolator =
            WorktreeIsolator::with_threshold(dir.path().to_path_buf(), u64::MAX / 2).unwrap();
        match isolator.prepare_round(4) {
            Err(WorktreeError::DiskFull { .. }) => {}
            other => panic!("expected DiskFull, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn apply_patch_in_worktree_modifies_worktree_only() {
        let dir = init_main_repo();
        let isolator = WorktreeIsolator::with_threshold(dir.path().to_path_buf(), 0).unwrap();
        let round = isolator.prepare_round(5).unwrap();

        let patch = NvPatch::new(vec![FilePatch::new("seed.txt", "seed\n", "seed-edited\n")]);
        isolator
            .apply_patch_in_worktree(&round, &patch)
            .await
            .unwrap();

        let edited = fs::read_to_string(round.worktree_path.join("seed.txt")).unwrap();
        assert_eq!(edited, "seed-edited\n");

        // main is untouched.
        let main_seed = fs::read_to_string(dir.path().join("seed.txt")).unwrap();
        assert_eq!(main_seed, "seed\n");

        isolator.discard_round(round).unwrap();
    }
}
