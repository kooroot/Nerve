use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use nerve_adapter::default_adapters;
use nerve_config::Config;
use nerve_core::store::NerveStore;
use nerve_core::{RunOptions, run_synaptic_loop};
use nerve_types::{AgentEvent, Task, Verdict};
use std::env;
use std::path::{Path, PathBuf};

#[derive(Debug, Parser)]
#[command(
    name = "nv",
    about = "Nerve reflexive AI orchestration CLI",
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

    #[arg(long, env = "NERVE_ADAPTER", default_value = "real")]
    adapter: AdapterMode,
}

#[derive(Debug, Clone, clap::ValueEnum)]
enum AdapterMode {
    Real,
    Mock,
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
}

#[derive(Debug, Subcommand)]
enum ConfigCommand {
    Validate,
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
        Some(Command::History { json }) => {
            let cwd = env::current_dir().context("failed to read current directory")?;
            let sessions = NerveStore::new(cwd).list_sessions()?;
            if json {
                println!("{}", serde_json::to_string_pretty(&sessions)?);
            } else if sessions.is_empty() {
                println!("No Nerve sessions found.");
            } else {
                for session in sessions {
                    println!(
                        "{} | {:?} | rounds={} | applied={} | patch={} | {}",
                        session.id,
                        session.verdict,
                        session.rounds,
                        session.applied,
                        session.patch_id.as_deref().unwrap_or("-"),
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
        Some(Command::Doctor) => {
            let cwd = env::current_dir().context("failed to read current directory")?;
            Config::load_from(&cwd)?;
            println!("config: ok");

            match cli.adapter {
                AdapterMode::Mock => {
                    println!("adapter: mock ok");
                    Ok(())
                }
                AdapterMode::Real => {
                    let claude = find_on_path("claude");
                    let codex = find_on_path("codex");
                    print_doctor_check("claude", &claude);
                    print_doctor_check("codex", &codex);
                    if claude.is_some() && codex.is_some() {
                        Ok(())
                    } else {
                        anyhow::bail!("real adapter prerequisites are missing")
                    }
                }
            }
        }
        None => {
            let prompt = cli
                .prompt
                .context("missing prompt; usage: nv \"add a /health endpoint\"")?;
            run_prompt(
                prompt,
                cli.apply,
                cli.json,
                matches!(cli.adapter, AdapterMode::Mock),
            )
            .await
        }
    }
}

async fn run_prompt(prompt: String, apply: bool, json: bool, mock: bool) -> Result<()> {
    let cwd = env::current_dir().context("failed to read current directory")?;
    let config = Config::load_from(&cwd)?;
    let task = Task::new(prompt, &cwd);
    let adapters = default_adapters(mock);
    let report = run_synaptic_loop(task, &config, &adapters, RunOptions { apply }).await?;
    NerveStore::new(&cwd).save_report(&report)?;

    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
        return Ok(());
    }

    print_report(&report, apply);
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

fn is_executable_file(path: &Path) -> bool {
    path.is_file()
}

fn print_doctor_check(binary: &str, path: &Option<PathBuf>) {
    match path {
        Some(path) => println!("{binary}: {}", path.display()),
        None => println!("{binary}: missing"),
    }
}
