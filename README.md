<p align="center">
  <h1 align="center">Nerve</h1>
  <p align="center">
    <strong>Reflexive AI orchestration for coding agents - one lead, one reviewer, one auditable patch.</strong>
  </p>
  <p align="center">
    <a href="#quick-start">Quick Start</a> &middot;
    <a href="#agent-loop">Agent Loop</a> &middot;
    <a href="#plan-goal-and-budget">Plan, Goal, Budget</a> &middot;
    <a href="#forks-mcp-and-mayorpatrol">Forks, MCP, Mayor/Patrol</a> &middot;
    <a href="#architecture">Architecture</a> &middot;
    <a href="#configuration">Configuration</a>
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

Nerve v1.0.0 has the original lead/reviewer patch loop plus the terminal-control, goal, budget, planning, RPC, fork, MCP, and Mayor/Patrol surfaces implemented.

| Area | Status |
|------|--------|
| Cargo workspace | Implemented |
| `nv` CLI | Implemented |
| Config loading and profile matching | Implemented |
| Mock lead/reviewer loop | Implemented and tested |
| Hash-checked patch apply/rollback | Implemented and tested |
| Claude/Codex subprocess boundary | Implemented with JSONL text, usage, and suggested-patch extraction |
| Real CLI JSON parsing | Generic JSONL string extraction plus adapter completion events |
| Real unified-diff to `NvPatch` conversion | Implemented for create/modify/delete/rename diffs |
| Machine-readable session reports | Implemented with `--json` |
| Persistent history / patch index | Implemented under `.nerve/` |
| Session names and linked follow-up runs | Implemented under `.nerve/session-meta/` |
| Strategy dispatch | Implemented for `consensus`, `pipeline`, and `tournament` |
| Real-time cross-firing | Implemented for `.nerve/scratch` watcher feedback |
| Prompt templates | Implemented with `nv template` |
| Terminal product UX | Implemented with `nv`, `nv interactive`, `--tui`, `daemon`, and `daemon --rpc` |
| Pi workflow benchmark | Implemented with `nv benchmark pi` |
| Goal mode | Implemented with interactive `/goal`, deterministic checks, NL conversion, persistence, timeout/output caps, no-progress guard, and active-goal reload |
| Budget controls | Implemented with interactive `/budget`, global ceilings, raise confirmation, budget audit hash-chain, and doctor validation |
| Worktree-isolated apply | Implemented with config plus `--worktree` / `--no-worktree` overrides |
| RPC envelope lifecycle | Implemented with typed JSONL envelopes, bearer-token lifecycle, payload caps, and metadata |
| Read-only plan mode | Implemented with `nv plan`, `/plan`, dual review, and patch-artifact rejection |
| Ratatui TUI | Implemented through `nerve-tui` |
| Session forks | Implemented with `nv fork`, `nv branch`, and `nv sessions` |
| MCP client | Implemented with `nv mcp` and interactive `/mcp` |
| Mayor/Patrol queue | Implemented with `nv mayor`, `nv patrol`, queue files, heartbeat/orphan recovery, and budget ceilings |

Important: the mock adapter path produces structured `NvPatch` values directly. The real subprocess path extracts unified diffs from raw text or JSONL string fields, converts create/modify/delete/rename diffs into safe `NvPatch` values, and attaches reported usage when provider JSONL includes token or cost fields.

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

### Install the CLI

Install the latest prebuilt binary with one command:

```bash
curl -fsSL https://github.com/kooroot/Nerve/releases/latest/download/install.sh | sh
```

The installer detects macOS/Linux and CPU architecture, downloads the matching release asset, and installs `nv` into a standard user-writable CLI directory such as `/opt/homebrew/bin`, `/usr/local/bin`, or `~/.local/bin`. To choose another directory:

```bash
curl -fsSL https://github.com/kooroot/Nerve/releases/latest/download/install.sh | NERVE_INSTALL_DIR=/usr/local/bin sh
```

On Windows PowerShell:

```powershell
irm https://github.com/kooroot/Nerve/releases/latest/download/install.ps1 | iex
```

Windows installs to `%LOCALAPPDATA%\Programs\Nerve` and adds that directory to the user `PATH`.

Manual downloads are also available from the latest GitHub release:

| Platform | Release asset |
|----------|---------------|
| macOS Apple Silicon | `nerve-aarch64-apple-darwin.tar.gz` |
| macOS Intel | `nerve-x86_64-apple-darwin.tar.gz` |
| Linux x64 | `nerve-x86_64-unknown-linux-gnu.tar.gz` |
| Windows x64 | `nerve-x86_64-pc-windows-msvc.zip` |

Or install from source with Cargo:

```bash
cargo install --path crates/nerve-cli
nv config validate
NERVE_ADAPTER=mock nv "add a health endpoint"
```

From GitHub:

```bash
cargo install --git https://github.com/kooroot/Nerve.git nerve-cli
nv config validate
```

After installation, verify which binary your shell will run and then check local prerequisites:

```bash
which -a nv
nv --version
nv setup
```

