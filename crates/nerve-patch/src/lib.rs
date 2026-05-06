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
    #[error("unsupported patch operation: {message}")]
    Unsupported { message: String },
    #[error("invalid patch state for `{path}`: {message}")]
    InvalidOperationState { path: PathBuf, message: String },
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
}

fn default_file_operation() -> FileOperation {
    FileOperation::Modify
}

#[derive(Debug, Clone, PartialEq, Eq)]
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
        self.files.iter().all(|file| file.original == file.modified)
    }

    pub fn to_unified_diff(&self) -> String {
        let mut out = String::new();
        for file in &self.files {
            if file.original == file.modified {
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
        if !dry_run {
            for file in &self.files {
                file.apply(cwd)?;
            }
        }
        Ok(ApplyReport {
            patch_id: self.id.clone(),
            changed_files: self.files.iter().map(|file| file.path.clone()).collect(),
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
            changed_files: self.files.iter().map(|file| file.path.clone()).collect(),
            dry_run,
        })
    }
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
        let old_path = match self.operation {
            FileOperation::Create => "/dev/null".to_string(),
            FileOperation::Modify | FileOperation::Delete => format!("a/{}", self.path.display()),
        };
        let new_path = match self.operation {
            FileOperation::Delete => "/dev/null".to_string(),
            FileOperation::Modify | FileOperation::Create => format!("b/{}", self.path.display()),
        };
        TextDiff::from_lines(&self.original, &self.modified)
            .unified_diff()
            .header(&old_path, &new_path)
            .to_string()
    }

    fn validate(&self, cwd: &Path, rollback: bool) -> Result<()> {
        ensure_safe_relative_path(cwd, &self.path)?;
        self.validate_operation_state(cwd, rollback)?;
        let expected = if rollback {
            &self.modified_sha256
        } else {
            &self.original_sha256
        };
        let current = read_to_string(cwd, &self.path)?;
        let actual = sha256_hex(&current);
        if &actual != expected {
            return Err(PatchError::HashMismatch {
                path: self.path.clone(),
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
            _ => Ok(()),
        }
    }

    fn apply(&self, cwd: &Path) -> Result<()> {
        match self.operation {
            FileOperation::Delete => remove_file(cwd, &self.path),
            FileOperation::Modify | FileOperation::Create => {
                write_string(cwd, &self.path, &self.modified)
            }
        }
    }

    fn rollback(&self, cwd: &Path) -> Result<()> {
        match self.operation {
            FileOperation::Create => remove_file(cwd, &self.path),
            FileOperation::Modify | FileOperation::Delete => {
                write_string(cwd, &self.path, &self.original)
            }
        }
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
    fs::write(&path, value).map_err(|source| PatchError::Io {
        path: relative.to_path_buf(),
        source,
    })
}

fn remove_file(cwd: &Path, relative: &Path) -> Result<()> {
    let path = cwd.join(relative);
    match fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(()),
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
    hunks: Vec<ParsedHunk>,
}

impl ParsedFileDiff {
    fn to_file_patch(&self, cwd: &Path) -> Result<FilePatch> {
        let (path, operation) = match (&self.old_path, &self.new_path) {
            (None, Some(new_path)) => (new_path.clone(), FileOperation::Create),
            (Some(old_path), None) => (old_path.clone(), FileOperation::Delete),
            (Some(old_path), Some(new_path)) if old_path == new_path => {
                (new_path.clone(), FileOperation::Modify)
            }
            (Some(old_path), Some(new_path)) => {
                return Err(PatchError::Unsupported {
                    message: format!(
                        "file rename from `{}` to `{}` is not supported by NvPatch yet",
                        old_path.display(),
                        new_path.display()
                    ),
                });
            }
            (None, None) => {
                return Err(PatchError::InvalidUnifiedDiff {
                    message: "file diff has neither old nor new path".to_string(),
                });
            }
        };

        ensure_safe_relative_path(cwd, &path)?;
        let original = match operation {
            FileOperation::Create => String::new(),
            FileOperation::Modify | FileOperation::Delete => read_to_string(cwd, &path)?,
        };
        let modified = apply_hunks(&original, &self.hunks)?;

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
                hunks,
            });
        }
    }

    Ok(files)
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
}
