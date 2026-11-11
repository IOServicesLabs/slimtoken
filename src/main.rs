use anyhow::{bail, Context, Result};
use clap::{Args, Parser, Subcommand};
use serde::{Deserialize, Serialize};
use serde_json::json;
use slimtoken::architect::{generate_manifest, plan_from_manifest, ArchitectOptions};
use slimtoken::client::{MockClient, ModelClient, OpenAiClient};
use slimtoken::control::RunControl;
use slimtoken::controller::ControllerConfig;
use slimtoken::dag::{Plan, Task};
use slimtoken::planner::{generate_plan_with_steering, mock_plan};
use slimtoken::profile::{DurationProfile, MaxTokensProfile, Profile};
use slimtoken::runner::RunManager;
use slimtoken::scheduler::ProbeConfig;
use slimtoken::scheduler::{
    execute_plan, execute_plan_with_control, RunReport, SchedulerConfig, TaskStatus,
};
#[cfg(feature = "serve")]
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tracing::info;
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(
    name = "slimtoken",
    version,
    about = "Concurrency-first DAG harness for local LLM coding and agent work"
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Generate (or load) a task DAG and execute it.
    Run(Box<RunArgs>),
    /// Submit and control runs interactively with task-style commands.
    Interactive(RuntimeArgs),
    /// Expose the shared run manager through an optional REST API.
    /// Requires the `serve` cargo feature.
    #[cfg(feature = "serve")]
    Serve(ServeArgs),
    /// Load the model server with independent requests and report throughput.
    Bench(BenchArgs),
    /// A/B two configurations over paired, interleaved rounds with bootstrap CIs.
    Ab(AbArgs),
    /// Validate a saved plan without executing it.
    Validate {
        #[arg(long)]
        plan: PathBuf,
    },
}

#[derive(Args)]
struct RunArgs {
    /// Objective to decompose. Required unless --plan is supplied.
    #[arg(long, conflicts_with = "plan")]
    objective: Option<String>,
    /// Load a previously generated JSON plan instead of calling the planner.
    #[arg(long)]
    plan: Option<PathBuf>,
    /// Directory for plan.json and report.json.
    #[arg(long, default_value = "slimtoken-out")]
    output: PathBuf,
    /// Write and validate the plan, but do not execute its tasks.
    #[arg(long)]
    plan_only: bool,
    /// Steering directive for planning and worker prompts; repeat as needed.
    #[arg(long = "steer")]
    steering: Vec<String>,
    /// Use a deterministic local planner and worker; no model server needed.
    #[arg(long)]
    mock: bool,
    /// Maximum tasks the model planner may emit.
    #[arg(long, default_value_t = DEFAULT_MAX_TASKS)]
    max_tasks: usize,
    /// Simulated latency for each mock worker.
    #[arg(long, default_value_t = 30)]
    mock_latency_ms: u64,
    /// Append a final integration task that depends on every leaf task and
    /// writes one consolidated final-output.md (the model merges all worker
    /// artifacts into a single deliverable). Disable with `--integrate false`.
    #[arg(long, default_value_t = true)]
    integrate: bool,
    /// Greenfield mode: one cheap architect call fixes the file manifest and
    /// the per-file interface contracts; workers are built deterministically
    /// with zero dependency edges (one fully parallel wave) and code against
    /// the contracts instead of each other's output.
    #[arg(long)]
    architect: bool,
    /// Split fragments are the default planning mode (skeleton, then all
    /// export fragments concurrently). Kept for script compatibility.
    #[arg(long, requires = "architect", hide = true)]
    architect_split: bool,
    /// Plan with one single-shot manifest call (falls back to fragments on a
    /// cut). Measured worse: degraded server windows cap completions at ~1k
    /// tokens incl. reasoning, so single-shot planning took 284-353s or
    /// failed where split fragments took 26-62s; healthy windows are on par.
    #[arg(long, requires = "architect", conflicts_with = "architect_split")]
    architect_single_shot: bool,
    /// Replay a recorded architect manifest (an earlier run's manifest.json)
    /// instead of calling the architect. Planner-side changes then A/B on
    /// identical contracts, free of planning-call noise.
    #[arg(long, requires = "architect", value_name = "PATH")]
    manifest: Option<PathBuf>,
    /// Race this many identical architect requests per ladder step and keep
    /// the first parseable manifest (planning tail-latency hedge). 1 = off.
    #[arg(long, requires = "architect", default_value_t = 1, value_name = "N")]
    architect_hedge: usize,
    /// Constrain split-planning export fragments with a strict json_schema
    /// response_format. On by default (ABBA run: planning 11.0 vs 20.3s
    /// mean, worst 12.6 vs 46.8s, -27% tokens, 0 vs 1 retries, layouts
    /// intact). The layout skeleton stays free: a schema there merged the
    /// objective's named modules. `--architect-schema false` for servers
    /// without json_schema support.
    #[arg(long, requires = "architect", default_value_t = true, action = clap::ArgAction::Set)]
    architect_schema: bool,
    /// Reasoning effort for the first attempt of each planning fragment.
    /// "minimal" measured -62% planning time, -63% tokens vs "low".
    #[arg(long, requires = "architect", default_value = "minimal",
          value_parser = ["none", "minimal", "low", "medium", "high"])]
    architect_effort: String,
    /// Export contracts pin each function's edge behavior (empty input,
    /// which invalid inputs raise, bounds) so blind module and test workers
    /// agree on it. On by default: e2e ABBA own-test pass 87.5 -> 93.6%
    /// (DeepSeek, 3 pairs) and 93.3 -> 96.6% (Muse, 2 pairs), -11% tokens.
    #[arg(long, requires = "architect", default_value_t = true, action = clap::ArgAction::Set)]
    architect_edges: bool,
    /// Export contracts end with one literal call -> result example pinning
    /// the value format (URL vs file path, ordering, key names) that edge
    /// rules leave open. Under A/B.
    #[arg(long, requires = "architect", default_value_t = false, action = clap::ArgAction::Set)]
    architect_examples: bool,
    /// Files per concurrent export-contract fragment. Planning wall is the
    /// slowest fragment, so fewer files per fragment shortens the tail at the
    /// cost of more (prefix-cached) requests. Under A/B.
    #[arg(long, requires = "architect", default_value_t = 2)]
    architect_files_per_fragment: usize,
    /// Python objectives that enumerate their modules (`(1) path.py -
    /// purpose; ...`) skip the skeleton model call. Under A/B.
    #[arg(long, requires = "architect", default_value_t = true, action = clap::ArgAction::Set)]
    architect_objective_skeleton: bool,
    /// Stack profile for architect plans (python, nextjs, rust, or a
    /// --profile-dir library name); auto detects it from the objective.
    #[arg(long, requires = "architect", default_value = "auto", value_parser = parse_profile)]
    profile: ProfileArg,
    /// Load a stack profile from a JSON file: data fields override a
    /// built-in `base` profile's strings; behavior hooks (validators,
    /// scaffold, fixups) are inherited from `base`. Lets a host add a stack
    /// without recompiling. Conflicts with --profile.
    #[arg(long, requires = "architect", value_name = "PATH")]
    profile_file: Option<PathBuf>,
    /// Load every `*.json` profile in a directory into the profile library
    /// `--profile NAME` may then select one by name, and auto
    /// detection consults the library before the built-ins. Later-loaded
    /// names override earlier ones; a file profile may also override a
    /// built-in name. Composes with --profile; the explicit --profile-file
    /// still wins for selection.
    #[arg(long, requires = "architect", value_name = "DIR")]
    profile_dir: Option<PathBuf>,
    /// Pin which exports each consumer file (entry point, page, component)
    /// must call: `plan` asks after the export contracts (+2-3s planning),
    /// `wave` asks as a wave task only consumer files wait for. Under A/B.
    #[arg(long, requires = "architect", value_enum, default_value_t = Wiring::Off)]
    architect_wiring: Wiring,
    /// Append the profile's density rule to both worker heads: no docstrings,
    /// no comments except contract annotations, no re-export facades. Built-in
    /// heads already ban boilerplate comments; this is the strict tier. A/B.
    #[arg(long, requires = "architect", default_value_t = false, action = clap::ArgAction::Set)]
    architect_density: bool,
    /// Cache architect manifests by content hash (one JSON file per key).
    /// A rebuild of the same objective with the same manifest-shaping flags
    /// skips the architect entirely: planning wall -> 0 and planning tokens
    /// -> 0, byte-identical manifest. Plan-shaping flags (tests-see-code,
    /// density, test quota, scope contracts, wave wiring, steering on the
    /// split path) do NOT invalidate the entry — they never reach the
    /// architect. Opt-in; delete the dir to invalidate. A/B, key split
    ///.
    #[arg(long, requires = "architect", value_name = "DIR")]
    manifest_cache: Option<std::path::PathBuf>,
    /// Manifest rules pin shared-type ownership (TypeScript): each
    /// interface/type defined in exactly one file, imported by relative path
    /// elsewhere. Fixes the duplicate-`Article` drift found in every Digest
    /// arm. A/B.
    #[arg(long, requires = "architect", default_value_t = false, action = clap::ArgAction::Set)]
    architect_type_ownership: bool,
    /// Demanded-test headroom per shard, percent of the objective's demand
    /// (default 115 = the measured value). A/B.
    #[arg(long, requires = "architect", default_value_t = slimtoken::architect::DEFAULT_TEST_QUOTA_PCT)]
    architect_test_quota: u64,
    /// Module workers see only their own contract plus the contracts they
    /// call (input-token diet; breaks the shared prefix cache on purpose).
    /// A/B.
    #[arg(long, requires = "architect", default_value_t = false, action = clap::ArgAction::Set)]
    architect_scope_contracts: bool,
    /// Reconcile repairs answer with search/replace edit blocks applied to
    /// the current artifact instead of regenerating the file. A/B run (Recall
    /// plan replays, 4+4): all 8 rounds green; median round 3,956 -> 1,086
    /// tokens and 19.5 -> 10.9s (one edit round reasoned 7.8k tokens).
    #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
    reconcile_edits: bool,
    /// Test shards depend on the modules they cover and see their code, so
    /// tests assert the implemented behavior where contracts are silent.
    /// Plan-replay ABBA run (DeepSeek, 2 rounds): own tests 91.1 -> 97.2%,
    /// contract suite unchanged (38/38); costs a second wave stage.
    #[arg(long, requires = "architect", default_value_t = false, action = clap::ArgAction::Set)]
    architect_tests_see_code: bool,

    /// Size-router override: `auto` estimates the deliverable and sends small
    /// ones to one direct call; `direct` / `wave` force a path (router A/B).
    #[arg(long, requires = "architect", value_enum, default_value_t = Route::Auto)]
    route: Route,
    /// Final-output strategy. "assemble" concatenates leaf outputs in plan
    /// order — deterministic, zero extra tokens, no rewrite artifacts.
    /// "model" appends an integration sink that merges artifacts into one
    /// deliverable (costs a model call and discards most worker tokens).
    #[arg(long, value_parser = ["assemble", "model"], default_value = "model")]
    sink: String,
    /// Split the model sink into N parallel shard tasks (requires
    /// `--sink model`): leaves are partitioned into N weight-balanced groups,
    /// each shard merges its group into a final-deliverable fragment, and the
    /// fragments concatenate in shard order — deterministic, no second model
    /// pass. Motivation: the single sink starts only after the LAST leaf and
    /// then generates the whole document serially (the XL sink wave is
    /// 71% of wall); shards cut both the wait and the per-call generation.
    /// Trade-off: cross-shard consistency fixes are limited to within a group;
    /// 1 (default) keeps the legacy single sink bit-for-bit.
    #[arg(long, default_value_t = 1)]
    sink_shards: usize,
    #[command(flatten)]
    client: ClientArgs,
    #[command(flatten)]
    scheduler: SchedulerArgs,
}

