//! SlimToken Showdown: a side-by-side terminal benchmark.
//!
//! Left: one model stream writes a whole app in one answer. Right: SlimToken
//! plans the app, then writes every file at the same time. Both arms get the
//! same objective, the same model and the same streaming client.
//!
//!   showdown run      a live, measured run (needs an API key)
//!   showdown replay   play a recording of a live run
//!   showdown demo     simulated data, for layout and rehearsal only
//!   showdown judge    a blind quality judge of a finished run (needs an API key)

mod arms;
mod codeindex;
mod demo;
mod events;
mod extract;
mod gate;
mod guard;
mod judge;
mod launch;
mod pages;
mod pchk;
mod qlint;
mod repair;
mod shellfix;
mod state;
mod tap;
mod ui;

use anyhow::{bail, Context, Result};
use clap::{Args, Parser, Subcommand, ValueEnum};
use events::{Arm, Emitter, Ev, Timed};
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use state::Show;
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc::UnboundedReceiver;

/// Default quality checks for the default (Next.js) objective.
const DEFAULT_CHECKS: &[(&str, &str)] = &[
    ("install", "npm install --no-audit --no-fund"),
    ("build", "npm run build"),
    ("test", "npm test"),
];
const DEFAULT_LAUNCH: &str = "npm run dev -- -p {port}";

#[derive(Parser)]
#[command(
    name = "showdown",
    about = "Side-by-side benchmark: one model stream vs a SlimToken parallel wave"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run both arms live against a model server and record the run.
    Run(Box<RunArgs>),
    /// Play the recording of a live run.
    Replay(ReplayArgs),
    /// Play simulated data (not a measurement).
    Demo(PlayArgs),
    /// Start the two apps of a finished run and open them in the browser.
    Launch(LaunchArgs),
    /// Run only the SlimToken arm, some times, and print one line of phase
    /// times for each run (for A/B tests of the pipeline). No quality checks
    /// after the run: the gate's real build is the pass criterion.
    #[command(hide = true)]
    BenchSlim {
        #[arg(long, default_value_t = 1)]
        runs: usize,
        /// Use a saved manifest (no planning call).
        #[arg(long)]
        manifest: Option<PathBuf>,
        #[arg(long)]
        flat: bool,
        #[arg(long)]
        no_repair: bool,
        #[arg(long, default_value = "deepseek")]
        provider: String,
        #[arg(long, default_value_t = 8)]
        repair_rounds: usize,
        /// A name for the lines of this arm of the test.
        #[arg(long, default_value = "run")]
        label: String,
        /// Read the objective from a file. Default: the TaskFlow objective.
        #[arg(long, value_name = "PATH")]
        objective_file: Option<PathBuf>,
        /// Provider preset's model, replaced.
        #[arg(long)]
        model: Option<String>,
    },
    /// Run SlimToken's check-and-repair step again on a finished run.
    #[command(hide = true)]
    Rerepair {
        /// Directory of the run.
        run: PathBuf,
        #[arg(long, default_value = "deepseek")]
        provider: String,
        #[arg(long, default_value_t = 8)]
        repair_rounds: usize,
        /// The app to repair: `slimtoken`, or `single` (the single stream's
        /// app gets the same repair step; its files are the plan).
        #[arg(long, default_value = "slimtoken")]
        arm: String,
    },
    /// The `pages` quality check: start the built app in the current
    /// directory and request every page.
    #[command(hide = true)]
    Pages,
    /// The gate of SlimToken's check-and-repair step (the engine calls it).
    #[command(hide = true)]
    Gate {
        /// Directory that holds the prepared `node_modules`.
        #[arg(long)]
        deps: PathBuf,
    },
    /// Replay saved runs into the persistent checker (`pchk`) in the close
    /// order of their recordings and compare it with the command-line gate.
    /// No model call.
    #[command(hide = true)]
    PchkReplay {
        /// Directory that holds the prepared `node_modules`.
        #[arg(long)]
        deps: PathBuf,
        /// Replay speed (2: the wave in half its recorded time).
        #[arg(long, default_value_t = 1.0)]
        speed: f64,
        /// Milliseconds between two checks (the model's part of a round).
        #[arg(long, default_value_t = 2_800)]
        round_ms: u64,
        /// Run directories (each with `slimtoken/` and `events.jsonl`).
        runs: Vec<PathBuf>,
    },
    /// Judge a finished run blind: requirement coverage and working defects
    /// of both apps from the model, a pairwise call in both orders, and the
    /// static lints. Writes `judge.json` in the run directory.
    Judge(judge::JudgeArgs),
    /// Run the judge on the five pairs of the 2026-11-15 audit and compare
    /// its verdicts with the audit's.
    JudgeValidate(judge::validate::ValidateArgs),
    /// The cross-file quality lints (`qlint`) on app directories, with no
    /// model call. Prints one line per finding and a count per app.
    #[command(hide = true)]
    Lint {
        /// App directories.
        dirs: Vec<PathBuf>,
    },
}

#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
enum Order {
    /// Both arms start at the same moment.
    Race,
    /// The single stream runs first, then SlimToken. The arms do not share
    /// the server. A replay of the recording shows them side by side.
    Sequential,
}

