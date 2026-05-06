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