If `which -a nv` shows an older path before the newly installed binary, your shell will keep running the old CLI. Remove the stale earlier binary or put the new install directory earlier in `PATH`, then run `hash -r` in zsh. The old prompt `Nerve interactive. Type /help for commands.` means the shell is still resolving an older `nv`; the current terminal workspace starts with `Nerve Terminal` and a `nerve:<adapter>:<mode>` prompt.

## Agent Loop

Nerve exposes one CLI binary, `nv`.

| Command | Purpose |
|---------|---------|
| `nv "<task>"` | Run the lead/reviewer orchestration loop |
| `nv --apply "<task>"` | Apply the accepted structured patch |
| `nv --adapter mock "<task>"` | Use deterministic local mock adapters |
| `nv --adapter real "<task>"` | Spawn real `claude` and `codex` subprocesses |
| `nv --json "<task>"` | Emit a structured session report for downstream tooling |
| `nv --tui "<task>"` | Render a three-pane terminal summary |
| `nv --worktree "<task>"` | Force worktree-isolated apply for this run |
| `nv --no-worktree "<task>"` | Force legacy in-place apply for this run |
| `nv` | Start the terminal workspace when run from a terminal |
| `nv interactive` | Force the terminal workspace, useful for scripts and tests |
| `nv setup` | Initialize `.nerve/` and check config plus adapter prerequisites |
| `nv login [all\|claude\|codex]` | Start provider login using Claude Code and/or Codex CLI subscriptions |
| `nv plan [--dual-review] "<task>"` | Produce a read-only structured plan; never applies or emits a patch |
| `nv history` | List stored session summaries from `.nerve/sessions/` |
| `nv history --applied --blocked --named` | Filter stored session summaries |
| `nv resume <session-id>` | Print a stored session report |
| `nv name <session-id> <name>` | Attach a human-readable name to a session |
| `nv rerun <session-id> "<task>"` | Run a follow-up task linked to an existing session |
| `nv list` | List indexed patches from `.nerve/patches/index.json` |
| `nv apply <patch-id>` | Apply a stored patch by id |
| `nv rollback <patch-id>` | Roll back a stored patch by id |
| `nv doctor` | Check config and adapter prerequisites |
| `nv template list` | List configured prompt templates |
| `nv template run <template-id> [args...]` | Run a configured prompt template |
| `nv daemon` | Run a line-oriented daemon for editor and shell integrations |
| `nv daemon --rpc` | Run a JSONL RPC daemon with lifecycle events |
| `nv daemon --rpc --print-token` | Start RPC mode and print a typed `rpc.token` envelope |
| `nv rpc rotate-token` | Rotate the RPC bearer token under `.nerve/session-meta/` |
| `nv fork <session-id>` | Fork a stored session into a child branch |
| `nv branch <session-id>` | Create a child session using branch defaults |
| `nv sessions list` | List registered session roots |
| `nv sessions tree <root-id>` | Print a fork tree |
| `nv mcp list-tools` | Start configured MCP servers and list advertised tools |
| `nv mcp probe <server>` | Handshake one configured MCP server and print its tools |
| `nv mayor --status-only` | Show Mayor queue depth without dispatching |
| `nv mayor --ledger` | Print the non-authoritative coordination ledger and exit |
| `nv mayor --reconcile` | Rebuild the ledger from the queue directories (dirs win), print it, and exit |
| `nv patrol --id <slot> --status` | Show a patrol slot's token/worktree/status view |
| `nv patrol --id <slot> --once` | Claim and run one queued patrol task |
| `nv benchmark pi` | Run the deterministic Pi-inspired workflow benchmark |
| `nv config validate` | Validate `nerve.config.json` |

Real adapter mode expects these CLIs on `PATH` and already authenticated:

```bash
claude -p "{prompt}" --output-format stream-json --verbose
codex exec --skip-git-repo-check --json "{prompt}"
```

Use `nv login` or interactive `/login` to start the provider subscription login flows from Nerve.

### Terminal Workspace

Run `nv` with no prompt to open the Nerve terminal workspace. It keeps the last reviewed patch in memory so the next command can inspect or apply it without copying ids:

```text
nerve:real:dry-run:main> fix the auth callback bug
nerve:real:dry-run:main patch=abc12345> /diff
nerve:real:dry-run:main patch=abc12345> /apply
```

Available workspace commands:

```text
/paste
!<command>
/login
/doctor
/status
/mode <dry-run|apply>
/adapter <real|mock>
/cd <path>
/pwd
/clear
/history
/resume <session-id>
/list
/templates
/template <template-id> [args...]
/benchmark pi [iterations]
/goal <argv...>
/goal :nl <natural language condition>
/goal show
/goal clear
/budget show
/budget cost=$5
/budget tokens=200000
/plan <task>
/fork [--from-round N] [--name NAME]
/mcp list
/mcp call <server> <tool> <json>
/mayor status
/patrol status
/diff
/apply [patch-id]
/rollback [patch-id]
/quit
```

When `nv` is attached to a real terminal, it uses an interactive line editor:

- Type `/` to open the command palette.
- Use Up/Down to move through command suggestions, or to navigate prompt history when the palette is closed.
- Press Tab or Right to complete the selected slash command.

Task errors do not close the workspace; use `/login` for provider auth, `/doctor` for setup checks, and `NERVE_ADAPTER=mock nv` for a local smoke test.

Verified real output shapes:

- Claude Code 2.1.128 requires `--verbose` with `stream-json` and emits assistant text under `message.content[].text`.
- Codex CLI 0.128.0 emits assistant text under `item.text` in `item.completed` events.
- Only assistant-authored text fields are considered for unified diff parsing; tool inputs/results and other JSON string fields are ignored.

The subprocess boundary is intentionally CLI-first. Nerve does not depend on a vendor SDK; it treats model tools as external executables and streams their output into the orchestration state.

### Session Reports

Use `--json` when another tool needs to consume the session result:

```bash
NERVE_ADAPTER=mock cargo run -p nerve-cli -- --json "add a health endpoint"
```

The JSON report includes the task, selected profile, round records, final reviewer verdict, final patch, captured agent events, and whether the patch was applied or blocked.

The report also carries `ran_unconfined`: a machine-readable signal that is `true` only when a goal check actually ran *without* OS sandbox confinement because `sandbox.mode = auto` requested a sandbox but no backend was available on this host. This makes the two non-`off` postures observable: `auto` is *confine-if-possible, else run openly* — it degrades to an unconfined run and records `ran_unconfined: true` rather than failing — whereas `required` *fails closed*, refusing to run the check at all when no backend is available (so a `required` run never reports an unconfined execution). The field is pure telemetry: it never changes the deterministic acceptance verdict (`blocked` / `goal_satisfied`); it exists so an operator or downstream tool can detect that a run under `auto` silently lost confinement. It is `false` for `off` (unconfined by configuration, not a degrade), for any backend-confined run, and for a check that never executed; reports written before the field existed default it to `false`.

### Stored History And Patches

Every run writes a session report to `.nerve/sessions/{session-id}.json`. Accepted structured patches are written to `.nerve/patches/{patch-id}.json` and indexed in `.nerve/patches/index.json`.

```bash
nv history
nv history --named
nv resume <session-id>
nv name <session-id> "health endpoint"
nv rerun <session-id> "add tests for the endpoint"
nv list
nv apply <patch-id>
nv rollback <patch-id>
```

Use `--json` with `history`, `resume`, and `list` when another tool needs the stored data.

Session names and parent links are stored under `.nerve/session-meta/` so older session reports remain readable.

### Prompt Templates

Prompt templates live in `nerve.config.json` and substitute `{{args}}` with the arguments passed on the CLI:

```bash
nv template list
nv template run security-audit crates/nerve-core/src/lib.rs
```

### Doctor

Use `nv doctor` to validate local prerequisites. In real adapter mode it checks that executable `claude` and `codex` files are available on `PATH`; in mock mode it validates config and the built-in mock adapter path.

### Daemon Mode

Use `nv daemon` for simple editor and shell integrations. It reads one prompt per stdin line, runs the configured loop, stores the report, and writes one compact JSON report per stdout line. Add `--once` to process a single prompt and exit.

Use `nv daemon --rpc` when integrations need structured JSONL commands and lifecycle events:

```bash
printf '%s\n' '{"command":"prompt","prompt":"add a health endpoint"}' | nv daemon --rpc --once
printf '%s\n' '{"command":"plan","prompt":"map the migration"}' | nv daemon --rpc --once
```

Supported RPC commands are `prompt`, `plan`, `get_state`, `history`, `resume`, `list_patches`, `apply_patch`, and `rollback_patch`. Prompt runs emit typed envelopes such as `session.started`, `round.started`, `lead.stdout_chunk`, `reviewer.stdout_chunk`, `budget.changed`, `patch.applied`, `patch.discarded`, and `session.ended`. Plan runs emit `plan.proposed`.

Use `--print-token` to print a typed `rpc.token` envelope at daemon startup:

```bash
nv daemon --rpc --print-token
```

### Pi Benchmark

Use `nv benchmark pi` to run a deterministic local benchmark of the Pi-inspired workflow. It uses the mock adapter by default and checks config loading, store initialization, the lead/reviewer loop, structured patch creation, apply, rollback, history, and patch indexing. Add `--json` for machine-readable output or `--live` to exercise authenticated Claude/Codex subprocesses in a temporary workspace.

### Terminal TUI

Use `--tui` to render the final run as a three-pane terminal summary: Lead, Reviewer, and Orchestrator. This is the built-in fallback layout when a cmux-specific integration is not available.

## Plan, Goal, And Budget

### Plan Mode

Use `nv plan` when you want read-only analysis before allowing implementation. The adapter must return structured Markdown with the required sections, and Nerve rejects patch artifacts such as `NvPatch`, `diff --git`, or fenced diff blocks.

```bash
nv --adapter mock plan "split the CLI RPC code into smaller modules"
nv plan --dual-review "audit the new MCP client lifecycle"
```

