use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use nerve_adapter::{
    AdapterLimits, McpClient, McpRegistry, ModelAdapter, SubprocessAdapter,
    default_adapters_with_limits, default_write_tool_patterns, scope_mcp_spec_to_allowlist,
};
use nerve_config::{
    BuiltinVerifierMode, Config, DaemonProtocol, GoalIntent, GoalSpec, McpConfig, PlanStrategy,
    RpcConfig,
};
use nerve_core::session_fork::{ForkOptions as CoreForkOptions, SessionTree};
use nerve_core::store::{ApprovalGrant, NerveStore, RunCheckpoint};
use nerve_core::{
    ApplyConsent, AuditChainState, BudgetAuditEntry, BudgetSnapshot, ChainStatus, DoctorCheck,
    DoctorStatus, ForkConfig as CoreForkConfig, GoalIntentConverter, Mayor,
    PROJECT_VERIFIER_CONSENT_ENV, Patrol, PatrolTask, PlanError, PlanRunOptions, RpcBus,
    RunOptions, RunReport, SessionForker, append_budget_audit_entry, doctor_checks,
    format_chain_broken, project_verifier_consent_from_env, resolve_builtin_verifier,
    run_plan_mode, run_synaptic_loop, run_synaptic_loop_streaming,
};
use nerve_tui::{TuiApp, TuiAppOptions, TuiState};
use nerve_types::{
    AgentEvent, McpToolCall, PlanReport, RPC_SCHEMA_VERSION, RoundRecord, RpcEnvelope, Task, Verdict,
};
use serde::Serialize;
use std::collections::{BTreeMap, HashMap};
use std::env;
use std::fs;
use std::io::{IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command as StdCommand;
use std::sync::Arc;
use std::time::Instant;
use tokio::io::{self, AsyncBufReadExt};
use tokio::sync::{broadcast, mpsc, watch};
use tokio::task::JoinHandle;

const SURFACE_WIDTH: usize = 78;

#[derive(Debug, Parser)]
#[command(
    name = "nv",
    about = "Nerve reflexive AI orchestration CLI",
    version,
    subcommand_precedence_over_arg = true
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,

    #[arg(help = "Task prompt to dispatch through the Synaptic Loop")]
    prompt: Option<String>,

    #[arg(long, help = "Apply the final patch after review")]
    apply: bool,

    #[arg(long, help = "Emit a machine-readable JSON session report")]
    json: bool,

    #[arg(long, help = "Render a three-pane terminal summary")]
    tui: bool,

    #[arg(
        long,
        help = "Force Tier 2d worktree-isolated /apply (overrides nerve.config.json)"
    )]
    worktree: bool,

    #[arg(
        long,
        help = "Force the legacy in-place /apply path even if worktree_apply is enabled"
    )]
    no_worktree: bool,

    #[arg(long, env = "NERVE_ADAPTER", default_value = "real")]
    adapter: AdapterMode,
}

/// Resolve the optional Tier 2d worktree override from the global CLI flags.
/// `--worktree` wins over `--no-worktree`; `None` defers to config.
fn cli_worktree_override(worktree: bool, no_worktree: bool) -> Option<bool> {
    if worktree {
        Some(true)
    } else if no_worktree {
        Some(false)
    } else {
        None
    }
}

#[derive(Debug, Clone, clap::ValueEnum)]
enum AdapterMode {
    Real,
    Mock,
}

#[derive(Debug, Clone, clap::ValueEnum)]
enum LoginProvider {
    All,
    Claude,
    Codex,
}

#[derive(Debug, Subcommand)]
enum Command {
    #[command(alias = "bench", about = "Run Nerve benchmark workflows")]
    Benchmark {
        #[command(subcommand)]
        command: BenchmarkCommand,
    },
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },
    #[command(about = "List stored Nerve sessions")]
    History {
        #[arg(long, help = "Emit stored sessions as JSON")]
        json: bool,
        #[arg(long, help = "Show only sessions whose patch is applied")]
        applied: bool,
        #[arg(long, help = "Show only blocked sessions")]
        blocked: bool,
        #[arg(long, help = "Show only named sessions")]
        named: bool,
    },
    #[command(about = "Print a stored session report")]
    Resume {
        #[arg(help = "Stored task/session id")]
        task_id: String,
        #[arg(long, help = "Emit the stored session report as JSON")]
        json: bool,
    },
    #[command(about = "List indexed patches")]
    List {
        #[arg(long, help = "Emit indexed patches as JSON")]
        json: bool,
    },
    #[command(about = "Apply an indexed patch")]
    Apply {
        #[arg(help = "Patch id from `nv list`")]
        patch_id: String,
    },
    #[command(about = "Rollback an indexed patch")]
    Rollback {
        #[arg(help = "Patch id from `nv list`")]
        patch_id: String,
    },
    #[command(about = "Run config, environment, worktree, and audit-chain health checks")]
    Doctor,
    #[command(about = "Run a line-oriented daemon for editor and shell integrations")]
    Daemon {
        #[arg(long, help = "Process one prompt from stdin and exit")]
        once: bool,
        #[arg(long, help = "Use JSONL RPC input and lifecycle events")]
        rpc: bool,
        #[arg(
            long,
            help = "Print the RPC bearer token on stdout after the daemon starts"
        )]
        print_token: bool,
    },
    #[command(about = "Inspect or maintain the v0.5.0 RPC event-streaming surface")]
    Rpc {
        #[command(subcommand)]
        command: RpcCommand,
    },
    #[command(about = "Run /plan (Tier 2f read-only analysis); never produces a patch")]
    Plan(PlanArgs),
    #[command(about = "First-run setup and local prerequisite checks")]
    Setup,
    #[command(about = "Sign in to Claude Code and/or Codex subscriptions")]
    Login {
        #[arg(value_enum, default_value = "all")]
        provider: LoginProvider,
    },
    #[command(about = "Start the Nerve terminal workspace")]
    Interactive,
    #[command(about = "Name a stored Nerve session")]
    Name {
        #[arg(help = "Stored task/session id")]
        task_id: String,
        #[arg(help = "Human-readable session name")]
        name: String,
    },
    #[command(about = "Run a follow-up prompt linked to an existing session")]
    Rerun {
        #[arg(help = "Source task/session id")]
        task_id: String,
        #[arg(help = "Follow-up task prompt")]
        prompt: String,
    },
    #[command(about = "List or run configured prompt templates")]
    Template {
        #[command(subcommand)]
        command: TemplateCommand,
    },
    /// v1.0 Tier 3h: Fork a stored session into a child branch.
    #[command(about = "Fork a session into a child branch (Tier 3h)")]
    Fork(ForkArgs),
    /// v1.0 Tier 3h: alias for `fork` that auto-names the child via
    /// `ForkConfig.auto_name`. Maps to a regular `SessionForker::fork`
    /// invocation with `name=None` so the parent-side history stays untouched.
    #[command(about = "Branch a task into a child session (alias for fork)")]
    Branch {
        #[arg(help = "Parent task/session id")]
        task_id: String,
    },
    /// v1.0 Tier 3h: read-only inspectors over `.nerve/sessions/`.
    #[command(about = "List stored sessions and forks (Tier 3h)")]
    Sessions {
        #[command(subcommand)]
        command: SessionsCommand,
    },
    /// v1.0 Tier 3i: MCP probe / list helpers.
    #[command(about = "Inspect or probe configured MCP servers (Tier 3i)")]
    Mcp {
        #[command(subcommand)]
        command: McpCommand,
    },
    /// v1.0 Tier 3j: long-lived Mayor dispatcher.
    #[command(about = "Run the Mayor dispatcher (Tier 3j)")]
    Mayor(MayorArgs),
    /// v1.0 Tier 3j: Patrol worker bound to a slot id.
    #[command(about = "Run a Patrol worker (Tier 3j)")]
    Patrol(PatrolArgs),
}

/// Tier 3h `nv fork` arguments. Defaults preserve byte-identical legacy
/// behaviour — when no flags are passed and no parent is on-disk, the
/// command short-circuits to a clean ParentNotFound error.
#[derive(Debug, clap::Args)]
struct ForkArgs {
    #[arg(help = "Parent session id to branch off")]
    session_id: String,
    #[arg(long, help = "Inclusive parent round index to branch from")]
    from_round: Option<u32>,
    #[arg(long, help = "Optional child name (A-Za-z0-9_-, 1..=64 chars)")]
    name: Option<String>,
    #[arg(long, help = "Pin the child to an explicit base patch SHA")]
    from_patch_sha: Option<String>,
}

#[derive(Debug, Subcommand)]
enum SessionsCommand {
    #[command(about = "List root sessions")]
    List {
        #[arg(long, help = "Emit sessions as JSON")]
        json: bool,
    },
    #[command(about = "Render the fork tree rooted at <root-id>")]
    Tree {
        #[arg(help = "Root session id")]
        root_id: String,
        #[arg(long, help = "Emit the tree as JSON")]
        json: bool,
    },
}

/// Tier 3i `nv mcp` arguments.
#[derive(Debug, Subcommand)]
enum McpCommand {
    #[command(about = "List MCP tools exposed by configured servers")]
    ListTools {
        #[arg(long, help = "Emit tool list as JSON")]
        json: bool,
    },
    #[command(about = "Start, handshake, list tools, and tear down a server")]
    Probe {
        #[arg(help = "MCP server name from nerve.config.json")]
        server: String,
    },
}

/// Tier 3j `nv mayor` arguments. Defaults to running until idle so a
/// non-interactive `nv mayor` invocation drains the on-disk queue and exits
/// — the long-lived supervisor form is opt-in via the slash command.
#[derive(Debug, clap::Args)]
struct MayorArgs {
    #[arg(long, help = "Override the max-patrols ceiling")]
    max_patrols: Option<u32>,
    #[arg(long, help = "Per-patrol budget ceiling in micro-USD")]
    per_patrol_budget_microusd: Option<u64>,
    #[arg(long, help = "Override the queue directory (defaults to .nerve/queue)")]
    queue_dir: Option<PathBuf>,
    #[arg(
        long,
        help = "Override the results directory (defaults to .nerve/results)"
    )]
    results_dir: Option<PathBuf>,
    #[arg(long, help = "Print the queue status and exit (no dispatch)")]
    status_only: bool,
}

/// Tier 3j `nv patrol` arguments.
#[derive(Debug, clap::Args)]
struct PatrolArgs {
    #[arg(long, help = "Patrol slot id (used as the on-disk claim key)")]
    id: String,
    #[arg(long, help = "Per-patrol worktree directory (best-effort hint)")]
    worktree: Option<PathBuf>,
    #[arg(
        long = "mcp-server",
        help = "Bind this patrol to a configured MCP server (repeatable)"
    )]
    mcp_server: Vec<String>,
    #[arg(long, help = "Claim and run a single task, then exit")]
    once: bool,
    #[arg(long, help = "Print the patrol's local heartbeat / results and exit")]
    status: bool,
}

#[derive(Debug, Subcommand)]
enum ConfigCommand {
    Validate,
}

/// `nv plan` arguments. Plan mode runs the lead (and optionally reviewer)
/// adapter under a strict plan-only system prompt that forbids patches.
#[derive(Debug, clap::Args)]
struct PlanArgs {
    #[arg(help = "Natural-language task description for plan-mode")]
    task: String,
    #[arg(
        long,
        help = "Run the reviewer adapter as a second pass over the plan markdown"
    )]
    dual_review: bool,
    #[arg(
        long,
        help = "Override the workspace directory for plan-mode (defaults to cwd)"
    )]
    cwd: Option<PathBuf>,
}

/// `nv rpc` family. Currently exposes the v0.5.0 bearer-token rotation
/// helper; future maintenance subcommands hang off this enum.
#[derive(Debug, Subcommand)]
enum RpcCommand {
    #[command(about = "Rotate the bearer token persisted under .nerve/session-meta/")]
    RotateToken,
}

#[derive(Debug, Subcommand)]
enum BenchmarkCommand {
    #[command(about = "Run the Pi-inspired terminal workflow benchmark")]
    Pi {
        #[arg(long, default_value_t = 3, help = "Benchmark iterations, from 1 to 20")]
        iterations: u16,
        #[arg(long, help = "Emit a machine-readable benchmark report")]
        json: bool,
        #[arg(
            long,
            help = "Use the configured real provider adapter instead of the deterministic mock adapter"
        )]
        live: bool,
    },
}

#[derive(Debug, Subcommand)]
enum TemplateCommand {
    #[command(about = "List configured prompt templates")]
    List {
        #[arg(long, help = "Emit templates as JSON")]
        json: bool,
    },
    #[command(about = "Run a configured prompt template")]
    Run {
        #[arg(help = "Template id from `nv template list`")]
        template_id: String,
        #[arg(help = "Arguments substituted into {{args}}")]
        args: Vec<String>,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("NERVE_LOG")
                .unwrap_or_else(|_| "warn".into()),
        )
        .init();

    let cli = Cli::parse();

    match cli.command {
        Some(Command::Benchmark { command }) => {
            run_benchmark(command, cli.json, matches!(cli.adapter, AdapterMode::Mock)).await
        }
        Some(Command::Config {
            command: ConfigCommand::Validate,
        }) => {
            Config::load()?;
            println!("nerve.config.json is valid");
            Ok(())
        }
        Some(Command::History {
            json,
            applied,
            blocked,
            named,
        }) => {
            let cwd = env::current_dir().context("failed to read current directory")?;
            let mut sessions = NerveStore::new(cwd).list_sessions()?;
            sessions.retain(|session| {
                (!applied || session.applied)
                    && (!blocked || session.blocked)
                    && (!named || session.name.is_some())
            });
            if json {
                println!("{}", serde_json::to_string_pretty(&sessions)?);
            } else if sessions.is_empty() {
                println!("No Nerve sessions found.");
            } else {
                for session in sessions {
                    println!(
                        "{} | {:?} | rounds={} | applied={} | patch={} | name={} | {}",
                        session.id,
                        session.verdict,
                        session.rounds,
                        session.applied,
                        session.patch_id.as_deref().unwrap_or("-"),
                        session.name.as_deref().unwrap_or("-"),
                        session.prompt
                    );
                }
            }
            Ok(())
        }
        Some(Command::Resume { task_id, json }) => {
            let cwd = env::current_dir().context("failed to read current directory")?;
            let report = NerveStore::new(cwd).load_report(&task_id)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                print_report(&report, false);
            }
            Ok(())
        }
        Some(Command::List { json }) => {
            let cwd = env::current_dir().context("failed to read current directory")?;
            let patches = NerveStore::new(cwd).list_patches()?;
            if json {
                println!("{}", serde_json::to_string_pretty(&patches)?);
            } else if patches.is_empty() {
                println!("No indexed patches found.");
            } else {
                for patch in patches {
                    println!(
                        "{} | {:?} | files={} | applied={} | session={} | {}",
                        patch.id,
                        patch.verdict,
                        patch.file_count,
                        patch.applied,
                        patch.session_id,
                        patch.prompt
                    );
                }
            }
            Ok(())
        }
        Some(Command::Apply { patch_id }) => {
            let cwd = env::current_dir().context("failed to read current directory")?;
            let report = NerveStore::new(cwd).apply_patch(&patch_id)?;
            println!(
                "Applied patch {} to {} path(s).",
                report.patch_id,
                report.changed_files.len()
            );
            print_changed_files(&report.changed_files);
            Ok(())
        }
        Some(Command::Rollback { patch_id }) => {
            let cwd = env::current_dir().context("failed to read current directory")?;
            let report = NerveStore::new(cwd).rollback_patch(&patch_id)?;
            println!(
                "Rolled back patch {} across {} path(s).",
                report.patch_id,
                report.changed_files.len()
            );
            print_changed_files(&report.changed_files);
            Ok(())
        }
        Some(Command::Doctor) => run_doctor(
            matches!(cli.adapter, AdapterMode::Mock),
            &mut std::io::stdout(),
            &mut std::io::stderr(),
        ),
        Some(Command::Daemon {
            once,
            rpc,
            print_token,
        }) => {
            let config_prefers_rpc = Config::load()
                .map(|config| matches!(config.daemon.protocol, DaemonProtocol::Rpc))
                .unwrap_or(false);
            if rpc || config_prefers_rpc {
                run_rpc_daemon(
                    cli.apply,
                    matches!(cli.adapter, AdapterMode::Mock),
                    once,
                    print_token,
                    cli_worktree_override(cli.worktree, cli.no_worktree),
                )
                .await
            } else {
                if print_token {
                    anyhow::bail!("--print-token requires --rpc (or daemon.protocol=rpc)");
                }
                run_daemon(
                    cli.apply,
                    matches!(cli.adapter, AdapterMode::Mock),
                    once,
                    cli_worktree_override(cli.worktree, cli.no_worktree),
                )
                .await
            }
        }
        Some(Command::Rpc { command }) => run_rpc_subcommand(command),
        Some(Command::Plan(args)) => {
            run_plan_subcommand(args, matches!(cli.adapter, AdapterMode::Mock), cli.json).await
        }
        Some(Command::Setup) => run_setup(matches!(cli.adapter, AdapterMode::Mock)),
        Some(Command::Login { provider }) => run_login(provider),
        Some(Command::Interactive) => {
            run_interactive(
                cli.apply,
                matches!(cli.adapter, AdapterMode::Mock),
                cli_worktree_override(cli.worktree, cli.no_worktree),
                cli.tui,
            )
            .await
        }
        Some(Command::Name { task_id, name }) => {
            let cwd = env::current_dir().context("failed to read current directory")?;
            NerveStore::new(cwd).name_session(&task_id, name)?;
            println!("Named session {task_id}.");
            Ok(())
        }
        Some(Command::Rerun { task_id, prompt }) => {
            let cwd = env::current_dir().context("failed to read current directory")?;
            let store = NerveStore::new(&cwd);
            store.load_report(&task_id)?;
            let report = run_report(
                prompt,
                cli.apply,
                matches!(cli.adapter, AdapterMode::Mock),
                cli_worktree_override(cli.worktree, cli.no_worktree),
            )
            .await?;
            store.link_child_session(&report.task.id, &task_id)?;
            if cli.json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else if cli.tui {
                print_tui_report(&report);
            } else {
                print_report(&report, cli.apply);
                println!("Linked to parent session {task_id}.");
            }
            Ok(())
        }
        Some(Command::Template { command }) => {
            run_template(
                command,
                cli.apply,
                cli.json,
                cli.tui,
                matches!(cli.adapter, AdapterMode::Mock),
                cli_worktree_override(cli.worktree, cli.no_worktree),
            )
            .await
        }
        Some(Command::Fork(args)) => run_fork_subcommand(args, cli.json).await,
        Some(Command::Branch { task_id }) => run_branch_subcommand(task_id, cli.json).await,
        Some(Command::Sessions { command }) => run_sessions_subcommand(command).await,
        Some(Command::Mcp { command }) => run_mcp_subcommand(command).await,
        Some(Command::Mayor(args)) => {
            run_mayor_subcommand(args, matches!(cli.adapter, AdapterMode::Mock)).await
        }
        Some(Command::Patrol(args)) => {
            run_patrol_subcommand(args, matches!(cli.adapter, AdapterMode::Mock)).await
        }
        None => {
            let Some(prompt) = cli.prompt else {
                if std::io::stdin().is_terminal() {
                    return run_interactive(
                        cli.apply,
                        matches!(cli.adapter, AdapterMode::Mock),
                        cli_worktree_override(cli.worktree, cli.no_worktree),
                        cli.tui,
                    )
                    .await;
                }
                anyhow::bail!("missing prompt; usage: nv \"add a /health endpoint\"");
            };
            run_prompt(
                prompt,
                cli.apply,
                cli.json,
                cli.tui,
                matches!(cli.adapter, AdapterMode::Mock),
                cli_worktree_override(cli.worktree, cli.no_worktree),
            )
            .await
        }
    }
}

async fn run_benchmark(command: BenchmarkCommand, json: bool, mock: bool) -> Result<()> {
    match command {
        BenchmarkCommand::Pi {
            iterations,
            json: command_json,
            live,
        } => {
            let report = run_pi_benchmark(iterations, live, mock).await?;
            print_pi_benchmark_report(&report, json || command_json)?;
            if report.success {
                Ok(())
            } else {
                anyhow::bail!("Pi benchmark failed")
            }
        }
    }
}

#[derive(Debug, Clone, Serialize)]
struct PiBenchmarkReport {
    name: &'static str,
    adapter: &'static str,
    iterations: u16,
    success: bool,
    elapsed_ms: u128,
    checks: Vec<PiBenchmarkCheck>,
}

#[derive(Debug, Clone, Serialize)]
struct PiBenchmarkCheck {
    name: String,
    ok: bool,
    elapsed_ms: u128,
    detail: String,
}

impl PiBenchmarkCheck {
    fn new(name: impl Into<String>, ok: bool, elapsed_ms: u128, detail: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            ok,
            elapsed_ms,
            detail: detail.into(),
        }
    }
}

async fn run_pi_benchmark(iterations: u16, live: bool, mock: bool) -> Result<PiBenchmarkReport> {
    if iterations == 0 || iterations > 20 {
        anyhow::bail!("--iterations must be between 1 and 20");
    }
    if live && mock {
        anyhow::bail!("--live requires --adapter real");
    }

    let adapter_mock = !live;
    let adapter = if adapter_mock { "mock" } else { "real" };
    let started = Instant::now();
    let workspace = tempfile::tempdir().context("failed to create benchmark workspace")?;
    let cwd = workspace.path();
    let config = Config::load_from(cwd)?;
    let store = NerveStore::new(cwd);
    let mut checks = Vec::new();

    let check_started = Instant::now();
    store.init()?;
    let has_templates = config
        .templates
        .iter()
        .any(|template| template.id == "security-audit")
        && config
            .templates
            .iter()
            .any(|template| template.id == "rapid-fix");
    checks.push(PiBenchmarkCheck::new(
        "config-store-templates",
        has_templates,
        check_started.elapsed().as_millis(),
        format!(
            "strategy={:?} templates={} store=.nerve",
            config.orchestration.default_strategy,
            config.templates.len()
        ),
    ));

    let adapters = adapters_for_config(adapter_mock, &config);
    for iteration in 1..=iterations {
        let task = Task::new(
            format!("Pi benchmark iteration {iteration}: produce a reviewed patch artifact"),
            cwd,
        );
        let run_started = Instant::now();
        let report = run_synaptic_loop(task, &config, &adapters, RunOptions::new(false)).await?;
        store.save_report(&report)?;
        let patch_id = report.final_patch.as_ref().map(|patch| patch.id.clone());
        let loop_ok = report
            .final_feedback
            .verdict
            .accepts_under(report.selection.review_strictness.permits_nits())
            && !report.blocked
            && !report.rounds.is_empty()
            && patch_id.is_some();
        checks.push(PiBenchmarkCheck::new(
            format!("loop-{iteration}"),
            loop_ok,
            run_started.elapsed().as_millis(),
            format!(
                "session={} verdict={:?} rounds={} patch={}",
                report.task.id,
                report.final_feedback.verdict,
                report.rounds.len(),
                patch_id.as_deref().unwrap_or("-")
            ),
        ));

        let Some(patch) = &report.final_patch else {
            continue;
        };

        let apply_started = Instant::now();
        let apply_report = store.apply_patch(&patch.id)?;
        let applied_report = store.load_report(&report.task.id)?;
        checks.push(PiBenchmarkCheck::new(
            format!("apply-{iteration}"),
            applied_report.applied
                && apply_report.patch_id == patch.id
                && !apply_report.changed_files.is_empty(),
            apply_started.elapsed().as_millis(),
            format!("changed_files={}", apply_report.changed_files.len()),
        ));

        let rollback_started = Instant::now();
        let rollback_report = store.rollback_patch(&patch.id)?;
        let rolled_back_report = store.load_report(&report.task.id)?;
        checks.push(PiBenchmarkCheck::new(
            format!("rollback-{iteration}"),
            !rolled_back_report.applied
                && rollback_report.patch_id == patch.id
                && !rollback_report.changed_files.is_empty(),
            rollback_started.elapsed().as_millis(),
            format!("changed_files={}", rollback_report.changed_files.len()),
        ));
    }

    let index_started = Instant::now();
    let sessions = store.list_sessions()?;
    let patches = store.list_patches()?;
    checks.push(PiBenchmarkCheck::new(
        "history-patch-index",
        sessions.len() == usize::from(iterations) && patches.len() == usize::from(iterations),
        index_started.elapsed().as_millis(),
        format!("sessions={} patches={}", sessions.len(), patches.len()),
    ));

    let success = checks.iter().all(|check| check.ok);
    Ok(PiBenchmarkReport {
        name: "pi",
        adapter,
        iterations,
        success,
        elapsed_ms: started.elapsed().as_millis(),
        checks,
    })
}

fn print_pi_benchmark_report(report: &PiBenchmarkReport, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(report)?);
        return Ok(());
    }

    println!(
        "Pi benchmark: {} | adapter={} iterations={} elapsed={}ms",
        if report.success { "ok" } else { "failed" },
        report.adapter,
        report.iterations,
        report.elapsed_ms
    );
    for check in &report.checks {
        println!(
            "[{}] {} ({}ms) {}",
            if check.ok { "ok" } else { "fail" },
            check.name,
            check.elapsed_ms,
            check.detail
        );
    }
    Ok(())
}

async fn run_prompt(
    prompt: String,
    apply: bool,
    json: bool,
    tui: bool,
    mock: bool,
    worktree_override: Option<bool>,
) -> Result<()> {
    let report = run_report(prompt, apply, mock, worktree_override).await?;

    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
        return Ok(());
    }

    if tui {
        print_tui_report(&report);
    } else {
        print_report(&report, apply);
    }
    Ok(())
}

async fn run_template(
    command: TemplateCommand,
    apply: bool,
    json: bool,
    tui: bool,
    mock: bool,
    worktree_override: Option<bool>,
) -> Result<()> {
    let cwd = env::current_dir().context("failed to read current directory")?;
    let config = Config::load_from(&cwd)?;
    match command {
        TemplateCommand::List { json } => {
            if json {
                println!("{}", serde_json::to_string_pretty(&config.templates)?);
            } else if config.templates.is_empty() {
                println!("No prompt templates configured.");
            } else {
                for template in config.templates {
                    println!(
                        "{} | {}",
                        template.id,
                        template.description.as_deref().unwrap_or("-")
                    );
                }
            }
            Ok(())
        }
        TemplateCommand::Run { template_id, args } => {
            let Some(template) = config
                .templates
                .iter()
                .find(|template| template.id == template_id)
            else {
                anyhow::bail!("template `{template_id}` is not configured");
            };
            let prompt = template.prompt.replace("{{args}}", &args.join(" "));
            run_prompt(prompt, apply, json, tui, mock, worktree_override).await
        }
    }
}