#[derive(Args)]
struct BenchArgs {
    /// Number of independent requests; currently capped at 256.
    #[arg(long, default_value_t = 128)]
    requests: usize,
    /// Maximum output tokens per request.
    #[arg(long, default_value_t = 64)]
    max_tokens: u32,
    /// Base prompt. A unique request variant is appended to reduce cache hits.
    #[arg(
        long,
        default_value = "Write one concise paragraph explaining how a task scheduler works."
    )]
    prompt: String,
    #[command(flatten)]
    client: ClientArgs,
    #[command(flatten)]
    scheduler: SchedulerArgs,
}

#[derive(Args, Clone)]
struct ClientArgs {
    /// Measured settings for a known backend, filling only flags left at
    /// their defaults: base URL, model, worker reasoning effort, planner
    /// effort and schema, and the API-key env var. muse: muse-spark-1.3,
    /// effort minimal, strict planner schema. deepseek: deepseek-flash,
    /// effort none (thinking off), free-JSON planner; key from
    /// DEEPSEEK_API_KEY.
    #[arg(long, value_parser = ["muse", "deepseek"])]
    provider: Option<String>,
    /// Fan the worker wave out over several provider presets, comma-separated
    /// (e.g. muse,deepseek). Each task routes to one backend by a stable hash
    /// of its prompt, so retries stay on the same provider; every backend
    /// applies its own measured reasoning effort. The FIRST entry also serves
    /// planning (manifest + plan), since planner settings (schema, effort) are
    /// provider-specific. Keys come from each preset's env var
    /// (MUSE_API_KEY, DEEPSEEK_API_KEY). Conflicts with --provider, --mock
    /// and --replay-from; report.json records the serving backend per task.
    #[arg(long, value_delimiter = ',')]
    fanout: Option<Vec<String>>,
    /// Circuit-breaker routing for a --fanout pool (opt-in): each
    /// backend carries an EWMA of failure outcomes and call latency; a
    /// backend whose error EMA crosses the threshold sheds its hashed share
    /// to the next healthy peer instead of failing every task routed to it.
    /// Recovery is organic — the EMA decays as probes succeed, and when all
    /// backends are broken the hash-chosen one takes a probe anyway. Without
    /// this flag the pool keeps the pure sticky hash route unchanged.
    #[arg(long, default_value_t = false, action = clap::ArgAction::Set)]
    fanout_health: bool,
    /// OpenAI-compatible base URL; /chat/completions is appended.
    #[arg(long, default_value = DEFAULT_BASE_URL)]
    base_url: String,
    /// Model identifier expected by your local server.
    #[arg(long, default_value = DEFAULT_MODEL)]
    model: String,
    /// Optional bearer token. OPENAI_API_KEY is used as a fallback.
    #[arg(long)]
    api_key: Option<String>,
    /// Ask the server to include exact token usage in its streaming response.
    /// On by default so reports carry real numbers instead of character
    /// estimates; disable with `--stream-usage false`.
    #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
    stream_usage: bool,
    /// Reasoning effort for reasoning models. On muse-spark-1.3 "minimal" is
    /// the measured best for workers: vs "low", wave -53%, output tokens
    /// -40%, reasoning -70%, TTFT -69% at an unchanged own-test pass rate
    /// (ABBA run). "none" disables thinking where the server supports it
    /// (deepseek-flash: 0 reasoning tokens; Muse rejects it). Omit to let the
    /// server default.
    #[arg(long, value_parser = ["none", "minimal", "low", "medium", "high"])]
    reasoning_effort: Option<String>,
    /// Record every provider request and response (errors included) to this
    /// JSONL file. Replay the recording later with --replay-from for
    /// zero-token, deterministic reruns of the same run.
    #[arg(long)]
    record_to: Option<PathBuf>,
    /// Serve provider responses from a recording made by --record-to instead
    /// of calling the API. No API key is needed. Unmatched requests fail the
    /// task; the run is only as complete as the recording.
    #[arg(long, conflicts_with = "record_to")]
    replay_from: Option<PathBuf>,
}

#[derive(Args, Clone)]
struct SchedulerArgs {
    /// Maximum model requests in flight at once.
    #[arg(long, default_value_t = 128)]
    max_concurrency: usize,
    /// Repair-wave attempts run one reasoning level higher than the task's
    /// first attempt (minimal -> low ...): reasoning is spent only where a
    /// validator proved the cheap attempt wrong.
    #[arg(long)]
    escalate_on_repair: bool,
    /// Retries per task after the initial attempt.
    #[arg(long, default_value_t = 2)]
    max_retries: u32,
    /// Initial exponential retry backoff in milliseconds.
    #[arg(long, default_value_t = 250)]
    retry_base_delay_ms: u64,
    /// Timeout for one complete streaming request, in seconds.
    #[arg(long, default_value_t = 300)]
    task_timeout_secs: u64,
    /// Character budget for each prerequisite output embedded in a prompt.
    #[arg(long, default_value_t = 12_000)]
    max_dependency_chars: usize,
    /// Worker temperature when a task does not override it.
    #[arg(long, default_value_t = 0.2)]
    temperature: f32,
    /// Adapt the in-flight cap with an AIMD controller (PLAN.md Phase 2).
    /// --max-concurrency stays on as the hard safety ceiling. Enabled by
    /// default: cold bursts make Muse-class servers shed load with empty
    /// completions; the controller ramps width up from a small cold start.
    #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
    adaptive: bool,
    /// Dynamic concurrency floor for the adaptive controller.
    #[arg(long, default_value_t = 4)]
    controller_min: usize,
    /// Slots added per healthy controller window.
    #[arg(long, default_value_t = 4)]
    controller_step: usize,
    /// Percentage of the limit kept after a controller decrease (1-99).
    #[arg(long, default_value_t = 60)]
    controller_decrease_pct: u32,
    /// Controller evaluation window in milliseconds.
    #[arg(long, default_value_t = 2_000)]
    controller_window_ms: u64,
    /// Percentage of the ceiling the adaptive limit starts at (1-100).
    /// 25 is conservative; 50 matches the sweep-measured knee for
    /// Muse-class servers.
    #[arg(long, default_value_t = 25)]
    controller_start_pct: u32,
    /// Absolute TTFT ceiling in milliseconds: a controller window whose
    /// median TTFT exceeds this is overload regardless of the EMA baseline
    /// (which can seed already-degraded under a burst). 0 disables.
    #[arg(long, default_value_t = 12_000)]
    controller_max_ttft_ms: u64,
    /// Cold-start admission burst cap (tasks admitted before the first
    /// controller window). Raise for small zero-dependency waves.
    #[arg(long, default_value_t = slimtoken::controller::DEFAULT_COLD_START_BURST_CAP)]
    controller_burst: usize,
    /// Task-count-aware cold start: with the adaptive controller, the
    /// admission limit seeds at the number of tasks ready at wave start,
    /// capped here (default 16, the measured fast-and-safe start: no
    /// provider knee to c=48, adaptive plateau at peak 16-20). 0 restores the
    /// legacy burst-share start.
    #[arg(long, default_value_t = 16)]
    controller_hint_cap: usize,
    /// Fast ramp: until the first overload signal, the adaptive
    /// controller's additive step doubles after each healthy window (capped at
    /// 16), reaching the measured no-knee region in 2-3 windows instead of 8+.
    /// The first overload reverts it permanently to the fixed step. Default on
    /// (A/B: -24.4% wall Muse / -11.9% DeepSeek on 49-task healthy waves);
    /// pass false to restore the legacy fixed-step climb.
    #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
    controller_fast_ramp: bool,
    /// File spliced into the shared worker prefix as cross-session context
    /// (project conventions, API docs, style guide). Deterministic content
    /// yields cross-session prefix-cache hits: every run embedding the same
    /// file shares one server-side prefill.
    #[arg(long, value_name = "PATH")]
    shared_context_file: Option<PathBuf>,
    /// Stable prefix-cache routing key forwarded to the server (Muse
    /// `prompt_cache_key`). One key across runs and sessions pins all
    /// requests to the same cache shard, maximizing cross-session reuse.
    #[arg(long, value_name = "KEY")]
    prompt_cache_key: Option<String>,
    /// Cap request starts per minute (token bucket; burst of rpm/12, then
    /// steady 60/rpm seconds between starts). Distinct from --max-concurrency
    /// (in-flight cap). Use against server-side rate walls such as Muse's
    /// 150 RPM team cap: an unpaced burst can fast-fail wholesale and spend
    /// a minute in Retry-After backoff.
    #[arg(long, value_name = "RPM")]
    rpm: Option<u32>,
    /// Cap ESTIMATED token spend per minute (TPM pacing): a leaky
    /// bucket that reserves rendered-input (chars/4) + max_tokens per
    /// admission and refunds the unused output reserve on completion, so the
    /// bucket tracks estimated input + actual output. Distinct from --rpm
    /// (request starts). Use when the provider enforces a token-per-minute
    /// quota. Off by default.
    #[arg(long, value_name = "TOKENS")]
    tpm: Option<u64>,
    /// Auto-tune the --rpm/--tpm pace caps from typed 429 observations
    /// each server rate-limit shrinks the effective pace toward the
    /// provider's true limit (Retry-After hints set the recovery cooldown;
    /// the configured cap stays the ceiling), and sustained health grows it
    /// back. Rate-limited attempts also stop counting as concurrency-
    /// controller failures — a 429 is a rate signal, not a concurrency
    /// signal; cutting concurrency is the wrong lever for a rate wall.
    /// Meaningful with --rpm/--tpm; off by default.
    #[arg(long)]
    rate_auto_tune: bool,
    /// Rate-aware retry gating (follow-up, opt-in): a retry
    /// triggered by a typed 429 waits for a token from a shared retry gate
    /// paced at the current effective start rate (tuner-scaled when
    /// --rate-auto-tune is on), plus the server's Retry-After hint, instead
    /// of its own fixed backoff clock. Retries are extra starts the provider
    /// sees; gating them behind the pace bucket keeps a burst of 429s from
    /// re-hammering a rate wall. Non-429 retries keep the backoff ladder.
    /// Requires --rpm; off by default.
    #[arg(long)]
    rate_retry_gating: bool,
    /// Max_tokens right-sizing profile: JSON file, e.g.
    /// {"default_cap":768,"caps":[{"title_contains":"integration",
    /// "max_tokens":2048}]}. Caps every plan task's output budget at plan
    /// load — the TPM reserve, the priority queue and repair-budget growth all
    /// see the clamped value. Trace evidence: 89.4% of observed task outputs
    /// fit in 2048 tokens. A cap never raises a plan-authored budget.
    /// Meaningful with --tpm; off by default.
    #[arg(long, value_name = "PATH")]
    max_tokens_profile: Option<PathBuf>,
    /// Duration-prediction profile: JSON file, e.g.
    /// {"default_ms":3000,"entries":[{"title_contains":"test shard",
    /// "predicted_ms":18000}]}. Within one critical-path depth the ready
    /// queue admits longest-predicted-first (by these durations) instead of
    /// largest-budget-first, so the wave's longest tasks start earliest.
    /// Trace basis: same-budget tasks span ~100x in duration. Off by default.
    #[arg(long, value_name = "PATH")]
    duration_profile: Option<PathBuf>,
    /// Concurrency auto-probe: fire a ladder of tiny 1-token waves
    /// ([1,2,4,8,16,32] parallel requests) at wave start and detect the
    /// provider's knee — the width where TTFT regresses >2x baseline, wall
    /// time saturates, or requests start failing. The detected width seeds
    /// the adaptive controller's cold-start caps for this run; the AIMD
    /// rails still guard the climb. For providers with a rate/TTFT knee
    /// (the two benchmark providers have no knee to c=48, so this
    /// is for hosts on unknown providers). Each ladder step fires twice and
    /// keeps the min — a single sample cannot separate a knee from jitter.
    /// Costs ~126 1-token requests once per run. Off by default.
    #[arg(long, action = clap::ArgAction::SetTrue)]
    concurrency_probe: bool,
    /// Shell command piped each `validate_artifact` task's output; exit 0
    /// accepts, non-zero rejects with stderr as the failure reason (which
    /// the repair wave hands back as context). E.g. a Python syntax check.
    /// Windows: run via cmd /C; unix: sh -c.
    #[arg(long, value_name = "CMD")]
    validate_cmd: Option<String>,
    /// Shell command piped the ASSEMBLED deliverable after the wave (e.g. a
    /// script that materializes the files and runs pytest); exit 0 passes.
    /// On failure, artifacts named in its report are repaired once under
    /// contract-arbitrated rules and the round is kept only if no more
    /// tests fail (see `reconcile`). Summary saved as reconcile.json.
    #[arg(long, value_name = "CMD")]
    test_cmd: Option<String>,
    /// Cache-warm gate: after cache primes complete, canary readers poll the
    /// server's cached-token report until every primed prefix reads back warm
    /// before the wave is admitted. Muse-class servers take seconds to make a
    /// finished prime visible; workers admitted inside that window prefill
    /// cold despite the prime. Disabling removes the wait (and the warmth).
    #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
    cache_warm_gate: bool,
    /// JSONL trace of wave events for host observability: run start,
    /// task admission/completion (tokens, TTFT, cache), retries with delay
    /// and error, repairs, cache-prime phase, run end. One JSON object per
    /// line, elapsed ms since wave start. Zero token cost; off by default.
    #[arg(long, value_name = "PATH")]
    trace_out: Option<PathBuf>,
    /// Character budget for the failure text embedded in a repair prompt
    /// (repair-context diet). Validator stderr can run to tens of KB,
    /// and the repair prompt's tail is the only part the server bills as
    /// fresh input — the byte-prefix stays cache-served. The cut keeps the
    /// error's tail (assertions and tracebacks live at the end).
    #[arg(long, default_value_t = 2000)]
    repair_error_chars: usize,
    /// Cap on in-flight in-run repairs. Uncapped, every failure
    /// re-admits its repair immediately and bypasses the admission
    /// ceiling, so a failure storm re-floods the provider at once. Capped,
    /// repairs queue FIFO and start only from slots fresh tasks did not
    /// take, while still firing immediately when the wave goes idle (the
    /// barrier case). Off by default (legacy behavior).
    #[arg(long, value_name = "N")]
    max_concurrent_repairs: Option<usize>,
    /// Patience bound for capped repairs in milliseconds. The capped
    /// drain normally yields every slot to fresh tasks, which under a
    /// saturated wave defers repairs to wave end (measured p50 wait ~4.2s on
    /// the all-fail app; paid +47% wall there by design). Once a queued
    /// repair has waited this long it starts from slots fresh tasks would
    /// claim — still bounded by both caps, so injection preempts at most
    /// max-concurrent-repairs slots. Requires --max-concurrent-repairs.
    #[arg(long, value_name = "MS")]
    repair_inject_after_ms: Option<u64>,
}