Plan mode supports profile-level `plan_strategy` and `plan_system_prompt_override` settings. `--dual-review` forces a reviewer pass over the generated plan.

### Goal Mode

Goal mode is interactive and lives in the terminal workspace. It registers a deterministic stop check that runs at round boundaries.

```text
nerve:real:dry-run:main> /goal --timeout 60 cargo test --workspace
nerve:real:dry-run:main> /goal show
nerve:real:dry-run:main> fix the flaky test until the workspace passes
nerve:real:dry-run:main> /goal clear
```

Natural-language goal conversion is also available, but only in an interactive terminal because Nerve shows the proposed command and requires confirmation before persisting it:

```text
/goal :nl all workspace tests pass
/goal "the CLI smoke test succeeds"
```

Goal checks are argv-based, not shell strings. Nerve rejects unsafe programs, freezes the workspace cwd, applies timeout/output caps, can apply configured ulimits, and stores the active goal under `.nerve/session-meta/active-goal.json`.

Goal `env` overrides are screened at validation — the deterministic chokepoint shared by the natural-language converter, the argv form, and the persisted active-goal reload — so neither a model proposal nor a repo-local `.nerve/session-meta/active-goal.json` can smuggle one in. Keys that are known process-level code-execution vectors — the dynamic-linker tunables (`LD_*`, `DYLD_*`), `PATH`, compiler/shell/interpreter hooks (`RUSTC_WRAPPER`, `BASH_ENV`, `IFS`, `GIT_SSH_COMMAND`, `NODE_OPTIONS`, `PYTHONPATH`, …), and values carrying control characters are rejected outright (fail-closed). For the converter path, every surviving override is then listed line-by-line in the confirmation prompt so it cannot ride along unseen; the interactive confirmation remains the final human gate. This is defense-in-depth, not an exhaustive capability model. Security-sensitive process env (e.g. `PATH`) should be supplied through the operator-controlled `orchestration.check_env` allowlist: because such keys are on the denylist they can never be carried by goal `env` at all. For a *non-denied* key, a goal `env` entry is still applied after — and so overrides — the inherited `check_env` value at execution, which is exactly why every surviving override is surfaced in the confirmation prompt for the operator to approve.

### Budget Controls

Use `/budget` to set per-session token or cost caps during an interactive run:

```text
/budget show
/budget cost=$5
/budget tokens=200000
/budget cost=$10 --force
```

Budget changes are appended to `.nerve/session-meta/budget-audit.json` as a hash chain. `nv doctor` validates the chain: a break means the log no longer verifies (it was edited after it was written, or — if you just configured a key — it predates the key). By default the chain is unkeyed SHA-256, so it catches accidental edits and naive tampering but not a determined local writer (who can recompute a fully valid chain). Set `NERVE_BUDGET_AUDIT_KEY` (or point `NERVE_BUDGET_AUDIT_KEY_FILE` at a key file via an **absolute** path) — kept OFF the host you are defending against — to key the chain with HMAC-SHA-256, so a non-key-holder cannot forge or edit a link. Keyed verification is strict and also authenticates the **tail**: each keyed entry stores a self-MAC, so even the most-recent budget change (which no successor links to yet) cannot be edited undetected. A downgrade to an unkeyed/pre-key chain is detected — keying over **any** pre-existing unkeyed log fails loudly (only an empty log can begin a keyed chain; archive or re-key an existing one). A misconfigured key (a set-but-non-UTF-8 env value, a relative key-file path, or an unreadable/empty key file) also fails closed rather than silently running unkeyed, and `nv doctor` reports it as a failure. The residual gap even when keyed: any writer can **truncate** the log to an earlier keyed prefix (rollback), and a key-holder can rewrite history — so an intact chain is not proof of authenticity. The key is read only from the operator environment, never from repo-local config. Global ceilings can be set in config so an interactive budget raise cannot exceed operator policy.

## Forks, MCP, And Mayor/Patrol

### Session Forks

Forks create child session records without rewriting the parent. They are useful for trying a follow-up path from a reviewed session while keeping the original report readable.

```bash
nv fork <session-id> --from-round 1 --name retry_tests
nv branch <session-id>
nv sessions list
nv sessions tree <root-id>
```

Fork payloads live under `.nerve/sessions/` with an index guarded by an advisory lock. Legacy stored sessions are bootstrapped into the fork index with their round history when first forked.

### MCP Tools

MCP support is configured in `roles.mcp` or `profiles[].mcp`. Nerve currently supports stdio MCP servers.

```bash
nv mcp list-tools
nv mcp probe docs
```

From the terminal workspace:

```text
/mcp list
/mcp call docs search '{"query":"release workflow"}'
```

MCP servers default to `read_only: true`. Admission is **deny-by-default**: a tool is callable only if it is in the server's `allowed_tools` (the hard boundary) or its MCP annotation reports `readOnlyHint: true` / `destructiveHint: false`; an unrecognized tool fails closed. The `write_tool_patterns` blacklist (`shell`, `exec`, `fs.write`, `write_file`, …) is then applied as a final veto. A global `mcp.allow_tools` list is intersected with each server's own `allowed_tools`.

