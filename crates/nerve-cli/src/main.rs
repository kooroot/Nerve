use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use nerve_adapter::{ModelAdapter, SubprocessAdapter, default_adapters_with_limits};
use nerve_config::{Config, DaemonProtocol, GoalIntent, GoalSpec};
use nerve_core::store::NerveStore;
use nerve_core::{
    AuditChainState, BudgetAuditEntry, BudgetSnapshot, ChainStatus, DoctorCheck, DoctorStatus,
    GoalIntentConverter, RunOptions, RunReport, append_budget_audit_entry, doctor_checks,
    format_chain_broken, run_synaptic_loop,
};
use nerve_types::{AgentEvent, Task, Verdict};
use serde::Serialize;
use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::io::{IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command as StdCommand;
use std::sync::Arc;
use std::time::Instant;
use tokio::io::{self, AsyncBufReadExt};

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
    },
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
}

#[derive(Debug, Subcommand)]
enum ConfigCommand {
    Validate,
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
        Some(Command::Daemon { once, rpc }) => {
            let config_prefers_rpc = Config::load()
                .map(|config| matches!(config.daemon.protocol, DaemonProtocol::Rpc))
                .unwrap_or(false);
            if rpc || config_prefers_rpc {
                run_rpc_daemon(cli.apply, matches!(cli.adapter, AdapterMode::Mock), once).await
            } else {
                run_daemon(cli.apply, matches!(cli.adapter, AdapterMode::Mock), once).await
            }
        }
        Some(Command::Setup) => run_setup(matches!(cli.adapter, AdapterMode::Mock)),
        Some(Command::Login { provider }) => run_login(provider),
        Some(Command::Interactive) => {
            run_interactive(
                cli.apply,
                matches!(cli.adapter, AdapterMode::Mock),
                cli_worktree_override(cli.worktree, cli.no_worktree),
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
        None => {
            let Some(prompt) = cli.prompt else {
                if std::io::stdin().is_terminal() {
                    return run_interactive(
                        cli.apply,
                        matches!(cli.adapter, AdapterMode::Mock),
                        cli_worktree_override(cli.worktree, cli.no_worktree),
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
        let loop_ok = report.final_feedback.verdict == Verdict::Lgtm
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

async fn run_interactive(apply: bool, mock: bool, worktree_override: Option<bool>) -> Result<()> {
    let mut state = InteractiveState::new(apply, mock, worktree_override);
    state.refresh_counts();
    let cwd = env::current_dir().context("failed to read current directory")?;
    refresh_active_goal(&mut state, &cwd);
    warn_if_audit_chain_broken(&cwd);
    print_interactive_banner(&state);
    if std::io::stdin().is_terminal() && std::io::stdout().is_terminal() {
        run_interactive_terminal(&mut state).await
    } else {
        run_interactive_lines(&mut state).await
    }
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
        command: "/budget",
        args: "<show | cost=$X | tokens=N>",
        description: "inspect or override session budget caps",
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
        let suggestions = self.command_suggestions();
        for (index, suggestion) in suggestions.iter().enumerate() {
            let marker = if index == self.selected_suggestion {
                ">"
            } else {
                " "
            };
            println!(
                "\n  {marker} {:<12} {:<16} {}",
                suggestion.command, suggestion.args, suggestion.description
            );
        }
        if !suggestions.is_empty() {
            print!("\x1b[{}A\r{prompt}{}", suggestions.len(), self.buffer);
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
    INTERACTIVE_COMMANDS
        .iter()
        .filter(|spec| spec.command.starts_with(&query))
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

fn print_interactive_banner(state: &InteractiveState) {
    println!("┌─ Nerve Terminal ─────────────────────────────────────────────┐");
    println!("│ Lead/reviewer coding loop with reviewed, auditable patches.  │");
    println!("│ Type a task, /paste multiline input, or !cmd for shell.      │");
    println!("│ Then inspect with /diff, /apply, /rollback, or /history.     │");
    println!("└──────────────────────────────────────────────────────────────┘");
    print_interactive_status(state);
}

fn print_interactive_status(state: &InteractiveState) {
    let branch = git_branch_label(env::current_dir().ok().as_deref()).unwrap_or_else(|| "-".into());
    println!(
        "adapter={} mode={} branch={} sessions={} patches={} cwd={}",
        state.adapter_label(),
        state.apply_label(),
        branch,
        state.session_count,
        state.patch_count,
        env::current_dir()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|_| "?".to_string())
    );
    print_status_bar(state);
}

fn print_status_bar(state: &InteractiveState) {
    // Tier 1a status bar: render on a single line. We save+hide cursor before
    // updating to avoid raw-mode flicker, then restore. The interactive caller
    // is responsible for not being in the middle of a paste sequence.
    let bar = render_status_bar(state);
    if std::io::stdout().is_terminal() {
        print!("\x1b[s\x1b[?25l");
        println!("{bar}");
        print!("\x1b[u\x1b[?25h");
    } else {
        println!("{bar}");
    }
    let _ = std::io::stdout().flush();
}

fn render_status_bar(state: &InteractiveState) -> String {
    let round_label = if state.last_max_rounds == 0 {
        "round -/-".to_string()
    } else {
        format!("round {}/{}", state.last_round_count, state.last_max_rounds)
    };
    let verdict_label = format!("verdict={}", state.last_verdict_label());
    let total_tokens = state
        .cumulative_input_tokens
        .saturating_add(state.cumulative_output_tokens);
    let cost_label = format_cost_microusd(state.cumulative_cost_microusd);
    let goal_label = match state.active_goal.as_ref() {
        Some(spec) => format!("goal={}", spec.id),
        None => "goal=-".to_string(),
    };
    format!(
        "[status] {} | {} | cost={} | tokens={} | {} | no-progress={}",
        round_label, verdict_label, cost_label, total_tokens, goal_label, state.no_progress_counter
    )
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
    format!(
        "nerve:{}:{}{}{}> ",
        state.adapter_label(),
        state.apply_label(),
        branch_hint,
        patch_hint
    )
}

fn print_interactive_error(error: &anyhow::Error) {
    eprintln!("Error: {error:#}");
    eprintln!(
        "Hint: run /login to authenticate providers, /doctor to inspect setup, or start with NERVE_ADAPTER=mock nv for a local smoke test."
    );
}

async fn run_interactive_task(prompt: String, state: &mut InteractiveState) -> Result<()> {
    println!("▶ task: {prompt}");
    println!("  lead/reviewer loop running...");
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
    print_interactive_result(&report, state.apply);
    state.record_report(&report);
    state.last_report = Some(report);
    state.refresh_counts();
    print_status_bar(state);
    Ok(())
}

fn print_interactive_result(report: &RunReport, apply_requested: bool) {
    println!(
        "✓ session {} | verdict={:?} | rounds={} | applied={} | blocked={}",
        short_id(&report.task.id),
        report.final_feedback.verdict,
        report.rounds.len(),
        report.applied,
        report.blocked
    );
    if let Some(patch) = &report.final_patch {
        println!("  patch {} | files={}", patch.id, patch.files.len());
        if report.applied {
            println!("  applied. Use /rollback to undo the last patch.");
        } else if !apply_requested && !report.blocked {
            println!("  reviewed patch ready. Use /diff to inspect or /apply to apply it.");
        }
    } else {
        println!(
            "  no structured patch produced. Use /resume {} for raw output.",
            report.task.id
        );
    }
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
            if let Some(report) = &state.last_report {
                println!(
                    "last session={} verdict={:?} patch={}",
                    report.task.id,
                    report.final_feedback.verdict,
                    report
                        .final_patch
                        .as_ref()
                        .map(|patch| patch.id.as_str())
                        .unwrap_or("-")
                );
            }
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
        "quit" | "exit" | "q" => return Ok(true),
        other => println!("Unknown command /{other}. Type /help for commands."),
    }
    Ok(false)
}

fn print_interactive_help() {
    println!("Tasks:");
    println!("  type a coding request to run the lead/reviewer loop");
    println!("  /paste                 enter a multiline task; finish with /end");
    println!("  !<command>             run a shell command in the current workspace");
    println!("Workflow:");
    println!("  /diff                  show the last reviewed patch");
    println!("  /apply [patch-id]      apply the last or selected patch");
    println!("  /rollback [patch-id]   roll back the last or selected patch");
    println!("  /history               show recent sessions");
    println!("  /resume <id>           print a stored session report");
    println!("  /list                  list stored patches");
    println!("Workspace:");
    println!("  /status                show adapter, mode, branch, counts, cwd");
    println!("  /mode <dry-run|apply>  switch apply behavior without restarting");
    println!("  /adapter <real|mock>   switch providers without restarting");
    println!("  /cd <path>             change workspace directory");
    println!("  /pwd                   print workspace directory");
    println!("  /clear                 redraw the terminal workspace");
    println!("Setup:");
    println!("  /login                 start provider login flows");
    println!("  /doctor                inspect config, adapters, and auth");
    println!("  /templates [query]     list or search prompt templates");
    println!("  /template <id> [args]  run a prompt template");
    println!("  /benchmark pi [n]      run the Pi workflow benchmark");
    println!("Loop controls:");
    println!("  /goal <argv...>        register a deterministic stop check_cmd");
    println!("  /goal :nl <prose>      LLM-convert natural language into a check_cmd");
    println!("  /goal \"<prose>\"        quoted form of :nl (must be the entire argument)");
    println!("  /goal show | clear     inspect or remove the active goal");
    println!("  /budget show           show current cap, cumulative, and remaining");
    println!("  /budget cost=$X        cap session cost (microusd-aware)");
    println!("  /budget tokens=N       cap session total tokens");
    println!("  /quit                  exit");
    println!("Tips:");
    println!("  type / to open the command palette");
    println!("  use Up/Down for history or command selection");
    println!("  use Tab or Right to complete a selected command");
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

async fn run_daemon(apply: bool, mock: bool, once: bool) -> Result<()> {
    let _echo_guard = disable_stdin_echo_if_terminal()?;
    let stdin = io::BufReader::new(io::stdin());
    let mut lines = stdin.lines();

    while let Some(line) = lines.next_line().await? {
        let prompt = line.trim();
        if prompt.is_empty() {
            continue;
        }

        let report = run_report(prompt.to_string(), apply, mock, None).await?;
        println!("{}", serde_json::to_string(&report)?);

        if once {
            break;
        }
    }

    Ok(())
}

async fn run_rpc_daemon(apply: bool, mock: bool, once: bool) -> Result<()> {
    let _echo_guard = disable_stdin_echo_if_terminal()?;
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

        if let Err(error) = handle_rpc_command(value, apply, mock).await {
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

    Ok(())
}

async fn handle_rpc_command(value: serde_json::Value, apply: bool, mock: bool) -> Result<()> {
    let command = value
        .get("command")
        .and_then(serde_json::Value::as_str)
        .context("missing string field `command`")?;
    match command {
        "prompt" => {
            let prompt = value
                .get("prompt")
                .and_then(serde_json::Value::as_str)
                .context("missing string field `prompt`")?;
            let report = run_report(prompt.to_string(), apply, mock, None).await?;
            emit_report_events(&report)?;
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
    }
    if let Some(on) = worktree_override {
        options = options.with_worktree(on);
    }
    let report = run_synaptic_loop(task, &config, &adapters, options).await?;
    NerveStore::new(&cwd).save_report(&report)?;
    Ok(report)
}

fn adapters_for_config(mock: bool, config: &Config) -> Vec<Box<dyn ModelAdapter>> {
    default_adapters_with_limits(
        mock,
        config.orchestration.adapter_timeout_secs,
        config.orchestration.adapter_max_output_bytes,
    )
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

    if report.blocked && !report.budget_exceeded {
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

fn print_tui_report(report: &RunReport) {
    println!("Nerve TUI {}", report.task.id);
    println!("{}", "=".repeat(72));
    println!("[ Lead ]");
    println!("agent: {}", report.final_output.agent_id);
    println!("{}", truncate_panel_text(&report.final_output.raw_text));
    if let Some(patch) = &report.final_patch {
        println!("patch: {} file(s), id={}", patch.files.len(), patch.id);
    } else {
        println!("patch: none");
    }
    println!("{}", "-".repeat(72));
    println!("[ Reviewer ]");
    println!(
        "agent: {} | verdict: {:?}",
        report.final_feedback.reviewer_id, report.final_feedback.verdict
    );
    println!("{}", truncate_panel_text(&report.final_feedback.raw_text));
    if !report.crossfire_feedback.is_empty() {
        println!("crossfire events: {}", report.crossfire_feedback.len());
    }
    println!("{}", "-".repeat(72));
    println!("[ Orchestrator ]");
    println!(
        "profile={} rounds={} applied={} blocked={} budget_exceeded={}",
        report.selection.id.as_deref().unwrap_or("default"),
        report.rounds.len(),
        report.applied,
        report.blocked,
        report.budget_exceeded
    );
    println!(
        "usage: input={} output={} total={} cost_microusd={}",
        report.usage.input_tokens,
        report.usage.output_tokens,
        report.usage.total_tokens(),
        report
            .usage
            .estimated_cost_microusd
            .map(|value| value.to_string())
            .unwrap_or_else(|| "-".to_string())
    );
    println!("{}", "=".repeat(72));
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

fn collect_context_paths(prompt: &str, cwd: &Path) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    for token in prompt.split_whitespace() {
        let token = token.trim_matches(|ch: char| {
            matches!(
                ch,
                '`' | '"' | '\'' | ',' | ':' | ';' | '(' | ')' | '[' | ']'
            )
        });
        if looks_like_path_token(token) {
            push_unique_path(&mut paths, PathBuf::from(token));
        }
    }

    for path in git_changed_paths(cwd) {
        push_unique_path(&mut paths, path);
    }

    paths
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
}
