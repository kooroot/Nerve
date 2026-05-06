use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use nerve_adapter::default_adapters;
use nerve_config::Config;
use nerve_core::{RunOptions, run_synaptic_loop};
use nerve_types::{AgentEvent, Task, Verdict};
use std::env;

#[derive(Debug, Parser)]
#[command(name = "nv", about = "Nerve reflexive AI orchestration CLI")]
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

    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
        return Ok(());
    }

    println!("Nerve session {}", report.task.id);
    println!(
        "Profile: {} | lead={} reviewer={} rounds={}",
        report.selection.id.as_deref().unwrap_or("default"),
        report.selection.lead,
        report.selection.reviewer,
        report.rounds.len()
    );
    print_events(&report.events);
    println!("Verdict: {:?}", report.final_feedback.verdict);

    if report.blocked {
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
        } else if !apply {
            println!("Dry run only. Re-run with --apply to change files.");
        }
    } else {
        println!("\nNo structured patch was produced. Raw lead output:\n");
        println!("{}", report.final_output.raw_text);
    }

    if report.final_feedback.verdict != Verdict::Lgtm {
        println!("\nReviewer feedback:\n{}", report.final_feedback.raw_text);
    }

    Ok(())
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
