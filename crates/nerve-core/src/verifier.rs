//! S4: built-in verification gate.
//!
//! Nerve's north star is "acceptance = reviewer accepts AND a deterministic
//! verifier is green". Before S4 that deterministic verifier only existed when
//! the operator supplied a `/goal check_cmd`; without one, the check returned
//! [`CheckResult::Skipped`](nerve_types::CheckResult) and acceptance silently
//! collapsed onto the reviewer's opinion alone.
//!
//! This module supplies the missing gate: when a run has no explicit `/goal`
//! and the operator has opted in
//! ([`BuiltinVerifierMode::Auto`](nerve_config::BuiltinVerifierMode::Auto) or
//! `Command`), it detects/uses the project's conventional test/build command and
//! synthesizes a [`GoalSpec`] so the *existing* sandboxed
//! [`GoalEvaluator`](crate::GoalEvaluator) machinery (env whitelist, timeout,
//! output cap, optional ulimit) runs it as the gate — supplying a real
//! `Pass`/`Fail` instead of `Skipped`.
//!
//! Execution is **opt-in, not default**: running a project's test command
//! executes project-controlled code, and the guards above are resource limits,
//! not filesystem/network isolation. The default
//! [`BuiltinVerifierMode::Off`](nerve_config::BuiltinVerifierMode::Off) keeps
//! Nerve from executing repo code without consent; the CLI warns loudly that
//! acceptance then rests on the reviewer alone (roadmap anti-pattern #1). For
//! filesystem/network confinement of the executed code, pair an opt-in verifier
//! with the OS execution sandbox (roadmap S5,
//! [`SandboxConfig`](nerve_config::SandboxConfig)) — `sandbox.mode=required`
//! fails closed if no backend is available.

use nerve_config::{BuiltinVerifierMode, GoalSpec, Orchestration};
use std::collections::BTreeMap;
use std::path::Path;

/// Synthetic goal id assigned to a built-in verifier spec so run reports and
/// logs can tell it apart from an operator-supplied `/goal`.
pub const BUILTIN_VERIFIER_GOAL_ID: &str = "builtin-verifier";

/// A verification command detected from project marker files, plus a short
/// human label for the CLI notice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DetectedVerifier {
    pub label: String,
    pub command: Vec<String>,
}

/// The synthesized deterministic check the loop runs when no explicit `/goal`
/// is set, plus a human label for surfacing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedVerifier {
    pub spec: GoalSpec,
    pub label: String,
}

/// Detect a deterministic test command from `cwd`'s ecosystem markers.
///
/// Only ecosystems whose conventional command exits `0` on a healthy tree with
/// no tests are auto-detected (Rust `cargo test`, Go `go test ./...`, and Node
/// only when a `test` script is defined) so the gate never fails *spuriously*
/// — a spurious `Fail` would block legitimate work. Anything else returns
/// `None`; the operator configures an explicit `command` instead.
pub fn detect_builtin_verifier(cwd: &Path) -> Option<DetectedVerifier> {
    if cwd.join("Cargo.toml").exists() {
        return Some(DetectedVerifier {
            label: "cargo test".to_string(),
            command: vec!["cargo".into(), "test".into(), "--quiet".into()],
        });
    }
    if cwd.join("go.mod").exists() {
        return Some(DetectedVerifier {
            label: "go test ./...".to_string(),
            command: vec!["go".into(), "test".into(), "./...".into()],
        });
    }
    if node_has_test_script(cwd) {
        return Some(DetectedVerifier {
            label: "npm test".to_string(),
            command: vec!["npm".into(), "test".into()],
        });
    }
    None
}

/// `true` when `cwd/package.json` declares a non-empty `scripts.test`. Without a
/// test script, `npm test` exits non-zero, which would block the loop — so a
/// script-less Node project is treated as undetectable.
fn node_has_test_script(cwd: &Path) -> bool {
    let Ok(raw) = std::fs::read_to_string(cwd.join("package.json")) else {
        return false;
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&raw) else {
        return false;
    };
    value
        .get("scripts")
        .and_then(|scripts| scripts.get("test"))
        .and_then(|test| test.as_str())
        .is_some_and(|test| !test.trim().is_empty())
}

