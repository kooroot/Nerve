use std::fs;
use std::path::PathBuf;
use std::process::Command;

#[test]
fn mock_cli_prints_reviewed_diff_without_applying() {
    let fixture = MockCliFixture::new();
    let output = fixture
        .command()
        .arg("add a health endpoint")
        .output()
        .expect("failed to run nv binary");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Verdict: Lgtm"));
    assert!(stdout.contains("Dry run only"));
    assert!(stdout.contains(".nerve/mock-output.txt"));
}

#[test]
fn mock_cli_emits_machine_readable_json_report() {
    let fixture = MockCliFixture::new();
    let output = fixture
        .command()
        .args(["--json", "add a health endpoint"])
        .output()
        .expect("failed to run nv binary");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    let report: serde_json::Value = serde_json::from_str(&stdout).unwrap();

    assert_eq!(report["task"]["prompt"], "add a health endpoint");
    assert_eq!(report["final_feedback"]["verdict"], "lgtm");
    assert_eq!(report["applied"], false);
    assert_eq!(report["blocked"], false);
    assert_eq!(report["rounds"].as_array().unwrap().len(), 2);
    assert_eq!(
        report["final_patch"]["files"][0]["path"],
        ".nerve/mock-output.txt"
    );
    assert!(fixture.cwd.join(".nerve/sessions").exists());
    assert!(fixture.cwd.join(".nerve/patches/index.json").exists());
}

#[test]
fn mock_cli_lists_applies_and_rolls_back_indexed_patch() {
    let fixture = MockCliFixture::new();
    let output = fixture
        .command()
        .args(["--json", "add a health endpoint"])
        .output()
        .expect("failed to run nv binary");
    assert!(output.status.success());

    let stdout = String::from_utf8_lossy(&output.stdout);
    let report: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    let session_id = report["task"]["id"].as_str().unwrap();
    let patch_id = report["final_patch"]["id"].as_str().unwrap();

    let history = fixture
        .command()
        .arg("history")
        .output()
        .expect("failed to run nv history");
    assert!(history.status.success());
    assert!(String::from_utf8_lossy(&history.stdout).contains(session_id));

    let resume = fixture
        .command()
        .args(["resume", session_id, "--json"])
        .output()
        .expect("failed to run nv resume");
    assert!(resume.status.success());
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&resume.stdout).unwrap()["task"]["id"],
        session_id
    );

    let list = fixture
        .command()
        .arg("list")
        .output()
        .expect("failed to run nv list");
    assert!(list.status.success());
    assert!(String::from_utf8_lossy(&list.stdout).contains(patch_id));

    let apply = fixture
        .command()
        .args(["apply", patch_id])
        .output()
        .expect("failed to run nv apply");
    assert!(
        apply.status.success(),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&apply.stdout),
        String::from_utf8_lossy(&apply.stderr)
    );
    assert!(
        fs::read_to_string(fixture.mock_output())
            .unwrap()
            .contains("Status: refined")
    );

    let rollback = fixture
        .command()
        .args(["rollback", patch_id])
        .output()
        .expect("failed to run nv rollback");
    assert!(
        rollback.status.success(),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&rollback.stdout),
        String::from_utf8_lossy(&rollback.stderr)
    );
    assert!(!fixture.mock_output().exists());
}

#[test]
fn mock_cli_doctor_passes_without_external_adapters() {
    let fixture = MockCliFixture::new();
    let output = fixture
        .command()
        .args(["--adapter", "mock"])
        .arg("doctor")
        .output()
        .expect("failed to run nv doctor");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("config: ok"));
    assert!(stdout.contains("adapter: mock ok"));
}

struct MockCliFixture {
    _tempdir: tempfile::TempDir,
    cwd: PathBuf,
}

impl MockCliFixture {
    fn new() -> Self {
        let tempdir = tempfile::tempdir().unwrap();
        Self {
            cwd: tempdir.path().to_path_buf(),
            _tempdir: tempdir,
        }
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_nv"));
        command.current_dir(&self.cwd).env("NERVE_ADAPTER", "mock");
        command
    }

    fn mock_output(&self) -> PathBuf {
        self.cwd.join(".nerve/mock-output.txt")
    }
}