#[derive(Args)]
struct RunArgs {
    /// What to build. Both arms get this text.
    #[arg(long, conflicts_with = "objective_file")]
    objective: Option<String>,
    /// Read the objective from a file.
    #[arg(long, value_name = "PATH")]
    objective_file: Option<PathBuf>,
    /// Provider preset: base URL, model, key variable and measured settings.
    #[arg(long, default_value = "deepseek", value_parser = ["deepseek", "muse", "anthropic", "none"])]
    provider: String,
    /// OpenAI-compatible base URL. Replaces the preset's URL.
    #[arg(long)]
    base_url: Option<String>,
    /// Model name. Replaces the preset's model.
    #[arg(long)]
    model: Option<String>,
    /// Environment variable that holds the API key.
    #[arg(long)]
    key_env: Option<String>,
    /// Reasoning effort for both arms. Default: the preset's worker effort.
    #[arg(long)]
    effort: Option<String>,
    /// Upper limit of files in the SlimToken plan.
    #[arg(long, default_value_t = 48)]
    max_tasks: usize,
    /// Output token limit of the single-stream answer.
    #[arg(long, default_value_t = 128_000)]
    single_max_tokens: u32,
    /// Use a saved manifest; SlimToken does not plan again.
    #[arg(long, value_name = "PATH")]
    manifest: Option<PathBuf>,
    /// Start both arms together, or one after the other.
    #[arg(long, value_enum, default_value_t = Order::Race)]
    order: Order,
    /// Directory for the two apps and the recording.
    #[arg(long, value_name = "DIR")]
    out: Option<PathBuf>,
    /// Keep `node_modules` and `.next` of earlier runs. By default a new run
    /// removes them from the earlier runs in `slimtoken-out-showdown` (each
    /// run installs about 0.7 GB; `launch` installs again when necessary).
    #[arg(long)]
    keep_deps: bool,
    /// One command that checks each app, in place of the default steps
    /// (npm install, npm run build, npm test). It runs in each app directory.
    #[arg(long)]
    check_cmd: Option<String>,
    /// Do not run the checks when the arms finish (the `c` key still runs them).
    #[arg(long)]
    no_check: bool,
    /// Skip SlimToken's check-and-repair step (type check and tests after
    /// the wave, then repair of the files that fail).
    #[arg(long)]
    no_repair: bool,
    /// Write every file in one wave, against the plan only. The default
    /// writes three stages (shared types, then lib modules, then pages,
    /// components and tests), and each stage sees the files before it.
    #[arg(long)]
    flat: bool,
    /// Most repair rounds in SlimToken's check-and-repair step.
    #[arg(long, default_value_t = 8)]
    repair_rounds: usize,
    /// Start both apps and open them in the browser when the run finishes.
    #[arg(long)]
    launch: bool,
    /// Command that starts an app's dev server. `{port}` is a free port.
    #[arg(long, default_value = DEFAULT_LAUNCH)]
    launch_cmd: String,
    /// No full-screen display: print progress lines and the result table.
    #[arg(long)]
    headless: bool,
    /// Seconds for which the opening screen shows the prompt before the
    /// run starts (Enter starts at once). 0: no opening screen.
    #[arg(long, default_value_t = 8.0)]
    intro_secs: f64,
}

#[derive(Args)]
struct LaunchArgs {
    /// Directory of the run (it holds `single` and `slimtoken`). Default:
    /// the newest run in `slimtoken-out-showdown`.
    run: Option<PathBuf>,
    /// Command that starts an app's dev server. `{port}` is a free port.
    #[arg(long, default_value = DEFAULT_LAUNCH)]
    launch_cmd: String,
    /// Stop after this number of seconds (default: stop on Enter).
    #[arg(long, hide = true)]
    for_secs: Option<u64>,
}

#[derive(Args)]
struct PlayArgs {
    /// Playback speed.
    #[arg(long, default_value_t = 1.0)]
    speed: f64,
    /// Seconds for which the opening screen shows the prompt before the
    /// playback starts (Enter starts at once). 0: no opening screen.
    #[arg(long, default_value_t = 8.0)]
    intro_secs: f64,
    /// Seed of the simulated data (demo only).
    #[arg(long, default_value_t = 7)]
    seed: u64,
    /// Command that starts an app's dev server (the `l` key). `{port}` is a free port.
    #[arg(long, default_value = DEFAULT_LAUNCH)]
    launch_cmd: String,
    /// Print frames as text and exit: WIDTHxHEIGHT@SECONDS[,SECONDS...].
    /// `end` is the last frame with the result panel, `final` without it.
    #[arg(long, value_name = "SPEC")]
    snapshot: Option<String>,
}

#[derive(Args)]
struct ReplayArgs {
    /// The recording.jsonl of a live run.
    file: PathBuf,
    #[command(flatten)]
    play: PlayArgs,
}

fn main() -> Result<()> {
    // The measured stack, unless the caller chose the switches itself (see
    // `arms::RECOMMENDED_ON`). Set before any thread or child process starts.
    if std::env::var_os("SHOWDOWN_ON").is_none() {
        std::env::set_var("SHOWDOWN_ON", arms::RECOMMENDED_ON.join(","));
    }
    match Cli::parse().command {
        Command::Run(args) => run_live(*args),
        Command::Replay(args) => {
            let events = load_recording(&args.file)?;
            play(events, &args.play)
        }
        Command::Demo(args) => play(demo::timeline(args.seed), &args),
        Command::Launch(args) => launch_run(&args),
        Command::Pages => std::process::exit(pages::run()),
        Command::BenchSlim {
            runs,
            manifest,
            flat,
            no_repair,
            provider,
            repair_rounds,
            label,
            objective_file,
            model,
        } => bench_slim(
            runs,
            manifest,
            flat,
            no_repair,
            &provider,
            repair_rounds,
            &label,
            objective_file.as_deref(),
            model,
        ),
        Command::Rerepair {
            run,
            provider,
            repair_rounds,
            arm,
        } => rerepair(&run, &provider, repair_rounds, &arm),
        Command::Gate { deps } => std::process::exit(gate::run(&deps)),
        Command::PchkReplay {
            deps,
            speed,
            round_ms,
            runs,
        } => tokio::runtime::Runtime::new()
            .context("failed to start the async runtime")?
            .block_on(pchk::replay(&deps, &runs, speed, round_ms)),
        Command::Judge(args) => judge::run(&args),
        Command::JudgeValidate(args) => judge::validate::run(&args),
        Command::Lint { dirs } => lint_dirs(&dirs),
    }
}

/// `showdown lint`: the `qlint` findings of each app directory (with
/// `qlint2` in `SHOWDOWN_ON`, the convention lints after them).
fn lint_dirs(dirs: &[PathBuf]) -> Result<()> {
    let conventions = arms::step_on("qlint2");
    let semantics = arms::step_on("qsemantic");
    // The switches change what is reported (`ungated-store` needs
    // `qsemantic`), so say which ones this run used.
    eprintln!(
        "showdown lint: qlint2 {}, qsemantic {} (set SHOWDOWN_ON to choose)",
        if conventions { "on" } else { "off" },
        if semantics { "on" } else { "off" }
    );
    for dir in dirs {
        let files = judge::bundle::load_app(dir)?;
        let lints = qlint::lint_app_with(&files, true, conventions, semantics);
        let errors = lints
            .iter()
            .filter(|lint| lint.level == qlint::Level::Error)
            .count();
        println!(
            "== {} errors {errors} warnings {}",
            dir.display(),
            lints.len() - errors
        );
        for lint in &lints {
            println!("{}", lint.render());
        }
    }
    Ok(())
}