/// Resolve the `ForkConfig` to use for CLI fork operations: profile-default
/// when nerve.config.json supplies one, otherwise `CoreForkConfig::default()`.
fn resolved_fork_config(config: &Config) -> CoreForkConfig {
    let cfg = config.roles.fork.as_ref();
    let copy_patch_history = cfg.map(|c| c.copy_patch_history).unwrap_or(true);
    CoreForkConfig {
        copy_patch_history,
        ..CoreForkConfig::default()
    }
}

/// `nv fork` and `nv branch` payload. Resolves the parent on disk (creating
/// a minimal root record from the stored session report when the index is
/// empty), then invokes [`SessionForker::fork`].
async fn run_fork_subcommand(args: ForkArgs, json: bool) -> Result<()> {
    let cwd = env::current_dir().context("failed to read current directory")?;
    let config = Config::load_from(&cwd)?;
    let forker = SessionForker::new(resolved_fork_config(&config), &cwd);
    bootstrap_root_session_if_missing(&forker, &cwd, &args.session_id).await?;

    let opts = CoreForkOptions {
        from_round: args.from_round,
        name: args.name,
        from_patch_sha: args.from_patch_sha,
    };
    let child = forker
        .fork(&args.session_id, opts)
        .await
        .with_context(|| format!("fork failed for parent `{}`", args.session_id))?;

    if json {
        println!("{}", serde_json::to_string_pretty(&child)?);
    } else {
        print_fork_summary(&child, &args.session_id);
    }
    Ok(())
}

async fn run_branch_subcommand(task_id: String, json: bool) -> Result<()> {
    let cwd = env::current_dir().context("failed to read current directory")?;
    let config = Config::load_from(&cwd)?;
    let forker = SessionForker::new(resolved_fork_config(&config), &cwd);
    bootstrap_root_session_if_missing(&forker, &cwd, &task_id).await?;

    // `nv branch` synthesises a child name from `ForkConfig.auto_name` so the
    // user never has to provide one. We mint a deterministic, sanitised name
    // (no path separators) from a short timestamp suffix.
    let name = if config.roles.fork.as_ref().is_some_and(|f| f.auto_name) {
        Some(format!(
            "branch-{}",
            chrono::Utc::now().format("%Y%m%dT%H%M%SZ")
        ))
    } else {
        None
    };

    let child = forker
        .fork(
            &task_id,
            CoreForkOptions {
                from_round: None,
                name,
                from_patch_sha: None,
            },
        )
        .await
        .with_context(|| format!("branch failed for parent `{task_id}`"))?;

    if json {
        println!("{}", serde_json::to_string_pretty(&child)?);
    } else {
        print_fork_summary(&child, &task_id);
    }
    Ok(())
}

/// When the user invokes `nv fork <id>` against a session that has no entry
/// in `.nerve/sessions/index.json` yet (because it was stored only under the
/// v0.x `NerveStore` layout), we register a minimal root payload so the fork
/// engine can attach to it without rewriting the legacy store.
async fn bootstrap_root_session_if_missing(
    forker: &SessionForker,
    cwd: &Path,
    session_id: &str,
) -> Result<()> {
    if forker.get(session_id).await?.is_some() {
        return Ok(());
    }
    // Only bootstrap when the legacy store has a record for the id. We refuse
    // to forge a brand-new parent out of thin air — that would let `nv fork
    // <typo>` silently create an empty bucket.
    let store = NerveStore::new(cwd);
    let Ok(report) = store.load_report(session_id) else {
        return Ok(());
    };
    let mut tree = SessionTree::root(session_id, None);
    tree.rounds = report.rounds;
    forker.persist_root(tree).await?;
    Ok(())
}

fn print_fork_summary(child: &SessionTree, parent_id: &str) {
    println!(
        "Forked session {} from parent {} (round={:?}, patch_sha={:?})",
        child.id,
        parent_id,
        child.branched_at_round,
        child.branched_from_patch_sha.as_deref()
    );
    if let Some(name) = &child.name {
        println!("  name: {name}");
    }
    println!("  rounds copied: {}", child.rounds.len());
}

async fn run_sessions_subcommand(command: SessionsCommand) -> Result<()> {
    let cwd = env::current_dir().context("failed to read current directory")?;
    let config = Config::load_from(&cwd)?;
    let forker = SessionForker::new(resolved_fork_config(&config), &cwd);
    match command {
        SessionsCommand::List { json } => {
            let roots = forker.list_root().await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&roots)?);
            } else if roots.is_empty() {
                println!("No sessions registered under .nerve/sessions.");
            } else {
                for root in roots {
                    println!(
                        "{} | name={} | children={} | forked_at={}",
                        root.id,
                        root.name.as_deref().unwrap_or("-"),
                        root.children.len(),
                        root.forked_at
                    );
                }
            }
        }
        SessionsCommand::Tree { root_id, json } => {
            let tree = forker.tree(&root_id).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&tree)?);
            } else {
                print_session_tree(&forker, &tree, 0).await?;
            }
        }
    }
    Ok(())
}

/// Recursively render the fork tree to stdout. Recursion is bounded by the
/// on-disk session index, so a malformed parent_id loop would surface as a
/// `ParentNotFound` error instead of stack-blowing.
fn print_session_tree<'a>(
    forker: &'a SessionForker,
    tree: &'a SessionTree,
    depth: usize,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send + 'a>> {
    Box::pin(async move {
        println!(
            "{}- {} (name={}, children={}, rounds={})",
            "  ".repeat(depth),
            tree.id,
            tree.name.as_deref().unwrap_or("-"),
            tree.children.len(),
            tree.rounds.len(),
        );
        for child_id in &tree.children {
            if let Some(child) = forker.get(child_id).await? {
                print_session_tree(forker, &child, depth + 1).await?;
            } else {
                println!("{}- {} (missing payload)", "  ".repeat(depth + 1), child_id);
            }
        }
        Ok(())
    })
}

/// Resolve the active MCP config from nerve.config.json — profile-level
/// override wins over the role-level default. Returns `None` when MCP is
/// disabled (the default).
fn active_mcp_config(config: &Config) -> Option<&McpConfig> {
    if let Some(profile) = config.profiles.first()
        && let Some(mcp) = profile.mcp.as_ref()
    {
        return Some(mcp);
    }
    config.roles.mcp.as_ref()
}

async fn run_mcp_subcommand(command: McpCommand) -> Result<()> {
    let cwd = env::current_dir().context("failed to read current directory")?;
    let config = Config::load_from(&cwd)?;
    let Some(mcp) = active_mcp_config(&config).cloned() else {
        println!("mcp: no servers configured (roles.mcp / profiles[].mcp empty)");
        return Ok(());
    };

    match command {
        McpCommand::ListTools { json } => {
            let mut registry = McpRegistry::new();
            if let Err(err) = registry
                .register_all(&mcp.servers, &mcp.write_tool_patterns, &mcp.allow_tools)
                .await
            {
                anyhow::bail!("failed to start MCP servers: {err}");
            }
            let mut entries: Vec<serde_json::Value> = Vec::new();
            for (name, client) in registry.iter() {
                let tools = client
                    .list_tools()
                    .await
                    .with_context(|| format!("list_tools failed for `{name}`"))?;
                for tool in tools {
                    entries.push(serde_json::json!({
                        "server": name,
                        "name": tool.name,
                        "description": tool.description,
                    }));
                }
            }
            registry
                .shutdown_all()
                .await
                .context("mcp shutdown after list_tools failed")?;
            if json {
                println!("{}", serde_json::to_string_pretty(&entries)?);
            } else if entries.is_empty() {
                println!("mcp: no tools advertised by configured servers.");
            } else {
                for entry in entries {
                    println!(
                        "{} | {} | {}",
                        entry["server"].as_str().unwrap_or("-"),
                        entry["name"].as_str().unwrap_or("-"),
                        entry["description"].as_str().unwrap_or("-"),
                    );
                }
            }
        }
        McpCommand::Probe { server } => {
            let Some(mut spec) = mcp.servers.iter().find(|s| s.name == server).cloned() else {
                anyhow::bail!("mcp server `{server}` not found in config");
            };
            scope_mcp_spec_to_allowlist(&mut spec, &mcp.allow_tools);
            let patterns = if mcp.write_tool_patterns.is_empty() {
                default_write_tool_patterns()
            } else {
                mcp.write_tool_patterns.clone()
            };
            let client = McpClient::new(spec, patterns);
            client
                .start()
                .await
                .with_context(|| format!("mcp `{server}`: start failed"))?;
            let tools = client
                .list_tools()
                .await
                .unwrap_or_default()
                .into_iter()
                .filter(|tool| {
                    client.spec().allowed_tools.is_empty()
                        || client
                            .spec()
                            .allowed_tools
                            .iter()
                            .any(|allowed| allowed == &tool.name)
                })
                .collect::<Vec<_>>();
            println!("mcp `{server}`: handshake ok ({} tool(s))", tools.len());
            for tool in tools {
                println!(
                    "  {} | {}",
                    tool.name,
                    tool.description.as_deref().unwrap_or("-")
                );
            }
            client
                .shutdown()
                .await
                .with_context(|| format!("mcp `{server}`: shutdown failed"))?;
        }
    }
    Ok(())
}

/// Resolve the workspace `MayorPatrolConfig`, applying CLI overrides on top
/// of the configured value. We never persist the override — it stays scoped
/// to the running process so a `--max-patrols 1` invocation doesn't bleed
/// into a later `nv mayor` run without the flag.
fn mayor_config_with_overrides(
    config: &Config,
    overrides_queue: Option<&Path>,
    overrides_results: Option<&Path>,
    overrides_max_patrols: Option<u32>,
    overrides_per_patrol_budget: Option<u64>,
) -> nerve_config::MayorPatrolConfig {
    let mut cfg = config
        .orchestration
        .mayor_patrol
        .clone()
        .unwrap_or_default();
    if let Some(dir) = overrides_queue {
        cfg.queue_dir = dir.to_path_buf();
    }
    if let Some(dir) = overrides_results {
        cfg.results_dir = dir.to_path_buf();
    }
    if let Some(max) = overrides_max_patrols {
        cfg.max_patrols = max;
    }
    if let Some(budget) = overrides_per_patrol_budget {
        cfg.per_patrol_budget_microusd = Some(budget);
    }
    cfg
}

async fn run_mayor_subcommand(args: MayorArgs, _mock: bool) -> Result<()> {
    let cwd = env::current_dir().context("failed to read current directory")?;
    let config = Config::load_from(&cwd)?;
    let mp = mayor_config_with_overrides(
        &config,
        args.queue_dir.as_deref(),
        args.results_dir.as_deref(),
        args.max_patrols,
        args.per_patrol_budget_microusd,
    );
    let total = config.orchestration.budget_cost_microusd_ceiling;
    let mayor = Mayor::new(mp, cwd.clone(), total);
    let status = mayor.status().await.context("mayor status failed")?;
    print_mayor_status(&status);
    if args.status_only {
        return Ok(());
    }
    // Drain the queue once. Long-lived supervisor mode is reserved for the
    // upcoming `mayor --supervise` flag — out of scope for v1.0's CLI bring-up.
    mayor
        .run_until_idle()
        .await
        .context("mayor run_until_idle failed")?;
    let final_status = mayor.status().await?;
    print_mayor_status(&final_status);
    Ok(())
}

fn print_mayor_status(status: &nerve_core::MayorStatus) {
    println!(
        "mayor: pending={} claimed={} done={} failed={} orphans_recovered={} active_patrols={}",
        status.pending_count,
        status.claimed_count,
        status.done_count,
        status.failed_count,
        status.orphans_recovered,
        status.active_patrols.len(),
    );
    for patrol in &status.active_patrols {
        println!("  patrol heartbeat: {patrol}");
    }
}

async fn run_patrol_subcommand(args: PatrolArgs, mock: bool) -> Result<()> {
    let cwd = env::current_dir().context("failed to read current directory")?;
    let config = Config::load_from(&cwd)?;
    let mp = config
        .orchestration
        .mayor_patrol
        .clone()
        .unwrap_or_default();
    let patrol = Patrol::new(args.id.clone(), mp.clone(), cwd.clone());

    if args.status {
        // Print local heartbeat + any results that match this patrol id.
        let session_meta = cwd.join(".nerve").join("session-meta");
        let token_path = patrol_rpc_token_path(&session_meta, &args.id);
        println!(
            "patrol `{}`: token={} worktree={}",
            args.id,
            token_path.display(),
            args.worktree
                .as_deref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "-".to_string())
        );
        if !args.mcp_server.is_empty() {
            println!("  mcp servers: {}", args.mcp_server.join(", "));
        }
        return Ok(());
    }

    // Tier 3j patrol RPC token isolation: each patrol gets its own bearer
    // token under `.nerve/session-meta/rpc-token-<id>` (0600). We materialise
    // the bus on a temporary rpc-config so the global token is never touched.
    let session_meta = cwd.join(".nerve").join("session-meta");
    fs::create_dir_all(&session_meta).with_context(|| {
        format!(
            "failed to create RPC session-meta dir `{}`",
            session_meta.display()
        )
    })?;
    let rpc_config = patrol_rpc_config(&config, &args.id);
    let _bus = Arc::new(
        RpcBus::new(rpc_config, &cwd)
            .with_context(|| format!("rpc bus init failed for patrol `{}`", args.id))?,
    );

    if args.once {
        // One-shot dispatch: claim a task, dispatch it through the in-process
        // synaptic loop, and write the result.
        let outcome = patrol
            .run_one(|task: PatrolTask| {
                Box::pin(
                    async move { Ok(nerve_core::PatrolResult::success(&task.task_id, "stub", 0)) },
                )
            })
            .await
            .context("patrol run_one failed")?;
        println!(
            "patrol `{}`: ran task {} verdict={:?} cost={}",
            args.id, outcome.task_id, outcome.verdict, outcome.cost_microusd
        );
        return Ok(());
    }

    let _ = mock; // adapter mode currently selected via config, not patrol arg
    let (_tx, rx) = watch::channel(false);
    patrol
        .run_loop(
            |task: PatrolTask| {
                Box::pin(
                    async move { Ok(nerve_core::PatrolResult::success(&task.task_id, "stub", 0)) },
                )
            },
            rx,
        )
        .await
        .context("patrol run_loop failed")?;
    Ok(())
}

fn patrol_rpc_token_path(session_meta: &Path, id: &str) -> PathBuf {
    session_meta.join(format!("rpc-token-{id}"))
}

/// Build an `RpcConfig` whose `token_path` is the patrol-isolated bearer
/// token file. We clone the workspace config so token-size / queue knobs
/// stay consistent with non-patrol callers.
fn patrol_rpc_config(config: &Config, id: &str) -> RpcConfig {
    let mut rpc = config.daemon.rpc.clone().unwrap_or_default();
    // RpcBus resolves relative paths against the workspace root.
    rpc.token_path = PathBuf::from(".nerve")
        .join("session-meta")
        .join(format!("rpc-token-{id}"));
    rpc
}

/// Run the v0.3.0 doctor surface. Loads config, runs `nerve_core::doctor_checks`
/// for the workspace, then layers the adapter-prerequisite checks. Returns
/// `Err` if any check is `Fail` or (for real adapters) a CLI is missing.
fn run_doctor(mock: bool, stdout: &mut dyn Write, stderr: &mut dyn Write) -> Result<()> {
    let cwd = env::current_dir().context("failed to read current directory")?;
    let config = Config::load_from(&cwd)?;
    writeln!(stdout, "config: ok").ok();

    let checks = doctor_checks(&config, &cwd);
    let mut any_fail = false;
    for check in &checks {
        render_doctor_check(check, stdout, stderr);
        if matches!(check.status, DoctorStatus::Fail(_)) {
            any_fail = true;
        }
    }

    // v0.5.0 Tier 2e: rpc-token file permission (0600) and envelope schema
    // version compatibility checks. We surface these even when the daemon
    // protocol is `line` because operators may switch protocols at runtime.
    let rpc_config = config.daemon.rpc.clone().unwrap_or_default();
    for check in rpc_doctor_checks(&cwd, &rpc_config) {
        render_doctor_check(&check, stdout, stderr);
        if matches!(check.status, DoctorStatus::Fail(_)) {
            any_fail = true;
        }
    }

    // v1.0 Tier 3h/3i/3j doctor extensions.
    for check in v1_doctor_checks(&config, &cwd) {
        render_doctor_check(&check, stdout, stderr);
        if matches!(check.status, DoctorStatus::Fail(_)) {
            any_fail = true;
        }
    }

    if mock {
        writeln!(stdout, "adapter: mock ok").ok();
        if any_fail {
            anyhow::bail!("nv doctor reported failing checks; review output above");
        }
        return Ok(());
    }

    let claude = find_on_path("claude");
    let codex = find_on_path("codex");
    writeln_doctor_check(stdout, "claude", &claude);
    writeln_doctor_check(stdout, "codex", &codex);
    writeln_auth_status(stdout, "claude", &claude, &["auth", "status"]);
    writeln_auth_status(stdout, "codex", &codex, &["login", "status"]);
    if any_fail {
        anyhow::bail!("nv doctor reported failing checks; review output above");
    }
    if claude.is_some() && codex.is_some() {
        Ok(())
    } else {
        anyhow::bail!("real adapter prerequisites are missing")
    }
}

/// Tier 2e doctor checks. We verify two invariants:
///
///   1. The bearer-token file (when it exists) is `0600`. A wider mode
///      could let other users on the host read or replace the token, so we
///      treat anything else as a `Fail`.
///   2. The configured `envelope_version` is compatible with the runtime
///      [`RPC_SCHEMA_VERSION`]. We only check the major version (semver) so
///      additive payload extensions don't trip the gate.
fn rpc_doctor_checks(cwd: &Path, rpc_config: &RpcConfig) -> Vec<DoctorCheck> {
    let mut checks = Vec::new();

    let token_path = if rpc_config.token_path.is_absolute() {
        rpc_config.token_path.clone()
    } else {
        cwd.join(".nerve")
            .join("session-meta")
            .join(rpc_config.token_path.file_name().unwrap_or_default())
    };

    let status = rpc_token_permission_status(&token_path);
    checks.push(DoctorCheck {
        name: "rpc_token_perm".to_string(),
        status,
    });

    let status = envelope_version_status(&rpc_config.envelope_version);
    checks.push(DoctorCheck {
        name: "rpc_envelope_version".to_string(),
        status,
    });

    checks
}

/// Tier 3h/3i/3j doctor checks. Each guard is non-fatal by default so
/// existing operators see no new failures unless they have explicitly opted
/// into the v1.0 features.
fn v1_doctor_checks(config: &Config, cwd: &Path) -> Vec<DoctorCheck> {
    let mut checks = Vec::new();

    // 3h: validate `.nerve/sessions/index.json` is well-formed and has no
    // dangling parent_id pointers.
    checks.push(DoctorCheck {
        name: "sessions_index".to_string(),
        status: sessions_index_status(cwd),
    });

    // 3i: per-server `command[0]` must resolve on PATH.
    if let Some(mcp) = active_mcp_config(config) {
        for spec in &mcp.servers {
            checks.push(DoctorCheck {
                name: format!("mcp_server_{}", spec.name),
                status: mcp_command_status(spec),
            });
        }
    }

    // 3j: writable queue/results dirs + orphan scan.
    if let Some(mp) = config.orchestration.mayor_patrol.as_ref() {
        let queue_root = if mp.queue_dir.is_absolute() {
            mp.queue_dir.clone()
        } else {
            cwd.join(&mp.queue_dir)
        };
        let results_root = if mp.results_dir.is_absolute() {
            mp.results_dir.clone()
        } else {
            cwd.join(&mp.results_dir)
        };
        checks.push(DoctorCheck {
            name: "mayor_queue_dir".to_string(),
            status: writable_dir_status(&queue_root),
        });
        checks.push(DoctorCheck {
            name: "mayor_results_dir".to_string(),
            status: writable_dir_status(&results_root),
        });
        checks.push(DoctorCheck {
            name: "mayor_orphaned_claims".to_string(),
            status: orphaned_claim_status(&queue_root),
        });
    }

    checks
}

fn sessions_index_status(cwd: &Path) -> DoctorStatus {
    let path = cwd.join(".nerve").join("sessions").join("index.json");
    if !path.exists() {
        return DoctorStatus::Ok;
    }
    let raw = match fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(err) => {
            return DoctorStatus::Warn(format!("could not read `{}`: {err}", path.display()));
        }
    };
    if raw.trim().is_empty() {
        return DoctorStatus::Ok;
    }
    let value: serde_json::Value = match serde_json::from_str(&raw) {
        Ok(v) => v,
        Err(err) => {
            return DoctorStatus::Fail(format!("`{}` is not valid JSON: {err}", path.display()));
        }
    };
    let Some(entries) = value.get("entries").and_then(|v| v.as_object()) else {
        return DoctorStatus::Fail(format!("`{}` is missing the `entries` map", path.display()));
    };
    // Scan for dangling parent_id pointers.
    let ids: std::collections::HashSet<&str> = entries.keys().map(String::as_str).collect();
    for (id, entry) in entries {
        let Some(parent) = entry.get("parent_id").and_then(|v| v.as_str()) else {
            continue;
        };
        if !ids.contains(parent) {
            return DoctorStatus::Fail(format!(
                "session `{id}` references missing parent `{parent}`"
            ));
        }
    }
    DoctorStatus::Ok
}

fn mcp_command_status(spec: &nerve_types::McpServerSpec) -> DoctorStatus {
    let Some(first) = spec.command.first() else {
        return DoctorStatus::Fail(format!("`{}`: empty command argv", spec.name));
    };
    // Accept absolute paths as long as they resolve. Otherwise look up on PATH.
    let candidate = PathBuf::from(first);
    if candidate.is_absolute() {
        return if is_executable_file(&candidate) {
            DoctorStatus::Ok
        } else {
            DoctorStatus::Fail(format!(
                "`{}`: `{}` is not an executable file",
                spec.name,
                candidate.display()
            ))
        };
    }
    if find_on_path(first).is_some() {
        DoctorStatus::Ok
    } else {
        DoctorStatus::Fail(format!("`{}`: `{first}` not found on PATH", spec.name))
    }
}

fn writable_dir_status(dir: &Path) -> DoctorStatus {
    if let Err(err) = fs::create_dir_all(dir) {
        return DoctorStatus::Fail(format!("`{}`: create failed: {err}", dir.display()));
    }
    let probe = dir.join(".doctor-write-probe");
    match fs::write(&probe, b"ok") {
        Ok(()) => {
            let _ = fs::remove_file(&probe);
            DoctorStatus::Ok
        }
        Err(err) => DoctorStatus::Fail(format!("`{}` not writable: {err}", dir.display())),
    }
}

/// Surface orphaned `claimed/<patrol>/<task>.json` entries — defined as any
/// directory under `claimed/` that contains JSON files. Operators can run
/// `nv mayor` (which kicks off orphan recovery) to clean these up.
fn orphaned_claim_status(queue_root: &Path) -> DoctorStatus {
    let claimed = queue_root.join("claimed");
    if !claimed.exists() {
        return DoctorStatus::Ok;
    }
    let mut orphans = 0usize;
    let mut patrols = Vec::new();
    let read_dir = match fs::read_dir(&claimed) {
        Ok(rd) => rd,
        Err(err) => {
            return DoctorStatus::Warn(format!("could not read `{}`: {err}", claimed.display()));
        }
    };
    for entry in read_dir.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let patrol_id = path
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        if let Ok(sub) = fs::read_dir(&path) {
            for child in sub.flatten() {
                if child.path().extension().and_then(|s| s.to_str()) == Some("json") {
                    orphans += 1;
                    patrols.push(patrol_id.clone());
                    break;
                }
            }
        }
    }
    if orphans == 0 {
        DoctorStatus::Ok
    } else {
        DoctorStatus::Fail(format!(
            "{orphans} orphaned claim(s) under `{}` (patrols: {}). Run `nv mayor --status-only` to recover.",
            claimed.display(),
            patrols.join(", ")
        ))
    }
}

#[cfg(unix)]
fn rpc_token_permission_status(token_path: &Path) -> DoctorStatus {
    use std::os::unix::fs::PermissionsExt;
    if !token_path.exists() {
        return DoctorStatus::Ok;
    }
    match fs::metadata(token_path) {
        Ok(metadata) => {
            let mode = metadata.permissions().mode() & 0o777;
            if mode == 0o600 {
                DoctorStatus::Ok
            } else {
                DoctorStatus::Fail(format!(
                    "{} has mode {mode:o}, expected 0600",
                    token_path.display()
                ))
            }
        }
        Err(err) => DoctorStatus::Warn(format!("could not stat `{}`: {err}", token_path.display())),
    }
}

#[cfg(not(unix))]
fn rpc_token_permission_status(_token_path: &Path) -> DoctorStatus {
    // Non-unix targets don't expose octal modes; assume ok.
    DoctorStatus::Ok
}

/// Compare `configured` against the runtime [`RPC_SCHEMA_VERSION`] on the
/// major-version axis. Anything that fails to parse as `MAJOR.MINOR.PATCH`
/// is a `Fail`.
fn envelope_version_status(configured: &str) -> DoctorStatus {
    let runtime_major = match major_version(RPC_SCHEMA_VERSION) {
        Some(v) => v,
        None => {
            return DoctorStatus::Warn(format!(
                "runtime RPC_SCHEMA_VERSION `{RPC_SCHEMA_VERSION}` is not semver-shaped",
            ));
        }
    };
    let configured_major = match major_version(configured) {
        Some(v) => v,
        None => {
            return DoctorStatus::Fail(format!(
                "daemon.rpc.envelope_version `{configured}` is not semver-shaped",
            ));
        }
    };
    if runtime_major == configured_major {
        DoctorStatus::Ok
    } else {
        DoctorStatus::Fail(format!(
            "daemon.rpc.envelope_version `{configured}` major mismatches runtime `{RPC_SCHEMA_VERSION}`",
        ))
    }
}

/// Strip the major version prefix (`1`) from a `MAJOR.MINOR.PATCH` string.
fn major_version(version: &str) -> Option<u32> {
    version.split('.').next().and_then(|s| s.parse().ok())
}

fn render_doctor_check(check: &DoctorCheck, stdout: &mut dyn Write, stderr: &mut dyn Write) {
    match &check.status {
        DoctorStatus::Ok => {
            writeln!(stdout, "{}: ok", check.name).ok();
        }
        DoctorStatus::Warn(msg) => {
            writeln!(stdout, "{}: warn ({msg})", check.name).ok();
        }
        DoctorStatus::Fail(msg) => {
            // sec-gap-12: emit chain breakage in RED to stderr so CI surfaces it.
            writeln!(stderr, "{}: fail ({msg})", check.name).ok();
        }
    }
}

