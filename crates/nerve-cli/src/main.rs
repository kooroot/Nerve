use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use nerve_adapter::default_adapters;
use nerve_config::{Config, DaemonProtocol};
use nerve_core::store::NerveStore;
use nerve_core::{RunOptions, RunReport, run_synaptic_loop};
use nerve_types::{AgentEvent, Task, Verdict};
use std::env;
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::Command as StdCommand;
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
    let stdin = io::BufReader::new(io::stdin());
    let mut lines = stdin.lines();

    loop {
        print!("{}", interactive_prompt(&state));
        std::io::stdout().flush()?;
        let Some(line) = lines.next_line().await? else {
            break;
        };
        let input = line.trim();
        if input.is_empty() {
            continue;
        }
        if let Some(command) = input.strip_prefix('/') {
            match handle_interactive_command(command, &mut state).await {
                Ok(true) => break,
                Ok(false) => {}
                Err(error) => print_interactive_error(&error),
            }
            continue;
        }
        if let Err(error) = run_interactive_task(input.to_string(), &mut state).await {
            print_interactive_error(&error);
        }
    }

    Ok(())
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
    println!("│ Type a task, then use /diff, /apply, /rollback, or /history. │");
    println!("└──────────────────────────────────────────────────────────────┘");
    print_interactive_status(state);
}

fn print_interactive_status(state: &InteractiveState) {
    println!(
        "adapter={} mode={} sessions={} patches={} cwd={}",
        state.adapter_label(),
        state.apply_label(),
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
    format!(
        "nerve:{}:{}{}> ",
        state.adapter_label(),
        state.apply_label(),
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
        "help" => {
            println!(
                "Commands: /login /doctor /status /history /resume <id> /list /templates /template <id> [args] /diff /apply [patch-id] /rollback [patch-id] /quit"
            );
            println!("Tasks: type any coding request without a leading slash.");
        }
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
        "quit" | "exit" => return Ok(true),
        other => println!("Unknown command /{other}. Type /help for commands."),
    }
    Ok(false)
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
}