// ---- Time ---------------------------------------------------------------

/// UTC date and time from the system clock: (year, month, day, hour, minute, second).
fn utc_now() -> (i64, u32, u32, u32, u32, u32) {
    let seconds = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    let days = seconds.div_euclid(86_400);
    let rest = seconds.rem_euclid(86_400) as u32;
    // Civil date from a day count (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day, rest / 3_600, rest % 3_600 / 60, rest % 60)
}

// ---- Live run -----------------------------------------------------------

fn find_key(preset_env: Option<&str>, key_env: Option<&str>, provider: &str) -> Option<String> {
    // A provider preset reads only its own variable, so a key for one
    // service never goes to another service's server.
    let fallback = preset_env.is_none().then_some("OPENAI_API_KEY");
    for name in [key_env, preset_env, fallback].into_iter().flatten() {
        if let Ok(value) = std::env::var(name) {
            if !value.trim().is_empty() {
                return Some(value.trim().to_string());
            }
        }
    }
    // The repository's key-file convention: `.deepseek-key`, `.muse-key`.
    let mut dir = std::env::current_dir().ok()?;
    for _ in 0..5 {
        let file = dir.join(format!(".{provider}-key"));
        if let Ok(text) = std::fs::read_to_string(&file) {
            if !text.trim().is_empty() {
                return Some(text.trim().to_string());
            }
        }
        if !dir.pop() {
            break;
        }
    }
    None
}

