use crate::runner::{RunDetails, RunManager, RunRequest, RunSummary};
use anyhow::{bail, Context, Result};
use std::collections::HashSet;
use std::io::{BufRead as _, Write as _};
use tokio::sync::mpsc;

const HELP: &str = r#"Commands:
  /task <objective>          Submit a background run (alias: /run)
  /plan-only <objective>     Generate and persist a plan without executing it
  /steer <instruction>       Steer pending tasks in the current run
  /steer <run-id> <text>     Steer pending tasks in a specific run
  /status [run-id]           Show the current/specified run, or all runs
  /tasks [run-id]            Show live per-task state
  /show-plan [run-id]        Print the generated plan as JSON
  /report [run-id]           Print final leaf outputs and artifact location
  /cancel [run-id]           Cancel active requests and pending tasks
  /use <run-id>              Select the current run
  /list                      List all runs
  /help                      Show this help
  /quit                      Cancel active runs and exit

Steering submitted during planning applies to worker tasks, not the in-flight
planner request. Steering during execution applies only to tasks that have not
started yet. A new submission becomes the current run automatically."#;

pub async fn run(manager: RunManager) -> Result<()> {
    println!("SlimToken interactive CLI. Type /help for commands.");
    let mut current = None::<String>;
    let (input_tx, mut input_rx) = mpsc::unbounded_channel();
    std::thread::spawn(move || {
        let stdin = std::io::stdin();
        for line in stdin.lock().lines() {
            let Ok(line) = line else {
                break;
            };
            if input_tx.send(line).is_err() {
                break;
            }
        }
    });

    loop {
        print!("slimtoken> ");
        std::io::stdout()
            .flush()
            .context("failed to flush prompt")?;
        let Some(line) = input_rx.recv().await else {
            break;
        };
        match handle_line(&manager, &mut current, &line).await {
            Ok(true) => break,
            Ok(false) => {}
            Err(error) => eprintln!("error: {error:#}"),
        }
        // A stream of already-buffered commands must not starve background runs.
        tokio::task::yield_now().await;
    }

    for summary in manager.list() {
        if !summary.status.is_terminal() {
            manager.cancel(&summary.id);
        }
    }
    Ok(())
}

async fn handle_line(
    manager: &RunManager,
    current: &mut Option<String>,
    line: &str,
) -> Result<bool> {
    let line = line.trim();
    if line.is_empty() {
        return Ok(false);
    }
    let (command, rest) = line
        .split_once(char::is_whitespace)
        .map(|(command, rest)| (command, rest.trim()))
        .unwrap_or((line, ""));

    match command {
        "/task" | "/run" => {
            submit(manager, current, rest, false).await?;
        }
        "/plan-only" => {
            submit(manager, current, rest, true).await?;
        }
        "/steer" => {
            let (id, instruction) = parse_steer(rest, current)?;
            let details = manager
                .steer(&id, &instruction)?
                .ok_or_else(|| anyhow::anyhow!("run '{id}' was not found"))?;
            println!(
                "Steering accepted for {} ({} directives total). It applies to pending tasks.",
                id, details.summary.steering_count
            );
        }
        "/status" => {
            if rest.is_empty() && current.is_none() {
                print_list(manager);
            } else {
                let id = resolve_id(rest, current)?;
                print_details(&required_details(manager, &id)?);
            }
        }
        "/tasks" => {
            let id = resolve_id(rest, current)?;
            let details = required_details(manager, &id)?;
            print_summary(&details.summary);
            if details.tasks.is_empty() {
                println!("No registered tasks yet.");
            }
            for task in details.tasks.values() {
                println!(
                    "  {} | {} | {} | attempts {}",
                    task.id, task.title, task.status, task.attempts
                );
            }
        }
        "/show-plan" => {
            let id = resolve_id(rest, current)?;
            let plan = manager
                .plan(&id)
                .ok_or_else(|| anyhow::anyhow!("plan for '{id}' is not ready"))?;
            println!("{}", serde_json::to_string_pretty(&plan)?);
        }
        "/report" => {
            let id = resolve_id(rest, current)?;
            print_report(manager, &id)?;
        }
        "/cancel" => {
            let id = resolve_id(rest, current)?;
            let details = manager
                .cancel(&id)
                .ok_or_else(|| anyhow::anyhow!("run '{id}' was not found"))?;
            println!("{} is now {}", id, details.summary.status);
        }
        "/use" => {
            if rest.is_empty() {
                bail!("usage: /use <run-id>");
            }
            required_details(manager, rest)?;
            *current = Some(rest.to_string());
            println!("Current run: {rest}");
        }
        "/list" => print_list(manager),
        "/help" => println!("{HELP}"),
        "/quit" | "/exit" => return Ok(true),
        _ => bail!("unknown command '{command}'; type /help"),
    }
    Ok(false)
}

