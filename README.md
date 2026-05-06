<p align="center">
  <h1 align="center">Nerve</h1>
  <p align="center">
    <strong>Reflexive AI orchestration for coding agents - one lead, one reviewer, one auditable patch.</strong>
  </p>
  <p align="center">
    <a href="#quick-start">Quick Start</a> &middot;
    <a href="#agent-loop">Agent Loop</a> &middot;
    <a href="#architecture">Architecture</a> &middot;
    <a href="#configuration">Configuration</a> &middot;
    <a href="#roadmap">Roadmap</a>
  </p>
</p>

---

Nerve is a Rust CLI that routes a coding task through a **lead/reviewer loop**. The lead agent proposes an implementation, the reviewer critiques it, and the orchestrator keeps refining until the configured policy accepts a final patch.

It is designed for agent pairs like Claude Code and Codex, but the core abstraction is generic: any model or tool that can implement the `ModelAdapter` trait can participate.

```text
User asks for a code change
  -> nv "add a /health endpoint"
  -> Lead agent proposes a patch
  -> Reviewer returns LGTM / REQUEST_CHANGES / BLOCK
  -> Lead refines when needed
  -> Nerve prints the reviewed diff
  -> Files change only when --apply is passed
```

## Why Nerve?

Single-agent coding has a predictable failure mode: the same model that writes a patch often rationalizes it. Nerve makes review a first-class execution step instead of a final afterthought.

| Problem | Traditional Agent Flow | Nerve |
|---------|------------------------|-------|
| Patch quality | One model writes and self-justifies | Separate lead and reviewer roles |
| Review loop | Manual, ad hoc, often skipped | Built into the orchestrator |
| File writes | Agent may edit directly | Dry-run by default, `--apply` required |
| Patch safety | Trust the generated diff | SHA-256 checked `NvPatch` apply/rollback |
| Role routing | One prompt fits all | Config profiles choose lead/reviewer per task |
| Auditability | Lost terminal scrollback | Round records and event stream in `Synapse` |

## Current Status

Nerve is in a Phase 1 MVP state.

| Area | Status |
|------|--------|
| Cargo workspace | Implemented |
| `nv` CLI | Implemented |
| Config loading and profile matching | Implemented |
| Mock lead/reviewer loop | Implemented and tested |
| Hash-checked patch apply/rollback | Implemented and tested |
| Claude/Codex subprocess boundary | Scaffolded |
| Real CLI JSON parsing | Generic JSONL string extraction |
| Real unified-diff to `NvPatch` conversion | Implemented for create/modify/delete/rename diffs |
| Persistent history / patch index | Roadmap |
| Real-time cross-firing / TUI | Roadmap |

Important: the mock adapter path produces structured `NvPatch` values directly. The real subprocess path now extracts unified diffs from raw text or JSONL string fields and converts create/modify/delete/rename diffs into safe `NvPatch` values.

## Quick Start

### Build

```bash
git clone https://github.com/kooroot/Nerve.git
cd Nerve
cargo build
```

### Validate config

```bash
cargo run -p nerve-cli -- config validate
```

### Run the deterministic mock loop

```bash
NERVE_ADAPTER=mock cargo run -p nerve-cli -- "add a health endpoint"
```

Expected behavior:

- Nerve creates a session.
- The mock lead proposes an initial patch.
- The mock reviewer requests one refinement.
- The lead refines.
- The reviewer returns `LGTM`.
- A unified diff is printed.
- No files are changed.

### Apply a reviewed mock patch

```bash
NERVE_ADAPTER=mock cargo run -p nerve-cli -- --apply "add a health endpoint"
```

### Install the CLI locally

```bash
cargo install --path crates/nerve-cli
nv config validate
NERVE_ADAPTER=mock nv "add a health endpoint"
```

## Agent Loop

Nerve exposes one CLI today:

| Command | Purpose |
|---------|---------|
| `nv "<task>"` | Run the lead/reviewer orchestration loop |
| `nv --apply "<task>"` | Apply the accepted structured patch |
| `nv --adapter mock "<task>"` | Use deterministic local mock adapters |
| `nv --adapter real "<task>"` | Spawn real `claude` and `codex` subprocesses |
| `nv config validate` | Validate `nerve.config.json` |

Real adapter mode expects these CLIs on `PATH` and already authenticated:

```bash
claude -p "{prompt}" --output-format stream-json --verbose
codex exec --json "{prompt}"
```

Verified real output shapes:

- Claude Code 2.1.128 requires `--verbose` with `stream-json` and emits assistant text under `message.content[].text`.
- Codex CLI 0.128.0 emits assistant text under `item.text` in `item.completed` events.
- Both shapes are parsed through generic JSON string extraction before unified diff parsing.

The subprocess boundary is intentionally CLI-first. Nerve does not depend on a vendor SDK in Phase 1; it treats model tools as external executables and streams their output into the orchestration state.

## Architecture