/// Windows canonical paths start with `\\?\`. Drop that prefix: it is hard to
/// read, and `cmd` and `npm` do not accept it as a working directory.
fn plain_path(path: PathBuf) -> PathBuf {
    match path.to_str().and_then(|text| text.strip_prefix(r"\\?\")) {
        Some(rest) if !rest.starts_with("UNC") => PathBuf::from(rest),
        _ => path,
    }
}

/// Prepare SlimToken's check-and-repair gate for a TypeScript objective and
/// return the gate command. The gate needs the stack's toolchain (TypeScript,
/// Vitest, the framework types) in a dependency directory. That directory is
/// made once, before the clocks start, and later runs use it again.
/// Returns `None` (no check-and-repair step) for other stacks.
fn prepare_gate(objective: &str, out_dir: &Path) -> Option<String> {
    let profile = slimtoken::profile::Profile::detect(objective);
    if profile.name != "nextjs" {
        return None;
    }
    let deps = out_dir.parent().unwrap_or(out_dir).join("_deps");
    let exe = std::env::current_exe().ok()?;
    let (exe, deps_text) = (exe.display().to_string(), deps.display().to_string());
    if cfg!(windows) && (exe.contains(' ') || deps_text.contains(' ')) {
        eprintln!("check-and-repair is off: a path with a space cannot go to the gate command");
        return None;
    }
    std::fs::create_dir_all(&deps).ok()?;
    let manifest = deps.join("package.json");
    if !manifest.is_file() {
        let rendered = (profile.scaffold)("package.json")?;
        let text = if repair::engcheck() {
            slimtoken::project::strip_fence(&rendered)
        } else {
            extract::strip_fence(&rendered)
        };
        std::fs::write(&manifest, text).ok()?;
    }
    if !deps.join("node_modules").join("typescript").is_dir() {
        println!(
            "Preparing the check toolchain in {} (one time, before the clocks start)...",
            deps.display()
        );
        let installed = arms::shell("npm install --no-audit --no-fund")
            .current_dir(&deps)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        if !installed.is_ok_and(|status| status.success()) {
            eprintln!(
                "check-and-repair is off: npm install failed in {}",
                deps.display()
            );
            return None;
        }
    }
    let quote = |text: &str| {
        if text.contains(' ') {
            format!("\"{text}\"")
        } else {
            text.to_string()
        }
    };
    Some(format!("{} gate --deps {}", quote(&exe), quote(&deps_text)))
}

/// The hidden `bench-slim` command.
#[allow(clippy::too_many_arguments)]
fn bench_slim(
    runs: usize,
    manifest: Option<PathBuf>,
    flat: bool,
    no_repair: bool,
    provider: &str,
    repair_rounds: usize,
    label: &str,
    objective_file: Option<&Path>,
    model: Option<String>,
) -> Result<()> {
    let objective = match objective_file {
        Some(path) => std::fs::read_to_string(path)
            .with_context(|| format!("failed to read {}", path.display()))?,
        None => include_str!("../objectives/taskflow.txt").to_string(),
    }
    .trim()
    .to_string();
    let preset = slimtoken::provider::provider_preset(provider)
        .with_context(|| format!("no provider preset {provider}"))?;
    let api_key = find_key(Some(preset.key_env), None, provider);
    if api_key.is_none() {
        bail!("no API key for {provider}");
    }
    let root = PathBuf::from("slimtoken-out-showdown").join("bench");
    let runtime = tokio::runtime::Runtime::new().context("failed to start the async runtime")?;
    for index in 0..runs {
        let (year, month, day, hour, minute, second) = utc_now();
        let stamp =
            format!("{year:04}{month:02}{day:02}-{hour:02}{minute:02}{second:02}-{label}-{index}");
        let out_dir = root.join(&stamp);
        std::fs::create_dir_all(&out_dir)?;
        let out_dir = plain_path(out_dir.canonicalize().unwrap_or(out_dir));
        let gate_cmd = if no_repair {
            None
        } else {
            // The toolchain directory is shared with the normal runs.
            prepare_gate(&objective, &root)
        };
        let config = Arc::new(arms::LiveConfig {
            objective: objective.clone(),
            base_url: preset.base_url.to_string(),
            model: model.clone().unwrap_or_else(|| preset.model.to_string()),
            api_key: api_key.clone(),
            preset: Some(preset),
            effort: None,
            max_tasks: 48,
            single_max_tokens: 0,
            manifest: manifest.clone(),
            out_dir: out_dir.clone(),
            gate_cmd,
            repair_rounds,
            staged: !flat,
        });
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let emitter = Emitter::new(tx);
        runtime.block_on(arms::run_slim(config, emitter));
        let (mut plan, mut wave, mut verify, mut done) = (0u64, None, None, None);
        let (mut before, mut after, mut ok, mut tokens, mut requests) = (0, 0, false, 0u64, 0u64);
        let mut dump = String::new();
        // indexio and the repair guards, added up as the screen does.
        let mut tools = Show::new();
        while let Ok(timed) = rx.try_recv() {
            if matches!(timed.ev, Ev::Tool { .. }) {
                tools.apply(&timed);
            }
            if !matches!(timed.ev, Ev::Delta { .. }) {
                if let Ok(line) = serde_json::to_string(&timed) {
                    dump.push_str(&line);
                    dump.push(char::from(10u8));
                }
            }
            match &timed.ev {
                Ev::Phase { phase, .. } => match phase {
                    events::Phase::Planning => plan = timed.t,
                    events::Phase::Wave => wave = wave.or(Some(timed.t)),
                    events::Phase::Verify => verify = Some(timed.t),
                    events::Phase::Done | events::Phase::Failed => done = Some(timed.t),
                    _ => {}
                },
                Ev::Verified {
                    before_failed,
                    after_failed,
                    ..
                } => {
                    before = *before_failed;
                    after = *after_failed;
                    ok = *after_failed == 0;
                }
                Ev::Close { output_tokens, .. } => {
                    requests += 1;
                    tokens += output_tokens.unwrap_or(0);
                }
                _ => {}
            }
        }
        let _ = std::fs::write(out_dir.join("events.jsonl"), dump);
        let seconds = |from: u64, to: Option<u64>| {
            to.map_or(0.0, |to| (to.saturating_sub(from)) as f64 / 1000.0)
        };
        let wave_at = wave.unwrap_or(plan);
        let code_at = verify.or(done).unwrap_or(wave_at);
        let saved = std::fs::read_to_string(out_dir.join("_meta").join("slimtoken-repair.json"))
            .ok()
            .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
            .unwrap_or_default();
        let rounds = saved["rounds"].as_array().map_or(0, Vec::len);
        let mut tools = tools_fields(&tools.arm(Arm::Slim).tools);
        // With the guards: the rounds that asked the model, and the layout
        // fixes made before the first check (they never reach its count).
        if let Some(root_only) = saved["guard"]["root_only_rounds"].as_u64() {
            tools.push_str(&format!(
                " model_rounds {} layout_fixes {}",
                (rounds as u64).saturating_sub(root_only),
                saved["layout_fixes"].as_u64().unwrap_or(0)
            ));
        }
        println!(
            "{label} total {:.1} plan {:.1} wave {:.1} verify {:.1} rounds {rounds} problems {before}->{after} pass {} tok {tokens} req {requests}{tools} dir {stamp}",
            seconds(plan, done),
            seconds(plan, Some(wave_at)),
            seconds(wave_at, Some(code_at)),
            seconds(code_at, done),
            ok || no_repair,
        );
    }
    Ok(())
}

/// The `bench-slim` fields of the repair guards and indexio (empty with
/// the guards off): ` lock_breaks 1 index indexio indexio_version 0.1.15
/// indexio_calls 22 indexio_ms 640 indexio_crit_ms 30 indexio_fallbacks 0
/// indexio_files 46 indexio_degraded 0` (the version `none` if the check
/// failed; degraded 1 if it failed, indexio was turned off, or every
/// lookup fell back).
fn tools_fields(tools: &state::ToolTotals) -> String {
    let Some(breaks) = tools.lock_breaks else {
        return String::new();
    };
    let mut line = format!(" lock_breaks {breaks}");
    match &tools.indexio {
        Some(_) => line.push_str(&format!(
            " index indexio indexio_version {} indexio_calls {} indexio_ms {} indexio_crit_ms {} indexio_fallbacks {} indexio_files {} indexio_degraded {}",
            tools.indexio_version.as_deref().unwrap_or("none"),
            tools.calls(),
            tools.total_ms(),
            tools.critical_ms,
            tools.fallbacks,
            tools.files,
            u8::from(tools.indexio_degraded()),
        )),
        None => line.push_str(" index text"),
    }
    line
}

/// The hidden `rerepair` command (for work on the repair step).
fn rerepair(run: &Path, provider: &str, repair_rounds: usize, arm: &str) -> Result<()> {
    let results = std::fs::read_to_string(run.join("results.json"))
        .with_context(|| format!("no results.json in {}", run.display()))?;
    let results: serde_json::Value = serde_json::from_str(&results)?;
    let objective = results["objective"]
        .as_str()
        .context("results.json has no objective")?
        .to_string();
    let preset = slimtoken::provider::provider_preset(provider)
        .with_context(|| format!("no provider preset {provider}"))?;
    let api_key = find_key(Some(preset.key_env), None, provider);
    if api_key.is_none() {
        bail!("no API key for {provider}");
    }
    let run = plain_path(run.canonicalize().unwrap_or(run.to_path_buf()));
    let gate_cmd = prepare_gate(&objective, &run);
    let config = arms::LiveConfig {
        objective,
        base_url: preset.base_url.to_string(),
        model: preset.model.to_string(),
        api_key,
        preset: Some(preset),
        effort: None,
        max_tasks: 48,
        single_max_tokens: 0,
        manifest: None,
        out_dir: run.clone(),
        gate_cmd,
        repair_rounds,
        staged: true,
    };
    let runtime = tokio::runtime::Runtime::new().context("failed to start the async runtime")?;
    // The events have no reader here; the channel only has to stay open.
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let emitter = Emitter::new(tx);
    let summary = runtime.block_on(arms::rerepair(&config, &emitter, &run, arm))?;
    println!(
        "before {} | after {} | {:.1} s",
        summary.before.summary, summary.after.summary, summary.seconds
    );
    for round in &summary.rounds {
        println!(
            "  round {:.1} s  {} tok  {}  {}  {:?}",
            round.seconds,
            round.output_tokens,
            if round.kept { "best" } else { "    " },
            round.after.summary,
            round.files
        );
    }
    for (file, text) in &summary.after.problems {
        println!(
            "  left in {file}:
{text}"
        );
    }
    Ok(())
}

fn run_live(args: RunArgs) -> Result<()> {
    let objective = match (&args.objective, &args.objective_file) {
        (Some(text), _) => text.clone(),
        (None, Some(path)) => std::fs::read_to_string(path)
            .with_context(|| format!("failed to read {}", path.display()))?,
        (None, None) => include_str!("../objectives/taskflow.txt").to_string(),
    };
    let objective = objective.trim().to_string();
    if objective.is_empty() {
        bail!("the objective is empty");
    }
    let preset = slimtoken::provider::provider_preset(&args.provider);
    let base_url = args
        .base_url
        .clone()
        .or_else(|| preset.map(|preset| preset.base_url.to_string()))
        .context("give --base-url when --provider is none")?;
    let model = args
        .model
        .clone()
        .or_else(|| preset.map(|preset| preset.model.to_string()))
        .context("give --model when --provider is none")?;
    let api_key = find_key(
        preset.map(|preset| preset.key_env),
        args.key_env.as_deref(),
        &args.provider,
    );
    let local = base_url.contains("127.0.0.1") || base_url.contains("localhost");
    if api_key.is_none() && !local {
        bail!(
            "no API key: set {} (or put the key in a .{}-key file in this directory)",
            args.key_env
                .as_deref()
                .or(preset.map(|preset| preset.key_env))
                .unwrap_or("OPENAI_API_KEY"),
            args.provider
        );
    }

    let (year, month, day, hour, minute, second) = utc_now();
    let stamp = format!("{year:04}{month:02}{day:02}-{hour:02}{minute:02}{second:02}");
    let out_dir = args
        .out
        .clone()
        .unwrap_or_else(|| PathBuf::from("slimtoken-out-showdown").join(&stamp));
    if args.out.is_none() && !args.keep_deps {
        prune_earlier_runs(Path::new("slimtoken-out-showdown"), &stamp);
    }
    std::fs::create_dir_all(&out_dir)
        .with_context(|| format!("failed to create {}", out_dir.display()))?;
    let out_dir = plain_path(out_dir.canonicalize().unwrap_or(out_dir));
    let recording_path = out_dir.join("recording.jsonl");
    let mut recording = std::io::BufWriter::new(
        std::fs::File::create(&recording_path)
            .with_context(|| format!("failed to create {}", recording_path.display()))?,
    );

    let gate_cmd = if args.no_repair {
        None
    } else {
        prepare_gate(&objective, &out_dir)
    };
    let config = Arc::new(arms::LiveConfig {
        objective: objective.clone(),
        base_url,
        model: model.clone(),
        api_key,
        preset,
        effort: args.effort.clone(),
        max_tasks: args.max_tasks,
        single_max_tokens: args.single_max_tokens,
        manifest: args.manifest.clone(),
        out_dir: out_dir.clone(),
        gate_cmd,
        repair_rounds: args.repair_rounds,
        staged: !args.flat,
    });

    let mut steps: Vec<arms::CheckStep> = match &args.check_cmd {
        Some(command) => vec![arms::CheckStep {
            name: "check".to_string(),
            command: command.clone(),
        }],
        None => DEFAULT_CHECKS
            .iter()
            .map(|(name, command)| arms::CheckStep {
                name: name.to_string(),
                command: command.to_string(),
            })
            .collect(),
    };
    // The fourth default check opens every page of the built app.
    let exe = std::env::current_exe().map(|path| path.display().to_string());
    if let (None, Ok(exe)) = (&args.check_cmd, exe) {
        if !exe.contains(' ') {
            steps.push(arms::CheckStep {
                name: "pages".to_string(),
                command: format!("{exe} pages"),
            });
        }
    }
    // The opening screen: the prompt, before any request is sent.
    let headless = args.headless || !std::io::stdout().is_terminal();
    if !headless && !intro(&objective, &model, &args.provider, false, args.intro_secs)? {
        return Ok(());
    }
    let runtime = tokio::runtime::Runtime::new().context("failed to start the async runtime")?;
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let emitter = Emitter::new(tx);
    emitter.emit(Ev::Meta {
        objective,
        provider: args.provider.clone(),
        model,
        source: "live".to_string(),
        recorded_at: format!("{year:04}-{month:02}-{day:02} {hour:02}:{minute:02} UTC"),
        check_cmd: Some(
            steps
                .iter()
                .map(|step| step.command.as_str())
                .collect::<Vec<_>>()
                .join(" && "),
        ),
    });
    match args.order {
        Order::Race => {
            runtime.spawn(arms::run_single(config.clone(), emitter.clone()));
            runtime.spawn(arms::run_slim(config.clone(), emitter.clone()));
        }
        Order::Sequential => {
            let (config, emitter) = (config.clone(), emitter.clone());
            runtime.spawn(async move {
                arms::run_single(config.clone(), emitter.clone()).await;
                arms::run_slim(config, emitter).await;
            });
        }
    }

    let mut show = Show::new();
    let mut live = Live {
        rx: &mut rx,
        emitter: emitter.clone(),
        recording: &mut recording,
        runtime: &runtime,
        steps,
        auto_check: !args.no_check,
        check_started: false,
        handles: Vec::new(),
        launcher: launch::Launcher::new(&args.launch_cmd),
        launch_wanted: args.launch,
    };
    let outcome = if headless {
        live_headless(&mut show, &mut live)
    } else {
        live_screen(&mut show, &mut live)
    };
    live.pump(&mut show)?;
    drop(live);
    recording.flush()?;
    drop(runtime);
    outcome?;

    std::fs::write(
        out_dir.join("results.json"),
        serde_json::to_string_pretty(&results_json(&show))?,
    )?;
    println!("{}", summary(&show));
    println!("recording   {}", recording_path.display());
    println!(
        "replay      showdown replay \"{}\"",
        recording_path.display()
    );
    Ok(())
}

struct Live<'a> {
    rx: &'a mut UnboundedReceiver<Timed>,
    emitter: Emitter,
    recording: &'a mut std::io::BufWriter<std::fs::File>,
    runtime: &'a tokio::runtime::Runtime,
    steps: Vec<arms::CheckStep>,
    auto_check: bool,
    check_started: bool,
    handles: Vec<tokio::task::JoinHandle<()>>,
    launcher: launch::Launcher,
    /// Launch the apps as soon as the arms and the checks are done.
    launch_wanted: bool,
}