#[derive(Args, Clone)]
struct AbArgs {
    /// Two JSON arm configs, e.g. {"reasoning_effort":"low","max_tokens":384}.
    /// Keys: reasoning_effort, adaptive, max_concurrency, max_tokens, temperature.
    #[arg(long, num_args = 2, value_names = ["ARM_A_JSON", "ARM_B_JSON"])]
    arms: Vec<PathBuf>,
    /// Paired rounds; arm order alternates each round to cancel server drift.
    #[arg(long, default_value_t = 5)]
    rounds: usize,
    /// Independent requests per arm per round.
    #[arg(long, default_value_t = 48)]
    requests: usize,
    /// Default output token budget per request when an arm does not set one.
    #[arg(long, default_value_t = 384)]
    max_tokens: u32,
    /// Base prompt; a unique variant is appended per request.
    #[arg(
        long,
        default_value = "Write one concise paragraph explaining how a task scheduler works."
    )]
    prompt: String,
    /// File spliced into the shared worker prefix; same semantics as
    /// `run --shared-context-file`. Applied to both arms.
    #[arg(long, value_name = "PATH")]
    shared_context_file: Option<PathBuf>,
    /// Prefix-cache routing key; defaults to a stable "slimtoken-ab" so every
    /// ab invocation shares one cache shard across sessions.
    #[arg(long, value_name = "KEY")]
    prompt_cache_key: Option<String>,
    /// Cap request starts per minute for BOTH arms (token bucket; burst of
    /// rpm/12, then steady 60/rpm seconds between starts). Use against
    /// server-side rate walls such as Muse's 150 RPM team cap.
    #[arg(long, value_name = "RPM")]
    rpm: Option<u32>,
    #[command(flatten)]
    client: ClientArgs,
}

#[derive(Args, Clone)]
struct RuntimeArgs {
    /// Root directory for per-run plans and reports.
    #[arg(long, default_value = "slimtoken-runs")]
    output: PathBuf,
    /// Use the deterministic planner and worker; no model server needed.
    #[arg(long)]
    mock: bool,
    /// Simulated latency for each mock worker.
    #[arg(long, default_value_t = 30)]
    mock_latency_ms: u64,
    #[command(flatten)]
    client: ClientArgs,
    #[command(flatten)]
    scheduler: SchedulerArgs,
}

#[derive(Args)]
#[cfg(feature = "serve")]
struct ServeArgs {
    /// Address for the optional REST API. Loopback is the safe default.
    #[arg(long, default_value = "127.0.0.1:3000")]
    listen: SocketAddr,
    #[command(flatten)]
    runtime: RuntimeArgs,
}

