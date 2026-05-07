use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use nerve_adapter::default_adapters;
use nerve_config::Config;
use nerve_core::store::NerveStore;
use nerve_core::{RunOptions, RunReport, run_synaptic_loop};
use nerve_types::{AgentEvent, Task, Verdict};
use std::env;
use std::path::{Path, PathBuf};
use std::process::Command as StdCommand;
use tokio::io::{self, AsyncBufReadExt};

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
    #[command(about = "Run a line-oriented daemon for editor and shell integrations")]
    Daemon {
        #[arg(long, help = "Process one prompt from stdin and exit")]
        once: bool,
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
        Some(Command::Daemon { once }) => {
            run_daemon(cli.apply, matches!(cli.adapter, AdapterMode::Mock), once).await
        }
        None => {
            let prompt = cli
                .prompt
                .context("missing prompt; usage: nv \"add a /health endpoint\"")?;
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

async fn run_daemon(apply: bool, mock: bool, once: bool) -> Result<()> {
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
}