fn run_setup(mock: bool) -> Result<()> {
    let cwd = env::current_dir().context("failed to read current directory")?;
    let store = NerveStore::new(&cwd);
    store.init()?;
    println!("store: {}", cwd.join(".nerve").display());
    run_doctor(mock, &mut std::io::stdout(), &mut std::io::stderr())
}

fn run_login(provider: LoginProvider) -> Result<()> {
    match provider {
        LoginProvider::All => {
            run_provider_login(LoginProvider::Claude)?;
            run_provider_login(LoginProvider::Codex)
        }
        LoginProvider::Claude | LoginProvider::Codex => run_provider_login(provider),
    }
}

fn run_provider_login(provider: LoginProvider) -> Result<()> {
    let (name, binary, args): (&str, &str, &[&str]) = match provider {
        LoginProvider::Claude => ("claude", "claude", &["auth", "login"]),
        LoginProvider::Codex => ("codex", "codex", &["login"]),
        LoginProvider::All => unreachable!("all is expanded before provider login"),
    };

    let Some(path) = find_on_path(binary) else {
        anyhow::bail!("{name}: missing from PATH");
    };
    println!("{name}: starting login via {}", path.display());
    let status = StdCommand::new(path)
        .args(args)
        .status()
        .with_context(|| format!("failed to start {name} login"))?;
    if status.success() {
        println!("{name}: login completed");
        Ok(())
    } else {
        anyhow::bail!("{name}: login exited with status {status}");
    }
}

async fn run_interactive(
    apply: bool,
    mock: bool,
    worktree_override: Option<bool>,
    force_tui: bool,
) -> Result<()> {
    let cwd = env::current_dir().context("failed to read current directory")?;
    let tui_decision = match Config::load_from(&cwd) {
        Ok(config) => decide_tui(force_tui, &config),
        Err(_) => TuiDecision {
            use_tui: false,
            refresh_ms: 100,
            log_height_pct: 60,
            suppressed_reason: Some(TuiSuppression::Disabled),
        },
    };
    if tui_decision.use_tui {
        return run_interactive_tui(apply, mock, worktree_override, tui_decision).await;
    }

    let mut state = InteractiveState::new(apply, mock, worktree_override);
    state.refresh_counts();
    refresh_active_goal(&mut state, &cwd);
    warn_if_audit_chain_broken(&cwd);
    print_interactive_banner(&state);
    if std::io::stdin().is_terminal() && std::io::stdout().is_terminal() {
        run_interactive_terminal(&mut state).await
    } else {
        run_interactive_lines(&mut state).await
    }
}

/// Outcome of [`decide_tui`] — whether to enable the v0.5.0 ratatui front-end
/// and the resolved TUI knobs to feed [`TuiAppOptions`].
#[derive(Debug, Clone, Copy)]
struct TuiDecision {
    use_tui: bool,
    refresh_ms: u64,
    log_height_pct: u8,
    /// Why TUI was suppressed (for diagnostics). `None` when `use_tui` is true.
    /// Read by `tui_skips_when_non_tty` and by future telemetry hooks.
    #[allow(dead_code)]
    suppressed_reason: Option<TuiSuppression>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TuiSuppression {
    /// Neither `--tui` nor auto-enable conditions matched.
    NotRequested,
    /// stdin or stdout is not a TTY; we refuse to clobber piped output.
    NotATty,
    /// `--tui` was requested but explicit `tui.enabled = false` overrides it.
    Disabled,
}

/// Resolve whether the interactive shell should hand off to the ratatui TUI.
///
/// Precedence (matches the task spec): explicit `--tui` flag wins when the
/// TTY check passes; otherwise we honour `tui.auto_in_cmux` + `CMUX_SESSION`.
/// Any non-TTY stream forces the legacy plain shell so piped consumers still
/// receive line-oriented output.
fn decide_tui(force_tui: bool, config: &Config) -> TuiDecision {
    let refresh_ms = config.tui.refresh_ms.max(16);
    let log_height_pct = config.tui.log_height_pct.clamp(10, 90);

    if !config.tui.enabled && !force_tui {
        return TuiDecision {
            use_tui: false,
            refresh_ms,
            log_height_pct,
            suppressed_reason: Some(TuiSuppression::Disabled),
        };
    }
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        return TuiDecision {
            use_tui: false,
            refresh_ms,
            log_height_pct,
            suppressed_reason: Some(TuiSuppression::NotATty),
        };
    }

    let auto = config.tui.enabled && config.tui.auto_in_cmux && env::var("CMUX_SESSION").is_ok();
    if !force_tui && !auto {
        return TuiDecision {
            use_tui: false,
            refresh_ms,
            log_height_pct,
            suppressed_reason: Some(TuiSuppression::NotRequested),
        };
    }

    TuiDecision {
        use_tui: true,
        refresh_ms,
        log_height_pct,
        suppressed_reason: None,
    }
}

/// Drive the v0.5.0 Tier 3g 3-pane ratatui shell. We spin three broadcast
/// channels (status / lead / reviewer) plus a watch-based shutdown flag,
/// hand them to `TuiApp::run`, and forward a single placeholder status
/// snapshot so the UI is responsive even before the first synaptic round.
///
/// The full lifecycle wiring (per-iter `TuiState` emit, per-token lead and
/// reviewer chunk fan-out) lands when `nerve-core` exposes a session-bound
/// `RpcBus`. Until then we expose the UI so cmux sessions can verify the
/// pane layout against the design doc without diverging from the daemon
/// transport contract.
async fn run_interactive_tui(
    _apply: bool,
    _mock: bool,
    _worktree_override: Option<bool>,
    decision: TuiDecision,
) -> Result<()> {
    let (state_tx, state_rx) = broadcast::channel::<TuiState>(256);
    let (lead_tx, lead_rx) = broadcast::channel::<String>(256);
    let (reviewer_tx, reviewer_rx) = broadcast::channel::<String>(256);
    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    // Emit an initial status so the bottom-right pane shows real content on
    // first paint instead of an empty widget. Failures (no subscribers yet)
    // are ignored — the TuiApp draws once before subscribing to `recv`.
    let _ = state_tx.send(TuiState {
        note: Some("tui ready (interactive)".to_string()),
        ..TuiState::default()
    });

    let app = TuiApp::new(TuiAppOptions {
        refresh_ms: decision.refresh_ms,
        log_height_pct: decision.log_height_pct,
    });

    // Run the TUI on the current task so Ctrl-C / Esc / `q` propagates
    // through the shutdown watch to the supervisor's join point below.
    let tui_handle =
        tokio::spawn(async move { app.run(state_rx, lead_rx, reviewer_rx, shutdown_rx).await });

    // Hold the publisher ends alive until the TUI exits. Dropping them
    // before the TUI returns would close the broadcast channels and force
    // `RecvError::Closed`.
    let _retain = (state_tx, lead_tx, reviewer_tx);

    let result = tui_handle.await.context("TUI task panicked")?;
    // Flip shutdown so any other observers can react; ignore send errors
    // because the watch may already be dropped.
    let _ = shutdown_tx.send(true);
    result.context("TUI driver failed")
}

/// Print the resolved system prompt + section validation summary for the
/// CLI `nv plan` command, then emit the lead's plan markdown. Reviewer
/// feedback (`--dual-review`) is appended below the plan body.
async fn run_plan_subcommand(args: PlanArgs, mock: bool, json: bool) -> Result<()> {
    let task_cwd = match args.cwd {
        Some(path) => path,
        None => env::current_dir().context("failed to read current directory")?,
    };
    let config = Config::load_from(&task_cwd)?;
    let requested_strategy = if args.dual_review {
        PlanStrategy::DualReview
    } else {
        PlanStrategy::Single
    };

    let mut task = Task::new(args.task, &task_cwd);
    task.context_paths = collect_context_paths(&task.prompt, &task_cwd);
    let adapters = adapters_for_config(mock, &config);
    let report = run_plan_mode(
        task,
        Arc::new(config),
        adapters,
        PlanRunOptions::new(requested_strategy),
    )
    .await
    .map_err(plan_error_to_anyhow)?;

    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
        return Ok(());
    }
    print_plan_report(&report);
    Ok(())
}

fn plan_error_to_anyhow(err: PlanError) -> anyhow::Error {
    anyhow::anyhow!("{err}")
}

/// Render a [`PlanReport`] for human consumption: header + plan markdown
/// + optional reviewer commentary + cost / LOC summary.
fn print_plan_report(report: &PlanReport) {
    println!("Nerve plan {}", report.task_id);
    println!("{}", "=".repeat(72));
    println!("{}", report.plan_markdown.trim_end());
    if !report.reviewer_feedback.is_empty() {
        println!("{}", "-".repeat(72));
        println!("Reviewer feedback:");
        println!("{}", report.reviewer_feedback.trim_end());
    }
    println!("{}", "-".repeat(72));
    let cost_label = report
        .cost
        .as_ref()
        .and_then(|c| c.estimated_cost_microusd.map(format_cost_microusd))
        .unwrap_or_else(|| "-".to_string());
    let loc_label = report
        .estimated_loc
        .map(|loc| loc.to_string())
        .unwrap_or_else(|| "-".to_string());
    println!(
        "summary: cost={} estimated_loc={} estimated_files={}",
        cost_label,
        loc_label,
        report.estimated_files.len()
    );
}

fn run_rpc_subcommand(command: RpcCommand) -> Result<()> {
    match command {
        RpcCommand::RotateToken => rpc_rotate_token(),
    }
}

/// Tier 2e RPC token rotation. Reconstructs `RpcBus` against the workspace
/// `.nerve/session-meta` directory (creating the parent dir if needed) and
/// writes a fresh 32-byte bearer token in `0600` mode.
fn rpc_rotate_token() -> Result<()> {
    let cwd = env::current_dir().context("failed to read current directory")?;
    let session_meta = cwd.join(".nerve").join("session-meta");
    fs::create_dir_all(&session_meta).with_context(|| {
        format!(
            "failed to create RPC session-meta dir `{}`",
            session_meta.display()
        )
    })?;
    let config = Config::load_from(&cwd)?;
    let rpc_config = config.daemon.rpc.clone().unwrap_or_default();
    let token_path = if rpc_config.token_path.is_absolute() {
        rpc_config.token_path.clone()
    } else {
        cwd.join(&rpc_config.token_path)
    };
    let bus = RpcBus::new(rpc_config, &cwd)
        .with_context(|| "failed to open RPC bus for token rotation")?;
    let new_token = bus
        .rotate_token(&cwd)
        .with_context(|| "failed to rotate RPC bearer token")?;
    println!(
        "rotated rpc bearer token (32 bytes / {} hex chars)",
        new_token.len()
    );
    println!("token written to {}", token_path.display());
    Ok(())
}

/// Interactive `/plan` slash handler. Routes a natural-language task through
/// `run_plan_mode` and writes the plan markdown + reviewer commentary back to
/// the terminal. Per the design doc, plan mode must never apply a patch — we
/// therefore don't touch `state.last_report` so subsequent `/apply` calls
/// see the previous coding session's patch, not the plan output.
async fn run_interactive_plan(prompt: String, state: &InteractiveState) -> Result<()> {
    let cwd = env::current_dir().context("failed to read current directory")?;
    let config = Config::load_from(&cwd)?;
    let mut task = Task::new(prompt, &cwd);
    task.context_paths = collect_context_paths(&task.prompt, &cwd);
    let adapters = adapters_for_config(state.mock, &config);
    let report = run_plan_mode(
        task,
        Arc::new(config),
        adapters,
        PlanRunOptions::new(PlanStrategy::Single),
    )
    .await
    .map_err(plan_error_to_anyhow)?;
    print_plan_report(&report);
    Ok(())
}

/// sec-gap-12: surface a RED warning on stderr when the on-disk audit chain
/// has a tampered or missing prev_hash link. Empty/Intact chains stay silent.
fn warn_if_audit_chain_broken(cwd: &Path) {
    let path = budget_audit_path(cwd);
    if !path.exists() {
        return;
    }
    match AuditChainState::verify(&path) {
        Ok(status @ ChainStatus::Broken { .. }) => {
            if let Some(msg) = format_chain_broken(&status) {
                eprintln!("{msg}");
                eprintln!(
                    "Hint: run `nv doctor` to confirm and consult docs/audit-recovery before continuing.",
                );
            }
        }
        Ok(_) => {}
        Err(err) => {
            eprintln!("warning: budget audit chain could not be verified: {err}");
        }
    }
}

async fn run_interactive_lines(state: &mut InteractiveState) -> Result<()> {
    let stdin = io::BufReader::new(io::stdin());
    let mut lines = stdin.lines();
    let mut paste_lines: Option<Vec<String>> = None;

    loop {
        if paste_lines.is_some() {
            print!("paste> ");
        } else {
            print!("{}", interactive_prompt(state));
        }
        std::io::stdout().flush()?;
        let Some(line) = lines.next_line().await? else {
            break;
        };
        println!();
        if process_interactive_input(line.trim_end(), state, &mut paste_lines).await? {
            break;
        }
    }

    Ok(())
}

async fn run_interactive_terminal(state: &mut InteractiveState) -> Result<()> {
    let mut raw_guard = enable_stdin_raw_if_terminal()?;
    let mut editor = InteractiveLineEditor::new();
    let mut paste_lines: Option<Vec<String>> = None;

    loop {
        let prompt = if paste_lines.is_some() {
            "paste> ".to_string()
        } else {
            interactive_prompt(state)
        };
        match editor.read_line(&prompt)? {
            EditorRead::Line(line) => {
                raw_guard.suspend()?;
                let should_exit = process_interactive_input(&line, state, &mut paste_lines).await;
                raw_guard.resume()?;
                if should_exit? {
                    break;
                }
            }
            EditorRead::Interrupted => {
                raw_guard.suspend()?;
                println!("Interrupted. Type /quit to exit.");
                raw_guard.resume()?;
            }
            EditorRead::Eof => break,
        }
    }

    Ok(())
}

async fn process_interactive_input(
    raw_input: &str,
    state: &mut InteractiveState,
    paste_lines: &mut Option<Vec<String>>,
) -> Result<bool> {
    if let Some(lines) = paste_lines.as_mut() {
        match raw_input {
            "/end" => {
                let prompt = lines.join("\n").trim().to_string();
                *paste_lines = None;
                if prompt.is_empty() {
                    println!("Paste cancelled: empty task.");
                } else if let Err(error) = run_interactive_task(prompt, state).await {
                    print_interactive_error(&error);
                }
            }
            "/cancel" => {
                *paste_lines = None;
                println!("Paste cancelled.");
            }
            _ => lines.push(raw_input.to_string()),
        }
        return Ok(false);
    }

    let input = raw_input.trim();
    if input.is_empty() {
        return Ok(false);
    }
    if input == "?" {
        print_interactive_help();
        return Ok(false);
    }
    if let Some(shell_command) = input.strip_prefix('!') {
        if let Err(error) = run_interactive_shell(shell_command.trim()) {
            print_interactive_error(&error);
        }
        return Ok(false);
    }
    if input == "/paste" {
        println!("Paste a multiline task. Finish with /end or cancel with /cancel.");
        *paste_lines = Some(Vec::new());
        return Ok(false);
    }
    if let Some(command) = input.strip_prefix('/') {
        return match handle_interactive_command(command, state).await {
            Ok(should_exit) => Ok(should_exit),
            Err(error) => {
                print_interactive_error(&error);
                Ok(false)
            }
        };
    }
    if let Err(error) = run_interactive_task(input.to_string(), state).await {
        print_interactive_error(&error);
    }
    Ok(false)
}

#[derive(Debug, Clone, Copy)]
struct InteractiveCommandSpec {
    command: &'static str,
    args: &'static str,
    description: &'static str,
}

const INTERACTIVE_COMMANDS: &[InteractiveCommandSpec] = &[
    InteractiveCommandSpec {
        command: "/login",
        args: "",
        description: "authenticate Claude and Codex",
    },
    InteractiveCommandSpec {
        command: "/doctor",
        args: "",
        description: "inspect config, adapters, and auth",
    },
    InteractiveCommandSpec {
        command: "/status",
        args: "",
        description: "show current workspace state",
    },
    InteractiveCommandSpec {
        command: "/mode",
        args: "<dry-run|apply>",
        description: "switch apply behavior",
    },
    InteractiveCommandSpec {
        command: "/adapter",
        args: "<real|mock>",
        description: "switch provider adapter",
    },
    InteractiveCommandSpec {
        command: "/cd",
        args: "<path>",
        description: "change workspace directory",
    },
    InteractiveCommandSpec {
        command: "/pwd",
        args: "",
        description: "print workspace directory",
    },
    InteractiveCommandSpec {
        command: "/clear",
        args: "",
        description: "redraw terminal workspace",
    },
    InteractiveCommandSpec {
        command: "/paste",
        args: "",
        description: "enter a multiline task",
    },
    InteractiveCommandSpec {
        command: "/diff",
        args: "",
        description: "show last reviewed patch",
    },
    InteractiveCommandSpec {
        command: "/apply",
        args: "[patch-id]",
        description: "apply last or selected patch",
    },
    InteractiveCommandSpec {
        command: "/rollback",
        args: "[patch-id]",
        description: "roll back last or selected patch",
    },
    InteractiveCommandSpec {
        command: "/history",
        args: "",
        description: "show recent sessions",
    },
    InteractiveCommandSpec {
        command: "/resume",
        args: "<session-id>",
        description: "print stored session report",
    },
    InteractiveCommandSpec {
        command: "/list",
        args: "",
        description: "list stored patches",
    },
    InteractiveCommandSpec {
        command: "/templates",
        args: "",
        description: "list prompt templates",
    },
    InteractiveCommandSpec {
        command: "/template",
        args: "<id> [args]",
        description: "run prompt template",
    },
    InteractiveCommandSpec {
        command: "/benchmark",
        args: "pi [iterations]",
        description: "run the Pi workflow benchmark",
    },
    InteractiveCommandSpec {
        command: "/goal",
        args: "<argv | :nl <prose> | clear | show>",
        description: "register a deterministic stop condition (argv or LLM-converted prose)",
    },
    InteractiveCommandSpec {
        command: "/plan",
        args: "<task>",
        description: "run /plan (read-only analysis; never produces a patch)",
    },
    InteractiveCommandSpec {
        command: "/budget",
        args: "<show | cost=$X | tokens=N>",
        description: "inspect or override session budget caps",
    },
    InteractiveCommandSpec {
        command: "/fork",
        args: "[--from-round N] [--name NAME]",
        description: "branch the current session into a child fork",
    },
    InteractiveCommandSpec {
        command: "/mcp",
        args: "list | call <server> <tool> <json>",
        description: "inspect MCP servers or dispatch a tool",
    },
    InteractiveCommandSpec {
        command: "/mayor",
        args: "status",
        description: "show Mayor queue depths and active patrols",
    },
    InteractiveCommandSpec {
        command: "/patrol",
        args: "status",
        description: "show local Patrol heartbeat and recent results",
    },
    InteractiveCommandSpec {
        command: "/help",
        args: "",
        description: "show command help",
    },
    InteractiveCommandSpec {
        command: "/quit",
        args: "",
        description: "exit Nerve",
    },
];

#[derive(Debug, Clone, PartialEq, Eq)]
enum EditorRead {
    Line(String),
    Interrupted,
    Eof,
}

struct InteractiveLineEditor {
    buffer: String,
    history: Vec<String>,
    history_index: Option<usize>,
    draft: String,
    selected_suggestion: usize,
}

impl InteractiveLineEditor {
    fn new() -> Self {
        Self {
            buffer: String::new(),
            history: Vec::new(),
            history_index: None,
            draft: String::new(),
            selected_suggestion: 0,
        }
    }

    fn read_line(&mut self, prompt: &str) -> Result<EditorRead> {
        self.buffer.clear();
        self.history_index = None;
        self.draft.clear();
        self.selected_suggestion = 0;
        self.render(prompt)?;

        let stdin = std::io::stdin();
        let mut stdin = stdin.lock();
        loop {
            let mut byte = [0_u8; 1];
            if let Err(error) = stdin.read_exact(&mut byte) {
                if error.kind() == std::io::ErrorKind::UnexpectedEof {
                    self.finish_render(prompt)?;
                    return Ok(EditorRead::Eof);
                }
                return Err(error).context("failed to read terminal input");
            }

            match byte[0] {
                b'\r' | b'\n' => {
                    self.finish_render(prompt)?;
                    let line = self.buffer.trim_end().to_string();
                    if !line.is_empty() {
                        self.history.push(line.clone());
                    }
                    return Ok(EditorRead::Line(line));
                }
                3 => {
                    self.finish_render(prompt)?;
                    return Ok(EditorRead::Interrupted);
                }
                4 if self.buffer.is_empty() => {
                    self.finish_render(prompt)?;
                    return Ok(EditorRead::Eof);
                }
                9 => {
                    self.complete_selected_suggestion();
                    self.render(prompt)?;
                }
                8 | 127 => {
                    self.buffer.pop();
                    self.selected_suggestion = 0;
                    self.history_index = None;
                    self.render(prompt)?;
                }
                27 => {
                    self.handle_escape_sequence(&mut stdin);
                    self.render(prompt)?;
                }
                value if value.is_ascii_graphic() || value == b' ' => {
                    self.buffer.push(value as char);
                    self.selected_suggestion = 0;
                    self.history_index = None;
                    self.render(prompt)?;
                }
                _ => {}
            }
        }
    }

    fn handle_escape_sequence(&mut self, stdin: &mut std::io::StdinLock<'_>) {
        let mut sequence = [0_u8; 2];
        if stdin.read_exact(&mut sequence).is_err() || sequence[0] != b'[' {
            return;
        }
        match sequence[1] {
            b'A' => {
                if self.has_command_suggestions() {
                    self.move_suggestion(-1);
                } else {
                    self.move_history(-1);
                }
            }
            b'B' => {
                if self.has_command_suggestions() {
                    self.move_suggestion(1);
                } else {
                    self.move_history(1);
                }
            }
            b'C' => self.complete_selected_suggestion(),
            _ => {}
        }
    }

    fn render(&self, prompt: &str) -> Result<()> {
        print!("\r\x1b[J{prompt}{}", self.buffer);
        let palette_lines = self.command_palette_lines();
        for line in &palette_lines {
            println!("\n{line}");
        }
        if !palette_lines.is_empty() {
            print!("\x1b[{}A\r{prompt}{}", palette_lines.len(), self.buffer);
        }
        std::io::stdout().flush()?;
        Ok(())
    }

    fn finish_render(&self, prompt: &str) -> Result<()> {
        print!("\r\x1b[J{prompt}{}\n", self.buffer);
        std::io::stdout().flush()?;
        Ok(())
    }

    fn command_suggestions(&self) -> Vec<&'static InteractiveCommandSpec> {
        command_suggestions(&self.buffer)
    }

    fn command_palette_lines(&self) -> Vec<String> {
        let suggestions = self.command_suggestions();
        if suggestions.is_empty() {
            return Vec::new();
        }

        let rows: Vec<String> = suggestions
            .iter()
            .enumerate()
            .map(|(index, suggestion)| {
                let marker = if index == self.selected_suggestion {
                    ">"
                } else {
                    " "
                };
                format!(
                    "{marker} {:<12} {:<24} {}",
                    suggestion.command, suggestion.args, suggestion.description
                )
            })
            .collect();

        boxed_lines("Commands", &rows)
            .into_iter()
            .map(|line| format!("  {line}"))
            .collect()
    }

    fn has_command_suggestions(&self) -> bool {
        !self.command_suggestions().is_empty()
    }

    fn complete_selected_suggestion(&mut self) {
        let Some(suggestion) = self
            .command_suggestions()
            .get(self.selected_suggestion)
            .copied()
        else {
            return;
        };
        self.buffer = if suggestion.args.is_empty() {
            suggestion.command.to_string()
        } else {
            format!("{} ", suggestion.command)
        };
        self.selected_suggestion = 0;
    }

    fn move_suggestion(&mut self, delta: isize) {
        let count = self.command_suggestions().len();
        if count == 0 {
            return;
        }
        self.selected_suggestion =
            (self.selected_suggestion as isize + delta).rem_euclid(count as isize) as usize;
    }

    fn move_history(&mut self, delta: isize) {
        if self.history.is_empty() {
            return;
        }
        if self.history_index.is_none() {
            self.draft = self.buffer.clone();
        }

        let next = match (self.history_index, delta) {
            (None, -1) => Some(self.history.len() - 1),
            (None, 1) => None,
            (Some(0), -1) => Some(0),
            (Some(index), -1) => Some(index - 1),
            (Some(index), 1) if index + 1 >= self.history.len() => None,
            (Some(index), 1) => Some(index + 1),
            (current, _) => current,
        };

        self.history_index = next;
        self.buffer = match self.history_index {
            Some(index) => self.history[index].clone(),
            None => self.draft.clone(),
        };
        self.selected_suggestion = 0;
    }
}

fn command_suggestions(input: &str) -> Vec<&'static InteractiveCommandSpec> {
    let trimmed = input.trim_start();
    if !trimmed.starts_with('/') || trimmed.contains(char::is_whitespace) {
        return Vec::new();
    }
    let query = trimmed.to_ascii_lowercase();
    let prefix_matches: Vec<_> = INTERACTIVE_COMMANDS
        .iter()
        .filter(|spec| spec.command.starts_with(&query))
        .take(8)
        .collect();
    if !prefix_matches.is_empty() {
        return prefix_matches;
    }

    let needle = query.trim_start_matches('/');
    if needle.len() < 2 {
        return Vec::new();
    }

    INTERACTIVE_COMMANDS
        .iter()
        .filter(|spec| {
            spec.command.contains(needle)
                || spec.args.to_ascii_lowercase().contains(needle)
                || spec.description.to_ascii_lowercase().contains(needle)
        })
        .take(8)
        .collect()
}

#[derive(Debug, Clone)]
struct InteractiveState {
    apply: bool,
    mock: bool,
    /// Tier 2d (v0.3.0) per-session worktree override carried from CLI flags or
    /// runtime `/mode worktree` toggles. `None` defers to nerve.config.json.
    worktree_override: Option<bool>,
    last_report: Option<RunReport>,
    session_count: usize,
    patch_count: usize,
    cumulative_input_tokens: u64,
    cumulative_output_tokens: u64,
    cumulative_cost_microusd: u64,
    no_progress_counter: u32,
    last_round_count: usize,
    last_max_rounds: u8,
    active_goal: Option<GoalSpec>,
    budget_override_cost_microusd: Option<u64>,
    budget_override_tokens: Option<u64>,
    /// v1.0 Tier 3h: when set, the interactive prompt is anchored to a
    /// non-root session id (carried verbatim from the last `/fork` invocation
    /// or a manual session selection). The prompt suffix renders the short
    /// 8-hex-char form so the operator never loses track of which branch is
    /// active.
    current_session_id: Option<String>,
}

