//! Reference host harness: the smallest agent loop that bolts onto
//! slimtoken as its engine, using ONLY the public library surface. This is
//! the pattern another agent system or coding harness embeds: the engine
//! owns planning, wave execution, cache priming, and repair arbitration;
//! the host owns model wiring, the optional stack profile, the assembled
//! deliverable (the engine calls back for it), and the gate command.
//!
//! Loop: plan -> wave -> gate -> repair.
//!
//! Run (one-flag provider preset = the measured fast path):
//!   cargo run --release --example host-harness -- \
//!     --objective "Build a Python CLI that ..." \
//!     --provider deepseek --gate-cmd "python -m pytest suite/ -q"
//!
//! Manual provider wiring (no preset):
//!   ... --base-url https://api.deepseek.com/v1 --model deepseek-flash \
//!     --key-env DEEPSEEK_API_KEY
//!
//! With a host stack profile (file-loaded profile, no recompile):
//!   ... --profile my-stack.json
//! Profile file shape: see `slimtoken::profile::ProfileFile` (base + string
//! overrides; behavior hooks inherit from the base profile).

use anyhow::{Context, Result};
use clap::Parser;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use slimtoken::architect::{generate_manifest, plan_from_manifest, ArchitectOptions, PlanOptions};
use slimtoken::client::OpenAiClient;
use slimtoken::profile::Profile;
use slimtoken::reconcile::reconcile_with;
use slimtoken::scheduler::{execute_plan, SchedulerConfig, TaskStatus};

#[derive(Parser)]
struct Args {
    /// Greenfield objective, same shape the slimtoken CLI takes.
    #[arg(long)]
    objective: String,
    /// Measured provider preset: stamps the tuned architect effort /
    /// schema, worker effort, and cold-burst cap onto the run, so host
    /// planning matches the CLI's measured path instead of the untuned
    /// library defaults (host planning measured 26-80s vs 4.1s before this).
    /// Explicit flags still win. Without it the host wires the provider by
    /// hand with --base-url / --model / --key-env.
    #[arg(long, value_parser = ["muse", "deepseek"])]
    provider: Option<String>,
    #[arg(long, default_value = "https://api.meta.ai/v1")]
    base_url: String,
    #[arg(long, default_value = "muse-spark-1.3-contributor")]
    model: String,
    /// Environment variable holding the provider API key.
    #[arg(long, default_value = "OPENAI_API_KEY")]
    key_env: String,
    /// Task budget handed to the architect.
    #[arg(long, default_value_t = 12)]
    max_tasks: usize,
    /// Optional host stack profile (JSON). Without it the engine
    /// auto-detects a built-in profile from the objective text.
    #[arg(long)]
    profile: Option<PathBuf>,
    /// Optional gate: shell command piped the assembled deliverable; a
    /// non-zero exit triggers one engine repair round, kept only if no more
    /// tests fail. Omit to skip the gate entirely.
    #[arg(long)]
    gate_cmd: Option<String>,
    /// Where the host writes its assembled deliverable.
    #[arg(long, default_value = "host-output.md")]
    output: PathBuf,
    /// Worker reasoning effort forwarded to the provider.
    #[arg(long)]
    effort: Option<String>,
}

