//! Architect-driven planning for greenfield builds.
//!
//! The classic planner decomposes an objective into an arbitrary DAG, which
//! for "build a program from scratch" produces overlapping worker outputs
//! that a model sink must then rewrite (measured: ~90% of worker output
//! tokens discarded, ~15k sink tokens, and merge defects like builtin
//! shadowing). The architect instead makes ONE cheap call to fix the file
//! manifest and the interface contracts up front. Tasks are then built
//! deterministically in Rust: one worker per file, one for the test suite,
//! one for the README. Because every worker codes against the published
//! contracts instead of guessing other workers' output, the DAG has zero
//! dependency edges — a single fully-parallel wave — and the final document
//! is assembled by concatenation, not regenerated.
//!
//! Token discipline: each worker's prompt carries only the contract it
//! needs (its own spec; other modules as one-line signature lists; the test
//! worker sees every signature). Measured on the finance-suite bench, the
//! whole-manifest shared prefix cost ~880 input tokens per task; per-task
//! contracts cut that to ~300.

use crate::client::{Completion, CompletionRequest, ModelClient};
use crate::control::validate_steering;
use crate::dag::{Plan, Task};
use crate::jsonfix;
use crate::profile::Profile;
use anyhow::{bail, Context, Result};
use futures::{stream::FuturesUnordered, StreamExt};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    pub files: Vec<ManifestFile>,
    #[serde(default)]
    pub test_files: Vec<ManifestFile>,
    /// When true the plan includes a README worker task.
    #[serde(default)]
    pub readme: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(from = "ManifestFileRepr")]
pub struct ManifestFile {
    pub path: String,
    pub purpose: String,
    /// Exported signatures other files (and tests) must code against,
    /// e.g. `def add_expense(date, category, amount, note="") -> dict`.
    #[serde(default)]
    pub exports: Vec<String>,
    /// Other files' exports this file must call (`path:name`), from the
    /// wiring pass. Empty unless `ArchitectOptions::wiring` ran.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub uses: Vec<String>,
}

/// Accepted wire shapes: the full object, or a bare path string. Skeletons
/// list `test_files` as plain paths about one run in three (DeepSeek,
/// sitegen); rejecting that cost a reasoning-heavy retry (0.7-2.5k tokens,
/// 8-18s) for a field the plan only uses for its path.
#[derive(Deserialize)]
#[serde(untagged)]
enum ManifestFileRepr {
    Path(String),
    Full {
        path: String,
        #[serde(default)]
        purpose: String,
        #[serde(default)]
        exports: Vec<String>,
        #[serde(default)]
        uses: Vec<String>,
    },
}

impl From<ManifestFileRepr> for ManifestFile {
    fn from(repr: ManifestFileRepr) -> Self {
        match repr {
            ManifestFileRepr::Path(path) => Self {
                path,
                purpose: String::new(),
                exports: Vec::new(),
                uses: Vec::new(),
            },
            ManifestFileRepr::Full {
                path,
                purpose,
                exports,
                uses,
            } => Self {
                path,
                purpose,
                exports,
                uses,
            },
        }
    }
}

impl Manifest {
    fn exports_inline(&self, file: &ManifestFile) -> String {
        if file.exports.is_empty() {
            "as the purpose describes".to_string()
        } else if file
            .exports
            .iter()
            .any(|export| export.contains('#') || export.contains("//"))
        {
            // Edge-behavior comments use `;` themselves.
            file.exports.join(" | ")
        } else {
            file.exports.join("; ")
        }
    }