impl InteractiveState {
    fn new(apply: bool, mock: bool, worktree_override: Option<bool>) -> Self {
        Self {
            apply,
            mock,
            worktree_override,
            last_report: None,
            session_count: 0,
            patch_count: 0,
            cumulative_input_tokens: 0,
            cumulative_output_tokens: 0,
            cumulative_cost_microusd: 0,
            no_progress_counter: 0,
            last_round_count: 0,
            last_max_rounds: 0,
            active_goal: None,
            budget_override_cost_microusd: None,
            budget_override_tokens: None,
            current_session_id: None,
        }
    }

    fn adapter_label(&self) -> &'static str {
        if self.mock { "mock" } else { "real" }
    }

    fn apply_label(&self) -> &'static str {
        if self.apply { "apply" } else { "dry-run" }
    }

    fn refresh_counts(&mut self) {
        let Ok(cwd) = env::current_dir() else {
            return;
        };
        let store = NerveStore::new(cwd);
        self.session_count = store.list_sessions().map(|items| items.len()).unwrap_or(0);
        self.patch_count = store.list_patches().map(|items| items.len()).unwrap_or(0);
    }

    fn last_patch_id(&self) -> Option<&str> {
        self.last_report
            .as_ref()
            .and_then(|report| report.final_patch.as_ref())
            .map(|patch| patch.id.as_str())
    }

    fn last_verdict_label(&self) -> &'static str {
        let Some(report) = self.last_report.as_ref() else {
            return "-";
        };
        match report.final_feedback.verdict {
            Verdict::Lgtm => "lgtm",
            Verdict::AcceptWithNits => "nits",
            Verdict::RequestChanges => "changes",
            Verdict::Block => {
                if report.budget_exceeded {
                    "block(budget)"
                } else if report.no_progress_exceeded {
                    "block(no-progress)"
                } else {
                    "block"
                }
            }
        }
    }

    fn record_report(&mut self, report: &RunReport) {
        self.cumulative_input_tokens = self
            .cumulative_input_tokens
            .saturating_add(report.usage.input_tokens);
        self.cumulative_output_tokens = self
            .cumulative_output_tokens
            .saturating_add(report.usage.output_tokens);
        if let Some(cost) = report.usage.estimated_cost_microusd {
            self.cumulative_cost_microusd = self.cumulative_cost_microusd.saturating_add(cost);
        }
        if report.no_progress_exceeded {
            self.no_progress_counter = self.no_progress_counter.saturating_add(1);
        }
        self.last_round_count = report.rounds.len();
        self.last_max_rounds = report.selection.max_refinement_rounds;
    }
}

fn color_enabled() -> bool {
    std::io::stdout().is_terminal() && env::var_os("NO_COLOR").is_none()
}

fn paint(code: &str, value: impl AsRef<str>) -> String {
    let value = value.as_ref();
    if color_enabled() {
        format!("\x1b[{code}m{value}\x1b[0m")
    } else {
        value.to_string()
    }
}

fn accent(value: impl AsRef<str>) -> String {
    paint("1;36", value)
}

fn muted(value: impl AsRef<str>) -> String {
    paint("2", value)
}

fn success(value: impl AsRef<str>) -> String {
    paint("1;32", value)
}

fn warn(value: impl AsRef<str>) -> String {
    paint("1;33", value)
}

fn error_style(value: impl AsRef<str>) -> String {
    paint("1;31", value)
}

fn visible_len(value: &str) -> usize {
    value.chars().count()
}

fn fit_line(value: &str, width: usize) -> String {
    if visible_len(value) <= width {
        return value.to_string();
    }
    if width <= 1 {
        return "…".to_string();
    }
    let mut out: String = value.chars().take(width.saturating_sub(1)).collect();
    out.push('…');
    out
}

fn print_box(title: &str, lines: &[String]) {
    for line in boxed_lines(title, lines) {
        println!("{line}");
    }
}

fn boxed_lines(title: &str, lines: &[String]) -> Vec<String> {
    let inner = SURFACE_WIDTH.saturating_sub(2);
    let title_text = format!(" {title} ");
    let title_len = visible_len(&title_text);
    let dash_len = inner.saturating_sub(title_len);
    let mut rendered = Vec::with_capacity(lines.len() + 2);
    rendered.push(format!("╭{}{}╮", title_text, "─".repeat(dash_len)));
    for line in lines {
        let fitted = fit_line(line, inner.saturating_sub(2));
        let pad = inner.saturating_sub(2).saturating_sub(visible_len(&fitted));
        rendered.push(format!("│ {}{} │", fitted, " ".repeat(pad)));
    }
    rendered.push(format!("╰{}╯", "─".repeat(inner)));
    rendered
}

fn display_path(path: &Path) -> String {
    let raw = path.display().to_string();
    let Some(home) = env::var_os("HOME").or_else(|| env::var_os("USERPROFILE")) else {
        return raw;
    };
    let home = PathBuf::from(home).display().to_string();
    if raw == home {
        "~".to_string()
    } else if let Some(rest) = raw.strip_prefix(&(home + "/")) {
        format!("~/{rest}")
    } else {
        raw
    }
}

fn print_command_section(title: &str, commands: &[(&str, &str)]) {
    let mut lines = Vec::with_capacity(commands.len() + 2);
    lines.push("command                         action".to_string());
    lines.push("───────                         ──────".to_string());
    for (command, description) in commands {
        lines.push(format!("  {command:<30} {description}"));
    }
    print_box(title, &lines);
}

fn print_interactive_banner(state: &InteractiveState) {
    print_box(
        "Nerve Terminal",
        &[
            "Lead implements, reviewer gates, orchestrator keeps the patch auditable.".to_string(),
            "Type a task directly, use /paste for long work, or !cmd for shell context."
                .to_string(),
            "Review-first flow: /diff inspect, /apply commit, /rollback recover.".to_string(),
        ],
    );
    print_interactive_status(state);
}

fn print_interactive_status(state: &InteractiveState) {
    let branch = git_branch_label(env::current_dir().ok().as_deref()).unwrap_or_else(|| "-".into());
    let cwd = env::current_dir()
        .map(|path| display_path(&path))
        .unwrap_or_else(|_| "?".to_string());
    print_box(
        "Workspace",
        &[
            format!(
                "adapter={} mode={} branch={}",
                state.adapter_label(),
                state.apply_label(),
                branch
            ),
            format!(
                "sessions={} patches={} cwd={}",
                state.session_count, state.patch_count, cwd
            ),
        ],
    );
    print_status_bar(state);
    print_next_actions(state);
}

fn print_status_bar(state: &InteractiveState) {
    let bar = render_status_bar(state);
    println!("{bar}");
    let _ = std::io::stdout().flush();
}

fn render_status_bar(state: &InteractiveState) -> String {
    let round_label = if state.last_max_rounds == 0 {
        "r -/-".to_string()
    } else {
        format!("r {}/{}", state.last_round_count, state.last_max_rounds)
    };
    let verdict_value = state.last_verdict_label();
    let verdict_label = match verdict_value {
        "lgtm" => success("ok"),
        "nits" => warn("nits"),
        "changes" => warn("changes"),
        value if value.starts_with("block") => error_style("blocked"),
        _ => "-".to_string(),
    };
    let total_tokens = state
        .cumulative_input_tokens
        .saturating_add(state.cumulative_output_tokens);
    let cost_label = format_cost_microusd(state.cumulative_cost_microusd);
    let goal_label = match state.active_goal.as_ref() {
        Some(spec) => format!("goal {}", spec.id),
        None => "goal -".to_string(),
    };
    let no_progress_label = if state.no_progress_counter == 0 {
        String::new()
    } else {
        format!("  │  np {}", state.no_progress_counter)
    };
    format!(
        "{}  {}  │  {}  │  {}  │  tok {}  │  {}  │  {}{}",
        muted("status"),
        round_label,
        verdict_label,
        cost_label,
        total_tokens,
        goal_label,
        state.apply_label(),
        no_progress_label
    )
}

fn next_action_lines(state: &InteractiveState) -> Vec<String> {
    let safety = if state.apply {
        "apply mode is live: reviewed patches can change files."
    } else {
        "dry-run mode is safe: patches wait for /apply."
    };

    let Some(report) = state.last_report.as_ref() else {
        return vec![
            "Start: type a task, or use /paste for a multi-line request.".to_string(),
            "Inspect: /doctor checks setup; /help shows the command map.".to_string(),
            format!("Safety: {safety}"),
        ];
    };

    let mut lines = Vec::new();
    lines.push(format!(
        "Last: session {}  verdict {:?}  rounds {}",
        short_id(&report.task.id),
        report.final_feedback.verdict,
        report.rounds.len()
    ));

    match report.final_patch.as_ref() {
        Some(patch) if report.applied => {
            lines.push(format!(
                "Applied patch {}. Use /rollback to undo or /diff to inspect.",
                short_id(&patch.id)
            ));
        }
        Some(_patch) if report.blocked => {
            lines.push(
                "Patch is blocked. Use /resume for raw feedback, then revise the task.".to_string(),
            );
        }
        Some(_patch) => {
            lines.push(
                "reviewed patch ready. Use /diff to inspect or /apply to apply it.".to_string(),
            );
        }
        None => {
            lines.push(
                "No structured patch. Use /resume for raw output, then refine the task."
                    .to_string(),
            );
        }
    }

    if state.active_goal.is_some() {
        lines.push("/goal show explains the active stop condition.".to_string());
    } else {
        lines.push(
            "Optional: /goal :nl <done condition> adds a deterministic stop check.".to_string(),
        );
    }
    lines
}

fn print_next_actions(state: &InteractiveState) {
    print_box("Next", &next_action_lines(state));
}

fn format_cost_microusd(microusd: u64) -> String {
    // microusd is integer micro-dollars; format two decimals from the
    // higher cent precision (4 fractional digits) for status bar density.
    let dollars = microusd / 1_000_000;
    let fractional = microusd % 1_000_000;
    // 4-digit fractional gives <0.0001 resolution which is plenty for token cost.
    let four = fractional / 100;
    format!("${dollars}.{:04}", four)
}

fn interactive_prompt(state: &InteractiveState) -> String {
    let patch_hint = state
        .last_patch_id()
        .map(|id| format!(" patch={}", short_id(id)))
        .unwrap_or_default();
    let branch_hint = git_branch_label(env::current_dir().ok().as_deref())
        .map(|branch| format!(":{branch}"))
        .unwrap_or_default();
    let session_hint = state
        .current_session_id
        .as_deref()
        .map(|id| format!(" session={}", short_id(id)))
        .unwrap_or_default();
    accent(format!(
        "nerve:{}:{}{}{}{}> ",
        state.adapter_label(),
        state.apply_label(),
        branch_hint,
        session_hint,
        patch_hint
    ))
}

fn print_interactive_error(error: &anyhow::Error) {
    eprintln!("Error: {error:#}");
    eprintln!(
        "Hint: run /login to authenticate providers, /doctor to inspect setup, or start with NERVE_ADAPTER=mock nv for a local smoke test."
    );
}

async fn run_interactive_task(prompt: String, state: &mut InteractiveState) -> Result<()> {
    print_box(
        "Task",
        &[
            format!("request {}", prompt),
            format!(
                "adapter={} mode={} workspace={}",
                state.adapter_label(),
                state.apply_label(),
                env::current_dir()
                    .map(|path| display_path(&path))
                    .unwrap_or_else(|_| "?".to_string())
            ),
            "lead/reviewer loop running...".to_string(),
        ],
    );
    let apply_requested = state.apply;
    let report = run_report_with_overrides(
        prompt,
        state.apply,
        state.mock,
        state.active_goal.clone(),
        state.budget_override_cost_microusd,
        state.budget_override_tokens,
        state.worktree_override,
    )
    .await?;
    state.record_report(&report);
    // Fresh task at the prompt always resets to a root session so the prompt
    // suffix reflects the new top-level session id rather than a previously
    // anchored fork. `/fork` re-anchors after the fact.
    state.current_session_id = Some(report.task.id.clone());
    state.last_report = Some(report);
    state.refresh_counts();
    if let Some(report) = state.last_report.as_ref() {
        print_interactive_result(report, apply_requested);
    }
    print_status_bar(state);
    print_next_actions(state);
    Ok(())
}

fn print_interactive_result(report: &RunReport, apply_requested: bool) {
    let mut lines = vec![
        format!(
            "session {}  verdict {:?}  rounds {}",
            short_id(&report.task.id),
            report.final_feedback.verdict,
            report.rounds.len()
        ),
        format!(
            "applied={} blocked={} budget_exceeded={}",
            report.applied, report.blocked, report.budget_exceeded
        ),
        format!(
            "usage input={} output={} total={}",
            report.usage.input_tokens,
            report.usage.output_tokens,
            report.usage.total_tokens()
        ),
    ];

    if let Some(patch) = &report.final_patch {
        lines.push(format!("patch {}  files={}", patch.id, patch.files.len()));
        if report.applied {
            lines.push("Applied patch. Use /rollback to undo the last patch.".to_string());
        } else if !apply_requested && !report.blocked {
            lines.push(
                "reviewed patch ready. Use /diff to inspect or /apply to apply it.".to_string(),
            );
        }
    } else {
        lines.push(format!(
            "no structured patch produced. Use /resume {} for raw output.",
            report.task.id
        ));
    }
    print_box("Result", &lines);
}

async fn handle_interactive_command(command: &str, state: &mut InteractiveState) -> Result<bool> {
    let cwd = env::current_dir().context("failed to read current directory")?;
    let mut parts = command.split_whitespace();
    let name = parts.next().unwrap_or("");
    match name {
        "" => print_interactive_help(),
        "help" => print_interactive_help(),
        "login" => run_login(LoginProvider::All)?,
        "doctor" => run_doctor(state.mock, &mut std::io::stdout(), &mut std::io::stderr())?,
        "status" => {
            state.refresh_counts();
            print_interactive_status(state);
        }
        "mode" => {
            let mode = parts.next().context("usage: /mode <dry-run|apply>")?;
            match mode {
                "dry-run" | "dryrun" | "dry" => state.apply = false,
                "apply" => state.apply = true,
                _ => anyhow::bail!("usage: /mode <dry-run|apply>"),
            }
            println!("mode: {}", state.apply_label());
        }
        "adapter" => {
            let adapter = parts.next().context("usage: /adapter <real|mock>")?;
            match adapter {
                "real" => state.mock = false,
                "mock" => state.mock = true,
                _ => anyhow::bail!("usage: /adapter <real|mock>"),
            }
            println!("adapter: {}", state.adapter_label());
        }
        "pwd" => println!("{}", cwd.display()),
        "cd" => {
            let target = parts.collect::<Vec<_>>().join(" ");
            if target.is_empty() {
                anyhow::bail!("usage: /cd <path>");
            }
            let next = resolve_workspace_path(&target, &cwd);
            env::set_current_dir(&next)
                .with_context(|| format!("failed to change directory to {}", next.display()))?;
            state.refresh_counts();
            refresh_active_goal(state, &next);
            print_interactive_status(state);
        }
        "clear" => {
            print!("\x1b[2J\x1b[H");
            std::io::stdout().flush()?;
            print_interactive_banner(state);
        }
        "history" => {
            for session in NerveStore::new(cwd).list_sessions()?.into_iter().take(10) {
                println!(
                    "{} | {:?} | applied={} | name={} | {}",
                    session.id,
                    session.verdict,
                    session.applied,
                    session.name.as_deref().unwrap_or("-"),
                    session.prompt
                );
            }
        }
        "resume" => {
            let id = parts.next().context("usage: /resume <session-id>")?;
            let report = NerveStore::new(cwd).load_report(id)?;
            print_report(&report, false);
        }
        "list" => {
            for patch in NerveStore::new(cwd).list_patches()? {
                println!(
                    "{} | {:?} | files={} | applied={} | {}",
                    patch.id, patch.verdict, patch.file_count, patch.applied, patch.prompt
                );
            }
        }
        "templates" | "template" => {
            let config = Config::load()?;
            if name == "template"
                && let Some(template_id) = parts.next()
            {
                let Some(template) = config
                    .templates
                    .iter()
                    .find(|template| template.id == template_id)
                else {
                    anyhow::bail!("template `{template_id}` is not configured");
                };
                let args = parts.collect::<Vec<_>>().join(" ");
                let prompt = template.prompt.replace("{{args}}", &args);
                let template_id_owned = template.id.clone();
                if let Err(err) = bump_template_usage(&cwd, &template_id_owned) {
                    eprintln!("warning: template usage counter not persisted: {err:#}");
                }
                run_interactive_task(prompt, state).await?;
            } else {
                let query = parts.collect::<Vec<_>>().join(" ");
                let usage = load_template_usage(&cwd).unwrap_or_default();
                let mut filtered: Vec<_> = config
                    .templates
                    .iter()
                    .filter(|template| matches_template_query(template, &query))
                    .collect();
                filtered.sort_by(|a, b| {
                    let count_a = usage.get(&a.id).copied().unwrap_or(0);
                    let count_b = usage.get(&b.id).copied().unwrap_or(0);
                    count_b.cmp(&count_a).then_with(|| a.id.cmp(&b.id))
                });
                if filtered.is_empty() {
                    if config.templates.is_empty() {
                        println!("No prompt templates configured.");
                    } else {
                        println!("No prompt templates matched `{query}`.");
                    }
                }
                for template in filtered {
                    let count = usage.get(&template.id).copied().unwrap_or(0);
                    println!(
                        "{} | uses={} | {}",
                        template.id,
                        count,
                        template.description.as_deref().unwrap_or("-")
                    );
                }
            }
        }
        "goal" => {
            // Preserve the raw remainder so Phase 2 natural-language input can
            // recover the original `:nl` body or "quoted" sentence verbatim.
            let raw = command
                .strip_prefix("goal")
                .map(str::trim_start)
                .unwrap_or_default();
            handle_goal_command(parts.collect::<Vec<_>>(), raw, state, &cwd).await?;
        }
        "plan" => {
            // Tier 2f (v0.5.0): forward the remainder as a natural-language plan
            // task. Preserve the raw text so quoted strings or `:nl`-style
            // wrappers survive into the prompt verbatim.
            let raw = command
                .strip_prefix("plan")
                .map(str::trim_start)
                .unwrap_or_default();
            if raw.is_empty() {
                anyhow::bail!("usage: /plan <task description>");
            }
            run_interactive_plan(raw.to_string(), state).await?;
        }
        "budget" => {
            handle_budget_command(parts.collect::<Vec<_>>(), state, &cwd).await?;
        }
        "benchmark" => {
            let target = parts.next().context("usage: /benchmark pi [iterations]")?;
            if target != "pi" {
                anyhow::bail!("usage: /benchmark pi [iterations]");
            }
            let iterations = parts
                .next()
                .map(str::parse::<u16>)
                .transpose()
                .context("iterations must be a number")?
                .unwrap_or(3);
            let report = run_pi_benchmark(iterations, false, state.mock).await?;
            print_pi_benchmark_report(&report, false)?;
        }
        "diff" => {
            let Some(report) = &state.last_report else {
                anyhow::bail!("no last session; run a task first");
            };
            let Some(patch) = &report.final_patch else {
                anyhow::bail!("last session has no structured patch");
            };
            println!("{}", patch.to_unified_diff());
        }
        "apply" => {
            let id = parts
                .next()
                .map(str::to_string)
                .or_else(|| state.last_patch_id().map(str::to_string))
                .context("usage: /apply [patch-id]")?;
            let report = NerveStore::new(cwd).apply_patch(&id)?;
            println!("Applied patch {}.", report.patch_id);
            state.refresh_counts();
        }
        "rollback" => {
            let id = parts
                .next()
                .map(str::to_string)
                .or_else(|| state.last_patch_id().map(str::to_string))
                .context("usage: /rollback [patch-id]")?;
            let report = NerveStore::new(cwd).rollback_patch(&id)?;
            println!("Rolled back patch {}.", report.patch_id);
            state.refresh_counts();
        }
        "fork" => {
            handle_fork_slash(parts.collect::<Vec<_>>(), state, &cwd).await?;
        }
        "mcp" => {
            handle_mcp_slash(command, parts.collect::<Vec<_>>(), &cwd).await?;
        }
        "mayor" => {
            handle_mayor_slash(parts.collect::<Vec<_>>(), &cwd).await?;
        }
        "patrol" => {
            handle_patrol_slash(parts.collect::<Vec<_>>(), &cwd).await?;
        }
        "quit" | "exit" | "q" => return Ok(true),
        other => println!("Unknown command /{other}. Type /help for commands."),
    }
    Ok(false)
}

/// `/fork [--from-round N] [--name NAME]`. Anchors the prompt to the new
/// child session so subsequent `/diff` / `/apply` calls operate on the
/// branch the operator just minted.
async fn handle_fork_slash(
    args: Vec<&str>,
    state: &mut InteractiveState,
    cwd: &Path,
) -> Result<()> {
    let Some(parent_id) = state
        .current_session_id
        .clone()
        .or_else(|| state.last_report.as_ref().map(|r| r.task.id.clone()))
    else {
        anyhow::bail!("/fork requires an active session (run a task or `/resume <id>` first)");
    };
    let mut from_round: Option<u32> = None;
    let mut name: Option<String> = None;
    let mut iter = args.into_iter();
    while let Some(token) = iter.next() {
        match token {
            "--from-round" => {
                let value = iter
                    .next()
                    .context("usage: /fork [--from-round N] [--name NAME]")?;
                from_round = Some(value.parse::<u32>().context("--from-round must be u32")?);
            }
            "--name" => {
                let value = iter
                    .next()
                    .context("usage: /fork [--from-round N] [--name NAME]")?;
                name = Some(value.to_string());
            }
            other => anyhow::bail!("unknown flag `{other}` for /fork"),
        }
    }
    let config = Config::load_from(cwd)?;
    let forker = SessionForker::new(resolved_fork_config(&config), cwd);
    bootstrap_root_session_if_missing(&forker, cwd, &parent_id).await?;
    let child = forker
        .fork(
            &parent_id,
            CoreForkOptions {
                from_round,
                name,
                from_patch_sha: None,
            },
        )
        .await?;
    println!(
        "Forked into child session {} (from parent {}). prompt now scoped to the branch.",
        child.id, parent_id
    );
    state.current_session_id = Some(child.id);
    Ok(())
}

async fn handle_mcp_slash(raw_command: &str, args: Vec<&str>, cwd: &Path) -> Result<()> {
    let config = Config::load_from(cwd)?;
    let Some(mcp) = active_mcp_config(&config).cloned() else {
        println!("/mcp: no servers configured (roles.mcp / profiles[].mcp empty)");
        return Ok(());
    };
    let mut iter = args.into_iter();
    let action = iter.next().unwrap_or("list");
    match action {
        "list" => {
            let mut registry = McpRegistry::new();
            registry
                .register_all(&mcp.servers, &mcp.write_tool_patterns, &mcp.allow_tools)
                .await
                .context("/mcp list: failed to start configured servers")?;
            for (name, client) in registry.iter() {
                let tools = client.list_tools().await.unwrap_or_default();
                println!("server `{name}` ({} tool(s))", tools.len());
                for tool in tools {
                    println!(
                        "  {} | {}",
                        tool.name,
                        tool.description.as_deref().unwrap_or("-")
                    );
                }
            }
            registry.shutdown_all().await.ok();
        }
        "call" => {
            let server = iter
                .next()
                .context("usage: /mcp call <server> <tool> <json>")?;
            let tool = iter
                .next()
                .context("usage: /mcp call <server> <tool> <json>")?;
            // Recover the JSON argument verbatim from the raw command so we
            // preserve whitespace and nested objects. We slice from the
            // position right after `mcp call <server> <tool>`.
            let needle = format!("call {server} {tool}");
            let json_str = raw_command
                .strip_prefix("mcp")
                .map(str::trim_start)
                .and_then(|rest| rest.strip_prefix(&needle))
                .map(str::trim)
                .unwrap_or("");
            let arguments: serde_json::Value = if json_str.is_empty() {
                serde_json::Value::Object(Default::default())
            } else {
                serde_json::from_str(json_str)
                    .with_context(|| format!("invalid JSON arguments: `{json_str}`"))?
            };

            let Some(mut spec) = mcp.servers.iter().find(|s| s.name == server).cloned() else {
                anyhow::bail!("/mcp call: server `{server}` not configured");
            };
            scope_mcp_spec_to_allowlist(&mut spec, &mcp.allow_tools);
            let patterns = if mcp.write_tool_patterns.is_empty() {
                default_write_tool_patterns()
            } else {
                mcp.write_tool_patterns.clone()
            };
            let client = McpClient::new(spec, patterns);
            client.start().await?;
            let result = client
                .call_tool(McpToolCall {
                    server: server.to_string(),
                    tool: tool.to_string(),
                    arguments,
                })
                .await;
            client.shutdown().await.ok();
            let result = result?;
            println!("{}", serde_json::to_string_pretty(&result)?);
        }
        other => anyhow::bail!("/mcp: unknown action `{other}` (expected `list` or `call`)"),
    }
    Ok(())
}

async fn handle_mayor_slash(args: Vec<&str>, cwd: &Path) -> Result<()> {
    let action = args.first().copied().unwrap_or("status");
    if action != "status" {
        anyhow::bail!("usage: /mayor status");
    }
    let config = Config::load_from(cwd)?;
    let mp = config
        .orchestration
        .mayor_patrol
        .clone()
        .unwrap_or_default();
    let mayor = Mayor::new(mp, cwd.to_path_buf(), None);
    let status = mayor.status().await?;
    print_mayor_status(&status);
    Ok(())
}

async fn handle_patrol_slash(args: Vec<&str>, cwd: &Path) -> Result<()> {
    let action = args.first().copied().unwrap_or("status");
    if action != "status" {
        anyhow::bail!("usage: /patrol status");
    }
    let config = Config::load_from(cwd)?;
    let mp = config
        .orchestration
        .mayor_patrol
        .clone()
        .unwrap_or_default();
    let mayor = Mayor::new(mp, cwd.to_path_buf(), None);
    let status = mayor.status().await?;
    println!(
        "patrol heartbeats: {}",
        if status.active_patrols.is_empty() {
            "none".to_string()
        } else {
            status.active_patrols.join(", ")
        }
    );
    println!(
        "queue: pending={} claimed={} done={} failed={}",
        status.pending_count, status.claimed_count, status.done_count, status.failed_count
    );
    Ok(())
}

