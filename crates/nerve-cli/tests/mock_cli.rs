use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

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
fn mock_cli_renders_three_pane_tui_summary() {
    let fixture = MockCliFixture::new();
    let output = fixture
        .command()
        .args(["--tui", "add a health endpoint"])
        .output()
        .expect("failed to run nv binary");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Nerve TUI"));
    assert!(stdout.contains("[ Lead ]"));
    assert!(stdout.contains("[ Reviewer ]"));
    assert!(stdout.contains("[ Orchestrator ]"));
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

#[test]
fn mock_cli_setup_initializes_store_and_checks_mock_adapter() {
    let fixture = MockCliFixture::new();
    let output = fixture
        .command()
        .args(["--adapter", "mock"])
        .arg("setup")
        .output()
        .expect("failed to run nv setup");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("store:"));
    assert!(stdout.contains("adapter: mock ok"));
    assert!(fixture.cwd.join(".nerve/sessions").exists());
    assert!(fixture.cwd.join(".nerve/session-meta").exists());
}

#[test]
fn mock_cli_interactive_shows_terminal_workspace_help() {
    let fixture = MockCliFixture::new();
    let mut child = fixture
        .command()
        .arg("interactive")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("failed to spawn nv interactive");

    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(b"/help\n/quit\n")
        .unwrap();
    let output = child.wait_with_output().unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Nerve Terminal"));
    assert!(stdout.contains("nerve:mock:dry-run>"));
    assert!(stdout.contains("/diff"));
    assert!(stdout.contains("/apply [patch-id]"));
}

#[test]
fn mock_cli_interactive_remembers_last_patch_for_apply() {
    let fixture = MockCliFixture::new();
    let mut child = fixture
        .command()
        .arg("interactive")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("failed to spawn nv interactive");

    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(b"add a health endpoint\n/apply\n/quit\n")
        .unwrap();
    let output = child.wait_with_output().unwrap();

    assert!(
        output.status.success(),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("reviewed patch ready"));
    assert!(stdout.contains("Applied patch"));
    assert!(
        fs::read_to_string(fixture.mock_output())
            .unwrap()
            .contains("Status: refined")
    );
}

#[test]
fn mock_cli_names_and_reruns_sessions() {
    let fixture = MockCliFixture::new();
    let output = fixture
        .command()
        .args(["--json", "add a health endpoint"])
        .output()
        .expect("failed to run nv");
    assert!(output.status.success());
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let session_id = report["task"]["id"].as_str().unwrap();

    let name = fixture
        .command()
        .args(["name", session_id, "health check"])
        .output()
        .expect("failed to name session");
    assert!(name.status.success());

    let history = fixture
        .command()
        .args(["history", "--json", "--named"])
        .output()
        .expect("failed to run history");
    assert!(history.status.success());
    let sessions: serde_json::Value = serde_json::from_slice(&history.stdout).unwrap();
    assert_eq!(sessions[0]["name"], "health check");

    let rerun = fixture
        .command()
        .args(["--json", "rerun", session_id, "follow up change"])
        .output()
        .expect("failed to rerun session");
    assert!(rerun.status.success());
    let child: serde_json::Value = serde_json::from_slice(&rerun.stdout).unwrap();
    let child_id = child["task"]["id"].as_str().unwrap();

    let child_history = fixture
        .command()
        .args(["history", "--json"])
        .output()
        .expect("failed to run history");
    let sessions: serde_json::Value = serde_json::from_slice(&child_history.stdout).unwrap();
    let child_summary = sessions
        .as_array()
        .unwrap()
        .iter()
        .find(|summary| summary["id"] == child_id)
        .unwrap();
    assert_eq!(child_summary["parent_session_id"], session_id);
}

#[test]
fn mock_cli_runs_configured_template() {
    let fixture = MockCliFixture::new();
    let list = fixture
        .command()
        .args(["template", "list"])
        .output()
        .expect("failed to list templates");
    assert!(list.status.success());
    assert!(String::from_utf8_lossy(&list.stdout).contains("security-audit"));

    let output = fixture
        .command()
        .args(["--json", "template", "run", "security-audit", "src/lib.rs"])
        .output()
        .expect("failed to run template");
    assert!(output.status.success());
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        report["task"]["prompt"],
        "audit src/lib.rs for correctness, safety, and missing tests"
    );
}

#[test]
fn mock_cli_daemon_processes_one_prompt_as_json_line() {
    let fixture = MockCliFixture::new();
    let mut child = fixture
        .command()
        .args(["daemon", "--once"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("failed to spawn nv daemon");

    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(b"daemon health endpoint\n")
        .unwrap();
    let output = child.wait_with_output().unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    let report: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    assert_eq!(report["task"]["prompt"], "daemon health endpoint");
    assert_eq!(report["final_feedback"]["verdict"], "lgtm");
    assert!(fixture.cwd.join(".nerve/sessions").exists());
}

#[test]
fn mock_cli_rpc_daemon_emits_lifecycle_events() {
    let fixture = MockCliFixture::new();
    let mut child = fixture
        .command()
        .args(["daemon", "--rpc", "--once"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("failed to spawn nv daemon");

    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(br#"{"command":"prompt","prompt":"rpc health endpoint"}"#)
        .unwrap();
    child.stdin.as_mut().unwrap().write_all(b"\n").unwrap();
    let output = child.wait_with_output().unwrap();

    assert!(
        output.status.success(),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let events = String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
        .collect::<Vec<_>>();
    assert!(events.iter().any(|event| event["type"] == "session_start"));
    assert!(events.iter().any(|event| event["type"] == "lead_start"));
    assert!(events.iter().any(|event| event["type"] == "review_start"));
    assert!(events.iter().any(|event| event["type"] == "patch_ready"));
    assert!(events.iter().any(|event| event["type"] == "session_end"));
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