impl Live<'_> {
    /// Move new events from the arms into the state and the recording.
    fn pump(&mut self, show: &mut Show) -> Result<()> {
        while let Ok(timed) = self.rx.try_recv() {
            serde_json::to_writer(&mut *self.recording, &timed)?;
            self.recording.write_all(b"\n")?;
            show.apply(&timed);
        }
        show.tick(self.emitter.now());
        if self.auto_check && show.all_finished() {
            self.start_check(show);
        }
        // Launch waits for the checks: both need `npm install` in the app.
        if self.launch_wanted && show.all_finished() && !self.checks_pending() {
            self.launch_wanted = false;
            launch_apps(&self.launcher, show);
        }
        Ok(())
    }

    fn start_check(&mut self, show: &Show) {
        if self.check_started || !show.all_finished() {
            return;
        }
        self.check_started = true;
        for arm in Arm::ALL {
            let Some(outcome) = &show.arm(arm).outcome else {
                continue;
            };
            if outcome.files.is_empty() {
                continue;
            }
            self.handles.push(self.runtime.spawn(arms::run_checks(
                arm,
                PathBuf::from(&outcome.out_dir),
                self.steps.clone(),
                self.emitter.clone(),
            )));
        }
    }

    fn checks_pending(&self) -> bool {
        self.handles.iter().any(|handle| !handle.is_finished())
    }
}