fn print_interactive_help() {
    print_box(
        "Command Map",
        &[
            "Type a request to start the lead/reviewer loop for this workspace.".to_string(),
            "Slash commands operate on the active session; shell commands start with !."
                .to_string(),
            "Use /clear to redraw this surface after long output.".to_string(),
        ],
    );
    print_command_section(
        "Tasks",
        &[
            ("request text", "run the lead/reviewer loop"),
            ("/paste", "enter multiline input; finish with /end"),
            ("!<command>", "run a shell command in the workspace"),
        ],
    );
    print_command_section(
        "Review",
        &[
            ("/diff", "show the last reviewed patch"),
            ("/apply [patch-id]", "apply the last or selected patch"),
            (
                "/rollback [patch-id]",
                "roll back the last or selected patch",
            ),
            ("/history", "show recent sessions"),
            ("/resume <id>", "print a stored session report"),
            ("/list", "list stored patches"),
        ],
    );
    print_command_section(
        "Workspace",
        &[
            ("/status", "show adapter, mode, branch, counts, cwd"),
            (
                "/mode <dry-run|apply>",
                "switch apply behavior without restarting",
            ),
            (
                "/adapter <real|mock>",
                "switch providers without restarting",
            ),
            ("/cd <path>", "change workspace directory"),
            ("/pwd", "print workspace directory"),
            ("/clear", "redraw the terminal workspace"),
        ],
    );
    print_command_section(
        "Setup",
        &[
            ("/login", "start provider login flows"),
            ("/doctor", "inspect config, adapters, and auth"),
            ("/templates [query]", "list or search prompt templates"),
            ("/template <id> [args]", "run a prompt template"),
            ("/benchmark pi [n]", "run the Pi workflow benchmark"),
        ],
    );
    print_command_section(
        "Multi-Instance",
        &[
            (
                "/fork [--from-round N] [--name NAME]",
                "branch the current session",
            ),
            (
                "/mcp list | call <s> <t> <json>",
                "inspect or dispatch MCP tools",
            ),
            ("/mayor status", "show queue depth + active patrols"),
            ("/patrol status", "show local heartbeat + queue"),
        ],
    );
    print_command_section(
        "Loop Controls",
        &[
            ("/plan <task>", "run read-only plan analysis"),
            ("/goal <argv...>", "register a deterministic stop check_cmd"),
            (
                "/goal :nl <prose>",
                "LLM-convert natural language into a check_cmd",
            ),
            (
                "/goal \"<prose>\"",
                "quoted form of :nl for the whole argument",
            ),
            ("/goal show | clear", "inspect or remove the active goal"),
            ("/budget show", "show cap, cumulative, and remaining"),
            ("/budget cost=$X", "cap session cost"),
            ("/budget tokens=N", "cap session total tokens"),
            ("/quit", "exit"),
        ],
    );
    print_box(
        "Keys",
        &[
            "type / to open the command palette".to_string(),
            "use Up/Down for history or command selection".to_string(),
            "use Tab or Right to complete a selected command".to_string(),
        ],
    );
}

fn resolve_workspace_path(target: &str, cwd: &Path) -> PathBuf {
    if target == "~"
        && let Some(home) = env::var_os("HOME").or_else(|| env::var_os("USERPROFILE"))
    {
        return PathBuf::from(home);
    }
    if let Some(rest) = target.strip_prefix("~/")
        && let Some(home) = env::var_os("HOME").or_else(|| env::var_os("USERPROFILE"))
    {
        return PathBuf::from(home).join(rest);
    }
    let path = PathBuf::from(target);
    if path.is_absolute() {
        path
    } else {
        cwd.join(path)
    }
}

fn git_branch_label(cwd: Option<&Path>) -> Option<String> {
    let cwd = cwd?;
    let output = StdCommand::new("git")
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .current_dir(cwd)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let branch = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if branch.is_empty() {
        return None;
    }
    let dirty = StdCommand::new("git")
        .args(["status", "--porcelain"])
        .current_dir(cwd)
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| !output.stdout.is_empty())
        .unwrap_or(false);
    Some(if dirty { format!("{branch}*") } else { branch })
}

fn run_interactive_shell(command: &str) -> Result<()> {
    if command.is_empty() {
        anyhow::bail!("usage: !<command>");
    }
    let cwd = env::current_dir().context("failed to read current directory")?;
    #[cfg(windows)]
    let mut child = {
        let mut child = StdCommand::new("cmd");
        child.args(["/C", command]);
        child
    };
    #[cfg(not(windows))]
    let mut child = {
        let shell = env::var("SHELL").unwrap_or_else(|_| "sh".to_string());
        let mut child = StdCommand::new(shell);
        child.args(["-lc", command]);
        child
    };
    let status = child
        .current_dir(cwd)
        .status()
        .with_context(|| format!("failed to run shell command `{command}`"))?;
    if status.success() {
        Ok(())
    } else {
        anyhow::bail!("shell command exited with status {status}");
    }
}

fn matches_template_query(template: &nerve_config::PromptTemplate, query: &str) -> bool {
    if query.trim().is_empty() {
        return true;
    }
    let haystack = format!(
        "{} {} {}",
        template.id,
        template.description.as_deref().unwrap_or(""),
        template.prompt
    )
    .to_ascii_lowercase();
    query
        .split_whitespace()
        .all(|term| haystack.contains(&term.to_ascii_lowercase()))
}

fn template_usage_path(cwd: &Path) -> PathBuf {
    cwd.join(".nerve")
        .join("session-meta")
        .join("template-usage.json")
}

fn load_template_usage(cwd: &Path) -> Result<BTreeMap<String, u64>> {
    let path = template_usage_path(cwd);
    if !path.exists() {
        return Ok(BTreeMap::new());
    }
    let raw = fs::read_to_string(&path)
        .with_context(|| format!("failed to read `{}`", path.display()))?;
    if raw.trim().is_empty() {
        return Ok(BTreeMap::new());
    }
    serde_json::from_str(&raw)
        .with_context(|| format!("invalid template usage JSON in `{}`", path.display()))
}

fn bump_template_usage(cwd: &Path, template_id: &str) -> Result<()> {
    let mut usage = load_template_usage(cwd).unwrap_or_default();
    let entry = usage.entry(template_id.to_string()).or_insert(0);
    *entry = entry.saturating_add(1);
    let path = template_usage_path(cwd);
    write_json_atomic(&path, &usage)
}

fn write_json_atomic<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create `{}`", parent.display()))?;
    }
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let raw = serde_json::to_vec_pretty(value)?;
    let mut tmp = tempfile::NamedTempFile::new_in(parent)
        .with_context(|| format!("failed to create temp file in `{}`", parent.display()))?;
    tmp.write_all(&raw)
        .with_context(|| format!("failed to write temp JSON for `{}`", path.display()))?;
    tmp.persist(path)
        .map_err(|err| err.error)
        .with_context(|| format!("failed to move temp JSON to `{}`", path.display()))?;
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum GoalAction {
    Show,
    Clear,
    RegisterArgv {
        argv: Vec<String>,
        timeout_secs: Option<u64>,
    },
    /// Phase 2 (§3 Tier 1b): forward `free_form` to `GoalIntentConverter`.
    RegisterNaturalLanguage {
        free_form: String,
    },
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
enum GoalParseError {
    #[error(
        "usage: /goal <argv> | /goal :nl <prose> | /goal \"<prose>\" | /goal show | /goal clear | /goal --timeout N <argv>"
    )]
    Empty,
    #[error("/goal --timeout requires a positive number")]
    BadTimeout,
    #[error("/goal natural-language input must be non-empty")]
    EmptyNaturalLanguage,
}

/// Parse `/goal` arguments. `raw` is the original remainder after the
/// `/goal ` prefix (whitespace preserved) so quoted Phase-2 sentences are
/// detected verbatim. `tokens` is the whitespace-split form used for the
/// historical argv path.
fn parse_goal_argv_with_raw(tokens: &[&str], raw: &str) -> Result<GoalAction, GoalParseError> {
    if let Some(action) = parse_goal_nl_form(raw) {
        return action;
    }
    parse_goal_argv(tokens)
}

/// Detect the Phase 2 natural-language forms (`:nl <prose>` or `"<prose>"`)
/// and short-circuit before the argv parser. Returns:
/// - `Some(Ok(action))` when the raw is unambiguously natural-language.
/// - `Some(Err(_))` when the form is detected but malformed (e.g. empty body).
/// - `None` when the raw does not look like natural language.
fn parse_goal_nl_form(raw: &str) -> Option<Result<GoalAction, GoalParseError>> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Some(body) = trimmed.strip_prefix(":nl") {
        let free_form = body.trim();
        if free_form.is_empty() {
            return Some(Err(GoalParseError::EmptyNaturalLanguage));
        }
        return Some(Ok(GoalAction::RegisterNaturalLanguage {
            free_form: free_form.to_string(),
        }));
    }
    // Quoted form: the entire remainder is a single "..." string. We require
    // the closing quote to be the last non-whitespace character to avoid
    // colliding with shell-literal argv strings that just happen to start
    // with a quote.
    if trimmed.starts_with('"') && trimmed.ends_with('"') && trimmed.len() >= 2 {
        let inner = &trimmed[1..trimmed.len() - 1];
        if inner.is_empty() {
            return Some(Err(GoalParseError::EmptyNaturalLanguage));
        }
        return Some(Ok(GoalAction::RegisterNaturalLanguage {
            free_form: inner.to_string(),
        }));
    }
    None
}

fn parse_goal_argv(tokens: &[&str]) -> Result<GoalAction, GoalParseError> {
    let mut iter = tokens.iter().map(|s| s.to_string()).peekable();
    let Some(first) = iter.peek().cloned() else {
        return Err(GoalParseError::Empty);
    };
    if tokens.len() == 1 {
        match first.as_str() {
            "show" => return Ok(GoalAction::Show),
            "clear" => return Ok(GoalAction::Clear),
            _ => {}
        }
    }

    let mut timeout_secs: Option<u64> = None;
    let mut argv: Vec<String> = Vec::new();
    while let Some(token) = iter.next() {
        match token.as_str() {
            "--timeout" => {
                let value = iter.next().ok_or(GoalParseError::BadTimeout)?;
                let parsed: u64 = value.parse().map_err(|_| GoalParseError::BadTimeout)?;
                if parsed == 0 {
                    return Err(GoalParseError::BadTimeout);
                }
                timeout_secs = Some(parsed);
            }
            _ => argv.push(token),
        }
    }

    if argv.is_empty() {
        return Err(GoalParseError::Empty);
    }

    Ok(GoalAction::RegisterArgv { argv, timeout_secs })
}

fn active_goal_path(cwd: &Path) -> PathBuf {
    cwd.join(".nerve")
        .join("session-meta")
        .join("active-goal.json")
}

fn load_active_goal(cwd: &Path) -> Result<Option<GoalSpec>> {
    let path = active_goal_path(cwd);
    if !path.exists() {
        return Ok(None);
    }
    let raw = fs::read_to_string(&path)
        .with_context(|| format!("failed to read `{}`", path.display()))?;
    if raw.trim().is_empty() {
        return Ok(None);
    }
    let spec: GoalSpec = serde_json::from_str(&raw)
        .with_context(|| format!("invalid active goal JSON in `{}`", path.display()))?;
    spec.validate()
        .with_context(|| format!("invalid active goal in `{}`", path.display()))?;
    Ok(Some(spec))
}

fn refresh_active_goal(state: &mut InteractiveState, cwd: &Path) {
    match load_active_goal(cwd) {
        Ok(goal) => state.active_goal = goal,
        Err(err) => {
            eprintln!("warning: active goal not loaded: {err:#}");
            state.active_goal = None;
        }
    }
}

async fn handle_goal_command(
    args: Vec<&str>,
    raw: &str,
    state: &mut InteractiveState,
    cwd: &Path,
) -> Result<()> {
    let action = parse_goal_argv_with_raw(&args, raw).map_err(|err| anyhow::anyhow!("{err}"))?;
    match action {
        GoalAction::Show => match state.active_goal.as_ref() {
            Some(spec) => {
                println!(
                    "goal id={} timeout={}s cmd={:?}",
                    spec.id, spec.timeout_secs, spec.check_cmd
                );
            }
            None => println!("No active goal."),
        },
        GoalAction::Clear => {
            state.active_goal = None;
            let path = active_goal_path(cwd);
            if path.exists() {
                let _ = fs::remove_file(&path);
            }
            println!("Cleared active goal.");
        }
        GoalAction::RegisterArgv { argv, timeout_secs } => {
            let id = format!("goal-{}", chrono::Utc::now().format("%Y%m%dT%H%M%SZ"));
            let spec = GoalSpec {
                id,
                check_cmd: argv,
                timeout_secs: timeout_secs.unwrap_or(300),
                cwd: Some(cwd.to_path_buf()),
                env: BTreeMap::new(),
                no_progress_max: None,
            };
            spec.validate()
                .with_context(|| "goal spec validation failed")?;
            // sec-1 #5: prevent path traversal from cwd by writing relative to
            // the freeze-locked workspace root only.
            let target = active_goal_path(cwd);
            write_json_atomic(&target, &spec)?;
            println!(
                "Registered goal `{}`: {} ({}s timeout)",
                spec.id,
                spec.check_cmd.join(" "),
                spec.timeout_secs
            );
            state.active_goal = Some(spec);
        }
        GoalAction::RegisterNaturalLanguage { free_form } => {
            register_goal_from_natural_language(free_form, state, cwd).await?;
        }
    }
    Ok(())
}

/// §3 Tier 1b Phase 2 user-confirmation flow.
///
/// 1. Spin up a fresh lead adapter (`SubprocessAdapter::claude_code` for real
///    mode, `MockAdapter::lead` for mock) and wrap it in
///    `GoalIntentConverter`.
/// 2. Call `convert(free_form, cwd)` to get a vetted `GoalIntent`.
/// 3. Render the proposal to stdout and gate registration on an
///    interactive y/N confirmation. Non-interactive sessions are rejected so
///    automation cannot register an LLM-proposed argv silently.
/// 4. On accept, append the confirmed GoalIntent to
///    `.nerve/session-meta/goal-history.jsonl` (sec-1 #6) and persist the
///    proposed `GoalSpec` to `active-goal.json`.
async fn register_goal_from_natural_language(
    free_form: String,
    state: &mut InteractiveState,
    cwd: &Path,
) -> Result<()> {
    if !std::io::stdin().is_terminal() {
        anyhow::bail!(
            "/goal natural-language input requires an interactive terminal so the LLM proposal can be confirmed before it runs"
        );
    }

    let config = Config::load_from(cwd)
        .context("failed to load nerve.config.json for /goal natural-language conversion")?;
    let adapter = goal_intent_lead_adapter(state.mock, &config);
    let converter = GoalIntentConverter::new(adapter);

    println!(
        "Asking `{}` to convert `{}` into a deterministic check_cmd...",
        converter.adapter_id(),
        free_form
    );
    let intent = converter
        .convert(&free_form, cwd)
        .await
        .with_context(|| "natural-language /goal conversion failed")?;

    render_goal_intent_proposal(&intent);

    if !confirm_goal_intent_interactive()? {
        println!("Goal proposal rejected. No active goal registered.");
        return Ok(());
    }

    // Persist the audited intent before mutating active-goal.json so a crash
    // between the two writes leaves a recoverable trail (sec-1 #6).
    append_goal_history_entry(cwd, &intent)
        .with_context(|| "failed to append goal-history.jsonl audit entry")?;

    let mut spec = intent.proposed_spec.clone();
    if spec.cwd.is_none() {
        spec.cwd = Some(cwd.to_path_buf());
    }
    spec.validate()
        .with_context(|| "proposed goal failed validation")?;

    let target = active_goal_path(cwd);
    write_json_atomic(&target, &spec)?;
    println!(
        "Registered goal `{}`: {} ({}s timeout) [source={}]",
        spec.id,
        spec.check_cmd.join(" "),
        spec.timeout_secs,
        intent.source_adapter
    );
    state.active_goal = Some(spec);
    Ok(())
}

/// Build the lead adapter used by `GoalIntentConverter`.
///
/// We deliberately don't reuse `adapters_for_config` because that returns
/// `Vec<Box<dyn ModelAdapter>>`; the converter wants `Arc` for cheap clones.
fn goal_intent_lead_adapter(mock: bool, config: &Config) -> Arc<dyn ModelAdapter> {
    if mock {
        Arc::new(nerve_adapter::MockAdapter::lead())
    } else {
        let mut adapter = SubprocessAdapter::claude_code();
        if let Some(secs) = config.orchestration.adapter_timeout_secs {
            adapter = adapter.with_timeout_secs(secs);
        }
        if let Some(bytes) = config.orchestration.adapter_max_output_bytes {
            adapter = adapter.with_max_output_bytes(bytes);
        }
        Arc::new(adapter)
    }
}

fn render_goal_intent_proposal(intent: &GoalIntent) {
    println!("Proposed goal from \"{}\":", intent.free_form);
    println!("  check_cmd:    {:?}", intent.proposed_spec.check_cmd);
    println!("  timeout_secs: {}", intent.proposed_spec.timeout_secs);
    if intent.proposed_spec.env.is_empty() {
        println!("  env:          (inherit only configured allowlist)");
    } else {
        println!(
            "  env override: {}",
            intent
                .proposed_spec
                .env
                .iter()
                .map(|(k, v)| format!("{k}={v}"))
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    println!("  rationale:    {}", intent.rationale);
    println!("  source:       {}", intent.source_adapter);
}

fn confirm_goal_intent_interactive() -> Result<bool> {
    print!("Accept? [y/N]: ");
    std::io::stdout().flush().ok();
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    let answer = line.trim().to_ascii_lowercase();
    Ok(matches!(answer.as_str(), "y" | "yes"))
}

fn goal_history_path(cwd: &Path) -> PathBuf {
    cwd.join(".nerve")
        .join("session-meta")
        .join("goal-history.jsonl")
}

/// Append a confirmed `GoalIntent` to `.nerve/session-meta/goal-history.jsonl`
/// as a one-line JSON record. The file is created atomically when missing and
/// appended to under the parent dir's default permissions.
fn append_goal_history_entry(cwd: &Path, intent: &GoalIntent) -> Result<()> {
    let path = goal_history_path(cwd);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create `{}`", parent.display()))?;
    }
    let mut line = serde_json::to_string(intent)
        .with_context(|| "failed to encode goal-history entry as JSON")?;
    line.push('\n');
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("failed to open `{}`", path.display()))?;
    file.write_all(line.as_bytes())
        .with_context(|| format!("failed to append to `{}`", path.display()))?;
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum BudgetAction {
    Show,
    Set {
        cost_microusd: Option<u64>,
        tokens: Option<u64>,
        force: bool,
    },
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
enum BudgetParseError {
    #[error("usage: /budget show | /budget cost=$X | /budget tokens=N [--force]")]
    Empty,
    #[error("/budget value must not be empty")]
    EmptyValue,
    #[error("/budget value must specify a unit: $ prefix for cost, `tokens` suffix for tokens")]
    UnitMissing,
    #[error("/budget value `{0}` is not a positive number")]
    InvalidValue(String),
}

fn parse_budget_args(tokens: &[&str]) -> Result<BudgetAction, BudgetParseError> {
    if tokens.is_empty() {
        return Err(BudgetParseError::Empty);
    }
    if tokens.len() == 1 && tokens[0] == "show" {
        return Ok(BudgetAction::Show);
    }
    let mut cost: Option<u64> = None;
    let mut toks: Option<u64> = None;
    let mut force = false;
    for token in tokens {
        if *token == "--force" {
            force = true;
            continue;
        }
        let trimmed = token.trim();
        if trimmed.is_empty() {
            return Err(BudgetParseError::EmptyValue);
        }
        let lower = trimmed.to_ascii_lowercase();
        let value_after_eq = trimmed.split_once('=').map(|(_, v)| v.trim()).unwrap_or("");
        if lower.starts_with("cost=") {
            if value_after_eq.is_empty() {
                return Err(BudgetParseError::EmptyValue);
            }
            cost = Some(parse_cost_value(value_after_eq)?);
        } else if lower.starts_with("tokens=") {
            if value_after_eq.is_empty() {
                return Err(BudgetParseError::EmptyValue);
            }
            toks = Some(parse_tokens_value(value_after_eq)?);
        } else {
            return Err(BudgetParseError::UnitMissing);
        }
    }
    if cost.is_none() && toks.is_none() {
        return Err(BudgetParseError::Empty);
    }
    Ok(BudgetAction::Set {
        cost_microusd: cost,
        tokens: toks,
        force,
    })
}

fn parse_cost_value(raw: &str) -> Result<u64, BudgetParseError> {
    // sec-3 #5: require `$` prefix for cost, reject bare numbers / negatives /
    // NaN. Decimal `$5.00` becomes microusd.
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(BudgetParseError::EmptyValue);
    }
    let Some(rest) = trimmed.strip_prefix('$') else {
        return Err(BudgetParseError::UnitMissing);
    };
    if rest.is_empty() {
        return Err(BudgetParseError::EmptyValue);
    }
    if rest.contains('-') || rest.contains('+') || rest.starts_with('.') {
        return Err(BudgetParseError::InvalidValue(raw.to_string()));
    }
    let (dollars_str, fractional_str) = match rest.split_once('.') {
        Some((d, f)) => (d, f),
        None => (rest, ""),
    };
    if dollars_str.is_empty() || !dollars_str.chars().all(|c| c.is_ascii_digit()) {
        return Err(BudgetParseError::InvalidValue(raw.to_string()));
    }
    if !fractional_str.chars().all(|c| c.is_ascii_digit()) {
        return Err(BudgetParseError::InvalidValue(raw.to_string()));
    }
    let dollars: u64 = dollars_str
        .parse()
        .map_err(|_| BudgetParseError::InvalidValue(raw.to_string()))?;
    let padded = format!("{:0<6}", fractional_str);
    let truncated = &padded[..6.min(padded.len())];
    let micros: u64 = if truncated.is_empty() {
        0
    } else {
        truncated
            .parse()
            .map_err(|_| BudgetParseError::InvalidValue(raw.to_string()))?
    };
    let total = dollars
        .checked_mul(1_000_000)
        .and_then(|v| v.checked_add(micros))
        .ok_or_else(|| BudgetParseError::InvalidValue(raw.to_string()))?;
    if total == 0 {
        return Err(BudgetParseError::InvalidValue(raw.to_string()));
    }
    Ok(total)
}

fn parse_tokens_value(raw: &str) -> Result<u64, BudgetParseError> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(BudgetParseError::EmptyValue);
    }
    if trimmed.starts_with('-') || trimmed.starts_with('+') {
        return Err(BudgetParseError::InvalidValue(raw.to_string()));
    }
    if !trimmed.chars().all(|c| c.is_ascii_digit()) {
        return Err(BudgetParseError::InvalidValue(raw.to_string()));
    }
    let parsed: u64 = trimmed
        .parse()
        .map_err(|_| BudgetParseError::InvalidValue(raw.to_string()))?;
    if parsed == 0 {
        return Err(BudgetParseError::InvalidValue(raw.to_string()));
    }
    Ok(parsed)
}

fn budget_audit_path(cwd: &Path) -> PathBuf {
    cwd.join(".nerve")
        .join("session-meta")
        .join("budget-audit.json")
}

async fn handle_budget_command(
    args: Vec<&str>,
    state: &mut InteractiveState,
    cwd: &Path,
) -> Result<()> {
    if args.is_empty() {
        anyhow::bail!("usage: /budget show | /budget cost=$X | /budget tokens=N [--force]");
    }
    let action = parse_budget_args(&args).map_err(|err| anyhow::anyhow!("{err}"))?;
    let config = Config::load_from(cwd)?;
    match action {
        BudgetAction::Show => {
            print_budget_show(state, &config);
            return Ok(());
        }
        BudgetAction::Set {
            cost_microusd,
            tokens,
            force,
        } => {
            let prev = BudgetSnapshot {
                max_total_tokens: state
                    .budget_override_tokens
                    .or(config.orchestration.max_total_tokens),
                max_estimated_cost_microusd: state
                    .budget_override_cost_microusd
                    .or(config.orchestration.max_estimated_cost_microusd),
            };

            let mut new_cost = state
                .budget_override_cost_microusd
                .or(config.orchestration.max_estimated_cost_microusd);
            let mut new_tokens = state
                .budget_override_tokens
                .or(config.orchestration.max_total_tokens);
            let mut user_confirmed = false;

            if let Some(requested_cost) = cost_microusd {
                if let Some(ceiling) = config.orchestration.budget_cost_microusd_ceiling
                    && requested_cost > ceiling
                {
                    anyhow::bail!(
                        "/budget cost cap {} exceeds global ceiling {} (microusd). \
                         --force cannot bypass the ceiling.",
                        requested_cost,
                        ceiling
                    );
                }
                if requires_budget_raise_confirmation(new_cost, requested_cost, force) {
                    if !confirm_raise_interactive("cost")? {
                        anyhow::bail!("budget raise cancelled");
                    }
                    user_confirmed = true;
                }
                new_cost = Some(requested_cost);
            }
            if let Some(requested_tokens) = tokens {
                if let Some(ceiling) = config.orchestration.budget_tokens_ceiling
                    && requested_tokens > ceiling
                {
                    anyhow::bail!(
                        "/budget tokens cap {} exceeds global ceiling {}. \
                         --force cannot bypass the ceiling.",
                        requested_tokens,
                        ceiling
                    );
                }
                if requires_budget_raise_confirmation(new_tokens, requested_tokens, force) {
                    if !confirm_raise_interactive("tokens")? {
                        anyhow::bail!("budget raise cancelled");
                    }
                    user_confirmed = true;
                }
                new_tokens = Some(requested_tokens);
            }

            let next = BudgetSnapshot {
                max_total_tokens: new_tokens,
                max_estimated_cost_microusd: new_cost,
            };
            let entry = BudgetAuditEntry {
                ts: chrono::Utc::now(),
                prev,
                next: next.clone(),
                source: "slash".to_string(),
                user_confirmed,
                // sec-gap-12 hash chain: append_budget_audit_entry fills this
                // with the current chain head before persisting.
                prev_hash: None,
            };
            append_budget_audit_entry(&budget_audit_path(cwd), entry)
                .with_context(|| "failed to append budget audit entry")?;

            state.budget_override_cost_microusd = next.max_estimated_cost_microusd;
            state.budget_override_tokens = next.max_total_tokens;
            print_budget_show(state, &config);
            print_status_bar(state);
        }
    }
    Ok(())
}