impl SchedulerArgs {
    fn config(&self, reasoning_effort: Option<String>) -> Result<SchedulerConfig> {
        let shared_context_extra = match &self.shared_context_file {
            Some(path) => Some(
                std::fs::read_to_string(path)
                    .with_context(|| format!("failed to read {}", path.display()))?,
            ),
            None => None,
        };
        Ok(SchedulerConfig {
            max_concurrency: self.max_concurrency,
            max_retries: self.max_retries,
            retry_base_delay: Duration::from_millis(self.retry_base_delay_ms),
            task_timeout: Duration::from_secs(self.task_timeout_secs),
            max_dependency_chars: self.max_dependency_chars,
            default_temperature: self.temperature,
            adaptive: self.adaptive,
            controller: ControllerConfig {
                min_limit: self.controller_min,
                additive_step: self.controller_step,
                decrease_percent: self.controller_decrease_pct,
                window: Duration::from_millis(self.controller_window_ms),
                cooldown_windows: 2,
                max_events_per_window: 16,
                start_percent: self.controller_start_pct,
                max_ttft_ms: self.controller_max_ttft_ms,
                cold_burst_cap: self.controller_burst,
                cold_hint_cap: self.controller_hint_cap,
                fast_ramp: self.controller_fast_ramp,
            },
            reasoning_effort,
            escalate_on_repair: self.escalate_on_repair,
            shared_context_extra,
            prompt_cache_key: self.prompt_cache_key.clone(),
            max_starts_per_minute: self.rpm,
            max_tokens_per_minute: self.tpm,
            rate_auto_tune: self.rate_auto_tune,
            rate_retry_gating: self.rate_retry_gating,
            max_tokens_profile: self
                .max_tokens_profile
                .as_deref()
                .map(MaxTokensProfile::load_file)
                .transpose()?,
            duration_profile: self
                .duration_profile
                .as_deref()
                .map(DurationProfile::load_file)
                .transpose()?,
            concurrency_probe: self.concurrency_probe.then(ProbeConfig::default),
            validator: self.validate_cmd.clone().map(|command| {
                slimtoken::scheduler::SharedValidator(Arc::new(
                    slimtoken::scheduler::CommandValidator::new(command),
                ))
            }),
            cache_warm_gate: self.cache_warm_gate,
            cache_warm_poll: Duration::from_secs(1),
            cache_warm_budget: Duration::from_secs(12),
            trace_path: self.trace_out.clone(),
            repair_error_chars: Some(self.repair_error_chars),
            max_concurrent_repairs: self.max_concurrent_repairs,
            repair_inject_after_ms: self.repair_inject_after_ms,
            // Planning accounting is stamped by `run` after the planning
            // phase; the args-level builder leaves it unset.
            planning: None,
        })
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("slimtoken=info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_writer(std::io::stderr)
        .init();

    match Cli::parse().command {
        Commands::Run(args) => run(*args).await,
        Commands::Interactive(args) => {
            let manager = make_manager(&args)?;
            slimtoken::interactive::run(manager).await
        }
        #[cfg(feature = "serve")]
        Commands::Serve(args) => serve(args).await,
        Commands::Bench(args) => bench(args).await,
        Commands::Ab(args) => ab(args).await,
        Commands::Validate { plan } => {
            let plan = load_plan(&plan).await?;
            println!("Valid plan: {} tasks", plan.tasks.len());
            Ok(())
        }
    }
}

/// `--max-tasks` default (general planner); architect builds cap at 12
/// unless the flag is set explicitly.
const DEFAULT_MAX_TASKS: usize = 48;
const DEFAULT_BASE_URL: &str = "http://127.0.0.1:8080/v1";
const DEFAULT_MODEL: &str = "local-model";

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
enum Wiring {
    Off,
    Plan,
    Wave,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
enum Route {
    Auto,
    Direct,
    Wave,
}

/// A provider's measured defaults (see `--provider`). The presets live in
/// the library (`slimtoken::provider`) so host harnesses plan on the same
/// tuned path as the CLI ; the CLI only applies them to its flags.
use slimtoken::provider::provider_preset;

/// Fill the client flags a preset covers, leaving explicit values alone.
fn apply_client_preset(client: &mut ClientArgs) -> Option<slimtoken::provider::ProviderPreset> {
    let preset = provider_preset(client.provider.as_deref()?)?;
    if client.base_url == DEFAULT_BASE_URL {
        client.base_url = preset.base_url.to_string();
    }
    if client.model == DEFAULT_MODEL {
        client.model = preset.model.to_string();
    }
    if client.reasoning_effort.is_none() {
        client.reasoning_effort = Some(preset.worker_effort.to_string());
    }
    if client.api_key.is_none() {
        client.api_key = preset.api_key_from_env().ok();
    }
    Some(preset)
}

/// Bump when profile rules change enough that cached manifests should be
/// rebuilt rather than replayed.
const MANIFEST_CACHE_VERSION: u32 = 2;

/// Content key for `--manifest-cache`: the objective plus every flag that
/// shapes the MANIFEST. v2: plan-shaping flags are excluded — they are
/// `PlanOptions` (applied at plan build, never sent to the architect), so
/// hashing them caused false misses (a density/quota/tests-see-code flip on a
/// fixed objective re-planned for zero reason). Verified against
/// `ArchitectOptions`/`PlanOptions` at the call site: only fields that reach
/// `generate_manifest` are hashed. Two edges matter:
/// - `--architect-single-shot` changes the generation path itself (steering
///   enters the single-shot prompt but never the split path's), so it is
///   hashed, and steering is hashed only when single-shot is on.
/// - `Wiring::Plan` wires at manifest time (`split_manifest` calls
///   `wire_manifest`); `Off`/`Wave` never touch the manifest, so only the
///   Plan case is hashed. Provider/model stay excluded on purpose — a
///   manifest is provider-agnostic by design.
fn manifest_cache_key(objective: &str, args: &RunArgs, max_tasks: usize) -> String {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    MANIFEST_CACHE_VERSION.hash(&mut hasher);
    objective.hash(&mut hasher);
    max_tasks.hash(&mut hasher);
    args.architect_single_shot.hash(&mut hasher);
    args.architect_edges.hash(&mut hasher);
    args.architect_examples.hash(&mut hasher);
    args.architect_files_per_fragment.hash(&mut hasher);
    args.architect_objective_skeleton.hash(&mut hasher);
    args.architect_schema.hash(&mut hasher);
    args.architect_type_ownership.hash(&mut hasher);
    args.architect_hedge.hash(&mut hasher);
    args.architect_effort.hash(&mut hasher);
    (args.architect_wiring == Wiring::Plan).hash(&mut hasher);
    args.profile.0.as_deref().hash(&mut hasher);
    // a file profile shapes the plan like a named one; hash its CONTENT
    // so an edited file invalidates the cache without a name change.
    if let Some(path) = &args.profile_file {
        match std::fs::read(path) {
            Ok(bytes) => bytes.hash(&mut hasher),
            Err(_) => path.hash(&mut hasher),
        }
    }
    // same for the library — every JSON file's name+content, in sorted
    // order so the hash is independent of how the dir is enumerated.
    if let Some(dir) = &args.profile_dir {
        let mut entries: Vec<_> = std::fs::read_dir(dir)
            .ok()
            .into_iter()
            .flatten()
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
            .collect();
        entries.sort();
        for path in entries {
            path.hash(&mut hasher);
            match std::fs::read(&path) {
                Ok(bytes) => bytes.hash(&mut hasher),
                Err(_) => path.hash(&mut hasher),
            }
        }
    }
    // Steering reaches the architect only on the single-shot path; the split
    // path's prompts (skeleton/exports/wiring) never embed it.
    if args.architect_single_shot {
        args.steering.hash(&mut hasher);
    }
    format!("{:016x}", hasher.finish())
}

/// Manifest from the content cache when present, else generate and store.
/// A cache hit skips every architect call: planning wall and planning
/// tokens both drop to zero with a byte-identical manifest.
async fn manifest_from_cache_or_generate(
    cache_file: Option<&std::path::Path>,
    client: &dyn slimtoken::client::ModelClient,
    objective: &str,
    max_tasks: usize,
    steering: &[String],
    options: &ArchitectOptions,
) -> Result<slimtoken::architect::Manifest> {
    if let Some(path) = cache_file {
        if let Ok(text) = std::fs::read_to_string(path) {
            info!(path = %path.display(), "manifest cache hit");
            return serde_json::from_str(&text)
                .with_context(|| format!("invalid cached manifest {}", path.display()));
        }
    }
    let manifest = generate_manifest(client, objective, max_tasks, steering, options).await?;
    if let Some(path) = cache_file {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        std::fs::write(path, serde_json::to_string_pretty(&manifest)?)
            .with_context(|| format!("failed to write {}", path.display()))?;
    }
    Ok(manifest)
}

async fn run(mut args: RunArgs) -> Result<()> {
    // fan-out: the first listed preset plays the planner role (schema
    // and effort are provider-specific), so route all preset-derived flags
    // through it. Explicit --provider with --fanout is ambiguous: bail.
    if args.client.fanout_health && args.client.fanout.is_none() {
        bail!("--fanout-health needs --fanout; there is no pool to route");
    }
    if let Some(names) = args.client.fanout.clone() {
        if args.client.provider.is_some() {
            bail!("--provider conflicts with --fanout; the first --fanout entry is the planner provider");
        }
        if args.mock {
            bail!("--fanout needs real providers; it conflicts with --mock");
        }
        if args.client.replay_from.is_some() {
            bail!("--fanout conflicts with --replay-from; a recording replays one provider's responses");
        }
        let first = names.first().map(String::as_str).unwrap_or("");
        if provider_preset(first).is_none() {
            bail!("unknown --fanout preset '{first}'; known: muse, deepseek");
        }
        args.client.provider = Some(first.to_string());
    }
    if let Some(preset) = apply_client_preset(&mut args.client) {
        if args.architect_effort == "minimal" {
            args.architect_effort = preset.architect_effort.to_string();
        }
        if !preset.architect_schema {
            args.architect_schema = false;
        }
        if args.scheduler.controller_burst == slimtoken::controller::DEFAULT_COLD_START_BURST_CAP {
            args.scheduler.controller_burst = preset.cold_burst_cap;
        }
    }
    if args.client.fanout.is_some() {
        // The pool stamps each backend's own measured effort onto requests
        // that pin none (per-provider vocabulary: Muse wants "minimal",
        // DeepSeek wants "none"). A global effort would ride to every backend
        // and defeat that, so fan-out runs send worker requests effort-less.
        args.client.reasoning_effort = None;
    }
    let single_response_threshold = provider_preset(args.client.provider.as_deref().unwrap_or(""))
        .map_or(
            slimtoken::architect::SINGLE_RESPONSE_THRESHOLD_TOKENS,
            |preset| preset.single_response_threshold,
        );
    let control = RunControl::with_steering(args.steering.clone())?;
    let planner_client = make_client(&args.client, args.mock, args.mock_latency_ms)?;
    // the worker wave may fan out over several presets while planning
    // stays on the first one. FanOutClient is a ModelClient, so the scheduler
    // is unchanged; the backend label reaches report.json per task.
    let wave_client: Arc<dyn ModelClient> = match &args.client.fanout {
        Some(names) => {
            let mut backends = Vec::new();
            for name in names {
                let preset = provider_preset(name.as_str())
                    .with_context(|| format!("unknown --fanout preset '{name}'"))?;
                let key = preset
                    .api_key_from_env()
                    .map_err(|message| anyhow::anyhow!(message))?;
                let client: Arc<dyn ModelClient> = Arc::new(
                    OpenAiClient::new(preset.base_url, preset.model, Some(key))?
                        .with_stream_usage(args.client.stream_usage),
                );
                backends.push(slimtoken::FanOutBackend::new(
                    preset.name,
                    client,
                    Some(preset.worker_effort.to_string()),
                ));
            }
            let pool = slimtoken::FanOutClient::new(backends)?;
            // opt-in circuit-breaker routing over the sticky hash route.
            let pool = if args.client.fanout_health {
                pool.with_health_routing(slimtoken::HealthRoutingConfig::default())
            } else {
                pool
            };
            Arc::new(pool)
        }
        None => planner_client.clone(),
    };
    // / an explicit profile file wins; a named --profile resolves
    // against the file library (--profile-dir) plus built-ins; auto detects.
    // The resolved profile is 'static (leased by Profile::load_file), so
    // everything downstream keeps taking Option<&'static Profile>.
    if let Some(dir) = &args.profile_dir {
        let loaded = Profile::load_dir(dir)
            .with_context(|| format!("failed to load profile directory {}", dir.display()))?;
        info!(dir = %dir.display(), loaded, "profile library loaded");
    }
    let profile: Option<&'static Profile> = match &args.profile_file {
        Some(path) => {
            if args.profile.0.is_some() {
                bail!("--profile and --profile-file conflict; pass only one");
            }
            Some(
                Profile::load_file(path)
                    .with_context(|| format!("failed to load profile file {}", path.display()))?,
            )
        }
        None => match &args.profile.0 {
            Some(name) => {
                let known: Vec<_> = slimtoken::profile::PROFILES
                    .iter()
                    .map(|p| p.name)
                    .collect();
                Some(Profile::named(name).ok_or_else(|| {
                    anyhow::anyhow!(
                        "unknown profile {name:?} (auto, {}{})",
                        known.join(", "),
                        if args.profile_dir.is_some() {
                            ", plus --profile-dir library names"
                        } else {
                            ""
                        }
                    )
                })?)
            }
            None => None,
        },
    };
    // count planning-phase billing through a decorating client so the
    // report and trace can attribute it — planning paths nest many calls
    // (fragment ladders, salvage, hedged races, wiring) and wave totals alone
    // read 0% overhead (gap). The wave client stays unwrapped: wave
    // tokens are already per-task in the report.
    let planning_usage =
        std::sync::Arc::new(std::sync::Mutex::new(slimtoken::PlanningUsage::default()));
    let planning_client: Arc<dyn slimtoken::ModelClient> = Arc::new(
        slimtoken::UsageCountingClient::new(planner_client.clone(), planning_usage.clone()),
    );
    let planning_start = std::time::Instant::now();
    let mut plan = if let Some(path) = &args.plan {
        load_plan(path).await?
    } else {
        let objective = args
            .objective
            .as_deref()
            .context("provide --objective or --plan")?;
        if args.mock {
            mock_plan(objective)
        } else if args.architect {
            // Tighter budget than the general planner: a greenfield build
            // should be ~7 one-file workers, not 48. Larger requests are
            // clamped because single-wave greenfield fan-out degrades past
            // ~12 workers anyway (TTFT under load grows with width).
            // An explicit --max-tasks wins: wide-capable servers (DeepSeek
            // ran 15-wide waves fastest, ABBA run) need larger builds.
            let max_tasks = if args.max_tasks == DEFAULT_MAX_TASKS {
                12
            } else {
                args.max_tasks
            };
            // a cached manifest makes the wave's planning ~free, and
            // the wave's deliverable is verified (own tests + acceptance)
            // where the direct call's is not (direct arms escape the
            // chain and under-delivered 22/24). On a cache hit the auto
            // router prefers the wave even under the direct threshold.
            // Explicit --route direct/wave still wins.
            let cache_hit = args.manifest_cache.as_ref().is_some_and(|dir| {
                dir.join(format!(
                    "{}.json",
                    manifest_cache_key(objective, &args, max_tasks)
                ))
                .is_file()
            });
            // Size router: small deliverables go out as one direct call;
            // oversized ones get the manifest + parallel wave.
            let direct = match args.route {
                Route::Auto => {
                    !cache_hit
                        && slimtoken::architect::estimate_single_response_tokens(objective)
                            <= single_response_threshold
                }
                Route::Direct => true,
                Route::Wave => false,
            };
            if direct {
                slimtoken::architect::direct_response_plan(objective)
            } else {
                let manifest = match &args.manifest {
                    Some(path) => {
                        let text = tokio::fs::read_to_string(path)
                            .await
                            .with_context(|| format!("failed to read {}", path.display()))?;
                        serde_json::from_str(&text)
                            .with_context(|| format!("invalid manifest {}", path.display()))?
                    }
                    None => {
                        let options = ArchitectOptions {
                            force_split: !args.architect_single_shot,
                            cache_key: args.scheduler.prompt_cache_key.clone(),
                            hedge: args.architect_hedge.clamp(1, 4),
                            schema: args.architect_schema,
                            fragment_effort: args.architect_effort.clone(),
                            edges: args.architect_edges,
                            examples: args.architect_examples,
                            files_per_fragment: args.architect_files_per_fragment,
                            objective_skeleton: args.architect_objective_skeleton,
                            profile,
                            wiring: args.architect_wiring == Wiring::Plan,
                            type_ownership: args.architect_type_ownership,
                        };
                        let cache_file = args.manifest_cache.as_ref().map(|dir| {
                            dir.join(format!(
                                "{}.json",
                                manifest_cache_key(objective, &args, max_tasks)
                            ))
                        });
                        manifest_from_cache_or_generate(
                            cache_file.as_deref(),
                            planning_client.as_ref(),
                            objective,
                            max_tasks,
                            &args.steering,
                            &options,
                        )
                        .await?
                    }
                };
                tokio::fs::create_dir_all(&args.output)
                    .await
                    .with_context(|| format!("failed to create {}", args.output.display()))?;
                write_json(&args.output.join("manifest.json"), &manifest).await?;
                plan_from_manifest(
                    objective,
                    &manifest,
                    max_tasks,
                    profile,
                    &slimtoken::architect::PlanOptions {
                        tests_see_code: args.architect_tests_see_code,
                        wave_wiring: args.architect_wiring == Wiring::Wave,
                        density: args.architect_density,
                        quota_pct: args.architect_test_quota,
                        scope_contracts: args.architect_scope_contracts,
                    },
                )?
            }
        } else {
            generate_plan_with_steering(
                planning_client.as_ref(),
                objective,
                args.max_tasks,
                &args.steering,
            )
            .await?
        }
    };
    plan.validate().context("invalid execution plan")?;
    let planning_ms = planning_start.elapsed().as_millis() as u64;
    // planning-phase billing counted at the client seam. Zero calls is
    // the truthful accounting for a --plan load, a mock run, or a manifest
    // cache hit (planning happened, no model calls).
    let counted_planning = planning_usage.lock().expect("planning usage").clone();

    tokio::fs::create_dir_all(&args.output)
        .await
        .with_context(|| format!("failed to create {}", args.output.display()))?;
    let plan_path = args.output.join("plan.json");
    write_json(&plan_path, &plan).await?;
    info!(tasks = plan.tasks.len(), path = %plan_path.display(), "plan saved");

    if args.plan_only {
        println!("Plan saved to {}", plan_path.display());
        return Ok(());
    }

    // Harness-level final integration: append a sink task that depends on
    // every leaf, so the user gets ONE consolidated deliverable instead of
    // split per-task fragments. Planners often emit a fan-out DAG whose own
    // "integration" task has no dependencies and therefore integrates
    // nothing; wiring the sink here makes aggregation unconditional. The
    // "assemble" sink skips the model call entirely and lets
    // build_final_output concatenate leaf outputs deterministically.
    let integration_id = if args.integrate && args.sink == "model" && !plan.tasks.is_empty() {
        let ids = append_integration_task(&mut plan, args.sink_shards);
        write_json(&plan_path, &plan).await?;
        Some(ids)
    } else {
        None
    };

    let mut config = args
        .scheduler
        .config(args.client.reasoning_effort.clone())?;
    config.planning = Some(slimtoken::PlanningSnapshot {
        planning_ms,
        calls: counted_planning.calls,
        input_tokens: counted_planning.input_tokens,
        output_tokens: counted_planning.output_tokens,
        cached_input_tokens: counted_planning.cached_input_tokens,
    });

    let test_cmd = args.scheduler.test_cmd.clone();
    let mut report =
        execute_plan_with_control(&plan, wave_client.clone(), config.clone(), control).await?;
    // phase attribution: the wave duration is the scheduler's own
    // elapsed; planning (manifest + plan build, measured above) and reconcile
    // happen outside it and are stamped here.
    report.phases.planning_ms = Some(planning_ms);
    // planning-phase tokens live in the phases, outside the wave
    // totals (which cover wave tasks only) — overhead ratios add both.
    report.phases.planning_input_tokens = Some(counted_planning.input_tokens);
    report.phases.planning_output_tokens = Some(counted_planning.output_tokens);
    if let Some(command) = &test_cmd {
        let reconcile_start = std::time::Instant::now();
        let summary = slimtoken::reconcile::reconcile_with(
            &plan,
            &mut report,
            wave_client,
            config,
            command,
            |report| build_final_output(report, &plan, integration_id.as_deref().unwrap_or(&[])),
            args.reconcile_edits,
        )
        .await?;
        report.phases.reconcile_ms = Some(reconcile_start.elapsed().as_millis() as u64);
        write_json(&args.output.join("reconcile.json"), &summary).await?;
    }
    let report_path = args.output.join("report.json");
    write_json(&report_path, &report).await?;
    print_report(&report);
    println!("Report saved to {}", report_path.display());

    let final_path = args.output.join("final-output.md");
    let final_doc = build_final_output(&report, &plan, integration_id.as_deref().unwrap_or(&[]));
    std::fs::write(&final_path, &final_doc)
        .with_context(|| format!("failed to write {}", final_path.display()))?;
    println!("Final output saved to {}", final_path.display());

    if !report.is_success() {
        bail!("run completed with failed or skipped tasks; inspect report.json");
    }
    Ok(())
}

/// Appends sink integration tasks depending on every leaf of `plan`.
/// `shards == 1` appends the legacy single sink and returns its id; `shards
/// > 1` partitions the leaves into that many weight-balanced (LPT over
/// max_tokens) groups and appends one shard task per group, returning the
/// shard ids in shard order. The final document is the concatenation of the
/// shard fragments in that order (see `build_final_output`).
fn append_integration_task(plan: &mut Plan, shards: usize) -> Vec<String> {
    const LEGACY_ID: &str = "slimtoken-final-integration";
    if plan.tasks.iter().any(|task| task.id == LEGACY_ID) || shards == 0 {
        return vec![LEGACY_ID.to_string()];
    }
    let dependents: std::collections::HashSet<&str> = plan
        .tasks
        .iter()
        .flat_map(|task| task.deps.iter().map(String::as_str))
        .collect();
    let leaves: Vec<&Task> = plan
        .tasks
        .iter()
        .filter(|task| !dependents.contains(task.id.as_str()))
        .collect();
    if leaves.len() < 2 || shards < 2 {
        if plan.tasks.iter().any(|task| task.id == LEGACY_ID) {
            return vec![LEGACY_ID.to_string()];
        }
        let ids: Vec<String> = leaves.iter().map(|task| task.id.clone()).collect();
        plan.tasks.push(Task {
            id: LEGACY_ID.to_string(),
            title: "Final integration".to_string(),
            prompt: integration_prompt(
                "Produce the SINGLE final deliverable",
                "every artifact",
                false,
            ),
            deps: ids,
            // Long unified documents: the sink routinely approaches the output
            // budget, and a truncated aggregate is worse than a longer one.
            max_tokens: 8192,
            temperature: Some(0.2),
            require_closed_fences: true,
            validate_artifact: false,
            static_output: None,
            reasoning_effort: None,
            internal: false,
        });
        return vec![LEGACY_ID.to_string()];
    }
    // LPT partition: heaviest leaf first onto the currently lightest
    // shard, weight = max_tokens (the only size signal available at plan
    // time). Deterministic, disjoint, covers every leaf exactly once.
    let count = shards.min(leaves.len());
    let mut groups: Vec<(u64, Vec<String>)> = (0..count).map(|_| (0, Vec::new())).collect();
    let mut ordered: Vec<&Task> = leaves.clone();
    ordered.sort_by(|a, b| {
        b.max_tokens
            .cmp(&a.max_tokens)
            .then_with(|| a.id.cmp(&b.id))
    });
    for leaf in ordered {
        let lightest = groups
            .iter_mut()
            .min_by_key(|(weight, _)| *weight)
            .expect("at least one group");
        lightest.0 += u64::from(leaf.max_tokens);
        lightest.1.push(leaf.id.clone());
    }
    // Budget per shard: the section must hold its leaves' combined output, so
    // weight (sum of leaf max_tokens) is the floor — the fixed 8192/N split
    // truncated on code-heavy leaves in the live A/B (retry cost > serial
    // savings). Cap stays at the legacy single-sink budget.
    let mut ids = Vec::new();
    for (index, (weight, members)) in groups.iter().enumerate() {
        let id = format!("slimtoken-final-integration-{index:02}");
        let budget = (*weight as u32).clamp(2048, 8192);
        plan.tasks.push(Task {
            id: id.clone(),
            title: format!("Final integration (shard {}/{count})", index + 1),
            prompt: integration_prompt(
                &format!(
                    "You are shard {index} of {count} of the final integration. Produce YOUR \
                     SECTION of the single final deliverable",
                ),
                "exactly the artifacts listed as your dependencies — no others",
                true,
            ),
            deps: members.clone(),
            max_tokens: budget,
            temperature: Some(0.2),
            require_closed_fences: true,
            validate_artifact: false,
            static_output: None,
            reasoning_effort: None,
            internal: false,
        });
        ids.push(id);
    }
    ids
}

/// Shared merge discipline for the sink and its shards. `scope` names the
/// covering instruction (all artifacts vs exactly the dependency subset);
/// both keep the same output shape so shard fragments concatenate cleanly.
/// The shard-only discipline tail lands in shard prompts only, so the legacy
/// single-sink prompt stays byte-identical.
fn integration_prompt(headline: &str, scope: &str, shard: bool) -> String {
    let mut prompt = format!(
        "{headline} for the overall objective: merge {scope} into one cohesive \
         document, resolved to a consistent naming scheme and consistent interfaces. Where \
         outputs disagree (types, file layout, function signatures, behavior), pick the most \
         complete version and fix the others to match. Preserve all code verbatim unless it \
         needs a consistency fix — do not summarize or truncate working code. Structure the \
         result as: (1) the complete runnable program, file by file, each in its own fenced \
         code block with a path header; (2) the test suite; (3) run instructions. No \
         meta-commentary about the process."
    );
    if shard {
        prompt.push_str(
            " Shard discipline: your section covers only your listed artifacts, starts with a \
             `## Part` heading, and must not reference parts outside your scope.",
        );
    }
    prompt
}

/// Builds the single consolidated output document. One sink id: prefers that
/// task's output (legacy). Several (shards): concatenates the shard
/// fragments in shard order, falling back to a failed shard's own leaf
/// outputs so a partial run still covers every group. No usable sink output:
/// concatenates task outputs in plan order so a partial run still leaves one
/// readable file.
fn build_final_output(report: &RunReport, plan: &Plan, sink_ids: &[String]) -> String {
    if sink_ids.len() == 1 {
        if let Some(output) = report
            .tasks
            .get(&sink_ids[0])
            .and_then(|task| task.output.as_deref())
        {
            if !output.trim().is_empty() {
                return output.to_string();
            }
        }
    }
    if sink_ids.len() > 1 {
        let mut doc = String::new();
        for id in sink_ids {
            if let Some(output) = report.tasks.get(id).and_then(|task| task.output.as_deref()) {
                if !output.trim().is_empty() {
                    doc.push_str(output.trim());
                    doc.push_str("\n\n");
                    continue;
                }
            }
            // Failed shard: fall back to its own leaves so its group is not
            // silently missing from the partial document.
            if let Some(shard) = plan.tasks.iter().find(|task| task.id == *id) {
                for dep in &shard.deps {
                    if let Some(output) = report
                        .tasks
                        .get(dep)
                        .and_then(|task| task.output.as_deref())
                    {
                        if !output.trim().is_empty() {
                            doc.push_str(output.trim());
                            doc.push_str("\n\n");
                        }
                    }
                }
            }
        }
        if !doc.trim().is_empty() {
            return dedup_fragment_file_blocks(&doc);
        }
    }
    let mut doc = String::new();
    for task in &plan.tasks {
        let Some(record) = report.tasks.get(&task.id) else {
            continue;
        };
        if record.status != TaskStatus::Succeeded || task.internal {
            continue;
        }
        let Some(output) = &record.output else {
            continue;
        };
        if output.trim().is_empty() {
            continue;
        }
        doc.push_str(&format!("## {}\n\n{}\n\n", task.title, output.trim()));
    }
    doc.trim_end().to_string()
}

/// Shard fragments may restate a file that an earlier fragment already
/// covered (the shard prompt forbids it, but models do it anyway — measured
/// 11 duplicated file headings across 4 fragments on the DeepSeek digest
/// A/B). This pass drops later copies deterministically: a section that
/// starts with a path heading (`### \`src/app.tsx\``) seen before is removed
/// up to the next heading. Fenced code is never scanned for headings, so a
/// `#` inside a code block cannot split a section. Headings that are not
/// paths (## Part, prose) pass through and end any skip in progress. If no
/// duplicate is found the document is returned unchanged apart from trailing
/// whitespace.
fn dedup_fragment_file_blocks(doc: &str) -> String {
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut out = String::with_capacity(doc.len());
    let mut in_fence = false;
    let mut skipping = false;
    for line in doc.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") {
            in_fence = !in_fence;
            if !skipping {
                out.push_str(line);
                out.push('\n');
            }
            continue;
        }
        if !in_fence && trimmed.starts_with('#') {
            skipping = match heading_path(trimmed) {
                Some(path) => !seen.insert(path),
                None => false,
            };
            if !skipping {
                out.push_str(line);
                out.push('\n');
            }
            continue;
        }
        if !skipping {
            out.push_str(line);
            out.push('\n');
        }
    }
    out.trim_end().to_string()
}