    /// Every module with its signatures; used by the test worker, which
    /// must exercise the whole contract surface.
    fn all_modules(&self) -> String {
        self.files
            .iter()
            .map(|file| format!("- `{}`: {}", file.path, self.exports_inline(file)))
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// Deliverables up to this size fit one model response; routing them to a
/// single direct call costs exactly the single-stream token spend, while a
/// parallel wave pays the N-worker tax for nothing. Measured: finance
/// objective ~6.7k, "40+ tests" objective ~15k real output — both direct.
pub const SINGLE_RESPONSE_THRESHOLD_TOKENS: u64 = 28_000;

/// Direct-call task ceiling when the router decides the deliverable fits
/// one response. Mirrors the bench single-stream arm (32k cap).
const DIRECT_MAX_TOKENS: u32 = 32_768;

/// Estimate the output tokens a single-response deliverable needs, from
/// demands stated in the objective text. Zero model calls. Coefficients are
/// calibrated on measured single-stream outputs:
/// compact pytest test ~300 out tok, module file ~800 out tok, prose ~8/word.
pub fn estimate_single_response_tokens(objective: &str) -> u64 {
    let words = objective.split_whitespace().count() as u64;
    let files =
        (objective.matches(".py").count() + objective.matches(".ts").count()).min(24) as u64;
    let tests = demanded_test_count(objective);
    4_000 + 300 * tests + 800 * files + 8 * words
}

/// Parse an explicit test-count demand like "at least 40 tests", "40+ tests",
/// "40 tests". Absent a number, assumes a modest default suite of 15.
fn demanded_test_count(objective: &str) -> u64 {
    let lower = objective.to_lowercase();
    let tokens: Vec<&str> = lower
        .split(|ch: char| !(ch.is_ascii_alphanumeric() || ch == '+'))
        .filter(|token| !token.is_empty())
        .collect();
    for (index, token) in tokens.iter().enumerate() {
        if *token != "tests" && *token != "test" {
            continue;
        }
        // "at least N tests" / "N tests" / "N+ tests": scan back 3 tokens.
        for back in tokens[..index].iter().rev().take(3) {
            let digits: String = back.chars().take_while(|ch| ch.is_ascii_digit()).collect();
            if !digits.is_empty() {
                return digits.parse().unwrap_or(15);
            }
        }
    }
    15
}

/// Router: does this objective's deliverable fit one model response?
pub fn fits_single_response(objective: &str) -> bool {
    estimate_single_response_tokens(objective) <= SINGLE_RESPONSE_THRESHOLD_TOKENS
}

/// Shared architect rules, reused by the full-manifest prompt and by every
/// split-manifest fragment so contracts stay consistent across calls.
fn manifest_rules(max_tasks: usize, profile: &Profile, type_ownership: bool) -> String {
    let language = profile.planner_rules;
    let ownership = if type_ownership {
        profile.type_ownership_rule
    } else {
        ""
    };
    format!(
        r#"Rules:
{language}{ownership}
- Prefer the smallest file set that satisfies the objective; every file is
  written by exactly one worker in parallel, so files must be implementable
  from the contracts alone — no file may need to read another file's source.
- Keep the manifest compact: purposes are at most 8 words, at most 4 exports
  per file, one line per export (return-shape/CLI annotations included on
  that line). The whole JSON must fit in ~1800 tokens.
- List exported functions/classes/signatures precisely; tests will be
  written from these signatures WITHOUT seeing implementations.
- Pin return semantics in an annotation after each export (still one line):
  for functions returning dict/list[dict], name the exact keys, e.g.
  `def expense_summary(path: str, month: str) -> dict  # {{total: float,
  categories: {{category: float}}, count: int}}`. For a CLI entry point, pin
  the exact argument grammar, e.g. `# args: add --date D --category C
  --amount A [--note N] --file P; summary --month M --file P`.
- Pin the exact keys of every dict-typed PARAMETER the same way, e.g.
  `def next_occurrence(template: dict{{category: str, amount: float,
  frequency: str, anchor_date: str}}, after: str) -> str`. Module workers and
  test writers copy these key names character for character — an unpinned
  `dict` lets each side invent different keys and their code drifts apart
  (measured: 12 test failures from one unpinned template dict).
- Every operation the objective names for a file must appear as an export
  with the exact operation name (e.g. an objective asking for
  "add/list/summary operations" forces exports named add, list, summary).
- CLI entry points must RETURN exit codes (0 success, 2 usage error) and
  never raise SystemExit inside main; tests will assert return values.
- Choose module names that never shadow Python builtins (avoid `list.py`,
  a function named `list`, etc).
- Use only the standard library unless the objective says otherwise.
- Keep the total task count within the budget: at most {max_tasks} files +
  test files + 1 README combined. Group tiny modules into one file rather
  than exceeding the budget.
- Prefer ONE combined test file covering all modules (e.g.
  tests/test_all.py); use separate test files only for clearly distinct
  subsystems. If the objective demands more than ~45 tests, split the suite
  into multiple test_files of at most ~40 tests each (e.g. tests/test_core.py,
  tests/test_io.py) that partition the demanded coverage.
- The README is a separate task and will be generated too; do not put
  documentation files in `files`."#
    )
}

/// Backoff after a failed architect call: empty completions arrive in
/// multi-minute server bursts, so ride those out long; anything else is a
/// fast 3s re-arm.
fn backoff_for(error: &anyhow::Error) -> std::time::Duration {
    if format!("{error:#}").contains("empty completion") {
        std::time::Duration::from_secs(45)
    } else {
        std::time::Duration::from_secs(3)
    }
}

/// Build the router's direct plan: one task carrying the objective verbatim,
/// exactly what a user would send the model in a single call.
pub fn direct_response_plan(objective: &str) -> Plan {
    Plan {
        cache_primes: Vec::new(),
        objective: objective.to_string(),
        tasks: vec![Task {
            id: "single-response".to_string(),
            title: "single-response deliverable".to_string(),
            prompt: objective.to_string(),
            deps: Vec::new(),
            max_tokens: DIRECT_MAX_TOKENS,
            temperature: Some(0.2),
            // The deliverable is a document with many fenced blocks, not one
            // fenced artifact — fence-balance checks don't apply.
            require_closed_fences: false,
            validate_artifact: false,
            static_output: None,
            reasoning_effort: None,
            internal: false,
        }],
    }
}

/// One cheap model call fixes the manifest; tasks are built in Rust.
pub async fn generate_manifest_plan(
    client: &dyn ModelClient,
    objective: &str,
    max_tasks: usize,
    steering: &[String],
) -> Result<Plan> {
    generate_manifest_plan_with_options(client, objective, max_tasks, steering, false, None).await
}

/// Stable prefix shared by every architect call in a run (single-shot ladder,
/// regenerations, and all fragments): persona + manifest rules. Carried as
/// the request `shared_context` so the server's prefix cache computes it
/// once per run — and, with a stable `cache_key`, once per project across
/// runs. Objective stays at the top of each user body so same-objective
/// calls prefix-align past the shared block too.
fn architect_shared_context(max_tasks: usize, profile: &Profile, type_ownership: bool) -> String {
    format!(
        "You are the architect for a greenfield software build. You decide \
         the file layout and the exact interface contracts. Return JSON only.\n\n{}",
        manifest_rules(max_tasks, profile, type_ownership)
    )
}

/// `force_split` skips the single-shot manifest entirely and emits fragments
/// from the start — retry loops use it once a build has demonstrably
/// outgrown the model's reliable completion envelope, so a retry doesn't
/// re-pay the single-shot ladder before fragmenting again.
pub async fn generate_manifest_plan_with_options(
    client: &dyn ModelClient,
    objective: &str,
    max_tasks: usize,
    steering: &[String],
    force_split: bool,
    cache_key: Option<&str>,
) -> Result<Plan> {
    let options = ArchitectOptions {
        force_split,
        cache_key: cache_key.map(str::to_string),
        ..ArchitectOptions::default()
    };
    let manifest = generate_manifest(client, objective, max_tasks, steering, &options).await?;
    build_plan(objective, &manifest, max_tasks)
}

/// Plan-shaping knobs that flow from CLI flags into `plan_from_manifest`.
/// Bundled so the plan-build signature stays small as knobs accrue.
#[derive(Debug, Clone, Copy)]
pub struct PlanOptions {
    /// Test shards depend on the modules they cover and see their code.
    pub tests_see_code: bool,
    /// The wiring question runs as a task in the wave.
    pub wave_wiring: bool,
    /// Append the profile's density rule to both worker heads.
    pub density: bool,
    /// Demanded-test headroom per shard, percent.
    pub quota_pct: u64,
    /// Module workers see only their own contract plus the contracts they
    /// call, instead of the full contract surface (input-token diet).
    /// Test shards keep the full surface. Breaks the shared byte-prefix on
    /// purpose — the A/B measures input tokens vs prefix-cache hits.
    pub scope_contracts: bool,
}

impl Default for PlanOptions {
    fn default() -> Self {
        Self {
            tests_see_code: false,
            wave_wiring: false,
            density: false,
            quota_pct: DEFAULT_TEST_QUOTA_PCT,
            scope_contracts: false,
        }
    }
}

/// Turn a (possibly saved) manifest into the executable wave. Public so a
/// run can replay a recorded manifest: planner-side changes then A/B on
/// identical architect output instead of re-rolling the planning call.
pub fn plan_from_manifest(
    objective: &str,
    manifest: &Manifest,
    max_tasks: usize,
    profile: Option<&'static Profile>,
    options: &PlanOptions,
) -> Result<Plan> {
    let profile = profile.unwrap_or_else(|| Profile::detect(objective));
    let mut manifest = manifest.clone();
    (profile.normalize_manifest)(&mut manifest);
    build_plan_with(objective, &manifest, max_tasks, profile, options)
}

/// Appended after the profile's edge rule with `ArchitectOptions::examples`. Edge rules
/// pin behavior but not representation: sitegen contracts said
/// `written: list[str]` and module/test workers split on URLs vs file paths
/// (most own-test failures, run). One literal call -> result settles it.
const EXAMPLE_RULE: &str = "
- When a def returns strings, a list or a dict, end its comment with one
  literal example showing the exact value format (paths vs URLs, ordering,
  key names), e.g.
  `# e.g. build(['/a/']) -> {'written': ['a/index.html']}`.";

/// Knobs for the architect call. Defaults reproduce the measured best path
/// except where a knob is still under A/B.
#[derive(Debug, Clone)]
pub struct ArchitectOptions {
    /// Skip the single-shot manifest; plan with skeleton + concurrent export
    /// fragments (the measured default, see `--architect-single-shot`).
    pub force_split: bool,
    pub cache_key: Option<String>,
    /// Identical single-shot requests raced per ladder step (1 = off).
    pub hedge: usize,
    /// Constrain fragments with a strict json_schema `response_format`.
    pub schema: bool,
    /// Reasoning effort for the first attempt of each fragment.
    pub fragment_effort: String,
    /// Export contracts pin edge behavior (see `Profile::edge_rule`).
    pub edges: bool,
    /// Export contracts end with one literal call -> result example (see
    /// `EXAMPLE_RULE`).
    pub examples: bool,
    /// Files per export fragment (see `--architect-files-per-fragment`).
    pub files_per_fragment: usize,
    /// Manifest rules pin shared-type ownership: each interface/type defined
    /// exactly once, imported by relative path elsewhere (A/B, see
    /// `Profile::type_ownership_rule`).
    pub type_ownership: bool,
    /// Take the skeleton from an objective that enumerates its modules
    /// (`(1) path.py - purpose; ...`) instead of a model call.
    pub objective_skeleton: bool,
    /// Stack profile; `None` detects it from the objective.
    pub profile: Option<&'static Profile>,
    /// After the export fragments, one call pins which other files'
    /// exports each consumer file (entry point, page, component) must call
    /// (see `wire_manifest`).
    pub wiring: bool,
}

impl Default for ArchitectOptions {
    fn default() -> Self {
        Self {
            force_split: true,
            cache_key: None,
            hedge: 1,
            // ABBA run: export-fragment schema, see `--architect-schema`.
            schema: true,
            // ABBA run: minimal vs low planning 12.5 vs 32.6s, 3.1k vs
            // 8.6k output tokens, 0 vs 2 retries.
            fragment_effort: "minimal".to_string(),
            // e2e ABBA runs, see `--architect-edges`.
            edges: true,
            examples: false,
            files_per_fragment: 2,
            type_ownership: false,
            objective_skeleton: true,
            profile: None,
            wiring: false,
        }
    }
}

/// The architect call alone: file layout + interface contracts. `hedge` > 1
/// fires that many identical single-shot manifest requests per ladder step
/// and keeps the first that parses (losers are dropped, closing their
/// streams): planning wall is the slowest phase end-to-end and swings from
/// ~18s to minutes across server windows, so racing copies trades a few
/// thousand planning tokens for the minimum of N latency draws.
pub async fn generate_manifest(
    client: &dyn ModelClient,
    objective: &str,
    max_tasks: usize,
    steering: &[String],
    options: &ArchitectOptions,
) -> Result<Manifest> {
    let force_split = options.force_split;
    let cache_key = options.cache_key.as_deref();
    let hedge = options.hedge;
    validate_steering(steering)?;
    if objective.trim().is_empty() {
        bail!("objective cannot be empty");
    }
    if !(1..=256).contains(&max_tasks) {
        bail!("max_tasks must be between 1 and 256");
    }

    let profile = options
        .profile
        .unwrap_or_else(|| Profile::detect(objective));
    let shared = architect_shared_context(max_tasks, profile, options.type_ownership);
    if force_split {
        return split_manifest(client, objective, &shared, options, profile).await;
    }

    let system = "You are the architect for a greenfield software build. You \
decide the file layout and the exact interface contracts. Return JSON only."
        .to_string();
    let steering_block = if steering.is_empty() {
        "None".to_string()
    } else {
        steering
            .iter()
            .enumerate()
            .map(|(index, directive)| format!("{}. {directive}", index + 1))
            .collect::<Vec<_>>()
            .join("\n")
    };
    let user = format!(
        r#"Design the file manifest for this objective:

<objective>
{objective}
</objective>

User steering directives:
<steering>
{steering_block}
</steering>

Return only this JSON shape:
Return only this JSON shape, without Markdown or comments:
{{
  "files": [
    {{
      "path": "{module}",
      "purpose": "one sentence: what this file is responsible for",
      "exports": ["{export}", "..."]
    }}
  ],
  "test_files": [
    {{
      "path": "{test}",
      "purpose": "what behavior this test file covers",
      "exports": []
    }}
  ],
  "readme": true
}}
"#,
        module = profile.example_module,
        export = profile.example_export,
        test = profile.example_test,
    );

    // The manifest for a ~12-file build runs 1.5-2.5k tokens of JSON; the
    // first attempt gets a budget that usually fits, the retry (medium
    // effort, more tokens) covers rambling or wider builds.
    let attempt = |effort: &str, max_tokens: u32| CompletionRequest {
        system: system.clone(),
        shared_context: Some(shared.clone()),
        user: user.clone(),
        max_tokens,
        temperature: 0.1,
        reasoning_effort: Some(effort.to_string()),
        prompt_cache_key: cache_key.map(str::to_string),
        allow_empty: false,
        response_format: None,
    };
    // Ladder: two low-effort shots, then two medium. Empty completions come
    // in multi-minute server bursts, so hiccups back off long (45s) instead
    // of hammering every 3s through the burst; other errors stay at 3s.
    let ladder = [
        ("low", 2_560),
        ("low", 2_560),
        ("medium", 4_096),
        ("medium", 4_096),
    ];
    let burst_backoff = backoff_for;
    let mut completion = None;
    for (index, (effort, budget)) in ladder.iter().enumerate() {
        let attempt_started = std::time::Instant::now();
        match complete_hedged(client, attempt(effort, *budget), hedge).await {
            Ok(done) => {
                tracing::info!(
                    attempt = index + 1,
                    hedge,
                    elapsed_ms = attempt_started.elapsed().as_millis() as u64,
                    chars = done.text.len(),
                    output_tokens = done.output_tokens,
                    reasoning_tokens = done.reasoning_tokens.unwrap_or(0),
                    "architect attempt returned"
                );
                completion = Some(done);
                break;
            }
            Err(error) => {
                let wait = burst_backoff(&error);
                tracing::warn!(
                    attempt = index + 1,
                    elapsed_ms = attempt_started.elapsed().as_millis() as u64,
                    backoff_ms = wait.as_millis() as u64,
                    "architect attempt failed: {error:#}"
                );
                if index + 1 < ladder.len() {
                    tokio::time::sleep(wait).await;
                }
            }
        }
    }
    let completion =
        completion.context("architect failed after 4 attempts (low/low/medium/medium)")?;

    let json = match parse_manifest(&completion.text) {
        Some(json) => json,
        None => {
            // Response arrived but isn't a usable manifest: log the tail for
            // diagnosis, then up to three medium-effort regenerations — the
            // empty-completion hiccups are intermittent, so back off and
            // re-arm rather than dying on one bad window.
            tracing::warn!(
                "architect manifest unparseable ({} chars, head: {:.300})",
                completion.text.len(),
                completion.text
            );
            let mut regenerated = None;
            for regen in 1..=3u32 {
                tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                let retry_started = std::time::Instant::now();
                match client.complete(attempt("medium", 4_096)).await {
                    Ok(retry) => {
                        tracing::info!(
                            regen,
                            elapsed_ms = retry_started.elapsed().as_millis() as u64,
                            chars = retry.text.len(),
                            "architect regeneration returned"
                        );
                        if let Some(json) = parse_manifest(&retry.text) {
                            regenerated = Some(json);
                            break;
                        }
                        tracing::warn!(
                            regen,
                            "architect regeneration still unparseable ({} chars)",
                            retry.text.len()
                        );
                    }
                    Err(error) => {
                        let wait = burst_backoff(&error);
                        tracing::warn!(
                            regen,
                            backoff_ms = wait.as_millis() as u64,
                            "architect regeneration failed: {error:#}"
                        );
                        if regen < 3 {
                            tokio::time::sleep(wait).await;
                        }
                    }
                }
            }
            match regenerated {
                Some(json) => json,
                None => {
                    // Single-shot manifests past ~3.5k chars stop mid-JSON
                    // (measured: finish=stop at 3531 chars with the token
                    // budget untouched — the model under-continues, and
                    // regenerating the same size reproduces the same cut).
                    // Emit the manifest in fragments small enough to
                    // complete reliably, then merge.
                    tracing::warn!(
                        "single-shot manifest unparseable; falling back to split manifest"
                    );
                    return split_manifest(client, objective, &shared, options, profile).await;
                }
            }
        }
    };
    manifest_from_json(&json)
}

/// Last `count` characters of `text` (for diagnostics).
fn tail_chars(text: &str, count: usize) -> String {
    let skip = text.chars().count().saturating_sub(count);
    text.chars().skip(skip).collect()
}

/// Race `copies` identical requests; the first completion whose text parses
/// as a manifest wins. If none parses, the first successful completion is
/// returned so the caller's regeneration path still sees it; if all fail,
/// the last error.
async fn complete_hedged(
    client: &dyn ModelClient,
    request: CompletionRequest,
    copies: usize,
) -> Result<Completion> {
    if copies <= 1 {
        return client.complete(request).await;
    }
    let mut racing: FuturesUnordered<_> = (0..copies)
        .map(|_| client.complete(request.clone()))
        .collect();
    let mut unparseable = None;
    let mut last_error = None;
    while let Some(result) = racing.next().await {
        match result {
            Ok(done) if parse_manifest(&done.text).is_some() => return Ok(done),
            Ok(done) => {
                unparseable.get_or_insert(done);
            }
            Err(error) => last_error = Some(error),
        }
    }
    match (unparseable, last_error) {
        (Some(done), _) => Ok(done),
        (None, Some(error)) => Err(error),
        (None, None) => bail!("hedged architect request produced no result"),
    }
}

/// Extract the JSON object and parse it as a Manifest. Low-effort models
/// wrap long export annotations across lines (literal newlines inside JSON
/// strings are invalid), add trailing commas, and sometimes emit Python-style
/// single-quoted dicts — so: strict pass, then the deterministic `jsonfix`
/// repair passes (domain purposes here are code signatures, essentially
/// apostrophe-free).
fn parse_manifest(text: &str) -> Option<String> {
    let json = extract_json_object(text).ok()?;
    if serde_json::from_str::<Manifest>(json).is_ok() {
        return Some(json.to_string());
    }
    jsonfix::repair(json).filter(|repaired| serde_json::from_str::<Manifest>(repaired).is_ok())
}

/// Complete one manifest fragment and return its repaired JSON. Three shots
/// (low/medium/low): measured bursts consistently let sub-1k-char completions
/// through while cutting longer ones, so small fragments get several tries
/// across the window instead of two. A shot that parses but fails `validate`
/// (e.g. an exports fragment that dropped one of its two files) counts as a
/// miss and burns another try rather than killing the run.
async fn complete_fragment<F>(
    client: &dyn ModelClient,
    label: &str,
    user: &str,
    shared: &str,
    options: &ArchitectOptions,
    validate: F,
) -> Result<String>
where
    F: Fn(&Manifest) -> bool,
{
    // Cleared when the server rejects json_schema (e.g. DeepSeek: "This
    // response_format type is unavailable now"); the step then reruns free.
    let mut schema = options.schema && label != "skeleton";
    let request = |effort: &str, max_tokens: u32, schema: bool| CompletionRequest {
        system: "You are the architect for a greenfield software build. You \
decide the file layout and the exact interface contracts. Return JSON only."
            .to_string(),
        shared_context: Some(shared.to_string()),
        user: user.to_string(),
        max_tokens,
        temperature: 0.1,
        reasoning_effort: Some(effort.to_string()),
        prompt_cache_key: options.cache_key.clone(),
        allow_empty: false,
        // The skeleton decides the file layout: under a strict schema Muse
        // merged the objective's named modules (10 -> 7 files in 2/2 runs,
        // ABBA run). Export fragments fill contracts for fixed paths, and
        // they are where unparseable JSON occurred, so only they are bound.
        response_format: schema.then(manifest_schema),
    };
    let first = options.fragment_effort.as_str();
    for (index, (effort, budget)) in [(first, 2_560), ("medium", 4_096), (first, 2_560)]
        .iter()
        .enumerate()
    {
        if index > 0 {
            tokio::time::sleep(std::time::Duration::from_secs(3)).await;
        }
        let mut outcome = client.complete(request(effort, *budget, schema)).await;
        if let Err(error) = &outcome {
            if schema && format!("{error:#}").contains("response_format") {
                tracing::warn!(
                    fragment = label,
                    "server rejected json_schema; continuing without it"
                );
                schema = false;
                outcome = client.complete(request(effort, *budget, schema)).await;
            }
        }
        match outcome {
            Ok(done) => {
                tracing::info!(
                    fragment = label,
                    attempt = index + 1,
                    chars = done.text.len(),
                    output_tokens = done.output_tokens,
                    reasoning_tokens = done.reasoning_tokens.unwrap_or(0),
                    "architect fragment returned"
                );
                if let Some(json) = parse_manifest(&done.text) {
                    if manifest_from_json(&json)
                        .map(|manifest| validate(&manifest))
                        .unwrap_or(false)
                    {
                        return Ok(json);
                    }
                    tracing::warn!(
                        fragment = label,
                        attempt = index + 1,
                        "architect fragment failed validation; retrying"
                    );
                } else {
                    tracing::warn!(
                        fragment = label,
                        attempt = index + 1,
                        chars = done.text.len(),
                        head = %done.text.chars().take(200).collect::<String>(),
                        tail = %tail_chars(&done.text, 200),
                        "architect fragment unparseable"
                    );
                    // repair the failed output with one small salvage
                    // call instead of letting the next ladder step regenerate
                    // the whole fragment. Same parse + validate gate as fresh
                    // output, so an accepted fragment is unchanged.
                    if let Ok(fixed) = jsonfix::salvage(
                        client,
                        &done.text,
                        *budget,
                        &options.fragment_effort,
                        schema.then(manifest_schema),
                    )
                    .await
                    {
                        let salvaged = parse_manifest(&fixed).filter(|json| {
                            manifest_from_json(json)
                                .map(|manifest| validate(&manifest))
                                .unwrap_or(false)
                        });
                        if let Some(json) = salvaged {
                            tracing::info!(
                                fragment = label,
                                attempt = index + 1,
                                "architect fragment salvaged"
                            );
                            return Ok(json);
                        }
                        tracing::warn!(
                            fragment = label,
                            attempt = index + 1,
                            "architect fragment salvage did not validate"
                        );
                    }
                }
            }
            Err(error) => {
                let wait = backoff_for(&error);
                tracing::warn!(
                    fragment = label,
                    attempt = index + 1,
                    backoff_ms = wait.as_millis() as u64,
                    "architect fragment failed: {error:#}"
                );
                if index < 2 {
                    tokio::time::sleep(wait).await;
                }
            }
        }
    }
    bail!("architect fragment `{label}` failed to produce a usable manifest")
}

/// The skeleton an objective already spells out as a numbered module list,
/// `(1) path.py - purpose; (2) ...`. Every Python bench objective is written
/// this way and the skeleton call returned exactly those files in 8/8 checked
/// runs, so the call (~3s and ~400 tokens on Muse) only restated the
/// objective. None unless every entry parses and there are at least three.
fn skeleton_from_objective(objective: &str, exts: &[&str]) -> Option<Manifest> {
    // Split on "(N) " markers numbered 1, 2, 3, ...
    let mut starts = Vec::new();
    let mut next = 1;
    let mut from = 0;
    loop {
        let marker = format!("({next}) ");
        let Some(found) = objective[from..].find(&marker) else {
            break;
        };
        let at = from + found;
        starts.push((at, at + marker.len()));
        from = at + marker.len();
        next += 1;
    }
    if starts.len() < 3 {
        return None;
    }
    let mut files = Vec::new();
    for (index, &(_, body_start)) in starts.iter().enumerate() {
        let body_end = starts
            .get(index + 1)
            .map_or(objective.len(), |&(marker, _)| marker);
        let entry = &objective[body_start..body_end];
        let (path, rest) = entry.split_once(char::is_whitespace)?;
        let valid_path = exts.iter().any(|ext| path.ends_with(ext))
            && path
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '/' | '.' | '-'));
        let rest = rest.trim_start();
        let purpose = rest
            .strip_prefix('-')
            .or_else(|| rest.strip_prefix('\u{2014}'))?
            .trim();
        // The last entry runs on into the rest of the objective: stop at the
        // first sentence break.
        let purpose = purpose.split(". ").next().unwrap_or(purpose);
        let purpose = purpose.trim_end_matches([';', '.', ' ']).trim();
        if !valid_path || purpose.split_whitespace().count() < 3 {
            return None;
        }
        files.push(ManifestFile {
            path: path.to_string(),
            purpose: purpose.to_string(),
            exports: Vec::new(),
            uses: Vec::new(),
        });
    }
    Some(Manifest {
        files,
        test_files: Vec::new(),
        readme: false,
    })
}

