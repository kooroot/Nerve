use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use similar::TextDiff;
use std::fmt::Write as _;
use std::fs;
use std::path::{Component, Path, PathBuf};
use thiserror::Error;
use uuid::Uuid;

#[derive(Debug, Error)]
pub enum PatchError {
    #[error("patch target `{path}` changed: expected {expected}, got {actual}")]
    HashMismatch {
        path: PathBuf,
        expected: String,
        actual: String,
    },
    #[error("io error for `{path}`: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("invalid unified diff: {message}")]
    InvalidUnifiedDiff { message: String },
    #[error("unsafe patch path `{path}`: {reason}")]
    UnsafePath { path: PathBuf, reason: String },
    #[error("forbidden patch path `{path}`: writes into `{prefix}` are not allowed")]
    ForbiddenPath { path: PathBuf, prefix: String },
    #[error("unsupported patch operation: {message}")]
    Unsupported { message: String },
    #[error("invalid patch state for `{path}`: {message}")]
    InvalidOperationState { path: PathBuf, message: String },
    #[error(
        "patch apply failed and automatic rollback failed: apply error: {apply_error}; rollback error: {rollback_error}"
    )]
    RollbackFailed {
        apply_error: String,
        rollback_error: String,
    },
}

pub type Result<T> = std::result::Result<T, PatchError>;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct NvPatch {
    pub id: String,
    pub base_commit: Option<String>,
    pub files: Vec<FilePatch>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FilePatch {
    pub path: PathBuf,
    #[serde(default = "default_file_operation")]
    pub operation: FileOperation,
    pub original: String,
    pub modified: String,
    pub original_sha256: String,
    pub modified_sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FileOperation {
    Modify,
    Create,
    Delete,
    Rename { from: PathBuf },
}

fn default_file_operation() -> FileOperation {
    FileOperation::Modify
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ApplyReport {
    pub patch_id: String,
    pub changed_files: Vec<PathBuf>,
    pub dry_run: bool,
}

impl NvPatch {
    pub fn new(files: Vec<FilePatch>) -> Self {
        Self {
            id: Uuid::new_v4().to_string(),
            base_commit: None,
            files,
        }
    }

    pub fn single(
        path: impl Into<PathBuf>,
        original: impl Into<String>,
        modified: impl Into<String>,
    ) -> Self {
        Self::new(vec![FilePatch::new(path, original, modified)])
    }

    pub fn is_empty(&self) -> bool {
        self.files.iter().all(FilePatch::is_noop)
    }

    pub fn to_unified_diff(&self) -> String {
        let mut out = String::new();
        for file in &self.files {
            if file.is_noop() {
                continue;
            }
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(&file.to_unified_diff());
        }
        out
    }

    pub fn from_unified_diff(cwd: &Path, diff: &str) -> Result<Option<Self>> {
        let file_diffs = parse_file_diffs(diff)?;
        if file_diffs.is_empty() {
            return Ok(None);
        }

        let mut files = Vec::with_capacity(file_diffs.len());
        for file_diff in file_diffs {
            files.push(file_diff.to_file_patch(cwd)?);
        }

        Ok(Some(Self::new(files)))
    }

    pub fn validate(&self, cwd: &Path, rollback: bool) -> Result<()> {
        for file in &self.files {
            file.validate(cwd, rollback)?;
        }
        Ok(())
    }

    pub fn apply(&self, cwd: &Path, dry_run: bool) -> Result<ApplyReport> {
        self.validate(cwd, false)?;
        let changed_files = self.changed_files();
        if !dry_run {
            let snapshots = self.capture_snapshots(cwd)?;
            for file in &self.files {
                if let Err(apply_error) = file.apply(cwd) {
                    if let Err(rollback_error) = restore_snapshots(cwd, &snapshots) {
                        return Err(PatchError::RollbackFailed {
                            apply_error: apply_error.to_string(),
                            rollback_error: rollback_error.to_string(),
                        });
                    }
                    return Err(apply_error);
                }
            }
        }
        Ok(ApplyReport {
            patch_id: self.id.clone(),
            changed_files,
            dry_run,
        })
    }

    pub fn rollback(&self, cwd: &Path, dry_run: bool) -> Result<ApplyReport> {
        self.validate(cwd, true)?;
        if !dry_run {
            for file in &self.files {
                file.rollback(cwd)?;
            }
        }
        Ok(ApplyReport {
            patch_id: self.id.clone(),
            changed_files: self.changed_files(),
            dry_run,
        })
    }

    /// Deterministic SHA-256 fingerprint of the patch payload.
    ///
    /// Sorts files by path, normalises CRLF→LF, and excludes metadata
    /// (id, base_commit, timestamps) so equivalent patches hash equal
    /// regardless of construction order or line-ending quirks.
    pub fn canonical_hash(&self) -> String {
        let mut entries: Vec<&FilePatch> = self.files.iter().collect();
        entries.sort_by(|a, b| a.path.cmp(&b.path));

        let mut hasher = Sha256::new();
        for file in entries {
            let path = file.path.to_string_lossy();
            hasher.update(path.as_bytes());
            hasher.update(b"\0");
            match &file.operation {
                FileOperation::Modify => hasher.update(b"modify\0"),
                FileOperation::Create => hasher.update(b"create\0"),
                FileOperation::Delete => hasher.update(b"delete\0"),
                FileOperation::Rename { from } => {
                    hasher.update(b"rename\0");
                    hasher.update(from.to_string_lossy().as_bytes());
                    hasher.update(b"\0");
                }
            }
            let original = file.original.replace("\r\n", "\n");
            let modified = file.modified.replace("\r\n", "\n");
            hasher.update(original.as_bytes());
            hasher.update(b"\0");
            hasher.update(modified.as_bytes());
            hasher.update(b"\0");
        }

        let digest = hasher.finalize();
        let mut out = String::with_capacity(digest.len() * 2);
        for byte in digest {
            let _ = write!(&mut out, "{byte:02x}");
        }
        out
    }

    fn changed_files(&self) -> Vec<PathBuf> {
        self.files
            .iter()
            .flat_map(FilePatch::changed_paths)
            .collect()
    }

    fn capture_snapshots(&self, cwd: &Path) -> Result<Vec<FileSnapshot>> {
        self.changed_files()
            .into_iter()
            .map(|path| FileSnapshot::capture(cwd, path))
            .collect()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FileSnapshot {
    path: PathBuf,
    state: FileState,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum FileState {
    Present(String),
    Missing,
}

impl FileSnapshot {
    fn capture(cwd: &Path, path: PathBuf) -> Result<Self> {
        ensure_safe_relative_path(cwd, &path)?;
        let state = if target_exists(cwd, &path) {
            FileState::Present(read_to_string(cwd, &path)?)
        } else {
            FileState::Missing
        };
        Ok(Self { path, state })
    }

    fn restore(&self, cwd: &Path) -> Result<()> {
        match &self.state {
            FileState::Present(value) => write_string(cwd, &self.path, value),
            FileState::Missing => remove_file(cwd, &self.path),
        }
    }
}

fn restore_snapshots(cwd: &Path, snapshots: &[FileSnapshot]) -> Result<()> {
    for snapshot in snapshots.iter().rev() {
        snapshot.restore(cwd)?;
    }
    Ok(())
}

impl FilePatch {
    pub fn new(
        path: impl Into<PathBuf>,
        original: impl Into<String>,
        modified: impl Into<String>,
    ) -> Self {
        Self::with_operation(path, FileOperation::Modify, original, modified)
    }

    pub fn create(path: impl Into<PathBuf>, modified: impl Into<String>) -> Self {
        Self::with_operation(path, FileOperation::Create, "", modified)
    }

    pub fn delete(path: impl Into<PathBuf>, original: impl Into<String>) -> Self {
        Self::with_operation(path, FileOperation::Delete, original, "")
    }

    pub fn rename(
        from: impl Into<PathBuf>,
        to: impl Into<PathBuf>,
        original: impl Into<String>,
        modified: impl Into<String>,
    ) -> Self {
        Self::with_operation(
            to,
            FileOperation::Rename { from: from.into() },
            original,
            modified,
        )
    }

    pub fn with_operation(
        path: impl Into<PathBuf>,
        operation: FileOperation,
        original: impl Into<String>,
        modified: impl Into<String>,
    ) -> Self {
        let original = original.into();
        let modified = modified.into();
        Self {
            path: path.into(),
            operation,
            original_sha256: sha256_hex(&original),
            modified_sha256: sha256_hex(&modified),
            original,
            modified,
        }
    }

    pub fn to_unified_diff(&self) -> String {
        if let FileOperation::Rename { from } = &self.operation
            && self.original == self.modified
        {
            return format!(
                "diff --git a/{} b/{}\nsimilarity index 100%\nrename from {}\nrename to {}\n",
                from.display(),
                self.path.display(),
                from.display(),
                self.path.display()
            );
        }

        let old_path = match &self.operation {
            FileOperation::Create => "/dev/null".to_string(),
            FileOperation::Rename { from } => format!("a/{}", from.display()),
            FileOperation::Modify | FileOperation::Delete => format!("a/{}", self.path.display()),
        };
        let new_path = match &self.operation {
            FileOperation::Delete => "/dev/null".to_string(),
            FileOperation::Modify | FileOperation::Create | FileOperation::Rename { .. } => {
                format!("b/{}", self.path.display())
            }
        };
        TextDiff::from_lines(&self.original, &self.modified)
            .unified_diff()
            .header(&old_path, &new_path)
            .to_string()
    }

    fn validate(&self, cwd: &Path, rollback: bool) -> Result<()> {
        ensure_safe_relative_path(cwd, &self.path)?;
        if let FileOperation::Rename { from } = &self.operation {
            ensure_safe_relative_path(cwd, from)?;
        }
        self.validate_operation_state(cwd, rollback)?;
        let expected = if rollback {
            &self.modified_sha256
        } else {
            &self.original_sha256
        };
        let validation_path = self.validation_path(rollback);
        let current = read_to_string(cwd, validation_path)?;
        let actual = sha256_hex(&current);
        if &actual != expected {
            return Err(PatchError::HashMismatch {
                path: validation_path.to_path_buf(),
                expected: expected.clone(),
                actual,
            });
        }
        Ok(())
    }

    fn validate_operation_state(&self, cwd: &Path, rollback: bool) -> Result<()> {
        match (&self.operation, rollback) {
            (FileOperation::Create, false) if target_exists(cwd, &self.path) => {
                Err(PatchError::InvalidOperationState {
                    path: self.path.clone(),
                    message: "create target already exists".to_string(),
                })
            }
            (FileOperation::Delete, false) if !target_exists(cwd, &self.path) => {
                Err(PatchError::InvalidOperationState {
                    path: self.path.clone(),
                    message: "delete target does not exist".to_string(),
                })
            }
            (FileOperation::Rename { from }, false) if !target_exists(cwd, from) => {
                Err(PatchError::InvalidOperationState {
                    path: from.clone(),
                    message: "rename source does not exist".to_string(),
                })
            }
            (FileOperation::Rename { .. }, false) if target_exists(cwd, &self.path) => {
                Err(PatchError::InvalidOperationState {
                    path: self.path.clone(),
                    message: "rename target already exists".to_string(),
                })
            }
            (FileOperation::Rename { from }, true) if target_exists(cwd, from) => {
                Err(PatchError::InvalidOperationState {
                    path: from.clone(),
                    message: "rename rollback source already exists".to_string(),
                })
            }
            _ => Ok(()),
        }
    }

    fn apply(&self, cwd: &Path) -> Result<()> {
        match &self.operation {
            FileOperation::Delete => remove_file(cwd, &self.path),
            FileOperation::Rename { from } => {
                write_string(cwd, &self.path, &self.modified)?;
                remove_file(cwd, from)
            }
            FileOperation::Modify | FileOperation::Create => {
                write_string(cwd, &self.path, &self.modified)
            }
        }
    }

    fn rollback(&self, cwd: &Path) -> Result<()> {
        match &self.operation {
            FileOperation::Create => remove_file(cwd, &self.path),
            FileOperation::Rename { from } => {
                write_string(cwd, from, &self.original)?;
                remove_file(cwd, &self.path)
            }
            FileOperation::Modify | FileOperation::Delete => {
                write_string(cwd, &self.path, &self.original)
            }
        }
    }

    fn validation_path(&self, rollback: bool) -> &Path {
        match (&self.operation, rollback) {
            (FileOperation::Rename { from }, false) => from,
            _ => &self.path,
        }
    }

    fn changed_paths(&self) -> Vec<PathBuf> {
        match &self.operation {
            FileOperation::Rename { from } => vec![from.clone(), self.path.clone()],
            FileOperation::Modify | FileOperation::Create | FileOperation::Delete => {
                vec![self.path.clone()]
            }
        }
    }

    /// Whether this file patch changes nothing: identical content and not a
    /// rename. Used to skip empty entries when summarizing a patch (e.g. the S12
    /// auto-mode classifier counts only files that effect a real change).
    pub fn is_noop(&self) -> bool {
        self.original == self.modified && !matches!(self.operation, FileOperation::Rename { .. })
    }
}

fn read_to_string(cwd: &Path, relative: &Path) -> Result<String> {
    let path = cwd.join(relative);
    match fs::read_to_string(&path) {
        Ok(value) => Ok(value),
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(source) => Err(PatchError::Io {
            path: relative.to_path_buf(),
            source,
        }),
    }
}

fn write_string(cwd: &Path, relative: &Path, value: &str) -> Result<()> {
    let path = cwd.join(relative);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|source| PatchError::Io {
            path: parent.to_path_buf(),
            source,
        })?;
    }

    let tmp_path = temp_write_path(&path);
    fs::write(&tmp_path, value).map_err(|source| PatchError::Io {
        path: tmp_path.clone(),
        source,
    })?;

    if let Err(source) = fs::rename(&tmp_path, &path) {
        let _ = fs::remove_file(&tmp_path);
        return Err(PatchError::Io {
            path: relative.to_path_buf(),
            source,
        });
    }

    Ok(())
}

fn temp_write_path(path: &Path) -> PathBuf {
    let parent = path.parent().unwrap_or_else(|| Path::new(""));
    let file_name = path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("target");
    parent.join(format!(".{file_name}.nerve-write-{}.tmp", Uuid::new_v4()))
}

fn remove_file(cwd: &Path, relative: &Path) -> Result<()> {
    let path = cwd.join(relative);
    match fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(source)
            if matches!(
                source.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
            ) =>
        {
            Ok(())
        }
        Err(source) => Err(PatchError::Io {
            path: relative.to_path_buf(),
            source,
        }),
    }
}

fn target_exists(cwd: &Path, relative: &Path) -> bool {
    cwd.join(relative).exists()
}

#[derive(Debug, Clone)]
struct ParsedFileDiff {
    old_path: Option<PathBuf>,
    new_path: Option<PathBuf>,
    rename_from: Option<PathBuf>,
    rename_to: Option<PathBuf>,
    hunks: Vec<ParsedHunk>,
}

impl ParsedFileDiff {
    fn to_file_patch(&self, cwd: &Path) -> Result<FilePatch> {
        let (path, operation) = match (&self.old_path, &self.new_path) {
            _ if self.rename_from.is_some() || self.rename_to.is_some() => {
                let from =
                    self.rename_from
                        .clone()
                        .ok_or_else(|| PatchError::InvalidUnifiedDiff {
                            message: "rename diff is missing `rename from`".to_string(),
                        })?;
                let to = self
                    .rename_to
                    .clone()
                    .ok_or_else(|| PatchError::InvalidUnifiedDiff {
                        message: "rename diff is missing `rename to`".to_string(),
                    })?;
                (to, FileOperation::Rename { from })
            }
            (None, Some(new_path)) => (new_path.clone(), FileOperation::Create),
            (Some(old_path), None) => (old_path.clone(), FileOperation::Delete),
            (Some(old_path), Some(new_path)) if old_path == new_path => {
                (new_path.clone(), FileOperation::Modify)
            }
            (Some(old_path), Some(new_path)) => (
                new_path.clone(),
                FileOperation::Rename {
                    from: old_path.clone(),
                },
            ),
            (None, None) => {
                return Err(PatchError::InvalidUnifiedDiff {
                    message: "file diff has neither old nor new path".to_string(),
                });
            }
        };

        ensure_safe_relative_path(cwd, &path)?;
        if let FileOperation::Rename { from } = &operation {
            ensure_safe_relative_path(cwd, from)?;
        }
        let original = match &operation {
            FileOperation::Create => String::new(),
            FileOperation::Rename { from } => read_to_string(cwd, from)?,
            FileOperation::Modify | FileOperation::Delete => read_to_string(cwd, &path)?,
        };
        let modified = if self.hunks.is_empty() {
            match &operation {
                FileOperation::Rename { .. } => original.clone(),
                _ => {
                    return Err(PatchError::InvalidUnifiedDiff {
                        message: "file diff has no hunks".to_string(),
                    });
                }
            }
        } else {
            apply_hunks(&original, &self.hunks)?
        };

        Ok(FilePatch::with_operation(
            path, operation, original, modified,
        ))
    }
}

#[derive(Debug, Clone)]
struct ParsedHunk {
    old_start: usize,
    old_count: usize,
    new_count: usize,
    lines: Vec<HunkLine>,
}

#[derive(Debug, Clone)]
enum HunkLine {
    Context(String),
    Add(String),
    Remove(String),
}

fn parse_file_diffs(diff: &str) -> Result<Vec<ParsedFileDiff>> {
    let lines = diff.split_inclusive('\n').collect::<Vec<_>>();
    let mut files = Vec::new();
    let mut index = 0;

    while index < lines.len() {
        let Some(old_header) = parse_file_header(lines[index], "--- ") else {
            index += 1;
            continue;
        };

        let Some(next_line) = lines.get(index + 1) else {
            return Err(PatchError::InvalidUnifiedDiff {
                message: "missing +++ file header after --- header".to_string(),
            });
        };
        let Some(new_header) = parse_file_header(next_line, "+++ ") else {
            index += 1;
            continue;
        };

        index += 2;
        let mut hunks = Vec::new();
        while index < lines.len() {
            if parse_file_header(lines[index], "--- ").is_some() {
                break;
            }

            let Some((old_start, old_count, _new_start, new_count)) =
                parse_hunk_header(lines[index])
            else {
                index += 1;
                continue;
            };

            index += 1;
            let (hunk_lines, next_index) = parse_hunk_lines(&lines, index, old_count, new_count)?;
            index = next_index;
            hunks.push(ParsedHunk {
                old_start,
                old_count,
                new_count,
                lines: hunk_lines,
            });
        }

        if !hunks.is_empty() {
            files.push(ParsedFileDiff {
                old_path: old_header,
                new_path: new_header,
                rename_from: None,
                rename_to: None,
                hunks,
            });
        }
    }

    files.extend(parse_pure_rename_diffs(&lines)?);

    Ok(files)
}

fn parse_pure_rename_diffs(lines: &[&str]) -> Result<Vec<ParsedFileDiff>> {
    let mut files = Vec::new();
    let mut index = 0;

    while index < lines.len() {
        if !lines[index].starts_with("diff --git ") {
            index += 1;
            continue;
        }

        let block_start = index;
        index += 1;
        while index < lines.len() && !lines[index].starts_with("diff --git ") {
            index += 1;
        }

        let block = &lines[block_start..index];
        if block
            .iter()
            .any(|line| parse_file_header(line, "--- ").is_some())
        {
            continue;
        }

        let rename_from = parse_rename_path(block, "rename from ")?;
        let rename_to = parse_rename_path(block, "rename to ")?;
        if rename_from.is_some() || rename_to.is_some() {
            files.push(ParsedFileDiff {
                old_path: None,
                new_path: None,
                rename_from,
                rename_to,
                hunks: Vec::new(),
            });
        }
    }

    Ok(files)
}

fn parse_rename_path(lines: &[&str], prefix: &str) -> Result<Option<PathBuf>> {
    let Some(line) = lines
        .iter()
        .find_map(|line| line.trim_end_matches(['\r', '\n']).strip_prefix(prefix))
    else {
        return Ok(None);
    };

    let token =
        parse_diff_path_token(line.trim_start()).ok_or_else(|| PatchError::InvalidUnifiedDiff {
            message: format!("empty `{}` path", prefix.trim_end()),
        })?;

    normalize_diff_path(&token)
}

fn parse_file_header(line: &str, prefix: &str) -> Option<Option<PathBuf>> {
    let line = line.trim_end_matches(['\r', '\n']);
    let rest = line.strip_prefix(prefix)?;
    let token = parse_diff_path_token(rest.trim_start())?;
    normalize_diff_path(&token).ok()
}

fn parse_diff_path_token(value: &str) -> Option<String> {
    if let Some(stripped) = value.strip_prefix('"') {
        let mut token = String::new();
        let mut chars = stripped.chars();
        while let Some(ch) = chars.next() {
            match ch {
                '"' => return Some(token),
                '\\' => {
                    if let Some(escaped) = chars.next() {
                        token.push(match escaped {
                            'n' => '\n',
                            't' => '\t',
                            other => other,
                        });
                    }
                }
                other => token.push(other),
            }
        }
        return None;
    }

    value
        .split_whitespace()
        .next()
        .map(std::string::ToString::to_string)
}

fn normalize_diff_path(path: &str) -> Result<Option<PathBuf>> {
    if path == "/dev/null" {
        return Ok(None);
    }

    let relative = path
        .strip_prefix("a/")
        .or_else(|| path.strip_prefix("b/"))
        .unwrap_or(path);

    if relative.is_empty() {
        return Err(PatchError::UnsafePath {
            path: PathBuf::from(path),
            reason: "empty path".to_string(),
        });
    }

    Ok(Some(PathBuf::from(relative)))
}

fn parse_hunk_header(line: &str) -> Option<(usize, usize, usize, usize)> {
    let line = line.trim_end_matches(['\r', '\n']);
    let rest = line.strip_prefix("@@ ")?;
    let end = rest.find(" @@")?;
    let mut parts = rest[..end].split_whitespace();
    let old_range = parts.next()?;
    let new_range = parts.next()?;
    let (old_start, old_count) = parse_hunk_range(old_range, '-')?;
    let (new_start, new_count) = parse_hunk_range(new_range, '+')?;
    Some((old_start, old_count, new_start, new_count))
}

fn parse_hunk_range(value: &str, prefix: char) -> Option<(usize, usize)> {
    let value = value.strip_prefix(prefix)?;
    let (start, count) = match value.split_once(',') {
        Some((start, count)) => (start.parse().ok()?, count.parse().ok()?),
        None => (value.parse().ok()?, 1),
    };
    Some((start, count))
}

fn parse_hunk_lines(
    lines: &[&str],
    mut index: usize,
    old_count: usize,
    new_count: usize,
) -> Result<(Vec<HunkLine>, usize)> {
    let mut hunk_lines = Vec::new();
    let mut old_seen = 0;
    let mut new_seen = 0;

    while index < lines.len() && (old_seen < old_count || new_seen < new_count) {
        let line = lines[index];
        if line.starts_with("\\ No newline at end of file") {
            mark_previous_line_without_newline(&mut hunk_lines)?;
            index += 1;
            continue;
        }

        let Some(prefix) = line.chars().next() else {
            return Err(PatchError::InvalidUnifiedDiff {
                message: "empty hunk line".to_string(),
            });
        };
        let content = line[1..].to_string();

        match prefix {
            ' ' => {
                old_seen += 1;
                new_seen += 1;
                hunk_lines.push(HunkLine::Context(content));
            }
            '-' => {
                old_seen += 1;
                hunk_lines.push(HunkLine::Remove(content));
            }
            '+' => {
                new_seen += 1;
                hunk_lines.push(HunkLine::Add(content));
            }
            other => {
                return Err(PatchError::InvalidUnifiedDiff {
                    message: format!("unexpected hunk line prefix `{other}`"),
                });
            }
        }

        index += 1;
    }

    if old_seen != old_count || new_seen != new_count {
        return Err(PatchError::InvalidUnifiedDiff {
            message: format!(
                "hunk line count mismatch: expected -{old_count} +{new_count}, got -{old_seen} +{new_seen}"
            ),
        });
    }

    Ok((hunk_lines, index))
}

fn mark_previous_line_without_newline(hunk_lines: &mut [HunkLine]) -> Result<()> {
    let Some(line) = hunk_lines.last_mut() else {
        return Err(PatchError::InvalidUnifiedDiff {
            message: "newline marker without previous hunk line".to_string(),
        });
    };

    let value = match line {
        HunkLine::Context(value) | HunkLine::Add(value) | HunkLine::Remove(value) => value,
    };
    if value.ends_with('\n') {
        value.pop();
        if value.ends_with('\r') {
            value.pop();
        }
    }
    Ok(())
}

fn apply_hunks(original: &str, hunks: &[ParsedHunk]) -> Result<String> {
    let original_lines = split_lines_preserving_endings(original);
    let mut modified = Vec::new();
    let mut original_index = 0;

    for hunk in hunks {
        validate_hunk_counts(hunk)?;
        let target_index = hunk.old_start.saturating_sub(1);
        if target_index < original_index || target_index > original_lines.len() {
            return Err(PatchError::InvalidUnifiedDiff {
                message: format!("hunk starts outside target file at line {}", hunk.old_start),
            });
        }

        while original_index < target_index {
            modified.push(original_lines[original_index].clone());
            original_index += 1;
        }

        for line in &hunk.lines {
            match line {
                HunkLine::Context(value) => {
                    consume_original_line(&original_lines, &mut original_index, value)?;
                    modified.push(value.clone());
                }
                HunkLine::Remove(value) => {
                    consume_original_line(&original_lines, &mut original_index, value)?;
                }
                HunkLine::Add(value) => modified.push(value.clone()),
            }
        }
    }

    while original_index < original_lines.len() {
        modified.push(original_lines[original_index].clone());
        original_index += 1;
    }

    Ok(modified.concat())
}

fn validate_hunk_counts(hunk: &ParsedHunk) -> Result<()> {
    let old_seen = hunk
        .lines
        .iter()
        .filter(|line| matches!(line, HunkLine::Context(_) | HunkLine::Remove(_)))
        .count();
    let new_seen = hunk
        .lines
        .iter()
        .filter(|line| matches!(line, HunkLine::Context(_) | HunkLine::Add(_)))
        .count();

    if old_seen != hunk.old_count || new_seen != hunk.new_count {
        return Err(PatchError::InvalidUnifiedDiff {
            message: "parsed hunk counts do not match hunk header".to_string(),
        });
    }

    Ok(())
}

fn consume_original_line(
    original_lines: &[String],
    original_index: &mut usize,
    expected: &str,
) -> Result<()> {
    let Some(actual) = original_lines.get(*original_index) else {
        return Err(PatchError::InvalidUnifiedDiff {
            message: "hunk consumes past end of target file".to_string(),
        });
    };

    if actual != expected {
        return Err(PatchError::InvalidUnifiedDiff {
            message: format!(
                "hunk context mismatch at target line {}",
                *original_index + 1
            ),
        });
    }

    *original_index += 1;
    Ok(())
}

fn split_lines_preserving_endings(value: &str) -> Vec<String> {
    value
        .split_inclusive('\n')
        .map(std::string::ToString::to_string)
        .collect()
}

fn ensure_safe_relative_path(cwd: &Path, relative: &Path) -> Result<()> {
    if relative.as_os_str().is_empty() {
        return Err(PatchError::UnsafePath {
            path: relative.to_path_buf(),
            reason: "empty path".to_string(),
        });
    }

    for component in relative.components() {
        match component {
            Component::Normal(_) | Component::CurDir => {}
            Component::ParentDir => {
                return Err(PatchError::UnsafePath {
                    path: relative.to_path_buf(),
                    reason: "parent directory components are not allowed".to_string(),
                });
            }
            Component::RootDir | Component::Prefix(_) => {
                return Err(PatchError::UnsafePath {
                    path: relative.to_path_buf(),
                    reason: "absolute paths are not allowed".to_string(),
                });
            }
        }
    }

    let first_normal = relative.components().find_map(|component| match component {
        Component::Normal(value) => Some(value),
        _ => None,
    });
    if let Some(first) = first_normal {
        let first_str = first.to_string_lossy();
        if first_str == ".git" || first_str == ".nerve" {
            return Err(PatchError::ForbiddenPath {
                path: relative.to_path_buf(),
                prefix: format!("{first_str}/"),
            });
        }
    }

    let cwd = cwd.canonicalize().map_err(|source| PatchError::Io {
        path: cwd.to_path_buf(),
        source,
    })?;
    let target = cwd.join(relative);
    let mut ancestor = target.as_path();
    while !ancestor.exists() {
        let Some(parent) = ancestor.parent() else {
            break;
        };
        ancestor = parent;
    }

    let canonical_ancestor = ancestor.canonicalize().map_err(|source| PatchError::Io {
        path: ancestor.to_path_buf(),
        source,
    })?;
    if !canonical_ancestor.starts_with(&cwd) {
        return Err(PatchError::UnsafePath {
            path: relative.to_path_buf(),
            reason: "target resolves outside cwd".to_string(),
        });
    }

    Ok(())
}

pub fn sha256_hex(value: &str) -> String {
    let digest = Sha256::digest(value.as_bytes());
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        let _ = write!(&mut out, "{byte:02x}");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn applies_and_rolls_back_patch() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("src/lib.rs");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "fn old() {}\n").unwrap();

        let patch = NvPatch::single("src/lib.rs", "fn old() {}\n", "fn new() {}\n");

        patch.apply(dir.path(), false).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "fn new() {}\n");

        patch.rollback(dir.path(), false).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "fn old() {}\n");
    }

    #[test]
    fn rejects_changed_target() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("file.txt"), "unexpected\n").unwrap();
        let patch = NvPatch::single("file.txt", "before\n", "after\n");

        let err = patch.apply(dir.path(), true).unwrap_err();
        assert!(matches!(err, PatchError::HashMismatch { .. }));
    }

    #[test]
    fn parses_unified_diff_for_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("file.txt"), "before\nsame\n").unwrap();
        let diff = "\
diff --git a/file.txt b/file.txt
--- a/file.txt
+++ b/file.txt
@@ -1,2 +1,2 @@
-before
+after
 same
