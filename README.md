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

Nerve has the Phase 1 MVP plus the planned Phase 2/3 CLI execution features implemented.

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
| Machine-readable session reports | Implemented with `--json` |
| Persistent history / patch index | Implemented under `.nerve/` |
| Strategy dispatch | Implemented for `consensus`, `pipeline`, and `tournament` |
| Real-time cross-firing | Implemented for `.nerve/scratch` watcher feedback |
| Terminal TUI / daemon | Implemented with `--tui` and `daemon` |

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
| `nv --json "<task>"` | Emit a structured session report for downstream tooling |
| `nv --tui "<task>"` | Render a three-pane terminal summary |
| `nv history` | List stored session summaries from `.nerve/sessions/` |
| `nv resume <session-id>` | Print a stored session report |
| `nv list` | List indexed patches from `.nerve/patches/index.json` |
| `nv apply <patch-id>` | Apply a stored patch by id |
| `nv rollback <patch-id>` | Roll back a stored patch by id |
| `nv doctor` | Check config and adapter prerequisites |
| `nv daemon` | Run a line-oriented daemon for editor and shell integrations |
| `nv config validate` | Validate `nerve.config.json` |

Real adapter mode expects these CLIs on `PATH` and already authenticated:

```bash
claude -p "{prompt}" --output-format stream-json --verbose
codex exec --json "{prompt}"
```

Verified real output shapes:

- Claude Code 2.1.128 requires `--verbose` with `stream-json` and emits assistant text under `message.content[].text`.
- Codex CLI 0.128.0 emits assistant text under `item.text` in `item.completed` events.
- Only assistant-authored text fields are considered for unified diff parsing; tool inputs/results and other JSON string fields are ignored.

The subprocess boundary is intentionally CLI-first. Nerve does not depend on a vendor SDK in Phase 1; it treats model tools as external executables and streams their output into the orchestration state.

### Session Reports

Use `--json` when another tool needs to consume the session result:

```bash
NERVE_ADAPTER=mock cargo run -p nerve-cli -- --json "add a health endpoint"
```

The JSON report includes the task, selected profile, round records, final reviewer verdict, final patch, captured agent events, and whether the patch was applied or blocked.

### Stored History And Patches

Every run writes a session report to `.nerve/sessions/{session-id}.json`. Accepted structured patches are written to `.nerve/patches/{patch-id}.json` and indexed in `.nerve/patches/index.json`.

```bash
nv history
nv resume <session-id>
nv list
nv apply <patch-id>
nv rollback <patch-id>
```

Use `--json` with `history`, `resume`, and `list` when another tool needs the stored data.

### Doctor

Use `nv doctor` to validate local prerequisites. In real adapter mode it checks that executable `claude` and `codex` files are available on `PATH`; in mock mode it validates config and the built-in mock adapter path.

### Daemon Mode

Use `nv daemon` for simple editor and shell integrations. It reads one prompt per stdin line, runs the configured loop, stores the report, and writes one compact JSON report per stdout line. Add `--once` to process a single prompt and exit.

### Terminal TUI

Use `--tui` to render the final run as a three-pane terminal summary: Lead, Reviewer, and Orchestrator. This is the built-in fallback layout when a cmux-specific integration is not available.

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
  -> Strategy selection: consensus / pipeline / tournament
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
    "conflict_policy": "lead_priority",
    "max_total_tokens": 200000,
    "max_estimated_cost_microusd": 5000000
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
- Glob rules matched against task context paths collected from prompt path tokens and changed Git paths
- Combined `all` / `any` rule groups
- Per-profile `lead`
- Per-profile `reviewer`
- Per-profile `review_strictness`
- Optional per-profile `max_refinement_rounds`

`match_rules` accepts the original shorthand array or a logical rule object:

```json
{ "all": ["*.rs", "contract"], "any": ["audit", "security"] }
```

### Conflict Policies

The config schema includes:

| Policy | Phase 1 behavior |
|--------|------------------|
| `lead_priority` | Prefer the lead patch |
| `reviewer_priority` | Prefer reviewer suggested patch when present |
| `merge_attempt` | Merge lead and reviewer patches with `git merge-file` when both touch the same file |
| `abort_on_conflict` | Block apply unless the reviewer returns `LGTM` |
| `reviewer_block` | Block on reviewer `BLOCK` |
| `manual` | Always block auto-apply and leave the patch for manual handling |

### Session Budgets

Optional orchestration budgets stop refinement and prevent apply when reported model usage exceeds a configured ceiling:

- `max_total_tokens`: maximum input plus output tokens for the session.
- `max_estimated_cost_microusd`: maximum estimated session cost in micro-USD.

Adapters that do not report usage leave these counters at zero; budget enforcement applies when usage is available.

### Strategies

`orchestration.default_strategy` controls the core execution mode:

| Strategy | Behavior |
|----------|----------|
| `consensus` | Lead implements, reviewer critiques, and lead refines until `LGTM` or the round limit. |
| `pipeline` | Lead implements once and reviewer critiques once; no refinement loop runs. |
| `tournament` | Lead and reviewer both generate candidate outputs, cross-review each other, and Nerve selects the accepted candidate. |

`max_refinement_rounds` counts lead refinement attempts. A consensus run may therefore perform one initial review plus one review after each allowed refinement.

## Safety Model

Nerve is built around conservative file mutation:

- Dry-run is the default behavior.
- `--apply` is required for file writes.
- `NvPatch` validates the current file SHA-256 before applying.
- `NvPatch` validates the modified file SHA-256 before rollback.
- `NvPatch` rejects absolute paths, `..` traversal, and symlinked directories that resolve outside the working directory.
- File writes are staged through sibling temp files and committed with rename.
- Stored JSON writes use unique same-directory temp files, and patch index updates are serialized with a lock file.
- Multi-file apply captures pre-apply snapshots and restores them automatically if any file operation fails.
- Created files are removed during rollback.
- Deleted files are restored from the original content during rollback.
- Reviewer `BLOCK` can prevent application depending on conflict policy.
- Generated runtime state under `.nerve/` is ignored by Git.

Runtime persistence and patch indexing are implemented under `.nerve/`.

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
- Temp-file staged writes and cleanup on commit failure
- Created-file rollback removal and deleted-file rollback restore
- Pure rename and rename-with-content-change apply/rollback
- Unsafe path rejection for traversal and symlink escapes
- Real adapter raw text / JSONL diff extraction
- Fenced Claude JSONL diff extraction
- Fixture-based real adapter CLI dry-run and apply paths
- CLI smoke test with dry-run diff output
- CLI JSON report output
- Persistent session history and patch index commands
- Doctor checks for config and adapter prerequisites
- Token and estimated-cost budget enforcement when adapter usage is reported
- Strategy dispatch for `consensus`, `pipeline`, and `tournament`
- Profile `all` / `any` match rule groups
- `merge_attempt` conflict policy patch merging
- Scratch-file crossfire watcher feedback during lead execution
- Line-oriented `nv daemon` mode for editor and shell integrations
- Three-pane terminal TUI summary

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

### Phase 1

- Phase 1 MVP is implemented, including machine-readable session reports.

### Phase 2

- Persisted session history and `.nerve/patches/index.json` are implemented.
- `nv history`, `nv resume`, `nv list`, `nv apply <id>`, and `nv rollback <id>` are implemented.
- Temp-file based write staging is implemented for patch file writes.
- Token and estimated-cost budgets are implemented for adapters that report usage.
- `nv doctor` checks config validity and real adapter binaries.
- Scratch-file crossfire feedback is implemented for lead execution.

### Phase 3

- Real-time scratch-file cross-firing between lead and reviewer is implemented.
- Profile `all` / `any` match rule groups are implemented.
- Strategy dispatch for `consensus`, `pipeline`, and `tournament` is implemented.
- Terminal TUI layout for lead, reviewer, and orchestrator state is implemented.
- Long-running daemon mode for editor and shell integrations is implemented.

## Design Documents

- [Architecture](./nerve-architecture.md)
- [Implementation Plan](./nerve-implementation-plan.md)

## License

MIT