/// Emit an oversized manifest in fragments the model can complete reliably:
/// (1) skeleton — paths + purposes only; (2) exports two files at a time;
/// (3) test layout + readme given the full contracts. Merging validates
/// that every fragment covers exactly its assigned slice.
async fn split_manifest(
    client: &dyn ModelClient,
    objective: &str,
    shared: &str,
    options: &ArchitectOptions,
    profile: &Profile,
) -> Result<Manifest> {
    let skeleton_user = format!(
        r#"Design the file manifest for this objective.

<objective>
{objective}
</objective>

Return only this JSON shape — paths and one-sentence purposes ONLY, no
exports (they come in a later pass), no test files, readme false:
{{
  "files": [
    {{"path": "{module}", "purpose": "one sentence: what this file is responsible for"}}
  ],
  "test_files": [],
  "readme": false
}}
"#,
        module = profile.example_module,
    );
    let listed = options
        .objective_skeleton
        .then(|| skeleton_from_objective(objective, profile.listed_module_exts))
        .flatten();
    let mut skeleton = match listed {
        Some(skeleton) => {
            tracing::info!(
                files = skeleton.files.len(),
                "skeleton taken from the objective's module list (no model call)"
            );
            skeleton
        }
        None => manifest_from_json(
            &complete_fragment(
                client,
                "skeleton",
                &skeleton_user,
                shared,
                options,
                |manifest| !manifest.files.is_empty(),
            )
            .await?,
        )?,
    };
    // Before export fragments, so contracts are written against final paths.
    (profile.normalize_skeleton)(&mut skeleton);

    // Two files per exports fragment: measured bursts cut ~1k+ char
    // completions but consistently let sub-1k ones through, and 2 files x
    // ~4 annotated exports land around 700-900 chars.
    let pair = options.files_per_fragment.max(1);
    // Fragments are independent (each sees only its own files), so they run
    // concurrently: planning depth is skeleton + ONE fragment round instead
    // of one round per file pair.
    let mut fragment_requests = Vec::new();
    for (index, chunk) in skeleton.files.chunks(pair).enumerate() {
        let listing = chunk
            .iter()
            .map(|file| format!("- `{}`: {}", file.path, file.purpose))
            .collect::<Vec<_>>()
            .join("\n");
        let exports_user = format!(
            r#"Design the interface contracts for this objective.

<objective>
{objective}
</objective>

These files exist (path + purpose). Fill in the exact exports for EVERY
file listed — keep each file's given path and purpose verbatim:

{listing}

Fragment discipline (a later pass handles the other files — do NOT list
them, do NOT add files, do NOT add prose):
- ONLY the {count} file(s) listed above, nothing else.
- At most 4 exports per file, each ONE line{css_rule}.{edge_rule}{example_rule}
- The entire JSON must stay under {limit} characters — shorter is better.

Return only this JSON shape — the same files with their exports arrays:
{{
  "files": [
    {{
      "path": "{module}",
      "purpose": "same purpose as given",
      "exports": ["{export}  {comment} {{exact keys}}", "..."]
    }}
  ],
  "test_files": [],
  "readme": false
}}
"#,
            count = chunk.len(),
            edge_rule = if options.edges { profile.edge_rule } else { "" },
            module = profile.example_module,
            export = profile.example_export,
            comment = profile.comment,
            example_rule = if options.examples { EXAMPLE_RULE } else { "" },
            css_rule = profile.fragment_note,
            limit =
                1000 + if options.edges { 600 } else { 0 } + if options.examples { 600 } else { 0 },
        );
        let expected: std::collections::HashSet<String> =
            chunk.iter().map(|file| file.path.clone()).collect();
        fragment_requests.push(async move {
            let text = complete_fragment(
                client,
                &format!("exports-{index}"),
                &exports_user,
                shared,
                options,
                |manifest| {
                    manifest
                        .files
                        .iter()
                        .map(|file| file.path.clone())
                        .collect::<std::collections::HashSet<_>>()
                        == expected
                },
            )
            .await?;
            manifest_from_json(&text)
        });
    }
    let mut files = Vec::with_capacity(skeleton.files.len());
    for fragment in futures::future::try_join_all(fragment_requests).await? {
        files.extend(fragment.files);
    }
    if options.wiring {
        wire_manifest(client, objective, shared, options, &mut files).await;
    }

    // Test layout is deterministic: build_plan shards suites by module group
    // with quotas, so a model "tests" fragment would only add a sequential
    // call. Tests and README follow what the objective asks for.
    let lower = objective.to_lowercase();
    let test_files = if lower.contains("test") {
        vec![ManifestFile {
            path: profile.example_test.to_string(),
            purpose: "tests for every module".to_string(),
            exports: Vec::new(),
            uses: Vec::new(),
        }]
    } else {
        Vec::new()
    };
    Ok(Manifest {
        files,
        test_files,
        readme: lower.contains("readme"),
    })
}