/// Environment variable name carrying explicit operator consent to honor a
/// *project-local* config that enables an executing built-in verifier.
pub const PROJECT_VERIFIER_CONSENT_ENV: &str = "NERVE_TRUST_PROJECT_VERIFIER";

/// Out-of-band operator consent to run repo code from *project-local* config.
///
/// Read from the operator's environment — a cloned repo cannot set the
/// operator's shell env, so it cannot forge this. Truthy values: `1`, `true`,
/// `yes`, `on` (case-insensitive).
pub fn project_verifier_consent_from_env() -> bool {
    std::env::var(PROJECT_VERIFIER_CONSENT_ENV)
        .map(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on"))
        .unwrap_or(false)
}

/// Resolve the built-in verifier for `orch` against `cwd` into a synthetic
/// [`GoalSpec`], or `None` when disabled (`Off`), untrusted, or undetectable
/// (`Auto` with no recognized markers).
///
/// `exec_trusted` gates the *executing* modes (`Auto`/`Command`): when `false`
/// (project-local config without operator consent — see
/// [`nerve_config::Config::builtin_verifier_exec_trusted`]) this returns `None`
/// so a cloned repo cannot opt itself into running code. `Off` always returns
/// `None`. `Command` mode trusts the operator's argv verbatim (validated at
/// config load by [`nerve_config::BuiltinVerifierConfig::validate`]).
pub fn resolve_builtin_verifier(
    orch: &Orchestration,
    cwd: &Path,
    exec_trusted: bool,
) -> Option<ResolvedVerifier> {
    let cfg = &orch.builtin_verifier;
    if matches!(cfg.mode, BuiltinVerifierMode::Off) {
        return None;
    }
    // Auto and Command both execute project-controlled code; refuse unless the
    // enabling config is operator-trusted.
    if !exec_trusted {
        return None;
    }
    let detected = match cfg.mode {
        BuiltinVerifierMode::Off => return None,
        BuiltinVerifierMode::Auto => detect_builtin_verifier(cwd)?,
        BuiltinVerifierMode::Command => DetectedVerifier {
            label: format!("configured: {}", cfg.command.join(" ")),
            command: cfg.command.clone(),
        },
    };
    if detected.command.is_empty() {
        return None;
    }
    let spec = GoalSpec {
        id: BUILTIN_VERIFIER_GOAL_ID.to_string(),
        check_cmd: detected.command,
        timeout_secs: cfg.timeout_secs,
        cwd: None,
        env: BTreeMap::new(),
        no_progress_max: None,
    };
    Some(ResolvedVerifier {
        spec,
        label: detected.label,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use nerve_config::BuiltinVerifierConfig;

    fn orch_with(builtin_verifier: BuiltinVerifierConfig) -> Orchestration {
        // Build a minimal Orchestration via JSON so this test does not depend on
        // every field's constructor; then override the verifier config.
        let mut config = nerve_config::Config::from_json_str(
            r#"{
              "orchestration": {
                "default_strategy": "consensus",
                "max_refinement_rounds": 2,
                "conflict_policy": "lead_priority"
              },
              "roles": { "architect": "claude-code", "reviewer": "codex" },
              "profiles": []
            }"#,
        )
        .unwrap();
        config.orchestration.builtin_verifier = builtin_verifier;
        config.orchestration
    }

    #[test]
    fn detects_cargo_for_rust_project() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Cargo.toml"), "[package]\n").unwrap();

        let detected = detect_builtin_verifier(dir.path()).unwrap();
        assert_eq!(detected.command, vec!["cargo", "test", "--quiet"]);
        assert_eq!(detected.label, "cargo test");
    }

    #[test]
    fn detects_go_for_go_module() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("go.mod"), "module x\n").unwrap();

        let detected = detect_builtin_verifier(dir.path()).unwrap();
        assert_eq!(detected.command, vec!["go", "test", "./..."]);
    }

    #[test]
    fn detects_npm_only_when_test_script_present() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("package.json"),
            r#"{ "scripts": { "test": "jest" } }"#,
        )
        .unwrap();
        let detected = detect_builtin_verifier(dir.path()).unwrap();
        assert_eq!(detected.command, vec!["npm", "test"]);

        // No test script → undetectable (npm test would exit non-zero).
        std::fs::write(dir.path().join("package.json"), r#"{ "scripts": {} }"#).unwrap();
        assert!(detect_builtin_verifier(dir.path()).is_none());
    }

    #[test]
    fn no_markers_yields_no_detection() {
        let dir = tempfile::tempdir().unwrap();
        assert!(detect_builtin_verifier(dir.path()).is_none());
    }

    #[test]
    fn resolve_auto_synthesizes_goal_spec_for_rust() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Cargo.toml"), "[package]\n").unwrap();
        // Auto is opt-in (default is Off), so enable it explicitly. `true` =
        // operator-trusted config.
        let orch = orch_with(BuiltinVerifierConfig {
            mode: BuiltinVerifierMode::Auto,
            ..Default::default()
        });

        let resolved = resolve_builtin_verifier(&orch, dir.path(), true).unwrap();
        assert_eq!(resolved.spec.id, BUILTIN_VERIFIER_GOAL_ID);
        assert_eq!(resolved.spec.check_cmd, vec!["cargo", "test", "--quiet"]);
        assert_eq!(resolved.spec.timeout_secs, 600);
        // Synthesized spec must pass the same validation as a user /goal.
        resolved.spec.validate().unwrap();
    }

    #[test]
    fn resolve_off_returns_none_even_with_markers() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Cargo.toml"), "[package]\n").unwrap();
        let orch = orch_with(BuiltinVerifierConfig {
            mode: BuiltinVerifierMode::Off,
            ..Default::default()
        });

        assert!(resolve_builtin_verifier(&orch, dir.path(), true).is_none());
    }

    #[test]
    fn resolve_command_mode_uses_operator_argv() {
        let dir = tempfile::tempdir().unwrap();
        // No markers; explicit command must still resolve.
        let orch = orch_with(BuiltinVerifierConfig {
            mode: BuiltinVerifierMode::Command,
            command: vec!["just".into(), "verify".into()],
            timeout_secs: 120,
        });

        let resolved = resolve_builtin_verifier(&orch, dir.path(), true).unwrap();
        assert_eq!(resolved.spec.check_cmd, vec!["just", "verify"]);
        assert_eq!(resolved.spec.timeout_secs, 120);
        assert!(resolved.label.contains("just verify"));
    }

    #[test]
    fn resolve_auto_without_markers_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let orch = orch_with(BuiltinVerifierConfig {
            mode: BuiltinVerifierMode::Auto,
            ..Default::default()
        });
        assert!(resolve_builtin_verifier(&orch, dir.path(), true).is_none());
    }

    #[test]
    fn resolve_defaults_to_off_so_markers_are_ignored() {
        // Default config must NOT execute repo code: even with a Cargo.toml
        // present, the default (Off) resolves to no gate.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Cargo.toml"), "[package]\n").unwrap();
        let orch = orch_with(BuiltinVerifierConfig::default());
        assert!(resolve_builtin_verifier(&orch, dir.path(), true).is_none());
    }

    #[test]
    fn resolve_untrusted_project_config_refuses_to_execute() {
        // The codex BLOCK: a cloned repo's `nerve.config.json` enabling Auto (or
        // Command) must NOT run code without operator consent. `exec_trusted =
        // false` models project-local config without consent → no gate.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Cargo.toml"), "[package]\n").unwrap();

        let auto = orch_with(BuiltinVerifierConfig {
            mode: BuiltinVerifierMode::Auto,
            ..Default::default()
        });
        assert!(resolve_builtin_verifier(&auto, dir.path(), false).is_none());

        let command = orch_with(BuiltinVerifierConfig {
            mode: BuiltinVerifierMode::Command,
            command: vec!["just".into(), "verify".into()],
            timeout_secs: 60,
        });
        assert!(resolve_builtin_verifier(&command, dir.path(), false).is_none());

        // With operator consent (exec_trusted = true) the same config resolves.
        assert!(resolve_builtin_verifier(&command, dir.path(), true).is_some());
    }
}
