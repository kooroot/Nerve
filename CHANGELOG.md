# Changelog

All notable changes to Nerve are documented here.

## v0.1.4 - 2026-05-13

### Added

- `nv login` and interactive `/login` to start Claude Code and Codex subscription login flows.

### Changed

- Interactive mode now uses a Nerve-specific banner/prompt, includes `/login` and `/templates`, and keeps the session open after task errors.
- The macOS/Linux installer now chooses standard CLI bin directories instead of the first writable directory on `PATH`.
- Codex subprocess execution now passes `--skip-git-repo-check` for non-interactive runs.

## v0.1.3 - 2026-05-13

### Added

- `nv setup` for first-run store initialization and prerequisite checks.
- Lightweight interactive mode when `nv` is run from a terminal without a prompt.
- Session naming and linked follow-up runs through `nv name` and `nv rerun`.
- Prompt templates through `nv template list` and `nv template run`.
- JSONL RPC daemon mode with lifecycle events via `nv daemon --rpc`.

## v0.1.2 - 2026-05-12

### Added

- One-command installers for macOS/Linux (`install.sh`) and Windows (`install.ps1`).
- Versionless latest release asset names so install commands do not need to know the current Nerve version.

## v0.1.1 - 2026-05-11

Follow-up distribution release for the Nerve CLI.

### Added

- GitHub release automation for Linux, macOS, and Windows `nv` binaries.
- README installation instructions for release downloads and `cargo install --git`.

### Fixed

- `nv daemon` now disables stdin echo when running under a Unix terminal or pty, then restores the original terminal settings on exit. This keeps daemon stdout as clean JSON lines for editor and shell integrations.

### Verified

- `cargo fmt --check`
- `cargo test`

## v0.1.0 - 2026-05-09

Initial usable release of the Nerve CLI orchestration workspace.

### Added

- `nv` CLI for lead/reviewer coding orchestration.
- Config loading from `./nerve.config.json`, user config, or embedded defaults.
- Profile routing with keyword, glob, and logical `all` / `any` match rules.
- Consensus, pipeline, and tournament orchestration strategies.
- Mock and subprocess adapters for local tests and real `claude` / `codex` CLI runs.
- Machine-readable `RunReport` JSON output via `--json`.
- Terminal three-pane summary via `--tui`.
- Line-oriented `nv daemon` mode.
- Persistent `.nerve/` session reports, patch index, history, resume, list, apply, and rollback commands.
- Hash-checked `NvPatch` apply and rollback for create, modify, delete, and rename operations.
- Staged file writes and multi-file snapshot rollback on apply failure.
- Usage budget guards for token and estimated cost ceilings.
- `nv doctor` prerequisite checks.
- Scratch-file crossfire feedback during lead execution.
- `merge_attempt` patch merging through `git merge-file`.
- `nerve-101.md` quick-start and architecture guide.

### Fixed

- Reviewer verdict parsing now reads the leading verdict token instead of matching incidental words such as `no blockers`.
- JSONL diff extraction now uses assistant-authored text fields and ignores tool input/result strings.
- Structured reviewer issues skip leading verdict lines and preserve the actual finding text.
- Crossfire and TUI truncation preserve UTF-8 character boundaries.
- `merge_attempt` preserves conflict-marker output from `git merge-file` instead of treating conflict exit codes as fatal.
- Conflict policies now have explicit runtime behavior for `abort_on_conflict`, `reviewer_block`, and `manual`.
- CLI dispatch populates context paths so glob profile rules can match production tasks.
- `nv doctor` checks Unix executable bits for real adapter binaries.
- Indexed apply and rollback now keep session report `applied` state in sync.
- Store JSON writes use unique temporary files, and patch index updates are serialized with a lock file.

### Verified

- `cargo fmt --check`
- `cargo clippy --all-targets -- -D warnings`
- `cargo test`
- `target/debug/nv config validate`
- Mock doctor, JSON, TUI, and daemon smoke paths.