/// Contracts pin what each file provides but not who calls it, so module
/// workers implement every export while the entry points and pages call only
/// some of them: in recorded runs 28-34 of 38-39 Python exports and 4-8 of
/// 21-26 Next.js exports were reachable from tests alone (a Recall review
/// page with no way to create cards, games never recorded in progress). One
/// call over the finished contracts names, per consumer file, the exports it
/// must call; the consumer's worker prompt then lists them. Best effort: a
/// failed call leaves the manifest unwired.
async fn wire_manifest(
    client: &dyn ModelClient,
    objective: &str,
    shared: &str,
    options: &ArchitectOptions,
    files: &mut [ManifestFile],
) {
    let user = wiring_prompt(objective, files);
    #[derive(Deserialize)]
    struct Wiring {
        calls: std::collections::BTreeMap<String, Vec<String>>,
    }
    // Its own answer shape: asked for the manifest's `files`/`exports`
    // shape, the model echoed the whole contract list back (2 of 4 Python
    // runs, ~2.2k tokens, nothing wired).
    let request = CompletionRequest {
        system: "You are the architect for a greenfield software build. You \
decide the file layout and the exact interface contracts. Return JSON only."
            .to_string(),
        shared_context: Some(shared.to_string()),
        user,
        max_tokens: 4_096,
        temperature: 0.1,
        reasoning_effort: Some(options.fragment_effort.clone()),
        prompt_cache_key: options.cache_key.clone(),
        allow_empty: false,
        response_format: None,
    };
    let mut answer = None;
    for attempt in 1..=2 {
        match client.complete(request.clone()).await {
            Ok(done) => {
                tracing::info!(
                    fragment = "wiring",
                    attempt,
                    chars = done.text.len(),
                    output_tokens = done.output_tokens,
                    reasoning_tokens = done.reasoning_tokens.unwrap_or(0),
                    "architect fragment returned"
                );
                let parsed = extract_json_object(&done.text)
                    .ok()
                    .and_then(|json| serde_json::from_str::<Wiring>(json).ok());
                if parsed.is_some() {
                    answer = parsed.map(|wiring| (wiring, done.text));
                    break;
                }
                tracing::warn!(attempt, head = %done.text.chars().take(300).collect::<String>(), "wiring pass unparseable");
            }
            Err(error) => tracing::warn!(attempt, "wiring pass failed: {error:#}"),
        }
    }
    let Some((wiring, text)) = answer else {
        tracing::warn!("contracts stay unwired");
        return;
    };
    let provided: std::collections::HashMap<String, Vec<String>> = files
        .iter()
        .map(|file| (file.path.clone(), file.exports.clone()))
        .collect();
    let mut wired = 0;
    for (consumer, entries) in wiring.calls {
        let uses: Vec<String> = entries
            .iter()
            .filter_map(|entry| normalize_use(entry, &consumer, &provided))
            .collect();
        if let Some(file) = files.iter_mut().find(|file| file.path == consumer) {
            wired += uses.len();
            file.uses = uses;
        }
    }
    if wired == 0 {
        // Every entry failed normalization: keep a sample to see the shape.
        tracing::warn!(head = %text.chars().take(400).collect::<String>(), "wiring pass pinned nothing");
    } else {
        tracing::info!(calls = wired, "wiring pass pinned consumer calls");
    }
}

/// The wiring question over the finished contracts (see `wire_manifest`);
/// also the prompt of the in-wave wiring task (`--architect-wiring wave`).
pub fn wiring_prompt(objective: &str, files: &[ManifestFile]) -> String {
    let contracts = files
        .iter()
        .map(|file| format!("- `{}`: {}", file.path, file.exports.join(" | ")))
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        r#"Wire this program together.

<objective>
{objective}
</objective>

Contracts (path: exports):
{contracts}

For every file that presents features to the user or drives the program
(entry points, CLI commands, pages, UI components), list the exports of
OTHER files it must call so that every feature the objective asks for is
reachable from the running app, not only from tests. Every export that
implements a user-facing feature must be called by at least one listed file.
Name each export as `path:name` with the bare name only (no signature, no
types), e.g. `lib/store.ts:loadState`. Omit pure library files that call
nothing. Keep the JSON compact.

Return only this JSON object, keyed by consumer path:
{{"calls": {{"<consumer path>": ["<other path>:<export name>", "..."]}}}}
"#
    )
}

/// `path:name` for a wiring entry whose path is another manifest file and
/// whose name appears among that file's exports; `None` otherwise. Accepts
/// entries the model decorated with a signature (`path:name(args): T`).
fn normalize_use(
    entry: &str,
    consumer: &str,
    provided: &std::collections::HashMap<String, Vec<String>>,
) -> Option<String> {
    let (path, rest) = entry.trim().trim_matches('`').split_once(':')?;
    let name: String = rest
        .trim()
        .chars()
        .take_while(|ch| ch.is_alphanumeric() || *ch == '_' || *ch == '$')
        .collect();
    let exports = provided.get(path)?;
    let declared = exports.iter().any(|export| {
        export
            .split(|ch: char| !(ch.is_alphanumeric() || ch == '_' || ch == '$'))
            .any(|word| word == name)
    });
    (path != consumer && !name.is_empty() && declared).then(|| format!("{path}:{name}"))
}

/// Strict json_schema for manifest fragments: Muse then emits parseable
/// JSON by construction (every property required, no extras).
fn manifest_schema() -> serde_json::Value {
    let file = serde_json::json!({
        "type": "object",
        "properties": {
            "path": {"type": "string"},
            "purpose": {"type": "string"},
            "exports": {"type": "array", "items": {"type": "string"}}
        },
        "required": ["path", "purpose", "exports"],
        "additionalProperties": false
    });
    serde_json::json!({
        "type": "json_schema",
        "json_schema": {
            "name": "manifest",
            "strict": true,
            "schema": {
                "type": "object",
                "properties": {
                    "files": {"type": "array", "items": file},
                    "test_files": {"type": "array", "items": file},
                    "readme": {"type": "boolean"}
                },
                "required": ["files", "test_files", "readme"],
                "additionalProperties": false
            }
        }
    })
}

fn manifest_from_json(json: &str) -> Result<Manifest> {
    serde_json::from_str(json).context("architect returned invalid manifest JSON")
}

/// Deterministically turn a manifest into a zero-dependency plan: every
/// worker codes against the published contracts, so no task needs another
/// task's output and all workers run in one parallel wave.
fn build_plan(objective: &str, manifest: &Manifest, max_tasks: usize) -> Result<Plan> {
    build_plan_with(
        objective,
        manifest,
        max_tasks,
        Profile::detect(objective),
        &PlanOptions::default(),
    )
}