```text
                         +-------------------------+
                         |        nv CLI           |
                         | prompt / config / apply |
                         +-----------+-------------+
                                     |
                         +-----------v-------------+
                         |      nerve-core         |
                         | Synapse + orchestrator  |
                         +-----------+-------------+
                                     |
          +--------------------------+--------------------------+
          |                          |                          |
 +--------v---------+       +--------v---------+       +--------v---------+
 |  nerve-config    |       | nerve-adapter    |       |  nerve-patch    |
 | profiles + rules |       | mock/subprocess  |       | NvPatch safety  |
 +--------+---------+       +--------+---------+       +--------+---------+
          |                          |                          |
          +--------------------------+--------------------------+
                                     |
                         +-----------v-------------+
                         |      nerve-types        |
                         | Task / Event / Verdict  |
                         +-------------------------+
```

Workspace layout:

```text
crates/
  nerve-cli/       `nv` binary, clap args, terminal output
  nerve-core/      Synapse state and refinement loop
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

## Configuration

Nerve loads configuration in this order:

1. `./nerve.config.json`
2. `~/.config/nerve/config.json`
3. Embedded default config

Default shape:

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

### Profile Matching

Profiles can route work by:

- Keyword rules matched against the task prompt
- Glob rules matched against task context paths
- Per-profile `lead`
- Per-profile `reviewer`
- Per-profile `review_strictness`
- Optional per-profile `max_refinement_rounds`

### Conflict Policies

The config schema includes:

| Policy | Phase 1 behavior |
|--------|------------------|
| `lead_priority` | Prefer the lead patch |
| `reviewer_priority` | Prefer reviewer suggested patch when present |
| `merge_attempt` | Accepted by config, full merge behavior is roadmap |
| `abort_on_conflict` | Block on reviewer `BLOCK` |
| `reviewer_block` | Block on reviewer `BLOCK` |
| `manual` | Block on reviewer `BLOCK` |

## Safety Model

Nerve is built around conservative file mutation:

- Dry-run is the default behavior.
- `--apply` is required for file writes.
- `NvPatch` validates the current file SHA-256 before applying.
- `NvPatch` validates the modified file SHA-256 before rollback.
- `NvPatch` rejects absolute paths, `..` traversal, and symlinked directories that resolve outside the working directory.
- Multi-file apply captures pre-apply snapshots and restores them automatically if any file operation fails.
- Created files are removed during rollback.
- Deleted files are restored from the original content during rollback.
- Reviewer `BLOCK` can prevent application depending on conflict policy.
- Generated runtime state under `.nerve/` is ignored by Git.

The next core task is machine-readable session reports for downstream tooling.

## Development

```bash
cargo test
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo run -p nerve-cli -- config validate
NERVE_ADAPTER=mock cargo run -p nerve-cli -- "add log line to main.rs"
```

Current test coverage verifies:

- Default config loading
- Keyword and glob profile matching
- Mock lead/reviewer refinement until `LGTM`
- Patch apply and rollback round trip
- Hash mismatch rejection
- Unified diff parsing for existing, new, deleted, and renamed files
- Atomic multi-file apply rollback on mid-apply failure
- Created-file rollback removal and deleted-file rollback restore
- Pure rename and rename-with-content-change apply/rollback
- Unsafe path rejection for traversal and symlink escapes
- Real adapter raw text / JSONL diff extraction
- Fenced Claude JSONL diff extraction
- Fixture-based real adapter CLI dry-run and apply paths
- CLI smoke test with dry-run diff output

## How It Works

```text
User: "add a health endpoint"

1. Config::select_profile(task)
   -> choose lead, reviewer, strictness, max rounds

2. lead.implement(task)
   -> AgentOutput { raw_text, proposed_patch }

3. reviewer.review(task, lead_output)
   -> ReviewerFeedback { verdict, issues, suggested_patch }

4. If verdict is REQUEST_CHANGES and rounds remain:
   lead.refine(task, previous_output, feedback)

5. Fusion:
   select_final_patch(lead_output, reviewer_feedback, conflict_policy)

6. Output:
   - print reviewed diff
   - apply only when --apply and policy allows it
```

## Roadmap

### Phase 1 Completion

- Emit machine-readable session reports.

### Phase 2

- Persist Synapse history under `.nerve/`.
- Add `.nerve/patches/index.json`.
- Add `nv history`, `nv resume`, `nv list`, `nv apply <id>`, and `nv rollback <id>`.
- Add temp-file based write staging for stronger crash safety.
- Add token and cost budgets per session.

### Phase 3

- Real-time cross-firing between lead and reviewer.
- Strategy plugins: `consensus`, `pipeline`, and `tournament`.
- cmux or TUI layout for lead stream, reviewer stream, and orchestrator state.
- Long-running daemon mode for editor and shell integrations.

## Design Documents

- [Architecture](./nerve-architecture.md)
- [Implementation Plan](./nerve-implementation-plan.md)

## License

MIT
