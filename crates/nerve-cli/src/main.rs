use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use nerve_adapter::default_adapters;
use nerve_config::{Config, DaemonProtocol};
use nerve_core::store::NerveStore;
use nerve_core::{RunOptions, RunReport, run_synaptic_loop};
use nerve_types::{AgentEvent, Task, Verdict};
use serde::Serialize;
use std::env;
use std::io::{IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command as StdCommand;
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

    #[arg(long, env = "NERVE_ADAPTER", default_value = "real")]
    adapter: AdapterMode,
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
    #[command(about = "Check config and adapter prerequisites")]
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
        Some(Command::Doctor) => run_doctor(matches!(cli.adapter, AdapterMode::Mock)),
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
            run_interactive(cli.apply, matches!(cli.adapter, AdapterMode::Mock)).await
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
            let report =
                run_report(prompt, cli.apply, matches!(cli.adapter, AdapterMode::Mock)).await?;
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
            )
            .await
        }
        None => {
            let Some(prompt) = cli.prompt else {
                if std::io::stdin().is_terminal() {
                    return run_interactive(cli.apply, matches!(cli.adapter, AdapterMode::Mock))
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

    let adapters = default_adapters(adapter_mock);
    for iteration in 1..=iterations {
        let task = Task::new(
            format!("Pi benchmark iteration {iteration}: produce a reviewed patch artifact"),
            cwd,
        );
        let run_started = Instant::now();
        let report =
            run_synaptic_loop(task, &config, &adapters, RunOptions { apply: false }).await?;
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

async fn run_prompt(prompt: String, apply: bool, json: bool, tui: bool, mock: bool) -> Result<()> {
    let report = run_report(prompt, apply, mock).await?;

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
            run_prompt(prompt, apply, json, tui, mock).await
        }
    }
}

fn run_doctor(mock: bool) -> Result<()> {
    let cwd = env::current_dir().context("failed to read current directory")?;
    Config::load_from(&cwd)?;
    println!("config: ok");

    if mock {
        println!("adapter: mock ok");
        return Ok(());
    }

    let claude = find_on_path("claude");
    let codex = find_on_path("codex");
    print_doctor_check("claude", &claude);
    print_doctor_check("codex", &codex);
    print_auth_status("claude", &claude, &["auth", "status"]);
    print_auth_status("codex", &codex, &["login", "status"]);
    if claude.is_some() && codex.is_some() {
        Ok(())
    } else {
        anyhow::bail!("real adapter prerequisites are missing")
    }
}

fn run_setup(mock: bool) -> Result<()> {
    let cwd = env::current_dir().context("failed to read current directory")?;
    let store = NerveStore::new(&cwd);
    store.init()?;
    println!("store: {}", cwd.join(".nerve").display());
    run_doctor(mock)
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

async fn run_interactive(apply: bool, mock: bool) -> Result<()> {
    let mut state = InteractiveState::new(apply, mock);
    state.refresh_counts();
    print_interactive_banner(&state);
    if std::io::stdin().is_terminal() && std::io::stdout().is_terminal() {
        run_interactive_terminal(&mut state).await
    } else {
        run_interactive_lines(&mut state).await
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
    last_report: Option<RunReport>,
    session_count: usize,
    patch_count: usize,
}

impl InteractiveState {
    fn new(apply: bool, mock: bool) -> Self {
        Self {
            apply,
            mock,
            last_report: None,
            session_count: 0,
            patch_count: 0,
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
    let report = run_report(prompt, state.apply, state.mock).await?;
    print_interactive_result(&report, state.apply);
    state.last_report = Some(report);
    state.refresh_counts();
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
        "doctor" => run_doctor(state.mock)?,
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
                run_interactive_task(prompt, state).await?;
            } else {
                if config.templates.is_empty() {
                    println!("No prompt templates configured.");
                }
                for template in config.templates {
                    println!(
                        "{} | {}",
                        template.id,
                        template.description.as_deref().unwrap_or("-")
                    );
                }
            }
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
    println!("  /templates             list configured prompt templates");
    println!("  /template <id> [args]  run a prompt template");
    println!("  /benchmark pi [n]      run the Pi workflow benchmark");
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

async fn run_daemon(apply: bool, mock: bool, once: bool) -> Result<()> {
    let _echo_guard = disable_stdin_echo_if_terminal()?;
    let stdin = io::BufReader::new(io::stdin());
    let mut lines = stdin.lines();

    while let Some(line) = lines.next_line().await? {
        let prompt = line.trim();
        if prompt.is_empty() {
            continue;
        }

        let report = run_report(prompt.to_string(), apply, mock).await?;
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
            let report = run_report(prompt.to_string(), apply, mock).await?;
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

async fn run_report(prompt: String, apply: bool, mock: bool) -> Result<RunReport> {
    let cwd = env::current_dir().context("failed to read current directory")?;
    let config = Config::load_from(&cwd)?;
    let mut task = Task::new(prompt, &cwd);
    task.context_paths = collect_context_paths(&task.prompt, &cwd);
    let adapters = default_adapters(mock);
    let report = run_synaptic_loop(task, &config, &adapters, RunOptions { apply }).await?;
    NerveStore::new(&cwd).save_report(&report)?;
    Ok(report)
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

fn print_doctor_check(binary: &str, path: &Option<PathBuf>) {
    match path {
        Some(path) => println!("{binary}: {}", path.display()),
        None => println!("{binary}: missing"),
    }
}

fn print_auth_status(name: &str, path: &Option<PathBuf>, args: &[&str]) {
    let Some(path) = path else {
        return;
    };
    let Ok(output) = StdCommand::new(path).args(args).output() else {
        println!("{name} auth: unknown");
        return;
    };
    if output.status.success() {
        let text = String::from_utf8_lossy(&output.stdout);
        println!("{name} auth: {}", summarize_auth_output(name, &text, true));
    } else {
        let text = String::from_utf8_lossy(&output.stdout);
        println!("{name} auth: {}", summarize_auth_output(name, &text, false));
    }
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
}
