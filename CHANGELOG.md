# Changelog

All notable changes to Nerve are documented here.

## v1.0.0 - 2026-06-15

### Added

- `/goal` support in the terminal workspace, including deterministic argv checks, natural-language goal conversion with interactive confirmation, active-goal persistence, goal history audit entries, timeout/output caps, no-progress detection, and ulimit-backed check execution.
- `/budget` support in the terminal workspace with session token/cost caps, global ceiling enforcement, raise confirmation, and append-only budget audit hash-chain validation.
- Worktree-isolated apply mode with `--worktree` / `--no-worktree` overrides and safety checks for dirty trees, symlink escapes, orphaned worktrees, and main-branch movement during merge/discard.
- Typed RPC envelope streaming for `nv daemon --rpc`, including lifecycle events, bearer-token rotation, envelope metadata, payload caps, backpressure tracking, and RPC doctor checks.
- `nv plan` read-only planning mode with structured Markdown validation, optional dual review, profile-aware plan strategy, and hard guards against patch/diff artifacts.
- Ratatui-backed terminal summary/TUI support through the `nerve-tui` crate.
- Session fork/branch support with `nv fork`, `nv branch`, `nv sessions list`, and `nv sessions tree`.
- MCP client support with `nv mcp list-tools`, `nv mcp probe`, and interactive `/mcp list` / `/mcp call`.
- Mayor/Patrol multi-instance queue primitives with `nv mayor`, `nv patrol`, queue/result directories, atomic claim transitions, heartbeat/orphan recovery, and per-task budget ceilings.
- New shared v1.0 wire types for RPC envelopes, plan reports, MCP tools, session trees, and patrol state.

### Changed

- README and usage docs now describe the v1.0 command surface, configuration knobs, operational safety model, and release install flow.
- Mock one-shot plan responses now emit valid structured plan Markdown, so `nv --adapter mock plan "<task>"` works as a local smoke test.
- Profile matching and plan mode now populate context paths from prompt path tokens and respect per-profile plan settings.
- RPC token paths are resolved relative to the workspace root; patrol tokens are isolated under `.nerve/session-meta/rpc-token-<patrol-id>`.
- MCP global `allow_tools` is enforced by scoping each server's effective allowlist before list/call operations.

### Fixed

- Budget audit-chain verification now detects removed links after hashed entries begin while still accepting a contiguous legacy prefix.
- Natural-language goal conversion rejects model-controlled `cwd` changes and freezes the caller workspace.
- Worktree merge/discard refuses to reset when main HEAD moved after round preparation.
- Worktree chmod handling skips symlink targets.
- Release builds now compile on Linux and Windows by using the platform-native `setrlimit` resource type, a cross-platform disk-space probe, and Unix-only chmod/test guards.
- Legacy stored sessions keep their round history when bootstrapped into the fork index.
- Mayor/Patrol queue IDs are validated as safe file components and duplicate task IDs are rejected before enqueue.
- Patrol claim writes a heartbeat before moving a pending task so status/orphan recovery does not immediately reclaim fresh work.

### Verified

- `cargo fmt --check`
- `cargo test --workspace`
- `cargo clippy --workspace --all-targets -- -D warnings`
- `cargo check --locked -p nerve-cli --target x86_64-unknown-linux-gnu`
- `cargo check --locked -p nerve-cli --target x86_64-pc-windows-msvc`
- `cargo build --release --locked -p nerve-cli`
- `cargo run -p nerve-cli -- config validate`
- `cargo run -p nerve-cli -- --adapter mock benchmark pi --iterations 1 --json`
- `cargo run -p nerve-cli -- --adapter mock doctor`
- `cargo run -p nerve-cli -- --adapter mock plan "update docs"`
- `cargo run -p nerve-cli -- mcp list-tools`
- `cargo run -p nerve-cli -- mayor --status-only`
- `cargo run -p nerve-cli -- patrol --id slot-1 --status`
- `cargo run -p nerve-cli -- sessions list --json`

## v0.1.9 - 2026-05-14

### Added

- `nv benchmark pi` for a deterministic Pi-inspired workflow benchmark covering config load, store initialization, lead/reviewer loop, structured patch creation, apply, rollback, history, and patch index checks.
- Interactive `/benchmark pi [iterations]` command for running the same benchmark from the terminal workspace.

### Changed

- Real Claude/Codex subprocess integration now emits adapter completion events and extracts token/cost usage from JSONL output when providers report it.
- Reviewer subprocess output can now contribute a structured suggested patch, enabling `reviewer_priority` and `merge_attempt` policies to use real reviewer diffs.

## v0.1.8 - 2026-05-13

### Added

- Interactive slash-command palette in real terminals: type `/` to see commands, use arrow keys to move through command suggestions, and press Tab or Right to complete.
- Arrow-key prompt history for the terminal workspace.

## v0.1.7 - 2026-05-13

### Added

- Claude/Codex-style terminal workspace controls for changing mode, switching adapters, changing directories, clearing the screen, running shell commands, and submitting multiline tasks without restarting `nv`.
- Git branch and dirty-state context in the interactive prompt.

## v0.1.6 - 2026-05-13

### Added

- `nv --version` for checking which installed binary is active.

### Changed

- The macOS/Linux installer now warns when the current shell resolves `nv` to a different path than the newly installed binary.
- Installation docs now include `which -a nv`, version verification, and stale binary cleanup guidance.

## v0.1.5 - 2026-05-13

### Added

- `nv interactive` for a forced terminal workspace entrypoint.

### Changed

- The terminal workspace now remembers the last reviewed patch and supports `/diff`, `/apply [patch-id]`, `/rollback [patch-id]`, `/status`, and `/template <id> [args...]`.

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
