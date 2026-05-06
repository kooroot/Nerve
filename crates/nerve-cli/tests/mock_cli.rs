use std::process::Command;

#[test]
fn mock_cli_prints_reviewed_diff_without_applying() {
    let output = Command::new(env!("CARGO_BIN_EXE_nv"))
        .env("NERVE_ADAPTER", "mock")
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
    let output = Command::new(env!("CARGO_BIN_EXE_nv"))
        .env("NERVE_ADAPTER", "mock")
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
}