fn print_budget_show(state: &InteractiveState, config: &Config) {
    let cost_cap = state
        .budget_override_cost_microusd
        .or(config.orchestration.max_estimated_cost_microusd);
    let token_cap = state
        .budget_override_tokens
        .or(config.orchestration.max_total_tokens);
    let total_tokens = state
        .cumulative_input_tokens
        .saturating_add(state.cumulative_output_tokens);
    let cost_used = state.cumulative_cost_microusd;
    let cost_remaining = cost_cap.map(|c| c.saturating_sub(cost_used));
    let tokens_remaining = token_cap.map(|t| t.saturating_sub(total_tokens));
    println!(
        "budget: cost cap={} used={} remaining={}",
        cost_cap
            .map(format_cost_microusd)
            .unwrap_or_else(|| "-".to_string()),
        format_cost_microusd(cost_used),
        cost_remaining
            .map(format_cost_microusd)
            .unwrap_or_else(|| "-".to_string()),
    );
    println!(
        "budget: token cap={} used={} remaining={}",
        token_cap
            .map(|t| t.to_string())
            .unwrap_or_else(|| "-".to_string()),
        total_tokens,
        tokens_remaining
            .map(|t| t.to_string())
            .unwrap_or_else(|| "-".to_string()),
    );
    if let Some(ceiling) = config.orchestration.budget_cost_microusd_ceiling {
        println!(
            "budget: global cost ceiling={}",
            format_cost_microusd(ceiling)
        );
    }
    if let Some(ceiling) = config.orchestration.budget_tokens_ceiling {
        println!("budget: global token ceiling={ceiling}");
    }
}

fn requires_budget_raise_confirmation(current: Option<u64>, requested: u64, force: bool) -> bool {
    !force && current.map(|value| requested > value).unwrap_or(true)
}

fn confirm_raise_interactive(label: &str) -> Result<bool> {
    if !std::io::stdin().is_terminal() {
        anyhow::bail!(
            "/budget {label} raise requires interactive confirmation; \
             non-interactive sessions are rejected"
        );
    }
    print!("Raising {label} budget. Continue? [y/N]: ");
    std::io::stdout().flush().ok();
    let mut line = String::new();
    std::io::stdin().read_line(&mut line).ok();
    let answer = line.trim().to_ascii_lowercase();
    Ok(matches!(answer.as_str(), "y" | "yes"))
}

async fn run_daemon(
    apply: bool,
    mock: bool,
    once: bool,
    worktree_override: Option<bool>,
) -> Result<()> {
    let _echo_guard = disable_stdin_echo_if_terminal()?;
    let stdin = io::BufReader::new(io::stdin());
    let mut lines = stdin.lines();

    while let Some(line) = lines.next_line().await? {
        let prompt = line.trim();
        if prompt.is_empty() {
            continue;
        }

        let report = run_report(prompt.to_string(), apply, mock, worktree_override).await?;
        println!("{}", serde_json::to_string(&report)?);

        if once {
            break;
        }
    }

    Ok(())
}

async fn run_rpc_daemon(
    apply: bool,
    mock: bool,
    once: bool,
    print_token: bool,
    worktree_override: Option<bool>,
) -> Result<()> {
    let _echo_guard = disable_stdin_echo_if_terminal()?;

    // Tier 2e (sec-4 #4): bring up the per-session RPC bus before processing
    // any RPC commands. We deliberately keep the bus alive for the whole
    // daemon lifetime so subscribers continue to receive lifecycle envelopes
    // even when a single `prompt` command is in flight.
    let cwd = env::current_dir().context("failed to read current directory")?;
    let session_meta = cwd.join(".nerve").join("session-meta");
    fs::create_dir_all(&session_meta).with_context(|| {
        format!(
            "failed to create RPC session-meta dir `{}`",
            session_meta.display()
        )
    })?;
    let config_for_rpc = Config::load_from(&cwd)?;
    let rpc_config = config_for_rpc.daemon.rpc.clone().unwrap_or_default();
    let print_token = print_token || rpc_config.print_token;
    let bus = Arc::new(
        RpcBus::new(rpc_config, &cwd)
            .with_context(|| "failed to open RPC bus for daemon startup")?,
    );
    // S9: tracks in-flight (nonblocking) runs so the read loop stays responsive
    // and shutdown can await them.
    let registry: RunRegistry = Arc::new(std::sync::Mutex::new(HashMap::new()));

    if print_token {
        // Token surfaces as a typed envelope on stdout (not the bearer text
        // alone) so editors / shell wrappers can pull it out of the same JSONL
        // stream that carries lifecycle events.
        let envelope = rpc_envelope(
            "rpc.token",
            serde_json::json!({ "bearer": bus.bearer_token() }),
        );
        emit_envelope_line(&envelope);
    }

    let stdin = io::BufReader::new(io::stdin());
    let mut lines = stdin.lines();

    while let Some(line) = lines.next_line().await? {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        let value: serde_json::Value = match serde_json::from_str(line) {
            Ok(value) => value,
            Err(error) => {
                println!(
                    "{}",
                    serde_json::json!({
                        "type": "error",
                        "message": format!("invalid JSON: {error}")
                    })
                );
                if once {
                    break;
                }
                continue;
            }
        };

        if let Err(error) = handle_rpc_command(
            value,
            apply,
            mock,
            bus.clone(),
            worktree_override,
            registry.clone(),
        )
        .await
        {
            println!(
                "{}",
                serde_json::json!({
                    "type": "error",
                    "message": error.to_string()
                })
            );
        }

        if once {
            break;
        }
    }

    // S9: await every in-flight run before shutting down. This makes `--once`
    // wait for the spawned run to finish (it no longer blocks the read loop),
    // ensures a graceful shutdown never drops a running loop mid-flight, and
    // releases the bus clones held by the run tasks so the `Arc::try_unwrap`
    // below can reclaim the bus.
    let handles: Vec<JoinHandle<()>> = match registry.lock() {
        Ok(mut reg) => reg.drain().map(|(_, run)| run.join).collect(),
        Err(_) => Vec::new(),
    };
    for handle in handles {
        let _ = handle.await;
    }

    // sec-4 #5: tear down the bearer-token file on graceful shutdown so a
    // crashed daemon never leaves a stale token behind. `Arc::try_unwrap`
    // ensures we only delete when no other handle (e.g. an in-flight handler
    // hanging on a `tokio` task) is still using the bus.
    if let Ok(bus) = Arc::try_unwrap(bus) {
        bus.shutdown().context("rpc bus shutdown failed")?;
    }

    Ok(())
}

fn emit_envelope_line(envelope: &RpcEnvelope) {
    match serde_json::to_string(envelope) {
        Ok(line) => println!("{line}"),
        Err(error) => {
            eprintln!("warning: failed to serialise RPC envelope: {error}");
        }
    }
}

fn rpc_envelope(kind: &str, payload: serde_json::Value) -> RpcEnvelope {
    RpcEnvelope::new(kind, payload).with_fresh_metadata()
}

/// S9/S11: a tracked in-flight run — its join handle (for graceful shutdown)
/// plus its S11 [`ApplyConsent`] handle (so the operator can escalate THIS run
/// to apply mid-flight via the `approve` command). The consent handle is held
/// in the daemon's memory and is unreachable by the lead subprocess, which is
/// what makes it a forge-proof consent signal (see `ApplyConsent`).
struct TrackedRun {
    join: JoinHandle<()>,
    consent: ApplyConsent,
}

/// S11: grant apply-consent to an in-flight run, returning whether a grant was
/// made. A run-id that is ABSENT *or already FINISHED* returns `false` and flips
/// nothing: a finished run has already passed its single apply seam and can never
/// act on a grant, so "approving" it would only write a misleading audit record
/// for a run that can never apply. (Finished entries linger in the registry until
/// the next spawn/shutdown prunes them, so this guard — not mere presence — is
/// what makes the `approve` contract "in-flight only" hold.)
fn grant_in_flight(reg: &HashMap<String, TrackedRun>, run_id: &str) -> bool {
    match reg.get(run_id) {
        Some(run) if !run.join.is_finished() => {
            run.consent.grant();
            true
        }
        _ => false,
    }
}

/// S9: tracks in-flight (nonblocking) run tasks by run-id so the daemon read
/// loop stays responsive while runs execute, and graceful shutdown can await
/// them. A plain `std::sync::Mutex` is fine: every critical section is a short
/// synchronous insert / `retain` / `drain` with no `.await` held across it.
type RunRegistry = Arc<std::sync::Mutex<HashMap<String, TrackedRun>>>;

/// S9: map a completed round to its live `round.started` + `round.ended`
/// envelopes (the round-seam signal streamed as each round finishes).
fn round_seam_envelopes(session_id: &str, round: &RoundRecord) -> (RpcEnvelope, RpcEnvelope) {
    use nerve_types::rpc_kinds;
    let started = rpc_envelope(
        rpc_kinds::ROUND_STARTED,
        serde_json::json!({ "session_id": session_id, "round": round.round }),
    );
    let ended = rpc_envelope(
        rpc_kinds::ROUND_ENDED,
        serde_json::json!({
            "session_id": session_id,
            "round": round.round,
            "verdict": round.reviewer.verdict,
            "check": round.check_result,
        }),
    );
    (started, ended)
}

/// S9: map an in-flight S8 checkpoint to a `session.status` envelope (served by
/// the `status` command from the on-disk checkpoints, so it works even across a
/// daemon restart). A checkpoint carries no acceptance fields, so this can only
/// ever report progress — never that a run was accepted.
fn checkpoint_status_envelope(checkpoint: &RunCheckpoint) -> RpcEnvelope {
    use nerve_types::rpc_kinds;
    rpc_envelope(
        rpc_kinds::SESSION_STATUS,
        serde_json::json!({
            "session_id": checkpoint.task.id,
            "prompt": checkpoint.task.prompt,
            "status": checkpoint.status,
            "rounds": checkpoint.rounds.len(),
            "updated_at": checkpoint.updated_at,
        }),
    )
}

/// S9: emit the terminal lifecycle envelopes for a finished run (everything
/// v1's blocking `prompt` handler emitted EXCEPT `session.started` and the
/// round seams, which v2 now streams live). Preserves full v1 parity: the
/// legacy flat-JSON batch, the typed agent stdout chunks, and the typed
/// budget / patch / session.ended envelopes plus the legacy `session_end` line.
fn emit_terminal_envelopes(report: &RunReport, bus: &RpcBus) -> Result<()> {
    use nerve_types::rpc_kinds;

    emit_report_events(report)?;

    for event in &report.events {
        for envelope in agent_event_to_envelopes(event, &report.selection.lead) {
            emit_envelope_line(&envelope);
            let _ = bus.emit(&envelope.kind, envelope.payload.clone());
        }
    }

    let budget_envelope = rpc_envelope(
        rpc_kinds::BUDGET_CHANGED,
        serde_json::json!({
            "session_id": report.task.id,
            "input_tokens": report.usage.input_tokens,
            "output_tokens": report.usage.output_tokens,
            "estimated_cost_microusd": report.usage.estimated_cost_microusd,
            "budget_exceeded": report.budget_exceeded,
        }),
    );
    emit_envelope_line(&budget_envelope);
    let _ = bus.emit(rpc_kinds::BUDGET_CHANGED, budget_envelope.payload.clone());

    if let Some(patch) = &report.final_patch {
        let kind = if report.applied {
            rpc_kinds::PATCH_APPLIED
        } else {
            rpc_kinds::PATCH_DISCARDED
        };
        let patch_envelope = rpc_envelope(
            kind,
            serde_json::json!({
                "session_id": report.task.id,
                "patch_id": patch.id,
                "files": patch.files.len(),
                "blocked": report.blocked,
            }),
        );
        emit_envelope_line(&patch_envelope);
        let _ = bus.emit(kind, patch_envelope.payload.clone());
    }

    let session_ended = rpc_envelope(
        rpc_kinds::SESSION_ENDED,
        serde_json::json!({
            "session_id": report.task.id,
            "verdict": report.final_feedback.verdict,
            "applied": report.applied,
            "blocked": report.blocked,
            // S10: additive field naming the live-crossfire-Block reason behind a
            // blocked run (Halt action). Additive payload key, no schema bump.
            "crossfire_halted": report.crossfire_halted,
            "patch_id": report.final_patch.as_ref().map(|patch| patch.id.clone()),
        }),
    );
    emit_envelope_line(&session_ended);
    let _ = bus.emit(rpc_kinds::SESSION_ENDED, session_ended.payload.clone());

    // Legacy `session_end` JSON kept for v0.3.0 consumers; v0.5.0 consumers read
    // the typed envelope above.
    println!(
        "{}",
        serde_json::json!({
            "type": "session_end",
            "session_id": report.task.id,
            "verdict": report.final_feedback.verdict,
            "applied": report.applied,
            "blocked": report.blocked,
            "patch_id": report.final_patch.as_ref().map(|patch| patch.id.clone())
        })
    );
    Ok(())
}

/// S9: spawn a synaptic-loop run WITHOUT blocking the daemon read loop. Emits
/// `session.started` immediately, streams `round.started`/`round.ended` live as
/// each round completes (via the loop's round observer), and emits the terminal
/// lifecycle envelopes when the run finishes. The run is tracked in `registry`
/// by run-id so shutdown can await it.
///
/// Streaming + checkpoints are read-only telemetry: the run still goes through
/// the unchanged `run_synaptic_loop` deterministic gate, and nothing here can
/// auto-apply or fabricate acceptance.
fn spawn_streaming_run(
    prompt: String,
    apply: bool,
    mock: bool,
    worktree_override: Option<bool>,
    bus: Arc<RpcBus>,
    registry: RunRegistry,
) -> Result<()> {
    use nerve_types::rpc_kinds;

    let cwd = env::current_dir().context("failed to read current directory")?;
    let config = Config::load_from(&cwd)?;
    let mut task = Task::new(prompt, &cwd);
    task.context_paths = collect_context_paths(&task.prompt, &cwd);
    let run_id = task.id.clone();
    // Resolve the profile up front so `session.started` can name lead/reviewer;
    // the loop recomputes it deterministically (cheap, identical result).
    let selection = config.select_profile(&task)?;

    let session_started = rpc_envelope(
        rpc_kinds::SESSION_STARTED,
        serde_json::json!({
            "session_id": run_id,
            "prompt": task.prompt,
            "lead": selection.lead,
            "reviewer": selection.reviewer,
        }),
    );
    emit_envelope_line(&session_started);
    let _ = bus.emit(rpc_kinds::SESSION_STARTED, session_started.payload.clone());

    // S11: the shared apply-consent handle for THIS run. The run task holds one
    // clone (read at the apply seam); the registry holds another so the operator
    // can `approve` this run-id mid-flight. Held in daemon memory — unreachable
    // by the lead subprocess.
    let consent = ApplyConsent::new();
    let run_consent = consent.clone();

    let (round_tx, mut round_rx) = mpsc::unbounded_channel::<RoundRecord>();

    // STREAMER: forward each live round to round.started + round.ended. Ends
    // when `round_tx` drops (the loop finished and released the observer).
    let streamer_bus = bus.clone();
    let streamer_id = run_id.clone();
    let streamer = tokio::spawn(async move {
        while let Some(round) = round_rx.recv().await {
            let (started, ended) = round_seam_envelopes(&streamer_id, &round);
            emit_envelope_line(&started);
            let _ = streamer_bus.emit(&started.kind, started.payload.clone());
            emit_envelope_line(&ended);
            let _ = streamer_bus.emit(&ended.kind, ended.payload.clone());
        }
    });

    // RUN: drive the streaming loop, persist, then emit terminal envelopes.
    let run_bus = bus.clone();
    let run_id_for_task = run_id.clone();
    let handle = tokio::spawn(async move {
        let mut options = RunOptions::new(apply).with_apply_grant(run_consent);
        if let Some(spec) = config.orchestration.check_ulimit.as_ref()
            && !spec.is_empty()
        {
            options = options.with_ulimit(core_ulimit_from_config(spec));
        }
        if let Some(on) = worktree_override {
            options = options.with_worktree(on);
        }
        let adapters = adapters_for_config(mock, &config);
        let outcome =
            run_synaptic_loop_streaming(task, &config, &adapters, options, round_tx).await;

        // The loop has released `round_tx`, so the streamer's channel is now
        // closed; await it FIRST so every live round seam is flushed to stdout
        // BEFORE any terminal envelope. This preserves causal ordering on the
        // JSONL stream (session.started -> round seams -> terminal envelopes);
        // without it the streamer could still be draining buffered rounds while
        // `session.ended` is printed, interleaving a round.ended after the end.
        let _ = streamer.await;

        match outcome {
            Ok(report) => {
                if let Err(error) = NerveStore::new(&cwd).save_report(&report) {
                    eprintln!("warning: failed to persist session report: {error}");
                }
                if let Err(error) = emit_terminal_envelopes(&report, &run_bus) {
                    eprintln!("warning: failed to emit terminal envelopes: {error}");
                }
            }
            Err(error) => {
                // The run failed before producing a report; surface it on the
                // same JSONL stream so a consumer is not left waiting forever.
                println!(
                    "{}",
                    serde_json::json!({
                        "type": "error",
                        "session_id": run_id_for_task,
                        "message": error.to_string(),
                    })
                );
            }
        }
    });

    if let Ok(mut reg) = registry.lock() {
        // Drop handles for runs that already finished so the map stays bounded.
        reg.retain(|_, run| !run.join.is_finished());
        reg.insert(
            run_id,
            TrackedRun {
                join: handle,
                consent,
            },
        );
    }
    Ok(())
}

/// Convert an [`AgentEvent`] into the Tier 2e envelope kinds expected by
/// subscribers (lead / reviewer stdout chunks). Returns `None` when the
/// event has no envelope representation (e.g. terminal `Done`).
fn agent_event_to_envelopes(event: &AgentEvent, lead_id: &str) -> Vec<RpcEnvelope> {
    use nerve_types::rpc_kinds;

    let (agent_id, line, is_lead) = match event {
        AgentEvent::Stdout { agent_id, line } => {
            (agent_id.clone(), line.clone(), agent_id == lead_id)
        }
        AgentEvent::Stderr { agent_id, line } => {
            (agent_id.clone(), line.clone(), agent_id == lead_id)
        }
        AgentEvent::Tool { agent_id, call } => (
            agent_id.clone(),
            format!("tool:{} {}", call.name, call.arguments),
            agent_id == lead_id,
        ),
        AgentEvent::Done { .. } => return Vec::new(),
    };
    let kind = if is_lead {
        rpc_kinds::LEAD_STDOUT
    } else {
        rpc_kinds::REVIEWER_STDOUT
    };
    vec![rpc_envelope(
        kind,
        serde_json::json!({ "agent_id": agent_id, "chunk": line }),
    )]
}

async fn handle_rpc_command(
    value: serde_json::Value,
    apply: bool,
    mock: bool,
    bus: Arc<RpcBus>,
    worktree_override: Option<bool>,
    registry: RunRegistry,
) -> Result<()> {
    use nerve_types::rpc_kinds;

    let command = value
        .get("command")
        .and_then(serde_json::Value::as_str)
        .context("missing string field `command`")?;
    match command {
        "prompt" => {
            // S9: spawn the run WITHOUT blocking the read loop. `session.started`
            // is emitted immediately, round seams stream live, and the terminal
            // envelopes are emitted when the run finishes. The deterministic
            // acceptance gate is unchanged — this only changes WHEN envelopes are
            // emitted (live vs the old post-hoc replay), never WHETHER a patch is
            // accepted.
            let prompt = value
                .get("prompt")
                .and_then(serde_json::Value::as_str)
                .context("missing string field `prompt`")?;
            spawn_streaming_run(
                prompt.to_string(),
                apply,
                mock,
                worktree_override,
                bus.clone(),
                registry.clone(),
            )?;
        }
        "approve" => {
            // S11: escalate a SPECIFIC in-flight run to apply-consent. The
            // operator names the run-id (from `session.started`/`status`); we
            // flip its IN-MEMORY `ApplyConsent` handle (the forge-proof gate
            // input the lead cannot reach) so the run applies its accepted patch
            // at its apply seam. We also record an audit grant under
            // `.nerve/approvals/` so a reconnecting client can see the standing
            // grant. This NEVER weakens acceptance: the run still applies only
            // if the deterministic gate did not block it.
            let run_id = value
                .get("run_id")
                .or_else(|| value.get("session_id"))
                .and_then(serde_json::Value::as_str)
                .context("missing string field `run_id`")?;
            // Only an in-flight (not-yet-finished) run can still reach its apply
            // seam, so only it may be escalated; a finished/unknown run-id is a
            // no-op (`granted:false`, nothing written). See `grant_in_flight`.
            let granted = match registry.lock() {
                Ok(reg) => grant_in_flight(&reg, run_id),
                Err(_) => false,
            };
            if granted {
                let cwd = env::current_dir().context("failed to read current directory")?;
                if let Err(error) =
                    NerveStore::new(&cwd).record_approval(&ApprovalGrant::apply(run_id))
                {
                    eprintln!("warning: failed to record approval grant: {error}");
                }
            }
            // A plain ack line (additive, no schema bump). `granted=false` means
            // the run-id is not in flight (finished or never existed), so nothing
            // was escalated.
            println!(
                "{}",
                serde_json::json!({
                    "type": "approve_ack",
                    "run_id": run_id,
                    "granted": granted,
                })
            );
        }
        "status" => {
            // S9: report in-flight runs from the S8 on-disk checkpoints, so this
            // works even across a daemon restart. A checkpoint has no acceptance
            // fields, so `status` can only report progress, never acceptance.
            let cwd = env::current_dir().context("failed to read current directory")?;
            let store = NerveStore::new(&cwd);
            let checkpoints = store
                .list_checkpoints()
                .with_context(|| "failed to list in-flight run checkpoints")?;
            for checkpoint in &checkpoints {
                let envelope = checkpoint_status_envelope(checkpoint);
                emit_envelope_line(&envelope);
                let _ = bus.emit(&envelope.kind, envelope.payload.clone());
                // S11: surface a standing apply-consent grant for this run so a
                // reconnecting client knows it was approved. Audit-only — this
                // never drives the gate (the gate reads the in-memory handle).
                if let Ok(Some(grant)) = store.load_approval(&checkpoint.task.id) {
                    println!(
                        "{}",
                        serde_json::json!({
                            "type": "approval_grant",
                            "run_id": grant.run_id,
                            "apply_consent": grant.apply_consent,
                            "granted_at": grant.granted_at,
                        })
                    );
                }
            }
            // A terminal summary line so a consumer knows the listing is complete.
            println!(
                "{}",
                serde_json::json!({
                    "type": "status_end",
                    "in_flight": checkpoints.len(),
                })
            );
        }
        "plan" => {
            // RPC entry point for `/plan`. Mirrors the CLI subcommand but
            // emits a single `plan.proposed` envelope on success.
            let prompt = value
                .get("prompt")
                .and_then(serde_json::Value::as_str)
                .context("missing string field `prompt`")?;
            let dual_review = value
                .get("dual_review")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false);
            let cwd = env::current_dir().context("failed to read current directory")?;
            let config = Config::load_from(&cwd)?;
            let requested_strategy = if dual_review {
                PlanStrategy::DualReview
            } else {
                PlanStrategy::Single
            };
            let mut task = Task::new(prompt.to_string(), &cwd);
            task.context_paths = collect_context_paths(&task.prompt, &cwd);
            let adapters = adapters_for_config(mock, &config);
            let report = run_plan_mode(
                task,
                Arc::new(config),
                adapters,
                PlanRunOptions::new(requested_strategy),
            )
            .await
            .map_err(plan_error_to_anyhow)?;
            let envelope = rpc_envelope(
                rpc_kinds::PLAN_PROPOSED,
                serde_json::json!({
                    "task_id": report.task_id,
                    "plan_markdown": report.plan_markdown,
                    "reviewer_feedback": report.reviewer_feedback,
                    "estimated_loc": report.estimated_loc,
                    "estimated_files": report.estimated_files,
                }),
            );
            emit_envelope_line(&envelope);
            let _ = bus.emit(rpc_kinds::PLAN_PROPOSED, envelope.payload.clone());
        }
        "get_state" | "history" => {
            let cwd = env::current_dir().context("failed to read current directory")?;
            println!(
                "{}",
                serde_json::json!({
                    "type": "history",
                    "sessions": NerveStore::new(cwd).list_sessions()?
                })
            );
        }
        "resume" => {
            let session_id = value
                .get("session_id")
                .and_then(serde_json::Value::as_str)
                .context("missing string field `session_id`")?;
            let cwd = env::current_dir().context("failed to read current directory")?;
            println!(
                "{}",
                serde_json::json!({
                    "type": "session",
                    "report": NerveStore::new(cwd).load_report(session_id)?
                })
            );
        }
        "list_patches" => {
            let cwd = env::current_dir().context("failed to read current directory")?;
            println!(
                "{}",
                serde_json::json!({
                    "type": "patches",
                    "patches": NerveStore::new(cwd).list_patches()?
                })
            );
        }
        "apply_patch" => {
            let patch_id = value
                .get("patch_id")
                .and_then(serde_json::Value::as_str)
                .context("missing string field `patch_id`")?;
            let cwd = env::current_dir().context("failed to read current directory")?;
            println!(
                "{}",
                serde_json::json!({
                    "type": "apply_result",
                    "report": NerveStore::new(cwd).apply_patch(patch_id)?
                })
            );
        }
        "rollback_patch" => {
            let patch_id = value
                .get("patch_id")
                .and_then(serde_json::Value::as_str)
                .context("missing string field `patch_id`")?;
            let cwd = env::current_dir().context("failed to read current directory")?;
            println!(
                "{}",
                serde_json::json!({
                    "type": "apply_result",
                    "report": NerveStore::new(cwd).rollback_patch(patch_id)?
                })
            );
        }
        other => anyhow::bail!("unknown RPC command `{other}`"),
    }
    Ok(())
}

#[cfg(unix)]
struct RawTerminalGuard {
    fd: std::os::fd::RawFd,
    original: libc::termios,
    raw: libc::termios,
    active: bool,
}

#[cfg(unix)]
impl RawTerminalGuard {
    fn suspend(&mut self) -> Result<()> {
        if self.active {
            unsafe {
                if libc::tcsetattr(self.fd, libc::TCSANOW, &self.original) != 0 {
                    return Err(std::io::Error::last_os_error())
                        .context("failed to restore terminal settings");
                }
            }
            self.active = false;
        }
        Ok(())
    }

    fn resume(&mut self) -> Result<()> {
        if !self.active {
            unsafe {
                if libc::tcsetattr(self.fd, libc::TCSANOW, &self.raw) != 0 {
                    return Err(std::io::Error::last_os_error())
                        .context("failed to enable raw terminal input");
                }
            }
            self.active = true;
        }
        Ok(())
    }
}

#[cfg(unix)]
impl Drop for RawTerminalGuard {
    fn drop(&mut self) {
        unsafe {
            libc::tcsetattr(self.fd, libc::TCSANOW, &self.original);
        }
    }
}