/// Host-owned assembly: the engine hands the report back and asks for the
/// deliverable document it should gate on. A host harness renders whatever
/// shape its downstream consumers need; the engine never dictates it. This
/// reference renders `## <file path>` sections (task titles ARE the target
/// paths for architect plans), the shape report extractors consume.
fn assemble(report: &slimtoken::scheduler::RunReport) -> String {
    let mut doc = String::new();
    for record in report.tasks.values() {
        if record.status != TaskStatus::Succeeded {
            continue;
        }
        if let Some(output) = &record.output {
            // TaskRecord carries the plan title; for architect plans that IS
            // the target file path, which is what report extractors consume.
            doc.push_str(&format!("## {}\n\n{output}\n\n", record.title));
        }
    }
    doc
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    // the preset is the host's fast path — one flag applies every
    // measured provider knob (architect effort/schema, worker effort,
    // cold-burst cap). Explicit host flags still win over the preset.
    let preset = args
        .provider
        .as_deref()
        .map(|name| {
            slimtoken::provider::provider_preset(name)
                .with_context(|| format!("unknown provider preset {name:?}"))
        })
        .transpose()?;
    let (base_url, model, api_key) = match &preset {
        Some(preset) => (
            preset.base_url.to_string(),
            preset.model.to_string(),
            preset
                .api_key_from_env()
                .map_err(|message| anyhow::anyhow!(message))
                .context("provider key")?,
        ),
        None => (
            args.base_url.clone(),
            args.model.clone(),
            std::env::var(&args.key_env)
                .with_context(|| format!("environment variable {0} is not set", args.key_env))?,
        ),
    };
    let client: Arc<dyn slimtoken::ModelClient> =
        Arc::new(OpenAiClient::new(&base_url, &model, Some(api_key))?.with_stream_usage(true));
    let mut config = SchedulerConfig {
        reasoning_effort: args.effort.clone(),
        // The measured worker setting for coding arms (the CLI pins 0.0;
        // the library default stays 0.2 for historical reasons).
        default_temperature: 0.0,
        ..SchedulerConfig::default()
    };
    if let Some(preset) = &preset {
        preset.apply_scheduler(&mut config);
    }

    // ---- Plan -------------------------------------------------------------
    let planning_start = Instant::now();
    let plan = match &args.profile {
        Some(path) => {
            // Host-supplied stack: profile file + full control of architect
            // options. This is the integration seam a coding harness uses to
            // pin its own conventions without forking the engine.
            let profile: &'static Profile = Profile::load_file(path)?;
            let mut options = ArchitectOptions {
                profile: Some(profile),
                ..ArchitectOptions::default()
            };
            if let Some(preset) = &preset {
                preset.apply_architect(&mut options);
            }
            let manifest = generate_manifest(
                client.as_ref(),
                &args.objective,
                args.max_tasks,
                &[],
                &options,
            )
            .await?;
            plan_from_manifest(
                &args.objective,
                &manifest,
                args.max_tasks,
                Some(profile),
                &PlanOptions::default(),
            )?
        }
        None => {
            // Engine defaults: built-in profile auto-detection, with the
            // preset's measured planning knobs applied on top when given.
            let mut options = ArchitectOptions::default();
            if let Some(preset) = &preset {
                preset.apply_architect(&mut options);
            }
            let manifest = generate_manifest(
                client.as_ref(),
                &args.objective,
                args.max_tasks,
                &[],
                &options,
            )
            .await?;
            plan_from_manifest(
                &args.objective,
                &manifest,
                args.max_tasks,
                None,
                &PlanOptions::default(),
            )?
        }
    };
    let planning_s = planning_start.elapsed().as_secs_f64();
    println!(
        "[host] plan: {} task(s) in {planning_s:.1}s",
        plan.tasks.len()
    );

    // ---- Wave -------------------------------------------------------------
    let wave_start = Instant::now();
    let mut report = execute_plan(&plan, Arc::clone(&client), config.clone()).await?;
    let wave_s = wave_start.elapsed().as_secs_f64();
    let succeeded = report.status_count(TaskStatus::Succeeded);
    println!(
        "[host] wave: {succeeded}/{} task(s) in {wave_s:.1}s",
        plan.tasks.len()
    );

    // ---- Gate + repair ----------------------------------------------------
    let gate = args.gate_cmd.as_deref().filter(|_| report.is_success());
    if args.gate_cmd.is_some() && gate.is_none() {
        println!("[host] gate: skipped (wave has failed tasks)");
    }
    if let Some(gate) = gate {
        let gate_start = Instant::now();
        let summary = reconcile_with(
            &plan,
            &mut report,
            Arc::clone(&client),
            config,
            gate,
            assemble,
            true,
        )
        .await?;
        let elapsed = gate_start.elapsed().as_secs_f64();
        match summary {
            Some(s) => println!(
                "[host] gate: {} -> {} failing test(s), repaired in {elapsed:.1}s",
                s.before.failed.unwrap_or(0),
                s.after.as_ref().and_then(|after| after.failed).unwrap_or(0)
            ),
            None => println!("[host] gate: passed first try ({elapsed:.1}s)"),
        }
    }

    // ---- Host deliverable -------------------------------------------------
    let deliverable = assemble(&report);
    std::fs::write(&args.output, &deliverable)?;
    let total_out: u64 = report.tasks.values().map(|r| r.output_tokens).sum();
    let total_cached: u64 = report
        .tasks
        .values()
        .map(|r| r.cached_input_tokens.unwrap_or(0))
        .sum();
    let total_in: u64 = report.tasks.values().map(|r| r.input_tokens).sum();
    println!(
        "[host] done: {} tok out, cached share {:.0}%, deliverable -> {}",
        total_out,
        100.0 * total_cached as f64 / total_in.max(1) as f64,
        args.output.display()
    );
    Ok(())
}