/// Extracts the normalized path from a heading like `### \`src/app.tsx\``;
/// non-path headings (## Part 1/4, prose) return None.
fn heading_path(line: &str) -> Option<String> {
    let rest = line.trim_start_matches('#').trim_start();
    let path = rest.strip_prefix('`')?.trim_end().strip_suffix('`')?;
    // A path names a file or directory: require a separator or an extension.
    let is_path = path.contains('/')
        || path.contains('\\')
        || path
            .rsplit('/')
            .next()
            .is_some_and(|name| name.contains('.'));
    is_path.then(|| path.to_lowercase())
}

#[cfg(test)]
mod dedup_tests {
    use super::dedup_fragment_file_blocks;

    #[test]
    fn dedup_drops_later_copies_of_a_file_block() {
        let doc = "## Part 1/2\n\n### `src/app.ts`\n\n```ts\nexport const app = 1;\n```\n\n### `src/util.ts`\n\n```ts\nexport const util = 2;\n```\n\n## Part 2/2\n\n### `src/app.ts`\n\n```ts\nexport const app = 99;\n```\n\n### `src/extra.ts`\n\n```ts\nexport const extra = 3;\n```\n";
        let out = dedup_fragment_file_blocks(doc);
        assert!(out.contains("export const app = 1;"), "first copy kept");
        assert!(
            !out.contains("export const app = 99;"),
            "later copy dropped"
        );
        assert!(out.contains("export const util = 2;"), "other files kept");
        assert!(
            out.contains("export const extra = 3;"),
            "later novel files kept"
        );
        assert!(out.contains("## Part 2/2"), "part headings kept");
        assert_eq!(out.matches("### `src/app.ts`").count(), 1);
    }