async fn submit(
    manager: &RunManager,
    current: &mut Option<String>,
    objective: &str,
    plan_only: bool,
) -> Result<()> {
    if objective.trim().is_empty() {
        bail!("an objective is required");
    }
    let summary = manager
        .submit(RunRequest {
            objective: Some(objective.to_string()),
            plan_only,
            ..RunRequest::default()
        })
        .await?;
    *current = Some(summary.id.clone());
    println!("Submitted {} with status {}", summary.id, summary.status);
    println!("Use /status, /tasks, /steer <instruction>, or /cancel.");
    Ok(())
}

fn parse_steer(rest: &str, current: &Option<String>) -> Result<(String, String)> {
    if rest.trim().is_empty() {
        bail!("usage: /steer [run-id] <instruction>");
    }
    let mut parts = rest.splitn(2, |character: char| character.is_whitespace());
    let first = parts.next().unwrap_or_default();
    if first.starts_with("run-") {
        let instruction = parts.next().unwrap_or_default().trim();
        if instruction.is_empty() {
            bail!("a steering instruction is required");
        }
        Ok((first.to_string(), instruction.to_string()))
    } else {
        let id = current
            .clone()
            .context("select a run with /use or provide a run ID")?;
        Ok((id, rest.trim().to_string()))
    }
}

fn resolve_id(explicit: &str, current: &Option<String>) -> Result<String> {
    if !explicit.trim().is_empty() {
        return Ok(explicit.trim().to_string());
    }
    current
        .clone()
        .context("no current run; provide a run ID or use /list and /use")
}

fn required_details(manager: &RunManager, id: &str) -> Result<RunDetails> {
    manager
        .get(id)
        .ok_or_else(|| anyhow::anyhow!("run '{id}' was not found"))
}

fn print_list(manager: &RunManager) {
    let runs = manager.list();
    if runs.is_empty() {
        println!("No runs. Submit one with /task <objective>.");
        return;
    }
    for summary in runs {
        print_summary(&summary);
    }
}

fn print_details(details: &RunDetails) {
    print_summary(&details.summary);
    if let Some(error) = &details.summary.error {
        println!("  error: {error}");
    }
    if !details.steering.is_empty() {
        println!("  steering:");
        for (index, directive) in details.steering.iter().enumerate() {
            println!("    {}. {directive}", index + 1);
        }
    }
    println!("  artifacts: {}", details.summary.output_dir);
}

fn print_summary(summary: &RunSummary) {
    println!(
        "{} | {} | tasks {} total, {} running, {} pending, {} succeeded, {} failed, {} skipped, {} cancelled | steering {}",
        summary.id,
        summary.status,
        summary.task_count,
        summary.running,
        summary.pending,
        summary.succeeded,
        summary.failed,
        summary.skipped,
        summary.cancelled,
        summary.steering_count
    );
}

fn print_report(manager: &RunManager, id: &str) -> Result<()> {
    let details = required_details(manager, id)?;
    let report = manager.report(id).ok_or_else(|| {
        anyhow::anyhow!(
            "report for '{id}' is not ready; current status is {}",
            details.summary.status
        )
    })?;
    let plan = manager.plan(id).context("report exists without a plan")?;
    let dependencies: HashSet<&str> = plan
        .tasks
        .iter()
        .flat_map(|task| task.deps.iter().map(String::as_str))
        .collect();
    let mut printed = 0usize;
    for task in plan
        .tasks
        .iter()
        .filter(|task| !dependencies.contains(task.id.as_str()))
    {
        if let Some(output) = report
            .tasks
            .get(&task.id)
            .and_then(|record| record.output.as_ref())
        {
            println!("\n== {} ==\n{}", task.title, output);
            printed += 1;
        }
    }
    if printed == 0 {
        println!("No successful leaf outputs. Inspect report.json for task errors.");
    }
    println!("\nArtifacts: {}", details.summary.output_dir);
    Ok(())
}
