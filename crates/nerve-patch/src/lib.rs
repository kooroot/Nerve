use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use similar::TextDiff;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
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
    pub original: String,
    pub modified: String,
    pub original_sha256: String,
    pub modified_sha256: String,
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
                file.write_modified(cwd)?;
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
                file.write_original(cwd)?;
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
        let original = original.into();
        let modified = modified.into();
        Self {
            path: path.into(),
            original_sha256: sha256_hex(&original),
            modified_sha256: sha256_hex(&modified),
            original,
            modified,
        }
    }

    pub fn to_unified_diff(&self) -> String {
        let old_path = format!("a/{}", self.path.display());
        let new_path = format!("b/{}", self.path.display());
        TextDiff::from_lines(&self.original, &self.modified)
            .unified_diff()
            .header(&old_path, &new_path)
            .to_string()
    }

    fn validate(&self, cwd: &Path, rollback: bool) -> Result<()> {
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

    fn write_modified(&self, cwd: &Path) -> Result<()> {
        write_string(cwd, &self.path, &self.modified)
    }

    fn write_original(&self, cwd: &Path) -> Result<()> {
        write_string(cwd, &self.path, &self.original)
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
}
