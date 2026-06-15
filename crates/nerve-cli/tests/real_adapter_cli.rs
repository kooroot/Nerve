#![cfg(unix)]

use std::env;
use std::ffi::OsString;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

#[test]
fn real_adapter_fixture_prints_structured_diff_without_applying() {
    let fixture = RealAdapterFixture::new();

    let output = fixture
        .command()
        .args(["--adapter", "real", "update target file"])
        .output()
        .expect("failed to run nv binary");

    assert!(
        output.status.success(),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Verdict: Lgtm"));
    assert!(stdout.contains("Dry run only"));
    assert!(stdout.contains("--- a/target.txt"));
    assert!(stdout.contains("+after"));
    assert_eq!(
        fs::read_to_string(fixture.target_file()).unwrap(),
        "before\n"
    );
}

#[test]
fn real_adapter_fixture_applies_structured_diff_when_requested() {
    let fixture = RealAdapterFixture::new();

    let output = fixture
        .command()
        .args(["--adapter", "real", "--apply", "update target file"])
        .output()
        .expect("failed to run nv binary");

    assert!(
        output.status.success(),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Verdict: Lgtm"));
    assert!(stdout.contains("Applied patch"));
    assert_eq!(
        fs::read_to_string(fixture.target_file()).unwrap(),
        "after\n"
    );
}

#[test]
fn real_adapter_fixture_uses_reviewer_suggested_patch_when_prioritized() {
    let fixture = RealAdapterFixture::with_scripts(
        Some(
            r#"{
              "orchestration": {
                "default_strategy": "consensus",
                "max_refinement_rounds": 0,
                "conflict_policy": "reviewer_priority"
              },
              "roles": {
                "architect": "claude-code",
                "reviewer": "codex"
              },
              "profiles": []
            }"#,
        ),
        r#"#!/bin/sh
cat <<'JSON'
{"type":"assistant","message":{"content":[{"type":"text","text":"--- a/target.txt\n+++ b/target.txt\n@@ -1 +1 @@\n-before\n+lead\n"}]}}
JSON
"#,
        r#"#!/bin/sh
cat <<'JSON'
{"type":"assistant","message":{"content":[{"type":"text","text":"REQUEST_CHANGES: prefer reviewer patch\n\n--- a/target.txt\n+++ b/target.txt\n@@ -1 +1 @@\n-before\n+reviewed\n"}]}}
JSON
"#,
    );

    let output = fixture
        .command()
        .args(["--adapter", "real", "--apply", "update target file"])
        .output()
        .expect("failed to run nv binary");

    assert!(
        output.status.success(),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Verdict: RequestChanges"));
    assert_eq!(
        fs::read_to_string(fixture.target_file()).unwrap(),
        "reviewed\n"
    );
}

#[test]
fn real_adapter_fixture_honors_configured_output_cap() {
    let fixture = RealAdapterFixture::with_scripts(
        Some(
            r#"{
              "orchestration": {
                "default_strategy": "consensus",
                "max_refinement_rounds": 0,
                "conflict_policy": "lead_priority",
                "adapter_max_output_bytes": 8
              },
              "roles": {
                "architect": "claude-code",
                "reviewer": "codex"
              },
              "profiles": []
            }"#,
        ),
        r#"#!/bin/sh
printf '0123456789'
"#,
        r#"#!/bin/sh
printf '%s\n' 'LGTM: should not run'
"#,
    );

    let output = fixture
        .command()
        .args(["--adapter", "real", "overflow adapter output"])
        .output()
        .expect("failed to run nv binary");

    assert!(
        !output.status.success(),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("adapter output exceeded"),
        "stderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

struct RealAdapterFixture {
    _tempdir: tempfile::TempDir,
    cwd: PathBuf,
    path: OsString,
}

impl RealAdapterFixture {
    fn new() -> Self {
        Self::with_scripts(
            None,
            r#"#!/bin/sh
cat <<'JSON'
{"type":"assistant","message":{"content":[{"type":"text","text":"--- a/target.txt\n+++ b/target.txt\n@@ -1 +1 @@\n-before\n+after\n"}]}}
JSON
"#,
            r#"#!/bin/sh
printf '%s\n' 'LGTM: fixture reviewer accepts the patch'
"#,
        )
    }

    fn with_scripts(config: Option<&str>, claude_script: &str, codex_script: &str) -> Self {
        let tempdir = tempfile::tempdir().unwrap();
        let cwd = tempdir.path().join("workspace");
        let bin = tempdir.path().join("bin");
        fs::create_dir_all(&cwd).unwrap();
        fs::create_dir_all(&bin).unwrap();
        fs::write(cwd.join("target.txt"), "before\n").unwrap();
        if let Some(config) = config {
            fs::write(cwd.join("nerve.config.json"), config).unwrap();
        }

        write_executable(&bin.join("claude"), claude_script);
        write_executable(&bin.join("codex"), codex_script);

        let mut paths = vec![bin];
        if let Some(existing) = env::var_os("PATH") {
            paths.extend(env::split_paths(&existing));
        }
        let path = env::join_paths(paths).unwrap();

        Self {
            _tempdir: tempdir,
            cwd,
            path,
        }
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_nv"));
        command.current_dir(&self.cwd).env("PATH", &self.path);
        command
    }

    fn target_file(&self) -> PathBuf {
        self.cwd.join("target.txt")
    }
}

fn write_executable(path: &Path, contents: &str) {
    fs::write(path, contents).unwrap();
    let mut permissions = fs::metadata(path).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(path, permissions).unwrap();
}