/// `tests_see_code` / `wave_wiring` / `density` / `quota_pct`: see
/// `PlanOptions` — each knob is A/B'd there.
fn build_plan_with(
    objective: &str,
    manifest: &Manifest,
    max_tasks: usize,
    profile: &Profile,
    options: &PlanOptions,
) -> Result<Plan> {
    let mut seen = std::collections::HashSet::new();
    for file in manifest.files.iter().chain(manifest.test_files.iter()) {
        let path = file.path.trim();
        if path.is_empty() || path.contains("..") || path.starts_with('/') || path.starts_with('\\')
        {
            bail!("architect returned an unsafe path: {:?}", file.path);
        }
        if !seen.insert(path.to_string()) {
            bail!("architect returned a duplicate path: {path}");
        }
    }
    let readme_count = usize::from(manifest.readme);
    let total = manifest.files.len() + manifest.test_files.len() + readme_count;
    // The budget sizes the planner's layout; it is not a hard wall. A plan a
    // task or two over it (nextjs-memory-5: 21 vs 20, planning already paid
    // for) runs with a warning; only a runaway layout aborts. The README is
    // rendered (zero model cost) and never counts.
    let model_tasks = total - readme_count;
    let ceiling = max_tasks + max_tasks.div_ceil(4);
    if model_tasks == 0 || model_tasks > ceiling {
        bail!("architect task count {model_tasks} exceeds the budget of {max_tasks} (ceiling {ceiling})");
    }
    if model_tasks > max_tasks {
        tracing::warn!(
            model_tasks,
            max_tasks,
            "architect plan over the task budget; running it"
        );
    }

    let mut tasks = Vec::with_capacity(total);
    // Cache-aware layout: everything byte-identical across module workers
    // (the full contract surface + the rules) comes FIRST; the one per-file
    // instruction is the tail. Any task-specific text in the shared region
    // would make every worker's request diverge at that point and forfeit the
    // server-side prefix cache for the rest. The head is ALSO stored as a
    // cache prime: completed once for ~1 token before the wave so the server
    // has the prefix stored deterministically (a simultaneous wave otherwise
    // races — measured: only ~1 in 12 workers ever hits the long prefix).
    let all = manifest.all_modules();
    let mut module_head = profile.module_head.replace("{all}", &all);
    if options.density {
        module_head.push_str(profile.density_rule);
    }
    // scoped variant: a module worker sees only its own contract plus
    // the contracts of files it calls (same per-file line shape as
    // `all_modules`). Deterministic manifest order keeps the text stable.
    let scoped_modules = |file: &ManifestFile| {
        let used: Vec<&str> = file
            .uses
            .iter()
            .filter_map(|entry| entry.split(':').next())
            .collect();
        manifest
            .files
            .iter()
            .filter(|f| f.path == file.path || used.contains(&f.path.as_str()))
            .map(|f| format!("- `{}`: {}", f.path, manifest.exports_inline(f)))
            .collect::<Vec<_>>()
            .join("\n")
    };
    // One prime per unique worker head so every family's prefix is stored
    // before the wave (unscoped: the single shared head, as before).
    let mut module_primes: Vec<String> = Vec::new();
    let wired_in_wave = options.wave_wiring
        && manifest
            .files
            .iter()
            .any(|file| file.uses.is_empty() && (profile.is_consumer)(&file.path));
    if wired_in_wave {
        tasks.push(Task {
            id: WIRING_TASK.to_string(),
            title: "wiring plan".to_string(),
            prompt: wiring_prompt(objective, &manifest.files),
            deps: Vec::new(),
            // the wiring answer is a small JSON map (consumer path ->
            // exports to call); 4096 let a 2.2s decode tail run past
            // the map. 1536 is generous headroom and shortens the consumer
            // wait.
            max_tokens: 1_536,
            temperature: Some(0.1),
            require_closed_fences: false,
            validate_artifact: false,
            static_output: None,
            reasoning_effort: None,
            internal: true,
        });
    }
    for file in &manifest.files {
        let head = if options.scope_contracts {
            let scoped = profile.module_head.replace("{all}", &scoped_modules(file));
            let head = if options.density {
                format!("{scoped}{}", profile.density_rule)
            } else {
                scoped
            };
            // Cache primes are byte-prefixes of the family prompts; dedup
            // identical heads (files with the same own+called contracts).
            if !module_primes.iter().any(|prime| prime == &head) {
                module_primes.push(head.clone());
            }
            head
        } else {
            module_head.clone()
        };
        let wave_consumer = wired_in_wave
            && file.uses.is_empty()
            && (profile.is_consumer)(&file.path)
            && (profile.scaffold)(&file.path).is_none();
        let prompt = format!(
            "{head}Now write the COMPLETE contents of `{path}` — ONLY that file.\n\
             - Purpose: {purpose}\n- Exports (implement exactly these): {exports}{uses}",
            path = file.path,
            purpose = file.purpose,
            exports = manifest.exports_inline(file),
            uses = if wave_consumer {
                format!(
                    "\n- Calls: the prerequisite output `{WIRING_TASK}` maps consumer paths \
                     to the exports each must call; wire every call it lists under `{}` \
                     into this file's behavior, importing each from its file",
                    file.path
                )
            } else if file.uses.is_empty() {
                String::new()
            } else {
                format!(
                    "\n- Calls (wire each of these into this file's behavior; import \
                     them from their files): {}",
                    file.uses.join(", ")
                )
            },
        );
        tasks.push(Task {
            id: task_id(&file.path),
            title: file.path.clone(),
            prompt,
            deps: if wave_consumer {
                vec![WIRING_TASK.to_string()]
            } else {
                Vec::new()
            },
            // Roomier than the old planner defaults: hitting max_tokens
            // mid-fence reads as an unclosed-fence truncation failure. The
            // cap is a safety bound, not a target — decode length is what
            // the worker writes (~1.2k for a small module, ~2.5k for a fat
            // one); wide builds need the headroom.
            max_tokens: 4_096,
            temperature: Some(0.2),
            require_closed_fences: true,
            validate_artifact: (profile.validates)(&file.path),
            static_output: (profile.scaffold)(&file.path),
            reasoning_effort: None,
            internal: false,
        });
    }
    let visibility = if options.tests_see_code {
        "the implementations of the modules you test are attached below as \
         prerequisite outputs"
    } else {
        "you cannot see the implementations"
    };
    let silent = if options.tests_see_code {
        "- Where a contract is silent (representation, ordering, edge \
         semantics), assert what the attached implementation actually does; \
         where the contract is explicit, the contract wins.\n"
    } else {
        ""
    };
    let mut test_head = profile
        .test_head
        .replace("{visibility}", visibility)
        .replace("{silent}", silent)
        .replace("{all}", &all);
    if options.density {
        test_head.push_str(profile.density_rule);
    }
    let suite = profile.suite;
    // Test sharding by module group: one suite task per ~30 demanded tests,
    // each covering a contiguous group of modules with an explicit quota.
    // The single 5-8k-token suite was the wave's tail in every recorded run
    // (47-52s of a ~60s wave), and manifest-chosen shards each covered ALL
    // modules with no quota (huge-arm2 wrote 311 tests against a demand of
    // 120). The head stays byte-identical across shards (cache prime), the
    // module list and quota are the tail.
    for shard in test_shards(objective, manifest, profile, options.quota_pct) {
        let modules = shard
            .modules
            .iter()
            .map(|path| format!("`{path}`"))
            .collect::<Vec<_>>()
            .join(", ");
        // Shards that see the code overshot "about N" by ~50% (run: 204-302
        // tests vs ~160 written blind): the ceiling holds tokens to the quota.
        let ceiling = if options.tests_see_code {
            format!(" (at most {})", shard.quota + shard.quota / 5)
        } else {
            String::new()
        };
        let prompt = format!(
            "{test_head}Write a {suite} suite saved as `{path}` covering ONLY these modules: \
             {modules}. Other suites cover the remaining modules. Write about {quota} \
             focused tests{ceiling}.",
            path = shard.path,
            quota = shard.quota,
        );
        let deps = if options.tests_see_code {
            shard.modules.iter().map(|path| task_id(path)).collect()
        } else {
            Vec::new()
        };
        tasks.push(Task {
            id: task_id(&shard.path),
            title: shard.path.clone(),
            prompt,
            deps,
            // The cap only bounds truncation — decode length is what the
            // model writes; a quota'd shard runs ~2-3k tokens.
            max_tokens: 8_192,
            temperature: Some(0.2),
            require_closed_fences: true,
            // Test shards are where syntax slips actually appeared in arms
            // (one-shot 8k-token suites) — validate them like modules.
            validate_artifact: (profile.validates)(&shard.path),
            static_output: None,
            reasoning_effort: None,
            internal: false,
        });
    }
    if manifest.readme {
        // Rendered from the manifest: the model README cost a call, a slot
        // and ~1k tokens to restate what the manifest already says.
        tasks.push(Task {
            id: "readme".to_string(),
            title: "README.md".to_string(),
            prompt: "Write the project's README.".to_string(),
            deps: Vec::new(),
            max_tokens: 1_024,
            temperature: Some(0.2),
            require_closed_fences: true,
            validate_artifact: false,
            static_output: Some(render_readme(objective, manifest, profile)),
            reasoning_effort: None,
            internal: false,
        });
    }

    let mut cache_primes = Vec::new();
    if !manifest.files.is_empty() {
        if options.scope_contracts {
            cache_primes.extend(module_primes);
        } else {
            cache_primes.push(module_head);
        }
    }
    if !manifest.test_files.is_empty() {
        cache_primes.push(test_head);
    }
    let plan = Plan {
        objective: objective.to_string(),
        tasks,
        cache_primes,
    };
    plan.validate_with_limit(
        ceiling + readme_count + MAX_TEST_SHARDS + usize::from(wired_in_wave),
    )?;
    Ok(plan)
}

/// Task id of the in-wave wiring call (`--architect-wiring wave`).
pub const WIRING_TASK: &str = "wiring";

/// Measured default demanded-test headroom per shard: quota 100 still MET
/// demand 6/6 arms with -4.4% tokens (A/B, pipeline DeepSeek 3 pairs);
/// re-probe on Muse before assuming it holds there.
pub const DEFAULT_TEST_QUOTA_PCT: u64 = 100;

/// Upper bound on generated test suites; also the headroom build_plan adds
/// to the manifest's task budget (shards expand execution, not planning).
const MAX_TEST_SHARDS: usize = 6;

/// Demanded tests per shard: a quota'd shard of ~30 compact tests is ~2-3k
/// output tokens, about the size of a fat module, so no shard is the tail.
const TESTS_PER_SHARD: u64 = 30;

struct TestShard {
    path: String,
    modules: Vec<String>,
    quota: u64,
}

/// Partition the modules into contiguous groups, one test suite per group.
/// No test files in the manifest means the objective asked for none.
fn test_shards(
    objective: &str,
    manifest: &Manifest,
    profile: &Profile,
    quota_pct: u64,
) -> Vec<TestShard> {
    let modules: Vec<&ManifestFile> = manifest
        .files
        .iter()
        .filter(|file| (profile.is_testable)(&file.path))
        .collect();
    if manifest.test_files.is_empty() || modules.is_empty() {
        return Vec::new();
    }
    let demand = demanded_test_count(objective).max(1);
    let wanted = demand.div_ceil(TESTS_PER_SHARD) as usize;
    let count = wanted
        .max(manifest.test_files.len())
        .clamp(1, MAX_TEST_SHARDS)
        .min(modules.len());
    // Headroom over the demand so a few weak tests don't miss it; the
    // measured value is a plan-build knob (A/B,
    // `--architect-test-quota`).
    let quota = (demand * quota_pct).div_ceil(100 * count as u64).max(3);
    let (base, extra) = (modules.len() / count, modules.len() % count);
    let mut shards = Vec::with_capacity(count);
    let mut next = 0;
    for index in 0..count {
        let size = base + usize::from(index < extra);
        let group: Vec<String> = modules[next..next + size]
            .iter()
            .map(|file| file.path.clone())
            .collect();
        next += size;
        let path = if count == 1 {
            manifest.test_files[0].path.clone()
        } else {
            (profile.shard_path)(index + 1, &group[0])
        };
        shards.push(TestShard {
            path,
            modules: group,
            quota,
        });
    }
    shards
}

/// Deterministic README from the manifest, in the same single fenced
/// Markdown block a README worker returns.
fn render_readme(objective: &str, manifest: &Manifest, profile: &Profile) -> String {
    use std::fmt::Write as _;
    let summary = objective
        .split_inclusive(['.', '\n'])
        .next()
        .unwrap_or(objective)
        .trim();
    let mut readme = String::new();
    let _ = writeln!(readme, "```markdown\n# Project\n\n{summary}\n\n## Files\n");
    for file in &manifest.files {
        let _ = writeln!(readme, "- `{}` — {}", file.path, file.purpose.trim());
    }
    readme.push_str(&(profile.readme_sections)(manifest));
    readme
}

/// Stable, filesystem-friendly task IDs from paths.
fn task_id(path: &str) -> String {
    path.chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                ch
            } else {
                '-'
            }
        })
        .collect()
}