/// Remove the installed dependencies and build output of earlier runs in
/// `root`. The app sources, recordings and results stay.
fn prune_earlier_runs(root: &Path, current: &str) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    let mut removed = 0;
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        // Run directories are UTC time stamps: `YYYYMMDD-HHMMSS`.
        let is_run = name.len() == 15
            && name.chars().enumerate().all(|(index, ch)| {
                if index == 8 {
                    ch == '-'
                } else {
                    ch.is_ascii_digit()
                }
            });
        if !is_run || name == current {
            continue;
        }
        for arm in ["single", "slimtoken"] {
            for dir in ["node_modules", ".next"] {
                let path = entry.path().join(arm).join(dir);
                if path.is_dir() && std::fs::remove_dir_all(&path).is_ok() {
                    removed += 1;
                }
            }
        }
    }
    if removed > 0 {
        println!(
            "Removed node_modules and .next from earlier runs in {} (--keep-deps keeps them).",
            root.display()
        );
    }
}

/// The newest run directory in `root` (run names are UTC time stamps).
fn newest_run(root: &Path) -> Result<PathBuf> {
    let mut runs: Vec<PathBuf> = std::fs::read_dir(root)
        .with_context(|| format!("no runs in {}", root.display()))?
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| path.join("slimtoken").is_dir() || path.join("single").is_dir())
        .collect();
    runs.sort();
    runs.pop()
        .with_context(|| format!("no runs in {}", root.display()))
}

/// The `launch` command: start the apps of a finished run.
fn launch_run(args: &LaunchArgs) -> Result<()> {
    let run = match &args.run {
        Some(path) => path.clone(),
        None => newest_run(Path::new("slimtoken-out-showdown"))?,
    };
    let launcher = launch::Launcher::new(&args.launch_cmd);
    let names = [(Arm::Single, "single"), (Arm::Slim, "slimtoken")];
    let mut any = false;
    for (arm, name) in names {
        let dir = run.join(name);
        if dir.is_dir() {
            launcher.start(arm, dir);
            any = true;
        }
    }
    if !any {
        bail!("{} has no `single` or `slimtoken` directory", run.display());
    }
    println!("Starting the apps of {} ...", run.display());
    let mut shown: [Option<String>; 2] = [None, None];
    loop {
        let view = launcher.view();
        let mut settled = true;
        for (arm, name) in names {
            let Some((text, ready)) = &view[arm.index()] else {
                continue;
            };
            settled &= *ready || text.starts_with("launch failed");
            if shown[arm.index()].as_ref() != Some(text) {
                println!("{name:<10} {text}");
                shown[arm.index()] = Some(text.clone());
            }
        }
        if settled {
            break;
        }
        std::thread::sleep(Duration::from_millis(300));
    }
    match args.for_secs {
        Some(seconds) => std::thread::sleep(Duration::from_secs(seconds)),
        None => {
            println!("Press Enter to stop the apps.");
            let mut line = String::new();
            let _ = std::io::stdin().read_line(&mut line);
        }
    }
    Ok(())
}

/// Start each app that has files on disk.
fn launch_apps(launcher: &launch::Launcher, show: &Show) {
    for arm in Arm::ALL {
        if let Some(outcome) = &show.arm(arm).outcome {
            let dir = PathBuf::from(&outcome.out_dir);
            if dir.is_dir() {
                launcher.start(arm, dir);
            }
        }
    }
}