Annotation trust assumes a **semi-trusted** server — a hostile server can lie in `readOnlyHint`, so `allowed_tools` is the only hard guarantee. To restore the legacy substring-only blacklist (which fails *open* on unrecognized mutating tool names) set `mcp.read_only_posture: "legacy_denylist"`; this weaker posture is honored only from your own (`~/.config`) config — a repo-local `nerve.config.json` requesting it is ignored (and warns) unless you set `NERVE_TRUST_PROJECT_VERIFIER=1`, so a cloned repo can never silently weaken your write posture. This guard governs MCP dispatch only; it does not affect the deterministic acceptance gate.

Name gating decides *which* tool runs, not *what arguments* it gets. As optional defense-in-depth, `mcp.argument_policy` constrains the arguments of named tools:

```jsonc
"argument_policy": {
  "tools": {
    "read_file": { "path_args": ["path"] },        // must resolve inside the project root
    "run_query": { "deny_substrings": { "sql": [";", "drop "] } }  // case-insensitive
  }
}
```

`path_args` are checked **lexically** against the project root (the cwd Nerve loaded config from): an absolute path outside it, or a relative path whose `..` climbs above it — even one that later re-enters via matching components (`../../<root-tail>/x`) — is rejected, with no filesystem access, so it is TOCTOU-free and works for not-yet-created paths. Matching uses native filesystem-path semantics: it does **not** resolve symlinks (a symlink *inside* the root pointing outward is not caught) and does **not** parse URIs (a `file:///etc/passwd` value is treated as the relative filename `file:/etc/passwd` and confined under the root) — so if you name a `uri`-style key as a `path_arg` and the server resolves it as a URI, this lexical check won't confine it. It is hardening on top of name gating, not a complete capability sandbox. A rule only inspects arguments the call actually supplies *as strings* — an argument that is absent, or whose value is not a string (e.g. a number or array), is **not** checked by that rule (the one exception: a declared `path_args` rule with no resolvable root fails **closed**). This policy is strictly **monotone-restrictive** — an entry can only ever *reject* a call, never admit one name gating refused — so, unlike `read_only_posture`, it needs no provenance/consent gate: a repo-local config that enables it cannot broaden your tool access. A tool with no entry is unconstrained (byte-for-byte the pre-policy behavior), and a misspelled rule key is a loud config error, never a silently-inert rule. Like the other MCP guards it governs dispatch only and never touches the deterministic acceptance gate.

### Mayor/Patrol

Mayor/Patrol is the v1.0 multi-instance queue primitive. The Mayor owns queue state on disk; Patrol workers atomically claim tasks, write results, and heartbeat for orphan recovery.

```bash
nv mayor --status-only
nv mayor --queue-dir .nerve/queue --results-dir .nerve/results
nv patrol --id slot-1 --status
nv patrol --id slot-1 --once
```

Queue state lives under `.nerve/queue/{pending,claimed,done,failed,heartbeat}/`; results live under `.nerve/results/`. Task IDs and patrol IDs are validated as safe filename components, duplicate task IDs are rejected, and stale claimed tasks can be recovered when heartbeats exceed the configured TTL.

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
  nerve-core/      Synapse, refinement loop, goals, RPC, forks, Mayor/Patrol
  nerve-adapter/   ModelAdapter trait, mock/subprocess adapters, MCP stdio client
  nerve-config/    nerve.config.json loading and profile matching
  nerve-patch/     NvPatch model, hash validation, apply, rollback
  nerve-tui/       Ratatui terminal summary widgets
  nerve-types/     Shared task, event, output, verdict, and round types