#[cfg(unix)]
fn enable_stdin_raw_if_terminal() -> Result<RawTerminalGuard> {
    use std::os::fd::AsRawFd;

    let stdin = std::io::stdin();
    let fd = stdin.as_raw_fd();
    let mut original = std::mem::MaybeUninit::<libc::termios>::uninit();
    unsafe {
        if libc::tcgetattr(fd, original.as_mut_ptr()) != 0 {
            return Err(std::io::Error::last_os_error())
                .context("failed to read terminal settings");
        }

        let original = original.assume_init();
        let mut raw = original;
        raw.c_lflag &= !(libc::ECHO | libc::ICANON | libc::IEXTEN | libc::ISIG);
        raw.c_iflag &= !(libc::IXON | libc::ICRNL);
        raw.c_oflag &= !libc::OPOST;
        raw.c_cc[libc::VMIN] = 1;
        raw.c_cc[libc::VTIME] = 0;

        if libc::tcsetattr(fd, libc::TCSANOW, &raw) != 0 {
            return Err(std::io::Error::last_os_error())
                .context("failed to enable raw terminal input");
        }

        Ok(RawTerminalGuard {
            fd,
            original,
            raw,
            active: true,
        })
    }
}

#[cfg(not(unix))]
struct RawTerminalGuard;

#[cfg(not(unix))]
impl RawTerminalGuard {
    fn suspend(&mut self) -> Result<()> {
        Ok(())
    }

    fn resume(&mut self) -> Result<()> {
        Ok(())
    }
}

#[cfg(not(unix))]
fn enable_stdin_raw_if_terminal() -> Result<RawTerminalGuard> {
    Ok(RawTerminalGuard)
}

#[cfg(unix)]
struct TerminalEchoGuard {
    fd: std::os::fd::RawFd,
    original: libc::termios,
}

#[cfg(unix)]
impl Drop for TerminalEchoGuard {
    fn drop(&mut self) {
        unsafe {
            libc::tcsetattr(self.fd, libc::TCSANOW, &self.original);
        }
    }
}

#[cfg(unix)]
fn disable_stdin_echo_if_terminal() -> Result<Option<TerminalEchoGuard>> {
    use std::io::IsTerminal;
    use std::os::fd::AsRawFd;

    let stdin = std::io::stdin();
    if !stdin.is_terminal() {
        return Ok(None);
    }

    let fd = stdin.as_raw_fd();
    let mut original = std::mem::MaybeUninit::<libc::termios>::uninit();
    unsafe {
        if libc::tcgetattr(fd, original.as_mut_ptr()) != 0 {
            return Err(std::io::Error::last_os_error())
                .context("failed to read terminal settings");
        }

        let original = original.assume_init();
        let mut adjusted = original;
        adjusted.c_lflag &= !libc::ECHO;
        if libc::tcsetattr(fd, libc::TCSANOW, &adjusted) != 0 {
            return Err(std::io::Error::last_os_error())
                .context("failed to disable terminal input echo");
        }

        Ok(Some(TerminalEchoGuard { fd, original }))
    }
}

#[cfg(not(unix))]
fn disable_stdin_echo_if_terminal() -> Result<Option<()>> {
    Ok(None)
}

async fn run_report(
    prompt: String,
    apply: bool,
    mock: bool,
    worktree_override: Option<bool>,
) -> Result<RunReport> {
    run_report_with_overrides(prompt, apply, mock, None, None, None, worktree_override).await
}

async fn run_report_with_overrides(
    prompt: String,
    apply: bool,
    mock: bool,
    goal: Option<GoalSpec>,
    budget_cost_override: Option<u64>,
    budget_tokens_override: Option<u64>,
    worktree_override: Option<bool>,
) -> Result<RunReport> {
    let cwd = env::current_dir().context("failed to read current directory")?;
    let mut config = Config::load_from(&cwd)?;
    if let Some(cost) = budget_cost_override {
        config.orchestration.max_estimated_cost_microusd = Some(cost);
    }
    if let Some(tokens) = budget_tokens_override {
        config.orchestration.max_total_tokens = Some(tokens);
    }
    let mut task = Task::new(prompt, &cwd);
    task.context_paths = collect_context_paths(&task.prompt, &cwd);
    let adapters = adapters_for_config(mock, &config);
    let mut options = RunOptions::new(apply);
    if let Some(spec) = goal {
        options = options.with_goal(spec);
    } else {
        // S4: surface which deterministic gate will run when no explicit /goal
        // is set, so acceptance is never silently reduced to reviewer opinion.
        announce_builtin_verifier(&config, &cwd);
    }
    if let Some(spec) = config.orchestration.check_ulimit.as_ref()
        && !spec.is_empty()
    {
        options = options.with_ulimit(core_ulimit_from_config(spec));
    }
    if let Some(on) = worktree_override {
        options = options.with_worktree(on);
    }
    let report = run_synaptic_loop(task, &config, &adapters, options).await?;
    NerveStore::new(&cwd).save_report(&report)?;
    Ok(report)
}

/// S4: print which deterministic gate will run when a `nv run` has no explicit
/// `/goal`. Loud by design — the dangerous case (no verifier AND no goal, so
/// acceptance rests on the reviewer verdict alone) is always a warning, never
/// silence, even when the built-in verifier is `off` (the safe default).
fn announce_builtin_verifier(config: &Config, cwd: &Path) {
    // S4 trust boundary: a project-local `nerve.config.json` cannot enable the
    // executing Auto/Command modes without out-of-band operator consent.
    let consent = project_verifier_consent_from_env();
    let exec_trusted = config.builtin_verifier_exec_trusted(consent);
    let mode = &config.orchestration.builtin_verifier.mode;
    match resolve_builtin_verifier(&config.orchestration, cwd, exec_trusted) {
        // Operator opted in (auto/command) from a trusted source and a gate
        // resolved: it WILL execute repo code. Muted info — the consented case.
        Some(resolved) => {
            eprintln!(
                "{}",
                muted(format!(
                    "verifier: {} (built-in; no /goal set — override with --goal or orchestration.builtin_verifier)",
                    resolved.label
                ))
            );
        }
        // No deterministic gate. Acceptance would rest on the reviewer verdict
        // alone — the exact gap S4 closes — so warn loudly and show how to opt
        // in. `off` is the default (Nerve never executes repo code without
        // consent), so this fires for most fresh runs by design.
        None => {
            let hint: String = if !exec_trusted && !matches!(mode, BuiltinVerifierMode::Off) {
                // Project config asked for an executing verifier; refused.
                format!(
                    "this project's nerve.config.json requested it but repo config cannot run code without consent — move the setting to ~/.config/nerve/config.json or set {PROJECT_VERIFIER_CONSENT_ENV}=1, or pass --goal"
                )
            } else if matches!(mode, BuiltinVerifierMode::Off) {
                "set orchestration.builtin_verifier.mode = \"auto\" (runs your project's tests) or \"command\", or pass --goal".to_string()
            } else {
                "no test/build markers detected; set orchestration.builtin_verifier.command or pass --goal".to_string()
            };
            eprintln!(
                "{}",
                warn(format!(
                    "⚠ no deterministic verifier and no /goal — acceptance rests on the reviewer verdict alone; {hint}"
                ))
            );
        }
    }
}

fn adapters_for_config(mock: bool, config: &Config) -> Vec<Box<dyn ModelAdapter>> {
    default_adapters_with_limits(
        mock,
        AdapterLimits::new(
            config.orchestration.adapter_timeout_secs,
            config.orchestration.adapter_max_output_bytes,
            config.orchestration.adapter_spawn_retries,
        ),
    )
}

fn core_ulimit_from_config(spec: &nerve_config::CheckUlimit) -> nerve_core::ulimit::CheckUlimit {
    nerve_core::ulimit::CheckUlimit {
        nproc: spec.nproc,
        address_space_bytes: spec.memory_bytes,
        file_size_bytes: spec.file_size_bytes,
        cpu_secs: spec.cpu_secs,
    }
}

fn emit_report_events(report: &RunReport) -> Result<()> {
    println!(
        "{}",
        serde_json::json!({
            "type": "session_start",
            "session_id": report.task.id,
            "prompt": report.task.prompt,
            "profile": report.selection.id,
            "lead": report.selection.lead,
            "reviewer": report.selection.reviewer
        })
    );
    println!(
        "{}",
        serde_json::json!({
            "type": "lead_start",
            "session_id": report.task.id,
            "agent_id": report.final_output.agent_id
        })
    );
    for event in &report.events {
        let (event_type, payload) = match event {
            AgentEvent::Stdout { agent_id, line } => (
                "lead_event",
                serde_json::json!({ "agent_id": agent_id, "stream": "stdout", "line": line }),
            ),
            AgentEvent::Stderr { agent_id, line } => (
                "lead_event",
                serde_json::json!({ "agent_id": agent_id, "stream": "stderr", "line": line }),
            ),
            AgentEvent::Tool { agent_id, call } => (
                "lead_event",
                serde_json::json!({ "agent_id": agent_id, "tool": call.name, "arguments": call.arguments }),
            ),
            AgentEvent::Done { agent_id } => (
                "lead_event",
                serde_json::json!({ "agent_id": agent_id, "done": true }),
            ),
        };
        println!(
            "{}",
            serde_json::json!({
                "type": event_type,
                "session_id": report.task.id,
                "event": payload
            })
        );
    }
    println!(
        "{}",
        serde_json::json!({
            "type": "review_start",
            "session_id": report.task.id,
            "agent_id": report.final_feedback.reviewer_id
        })
    );
    println!(
        "{}",
        serde_json::json!({
            "type": "review_event",
            "session_id": report.task.id,
            "verdict": report.final_feedback.verdict,
            "message": report.final_feedback.raw_text
        })
    );
    println!(
        "{}",
        serde_json::json!({
            "type": "patch_ready",
            "session_id": report.task.id,
            "patch_id": report.final_patch.as_ref().map(|patch| patch.id.clone()),
            "files": report.final_patch.as_ref().map(|patch| patch.files.len()).unwrap_or(0),
            "blocked": report.blocked
        })
    );
    println!(
        "{}",
        serde_json::json!({
            "type": "apply_result",
            "session_id": report.task.id,
            "applied": report.applied
        })
    );
    Ok(())
}

fn print_report(report: &nerve_core::RunReport, apply_requested: bool) {
    println!("Nerve session {}", report.task.id);
    println!(
        "Profile: {} | lead={} reviewer={} rounds={}",
        report.selection.id.as_deref().unwrap_or("default"),
        report.selection.lead,
        report.selection.reviewer,
        report.rounds.len()
    );
    print_events(&report.events);
    if !report.crossfire_feedback.is_empty() {
        println!(
            "Crossfire feedback: {} event(s)",
            report.crossfire_feedback.len()
        );
    }
    println!("Verdict: {:?}", report.final_feedback.verdict);
    println!(
        "Usage: input={} output={} total={} cost_microusd={}",
        report.usage.input_tokens,
        report.usage.output_tokens,
        report.usage.total_tokens(),
        report
            .usage
            .estimated_cost_microusd
            .map(|value| value.to_string())
            .unwrap_or_else(|| "-".to_string())
    );

    if report.budget_exceeded {
        println!("Session budget exceeded; no files were changed.");
    }

    if report.crossfire_halted {
        println!(
            "Run short-circuited by a live crossfire Block (S10 halt); no files were changed."
        );
    } else if report.blocked && !report.budget_exceeded {
        println!("Patch blocked by reviewer policy; no files were changed.");
    }

    if let Some(patch) = &report.final_patch {
        let diff = patch.to_unified_diff();
        if diff.trim().is_empty() {
            println!("No diff produced.");
        } else {
            println!("\n{}", diff);
        }
        if report.applied {
            println!("Applied patch {}", patch.id);
        } else if !apply_requested {
            println!("Dry run only. Re-run with --apply to change files.");
        }
    } else {
        println!("\nNo structured patch was produced. Raw lead output:\n");
        println!("{}", report.final_output.raw_text);
    }

    if report.final_feedback.verdict != Verdict::Lgtm {
        println!("\nReviewer feedback:\n{}", report.final_feedback.raw_text);
    }
}

fn print_events(events: &[AgentEvent]) {
    for event in events {
        match event {
            AgentEvent::Stdout { agent_id, line } => println!("[{agent_id}] {line}"),
            AgentEvent::Stderr { agent_id, line } => eprintln!("[{agent_id} stderr] {line}"),
            AgentEvent::Tool { agent_id, call } => {
                println!("[{agent_id} tool] {} {}", call.name, call.arguments)
            }
            AgentEvent::Done { agent_id } => println!("[{agent_id}] done"),
        }
    }
}

fn push_panel_text(lines: &mut Vec<String>, value: &str) {
    let value = truncate_panel_text(value);
    let mut wrote_line = false;
    for line in value.lines() {
        wrote_line = true;
        if line.trim().is_empty() {
            lines.push("  ".to_string());
        } else {
            lines.push(format!("  {line}"));
        }
    }
    if !wrote_line {
        lines.push("  -".to_string());
    }
}

fn print_tui_report(report: &RunReport) {
    print_box(
        "Nerve TUI",
        &[
            format!("session {}", report.task.id),
            format!(
                "profile {}  rounds {}  verdict {:?}",
                report.selection.id.as_deref().unwrap_or("default"),
                report.rounds.len(),
                report.final_feedback.verdict
            ),
            format!(
                "applied {}  blocked {}  budget_exceeded {}",
                report.applied, report.blocked, report.budget_exceeded
            ),
        ],
    );

    let mut lead_lines = vec![
        format!("agent {}", report.final_output.agent_id),
        "output".to_string(),
    ];
    push_panel_text(&mut lead_lines, &report.final_output.raw_text);
    if let Some(patch) = &report.final_patch {
        lead_lines.push(format!(
            "patch {} file(s), id {}",
            patch.files.len(),
            patch.id
        ));
    } else {
        lead_lines.push("patch none".to_string());
    }
    print_box("[ Lead ]", &lead_lines);

    let mut reviewer_lines = vec![format!(
        "agent {}  verdict {:?}",
        report.final_feedback.reviewer_id, report.final_feedback.verdict
    )];
    push_panel_text(&mut reviewer_lines, &report.final_feedback.raw_text);
    if !report.crossfire_feedback.is_empty() {
        reviewer_lines.push(format!(
            "crossfire events {}",
            report.crossfire_feedback.len()
        ));
    }
    print_box("[ Reviewer ]", &reviewer_lines);

    print_box(
        "[ Orchestrator ]",
        &[
            format!(
                "profile {}  rounds {}",
                report.selection.id.as_deref().unwrap_or("default"),
                report.rounds.len()
            ),
            format!(
                "usage input {}  output {}  total {}",
                report.usage.input_tokens,
                report.usage.output_tokens,
                report.usage.total_tokens()
            ),
            format!(
                "cost_microusd {}",
                report
                    .usage
                    .estimated_cost_microusd
                    .map(|value| value.to_string())
                    .unwrap_or_else(|| "-".to_string())
            ),
        ],
    );
}

fn truncate_panel_text(value: &str) -> String {
    const LIMIT: usize = 1200;
    if value.len() <= LIMIT {
        return value.to_string();
    }
    format!("{}...", truncate_at_char_boundary(value, LIMIT))
}

fn truncate_at_char_boundary(value: &str, limit: usize) -> &str {
    let mut boundary = limit.min(value.len());
    while !value.is_char_boundary(boundary) {
        boundary -= 1;
    }
    &value[..boundary]
}

fn short_id(value: &str) -> &str {
    truncate_at_char_boundary(value, 8.min(value.len()))
}

fn print_changed_files(paths: &[std::path::PathBuf]) {
    for path in paths {
        println!("- {}", path.display());
    }
}

fn find_on_path(binary: &str) -> Option<PathBuf> {
    let path = env::var_os("PATH")?;
    env::split_paths(&path)
        .map(|dir| dir.join(binary))
        .find(|candidate| is_executable_file(candidate))
}