fn live_headless(show: &mut Show, live: &mut Live) -> Result<()> {
    let mut last_print = Instant::now();
    loop {
        live.pump(show)?;
        if show.all_finished() && !live.checks_pending() {
            // One more pump: the last check events may still be in the channel.
            live.pump(show)?;
            return Ok(());
        }
        if last_print.elapsed() >= Duration::from_secs(2) {
            last_print = Instant::now();
            let line: Vec<String> = Arm::ALL
                .iter()
                .map(|arm| {
                    let state = show.arm(*arm);
                    format!(
                        "{:<9} {:>6.1}s {:?} {:>6.0} tok/s {:>7.0} tok {}/{} files",
                        arm.dir_name(),
                        state.elapsed(show.now),
                        state.phase,
                        state.current_rate(),
                        state.output_tokens(show.chars_per_token),
                        state.files_done(),
                        state.rows.len()
                    )
                })
                .collect();
            println!("{}", line.join("  |  "));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn open_folder(path: &str) {
    let program = if cfg!(windows) {
        "explorer"
    } else if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    let _ = std::process::Command::new(program).arg(path).spawn();
}

/// The opening screen ([`ui::draw_intro`]) for `secs` seconds or until
/// Enter or space. `false`: the person quit. 0 seconds: no screen.
fn intro(objective: &str, model: &str, provider: &str, simulated: bool, secs: f64) -> Result<bool> {
    if secs <= 0.0 {
        return Ok(true);
    }
    let mut terminal = ratatui::init();
    let started = Instant::now();
    let result = (|| -> Result<bool> {
        loop {
            let left = secs - started.elapsed().as_secs_f64();
            if left <= 0.0 {
                return Ok(true);
            }
            terminal.draw(|frame| {
                ui::draw_intro(frame, objective, model, provider, simulated, Some(left))
            })?;
            if !event::poll(Duration::from_millis(100))? {
                continue;
            }
            let Event::Key(key) = event::read()? else {
                continue;
            };
            if key.kind != KeyEventKind::Press {
                continue;
            }
            match key.code {
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    return Ok(false)
                }
                KeyCode::Char('q') | KeyCode::Esc => return Ok(false),
                KeyCode::Enter | KeyCode::Char(' ') => return Ok(true),
                _ => {}
            }
        }
    })();
    ratatui::restore();
    result
}

fn live_screen(show: &mut Show, live: &mut Live) -> Result<()> {
    let mut terminal = ratatui::init();
    let mut view = ui::View {
        mode: "LIVE",
        results_open: false,
        frame: 0,
        paused: false,
        speed: 1.0,
        keys: "l launch apps · r results · c checks · o folder · q quit".to_string(),
        launch: [None, None],
    };
    let mut finished_at: Option<Instant> = None;
    let mut results_shown = false;
    let result = (|| -> Result<()> {
        loop {
            live.pump(show)?;
            view.launch = live.launcher.view();
            if show.all_finished() {
                let at = *finished_at.get_or_insert_with(Instant::now);
                if !results_shown && at.elapsed() >= Duration::from_millis(1_500) {
                    results_shown = true;
                    view.results_open = true;
                }
            }
            terminal.draw(|frame| ui::draw(frame, show, &view))?;
            view.frame += 1;
            if !event::poll(Duration::from_millis(60))? {
                continue;
            }
            let Event::Key(key) = event::read()? else {
                continue;
            };
            if key.kind != KeyEventKind::Press {
                continue;
            }
            match key.code {
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    return Ok(())
                }
                KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
                KeyCode::Char('r') => view.results_open = !view.results_open,
                KeyCode::Char('c') => live.start_check(show),
                KeyCode::Char('l') => live.launch_wanted = true,
                KeyCode::Char('o') => {
                    if let Some(outcome) = &show.arm(Arm::Slim).outcome {
                        if let Some(parent) = Path::new(&outcome.out_dir).parent() {
                            open_folder(&parent.display().to_string());
                        }
                    }
                }
                _ => {}
            }
        }
    })();
    ratatui::restore();
    result
}

// ---- Playback -----------------------------------------------------------

fn load_recording(path: &Path) -> Result<Vec<Timed>> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    let mut events = Vec::new();
    for (index, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        events.push(
            serde_json::from_str::<Timed>(line).with_context(|| {
                format!("{}: line {} is not an event", path.display(), index + 1)
            })?,
        );
    }
    if events.is_empty() {
        bail!("{} holds no events", path.display());
    }
    Ok(align(events))
}

/// Move each arm's events so the arm starts at second zero. A recording made
/// one arm after the other then plays as a side-by-side race, each arm on
/// its own measured clock.
fn align(mut events: Vec<Timed>) -> Vec<Timed> {
    let mut starts = [0u64; 2];
    for timed in &events {
        if let Ev::ArmStart { arm } = timed.ev {
            starts[arm.index()] = timed.t;
        }
    }
    for timed in &mut events {
        match timed.ev.arm() {
            Some(arm) => timed.t = timed.t.saturating_sub(starts[arm.index()]),
            None => timed.t = 0,
        }
    }
    events.sort_by_key(|timed| timed.t);
    events
}

struct Player {
    events: Vec<Timed>,
    cursor: usize,
    /// Show-clock seconds.
    clock: f64,
}

impl Player {
    fn advance(&mut self, show: &mut Show, seconds: f64) {
        self.clock += seconds;
        while self
            .events
            .get(self.cursor)
            .is_some_and(|timed| timed.t as f64 / 1000.0 <= self.clock)
        {
            show.apply(&self.events[self.cursor]);
            self.cursor += 1;
        }
        show.tick(self.clock);
    }

    fn end(&self) -> f64 {
        self.events.last().map(|timed| timed.t).unwrap_or(0) as f64 / 1000.0
    }
}

fn play(events: Vec<Timed>, args: &PlayArgs) -> Result<()> {
    if let Some(spec) = &args.snapshot {
        return snapshot(events, spec);
    }
    if !std::io::stdout().is_terminal() {
        bail!("playback needs a terminal (or use --snapshot WIDTHxHEIGHT@SECONDS)");
    }
    let mut show = Show::new();
    let mut player = Player {
        events,
        cursor: 0,
        clock: 0.0,
    };
    // Apply the header before the first frame so the mode badge is right.
    player.advance(&mut show, 0.0);
    let mut view = ui::View {
        mode: if show.is_demo() { "DEMO" } else { "REPLAY" },
        results_open: false,
        frame: 0,
        paused: false,
        speed: args.speed.clamp(0.1, 16.0),
        keys: "space pause · ←/→ speed · 0 restart · l launch apps · r results · q quit"
            .to_string(),
        launch: [None, None],
    };
    let launcher = launch::Launcher::new(&args.launch_cmd);
    if !intro(
        &show.meta.objective,
        &show.meta.model,
        &show.meta.provider,
        show.is_demo(),
        args.intro_secs,
    )? {
        return Ok(());
    }
    let mut terminal = ratatui::init();
    let mut last = Instant::now();
    let mut results_shown = false;
    let result = (|| -> Result<()> {
        loop {
            let delta = last.elapsed().as_secs_f64();
            last = Instant::now();
            player.advance(
                &mut show,
                if view.paused { 0.0 } else { delta * view.speed },
            );
            if show.all_finished() && !results_shown && player.clock >= player.end() + 1.5 {
                results_shown = true;
                view.results_open = true;
            }
            view.launch = launcher.view();
            terminal.draw(|frame| ui::draw(frame, &show, &view))?;
            view.frame += 1;
            if !event::poll(Duration::from_millis(60))? {
                continue;
            }
            let Event::Key(key) = event::read()? else {
                continue;
            };
            if key.kind != KeyEventKind::Press {
                continue;
            }
            match key.code {
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    return Ok(())
                }
                KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
                KeyCode::Char(' ') => view.paused = !view.paused,
                KeyCode::Char('l') if show.all_finished() => launch_apps(&launcher, &show),
                KeyCode::Char('r') => view.results_open = !view.results_open,
                KeyCode::Right => view.speed = (view.speed * 2.0).min(16.0),
                KeyCode::Left => view.speed = (view.speed / 2.0).max(0.125),
                KeyCode::Char('0') => {
                    show = Show::new();
                    player.cursor = 0;
                    player.clock = 0.0;
                    results_shown = false;
                    view.results_open = false;
                    view.paused = false;
                }
                _ => {}
            }
        }
    })();
    ratatui::restore();
    result
}