```

Core flow:

```text
Task
  -> Config profile selection
  -> Strategy selection: consensus / pipeline / tournament
  -> Lead adapter implementation
  -> Reviewer adapter critique
  -> Optional goal check / budget gate
  -> Optional lead refinement
  -> Conflict policy
  -> Final NvPatch
  -> Dry-run output, --apply, or worktree-isolated apply
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
  ],
  "templates": [
    {
      "id": "security-audit",
      "description": "Review a target path for correctness, safety, and missing tests.",
      "prompt": "audit {{args}} for correctness, safety, and missing tests"
    }
  ],
  "ui": {
    "default_mode": "print"
  },
  "daemon": {
    "protocol": "line"
  }
}
```

Optional v1.0 sections can be added only when needed:

```json
{
  "orchestration": {
    "worktree_apply": true,
    "check_ulimit": {
      "nproc": 64,
      "memory_bytes": 2147483648,
      "file_size_bytes": 104857600,
      "cpu_secs": 120
    },
    "budget_cost_microusd_ceiling": 5000000,
    "budget_tokens_ceiling": 200000,
    "mayor_patrol": {
      "queue_dir": ".nerve/queue",
      "results_dir": ".nerve/results",
      "max_patrols": 8,
      "heartbeat_secs": 30,
      "claim_ttl_secs": 600
    }
  },
  "roles": {
    "architect": "claude-code",
    "reviewer": "codex",
    "plan_strategy": "single",
    "fork": {
      "copy_patch_history": true,
      "auto_name": false
    },
    "mcp": {
      "allow_tools": ["search", "read_file"],
      "write_tool_patterns": ["shell", "exec", "fs.write", "write_file"],
      "servers": [
        {
          "name": "docs",
          "command": ["node", "server.js"],
          "role": "reviewer_only",
          "read_only": true,
          "allowed_tools": ["search"]
        }
      ]
    }
  },
  "daemon": {
    "protocol": "rpc",
    "rpc": {
      "per_consumer_queue": 1024,
      "payload_cap_kib": 64,
      "token_path": ".nerve/session-meta/rpc-token",
      "token_size_bytes": 32,
      "envelope_version": "1.0.0"
    }
  }
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
- Optional per-profile `plan_strategy` and `plan_system_prompt_override`
- Optional per-profile `fork`
- Optional per-profile `mcp`

`match_rules` accepts the original shorthand array or a logical rule object:

```json
{ "all": ["*.rs", "contract"], "any": ["audit", "security"] }
```

### Worktree Apply

Set `orchestration.worktree_apply: true` to run apply attempts in an isolated Git worktree. Use `--worktree` or `--no-worktree` to override the config for one CLI invocation. Nerve refuses dirty trees, symlink escapes, and merge/discard operations when main moved after round preparation.

### RPC Config

`daemon.protocol: "rpc"` makes daemon mode use JSONL RPC envelopes by default. Relative `daemon.rpc.token_path` values are resolved from the workspace root, so the default token path is `.nerve/session-meta/rpc-token`. `nv rpc rotate-token` rotates the bearer secret.

### MCP Config

Each MCP server needs a unique non-empty `name` and non-empty `command` argv. `read_only` defaults to `true`; keep it enabled unless the server is intentionally allowed to mutate state. Under `read_only`, admission is deny-by-default (see the MCP Attachment section above): set per-server `allowed_tools` to whitelist exact tool names, or rely on the server's `readOnlyHint`/`destructiveHint` annotations. `mcp.read_only_posture` (`deny_by_default` default, or `legacy_denylist`) selects the admission strategy and is provenance-gated. Global `allow_tools` is intersected with each server's `allowed_tools`. Optional `mcp.argument_policy` adds per-tool argument confinement on top of name gating (`path_args` lexically confined to the project root, `deny_substrings` denylists); it is monotone-restrictive (can only reject), so it needs no provenance gate — see the MCP Attachment section above.

### Mayor/Patrol Config

`orchestration.mayor_patrol` controls queue and result directories, max patrol count, heartbeat interval, claim TTL, and optional per-patrol budget. `claim_ttl_secs` must be at least twice `heartbeat_secs`.

### Conflict Policies

The config schema includes:

| Policy | Behavior |
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
- Multi-file apply captures byte-exact pre-apply snapshots and restores them automatically if any file operation fails. Patch content itself is UTF-8 text (unified diffs over text files): a binary/non-UTF-8 target is rejected with a clear "unsupported" error atomically — before any file is modified, so a mixed text+binary patch never partially applies — and binary diffs are not supported. The snapshot/rollback layer reads and writes raw bytes, so the restore path stays faithful regardless of the original file's encoding.
- Created files are removed during rollback.
- Deleted files are restored from the original content during rollback.
- Reviewer `BLOCK` can prevent application depending on conflict policy.
- Worktree-isolated apply refuses dirty worktrees, symlink escapes, and stale main resets.
- `/goal` deterministic checks are argv-only, cwd-frozen, timeout-capped, output-capped, and can run under configured ulimits; natural-language conversion screens model-proposed `env` for code-execution vectors (`LD_*`/`DYLD_*`, `PATH`, toolchain/shell/interpreter hooks) and lists every surviving override in the confirmation gate.
- On Linux, an opt-in `sandbox.landlock` layer composes an LSM-enforced Landlock write-confinement *inside* the existing `bwrap` jail as defense-in-depth (kernel-mediated, so an in-namespace daemon reachable over a bound socket cannot defeat it). Because Landlock applied to `bwrap` itself would deny `bwrap`'s own unprivileged-userns setup writes, it is applied by a tiny in-jail helper (`nv __nv-confine … -- <check>`) that `bwrap` execs: the helper restricts the ABI-V1 filesystem write rights (file create/write/remove and the `make_*` rights) to the same roots `bwrap` rw-binds (the check's `cwd`, the per-check private temp dir, and `bwrap`'s minimal `/dev`) — reads/execs stay open — then `execve`s the real check, which inherits the restriction. **The Landlock layer handles only the ABI-V1 write rights; later-ABI rights — `Truncate`, `Refer` (cross-directory rename/link), `IoctlDev` — are intentionally *not* handled by it and are backstopped by `bwrap`'s read-only host bind (which already denies out-of-root host modification at the mount layer). So the composed `bwrap`+Landlock stack is the boundary; the Landlock layer is defense-in-depth, not a standalone write jail, and "fully enforced" means every *handled* (V1) right is enforced — not that every write vector is covered by Landlock alone.** Like macOS `strict`, the layer is confinement-*tightening only* (it can only deny more writes, never enable execution or loosen a gate), so a repo-local (`Project`) config enabling it needs no operator-consent gate. Under `auto` it is best-effort (a kernel without Landlock falls back to `bwrap`-only, and Nerve re-surfaces an operator warning through its `tracing` log on every outcome where the check's stderr was captured — a passing check included, not only a failing one: the in-jail helper marks the degradation in the check's captured stderr and the parent re-emits it; a timeout or output-cap abort, where the stderr stream isn't available to scan, are the only cases it isn't separately re-emitted, and both already fail the check loudly — and because the marker rides the check's own stderr, a hostile check can spoof a spurious warning but can never *suppress* a real one); under `required` it is fail-closed (if the handled rights cannot be fully enforced, the check is refused and the code never runs, tied into the `required` confinement self-test). **Honest scope:** the argv-shape and fail-closed *decision* logic is unit-tested on every platform, but the `restrict_self()`/`execve` runtime path is exercised only by the CI Linux real-kernel test (it is cross-compile- and clippy-checked on non-Linux hosts, never run there), and the exact path-grant set (notably `/dev`) is provisional pending real-kernel CI.
- On Linux, a second opt-in layer `sandbox.seccomp` installs a **seccomp-bpf syscall denylist** inside the same in-jail helper (it composes with, or runs independently of, `landlock`). It kills the process (`SIGSYS`) if the check issues one of a fixed set of dangerous escape-primitive syscalls that a normal build/test workload never makes — namespace/mount control (`mount`, `umount2`, `pivot_root`, `chroot`, `unshare`, `setns`), process tampering (`ptrace`, `process_vm_writev`), kernel-object loading (`bpf`, `*_module`, `kexec_*`), the kernel keyring (`add_key`, `request_key`, `keyctl`), `userfaultfd`, and system control (`reboot`, `swapon`/`swapoff`). The denylist (like `landlock`) cannot be applied to `bwrap` itself — denying `mount`/`unshare` would break `bwrap`'s own namespace setup — so it is installed by the helper after Landlock, then survives the `execve` into the check. **Honest scope:** a denylist is inherently *incomplete* — it blocks only the enumerated syscalls and does **no** argument-level filtering (it does not inspect `clone`/`clone3` flags, so namespace *creation* is contained by `bwrap`'s setup plus the `unshare`/`setns` denial, not by this filter), and it is defense-in-depth on top of `bwrap`+Landlock, never a standalone boundary. It is confinement-*tightening only* (no operator-consent gate needed), and — unlike Landlock — it is **always best-effort, even under `required`**: seccomp is secondary hardening and is *never* the fail-closed basis, so a kernel that cannot install the filter runs without it (Nerve re-surfaces the degradation through `tracing`, same captured-stderr marker mechanism as Landlock); it never gates acceptance and never causes a refusal. Because it kills on a denied call, an unusual workload that legitimately needs a denied syscall (a debugger using `ptrace`, container tooling using `unshare`) would be killed — by design; don't enable it for such checks. The filter-install/kill runtime path is exercised only by the CI Linux real-kernel test; on non-Linux hosts it is cross-compile- and clippy-checked, never run.
- The Linux confinement layers are **proven on a real kernel in CI**: a GitHub Actions job installs `bubblewrap`, enables unprivileged user namespaces, and runs the test suite so the `#[cfg(target_os = "linux")]` real-kernel proofs actually execute (they only compile-check on the macOS/Windows dev host). The headline proof mirrors the macOS one — it runs a canary through the *production* decision path under `required` and asserts an out-of-root write is **denied** (the host `/` is bound read-only) and the network namespace is **unshared** — alongside the Landlock (write-confinement) and seccomp (denied-syscall `SIGSYS` kill) proofs. **Skip-vs-fail discipline:** on a dev host each proof *skips* (never false-fails) where its kernel feature — bubblewrap/user-namespaces, Landlock, or seccomp filtering — is unavailable; **every** proof routes that skip through one shared gate that the CI job arms with `NERVE_CI_REAL_KERNEL=1`, turning a "kernel lacks support" skip into a hard failure. So on a CI runner that has those features (current `ubuntu-latest` does) no Linux real-kernel proof can silently green-skip and prove nothing. **Honest scope:** this exercises the *Linux* confinement runtime only (macOS Seatbelt is proven by its own macOS-host tests); a Windows test job is intentionally not wired in yet, so CI makes no Windows-runtime claim. It is purely additive test/CI coverage — no production behavior changes.
- Model-CLI generation subprocesses are spawned with `kill_on_drop`, so a dropped in-flight generation (daemon shutdown, cancellation, panic unwind) reliably terminates the child instead of orphaning it. This applies only to model generation — the deterministic goal-check verifier is never killed by a dropped future — and operator cancellation is honored at the round seam, where a cancelled run is always reported blocked and never applied.
- `/budget` changes are persisted as an append-only hash chain and checked by `nv doctor`; the chain is unkeyed SHA-256 by default (detects accidental/naive edits) and uses keyed HMAC-SHA-256 when `NERVE_BUDGET_AUDIT_KEY`/`NERVE_BUDGET_AUDIT_KEY_FILE` (absolute path) is set off the defended host — a non-key-holder then cannot forge or edit a link (including the unlinked tail entry, which carries its own self-MAC), keying over any pre-existing unkeyed log fails loudly (only an empty log can begin a keyed chain), and a misconfigured key fails closed rather than silently downgrading to unkeyed.
- RPC tokens are stored under `.nerve/session-meta/` with restrictive permissions on Unix.
- MCP servers default to read-only mode with deny-by-default admission (allowlist or read-only annotation evidence required), a write-tool blacklist veto, and a provenance-gated legacy posture; an optional per-tool `argument_policy` adds lexical path-root confinement and substring denylists as monotone-restrictive defense-in-depth.
- Mayor/Patrol queue identifiers are restricted to safe file-component characters and duplicate task IDs are rejected.
- The `/goal` no-progress hint is reject-only telemetry, computed only for a check that already failed: it reads the pass-ratio of recognized test summaries (libtest, pytest, jest) plus `go test` per-test `--- PASS:`/`--- FAIL:` markers, taking the most pessimistic (minimum) ratio across stdout/stderr and across recognizers so it always biases toward more stall pressure. It is best-effort and not tamper-proof — because the summary recognizer uses the last summary line in a stream, a lead can inflate the hint by appending a later forged all-pass summary or by suppressing the real failure output entirely. That can only delay a stall-driven abort: progress is never read as acceptance, so it cannot satisfy a goal, shorten review, or flip a verdict — the deterministic exit-code check stays the sole acceptance authority. Unrecognized output yields no ratio and the loop falls back to identical-output stall detection.
- The coordination ledger and round checkpoints are non-authoritative projections, never consulted by the deterministic apply/`blocked` gate. Checkpoints are written atomically per run-id, so concurrent multi-instance writers never corrupt them. `nv mayor --reconcile` rebuilds the ledger from the authoritative queue directories — the directories win on any disagreement, so reconcile never downgrades a finished task back to pending nor reports a failed task as done (a result file that disagrees with its queue directory is ignored).
- The safety thesis's negative space is pinned by standing invariant tests that CI runs with `cargo test` (there is no separate lint binary): the OS sandbox and built-in verifier default `Off`, apply defaults to dry-run, repo-local (`Project`) config cannot enable code execution without out-of-band operator consent (the `ConfigSource` provenance gate), every apply gate (consensus and tournament strategies) reads only the in-memory consent handle and never the on-disk `.nerve/approvals/` audit record, and the inter-agent mailbox `MailKind` is a closed set with no consent variant (a new variant fails to compile). Each test goes red on the corresponding regression. A brand-new execution-enabling surface is **not** auto-detected (grep-not-AST); routing it through `ConfigSource` provenance and adding its invariant test is a documented reviewer requirement.
- Generated runtime state under `.nerve/` is ignored by Git.

Runtime persistence and patch indexing are implemented under `.nerve/`.

## Development

```bash
cargo test
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
cargo run -p nerve-cli -- config validate
cargo run -p nerve-cli -- --adapter mock "add log line to main.rs"
cargo run -p nerve-cli -- --adapter mock plan "update docs"
cargo run -p nerve-cli -- --adapter mock benchmark pi --iterations 1 --json
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
- Real adapter JSONL usage and reviewer suggested-patch extraction
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
- Typed RPC daemon envelopes, token rotation, metadata, and payload caps
- Three-pane terminal TUI summary
- `/goal`, natural-language goal conversion, active-goal reload, and goal history audit
- `/budget`, budget raise confirmation, global ceilings, and audit-chain tamper detection
- Worktree-isolated apply and stale-main refusal
- Plan mode validation and dual review
- Session fork/branch persistence and legacy session bootstrap
- MCP read-only guard, role matching, and global allowlist scoping
- Mayor/Patrol queue claim, result movement, orphan recovery, duplicate task rejection, and ID validation
- Pi workflow benchmark command

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

## Release Scope

v1.0.0 includes the original lead/reviewer patch loop plus:

- terminal workspace commands for login, doctor, templates, diff/apply/rollback, goal, budget, plan, fork, MCP, Mayor, and Patrol
- config-backed real adapter execution with timeout/output caps, usage capture, and suggested patch extraction
- persistent sessions, named sessions, linked reruns, patch index, and session forks
- worktree-isolated apply mode
- typed RPC daemon envelopes for editor and shell integrations
- read-only plan mode
- MCP stdio client with read-only and allowlist guards
- Mayor/Patrol queue primitives for multi-instance orchestration
- installable release binaries for macOS, Linux, and Windows

The next hardening areas are long-duration Mayor/Patrol operation, live MCP server compatibility across popular servers, and more end-to-end tests around real Claude/Codex adapter combinations.

## Design Documents

- [Architecture](./nerve-architecture.md)
- [Implementation Plan](./nerve-implementation-plan.md)

## License

MIT