#[cfg(unix)]
fn is_executable_file(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.metadata()
        .map(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable_file(path: &Path) -> bool {
    path.is_file()
}

fn writeln_doctor_check(stdout: &mut dyn Write, binary: &str, path: &Option<PathBuf>) {
    match path {
        Some(path) => {
            writeln!(stdout, "{binary}: {}", path.display()).ok();
        }
        None => {
            writeln!(stdout, "{binary}: missing").ok();
        }
    }
}

fn writeln_auth_status(stdout: &mut dyn Write, name: &str, path: &Option<PathBuf>, args: &[&str]) {
    let Some(path) = path else {
        return;
    };
    let Ok(output) = StdCommand::new(path).args(args).output() else {
        writeln!(stdout, "{name} auth: unknown").ok();
        return;
    };
    let text = String::from_utf8_lossy(&output.stdout);
    let summary = summarize_auth_output(name, &text, output.status.success());
    writeln!(stdout, "{name} auth: {summary}").ok();
}

fn summarize_auth_output(name: &str, text: &str, success: bool) -> String {
    if name == "claude"
        && let Ok(value) = serde_json::from_str::<serde_json::Value>(text)
        && let Some(logged_in) = value.get("loggedIn").and_then(serde_json::Value::as_bool)
    {
        let method = value
            .get("authMethod")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("unknown");
        return if logged_in {
            format!("logged in via {method}")
        } else {
            "not logged in; run nv login claude".to_string()
        };
    }

    let fallback = if success { "ok" } else { "not logged in" };
    text.lines()
        .find(|line| !line.trim().is_empty() && !line.starts_with("WARNING:"))
        .map(|line| line.trim().to_string())
        .unwrap_or_else(|| fallback.to_string())
}

/// Result of scanning a prompt for referenced context paths.
///
/// `missing_explicit` holds tokens the user *unambiguously* meant as file
/// references (they contain a path separator) but which do not exist on disk.
/// These are surfaced loudly by [`collect_context_paths`] so the loop never
/// runs against silently-dropped context (S3: fail-loud context loading).
struct ContextScan {
    paths: Vec<PathBuf>,
    missing_explicit: Vec<PathBuf>,
}

fn scan_context_paths(prompt: &str, cwd: &Path) -> ContextScan {
    let mut paths = Vec::new();
    let mut missing_explicit = Vec::new();
    for token in prompt.split_whitespace() {
        let token = token.trim_matches(|ch: char| {
            matches!(
                ch,
                '`' | '"' | '\'' | ',' | ':' | ';' | '(' | ')' | '[' | ']'
            )
        });
        if looks_like_path_token(token) {
            let path = PathBuf::from(token);
            // Only `/`-bearing tokens are treated as explicit, unambiguous file
            // references worth warning about — a bare `config.json` or a version
            // string like `v1.0.0` must not trigger a false "not found" alarm.
            if token.contains('/') && !context_path_exists(cwd, &path) {
                push_unique_path(&mut missing_explicit, path.clone());
            }
            push_unique_path(&mut paths, path);
        }
    }

    for path in git_changed_paths(cwd) {
        push_unique_path(&mut paths, path);
    }

    ContextScan {
        paths,
        missing_explicit,
    }
}

/// Resolve a referenced path against `cwd` (absolute paths checked as-is) and
/// report whether it exists.
fn context_path_exists(cwd: &Path, path: &Path) -> bool {
    if path.is_absolute() {
        path.exists()
    } else {
        cwd.join(path).exists()
    }
}

fn collect_context_paths(prompt: &str, cwd: &Path) -> Vec<PathBuf> {
    let scan = scan_context_paths(prompt, cwd);
    for missing in &scan.missing_explicit {
        eprintln!(
            "{}",
            warn(format!(
                "⚠ referenced path not found: {} — context may be incomplete",
                missing.display()
            ))
        );
    }
    scan.paths
}

fn looks_like_path_token(token: &str) -> bool {
    !token.is_empty()
        && !token.contains("://")
        && (token.contains('/')
            || token
                .rsplit_once('.')
                .is_some_and(|(_, ext)| !ext.is_empty()))
}

fn git_changed_paths(cwd: &Path) -> Vec<PathBuf> {
    let Ok(output) = StdCommand::new("git")
        .args(["diff", "--name-only", "HEAD"])
        .current_dir(cwd)
        .output()
    else {
        return Vec::new();
    };

    if !output.status.success() {
        return Vec::new();
    }

    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(PathBuf::from)
        .collect()
}

fn push_unique_path(paths: &mut Vec<PathBuf>, path: PathBuf) {
    if !paths.iter().any(|existing| existing == &path) {
        paths.push(path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tui_truncation_preserves_utf8_boundaries() {
        let mut value = "a".repeat(1199);
        value.push_str("한글");

        let truncated = truncate_panel_text(&value);

        assert!(truncated.ends_with("..."));
        assert!(truncated.is_char_boundary(truncated.len()));
    }

    #[test]
    fn context_paths_include_prompt_path_tokens() {
        let paths = collect_context_paths("audit crates/nerve-core/src/lib.rs now", Path::new("."));

        assert!(paths.contains(&PathBuf::from("crates/nerve-core/src/lib.rs")));
    }

    // --- S11: approve escalates in-flight runs only ---------------------------

    #[tokio::test]
    async fn approve_grants_only_in_flight_runs() {
        // S11 (codex r2 fix): a FINISHED or UNKNOWN run-id must NOT be granted and
        // must flip no consent — only an in-flight run can still reach its apply
        // seam, so approving a finished run would write a misleading audit record
        // for a run that can never apply. Finished entries linger in the registry
        // until the next spawn/shutdown, so `grant_in_flight`'s `is_finished`
        // guard (not mere presence) is what enforces the "in-flight only" contract.
        let mut reg: HashMap<String, TrackedRun> = HashMap::new();

        // A finished run: an empty task driven to completion by the scheduler.
        let finished = tokio::spawn(async {});
        while !finished.is_finished() {
            tokio::task::yield_now().await;
        }
        let finished_consent = ApplyConsent::new();
        reg.insert(
            "finished".to_string(),
            TrackedRun {
                join: finished,
                consent: finished_consent.clone(),
            },
        );

        // An in-flight run: a task that never completes on its own.
        let inflight = tokio::spawn(std::future::pending::<()>());
        let inflight_consent = ApplyConsent::new();
        reg.insert(
            "inflight".to_string(),
            TrackedRun {
                join: inflight,
                consent: inflight_consent.clone(),
            },
        );

        // Finished run: not granted, consent untouched (no misleading audit).
        assert!(!grant_in_flight(&reg, "finished"));
        assert!(!finished_consent.is_granted());
        // Unknown run-id: not granted.
        assert!(!grant_in_flight(&reg, "does-not-exist"));
        // In-flight run: granted, consent flipped.
        assert!(grant_in_flight(&reg, "inflight"));
        assert!(inflight_consent.is_granted());
    }

    // --- S9: live round-seam stream + status helpers --------------------------

    #[test]
    fn round_seam_envelopes_carry_session_round_and_verdict() {
        let round = nerve_types::RoundRecord {
            round: 2,
            lead: nerve_types::AgentOutput::text("lead", "round 2"),
            reviewer: nerve_types::ReviewerFeedback::lgtm("reviewer", "LGTM"),
            check_result: Some(nerve_types::CheckResult::Pass),
            patch_sha: Some("sha2".to_string()),
            envelope_id: None,
        };
        let (started, ended) = round_seam_envelopes("run-1", &round);

        assert_eq!(started.kind, nerve_types::rpc_kinds::ROUND_STARTED);
        assert_eq!(started.payload["session_id"], "run-1");
        assert_eq!(started.payload["round"], 2);

        assert_eq!(ended.kind, nerve_types::rpc_kinds::ROUND_ENDED);
        assert_eq!(ended.payload["session_id"], "run-1");
        assert_eq!(ended.payload["round"], 2);
        assert_eq!(
            ended.payload["verdict"],
            serde_json::to_value(round.reviewer.verdict.clone()).unwrap()
        );
        assert_eq!(
            ended.payload["check"],
            serde_json::to_value(round.check_result.clone()).unwrap()
        );
    }

    #[test]
    fn checkpoint_status_envelope_reports_progress_not_acceptance() {
        let dir = tempfile::tempdir().unwrap();
        let task = Task::new("in flight", dir.path());
        let checkpoint = RunCheckpoint {
            task: task.clone(),
            selection: nerve_config::ProfileSelection {
                id: None,
                lead: "lead".to_string(),
                reviewer: "reviewer".to_string(),
                review_strictness: nerve_config::ReviewStrictness::Normal,
                max_refinement_rounds: 1,
                plan_strategy: PlanStrategy::Single,
                plan_system_prompt_override: None,
            },
            status: nerve_core::store::RunStatus::Running,
            rounds: Vec::new(),
            updated_at: "2026-06-17T00:00:00Z".to_string(),
        };
        let envelope = checkpoint_status_envelope(&checkpoint);

        assert_eq!(envelope.kind, nerve_types::rpc_kinds::SESSION_STATUS);
        assert_eq!(envelope.payload["session_id"], task.id);
        assert_eq!(envelope.payload["status"], "running");
        assert_eq!(envelope.payload["rounds"], 0);
        // North star: a status envelope can NEVER assert acceptance — it is
        // derived from a checkpoint, which carries no acceptance fields.
        assert!(envelope.payload.get("applied").is_none());
        assert!(envelope.payload.get("blocked").is_none());
        assert!(envelope.payload.get("goal_satisfied").is_none());
    }

    #[test]
    fn scan_flags_missing_explicit_path_reference() {
        let dir = tempfile::tempdir().unwrap();
        let scan = scan_context_paths("please fix src/missing/module.rs", dir.path());

        // The slash-bearing reference is collected as context AND flagged missing.
        assert!(
            scan.paths
                .contains(&PathBuf::from("src/missing/module.rs"))
        );
        assert!(
            scan.missing_explicit
                .contains(&PathBuf::from("src/missing/module.rs")),
            "missing slash-path must be flagged: {:?}",
            scan.missing_explicit
        );
    }

    #[test]
    fn scan_does_not_flag_existing_path_reference() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/lib.rs"), "fn main() {}\n").unwrap();

        let scan = scan_context_paths("review src/lib.rs carefully", dir.path());

        assert!(scan.paths.contains(&PathBuf::from("src/lib.rs")));
        assert!(
            scan.missing_explicit.is_empty(),
            "existing path must not be flagged: {:?}",
            scan.missing_explicit
        );
    }

    #[test]
    fn scan_does_not_false_alarm_on_dotted_non_path_tokens() {
        let dir = tempfile::tempdir().unwrap();
        // `v1.0.0` and bare `config.json` lack a path separator: even though
        // they look path-ish, they must not raise a missing-context alarm.
        let scan = scan_context_paths("bump to v1.0.0 and tweak config.json", dir.path());

        assert!(
            scan.missing_explicit.is_empty(),
            "dotted non-slash tokens must not be flagged: {:?}",
            scan.missing_explicit
        );
    }

    #[test]
    fn scan_flags_missing_absolute_path_reference() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("nope/gone.rs");
        let prompt = format!("inspect {}", missing.display());

        let scan = scan_context_paths(&prompt, dir.path());

        assert!(
            scan.missing_explicit.contains(&missing),
            "missing absolute path must be flagged: {:?}",
            scan.missing_explicit
        );
    }

    #[cfg(unix)]
    #[test]
    fn executable_check_rejects_files_without_execute_bit() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("codex");
        std::fs::write(&path, "#!/bin/sh\n").unwrap();
        let mut permissions = std::fs::metadata(&path).unwrap().permissions();
        permissions.set_mode(0o644);
        std::fs::set_permissions(&path, permissions).unwrap();

        assert!(!is_executable_file(&path));
    }

    #[test]
    fn claude_auth_status_summary_parses_json() {
        let summary = summarize_auth_output(
            "claude",
            r#"{"loggedIn":false,"authMethod":"none","apiProvider":"firstParty"}"#,
            false,
        );

        assert_eq!(summary, "not logged in; run nv login claude");
    }

    #[test]
    fn slash_command_suggestions_filter_by_prefix() {
        let suggestions = command_suggestions("/do");

        assert_eq!(suggestions.len(), 1);
        assert_eq!(suggestions[0].command, "/doctor");
    }

    #[test]
    fn slash_command_suggestions_ignore_task_text_and_arguments() {
        assert!(command_suggestions("fix /doctor output").is_empty());
        assert!(command_suggestions("/mode dry-run").is_empty());
    }

    #[test]
    fn slash_command_suggestions_fall_back_to_description_search() {
        let suggestions = command_suggestions("/provider");

        assert!(suggestions.iter().any(|spec| spec.command == "/adapter"));
    }

    #[test]
    fn interactive_state_labels_accept_with_nits_verdict() {
        let dir = tempfile::tempdir().unwrap();
        let task = Task::new("review accepted with nits", dir.path());
        let report = RunReport {
            task,
            selection: nerve_config::ProfileSelection {
                id: None,
                lead: "lead".to_string(),
                reviewer: "reviewer".to_string(),
                review_strictness: nerve_config::ReviewStrictness::Normal,
                max_refinement_rounds: 1,
                plan_strategy: PlanStrategy::Single,
                plan_system_prompt_override: None,
            },
            rounds: Vec::new(),
            crossfire_feedback: Vec::new(),
            final_output: nerve_types::AgentOutput::text("lead", "done"),
            final_feedback: nerve_types::ReviewerFeedback::accept_with_nits(
                "reviewer",
                Vec::new(),
                "Accepted with minor notes",
            ),
            final_patch: None,
            events: Vec::new(),
            usage: Default::default(),
            budget_exceeded: false,
            no_progress_exceeded: false,
            crossfire_halted: false,
            goal_satisfied: None,
            applied: false,
            blocked: false,
        };
        let mut state = InteractiveState::new(false, true, None);
        state.last_report = Some(report);

        assert_eq!(state.last_verdict_label(), "nits");
    }

    #[test]
    fn budget_parse_rejects_negative() {
        let err = parse_budget_args(&["cost=$-5"]).unwrap_err();
        assert!(matches!(err, BudgetParseError::InvalidValue(_)));
        let err = parse_budget_args(&["tokens=-100"]).unwrap_err();
        assert!(matches!(err, BudgetParseError::InvalidValue(_)));
    }

    #[test]
    fn budget_parse_rejects_unit_missing() {
        let err = parse_budget_args(&["5"]).unwrap_err();
        assert!(matches!(err, BudgetParseError::UnitMissing));
        let err = parse_budget_args(&["foo=10"]).unwrap_err();
        assert!(matches!(err, BudgetParseError::UnitMissing));
    }

    #[test]
    fn budget_parse_rejects_zero_and_empty() {
        let err = parse_budget_args(&["cost=$0"]).unwrap_err();
        assert!(matches!(err, BudgetParseError::InvalidValue(_)));
        let err = parse_budget_args(&["tokens=0"]).unwrap_err();
        assert!(matches!(err, BudgetParseError::InvalidValue(_)));
        let err = parse_budget_args(&["cost="]).unwrap_err();
        assert!(matches!(err, BudgetParseError::EmptyValue));
        let err = parse_budget_args(&[]).unwrap_err();
        assert!(matches!(err, BudgetParseError::Empty));
    }

    #[test]
    fn budget_decimal_to_microusd() {
        assert_eq!(parse_cost_value("$5.00").unwrap(), 5_000_000);
        assert_eq!(parse_cost_value("$0.01").unwrap(), 10_000);
        assert_eq!(parse_cost_value("$1.234567").unwrap(), 1_234_567);
        assert_eq!(parse_cost_value("$2").unwrap(), 2_000_000);
    }

    #[test]
    fn budget_show_action_parses() {
        let action = parse_budget_args(&["show"]).unwrap();
        assert_eq!(action, BudgetAction::Show);
    }

    #[test]
    fn budget_set_tokens_and_cost() {
        let action = parse_budget_args(&["cost=$1.00", "tokens=50000"]).unwrap();
        assert_eq!(
            action,
            BudgetAction::Set {
                cost_microusd: Some(1_000_000),
                tokens: Some(50_000),
                force: false,
            }
        );
        let action = parse_budget_args(&["cost=$10", "--force"]).unwrap();
        assert!(matches!(
            action,
            BudgetAction::Set {
                force: true,
                cost_microusd: Some(10_000_000),
                ..
            }
        ));
    }

    #[test]
    fn budget_force_skips_raise_confirmation_requirement() {
        assert!(requires_budget_raise_confirmation(Some(100), 200, false));
        assert!(!requires_budget_raise_confirmation(Some(100), 200, true));
        assert!(!requires_budget_raise_confirmation(Some(200), 100, false));
        assert!(requires_budget_raise_confirmation(None, 100, false));
    }

    #[test]
    fn goal_argv_parse_strips_flags() {
        let action = parse_goal_argv(&["--timeout", "30", "cargo", "test"]).unwrap();
        match action {
            GoalAction::RegisterArgv { argv, timeout_secs } => {
                assert_eq!(argv, vec!["cargo".to_string(), "test".to_string()]);
                assert_eq!(timeout_secs, Some(30));
            }
            other => panic!("expected register, got {other:?}"),
        }
    }

    #[test]
    fn goal_argv_recognizes_show_and_clear() {
        assert_eq!(parse_goal_argv(&["show"]).unwrap(), GoalAction::Show);
        assert_eq!(parse_goal_argv(&["clear"]).unwrap(), GoalAction::Clear);
    }

    #[test]
    fn parse_goal_argv_still_works() {
        let tokens = ["exit", "0"];
        let raw = "exit 0";
        let action = parse_goal_argv_with_raw(&tokens, raw).unwrap();
        match action {
            GoalAction::RegisterArgv { argv, timeout_secs } => {
                assert_eq!(argv, vec!["exit".to_string(), "0".to_string()]);
                assert!(timeout_secs.is_none());
            }
            other => panic!("expected argv register, got {other:?}"),
        }
    }

    #[test]
    fn parse_goal_nl_form_detected_quoted() {
        let raw = "\"tests pass && diff applied\"";
        // The whitespace tokenization would split the inner sentence, but the
        // raw remainder triggers the natural-language short-circuit first.
        let tokens: Vec<&str> = raw.split_whitespace().collect();
        let action = parse_goal_argv_with_raw(&tokens, raw).unwrap();
        assert_eq!(
            action,
            GoalAction::RegisterNaturalLanguage {
                free_form: "tests pass && diff applied".to_string()
            }
        );
    }

    #[test]
    fn parse_goal_nl_prefix_detected() {
        let raw = ":nl tests pass && diff applied";
        let tokens: Vec<&str> = raw.split_whitespace().collect();
        let action = parse_goal_argv_with_raw(&tokens, raw).unwrap();
        assert_eq!(
            action,
            GoalAction::RegisterNaturalLanguage {
                free_form: "tests pass && diff applied".to_string()
            }
        );
    }

    #[test]
    fn parse_goal_nl_empty_quoted_rejected() {
        let raw = "\"\"";
        let tokens: Vec<&str> = raw.split_whitespace().collect();
        let err = parse_goal_argv_with_raw(&tokens, raw).unwrap_err();
        assert_eq!(err, GoalParseError::EmptyNaturalLanguage);
    }

    #[test]
    fn parse_goal_nl_empty_prefix_rejected() {
        let raw = ":nl   ";
        let tokens: Vec<&str> = raw.split_whitespace().collect();
        let err = parse_goal_argv_with_raw(&tokens, raw).unwrap_err();
        assert_eq!(err, GoalParseError::EmptyNaturalLanguage);
    }

    #[test]
    fn goal_argv_rejects_empty() {
        let err = parse_goal_argv(&[]).unwrap_err();
        assert!(matches!(err, GoalParseError::Empty));
    }

    #[test]
    fn active_goal_loads_from_session_meta() {
        let dir = tempfile::tempdir().unwrap();
        let spec = GoalSpec {
            id: "goal-test".to_string(),
            check_cmd: vec!["cargo".to_string(), "test".to_string()],
            timeout_secs: 30,
            cwd: Some(dir.path().to_path_buf()),
            env: BTreeMap::new(),
            no_progress_max: Some(2),
        };
        write_json_atomic(&active_goal_path(dir.path()), &spec).unwrap();

        let loaded = load_active_goal(dir.path()).unwrap().unwrap();

        assert_eq!(loaded, spec);
    }

    #[test]
    fn cost_format_two_decimals_and_four_fraction() {
        assert_eq!(format_cost_microusd(0), "$0.0000");
        assert_eq!(format_cost_microusd(123_456), "$0.1234");
        assert_eq!(format_cost_microusd(5_000_000), "$5.0000");
    }

    #[test]
    fn doctor_renders_chain_warning() {
        let broken = DoctorCheck {
            name: "budget_audit_chain".to_string(),
            status: DoctorStatus::Fail("chain broken at entry 3".to_string()),
        };
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        render_doctor_check(&broken, &mut stdout, &mut stderr);
        let stdout_text = String::from_utf8(stdout).unwrap();
        let stderr_text = String::from_utf8(stderr).unwrap();
        assert!(
            stdout_text.is_empty(),
            "fail status must not pollute stdout"
        );
        assert!(
            stderr_text.contains("budget_audit_chain: fail"),
            "stderr must surface the failing check (got `{stderr_text}`)"
        );
        assert!(
            stderr_text.contains("chain broken at entry 3"),
            "stderr must include the failure message"
        );
    }

    #[test]
    fn doctor_renders_ok_to_stdout() {
        let ok = DoctorCheck {
            name: "git".to_string(),
            status: DoctorStatus::Ok,
        };
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        render_doctor_check(&ok, &mut stdout, &mut stderr);
        assert_eq!(String::from_utf8(stdout).unwrap(), "git: ok\n");
        assert!(String::from_utf8(stderr).unwrap().is_empty());
    }

    #[test]
    fn goal_history_appends_jsonl_line() {
        use chrono::Utc;
        let dir = tempfile::tempdir().unwrap();
        let intent = GoalIntent {
            free_form: "tests pass".into(),
            proposed_spec: GoalSpec {
                id: "intent-1".into(),
                check_cmd: vec!["cargo".into(), "test".into()],
                timeout_secs: 60,
                cwd: Some(dir.path().to_path_buf()),
                env: BTreeMap::new(),
                no_progress_max: None,
            },
            rationale: "user wants cargo test gate".into(),
            source_adapter: "mock".into(),
            created_at: Utc::now(),
        };
        append_goal_history_entry(dir.path(), &intent).unwrap();
        append_goal_history_entry(dir.path(), &intent).unwrap();
        let raw = std::fs::read_to_string(goal_history_path(dir.path())).unwrap();
        let lines: Vec<&str> = raw.lines().collect();
        assert_eq!(lines.len(), 2);
        for line in lines {
            let back: GoalIntent = serde_json::from_str(line).unwrap();
            assert_eq!(back, intent);
        }
    }

    #[test]
    fn cli_worktree_override_priority() {
        assert_eq!(cli_worktree_override(false, false), None);
        assert_eq!(cli_worktree_override(true, false), Some(true));
        assert_eq!(cli_worktree_override(false, true), Some(false));
        // --worktree wins over --no-worktree when both are set.
        assert_eq!(cli_worktree_override(true, true), Some(true));
    }

    #[test]
    fn config_check_ulimit_maps_to_core_runtime_spec() {
        let spec = nerve_config::CheckUlimit {
            nproc: Some(32),
            memory_bytes: Some(2_147_483_648),
            file_size_bytes: Some(104_857_600),
            cpu_secs: Some(60),
        };

        let mapped = core_ulimit_from_config(&spec);

        assert_eq!(mapped.nproc, Some(32));
        assert_eq!(mapped.address_space_bytes, Some(2_147_483_648));
        assert_eq!(mapped.file_size_bytes, Some(104_857_600));
        assert_eq!(mapped.cpu_secs, Some(60));
    }

    #[test]
    fn template_query_matches_substring_case_insensitive() {
        let template = nerve_config::PromptTemplate {
            id: "security-audit".to_string(),
            prompt: "Run a security review".to_string(),
            description: Some("Audit dependencies for CVEs".to_string()),
        };
        assert!(matches_template_query(&template, "security"));
        assert!(matches_template_query(&template, "CVE"));
        assert!(matches_template_query(&template, ""));
        assert!(!matches_template_query(&template, "performance"));
    }

    /// `nv plan --dual-review "<task>"` must parse into the typed
    /// `PlanArgs` (clap derive) with `dual_review = true` and the task
    /// preserved verbatim.
    #[test]
    fn parse_plan_subcommand_dual_review() {
        use clap::Parser;
        let cli = Cli::try_parse_from([
            "nv",
            "plan",
            "--dual-review",
            "audit crates/nerve-cli for /plan",
        ])
        .expect("clap must accept `nv plan --dual-review <task>`");
        match cli.command {
            Some(Command::Plan(args)) => {
                assert!(args.dual_review, "--dual-review must set dual_review=true");
                assert_eq!(args.task, "audit crates/nerve-cli for /plan");
                assert!(args.cwd.is_none());
            }
            other => panic!("expected Plan subcommand, got {other:?}"),
        }

        // Sanity: omitting --dual-review leaves the field false.
        let cli = Cli::try_parse_from(["nv", "plan", "single mode task"]).expect("plan w/o flag");
        match cli.command {
            Some(Command::Plan(args)) => {
                assert!(!args.dual_review);
                assert_eq!(args.task, "single mode task");
            }
            other => panic!("expected Plan, got {other:?}"),
        }
    }

    /// When stdin or stdout is not a TTY, `decide_tui` must refuse to
    /// engage the ratatui surface even if `--tui` was requested. The cargo
    /// test harness always runs without a TTY, so this is enforced by the
    /// runtime check inside `decide_tui` itself.
    #[test]
    fn tui_skips_when_non_tty() {
        let config = Config::from_json_str(
            r#"{
              "orchestration": {
                "default_strategy": "consensus",
                "max_refinement_rounds": 2,
                "conflict_policy": "lead_priority"
              },
              "roles": {
                "architect": "claude-code",
                "reviewer": "codex"
              },
              "tui": {
                "enabled": true,
                "auto_in_cmux": true,
                "refresh_ms": 100,
                "log_height_pct": 60
              }
            }"#,
        )
        .expect("tui config must parse");

        // Force-on path: --tui requested but no TTY → suppressed with NotATty.
        let decision = decide_tui(true, &config);
        assert!(
            !decision.use_tui,
            "non-tty must never engage the TUI (got {decision:?})"
        );
        assert_eq!(decision.suppressed_reason, Some(TuiSuppression::NotATty));

        // Auto path: --tui false, no CMUX_SESSION → suppressed (NotATty
        // still wins because the TTY check runs first).
        // SAFETY: tests are serialized in this binary; no parallel
        // `decide_tui` invocations observe the mutated env.
        unsafe { std::env::remove_var("CMUX_SESSION") };
        let decision = decide_tui(false, &config);
        assert!(!decision.use_tui);
        assert!(decision.suppressed_reason.is_some());

        // Disabled path: tui.enabled = false and no --tui override.
        let mut disabled = config.clone();
        disabled.tui.enabled = false;
        let decision = decide_tui(false, &disabled);
        assert!(!decision.use_tui);
        assert_eq!(decision.suppressed_reason, Some(TuiSuppression::Disabled));
    }

    /// `nv rpc rotate-token` rebuilds an `RpcBus` against the workspace
    /// session-meta dir and persists a fresh token. We exercise the
    /// underlying rotation helper directly so the test does not depend on
    /// process-level current_dir mutation.
    #[test]
    fn rpc_rotate_token_persists() {
        let dir = tempfile::tempdir().expect("tempdir for rpc rotate test");
        let session_meta = dir.path().join(".nerve").join("session-meta");
        std::fs::create_dir_all(&session_meta).expect("create session-meta dir");

        // Pin the token under the tempdir so the test never touches the
        // user's real workspace.
        let rpc_config = RpcConfig {
            token_path: session_meta.join("rpc-token"),
            ..Default::default()
        };

        let bus = RpcBus::new(rpc_config, dir.path()).expect("rpc bus init");
        let before = bus.bearer_token();
        let after = bus.rotate_token(dir.path()).expect("rotate token");

        assert_ne!(before, after, "rotated token must differ from previous");
        let on_disk =
            std::fs::read_to_string(session_meta.join("rpc-token")).expect("rotated token file");
        assert_eq!(on_disk.trim(), after);
        assert_eq!(after.len(), 64, "32-byte token encoded as 64 hex chars");

        // Validate the 0600 doctor check sees the rotated file as ok.
        #[cfg(unix)]
        {
            let status = rpc_token_permission_status(&session_meta.join("rpc-token"));
            assert!(matches!(status, DoctorStatus::Ok));
        }
    }

    /// `nv fork`, `nv branch`, `nv mcp`, `nv mayor`, `nv patrol`, and the
    /// `nv sessions` family must all be parsed by clap without colliding with
    /// existing subcommands. We rely on clap's derive macro to enforce the
    /// canonical names; a regression here would manifest as a clap error.
    #[test]
    fn v1_subcommands_parse_help() {
        use clap::Parser;
        for argv in [
            vec!["nv", "fork", "--help"],
            vec!["nv", "branch", "--help"],
            vec!["nv", "sessions", "--help"],
            vec!["nv", "sessions", "list", "--help"],
            vec!["nv", "sessions", "tree", "--help"],
            vec!["nv", "mcp", "--help"],
            vec!["nv", "mcp", "list-tools", "--help"],
            vec!["nv", "mcp", "probe", "--help"],
            vec!["nv", "mayor", "--help"],
            vec!["nv", "patrol", "--help"],
        ] {
            let err = Cli::try_parse_from(argv.clone()).unwrap_err();
            assert!(
                matches!(
                    err.kind(),
                    clap::error::ErrorKind::DisplayHelp | clap::error::ErrorKind::DisplayVersion
                ),
                "expected --help on `{argv:?}` to surface help, got {err:?}"
            );
        }
    }

    #[test]
    fn parse_fork_subcommand_with_round_and_name() {
        use clap::Parser;
        let cli = Cli::try_parse_from([
            "nv",
            "fork",
            "01PARENT",
            "--from-round",
            "3",
            "--name",
            "hot-fix",
        ])
        .expect("clap must accept `nv fork ...`");
        match cli.command {
            Some(Command::Fork(args)) => {
                assert_eq!(args.session_id, "01PARENT");
                assert_eq!(args.from_round, Some(3));
                assert_eq!(args.name.as_deref(), Some("hot-fix"));
            }
            other => panic!("expected Fork subcommand, got {other:?}"),
        }
    }

    #[test]
    fn parse_branch_alias() {
        use clap::Parser;
        let cli =
            Cli::try_parse_from(["nv", "branch", "01TASK"]).expect("clap must accept `nv branch`");
        match cli.command {
            Some(Command::Branch { task_id }) => assert_eq!(task_id, "01TASK"),
            other => panic!("expected Branch subcommand, got {other:?}"),
        }
    }

    #[test]
    fn parse_mayor_subcommand_flags() {
        use clap::Parser;
        let cli = Cli::try_parse_from([
            "nv",
            "mayor",
            "--max-patrols",
            "4",
            "--per-patrol-budget-microusd",
            "100000",
            "--status-only",
        ])
        .expect("clap must accept `nv mayor ...`");
        match cli.command {
            Some(Command::Mayor(args)) => {
                assert_eq!(args.max_patrols, Some(4));
                assert_eq!(args.per_patrol_budget_microusd, Some(100_000));
                assert!(args.status_only);
            }
            other => panic!("expected Mayor subcommand, got {other:?}"),
        }
    }

    #[test]
    fn parse_patrol_subcommand_flags() {
        use clap::Parser;
        let cli = Cli::try_parse_from([
            "nv",
            "patrol",
            "--id",
            "slot-1",
            "--mcp-server",
            "fs",
            "--mcp-server",
            "shell",
            "--once",
        ])
        .expect("clap must accept `nv patrol ...`");
        match cli.command {
            Some(Command::Patrol(args)) => {
                assert_eq!(args.id, "slot-1");
                assert_eq!(args.mcp_server, vec!["fs".to_string(), "shell".to_string()]);
                assert!(args.once);
            }
            other => panic!("expected Patrol subcommand, got {other:?}"),
        }
    }

    /// Doctor must surface a `Fail` entry when `sessions/index.json` is
    /// present but invalid JSON. Operators rely on this to catch corrupted
    /// fork indexes before they cause silent fork misses.
    #[test]
    fn sessions_index_doctor_detects_malformed_json() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".nerve").join("sessions");
        std::fs::create_dir_all(&path).unwrap();
        std::fs::write(path.join("index.json"), b"not-json").unwrap();
        let status = sessions_index_status(dir.path());
        assert!(matches!(status, DoctorStatus::Fail(_)));
    }

    /// Doctor's `sessions_index_status` must accept an empty workspace
    /// (default `Ok`) so existing users see no new warning on v1.0 upgrade.
    #[test]
    fn sessions_index_doctor_accepts_missing_index() {
        let dir = tempfile::tempdir().unwrap();
        let status = sessions_index_status(dir.path());
        assert!(matches!(status, DoctorStatus::Ok));
    }

    /// `patrol_rpc_token_path` must namespace tokens per patrol id so two
    /// patrols on the same host cannot read each other's bearer secrets.
    #[test]
    fn patrol_rpc_token_path_is_isolated_per_id() {
        let dir = tempfile::tempdir().unwrap();
        let meta = dir.path().join(".nerve").join("session-meta");
        let a = patrol_rpc_token_path(&meta, "slot-1");
        let b = patrol_rpc_token_path(&meta, "slot-2");
        assert_ne!(a, b);
        assert!(a.file_name().unwrap().to_string_lossy().contains("slot-1"));
        assert!(b.file_name().unwrap().to_string_lossy().contains("slot-2"));
    }

    #[test]
    fn patrol_rpc_config_resolves_token_under_workspace_session_meta() {
        let dir = tempfile::tempdir().unwrap();
        let config = Config::from_json_str(
            r#"{
              "orchestration": {
                "default_strategy": "consensus",
                "max_refinement_rounds": 2,
                "conflict_policy": "lead_priority"
              },
              "roles": { "architect": "claude-code", "reviewer": "codex" },
              "profiles": [],
              "templates": [],
              "ui": { "default_mode": "print" },
              "daemon": { "protocol": "line" }
            }"#,
        )
        .unwrap();
        let rpc_config = patrol_rpc_config(&config, "slot-7");
        let expected = dir
            .path()
            .join(".nerve")
            .join("session-meta")
            .join("rpc-token-slot-7");

        let bus = RpcBus::new(rpc_config, dir.path()).expect("patrol rpc bus init");

        assert!(expected.exists());
        assert_eq!(bus.bearer_token().len(), 64);
    }

    #[tokio::test]
    async fn fork_bootstrap_preserves_legacy_report_rounds() {
        let dir = tempfile::tempdir().unwrap();
        let task = Task::new("legacy session", dir.path());
        let round = nerve_types::RoundRecord {
            round: 0,
            lead: nerve_types::AgentOutput::text("lead", "round 0"),
            reviewer: nerve_types::ReviewerFeedback::lgtm("reviewer", "LGTM"),
            check_result: None,
            patch_sha: Some("sha0".to_string()),
            envelope_id: None,
        };
        let report = RunReport {
            task: task.clone(),
            selection: nerve_config::ProfileSelection {
                id: None,
                lead: "lead".to_string(),
                reviewer: "reviewer".to_string(),
                review_strictness: nerve_config::ReviewStrictness::Normal,
                max_refinement_rounds: 1,
                plan_strategy: PlanStrategy::Single,
                plan_system_prompt_override: None,
            },
            rounds: vec![round],
            crossfire_feedback: Vec::new(),
            final_output: nerve_types::AgentOutput::text("lead", "done"),
            final_feedback: nerve_types::ReviewerFeedback::lgtm("reviewer", "LGTM"),
            final_patch: None,
            events: Vec::new(),
            usage: Default::default(),
            budget_exceeded: false,
            no_progress_exceeded: false,
            crossfire_halted: false,
            goal_satisfied: None,
            applied: false,
            blocked: false,
        };
        NerveStore::new(dir.path()).save_report(&report).unwrap();

        let forker = SessionForker::new(CoreForkConfig::default(), dir.path());
        bootstrap_root_session_if_missing(&forker, dir.path(), &task.id)
            .await
            .unwrap();
        let parent = forker.get(&task.id).await.unwrap().unwrap();
        assert_eq!(parent.rounds.len(), 1);

        let child = forker
            .fork(&task.id, CoreForkOptions::default())
            .await
            .unwrap();
        assert_eq!(child.branched_at_round, Some(0));
        assert_eq!(child.branched_from_patch_sha.as_deref(), Some("sha0"));
        assert_eq!(child.rounds.len(), 1);
    }

    #[test]
    fn envelope_version_status_accepts_runtime_major() {
        // Runtime matches itself.
        assert!(matches!(
            envelope_version_status(RPC_SCHEMA_VERSION),
            DoctorStatus::Ok
        ));
        // Same major (1.x.y) is accepted.
        assert!(matches!(envelope_version_status("1.2.3"), DoctorStatus::Ok));
        // Different major is rejected.
        assert!(matches!(
            envelope_version_status("2.0.0"),
            DoctorStatus::Fail(_)
        ));
        // Non-semver is rejected.
        assert!(matches!(
            envelope_version_status("not-a-version"),
            DoctorStatus::Fail(_)
        ));
    }
}