/// Render chosen frames to plain text. For layout checks and documentation.
fn snapshot(events: Vec<Timed>, spec: &str) -> Result<()> {
    let (size, times) = spec
        .split_once('@')
        .context("snapshot spec is WIDTHxHEIGHT@SECONDS[,SECONDS...]")?;
    let (width, height) = size
        .split_once('x')
        .context("snapshot size is WIDTHxHEIGHT")?;
    let (width, height): (u16, u16) = (width.parse()?, height.parse()?);
    let end = events.last().map(|timed| timed.t).unwrap_or(0) as f64 / 1000.0;
    for token in times.split(',') {
        let (target, results) = match token.trim() {
            "end" => (end + 2.0, true),
            "final" => (end + 2.0, false),
            seconds => (seconds.parse::<f64>()?, false),
        };
        let mut show = Show::new();
        let mut player = Player {
            events: events.clone(),
            cursor: 0,
            clock: 0.0,
        };
        player.advance(&mut show, 0.0);
        while player.clock < target {
            player.advance(&mut show, 0.1);
        }
        let view = ui::View {
            mode: if show.is_demo() { "DEMO" } else { "REPLAY" },
            results_open: results,
            frame: (target * 10.0) as usize,
            paused: false,
            speed: 1.0,
            keys: "space pause · ←/→ speed · 0 restart · l launch apps · r results · q quit"
                .to_string(),
            launch: [None, None],
        };
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height))?;
        terminal.draw(|frame| ui::draw(frame, &show, &view))?;
        let buffer = terminal.backend().buffer();
        println!("──── t = {target:.1}s ────");
        for y in 0..height {
            let mut line = String::new();
            for x in 0..width {
                line.push_str(buffer[(x, y)].symbol());
            }
            println!("{}", line.trim_end());
        }
    }
    Ok(())
}

// ---- Results ------------------------------------------------------------

fn results_json(show: &Show) -> serde_json::Value {
    let arm = |arm: Arm| {
        let state = show.arm(arm);
        let elapsed = state.elapsed(show.now);
        let tokens = state.output_tokens(show.chars_per_token);
        let mut value = serde_json::json!({
            "wall_seconds": (elapsed * 1000.0).round() / 1000.0,
            "planning_seconds": state.planning_seconds(show.now),
            "output_tokens": tokens.round() as u64,
            "output_tokens_exact": state.usage_exact(),
            "output_tokens_per_second": if elapsed > 0.0 { (tokens / elapsed).round() } else { 0.0 },
            "peak_output_tokens_per_second": state.peak_rate.round(),
            "input_tokens": state.input_tokens(),
            "cached_input_tokens": state.cached_input_tokens(),
            "requests": state.requests,
            "failed_requests": state.failures,
            "peak_streams": state.peak_streams,
            "files": state.outcome.as_ref().map(|outcome| outcome.files.len()),
            "lines": state.lines_written(),
            "ok": state.outcome.as_ref().map(|outcome| outcome.ok),
            "note": state.outcome.as_ref().and_then(|outcome| outcome.note.clone()),
            "out_dir": state.outcome.as_ref().map(|outcome| outcome.out_dir.clone()),
            "checks": state.checks.iter().map(|check| serde_json::json!({
                "name": check.name,
                "state": check.state,
                "seconds": check.seconds,
                "summary": check.tail,
            })).collect::<Vec<_>>(),
        });
        // The repair guards and indexio: only when they ran.
        if let Some(breaks) = state.tools.lock_breaks {
            value["guard"] = serde_json::json!({
                "lock_breaks": breaks,
                "summary": state.tools.guard,
            });
        }
        if let Some(start) = &state.tools.indexio {
            value["indexio"] = serde_json::json!({
                "version": state.tools.indexio_version,
                "start": start,
                "degraded": state.tools.indexio_degraded(),
                "down": state.tools.indexio_down,
                "files": state.tools.files,
                "index_ms": state.tools.index_ms,
                "reindex": state.tools.reindex,
                "reindex_ms": state.tools.reindex_ms,
                "queries": state.tools.queries,
                "query_ms": state.tools.query_ms,
                "calls": state.tools.calls(),
                "total_ms": state.tools.total_ms(),
                "critical_ms": state.tools.critical_ms,
                "fallbacks": state.tools.fallbacks,
            });
        }
        value
    };
    serde_json::json!({
        "objective": show.meta.objective,
        "provider": show.meta.provider,
        "model": show.meta.model,
        "source": show.meta.source,
        "recorded_at": show.meta.recorded_at,
        "speedup": show.speedup().filter(|_| show.all_finished()),
        "single": arm(Arm::Single),
        "slimtoken": arm(Arm::Slim),
    })
}

fn summary(show: &Show) -> String {
    let mut out = String::new();
    let cell = |arm: Arm| {
        let state = show.arm(arm);
        let elapsed = state.elapsed(show.now);
        let tokens = state.output_tokens(show.chars_per_token);
        let (files, ok, dir) = match &state.outcome {
            Some(outcome) => (outcome.files.len(), outcome.ok, outcome.out_dir.clone()),
            None => (0, false, "-".to_string()),
        };
        format!(
            "{:<10} {:>7.1}s  {:>7} tok  {:>6.0} tok/s  {:>3} files  {:>6} lines  {}  {}",
            arm.dir_name(),
            elapsed,
            tokens.round() as u64,
            if elapsed > 0.0 { tokens / elapsed } else { 0.0 },
            files,
            state.lines_written(),
            if ok { "ok    " } else { "NOT OK" },
            dir
        )
    };
    out.push('\n');
    out.push_str(&cell(Arm::Single));
    out.push('\n');
    out.push_str(&cell(Arm::Slim));
    out.push('\n');
    for arm in Arm::ALL {
        let state = show.arm(arm);
        if let Some(note) = state
            .outcome
            .as_ref()
            .and_then(|outcome| outcome.note.as_ref())
        {
            out.push_str(&format!("{:<10} note: {note}\n", arm.dir_name()));
        }
        for check in &state.checks {
            out.push_str(&format!(
                "{:<10} {:<8} {:?} in {:.0}s  {}\n",
                arm.dir_name(),
                check.name,
                check.state,
                check.seconds,
                check.tail
            ));
        }
    }
    if let (true, Some(speedup)) = (show.all_finished(), show.speedup()) {
        out.push_str(&format!(
            "speedup     {speedup:.2}x (single wall / slimtoken wall)\n"
        ));
    }
    out
}