    #[test]
    fn dedup_ignores_hash_lines_inside_fences_and_passes_clean_docs() {
        let with_fence_hash =
            "## Part 1/1\n\n### `src/a.ts`\n\n```ts\n# not a heading\nexport const a = 1;\n```\n";
        assert_eq!(
            dedup_fragment_file_blocks(with_fence_hash),
            with_fence_hash.trim_end()
        );
        let clean = "## Part 1/2\n\n### `src/a.ts`\n\n```ts\nexport const a = 1;\n```\n\n## Part 2/2\n\n### `src/b.ts`\n\n```ts\nexport const b = 2;\n```\n";
        assert_eq!(dedup_fragment_file_blocks(clean), clean.trim_end());
    }

    #[test]
    fn dedup_normalizes_path_case() {
        let doc = "### `Src/App.TS`\n\n```ts\nexport const a = 1;\n```\n\n### `src/app.ts`\n\n```ts\nexport const a = 2;\n```\n";
        let out = dedup_fragment_file_blocks(doc);
        assert!(out.contains("export const a = 1;"));
        assert!(!out.contains("export const a = 2;"));
    }
}

async fn bench(args: BenchArgs) -> Result<()> {
    if args.requests == 0 || args.requests > 256 {
        bail!("--requests must be between 1 and 256");
    }
    if args.max_tokens == 0 {
        bail!("--max-tokens must be greater than zero");
    }
    let client = make_client(&args.client, false, 0)?;
    let plan = Plan {
        objective: "Measure successful aggregate model throughput".into(),
        cache_primes: Vec::new(),
        tasks: (0..args.requests)
            .map(|index| Task {
                id: format!("bench-{index:04}"),
                title: format!("Benchmark request {index}"),
                prompt: format!("{}\nRequest variant: {index}", args.prompt),
                deps: Vec::new(),
                max_tokens: args.max_tokens,
                temperature: Some(0.0),
                require_closed_fences: false,
                validate_artifact: false,
                static_output: None,
                reasoning_effort: None,
                internal: false,
            })
            .collect(),
    };

    let report = execute_plan(
        &plan,
        client,
        args.scheduler
            .config(args.client.reasoning_effort.clone())?,
    )
    .await?;
    let summary = json!({
        "requests": report.task_count,
        "succeeded": report.status_count(TaskStatus::Succeeded),
        "failed": report.status_count(TaskStatus::Failed),
        "elapsed_ms": report.elapsed_ms,
        "requested_concurrency": args.scheduler.max_concurrency,
        "peak_concurrency": report.peak_concurrency,
        "input_tokens": report.total_input_tokens,
        "output_tokens": report.total_output_tokens,
        "reasoning_tokens": report.total_reasoning_tokens,
        "cached_input_tokens": report.total_cached_input_tokens,
        "mean_time_to_first_token_ms": report.mean_time_to_first_token_ms,
        "aggregate_output_tokens_per_second": report.aggregate_output_tokens_per_second,
        "aggregate_total_tokens_per_second": report.aggregate_total_tokens_per_second,
        "concurrency_curve": report.concurrency_curve,
        "controller_stats": report.controller_stats,
        "usage_is_estimated": report.usage_is_estimated
    });
    println!("{}", serde_json::to_string_pretty(&summary)?);
    if !report.is_success() {
        bail!("benchmark contained failed requests");
    }
    Ok(())
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
struct ArmConfig {
    reasoning_effort: Option<String>,
    adaptive: Option<bool>,
    max_concurrency: Option<usize>,
    max_tokens: Option<u32>,
    temperature: Option<f32>,
    start_percent: Option<u32>,
}

#[derive(Debug, Clone, Serialize)]
struct ArmMetrics {
    output_tokens_per_second: f64,
    output_tokens_per_request: f64,
    total_output_tokens: u64,
    reasoning_tokens: Option<u64>,
    cached_input_tokens: Option<u64>,
    mean_time_to_first_token_ms: Option<f64>,
    elapsed_ms: u64,
    succeeded: usize,
    failed: usize,
    peak_concurrency: usize,
    limit_start: usize,
    limit_end: usize,
}

impl ArmMetrics {
    fn from_report(report: &RunReport) -> Self {
        let succeeded = report.status_count(TaskStatus::Succeeded).max(1);
        Self {
            output_tokens_per_second: report.aggregate_output_tokens_per_second,
            output_tokens_per_request: report.total_output_tokens as f64 / succeeded as f64,
            total_output_tokens: report.total_output_tokens,
            reasoning_tokens: report.total_reasoning_tokens,
            cached_input_tokens: report.total_cached_input_tokens,
            mean_time_to_first_token_ms: report.mean_time_to_first_token_ms,
            elapsed_ms: report.elapsed_ms,
            succeeded: report.status_count(TaskStatus::Succeeded),
            failed: report.status_count(TaskStatus::Failed),
            peak_concurrency: report.peak_concurrency,
            limit_start: report
                .concurrency_curve
                .first()
                .map(|point| point.limit)
                .unwrap_or(report.peak_concurrency),
            limit_end: report
                .concurrency_curve
                .last()
                .map(|point| point.limit)
                .unwrap_or(report.peak_concurrency),
        }
    }
}

/// Deterministic xorshift64 for reproducible bootstrap resampling.
struct XorShift(u64);

impl XorShift {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

/// Percentile bootstrap over paired per-round deltas.
/// Returns (mean_delta, Some(p2.5), Some(p97.5)); the bounds are None when
/// fewer than two deltas exist — there is no interval to estimate — and the
/// JSON report serializes them as null rather than non-finite floats.
fn bootstrap_ci(deltas: &[f64], resamples: usize, seed: u64) -> (f64, Option<f64>, Option<f64>) {
    let n = deltas.len();
    let mean = deltas.iter().sum::<f64>() / n as f64;
    if n < 2 {
        return (mean, None, None);
    }
    let mut rng = XorShift(seed | 1);
    let mut means = Vec::with_capacity(resamples);
    for _ in 0..resamples {
        let mut sum = 0.0;
        for _ in 0..n {
            sum += deltas[(rng.next() % n as u64) as usize];
        }
        means.push(sum / n as f64);
    }
    means.sort_by(f64::total_cmp);
    let lo = means[((resamples as f64 * 0.025) as usize).min(resamples - 1)];
    let hi = means[((resamples as f64 * 0.975) as usize).min(resamples - 1)];
    (mean, Some(lo), Some(hi))
}

async fn run_arm(
    client: &Arc<dyn ModelClient>,
    label: &str,
    arm: &ArmConfig,
    ab: &AbArgs,
) -> Result<ArmMetrics> {
    // A constant objective keeps the shared prefix byte-identical across
    // arms, rounds, and invocations, so all A/B traffic shares one
    // server-side prefill; the arm distinction lives in per-task prompts.
    let plan = Plan {
        objective: "slimtoken ab workload".into(),
        cache_primes: Vec::new(),
        tasks: (0..ab.requests)
            .map(|index| Task {
                id: format!("ab-{label}-{index:04}"),
                title: format!("Arm {label} request {index}"),
                prompt: format!("{}\nRequest variant: {index}", ab.prompt),
                deps: Vec::new(),
                max_tokens: arm.max_tokens.unwrap_or(ab.max_tokens),
                temperature: Some(arm.temperature.unwrap_or(0.0)),
                require_closed_fences: false,
                validate_artifact: false,
                static_output: None,
                reasoning_effort: None,
                internal: false,
            })
            .collect(),
    };
    let shared_context_extra = match &ab.shared_context_file {
        Some(path) => Some(
            std::fs::read_to_string(path)
                .with_context(|| format!("failed to read {}", path.display()))?,
        ),
        None => None,
    };
    let config = SchedulerConfig {
        max_concurrency: arm.max_concurrency.unwrap_or(128),
        adaptive: arm.adaptive.unwrap_or(false),
        // An arm that pins its own effort wins; otherwise inherit the global
        // --reasoning-effort instead of silently dropping it.
        reasoning_effort: arm
            .reasoning_effort
            .clone()
            .or_else(|| ab.client.reasoning_effort.clone()),
        controller: ControllerConfig {
            start_percent: arm.start_percent.unwrap_or(25),
            ..ControllerConfig::default()
        },
        shared_context_extra,
        prompt_cache_key: ab
            .prompt_cache_key
            .clone()
            .or_else(|| Some("slimtoken-ab".to_string())),
        max_starts_per_minute: ab.rpm,
        ..SchedulerConfig::default()
    };
    config.validate()?;
    let report = execute_plan(&plan, Arc::clone(client), config).await?;
    if report.status_count(TaskStatus::Failed) > 0 {
        bail!("arm {label} contained failed requests");
    }
    Ok(ArmMetrics::from_report(&report))
}

async fn ab(args: AbArgs) -> Result<()> {
    if args.arms.len() != 2 {
        bail!("--arms expects exactly two JSON config files");
    }
    if args.rounds == 0 || args.requests == 0 || args.requests > 256 {
        bail!("--rounds must be positive and --requests must be between 1 and 256");
    }
    let arm_configs: Vec<ArmConfig> = args
        .arms
        .iter()
        .map(|path| {
            let text = std::fs::read_to_string(path)
                .with_context(|| format!("failed to read {}", path.display()))?;
            serde_json::from_str(&text)
                .with_context(|| format!("invalid arm config {}", path.display()))
        })
        .collect::<Result<_>>()?;
    if let Some(effort) = &args.client.reasoning_effort {
        for (index, arm) in arm_configs.iter().enumerate() {
            if arm.reasoning_effort.as_ref().is_some_and(|e| e != effort) {
                bail!(
                    "arm {} sets reasoning_effort={} but --reasoning-effort {} was also passed; keep the effort in the arm configs only",
                    index,
                    arm.reasoning_effort.as_deref().unwrap_or("?"),
                    effort
                );
            }
        }
    }
    let client = make_client(&args.client, false, 0)?;

    let mut rounds_a: Vec<ArmMetrics> = Vec::new();
    let mut rounds_b: Vec<ArmMetrics> = Vec::new();
    for round in 0..args.rounds {
        // Alternate which arm runs first so slow server drift hits both equally.
        let (a_metrics, b_metrics) = if round % 2 == 0 {
            let a = run_arm(&client, "a", &arm_configs[0], &args).await?;
            let b = run_arm(&client, "b", &arm_configs[1], &args).await?;
            (a, b)
        } else {
            let b = run_arm(&client, "b", &arm_configs[1], &args).await?;
            let a = run_arm(&client, "a", &arm_configs[0], &args).await?;
            (a, b)
        };
        info!(
            round,
            a_tps = format!("{:.1}", a_metrics.output_tokens_per_second),
            b_tps = format!("{:.1}", b_metrics.output_tokens_per_second),
            a_elapsed_ms = a_metrics.elapsed_ms,
            b_elapsed_ms = b_metrics.elapsed_ms,
            "ab round complete"
        );
        rounds_a.push(a_metrics);
        rounds_b.push(b_metrics);
    }

    let delta_tps: Vec<f64> = rounds_a
        .iter()
        .zip(&rounds_b)
        .map(|(a, b)| b.output_tokens_per_second - a.output_tokens_per_second)
        .collect();
    let delta_tokens: Vec<f64> = rounds_a
        .iter()
        .zip(&rounds_b)
        .map(|(a, b)| b.output_tokens_per_request - a.output_tokens_per_request)
        .collect();
    let delta_elapsed: Vec<f64> = rounds_a
        .iter()
        .zip(&rounds_b)
        .map(|(a, b)| b.elapsed_ms as f64 - a.elapsed_ms as f64)
        .collect();

    let ci = |deltas: &[f64]| {
        let (mean, lo, hi) = bootstrap_ci(deltas, 10_000, 0x9E37_79B9_7F4A_7C15);
        let significant = match (lo, hi) {
            (Some(lo), Some(hi)) => (lo > 0.0 && hi > 0.0) || (lo < 0.0 && hi < 0.0),
            _ => false,
        };
        json!({
            "mean": mean,
            "ci95": [lo, hi],
            "significant": significant,
        })
    };

    let a_mean = mean_metrics(&rounds_a);
    let b_mean = mean_metrics(&rounds_b);
    // Wall-clock is the primary verdict signal (it is what the user feels);
    // aggregate tok/s is secondary (it can mask a tokens-per-request win when
    // burn drops too, as it does for reasoning_effort=low).
    let elapsed_sig = ci(&delta_elapsed)["significant"].as_bool() == Some(true);
    let tps_sig = ci(&delta_tps)["significant"].as_bool() == Some(true);
    let verdict = if elapsed_sig {
        if delta_elapsed.iter().sum::<f64>() < 0.0 {
            "b_faster"
        } else {
            "a_faster"
        }
    } else if tps_sig {
        if delta_tps.iter().sum::<f64>() > 0.0 {
            "b_faster"
        } else {
            "a_faster"
        }
    } else {
        "no_significant_difference"
    };

    let summary = json!({
        "arms": {
            "a": serde_json::to_value(&arm_configs[0])?,
            "b": serde_json::to_value(&arm_configs[1])?,
        },
        "rounds": args.rounds,
        "requests_per_arm": args.requests,
        "mean_a": a_mean,
        "mean_b": b_mean,
        "paired_delta_b_minus_a": {
            "output_tokens_per_second": ci(&delta_tps),
            "output_tokens_per_request": ci(&delta_tokens),
            "elapsed_ms": ci(&delta_elapsed),
        },
        "verdict": verdict,
    });
    println!("{}", serde_json::to_string_pretty(&summary)?);
    Ok(())
}

fn mean_metrics(rounds: &[ArmMetrics]) -> serde_json::Value {
    let n = rounds.len().max(1) as f64;
    let avg = |f: fn(&ArmMetrics) -> f64| rounds.iter().map(f).sum::<f64>() / n;
    json!({
        "output_tokens_per_second": avg(|m| m.output_tokens_per_second),
        "output_tokens_per_request": avg(|m| m.output_tokens_per_request),
        "elapsed_ms": avg(|m| m.elapsed_ms as f64),
        "mean_time_to_first_token_ms": avg(|m| m.mean_time_to_first_token_ms.unwrap_or(0.0)),
    })
}

fn make_manager(args: &RuntimeArgs) -> Result<RunManager> {
    info!(mock = args.mock, output = %args.output.display(), "run manager configured");
    let client = make_client(&args.client, args.mock, args.mock_latency_ms)?;
    // The library default keeps admission deterministic for embedders; the
    // product surfaces (CLI run/serve/interactive) opt into AIMD explicitly
    // since cold bursts make Muse-class servers shed load.
    let mut config = args
        .scheduler
        .config(args.client.reasoning_effort.clone())?;
    config.adaptive = true;
    Ok(RunManager::new(
        client,
        config,
        args.output.clone(),
        args.mock,
    ))
}

#[cfg(feature = "serve")]
async fn serve(args: ServeArgs) -> Result<()> {
    let manager = make_manager(&args.runtime)?;
    let listener = tokio::net::TcpListener::bind(args.listen)
        .await
        .with_context(|| format!("failed to bind {}", args.listen))?;
    let address = listener
        .local_addr()
        .context("failed to read listen address")?;
    println!("SlimToken REST API listening on http://{address}");
    println!("Submit runs with POST /v1/runs; health is GET /health");
    axum::serve(listener, slimtoken::api::router(manager.clone()))
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await
        .context("REST server failed")?;
    for summary in manager.list() {
        if !summary.status.is_terminal() {
            manager.cancel(&summary.id);
        }
    }
    Ok(())
}

fn make_client(
    args: &ClientArgs,
    mock: bool,
    mock_latency_ms: u64,
) -> Result<Arc<dyn ModelClient>> {
    // Idempotent: `run` applies it earlier for the architect flags too.
    let mut resolved = args.clone();
    apply_client_preset(&mut resolved);
    let args = &resolved;
    if let Some(path) = &args.replay_from {
        let client = slimtoken::record::ReplayClient::load(path, Default::default())?;
        return Ok(Arc::new(client));
    }
    let client: Arc<dyn ModelClient> = if mock {
        Arc::new(MockClient::new(Duration::from_millis(mock_latency_ms)))
    } else {
        let api_key = args
            .api_key
            .clone()
            .or_else(|| std::env::var("OPENAI_API_KEY").ok());
        Arc::new(
            OpenAiClient::new(&args.base_url, args.model.clone(), api_key)?
                .with_stream_usage(args.stream_usage),
        )
    };
    if let Some(path) = &args.record_to {
        return Ok(Arc::new(slimtoken::record::RecordingClient::new(
            client, path,
        )?));
    }
    Ok(client)
}

async fn load_plan(path: &Path) -> Result<Plan> {
    let bytes = tokio::fs::read(path)
        .await
        .with_context(|| format!("failed to read {}", path.display()))?;
    let plan: Plan = serde_json::from_slice(&bytes)
        .with_context(|| format!("invalid plan JSON in {}", path.display()))?;
    plan.validate()
        .with_context(|| format!("invalid task graph in {}", path.display()))?;
    Ok(plan)
}

async fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(value).context("failed to serialize JSON")?;
    tokio::fs::write(path, bytes)
        .await
        .with_context(|| format!("failed to write {}", path.display()))
}

fn print_report(report: &RunReport) {
    println!(
        "Tasks: {}/{} succeeded, {} failed, {} skipped, {} cancelled",
        report.status_count(TaskStatus::Succeeded),
        report.task_count,
        report.status_count(TaskStatus::Failed),
        report.status_count(TaskStatus::Skipped),
        report.status_count(TaskStatus::Cancelled)
    );
    println!(
        "Elapsed: {} ms | peak concurrency: {} | output: {:.1} tokens/s",
        report.elapsed_ms, report.peak_concurrency, report.aggregate_output_tokens_per_second
    );
    if let Some(reasoning) = report.total_reasoning_tokens {
        println!("Hidden reasoning tokens: {reasoning}");
    }
    if let Some(cached) = report.total_cached_input_tokens {
        println!("Prefix-cache hits (prompt tokens): {cached}");
    }
    if let Some(ttft) = report.mean_time_to_first_token_ms {
        println!("Mean TTFT: {ttft:.0} ms");
    }
    if let Some(stats) = &report.controller_stats {
        println!(
            "Controller: {} windows, {} increases, {} decreases",
            stats.windows_evaluated, stats.increases, stats.decreases
        );
    }
    if report.usage_is_estimated {
        println!("Token usage includes character-based estimates; enable server usage reporting for exact figures.");
    }
}

/// `--profile`: `None` for `auto` (detect from the objective).
#[derive(Debug, Clone)]
struct ProfileArg(Option<String>);

/// `--profile`: `auto` or a profile name. The name resolves AFTER any
/// `--profile-dir` load, so a library profile is selectable the same way as
/// a built-in; unknown names error at resolution with the known names.
fn parse_profile(name: &str) -> Result<ProfileArg, String> {
    if name == "auto" {
        return Ok(ProfileArg(None));
    }
    Ok(ProfileArg(Some(name.to_string())))
}

#[cfg(test)]
mod tests {
    #[test]
    fn provider_preset_fills_only_defaulted_flags() {
        let cli = Cli::parse_from([
            "slimtoken",
            "run",
            "--objective",
            "x",
            "--provider",
            "deepseek",
            "--model",
            "custom",
        ]);
        let Commands::Run(mut args) = cli.command else {
            panic!("run subcommand");
        };
        let preset = apply_client_preset(&mut args.client).expect("known provider");
        assert_eq!(args.client.base_url, "https://api.deepseek.com/v1");
        assert_eq!(args.client.model, "custom", "explicit flag wins");
        assert_eq!(args.client.reasoning_effort.as_deref(), Some("none"));
        assert!(!preset.architect_schema);
        assert_eq!(preset.cold_burst_cap, 16);
        assert!(
            preset.single_response_threshold < 13_000,
            "finance goes to the wave"
        );
    }

    #[test]
    fn manifest_cache_key_tracks_manifest_shaping_flags_only() {
        let parse = |extra: &[&str]| {
            let mut argv = vec!["slimtoken", "run", "--objective", "obj", "--architect"];
            argv.extend_from_slice(extra);
            let Commands::Run(args) = Cli::parse_from(argv).command else {
                panic!("run subcommand");
            };
            args
        };
        let key = |args: &RunArgs| manifest_cache_key("obj", args, 12);
        let base = parse(&[]);
        let same = parse(&[]);
        assert_eq!(key(&base), key(&same), "same inputs -> same key");
        assert_ne!(key(&base), manifest_cache_key("other", &base, 12));
        assert_ne!(key(&base), manifest_cache_key("obj", &base, 24));

        // plan-shaping flags never reach the architect, so flipping
        // them on a fixed objective must NOT invalidate a cached manifest.
        // (edges and tests-see-code default to true, so flip them off.)
        for plan_only in [
            vec!["--architect-density", "true"],
            vec!["--architect-tests-see-code", "false"],
            vec!["--architect-test-quota", "150"],
            vec!["--architect-scope-contracts", "true"],
            // Wave wiring is a plan-time call; only Wiring::Plan wires the
            // manifest itself.
            vec!["--architect-wiring", "wave"],
            // Steering never enters the split path's prompts (the default
            // force-split generation).
            vec!["--steer", "use tabs"],
        ] {
            let flipped = parse(&plan_only);
            assert_eq!(
                key(&base),
                key(&flipped),
                "plan-only flag {:?} must not change the manifest key",
                plan_only
            );
        }

        // Manifest-shaping inputs still invalidate.
        for manifest_shaping in [
            vec!["--architect-edges", "false"],
            vec!["--architect-examples", "true"],
            vec!["--architect-files-per-fragment", "1"],
            vec!["--architect-wiring", "plan"],
            vec!["--architect-type-ownership", "true"],
            // Single-shot changes the generation path AND admits steering
            // into the architect prompt.
            vec!["--architect-single-shot"],
            vec!["--profile", "nextjs"],
        ] {
            let flipped = parse(&manifest_shaping);
            assert_ne!(
                key(&base),
                key(&flipped),
                "manifest-shaping flag {:?} must change the manifest key",
                manifest_shaping
            );
        }
        // Steering invalidates only on the single-shot path.
        let single_shot = parse(&["--architect-single-shot"]);
        let single_shot_steered = parse(&["--architect-single-shot", "--steer", "use tabs"]);
        assert_ne!(key(&single_shot), key(&single_shot_steered));
    }

    use super::*;

    fn plan(edges: &[(&str, &[&str])]) -> Plan {
        Plan {
            cache_primes: Vec::new(),
            objective: "test objective".into(),
            tasks: edges
                .iter()
                .map(|(id, deps)| Task {
                    id: id.to_string(),
                    title: id.to_string(),
                    prompt: format!("do {id}"),
                    deps: deps.iter().map(|dep| dep.to_string()).collect(),
                    max_tokens: 64,
                    temperature: Some(0.0),
                    require_closed_fences: false,
                    validate_artifact: false,
                    static_output: None,
                    reasoning_effort: None,
                    internal: false,
                })
                .collect(),
        }
    }

    #[test]
    fn integration_task_depends_on_every_leaf() {
        // Fan-out: every task is a leaf.
        let mut fanout = plan(&[("a", &[]), ("b", &[]), ("c", &[])]);
        let id = append_integration_task(&mut fanout, 1);
        let sink = fanout.tasks.iter().find(|t| t.id == id[0]).expect("sink");
        assert_eq!(sink.deps, vec!["a", "b", "c"]);
        fanout.validate().expect("plan with sink validates");

        // Chain: only the last task is a leaf.
        let mut chain = plan(&[("a", &[]), ("b", &["a"]), ("c", &["b"])]);
        let id = append_integration_task(&mut chain, 1);
        let sink = chain.tasks.iter().find(|t| t.id == id[0]).expect("sink");
        assert_eq!(sink.deps, vec!["c"]);
        chain.validate().expect("chain with sink validates");

        // Idempotent: appending twice does not duplicate.
        let again = append_integration_task(&mut chain, 1);
        assert_eq!(again, id);
        assert_eq!(chain.tasks.iter().filter(|t| t.id == id[0]).count(), 1);
    }

    #[test]
    fn sink_shards_partition_leaves_disjointly_and_balanced() {
        let mut plan = plan(&[
            ("a", &[]),
            ("b", &[]),
            ("c", &[]),
            ("d", &[]),
            ("e", &[]),
            ("f", &[]),
            ("g", &[]),
        ]);
        for (index, task) in plan.tasks.iter_mut().enumerate() {
            // Varied weights so LPT grouping is exercised: 8k, 4k, 2x2k, 3x1k.
            task.max_tokens = [8192, 4096, 2048, 2048, 1024, 1024, 1024][index];
        }
        let ids = append_integration_task(&mut plan, 3);
        assert_eq!(ids.len(), 3);
        plan.validate().expect("plan with shards validates");

        let mut covered: Vec<&str> = Vec::new();
        let mut weights: Vec<u64> = Vec::new();
        for id in &ids {
            let shard = plan.tasks.iter().find(|t| t.id == *id).expect("shard");
            assert!(shard.require_closed_fences);
            // Budget follows section weight (leaf max_tokens sum), clamped —
            // never below the content a shard must hold (truncation retries
            // cost more than the parallel save), never above the legacy cap.
            let section: u64 = shard
                .deps
                .iter()
                .map(|dep| {
                    plan.tasks
                        .iter()
                        .find(|t| &t.id == dep)
                        .map_or(0, |t| u64::from(t.max_tokens))
                })
                .sum();
            assert_eq!(shard.max_tokens, (section as u32).clamp(2048, 8192));
            for dep in &shard.deps {
                assert!(!covered.contains(&dep.as_str()), "leaf {dep} in two shards");
                covered.push(dep);
            }
            weights.push(shard.deps.len() as u64);
        }
        assert_eq!(covered.len(), 7, "every leaf covered exactly once");
        // LPT on the given weights: a(8k) alone fills its shard first, then
        // b(4k)+e+g / c+d+f(2k,2k,1k) land on the lightest shard each:
        // member counts [1,3,3].
        weights.sort_unstable();
        assert_eq!(weights, vec![1, 3, 3]);
        let shard_of_a = plan
            .tasks
            .iter()
            .filter(|t| t.id.starts_with("slimtoken-final-integration"))
            .find(|t| t.deps.contains(&"a".to_string()))
            .expect("shard holding a");
        assert_eq!(shard_of_a.deps.len(), 1, "the 8k leaf stands alone");
    }

    #[test]
    fn sink_shards_fall_back_to_the_legacy_sink() {
        let mut single = plan(&[("a", &[]), ("b", &[])]);
        let ids = append_integration_task(&mut single, 1);
        assert_eq!(ids, vec!["slimtoken-final-integration"]);
        assert_eq!(
            single
                .tasks
                .iter()
                .filter(|t| t.id.starts_with("slimtoken-final-integration"))
                .count(),
            1
        );
        // Fewer leaves than shards: one shard per leaf would be degenerate.
        let mut tiny = plan(&[("a", &[])]);
        let ids = append_integration_task(&mut tiny, 4);
        assert_eq!(ids, vec!["slimtoken-final-integration"]);
    }

    #[test]
    fn final_output_prefers_integration_then_concatenates() {
        let mut plan = plan(&[("a", &[]), ("b", &["a"])]);
        let ids = append_integration_task(&mut plan, 1);
        let id = ids[0].as_str();
        let mut report = report_with(&[
            ("a", Some("alpha artifact"), TaskStatus::Succeeded),
            ("b", Some("beta artifact"), TaskStatus::Succeeded),
            (id, Some("UNIFIED DELIVERABLE"), TaskStatus::Succeeded),
        ]);

        // Integration output wins when present.
        let doc = build_final_output(&report, &plan, &ids);
        assert_eq!(doc, "UNIFIED DELIVERABLE");

        // Without integration, succeeded outputs concatenate in plan order;
        // failed tasks (and the unused integration record) are omitted.
        report.tasks.get_mut("b").expect("b").status = TaskStatus::Failed;
        report.tasks.get_mut(id).expect("integration").status = TaskStatus::Failed;
        let doc = build_final_output(&report, &plan, &[]);
        assert!(doc.contains("alpha artifact"));
        assert!(!doc.contains("beta artifact"));
        assert!(!doc.contains("UNIFIED"));
    }

    fn report_with(entries: &[(&str, Option<&str>, TaskStatus)]) -> RunReport {
        let mut report = RunReport {
            objective: "test objective".into(),
            steering: Vec::new(),
            elapsed_ms: 1,
            task_count: entries.len(),
            peak_concurrency: 1,
            total_input_tokens: 3,
            total_output_tokens: 3,
            total_reasoning_tokens: None,
            total_cached_input_tokens: None,
            mean_time_to_first_token_ms: None,
            aggregate_output_tokens_per_second: 0.0,
            aggregate_total_tokens_per_second: 0.0,
            concurrency_curve: Vec::new(),
            controller_stats: None,
            rate_tune: None,
            usage_is_estimated: false,
            phases: Default::default(),
            tasks: std::collections::BTreeMap::new(),
        };
        for (task_id, output, status) in entries {
            report.tasks.insert(
                task_id.to_string(),
                slimtoken::TaskRecord {
                    id: task_id.to_string(),
                    title: task_id.to_string(),
                    status: *status,
                    output: output.map(str::to_string),
                    error: None,
                    attempts: 1,
                    duration_ms: 1,
                    input_tokens: 1,
                    output_tokens: 1,
                    reasoning_tokens: None,
                    cached_input_tokens: None,
                    provider: None,
                    rate_limited: false,
                    finish_reason: Some("stop".into()),
                    usage_is_estimated: false,
                    time_to_first_token_ms: None,
                },
            );
        }
        report
    }

    #[test]
    fn final_output_concatenates_shards_in_order_and_recovers_a_failed_shard() {
        let mut plan = plan(&[("a", &[]), ("b", &[]), ("c", &[]), ("d", &[])]);
        let ids = append_integration_task(&mut plan, 2);
        assert_eq!(ids.len(), 2);

        // Both shards produced: fragments concatenate in shard order; the
        // leaf outputs are NOT re-inlined (no duplication).
        let mut entries: Vec<(&str, Option<&str>, TaskStatus)> = vec![
            ("a", Some("LEAF-A"), TaskStatus::Succeeded),
            ("b", Some("LEAF-B"), TaskStatus::Succeeded),
            ("c", Some("LEAF-C"), TaskStatus::Succeeded),
            ("d", Some("LEAF-D"), TaskStatus::Succeeded),
        ];
        entries.push((
            ids[0].as_str(),
            Some("FRAGMENT-ZERO"),
            TaskStatus::Succeeded,
        ));
        entries.push((ids[1].as_str(), Some("FRAGMENT-ONE"), TaskStatus::Succeeded));
        let report = report_with(&entries);
        let doc = build_final_output(&report, &plan, &ids);
        assert_eq!(doc, "FRAGMENT-ZERO\n\nFRAGMENT-ONE");
        for leaf in ["LEAF-A", "LEAF-B", "LEAF-C", "LEAF-D"] {
            assert!(!doc.contains(leaf), "leaf {leaf} must not duplicate: {doc}");
        }

        // Second shard failed: its own leaves stand in so its group is covered.
        let mut entries: Vec<(&str, Option<&str>, TaskStatus)> = vec![
            ("a", Some("LEAF-A"), TaskStatus::Succeeded),
            ("b", Some("LEAF-B"), TaskStatus::Succeeded),
            ("c", Some("LEAF-C"), TaskStatus::Succeeded),
            ("d", Some("LEAF-D"), TaskStatus::Succeeded),
        ];
        entries.push((
            ids[0].as_str(),
            Some("FRAGMENT-ZERO"),
            TaskStatus::Succeeded,
        ));
        entries.push((ids[1].as_str(), None, TaskStatus::Failed));
        let report = report_with(&entries);
        let doc = build_final_output(&report, &plan, &ids);
        assert!(doc.starts_with("FRAGMENT-ZERO"));
        let failed_shard = plan
            .tasks
            .iter()
            .find(|t| t.id == ids[1])
            .expect("failed shard");
        for dep in &failed_shard.deps {
            let leaf_output = report.tasks[dep].output.as_deref().unwrap();
            assert!(
                doc.contains(leaf_output),
                "leaf {dep} of the failed shard must stand in: {doc}"
            );
        }
    }

    /// Size-aware stand-in for a real provider: generation time scales with
    /// the output budget (the property sink splitting exploits), plus a small
    /// input-size term for prompt ingestion.
    struct SizeAwareClient;

    #[slimtoken::async_trait]
    impl slimtoken::ModelClient for SizeAwareClient {
        async fn complete(
            &self,
            request: slimtoken::CompletionRequest,
        ) -> anyhow::Result<slimtoken::Completion> {
            let delay = Duration::from_millis(
                20 + u64::from(request.max_tokens) / 8 + (request.user.len() as u64) / 2000,
            );
            tokio::time::sleep(delay).await;
            Ok(slimtoken::Completion {
                text: "```\nfn shard_test() {}\n```".to_string(),
                provider: None,
                input_tokens: 10,
                output_tokens: 10,
                reasoning_tokens: None,
                cached_input_tokens: None,
                finish_reason: Some("stop".to_string()),
                usage_is_estimated: false,
                elapsed: delay,
                time_to_first_token: Some(Duration::from_millis(1)),
            })
        }
    }

    #[tokio::test]
    async fn split_sink_cuts_the_sink_wall_structurally() {
        let make = |shards: usize| {
            let mut plan = plan(&[
                ("a", &[]),
                ("b", &[]),
                ("c", &[]),
                ("d", &[]),
                ("e", &[]),
                ("f", &[]),
                ("g", &[]),
                ("h", &[]),
            ]);
            for task in &mut plan.tasks {
                task.max_tokens = 1024;
            }
            let ids = append_integration_task(&mut plan, shards);
            (plan, ids)
        };
        let (legacy_plan, legacy_ids) = make(1);
        assert_eq!(legacy_ids.len(), 1);
        let (shard_plan, shard_ids) = make(4);
        assert_eq!(shard_ids.len(), 4);

        let config = || SchedulerConfig {
            cache_warm_gate: false,
            max_concurrency: 16,
            ..SchedulerConfig::default()
        };
        let client = || -> Arc<dyn slimtoken::ModelClient> { Arc::new(SizeAwareClient) };
        let legacy = execute_plan(&legacy_plan, client(), config())
            .await
            .expect("legacy run");
        let split = execute_plan(&shard_plan, client(), config())
            .await
            .expect("split run");
        assert!(legacy.is_success(), "legacy run healthy: {legacy:?}");
        assert!(split.is_success(), "split run healthy: {split:?}");
        // Structural margin: shards cut the serial sink generation (~1.1s at
        // 8192 max_tokens) to one shard round (~0.3s at 2048), so the split
        // wall must come in well under the legacy wall even with jitter.
        assert!(
            split.elapsed_ms + 200 < legacy.elapsed_ms,
            "split sink must beat the single sink structurally: split {} ms vs legacy {} ms",
            split.elapsed_ms,
            legacy.elapsed_ms
        );
    }
}