";

        let patch = NvPatch::from_unified_diff(dir.path(), diff)
            .unwrap()
            .unwrap();

        assert_eq!(patch.files.len(), 1);
        assert_eq!(patch.files[0].path, PathBuf::from("file.txt"));
        assert_eq!(patch.files[0].operation, FileOperation::Modify);
        assert_eq!(patch.files[0].original, "before\nsame\n");
        assert_eq!(patch.files[0].modified, "after\nsame\n");
    }

    #[test]
    fn parses_unified_diff_for_new_file() {
        let dir = tempfile::tempdir().unwrap();
        let diff = "\
diff --git a/new.txt b/new.txt
--- /dev/null
+++ b/new.txt
@@ -0,0 +1,2 @@
+first
+second
";

        let patch = NvPatch::from_unified_diff(dir.path(), diff)
            .unwrap()
            .unwrap();

        assert_eq!(patch.files[0].path, PathBuf::from("new.txt"));
        assert_eq!(patch.files[0].operation, FileOperation::Create);
        assert_eq!(patch.files[0].original, "");
        assert_eq!(patch.files[0].modified, "first\nsecond\n");
    }

    #[test]
    fn create_patch_rolls_back_by_removing_created_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("new.txt");
        let patch = NvPatch::new(vec![FilePatch::create("new.txt", "created\n")]);

        patch.apply(dir.path(), false).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "created\n");

        patch.rollback(dir.path(), false).unwrap();
        assert!(!path.exists());
    }

    #[test]
    fn staged_write_replaces_file_without_leaving_temp_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("file.txt");
        fs::write(&path, "old\n").unwrap();
        let patch = NvPatch::single("file.txt", "old\n", "new\n");

        patch.apply(dir.path(), false).unwrap();

        assert_eq!(fs::read_to_string(&path).unwrap(), "new\n");
        assert_no_staged_write_temps(dir.path());
    }

    #[test]
    fn staged_write_cleans_up_temp_file_when_rename_fails() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("target")).unwrap();

        let err = write_string(dir.path(), Path::new("target"), "value\n").unwrap_err();

        assert!(matches!(err, PatchError::Io { .. }));
        assert_no_staged_write_temps(dir.path());
    }

    #[test]
    fn rejects_parent_directory_patch_path() {
        let dir = tempfile::tempdir().unwrap();
        let diff = "\
--- a/../outside.txt
+++ b/../outside.txt
@@ -0,0 +1 @@
+bad
";

        let err = NvPatch::from_unified_diff(dir.path(), diff).unwrap_err();

        assert!(matches!(err, PatchError::UnsafePath { .. }));
    }

    #[test]
    fn rejects_unsafe_patch_path_during_apply() {
        let dir = tempfile::tempdir().unwrap();
        let patch = NvPatch::single("../outside.txt", "", "bad\n");

        let err = patch.apply(dir.path(), true).unwrap_err();

        assert!(matches!(err, PatchError::UnsafePath { .. }));
    }

    #[cfg(unix)]
    #[test]
    fn rejects_patch_path_through_symlinked_directory() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        symlink(outside.path(), dir.path().join("linked")).unwrap();
        let patch = NvPatch::single("linked/file.txt", "", "bad\n");

        let err = patch.apply(dir.path(), true).unwrap_err();

        assert!(matches!(err, PatchError::UnsafePath { .. }));
    }

    #[test]
    fn parses_and_applies_file_deletion() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("file.txt");
        fs::write(&path, "remove\n").unwrap();
        let diff = "\
--- a/file.txt
+++ /dev/null
@@ -1 +0,0 @@
-remove
";

        let patch = NvPatch::from_unified_diff(dir.path(), diff)
            .unwrap()
            .unwrap();

        assert_eq!(patch.files[0].path, PathBuf::from("file.txt"));
        assert_eq!(patch.files[0].operation, FileOperation::Delete);
        assert_eq!(patch.files[0].original, "remove\n");
        assert_eq!(patch.files[0].modified, "");

        patch.apply(dir.path(), false).unwrap();
        assert!(!path.exists());

        patch.rollback(dir.path(), false).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "remove\n");
    }

    #[test]
    fn parses_and_applies_pure_file_rename() {
        let dir = tempfile::tempdir().unwrap();
        let old_path = dir.path().join("old.txt");
        let new_path = dir.path().join("new.txt");
        fs::write(&old_path, "same\n").unwrap();
        let diff = "\
diff --git a/old.txt b/new.txt
similarity index 100%
rename from old.txt
rename to new.txt
";

        let patch = NvPatch::from_unified_diff(dir.path(), diff)
            .unwrap()
            .unwrap();

        assert!(!patch.is_empty());
        assert_eq!(patch.files[0].path, PathBuf::from("new.txt"));
        assert_eq!(
            patch.files[0].operation,
            FileOperation::Rename {
                from: PathBuf::from("old.txt")
            }
        );
        assert_eq!(patch.files[0].original, "same\n");
        assert_eq!(patch.files[0].modified, "same\n");
        assert!(patch.to_unified_diff().contains("rename from old.txt"));

        let report = patch.apply(dir.path(), false).unwrap();
        assert_eq!(
            report.changed_files,
            vec![PathBuf::from("old.txt"), PathBuf::from("new.txt")]
        );
        assert!(!old_path.exists());
        assert_eq!(fs::read_to_string(&new_path).unwrap(), "same\n");

        patch.rollback(dir.path(), false).unwrap();
        assert_eq!(fs::read_to_string(&old_path).unwrap(), "same\n");
        assert!(!new_path.exists());
    }

    #[test]
    fn parses_and_applies_rename_with_content_change() {
        let dir = tempfile::tempdir().unwrap();
        let old_path = dir.path().join("old.txt");
        let new_path = dir.path().join("new.txt");
        fs::write(&old_path, "before\n").unwrap();
        let diff = "\
diff --git a/old.txt b/new.txt
similarity index 80%
rename from old.txt
rename to new.txt
--- a/old.txt
+++ b/new.txt
@@ -1 +1 @@
-before
+after
";

        let patch = NvPatch::from_unified_diff(dir.path(), diff)
            .unwrap()
            .unwrap();

        assert_eq!(
            patch.files[0].operation,
            FileOperation::Rename {
                from: PathBuf::from("old.txt")
            }
        );
        assert_eq!(patch.files[0].original, "before\n");
        assert_eq!(patch.files[0].modified, "after\n");

        patch.apply(dir.path(), false).unwrap();
        assert!(!old_path.exists());
        assert_eq!(fs::read_to_string(&new_path).unwrap(), "after\n");

        patch.rollback(dir.path(), false).unwrap();
        assert_eq!(fs::read_to_string(&old_path).unwrap(), "before\n");
        assert!(!new_path.exists());
    }

    #[test]
    fn apply_rolls_back_prior_files_when_later_file_fails() {
        let dir = tempfile::tempdir().unwrap();
        let blocking_path = dir.path().join("blocked");
        let nested_path = dir.path().join("blocked/child.txt");
        let patch = NvPatch::new(vec![
            FilePatch::create("blocked", "file, not a directory\n"),
            FilePatch::create("blocked/child.txt", "child\n"),
        ]);

        let err = patch.apply(dir.path(), false).unwrap_err();

        assert!(matches!(err, PatchError::Io { .. }));
        assert!(!blocking_path.exists());
        assert!(!nested_path.exists());
    }

    #[test]
    fn canonical_hash_idempotent() {
        let patch = NvPatch::new(vec![
            FilePatch::new("src/a.rs", "old\n", "new\n"),
            FilePatch::new("src/b.rs", "x\n", "y\n"),
        ]);
        assert_eq!(patch.canonical_hash(), patch.canonical_hash());
    }

    #[test]
    fn canonical_hash_normalizes_crlf() {
        let lf = NvPatch::new(vec![FilePatch::new("src/a.rs", "a\nb\n", "c\nd\n")]);
        let crlf = NvPatch::new(vec![FilePatch::new("src/a.rs", "a\r\nb\r\n", "c\r\nd\r\n")]);
        assert_eq!(lf.canonical_hash(), crlf.canonical_hash());
    }

    #[test]
    fn canonical_hash_path_order_invariant() {
        let a = NvPatch::new(vec![
            FilePatch::new("src/a.rs", "1\n", "2\n"),
            FilePatch::new("src/b.rs", "3\n", "4\n"),
        ]);
        let b = NvPatch::new(vec![
            FilePatch::new("src/b.rs", "3\n", "4\n"),
            FilePatch::new("src/a.rs", "1\n", "2\n"),
        ]);
        assert_eq!(a.canonical_hash(), b.canonical_hash());
    }

    #[test]
    fn canonical_hash_ignores_metadata_fields() {
        let mut a = NvPatch::new(vec![FilePatch::new("src/a.rs", "1\n", "2\n")]);
        let mut b = NvPatch::new(vec![FilePatch::new("src/a.rs", "1\n", "2\n")]);
        a.id = "id-one".to_string();
        b.id = "id-two".to_string();
        a.base_commit = Some("aaa".to_string());
        b.base_commit = Some("bbb".to_string());
        assert_eq!(a.canonical_hash(), b.canonical_hash());
    }

    #[test]
    fn rejects_git_meta_path() {
        let dir = tempfile::tempdir().unwrap();
        let patch = NvPatch::new(vec![FilePatch::create(".git/config", "evil\n")]);

        let err = patch.apply(dir.path(), true).unwrap_err();

        assert!(matches!(err, PatchError::ForbiddenPath { .. }));
    }

    #[test]
    fn rejects_nerve_meta_path() {
        let dir = tempfile::tempdir().unwrap();
        let patch = NvPatch::new(vec![FilePatch::create(".nerve/sessions/x.json", "evil\n")]);

        let err = patch.apply(dir.path(), true).unwrap_err();

        assert!(matches!(err, PatchError::ForbiddenPath { .. }));
    }

    fn assert_no_staged_write_temps(path: &Path) {
        for entry in fs::read_dir(path).unwrap() {
            let entry = entry.unwrap();
            let name = entry.file_name();
            let name = name.to_string_lossy();
            assert!(
                !name.contains(".nerve-write-"),
                "unexpected staged write temp file: {}",
                entry.path().display()
            );
        }
    }
}
