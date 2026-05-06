# Nerve

Nerve is a Rust CLI for reflexive AI orchestration: one lead coding agent proposes an implementation, a reviewer agent critiques it, and the orchestrator converges on a reviewed patch before anything is written to disk.

The long-term goal is not "run two models next to each other." The goal is a practical execution layer where Claude Code, Codex, and future coding agents can create friction, exchange review feedback, preserve the full decision trail, and apply changes through an auditable patch system.

## Current Goal

Nerve is currently targeting a conservative Phase 1 MVP:

- `nv "<task>"` dispatches a task through a lead/reviewer loop.
- The lead proposes work, the reviewer returns `LGTM`, `REQUEST_CHANGES`, or `BLOCK`.
- Refinement continues until `LGTM` or `max_refinement_rounds` is reached.
- Patches are dry-run by default and require `--apply` before files are changed.
- Mock adapters provide deterministic end-to-end behavior for local verification.
- Real adapters spawn `claude` and `codex` subprocesses as the integration boundary.

Important current limitation: the real subprocess adapters collect raw CLI output today. Structured `NvPatch` extraction from real unified diffs is the next core milestone before real-agent `--apply` should be treated as complete.

## Why It Exists

Single-agent coding has a predictable failure mode: the same model that writes a patch often rationalizes it. Nerve makes review a first-class part of the execution path. It keeps the lead and reviewer roles separate, records each round, and only promotes a patch after the configured conflict policy allows it.

## Architecture

The workspace is split into small crates with one-way dependencies:

```text
crates/
  nerve-cli/       nv binary, clap args, terminal output
  nerve-core/      Synapse state and orchestration loop
  nerve-adapter/   ModelAdapter trait, mock adapter, subprocess adapters
  nerve-config/    nerve.config.json loading and profile matching
  nerve-patch/     NvPatch model, hash validation, apply, rollback
  nerve-types/     Shared task, event, output, verdict, and round types
```

Core flow:

```text
Task
  -> Config profile selection
  -> Lead adapter implementation
  -> Reviewer adapter critique
  -> Optional lead refinement
  -> Conflict policy
  -> Final NvPatch
  -> Dry-run output or --apply
```

## Installation

Requirements:

- Rust stable with edition 2024 support
- `cargo`
- Optional for real adapters: authenticated `claude` and `codex` CLIs available on `PATH`

Build:

```bash
cargo build
```

Run the CLI through Cargo:

```bash
cargo run -p nerve-cli -- config validate
```

Install locally from the workspace:

```bash
cargo install --path crates/nerve-cli
```

After install, the binary is named `nv`.

## Quick Start

Validate the default config:

```bash
cargo run -p nerve-cli -- config validate
```

Run the deterministic mock loop:

```bash
NERVE_ADAPTER=mock cargo run -p nerve-cli -- "add a health endpoint"
```

By default this prints the reviewed diff and does not change files.

Apply the final mock patch:

```bash
NERVE_ADAPTER=mock cargo run -p nerve-cli -- --apply "add a health endpoint"
```

Run with real subprocess adapters:

```bash
cargo run -p nerve-cli -- "rename foo to bar in src/lib.rs"
```

Real mode expects:

- `claude -p "{prompt}" --output-format stream-json`
- `codex exec --json "{prompt}"`

Both tools must already be installed and authenticated by the user.

## Configuration

Nerve loads config in this order:

1. `./nerve.config.json`
2. `~/.config/nerve/config.json`
3. Embedded default config

Example:

```json
{
  "orchestration": {
    "default_strategy": "consensus",
    "max_refinement_rounds": 2,
    "conflict_policy": "lead_priority"
  },
  "roles": {
    "architect": "claude-code",
    "reviewer": "codex"
  },
  "profiles": [
    {
      "id": "blockchain_dev",
      "match_rules": ["*.rs", "*.sol", "contract"],
      "lead": "claude-code",
      "reviewer": "codex",
      "review_strictness": "high"
    },
    {
      "id": "rapid_fix",
      "match_rules": ["fix", "ui"],
      "lead": "codex",
      "reviewer": "claude-code"
    }
  ]
}
```

Profile matching supports:

- Keyword matching against the task prompt
- Glob matching against `Task.context_paths`
- Per-profile lead/reviewer selection
- Per-profile review strictness
- Optional per-profile `max_refinement_rounds`

## Safety Model

Nerve is intentionally conservative:

- Dry-run is the default behavior.
- `--apply` is required for file writes.
- `NvPatch` validates SHA-256 hashes before applying or rolling back.
- Reviewer `BLOCK` can prevent application depending on conflict policy.
- Generated runtime state under `.nerve/` is ignored by Git.

Before accepting model-generated real patches, the next implementation step should add a real unified-diff parser that converts subprocess output into hash-checked `NvPatch` values and rejects unsafe paths.

## Verification

Run the full local verification set:

```bash
cargo test
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo run -p nerve-cli -- config validate
NERVE_ADAPTER=mock cargo run -p nerve-cli -- "add log line to main.rs"
```

Current verified behavior:

- Config loading and profile matching
- Mock lead/reviewer refinement loop
- Patch apply and rollback round trip
- CLI smoke test with dry-run diff output

## Roadmap

Phase 1 completion:

- Parse real subprocess unified diffs into structured `NvPatch`.
- Confine patch paths to the task `cwd`.
- Add real-adapter E2E tests with fixture CLIs.
- Emit clearer machine-readable session reports.

Phase 2:

- Persist Synapse history under `.nerve/`.
- Add `nv history`, `nv resume`, `nv list`, `nv apply <id>`, and `nv rollback <id>`.
- Add an on-disk patch index.
- Add atomic apply with automatic rollback on failure.

Phase 3:

- Real-time cross-firing between lead and reviewer.
- Strategy plugins: `consensus`, `pipeline`, and `tournament`.
- cmux or TUI layout for lead stream, reviewer stream, and orchestrator state.
- Token and cost budgets per session.

## Repository Status

This repository is early-stage and intentionally transparent about what is finished. The mock path is suitable for verifying orchestration and patch safety. The real-agent path is a scaffold for subprocess integration and needs structured patch parsing before it should be used for automatic file changes.

## License

MIT