fn extract_json_object(text: &str) -> Result<&str> {
    jsonfix::extract_object(text).context("architect response did not contain a JSON object")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest() -> Manifest {
        serde_json::from_value(serde_json::json!({
            "files": [
                {"path": "expenses.py", "purpose": "core", "exports": ["def add(date, cat, amount) -> dict"]},
                {"path": "cli.py", "purpose": "front end", "exports": ["def main(argv=None) -> int"]}
            ],
            "test_files": [
                {"path": "tests/test_all.py", "purpose": "all tests"}
            ],
            "readme": true
        }))
        .unwrap()
    }

    #[test]
    fn plan_is_a_single_zero_dependency_wave() {
        let plan = build_plan("obj", &manifest(), 16).unwrap();
        assert_eq!(plan.tasks.len(), 4);
        assert!(plan.tasks.iter().all(|task| task.deps.is_empty()));
        assert!(plan.tasks.iter().all(|task| task.require_closed_fences));
        let ids: std::collections::HashSet<_> = plan.tasks.iter().map(|t| t.id.as_str()).collect();
        assert_eq!(ids.len(), 4, "task ids are unique");
        assert!(ids.contains("expenses-py"));
    }

    fn wide_manifest(modules: usize) -> Manifest {
        Manifest {
            files: (0..modules)
                .map(|i| ManifestFile {
                    path: format!("pkg/m{i}.py"),
                    purpose: format!("module {i}"),
                    exports: vec![format!("def f{i}() -> int")],
                    uses: Vec::new(),
                })
                .collect(),
            test_files: vec![ManifestFile {
                path: "tests/test_all.py".into(),
                purpose: "all".into(),
                exports: vec![],
                uses: Vec::new(),
            }],
            readme: true,
        }
    }

    #[test]
    fn numbered_module_lists_become_the_skeleton() {
        let objective = "Build a CLI. Modules: (1) games/sequence.py - Simon-style \
            sequence memory: growing sequences, replay; (2) stats.py - per-game \
            statistics with JSON persistence; (3) cli.py - argparse front-end wiring \
            every game. Include a pytest suite of at least 12 tests.";
        let skeleton = skeleton_from_objective(objective, &[".py"]).expect("listed modules");
        let paths: Vec<_> = skeleton.files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(paths, ["games/sequence.py", "stats.py", "cli.py"]);
        assert_eq!(
            skeleton.files[0].purpose,
            "Simon-style sequence memory: growing sequences, replay"
        );
        assert_eq!(
            skeleton.files[2].purpose,
            "argparse front-end wiring every game"
        );
        // Prose objectives, short lists and non-module entries fall back.
        assert!(skeleton_from_objective("Build a todo app with tests.", &[".py"]).is_none());
        assert!(
            skeleton_from_objective("(1) a.py - one two three; (2) b.py - x y z", &[".py"])
                .is_none()
        );
        assert!(skeleton_from_objective(
            "(1) a.py - one two three; (2) b.py - four five six; (3) the UI - seven eight nine",
            &[".py"],
        )
        .is_none());
    }

    #[test]
    fn nextjs_objective_gets_typescript_rules_and_vitest_shards() {
        let file = |path: &str| ManifestFile {
            path: path.into(),
            purpose: "x".into(),
            exports: vec![],
            uses: Vec::new(),
        };
        let manifest = Manifest {
            files: vec![
                file("package.json"),
                file("lib/srs.ts"),
                file("lib/games.ts"),
                file("app/page.tsx"),
                file("next.config.mjs"),
            ],
            test_files: vec![file("tests/all.test.ts")],
            readme: true,
        };
        let objective = "Build a Next.js app with at least 60 tests.";
        assert_eq!(Profile::detect(objective).name, "nextjs");
        let plan = build_plan(objective, &manifest, 16).unwrap();
        let shards: Vec<_> = plan
            .tasks
            .iter()
            .filter(|t| t.title.starts_with("tests/"))
            .collect();
        assert_eq!(shards.len(), 2, "only the two lib modules are testable");
        for shard in &shards {
            assert!(shard.title.ends_with(".test.ts"), "{}", shard.title);
            assert!(shard.prompt.contains("Vitest suite"));
            let covered = shard
                .prompt
                .split("covering ONLY these modules:")
                .nth(1)
                .unwrap();
            assert!(!covered.contains("app/page.tsx"));
            assert!(!shard.validate_artifact);
        }
        let page = plan
            .tasks
            .iter()
            .find(|t| t.title == "app/page.tsx")
            .unwrap();
        assert!(page.prompt.contains("use client"));
        assert!(!page.prompt.contains("Python"));
        assert!(!page.validate_artifact);
        let readme = plan.tasks.iter().find(|t| t.id == "readme").unwrap();
        assert!(readme
            .static_output
            .as_deref()
            .unwrap()
            .contains("npm run dev"));
        let package = plan
            .tasks
            .iter()
            .find(|t| t.title == "package.json")
            .unwrap();
        let package = package.static_output.as_deref().unwrap();
        assert!(package.contains("\"next\": \"^16.4.3\""), "{package}");
        assert!(page.static_output.is_none());
    }

    #[test]
    fn tests_see_code_shards_depend_on_their_modules() {
        let objective = "Include at least 120 tests.";
        let plan = build_plan_with(
            objective,
            &wide_manifest(10),
            12,
            Profile::detect(objective),
            &PlanOptions {
                tests_see_code: true,
                ..PlanOptions::default()
            },
        )
        .unwrap();
        for shard in plan.tasks.iter().filter(|t| t.title.starts_with("tests/")) {
            assert!(!shard.deps.is_empty());
            for dep in &shard.deps {
                let module = plan.tasks.iter().find(|t| &t.id == dep).unwrap();
                assert!(shard.prompt.contains(&format!("`{}`", module.title)));
            }
            assert!(shard.prompt.contains("attached below"));
            assert!(shard.prompt.contains("(at most "));
        }
        let blind = build_plan(objective, &wide_manifest(10), 12).unwrap();
        assert!(blind.tasks.iter().all(|task| task.deps.is_empty()));
        assert!(blind
            .tasks
            .iter()
            .all(|task| !task.prompt.contains("attached below")));
    }

    #[test]
    fn wide_test_demand_shards_by_module_group_with_quotas() {
        let plan = build_plan("Include at least 120 tests.", &wide_manifest(10), 12).unwrap();
        let shards: Vec<_> = plan
            .tasks
            .iter()
            .filter(|t| t.title.starts_with("tests/"))
            .collect();
        assert_eq!(shards.len(), 4, "120 demanded / 30 per shard");
        // Every module is covered by exactly one shard.
        for i in 0..10 {
            let needle = format!("`pkg/m{i}.py`");
            let covering = shards
                .iter()
                .filter(|t| {
                    t.prompt
                        .rsplit("ONLY these modules:")
                        .next()
                        .unwrap()
                        .contains(&needle)
                })
                .count();
            assert_eq!(covering, 1, "module {i} covered once");
        }
        // Quota = ceil(120 * 1.00 / 4) = 30 (default 100%), and the
        // shared head is intact.
        assert!(shards
            .iter()
            .all(|t| t.prompt.contains("about 30 focused tests")));
        assert!(shards
            .iter()
            .all(|t| t.prompt.starts_with(plan.cache_primes[1].as_str())));
        assert!(plan.validate().is_ok());
    }

    #[test]
    fn small_test_demand_keeps_one_suite_at_the_manifest_path() {
        let plan = build_plan("obj", &manifest(), 16).unwrap();
        let suites: Vec<_> = plan
            .tasks
            .iter()
            .filter(|t| t.title.starts_with("tests/"))
            .collect();
        assert_eq!(suites.len(), 1);
        assert_eq!(suites[0].title, "tests/test_all.py");
    }

    #[test]
    fn readme_is_rendered_from_the_manifest_without_a_model_call() {
        let plan = build_plan("Build a tracker. More text.", &manifest(), 16).unwrap();
        let readme = plan.tasks.iter().find(|t| t.id == "readme").unwrap();
        let text = readme.static_output.as_deref().expect("static README");
        assert!(text.starts_with("```markdown\n# Project\n\nBuild a tracker."));
        assert!(text.contains("- `expenses.py` — core"));
        assert!(text.contains("python cli.py --help"));
        assert!(text.trim_end().ends_with("```"));
        // Balanced fences: the assemble extractor must see one closed block.
        assert_eq!(text.lines().filter(|l| l.starts_with("```")).count() % 2, 0);
    }

    #[test]
    fn cache_primes_are_byte_prefixes_of_their_family_prompts() {
        let plan = build_plan("obj", &manifest(), 16).unwrap();
        assert_eq!(plan.cache_primes.len(), 2, "module + test families");

        // Module tasks are pushed first, one per manifest file.
        for task in plan.tasks.iter().take(2) {
            assert!(
                task.prompt.starts_with(plan.cache_primes[0].as_str()),
                "module task {} diverges from the module prime",
                task.id
            );
        }
        let tests = plan
            .tasks
            .iter()
            .find(|t| t.id == "tests-test_all-py")
            .expect("test task");
        assert!(
            tests.prompt.starts_with(plan.cache_primes[1].as_str()),
            "test task diverges from the test prime"
        );
        // The readme task is in neither family and must not be primed by
        // a head it does not start with.
        assert!(plan.cache_primes[0].starts_with("Program contracts"));
        assert!(plan.cache_primes[1].starts_with("Module contracts"));
    }

    #[test]
    fn module_workers_see_only_the_contracts_they_need() {
        let plan = build_plan("obj", &manifest(), 16).unwrap();
        let expenses = plan
            .tasks
            .iter()
            .find(|t| t.id == "expenses-py")
            .expect("expenses task");
        assert!(expenses
            .prompt
            .contains("def add(date, cat, amount) -> dict"));
        // Other modules appear as a compact signature list, not the full
        // manifest with purposes and test planning.
        assert!(expenses
            .prompt
            .contains("`cli.py`: def main(argv=None) -> int"));
        assert!(!expenses.prompt.contains("front end"));
        // The shared whole-manifest block is gone from worker prompts.
        assert!(!expenses.prompt.contains("Program manifest"));
    }

    #[test]
    fn wiring_entries_normalize_to_declared_exports() {
        let provided: std::collections::HashMap<String, Vec<String>> = [(
            "lib/money.ts".to_string(),
            vec!["export function formatCents(cents: number): string".to_string()],
        )]
        .into();
        let norm = |entry| normalize_use(entry, "app/page.tsx", &provided);
        assert_eq!(
            norm("lib/money.ts:formatCents").as_deref(),
            Some("lib/money.ts:formatCents")
        );
        assert_eq!(
            norm("`lib/money.ts:formatCents(cents: number): string`").as_deref(),
            Some("lib/money.ts:formatCents")
        );
        assert_eq!(norm("lib/money.ts:parseCents"), None, "not declared");
        assert_eq!(norm("lib/other.ts:formatCents"), None, "unknown file");
        assert_eq!(
            normalize_use("lib/money.ts:formatCents", "lib/money.ts", &provided),
            None
        );
    }

    #[test]
    fn wave_wiring_makes_only_consumers_wait_for_the_wiring_task() {
        let file = |path: &str| ManifestFile {
            path: path.into(),
            purpose: "x".into(),
            exports: vec!["export function f(): number".into()],
            uses: Vec::new(),
        };
        let manifest = Manifest {
            files: vec![
                file("lib/a.ts"),
                file("app/page.tsx"),
                file("components/Nav.tsx"),
            ],
            test_files: vec![file("tests/all.test.ts")],
            readme: false,
        };
        let objective = "Build a Next.js app with tests.";
        let plan = build_plan_with(
            objective,
            &manifest,
            16,
            &crate::profile::NEXTJS,
            &PlanOptions {
                wave_wiring: true,
                ..PlanOptions::default()
            },
        )
        .unwrap();
        let task = |title: &str| plan.tasks.iter().find(|t| t.title == title).unwrap();
        let wiring = plan.tasks.iter().find(|t| t.id == WIRING_TASK).unwrap();
        assert!(wiring.deps.is_empty() && wiring.prompt.contains("Wire this program together."));
        assert!(
            wiring.internal,
            "the wiring plan is not part of the deliverable"
        );
        assert!(task("lib/a.ts").deps.is_empty(), "libraries start at once");
        assert_eq!(task("app/page.tsx").deps, [WIRING_TASK]);
        assert!(task("app/page.tsx").prompt.contains("under `app/page.tsx`"));
        assert_eq!(task("components/Nav.tsx").deps, [WIRING_TASK]);
        assert!(plan
            .tasks
            .iter()
            .filter(|t| t.title.starts_with("tests/"))
            .all(|t| t.deps.is_empty()));
        let off = build_plan_with(
            objective,
            &manifest,
            16,
            &crate::profile::NEXTJS,
            &PlanOptions::default(),
        )
        .unwrap();
        assert!(off
            .tasks
            .iter()
            .all(|t| t.id != WIRING_TASK && t.deps.is_empty()));
    }

    #[test]
    fn density_rule_appends_to_both_heads_and_stays_prime_aligned() {
        let dense = build_plan_with(
            "obj",
            &manifest(),
            16,
            Profile::detect("obj"),
            &PlanOptions {
                density: true,
                ..PlanOptions::default()
            },
        )
        .unwrap();
        let plain = build_plan("obj", &manifest(), 16).unwrap();
        for plan in [&dense, &plain] {
            let is_dense = std::ptr::eq(plan, &dense);
            assert_eq!(
                plan.tasks[0].prompt.contains("Density: no docstrings"),
                is_dense,
                "density clause only with the flag"
            );
            // The clause is constant, so every cache prime stays a
            // byte-prefix of its family prompts.
            for prime in &plan.cache_primes {
                assert!(
                    plan.tasks
                        .iter()
                        .any(|t| t.prompt.starts_with(prime.as_str())),
                    "prime must be a byte-prefix of a family prompt: {prime:.40}"
                );
                assert_eq!(prime.contains("Density: no docstrings"), is_dense);
            }
        }
        let shard = dense
            .tasks
            .iter()
            .find(|t| t.title.starts_with("tests/"))
            .unwrap();
        assert!(shard.prompt.contains("Density: no docstrings"));
    }

    #[test]
    fn scoped_contracts_limit_the_head_and_keep_primes_aligned() {
        let mut scoped = manifest();
        scoped.files[0].uses = vec!["cli.py:main".to_string()];
        scoped.files[1].uses = vec!["nobody.py:helper".to_string()]; // not a file
        let plan = build_plan_with(
            "obj",
            &scoped,
            16,
            Profile::detect("obj"),
            &PlanOptions {
                scope_contracts: true,
                ..PlanOptions::default()
            },
        )
        .unwrap();
        let expenses = plan
            .tasks
            .iter()
            .find(|t| t.title == "expenses.py")
            .unwrap();
        let cli = plan.tasks.iter().find(|t| t.title == "cli.py").unwrap();
        // expenses calls cli.py -> sees both contracts; cli calls nothing
        // real -> sees only its own.
        assert!(expenses.prompt.contains("- `expenses.py`"));
        assert!(expenses.prompt.contains("- `cli.py`"));
        assert!(cli.prompt.contains("- `cli.py`"));
        assert!(!cli.prompt.contains("- `expenses.py`"));
        // Every module prompt must start with SOME prime (its family's head).
        for task in [expenses, cli] {
            assert!(
                plan.cache_primes
                    .iter()
                    .any(|p| task.prompt.starts_with(p.as_str())),
                "no prime prefixes task {}",
                task.title
            );
        }
        let full = build_plan("obj", &scoped, 16).unwrap();
        assert!(full.tasks[0].prompt.contains("- `expenses.py`"));
        assert!(full.tasks[0].prompt.contains("- `cli.py`"));
    }

    #[test]
    fn wired_consumers_list_the_calls_they_must_make() {
        let mut wired = manifest();
        wired.files[0].uses = vec!["b.py:helper".to_string()];
        let plan = build_plan("obj", &wired, 16).unwrap();
        assert!(plan.tasks[0].prompt.ends_with(
            "
- Calls (wire each of these into this file's behavior; import them from their files): b.py:helper"
        ));
        let plain = build_plan("obj", &manifest(), 16).unwrap();
        assert!(plain
            .tasks
            .iter()
            .all(|task| !task.prompt.contains("- Calls (")));
        let json = serde_json::to_string(&manifest()).unwrap();
        assert!(
            !json.contains("uses"),
            "unwired manifests serialize as before"
        );
    }

    #[test]
    fn test_worker_sees_every_signature_but_no_purposes() {
        let plan = build_plan("obj", &manifest(), 16).unwrap();
        let tests = plan
            .tasks
            .iter()
            .find(|t| t.id == "tests-test_all-py")
            .expect("test task");
        assert!(tests.prompt.contains("def add(date, cat, amount) -> dict"));
        assert!(tests.prompt.contains("def main(argv=None) -> int"));
        assert!(!tests.prompt.contains("front end"));
    }

    #[test]
    fn manifest_respects_the_task_budget() {
        let too_big = Manifest {
            files: (0..20)
                .map(|i| ManifestFile {
                    path: format!("m{i}.py"),
                    purpose: "x".into(),
                    exports: vec![],
                    uses: Vec::new(),
                })
                .collect(),
            test_files: vec![],
            readme: true,
        };
        assert!(build_plan("obj", &too_big, 8).is_err());
        // Slightly over (20 files + README vs 16 -> ceiling 20) still plans.
        assert!(build_plan("obj", &too_big, 16).is_ok());
    }

    #[test]
    fn unsafe_and_duplicate_paths_are_rejected() {
        let mut sneaky = manifest();
        sneaky.files[0].path = "../escape.py".into();
        assert!(build_plan("obj", &sneaky, 16).is_err());

        let mut dupe = manifest();
        dupe.files.push(dupe.files[0].clone());
        assert!(build_plan("obj", &dupe, 16).is_err());
    }

    /// Scripted client covering the architect retry matrix without a server.
    use std::time::Duration;

    use crate::client::Completion;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct ScriptedClient {
        responses: std::sync::Mutex<Vec<Result<crate::client::Completion>>>,
        calls: std::sync::atomic::AtomicUsize,
    }

    impl ScriptedClient {
        fn new(responses: Vec<Result<crate::client::Completion>>) -> Self {
            Self {
                responses: std::sync::Mutex::new(responses),
                calls: std::sync::atomic::AtomicUsize::new(0),
            }
        }

        fn call_count(&self) -> usize {
            self.calls.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    fn completion(text: &str) -> crate::client::Completion {
        crate::client::Completion {
            text: text.to_string(),
            provider: None,
            input_tokens: 10,
            output_tokens: 10,
            reasoning_tokens: None,
            cached_input_tokens: None,
            finish_reason: Some("stop".to_string()),
            usage_is_estimated: true,
            elapsed: Duration::ZERO,
            time_to_first_token: None,
        }
    }

    fn valid_manifest_json() -> String {
        r#"{"files":[{"path":"a.py","purpose":"core","exports":["def f() -> int"]}],"test_files":[],"readme":false}"#.to_string()
    }

    #[async_trait::async_trait]
    impl ModelClient for ScriptedClient {
        async fn complete(&self, _request: CompletionRequest) -> Result<Completion> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.responses.lock().expect("scripted responses").remove(0)
        }
    }

    #[tokio::test]
    async fn hedged_architect_keeps_the_first_parseable_manifest() {
        // Two racing copies: one returns junk, the other a valid manifest.
        // The hedge must take the valid one without entering the 3s-backoff
        // regeneration path (exactly two calls total).
        let client = ScriptedClient::new(vec![
            Ok(completion("not json at all")),
            Ok(completion(&valid_manifest_json())),
        ]);
        let manifest = generate_manifest(
            &client,
            "obj",
            8,
            &[],
            &ArchitectOptions {
                force_split: false,
                hedge: 2,
                ..ArchitectOptions::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(client.call_count(), 2);
        assert_eq!(manifest.files.len(), 1);
    }

    #[tokio::test]
    async fn empty_completion_on_attempt_one_triggers_a_retry() {
        // Muse degraded windows return empty completions; the architect must
        // retry rather than fail the whole run.
        let client = ScriptedClient::new(vec![
            Err(anyhow::anyhow!("model returned an empty completion")),
            Ok(completion(&valid_manifest_json())),
        ]);
        let plan = generate_manifest_plan(&client, "obj", 8, &[])
            .await
            .unwrap();
        assert_eq!(client.call_count(), 2);
        assert_eq!(plan.tasks.len(), 1);
    }

    #[test]
    fn bare_path_strings_parse_as_manifest_files() {
        let text = r#"{"files":[{"path":"a.py","purpose":"core"}],
            "test_files":["tests/test_a.py"],"readme":false}"#;
        let manifest = manifest_from_json(&parse_manifest(text).unwrap()).unwrap();
        assert_eq!(manifest.test_files[0].path, "tests/test_a.py");
        assert!(manifest.test_files[0].purpose.is_empty());
        assert_eq!(manifest.files[0].purpose, "core");
    }

    #[tokio::test]
    async fn unparseable_json_on_attempt_one_triggers_a_retry() {
        let client = ScriptedClient::new(vec![
            Ok(completion("no json here")),
            Ok(completion(&valid_manifest_json())),
        ]);
        let plan = generate_manifest_plan(&client, "obj", 8, &[])
            .await
            .unwrap();
        assert_eq!(client.call_count(), 2);
        assert_eq!(plan.tasks.len(), 1);
    }

    /// mock-A/B client: fragment-shaped and salvage-shaped calls get
    /// scripted results (distinguished by the salvage system prompt); input
    /// tokens are estimated from request size so the two fallback paths can
    /// be compared.
    struct GateClient {
        fragment: &'static str,
        salvage: &'static str,
        calls: AtomicUsize,
        fragment_input: std::sync::Mutex<u64>,
        salvage_input: std::sync::Mutex<u64>,
    }

    impl GateClient {
        fn new(fragment: &'static str, salvage: &'static str) -> Self {
            Self {
                fragment,
                salvage,
                calls: AtomicUsize::new(0),
                fragment_input: std::sync::Mutex::new(0),
                salvage_input: std::sync::Mutex::new(0),
            }
        }

        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }

        fn fragment_input_total(&self) -> u64 {
            *self.fragment_input.lock().expect("fragment input")
        }

        fn salvage_input_total(&self) -> u64 {
            *self.salvage_input.lock().expect("salvage input")
        }
    }

    #[async_trait::async_trait]
    impl ModelClient for GateClient {
        async fn complete(&self, request: CompletionRequest) -> Result<Completion> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let input = ((request.system.chars().count()
                + request.user.chars().count()
                + request
                    .shared_context
                    .as_deref()
                    .map_or(0, |context| context.chars().count()))
                / 4) as u64;
            let (text, bucket) = if request.system.contains("repair malformed JSON") {
                (self.salvage, &self.salvage_input)
            } else {
                (self.fragment, &self.fragment_input)
            };
            *bucket.lock().expect("input bucket") += input;
            Ok(completion(text))
        }
    }

    #[tokio::test]
    async fn salvage_repairs_unparseable_fragment_without_full_regeneration() {
        // Mock A/B (zero tokens): fragment output is unparseable prose; the
        // salvage call repairs it. Two calls total, and the input-token sum
        // must be well under the legacy three-full-fragment ladder.
        let client = GateClient::new(
            "no json here",
            r#"{"files":[{"path":"a.py","purpose":"core","exports":["def f() -> int"]}],"test_files":[],"readme":false}"#,
        );
        let json = complete_fragment(
            &client,
            "exports-0",
            "user text",
            "shared context",
            &ArchitectOptions::default(),
            |manifest| !manifest.files.is_empty(),
        )
        .await
        .unwrap();
        assert!(json.contains("a.py"));
        assert_eq!(client.calls(), 2);
        let salvage_path = client.fragment_input_total() + client.salvage_input_total();
        let legacy_path = client.fragment_input_total() * 3;
        assert!(
            salvage_path < legacy_path,
            "salvage path ({salvage_path}) must beat the legacy ladder ({legacy_path})"
        );
    }

    #[tokio::test]
    async fn failed_salvage_falls_through_to_the_legacy_ladder() {
        // Salvage unavailable (also junk): the ladder still runs to its
        // three attempts — one salvage try per unparseable attempt, no more.
        let client = GateClient::new("no json here", "still not json");
        let result = complete_fragment(
            &client,
            "exports-0",
            "user text",
            "shared context",
            &ArchitectOptions::default(),
            |manifest| !manifest.files.is_empty(),
        )
        .await;
        assert!(result.is_err());
        assert_eq!(client.calls(), 6, "3 fragments + 3 salvage tries");
    }

    #[tokio::test]
    async fn valid_manifest_on_attempt_one_makes_no_retry_call() {
        let client = ScriptedClient::new(vec![Ok(completion(&valid_manifest_json()))]);
        let plan = generate_manifest_plan(&client, "obj", 8, &[])
            .await
            .unwrap();
        assert_eq!(client.call_count(), 1);
        assert_eq!(plan.tasks.len(), 1);
    }

    #[tokio::test]
    async fn all_attempts_failing_fails_the_run() {
        let client = ScriptedClient::new(vec![
            Err(anyhow::anyhow!("model returned an empty completion")),
            Err(anyhow::anyhow!("model returned an empty completion")),
            Err(anyhow::anyhow!("model returned an empty completion")),
            Err(anyhow::anyhow!("model returned an empty completion")),
        ]);
        assert!(generate_manifest_plan(&client, "obj", 8, &[])
            .await
            .is_err());
        assert_eq!(client.call_count(), 4);
    }

    #[tokio::test]
    async fn truncated_manifest_falls_back_to_split_fragments() {
        // Measured failure mode on the huge objective: the model stops
        // mid-JSON around 3.5k chars (budget untouched), so regenerating the
        // same single shot reproduces the cut. The architect must fragment.
        let truncated = r#"{"files":[{"path":"a.py","purpose":"core","exports":["def f() -> int"]},{"path":"b.py","purpose":"more","exports":["def g() -> int"]"#;
        let skeleton = r#"{"files":[{"path":"a.py","purpose":"core"},{"path":"b.py","purpose":"more"}],"test_files":[],"readme":false}"#;
        let exports_a = r#"{"files":[{"path":"a.py","purpose":"core","exports":["def f() -> int"]},{"path":"b.py","purpose":"more","exports":["def g() -> str"]}],"test_files":[],"readme":false}"#;
        let client = ScriptedClient::new(vec![
            Ok(completion(truncated)),
            Ok(completion(truncated)),
            Ok(completion(truncated)),
            Ok(completion(truncated)),
            Ok(completion(skeleton)),
            Ok(completion(exports_a)),
        ]);
        let plan = generate_manifest_plan(&client, "obj with tests and a readme", 8, &[])
            .await
            .unwrap();
        // 1 single-shot + 3 regenerations + skeleton + exports. The test
        // layout is deterministic (no tests fragment).
        assert_eq!(client.call_count(), 6);
        assert_eq!(plan.tasks.len(), 4); // 2 modules + 1 test file + 1 README
        let a_task = plan.tasks.iter().find(|t| t.id == "a-py").unwrap();
        assert!(a_task.prompt.contains("def f() -> int"));
        let b_task = plan.tasks.iter().find(|t| t.id == "b-py").unwrap();
        assert!(b_task.prompt.contains("def g() -> str"));
    }

    #[tokio::test]
    async fn split_exports_fragment_with_wrong_paths_fails() {
        let truncated = r#"{"files":[{"path":"a.py","purpose":"core","exports":["def f() -> int"]},{"path":"b.py","purpose":"more","exports":["def g() -> int"]"#;
        let skeleton = r#"{"files":[{"path":"a.py","purpose":"core"},{"path":"b.py","purpose":"more"}],"test_files":[],"readme":false}"#;
        let wrong = r#"{"files":[{"path":"c.py","purpose":"sneaky","exports":["def h() -> int"]}],"test_files":[],"readme":false}"#;
        // Wrong coverage burns one fragment attempt each; three strikes bail.
        let client = ScriptedClient::new(vec![
            Ok(completion(truncated)),
            Ok(completion(truncated)),
            Ok(completion(truncated)),
            Ok(completion(truncated)),
            Ok(completion(skeleton)),
            Ok(completion(wrong)),
            Ok(completion(wrong)),
            Ok(completion(wrong)),
        ]);
        assert!(generate_manifest_plan(&client, "obj", 8, &[])
            .await
            .is_err());
        assert_eq!(client.call_count(), 8);
    }

    #[tokio::test]
    async fn split_exports_fragment_recovers_from_partial_coverage() {
        // Measured: a pair fragment came back covering only 1 of 2 files.
        // That burns one attempt; the next shot must be accepted.
        let truncated = r#"{"files":[{"path":"a.py","purpose":"core","exports":["def f() -> int"]},{"path":"b.py","purpose":"more","exports":["def g() -> int"]"#;
        let skeleton = r#"{"files":[{"path":"a.py","purpose":"core"},{"path":"b.py","purpose":"more"}],"test_files":[],"readme":false}"#;
        let partial = r#"{"files":[{"path":"a.py","purpose":"core","exports":["def f() -> int"]}],"test_files":[],"readme":false}"#;
        let full = r#"{"files":[{"path":"a.py","purpose":"core","exports":["def f() -> int"]},{"path":"b.py","purpose":"more","exports":["def g() -> str"]}],"test_files":[],"readme":false}"#;
        let client = ScriptedClient::new(vec![
            Ok(completion(truncated)),
            Ok(completion(truncated)),
            Ok(completion(truncated)),
            Ok(completion(truncated)),
            Ok(completion(skeleton)),
            Ok(completion(partial)),
            Ok(completion(full)),
        ]);
        let plan = generate_manifest_plan(&client, "obj with tests and a readme", 8, &[])
            .await
            .unwrap();
        // 1 single-shot + 3 regenerations + skeleton + exports x2.
        assert_eq!(client.call_count(), 7);
        assert_eq!(plan.tasks.len(), 4);
    }

    #[tokio::test]
    async fn split_export_fragments_run_concurrently() {
        // Four files = two export fragments. They are independent, so both
        // must be in flight before either returns: a client that blocks
        // each call until a second call arrives deadlocks a sequential loop.
        struct Barrier {
            both: tokio::sync::Barrier,
        }
        #[async_trait::async_trait]
        impl ModelClient for Barrier {
            async fn complete(&self, request: CompletionRequest) -> Result<Completion> {
                if request
                    .user
                    .contains("paths and one-sentence purposes ONLY")
                {
                    return Ok(completion(
                        r#"{"files":[{"path":"a.py","purpose":"a"},{"path":"b.py","purpose":"b"},{"path":"c.py","purpose":"c"},{"path":"d.py","purpose":"d"}],"test_files":[],"readme":false}"#,
                    ));
                }
                // Released only once BOTH fragment calls are in flight.
                self.both.wait().await;
                let pair = if request.user.contains("`a.py`") {
                    ("a.py", "b.py")
                } else {
                    ("c.py", "d.py")
                };
                Ok(completion(&format!(
                    r#"{{"files":[{{"path":"{}","purpose":"x","exports":["def f() -> int"]}},{{"path":"{}","purpose":"y","exports":["def g() -> int"]}}],"test_files":[],"readme":false}}"#,
                    pair.0, pair.1
                )))
            }
        }
        let client = Barrier {
            both: tokio::sync::Barrier::new(2),
        };
        let manifest = tokio::time::timeout(
            Duration::from_secs(5),
            generate_manifest(
                &client,
                "obj with tests",
                8,
                &[],
                &ArchitectOptions::default(),
            ),
        )
        .await
        .expect("fragments must not serialize")
        .unwrap();
        let paths: Vec<_> = manifest.files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(
            paths,
            ["a.py", "b.py", "c.py", "d.py"],
            "skeleton order kept"
        );
        assert_eq!(manifest.test_files.len(), 1, "deterministic test layout");
        assert!(!manifest.readme, "objective did not ask for a README");
    }

    #[tokio::test]
    async fn schema_option_constrains_every_fragment_request() {
        struct Recorder {
            seen: std::sync::Mutex<Vec<(Option<serde_json::Value>, Option<String>)>>,
        }
        #[async_trait::async_trait]
        impl ModelClient for Recorder {
            async fn complete(&self, request: CompletionRequest) -> Result<Completion> {
                self.seen.lock().unwrap().push((
                    request.response_format.clone(),
                    request.reasoning_effort.clone(),
                ));
                Ok(completion(
                    r#"{"files":[{"path":"a.py","purpose":"a","exports":["def f() -> int"]}],"test_files":[],"readme":false}"#,
                ))
            }
        }
        let client = Recorder {
            seen: Default::default(),
        };
        let options = ArchitectOptions {
            schema: true,
            fragment_effort: "minimal".to_string(),
            ..ArchitectOptions::default()
        };
        generate_manifest(&client, "obj", 8, &[], &options)
            .await
            .unwrap();
        let seen = client.seen.lock().unwrap();
        assert!(seen.len() >= 2, "skeleton plus an export fragment");
        assert!(seen[0].0.is_none(), "skeleton keeps a free layout");
        for (format, effort) in seen.iter().skip(1) {
            let format = format.as_ref().expect("schema attached");
            assert_eq!(format["json_schema"]["strict"], true);
            assert_eq!(effort.as_deref(), Some("minimal"));
        }
    }

    #[tokio::test]
    async fn schema_rejection_falls_back_to_free_json() {
        struct NoSchema {
            rejected: AtomicUsize,
        }
        #[async_trait::async_trait]
        impl ModelClient for NoSchema {
            async fn complete(&self, request: CompletionRequest) -> Result<Completion> {
                if request.response_format.is_some() {
                    self.rejected.fetch_add(1, Ordering::SeqCst);
                    bail!(
                        "model server returned HTTP 400: This response_format type is unavailable now"
                    );
                }
                Ok(completion(
                    r#"{"files":[{"path":"a.py","purpose":"a","exports":["def f() -> int"]}],"test_files":[],"readme":false}"#,
                ))
            }
        }
        let client = NoSchema {
            rejected: AtomicUsize::new(0),
        };
        let manifest = tokio::time::timeout(
            Duration::from_secs(5),
            generate_manifest(&client, "obj", 8, &[], &ArchitectOptions::default()),
        )
        .await
        .expect("fallback must not wait out the ladder backoff")
        .unwrap();
        assert_eq!(manifest.files[0].exports, ["def f() -> int"]);
        assert_eq!(client.rejected.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn router_routes_small_objectives_to_a_direct_response() {
        let finance = "Build a Python personal-finance command-line suite using only the standard library. Modules: (1) expenses.py - tracker; (2) budget.py - limits; (3) csvio.py - CSV; (4) report.py - HTML; (5) cli.py - argparse. Include a pytest test suite and a README.";
        assert!(fits_single_response(finance));
        let plan = direct_response_plan(finance);
        assert_eq!(plan.tasks.len(), 1);
        assert_eq!(plan.tasks[0].prompt, finance);
        assert!(!plan.tasks[0].require_closed_fences);
        assert_eq!(plan.tasks[0].max_tokens, 32_768);
    }

    #[test]
    fn router_sends_wide_objectives_to_the_wave() {
        let wide = "Build a finance suite: modules a.py b.py c.py d.py e.py f.py g.py h.py with at least 120 tests covering every function and edge case, plus docs.";
        assert!(!fits_single_response(wide));
    }

    #[test]
    fn demanded_test_count_parses_common_phrasings() {
        assert_eq!(demanded_test_count("at least 40 tests"), 40);
        assert_eq!(demanded_test_count("40+ tests"), 40);
        assert_eq!(demanded_test_count("a suite of 25 tests"), 25);
        // No explicit count: assume a modest default suite.
        assert_eq!(demanded_test_count("include a pytest test suite"), 15);
    }

    #[test]
    fn estimator_counts_files_and_scales_with_tests() {
        let base = estimate_single_response_tokens("include a pytest test suite and README");
        let wide = estimate_single_response_tokens(
            "at least 120 tests across a.py b.py c.py d.py e.py f.py g.py h.py",
        );
        assert!(wide > base + 30_000, "wide objective must far exceed base");
    }
}
