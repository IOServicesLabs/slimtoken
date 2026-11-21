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
use crate::profile::{glob_match, Layer, Profile, Staging};
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

    /// One line of the module head for `file`: its path and signatures,
    /// with `purposes` also its purpose as the planner wrote it
    /// (`planner_purpose`).
    fn module_line(&self, file: &ManifestFile, purposes: Option<&Staging>) -> String {
        match purposes.map(|staging| planner_purpose(&file.purpose, staging)) {
            Some(purpose) if !purpose.is_empty() => format!(
                "- `{}` ({}): {}",
                file.path,
                purpose.trim_end_matches('.'),
                self.exports_inline(file)
            ),
            _ => format!("- `{}`: {}", file.path, self.exports_inline(file)),
        }
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
fn manifest_rules(
    max_tasks: usize,
    profile: &Profile,
    type_ownership: bool,
    contract: bool,
    page_local: bool,
    semantics: bool,
) -> String {
    let language = profile.planner_rules;
    let ownership = if type_ownership {
        profile.type_ownership_rule
    } else {
        ""
    };
    // `ArchitectOptions::contract`: the profile's app contract rules.
    let contract = if contract {
        profile.staging.quality.contract_rule
    } else {
        ""
    };
    // `ArchitectOptions::page_local`: pages own their features (with
    // `semantics`, the variant with the store provider as the one owner).
    let quality = &profile.staging.quality;
    let page_local = match (page_local, semantics) {
        (true, true) if !quality.semantic_page_local_rule.is_empty() => {
            quality.semantic_page_local_rule
        }
        (true, _) => quality.page_local_rule,
        (false, _) => "",
    };
    // `ArchitectOptions::semantics`: the conventions every file follows.
    let semantics = if semantics {
        quality.semantics_rule
    } else {
        ""
    };
    format!(
        r#"Rules:
{language}{ownership}{contract}{page_local}{semantics}
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
            prefix_split: None,
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
fn architect_shared_context(
    max_tasks: usize,
    profile: &Profile,
    type_ownership: bool,
    contract: bool,
    page_local: bool,
    semantics: bool,
) -> String {
    format!(
        "You are the architect for a greenfield software build. You decide \
         the file layout and the exact interface contracts. Return JSON only.\n\n{}",
        manifest_rules(
            max_tasks,
            profile,
            type_ownership,
            contract,
            page_local,
            semantics
        )
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
/// Missing fields deserialize to their defaults.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
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
    /// Order the wave in stages from the profile's `staging` layers: types
    /// first, then Data and Core (they see the types), then Consumers and
    /// tests (they see the types and Core), with the profile's prompt rules.
    /// A plan with staged tasks has no cache primes: its first stage warms
    /// the prefix cache. No effect for a profile without staging layers.
    pub staged: bool,
    /// Lowest number of test shards (0: no floor). Applied before the
    /// `MAX_TEST_SHARDS` clamp and never more shards than testable modules.
    /// The shards it adds count toward the task budget as test files would.
    pub min_test_shards: usize,
    /// Each shared type is declared in one file only: `dedupe_type_owners`
    /// runs on the manifest before the plan is built. No effect for a
    /// profile without `staging.type_decl_prefixes`.
    pub one_type_owner: bool,
    /// Run the profile's measured manifest fixups before the plan is built:
    /// its `tidy_manifest` hook, then the `staging.data_note` on Data-layer
    /// files (see `apply_manifest_passes`). No effect for a profile without
    /// them (PYTHON, RUST).
    pub tidy: bool,
    /// Today's date `(year, month, day)`: `stamp_today` asks the Data-layer
    /// files for records around this day. `None`: no date (plans stay
    /// byte-identical from one day to the next).
    pub today: Option<(i32, u32, u32)>,
    /// Write the profile's long files in parts (`staging.split`, see
    /// `split_files`), and render each file that only joins other files
    /// (`staging.aggregate`) instead of asking the model for it. No effect
    /// for a profile without them (PYTHON, RUST).
    pub split: bool,
    /// Staged consumers depend on the Core files their `uses` name, not on
    /// all of Core. Reserved: no effect yet.
    pub uses_deps: bool,
    /// With `tidy`: each type that a signature names and no file of the plan
    /// declares gets a declaration in an owner file (`declare_signature_types`).
    /// No effect without `tidy`, or for a profile whose
    /// `staging.type_decl_prefixes` has no `export interface ` (PYTHON, RUST).
    pub declare_types: bool,
    /// The profile's fixed quality rules (`staging.quality`): its
    /// `head_rule` ends the module head, and its `rewrites` run on every
    /// purpose and export line after the other manifest passes (NEXTJS: the
    /// theme is `data-theme` on `<html>`, one store, random ids, settings
    /// that are read). No effect for a profile without them (PYTHON, RUST).
    pub quality_rules: bool,
    /// Data-layer files (seed records) get the profile's
    /// `staging.quality.date_note` (dates computed when the data loads,
    /// relative to the current date) in place of `today`'s fixed day: it
    /// replaces a `stamp_today` sentence already there, and `today` is then
    /// ignored. No effect for a profile without a date note.
    pub relative_dates: bool,
    /// Each route file (`staging.quality.page_glob`) gets its bullet of the
    /// objective's page list verbatim (matched by route name), the
    /// objective's quality-bar paragraph and the list of the planned routes;
    /// each component (`component_glob`) gets `component_rule`; the module
    /// head lists each file's purpose (without the engine's notes) and ends
    /// with `purposes_rule`. No effect for a profile without a page glob.
    pub objective_pages: bool,
    /// A string literal in a purpose or export line that equals a member of
    /// a string union of the Types-layer files apart from case, spaces and
    /// separators gets the union's spelling (`align_union_literals`). No
    /// effect for a profile without staging layers.
    pub align_literals: bool,
    /// The app contract on the plan side (see `ArchitectOptions::contract`):
    /// each file that the settings comment of the Types-layer file names as
    /// a reader of a field (`field: meaning -> path`) gets a tail line with
    /// those fields and their meanings (`contract::reader_line`). No effect
    /// for a profile without a settings type, or a plan whose settings
    /// comment names no readers.
    pub contract: bool,
    /// The wiring prompt (`ArchitectOptions::wiring` and `wave_wiring`)
    /// also gets the profile's `staging.quality.wiring_rule` (every create,
    /// add, update and remove export has a caller; pages list the components
    /// they render) and each settings field with its readers. With
    /// `wave_wiring`, only staged files (a file of a staging layer) wait for
    /// the wiring task (the CSS parts do not). No effect for a profile
    /// without a wiring rule.
    pub wiring_rules: bool,
    /// With `staged`: pages and components (the profile's `is_consumer`
    /// files of the Consumer layer, not tests) also depend on the parts of
    /// a split file (`staging.split`, the CSS parts), so they see the real
    /// selectors. No effect without `staged` and `split`.
    pub consumers_see_css: bool,
    /// Each model task's prompt marks where its shared head ends
    /// (`Task::prefix_split`): the scheduler then puts the prerequisite
    /// outputs, sorted by task id, between the head and the per-file tail,
    /// so sibling tasks with the same deps share a long identical prefix
    /// (prompt caching). The staged rule says "above" instead of "below".
    pub deps_first: bool,
    /// Pages own their features (`pagelocal`): after the other manifest
    /// passes, each component that exactly one page renders goes into that
    /// page's file (`pagelocal::fold_page_components`: one worker writes the
    /// page and its private components); each component left that no layout
    /// renders gets the profile's `staging.quality.shared_component_rule`
    /// (data through props, changes through callbacks, no state of its own);
    /// with `objective_pages`, each page also gets the shared components it
    /// renders, the links between pages that the routes imply and the
    /// controls for the create and add exports it owns
    /// (`pagelocal::page_lines`). No effect for a profile without a local
    /// note (PYTHON, RUST).
    pub page_local: bool,
    /// The conventions on the plan side (see `ArchitectOptions::semantics`):
    /// the module head ends with `semantics::conventions` (each money
    /// field's unit and sign, each date field's format and each status
    /// union's transition owner as the Types-layer file states them, else
    /// the profile's default; the store provider as the one state owner; the
    /// shell the root layout renders; then the profile's
    /// `staging.quality.conventions_rule`), the same text for every module
    /// worker; each component that a layout renders gets the profile's
    /// `layout_component_rule` (`pagelocal::layout_component_lines`). No
    /// effect for a profile without a semantics rule (PYTHON, RUST).
    pub semantics: bool,
    /// Each page (`staging.quality.page_glob`) of a plan whose root layout
    /// renders a shell component gets the profile's
    /// `staging.quality.page_shell_rule` in its prompt's tail: the layout
    /// already draws the app's frame around the page (the shell and its
    /// page-wrapper class), so the page returns only its own content
    /// (`pagelocal::page_shell_lines`). The bench's TS9021 (a page that
    /// wraps itself in the frame again) was the most common problem after
    /// the wave. No effect for a profile without the rule (PYTHON, RUST).
    pub shell_rule: bool,
    /// With `staged`: Core and Data files do not wait for the Types files.
    /// They start with the wave and take the types from the plan's
    /// declarations ([`TYPES_FROM_PLAN_RULE`] in place of the staged rule);
    /// Consumers still wait for the Types and the Core. For plans whose
    /// types file is planned as code (`ArchitectOptions::types_first`): on
    /// the bench's 23 runs since 2026-11-17 the written types file held 354
    /// of 356 planned declarations word for word, so the 2.5-3.3 s that Core
    /// waited for it bought nothing. No effect without `staged` or without a
    /// Types-layer file.
    pub types_from_plan: bool,
}

impl Default for PlanOptions {
    fn default() -> Self {
        Self {
            tests_see_code: false,
            wave_wiring: false,
            density: false,
            quota_pct: DEFAULT_TEST_QUOTA_PCT,
            scope_contracts: false,
            staged: false,
            min_test_shards: 0,
            one_type_owner: false,
            tidy: false,
            today: None,
            split: false,
            uses_deps: false,
            declare_types: false,
            quality_rules: false,
            relative_dates: false,
            objective_pages: false,
            align_literals: false,
            contract: false,
            wiring_rules: false,
            consumers_see_css: false,
            deps_first: false,
            page_local: false,
            semantics: false,
            shell_rule: false,
            types_from_plan: false,
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
    apply_manifest_passes(&mut manifest, profile, options);
    (profile.normalize_manifest)(&mut manifest);
    build_plan_with(objective, &manifest, max_tasks, profile, options)
}

/// The manifest passes that `plan_from_manifest` runs (on its copy) before
/// the profile's `normalize_manifest`, in this order, each only when its
/// option is set: `dedupe_type_owners` (`one_type_owner`), the profile's
/// `tidy_manifest` hook (`tidy`), `declare_signature_types` (`tidy` with
/// `declare_types`), `align_union_literals` (`align_literals`),
/// `split_files` (`split`), `apply_data_note` (`tidy`), then
/// `stamp_relative_dates` (`relative_dates`) or else `stamp_today`
/// (`today`), then `apply_quality_rewrites` (`quality_rules`), then
/// `pagelocal::fold_page_components` (`page_local`). With the
/// default options nothing changes. Each pass is idempotent, so a manifest
/// saved after the passes plans the same.
pub fn apply_manifest_passes(manifest: &mut Manifest, profile: &Profile, options: &PlanOptions) {
    if options.one_type_owner {
        dedupe_type_owners(manifest, profile);
    }
    if options.tidy {
        (profile.tidy_manifest)(manifest);
        if options.declare_types {
            declare_signature_types(manifest, &profile.staging);
        }
    }
    if options.align_literals {
        align_union_literals(manifest, &profile.staging);
    }
    // The bench's order: its tidy steps, the CSS split, then the seed notes.
    // For the built-in profiles the order of `split_files` does not change
    // the result: it only touches `*globals.css` and the part files it
    // adds, which no other pass reads or changes (they act on type
    // exports, `.tsx`/route paths and Data-layer files). A profile whose
    // split glob overlaps those files gets this order.
    if options.split {
        split_files(manifest, &profile.staging);
    }
    if options.tidy {
        apply_data_note(manifest, &profile.staging);
    }
    if options.relative_dates && !profile.staging.quality.date_note.is_empty() {
        stamp_relative_dates(manifest, profile);
    } else if let Some(date) = options.today {
        stamp_today(manifest, profile, date);
    }
    if options.quality_rules {
        apply_quality_rewrites(manifest, &profile.staging);
    }
    // Last: the note it adds to a page ends that page's purpose, and the
    // folded text has had the other passes.
    if options.page_local {
        crate::pagelocal::fold_page_components(manifest, profile);
    }
}

/// Write each long file of `staging.split` in parts: the first file that
/// matches a split's glob and has at least `min_exports` exports gives its
/// exports, in order, to 2 to `max_parts` new part files placed right after
/// it, and keeps only the job of joining them (no exports, the split's
/// `joined_purpose`); `staging.aggregate` renders it in the plan. A long
/// file is the slowest single task of a wave; its parts are written at the
/// same time with the same prompt prefix. Nothing changes when a part's
/// path is already in the manifest. Idempotent: the joined file has no
/// exports left to split.
pub fn split_files(manifest: &mut Manifest, staging: &Staging) {
    for split in staging.split {
        let Some(index) = manifest.files.iter().position(|file| {
            glob_match(split.glob, &file.path) && file.exports.len() >= split.min_exports
        }) else {
            continue;
        };
        let whole = manifest.files[index].clone();
        let dir = whole.path.rsplit_once('/').map_or("", |(dir, _)| dir);
        let per_part = split.part_exports.max(1);
        let count = whole
            .exports
            .len()
            .div_ceil(per_part)
            .clamp(2, split.max_parts.max(2));
        let size = whole.exports.len().div_ceil(count);
        let parts: Vec<ManifestFile> = whole
            .exports
            .chunks(size)
            .enumerate()
            .map(|(number, exports)| {
                let n = (number + 1).to_string();
                let part = split.part_pattern.replace("{n}", &n);
                let template = if number == 0 {
                    split.first_purpose
                } else {
                    split.part_purpose
                };
                ManifestFile {
                    path: if dir.is_empty() {
                        part
                    } else {
                        format!("{dir}/{part}")
                    },
                    purpose: template
                        .replace("{count}", &count.to_string())
                        .replace("{n}", &n),
                    exports: exports.to_vec(),
                    ..whole.clone()
                }
            })
            .collect();
        let taken = parts.iter().any(|part| {
            manifest
                .files
                .iter()
                .chain(manifest.test_files.iter())
                .any(|file| file.path == part.path)
        });
        if taken {
            continue;
        }
        let joined = &mut manifest.files[index];
        joined.exports.clear();
        joined.purpose = split.joined_purpose.to_string();
        manifest.files.splice(index + 1..index + 1, parts);
    }
}

/// Give each shared type one owner file, so workers that each declare their
/// own copy of a type do not write files that fail to compile together.
/// First `types_files_own_types`, then `first_type_owner_wins`. Does nothing
/// when the profile's `staging.type_decl_prefixes` is empty.
pub fn dedupe_type_owners(manifest: &mut Manifest, profile: &Profile) {
    types_files_own_types(manifest, &profile.staging);
    first_type_owner_wins(manifest, &profile.staging);
}

/// The name of the type that a planned export line declares
/// (`export interface Task`, `export type Status = ...`), if it starts with
/// one of `prefixes` (after leading spaces).
fn declared_type<'a>(line: &'a str, prefixes: &[&str]) -> Option<&'a str> {
    let line = line.trim_start();
    let rest = prefixes
        .iter()
        .find_map(|prefix| line.strip_prefix(prefix))?;
    let end = rest
        .find(|ch: char| !(ch.is_alphanumeric() || ch == '_'))
        .unwrap_or(rest.len());
    (end > 0).then(|| &rest[..end])
}

/// A type that a Types-layer file declares is declared there only: every
/// other file loses its export line for that type, and its purpose names
/// the first Types-layer file to import it from. Does nothing without
/// `type_decl_prefixes` or without a Types-layer file.
pub fn types_files_own_types(manifest: &mut Manifest, staging: &Staging) {
    let prefixes = staging.type_decl_prefixes;
    if prefixes.is_empty() {
        return;
    }
    let is_types = |path: &str| staging.layer_of(path) == Some(Layer::Types);
    let Some(owner) = manifest
        .files
        .iter()
        .find(|file| is_types(&file.path))
        .map(|file| file.path.clone())
    else {
        return;
    };
    let owned: Vec<String> = manifest
        .files
        .iter()
        .filter(|file| is_types(&file.path))
        .flat_map(|file| file.exports.iter())
        .filter_map(|line| declared_type(line, prefixes))
        .map(str::to_string)
        .collect();
    for file in &mut manifest.files {
        if is_types(&file.path) {
            continue;
        }
        let mut moved = Vec::new();
        file.exports
            .retain(|line| match declared_type(line, prefixes) {
                Some(name) if owned.iter().any(|own| own == name) => {
                    moved.push(name.to_string());
                    false
                }
                _ => true,
            });
        if !moved.is_empty() {
            file.purpose.push_str(&format!(
                " Imports {} from `{owner}`: this file does not declare or export a type with \
                 that name, and it uses the fields and literal values of `{owner}`.",
                moved.join(", ")
            ));
        }
    }
}

/// A type that two files matching `staging.type_owner_globs` declare (two
/// `AppState` in `lib/storage.ts` and `lib/store.ts`; Types-layer files
/// excepted): the first file in the manifest keeps it, and the others lose
/// the export line and import it from that file. Does nothing without
/// `type_decl_prefixes` or `type_owner_globs`.
pub fn first_type_owner_wins(manifest: &mut Manifest, staging: &Staging) {
    let prefixes = staging.type_decl_prefixes;
    if prefixes.is_empty() || staging.type_owner_globs.is_empty() {
        return;
    }
    let mut owners: Vec<(String, String)> = Vec::new();
    for file in &mut manifest.files {
        let in_scope = staging
            .type_owner_globs
            .iter()
            .any(|glob| crate::profile::glob_match(glob, &file.path));
        if !in_scope || staging.layer_of(&file.path) == Some(Layer::Types) {
            continue;
        }
        let path = file.path.clone();
        let mut moved: Vec<(String, String)> = Vec::new();
        file.exports.retain(|line| {
            let Some(name) = declared_type(line, prefixes) else {
                return true;
            };
            match owners.iter().find(|(own, _)| own == name) {
                Some((_, owner)) => {
                    moved.push((name.to_string(), owner.clone()));
                    false
                }
                None => {
                    owners.push((name.to_string(), path.clone()));
                    true
                }
            }
        });
        for (name, owner) in moved {
            file.purpose.push_str(&format!(
                " Imports {name} from `{owner}`: this file does not declare a type with that name."
            ));
        }
    }
}

/// Type names that a TypeScript signature can use with no declaration in the
/// plan: the language's, the DOM's, React's and Next.js's.
pub(crate) const KNOWN_TYPES: &[&str] = &[
    "Array",
    "ArrayLike",
    "Awaited",
    "BigInt",
    "Blob",
    "Boolean",
    "Capitalize",
    "ChangeEvent",
    "ComponentProps",
    "CSSProperties",
    "Date",
    "Dispatch",
    "Document",
    "Element",
    "Error",
    "Event",
    "Exclude",
    "Extract",
    "FC",
    "File",
    "FormData",
    "FormEvent",
    "Function",
    "Generator",
    "Headers",
    "HTMLButtonElement",
    "HTMLDivElement",
    "HTMLElement",
    "HTMLFormElement",
    "HTMLInputElement",
    "HTMLSelectElement",
    "HTMLTextAreaElement",
    "Infinity",
    "Intl",
    "Iterable",
    "IterableIterator",
    "JSON",
    "JSX",
    "KeyboardEvent",
    "Lowercase",
    "Map",
    "Math",
    "Metadata",
    "MouseEvent",
    "MutableRefObject",
    "NaN",
    "NextConfig",
    "NextRequest",
    "NextResponse",
    "Node",
    "NonNullable",
    "Number",
    "Object",
    "Omit",
    "Parameters",
    "Partial",
    "Pick",
    "Promise",
    "PromiseLike",
    "PropsWithChildren",
    "RangeError",
    "React",
    "ReactElement",
    "ReactNode",
    "Readonly",
    "ReadonlyArray",
    "Record",
    "Ref",
    "RefObject",
    "Request",
    "Required",
    "Response",
    "ReturnType",
    "Set",
    "SetStateAction",
    "Storage",
    "String",
    "Symbol",
    "SyntaxError",
    "TypeError",
    "Uint8Array",
    "Uppercase",
    "URL",
    "URLSearchParams",
    "Viewport",
    "WeakMap",
    "WeakSet",
    "Window",
];

/// Words after which the next name is declared, not used.
const DECLARING_WORDS: &[&str] = &[
    "interface",
    "type",
    "enum",
    "class",
    "function",
    "const",
    "let",
    "var",
];

/// The signature of a planned export line (the text before its `//`
/// comment) with the text of each string literal removed, so that literal
/// values (`'Done'`, `"In Progress"`) never read as type names.
pub(crate) fn bare_signature(line: &str) -> String {
    let head = line.split("//").next().unwrap_or(line);
    let mut out = String::with_capacity(head.len());
    let mut quote: Option<char> = None;
    for ch in head.chars() {
        match quote {
            Some(open) if ch == open => {
                quote = None;
                out.push(ch);
            }
            Some(_) => {}
            None => {
                if matches!(ch, '\'' | '"' | '`') {
                    quote = Some(ch);
                }
                out.push(ch);
            }
        }
    }
    out
}

/// The identifiers of `text` with their byte offsets.
pub(crate) fn identifiers(text: &str) -> Vec<(usize, &str)> {
    let mut found = Vec::new();
    let mut start: Option<usize> = None;
    for (index, ch) in text
        .char_indices()
        .chain(std::iter::once((text.len(), ' ')))
    {
        let part = ch.is_ascii_alphanumeric() || ch == '_' || ch == '$';
        match (start, part) {
            (None, true) => start = Some(index),
            (Some(from), false) => {
                let word = &text[from..index];
                if !word.starts_with(|c: char| c.is_ascii_digit()) {
                    found.push((from, word));
                }
                start = None;
            }
            _ => {}
        }
    }
    found
}

/// The names that a signature declares: the word after `interface`, `type`,
/// `enum`, `class`, `function`, `const`, `let` or `var`.
pub(crate) fn declared_names(signature: &str) -> Vec<String> {
    identifiers(signature)
        .windows(2)
        .filter(|pair| DECLARING_WORDS.contains(&pair[0].1))
        .map(|pair| pair[1].1.to_string())
        .collect()
}

/// The type names that a signature uses: capitalised identifiers of two or
/// more characters that are not a declared name, a call, a namespace or its
/// member (`React.ReactNode`), an object key, a `typeof` operand, a
/// component's `...Props` or a name of [`KNOWN_TYPES`].
pub(crate) fn used_type_names(signature: &str) -> Vec<String> {
    let words = identifiers(signature);
    let mut names: Vec<String> = Vec::new();
    for (index, &(at, word)) in words.iter().enumerate() {
        if word.len() < 2
            || !word.starts_with(|c: char| c.is_ascii_uppercase())
            || word.ends_with("Props")
            || KNOWN_TYPES.contains(&word)
        {
            continue;
        }
        let previous = index.checked_sub(1).map(|before| words[before].1);
        if previous.is_some_and(|word| DECLARING_WORDS.contains(&word) || word == "typeof") {
            continue;
        }
        let before = signature[..at].trim_end().chars().next_back();
        let after = signature[at + word.len()..].trim_start().chars().next();
        if before == Some('.') || matches!(after, Some('.' | '(' | ':')) {
            continue;
        }
        if !names.iter().any(|name| name == word) {
            names.push(word.to_string());
        }
    }
    names
}

/// The fields of a `{a: T, b: U}` shape that starts `comment`, as
/// `a: T; b: U`, when every field has a key and a value that reads as a
/// type. `None` for a comment that does not start with a shape, or a shape
/// of values (`{count: 3, theme: 'light'}`, `{owner: first member}`).
pub(crate) fn shape_fields(comment: &str) -> Option<String> {
    let text = comment.trim_start();
    if !text.starts_with('{') {
        return None;
    }
    let mut depth = 0usize;
    let mut end = None;
    for (index, ch) in text.char_indices() {
        match ch {
            '{' | '[' | '(' | '<' => depth += 1,
            '}' | ']' | ')' | '>' => {
                if ch == '>' && text[..index].ends_with('=') {
                    continue;
                }
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    end = Some(index);
                    break;
                }
            }
            _ => {}
        }
    }
    let inner = &text[1..end?];
    // Split at the commas and semicolons outside brackets.
    let mut fields: Vec<String> = Vec::new();
    let (mut depth, mut from) = (0usize, 0usize);
    for (index, ch) in inner
        .char_indices()
        .chain(std::iter::once((inner.len(), ',')))
    {
        match ch {
            '{' | '[' | '(' | '<' => depth += 1,
            '}' | ']' | ')' => depth = depth.saturating_sub(1),
            '>' if !inner[..index].ends_with('=') => depth = depth.saturating_sub(1),
            ',' | ';' if depth == 0 => {
                let field = inner[from..index].trim();
                from = index + 1;
                if field.is_empty() {
                    continue;
                }
                let (key, value) = field.split_once(':')?;
                let (key, value) = (key.trim(), value.trim());
                let bare = key.strip_suffix('?').unwrap_or(key);
                let key_ok = !bare.is_empty()
                    && !bare.starts_with(|c: char| c.is_ascii_digit())
                    && bare.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
                if !key_ok || !reads_as_type(value) {
                    return None;
                }
                fields.push(format!("{key}: {value}"));
            }
            _ => {}
        }
    }
    (!fields.is_empty()).then(|| fields.join("; "))
}

/// Whether `value` reads as a TypeScript type: it starts with a letter or a
/// bracket (a lone string literal is a value unless it is part of a
/// union), has no `=` other than an arrow, and no two plain words in a row
/// (`first member`) other than after a type operator. A default value is not
/// a type: an empty `[]` or `{}` (the empty tuple and the empty object type),
/// a `[...]` or `{...}` whose items are values (`{theme: 'light'}`), and a
/// lone `null`, `true`, `false` or lower-case word that is not a primitive
/// type (`light`) unless it is part of a union.
fn reads_as_type(value: &str) -> bool {
    let Some(first) = value.chars().next() else {
        return false;
    };
    let literal = matches!(first, '\'' | '"');
    let starts_well = first.is_ascii_alphabetic() || matches!(first, '(' | '[' | '{');
    if !(starts_well || (literal && value.contains('|'))) {
        return false;
    }
    if value.replace("=>", "").contains('=') {
        return false;
    }
    // A lone word: a union never is one, so `null` in `string | null` stays.
    let lone_word = value.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    if lone_word && first.is_ascii_lowercase() && !PRIMITIVE_TYPES.contains(&value) {
        return false;
    }
    match first {
        '{' if shape_fields(value).is_none() => return false,
        '[' if !tuple_reads_as_type(value) => return false,
        _ => {}
    }
    let bare = bare_signature(value);
    let words = identifiers(&bare);
    words.windows(2).all(|pair| {
        let gap = &bare[pair[0].0 + pair[0].1.len()..pair[1].0];
        !gap.trim().is_empty()
            || matches!(
                pair[0].1,
                "keyof" | "typeof" | "readonly" | "extends" | "infer" | "is" | "unique"
            )
    })
}

/// The lower-case words that are TypeScript types on their own (`null`,
/// `true` and `false` are left out: alone in a shape they are default values).
const PRIMITIVE_TYPES: &[&str] = &[
    "string",
    "number",
    "boolean",
    "bigint",
    "symbol",
    "object",
    "unknown",
    "any",
    "never",
    "void",
    "undefined",
];

/// Whether a value that starts with `[` reads as a tuple type: the leading
/// `[...]` holds at least one item and each item reads as a type
/// (`[string, number]`, not `[]` or `['a', 'b']`).
fn tuple_reads_as_type(value: &str) -> bool {
    let mut depth = 0usize;
    let mut end = None;
    for (index, ch) in value.char_indices() {
        match ch {
            '{' | '[' | '(' | '<' => depth += 1,
            '}' | ']' | ')' | '>' => {
                if ch == '>' && value[..index].ends_with('=') {
                    continue;
                }
                let Some(left) = depth.checked_sub(1) else {
                    return false;
                };
                depth = left;
                if depth == 0 {
                    end = Some(index);
                    break;
                }
            }
            _ => {}
        }
    }
    let Some(end) = end else {
        return false;
    };
    let inner = &value[1..end];
    let mut items: Vec<&str> = Vec::new();
    let (mut depth, mut from) = (0usize, 0usize);
    for (index, ch) in inner
        .char_indices()
        .chain(std::iter::once((inner.len(), ',')))
    {
        match ch {
            '{' | '[' | '(' | '<' => depth += 1,
            '}' | ']' | ')' => depth = depth.saturating_sub(1),
            '>' if !inner[..index].ends_with('=') => depth = depth.saturating_sub(1),
            ',' if depth == 0 => {
                items.push(inner[from..index].trim());
                from = index + 1;
            }
            _ => {}
        }
    }
    let item_type = |item: &str| -> Option<String> {
        let item = item.strip_prefix("...").unwrap_or(item);
        // A named member: `[start: Date, end: Date]`.
        let item = match item.split_once(':') {
            Some((label, rest))
                if label
                    .trim_end_matches('?')
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_') =>
            {
                rest.trim()
            }
            _ => item,
        };
        (!item.is_empty()).then(|| item.to_string())
    };
    !items.is_empty()
        && items
            .iter()
            .all(|item| item_type(item).is_some_and(|item| reads_as_type(&item)))
}

/// One type that signatures name and no file declares.
struct MissingType {
    name: String,
    /// Indexes of the files whose signatures name it, in manifest order.
    files: Vec<usize>,
    /// The first shape a comment gives for it: the file and the fields.
    shape: Option<(usize, String)>,
}

/// Give each type that a signature names a declaration, when no file of the
/// plan declares it (`PlanOptions::declare_types`): workers otherwise each
/// invent the type, or import it from a file that never exports it. Seen in
/// saved bench manifests: `loadState(): AppState  // {projects: Project[],
/// ...}` with no `AppState` anywhere, and `TaskStatus` in props where the
/// types file declares `Status`.
///
/// A type whose shape the comment of a signature that returns it gives
/// (`f(): X  // {a: T, b: U}`) becomes `export interface X { a: T; b: U }`.
/// A type with no shape whose name ends with exactly one type that the
/// Types-layer file declares (`TaskStatus` and `Status`) becomes an alias
/// there: `export type TaskStatus = Status`. A comment shape of default
/// values (`{projects: [], settings: {theme: 'light'}}`) is no shape. Any
/// other type with no shape becomes an `export type X` line that asks for an
/// alias of a declared type when one fits (a status union is not an
/// interface), else an interface with the fields the files that name it
/// use. The owner is the first Types-layer file when the plan has one (for
/// a shape: when every type the shape names is declared there, or the
/// shape's own file is a Consumer), else the shape's file, else the first
/// non-Consumer, non-test file that names the type; with none, nothing is
/// added for that type. Only `.ts`/`.tsx` files count. A shape can name a
/// type that nothing declares either, so the pass runs again on what it
/// added (at most three rounds). Idempotent: a declared type is never added
/// again. Does nothing for a profile whose `staging.type_decl_prefixes` has
/// no `export interface `.
pub fn declare_signature_types(manifest: &mut Manifest, staging: &Staging) {
    if !staging.type_decl_prefixes.contains(&"export interface ") {
        return;
    }
    for _ in 0..3 {
        if !declare_signature_types_once(manifest, staging) {
            break;
        }
    }
}

/// One round of [`declare_signature_types`]; true when it added a line.
fn declare_signature_types_once(manifest: &mut Manifest, staging: &Staging) -> bool {
    let is_code = |path: &str| path.ends_with(".ts") || path.ends_with(".tsx");
    let mut declared: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut types_declared: std::collections::HashSet<String> = std::collections::HashSet::new();
    for file in manifest.files.iter().filter(|file| is_code(&file.path)) {
        let in_types = staging.layer_of(&file.path) == Some(Layer::Types);
        for line in &file.exports {
            for name in declared_names(&bare_signature(line)) {
                if in_types {
                    types_declared.insert(name.clone());
                }
                declared.insert(name);
            }
        }
    }
    let mut missing: Vec<MissingType> = Vec::new();
    for (index, file) in manifest.files.iter().enumerate() {
        if !is_code(&file.path) || staging.is_test(&file.path) {
            continue;
        }
        for line in &file.exports {
            let signature = bare_signature(line);
            let comment = line.split_once("//").map(|(_, rest)| rest);
            for name in used_type_names(&signature) {
                if declared.contains(&name) {
                    continue;
                }
                let position = match missing.iter().position(|entry| entry.name == name) {
                    Some(position) => position,
                    None => {
                        missing.push(MissingType {
                            name: name.clone(),
                            files: Vec::new(),
                            shape: None,
                        });
                        missing.len() - 1
                    }
                };
                let entry = &mut missing[position];
                if !entry.files.contains(&index) {
                    entry.files.push(index);
                }
                let returns_it = signature
                    .trim_end()
                    .strip_suffix(name.as_str())
                    .is_some_and(|rest| rest.trim_end().ends_with(':'));
                if entry.shape.is_none() && returns_it {
                    entry.shape = comment.and_then(shape_fields).map(|shape| (index, shape));
                }
            }
        }
    }
    if missing.is_empty() {
        return false;
    }
    let types_file = manifest
        .files
        .iter()
        .position(|file| is_code(&file.path) && staging.layer_of(&file.path) == Some(Layer::Types));
    let is_consumer = |index: usize| {
        let path = &manifest.files[index].path;
        staging.layer_of(path) == Some(Layer::Consumer) || staging.is_test(path)
    };
    let shapeless: Vec<&str> = missing
        .iter()
        .filter(|entry| entry.shape.is_none())
        .map(|entry| entry.name.as_str())
        .collect();
    // The types the Types-layer file really declares (not a shapeless line
    // that an earlier round added), for an alias: a missing `TaskStatus`
    // whose name ends with exactly one of them (`Status`, four letters or
    // more, at a capital) is `export type TaskStatus = Status`.
    let real_types: std::collections::BTreeSet<&str> = types_file
        .map(|types| {
            manifest.files[types]
                .exports
                .iter()
                .filter_map(|line| {
                    let name = declared_type(line, staging.type_decl_prefixes)?;
                    let rest = line.trim_start().split_once(name)?.1.trim_start();
                    (!rest.starts_with("//")).then_some(name)
                })
                .collect()
        })
        .unwrap_or_default();
    let alias_of = |name: &str| -> Option<String> {
        let mut found = real_types.iter().filter(|target| {
            target.len() >= 4
                && target.len() < name.len()
                && target.starts_with(|c: char| c.is_ascii_uppercase())
                && name.ends_with(*target)
        });
        let target = found.next()?;
        found.next().is_none().then(|| target.to_string())
    };
    let mut added: Vec<(usize, String)> = Vec::new();
    for entry in &missing {
        let first_core = entry
            .files
            .iter()
            .copied()
            .find(|&index| !is_consumer(index));
        let (owner, line) = match &entry.shape {
            Some((shape_file, fields)) => {
                let in_types = used_type_names(fields).iter().all(|name| {
                    types_declared.contains(name)
                        || (types_file.is_some() && shapeless.contains(&name.as_str()))
                });
                let owner = match types_file {
                    Some(types) if in_types || is_consumer(*shape_file) => Some(types),
                    _ if !is_consumer(*shape_file) => Some(*shape_file),
                    _ => first_core,
                };
                let line = format!(
                    "export interface {} {{ {fields} }}  // declared here for the signatures \
                     that name it",
                    entry.name
                );
                (owner, line)
            }
            None if alias_of(&entry.name).is_some() => {
                let line = format!(
                    "export type {} = {};  // declared here for the signatures that name it",
                    entry.name,
                    alias_of(&entry.name).unwrap_or_default()
                );
                (types_file, line)
            }
            None => {
                let users: Vec<String> = entry
                    .files
                    .iter()
                    .map(|&index| format!("`{}`", manifest.files[index].path))
                    .collect();
                let line = format!(
                    "export type {}  // declared here for the signatures that name it (in {}): \
                     an alias of a type declared above when one fits, else an interface with \
                     the fields those files use",
                    entry.name,
                    users.join(", ")
                );
                (types_file.or(first_core), line)
            }
        };
        if let Some(owner) = owner {
            added.push((owner, line));
        }
    }
    let prefixes = staging.type_decl_prefixes;
    let changed = !added.is_empty();
    for (owner, line) in added {
        let exports = &mut manifest.files[owner].exports;
        let at = exports
            .iter()
            .rposition(|export| declared_type(export, prefixes).is_some())
            .map_or(0, |last| last + 1);
        exports.insert(at, line);
    }
    changed
}

/// Append the profile's `staging.data_note` to the purpose of each
/// Data-layer file (seed records) that does not have it yet.
pub fn apply_data_note(manifest: &mut Manifest, staging: &Staging) {
    let note = staging.data_note;
    if note.is_empty() {
        return;
    }
    for file in &mut manifest.files {
        if staging.layer_of(&file.path) == Some(Layer::Data) && !file.purpose.contains(note) {
            file.purpose.push_str(note);
        }
    }
}

/// Ask each Data-layer file (seed records) of `profile` for records dated
/// around `today` (`(year, month, day)`): old dates leave a calendar empty
/// and make each record overdue. A file whose purpose already names a day
/// keeps it. Public so a host can stamp a manifest it plans itself; with
/// `PlanOptions::today` the plan does it on its own copy.
pub fn stamp_today(manifest: &mut Manifest, profile: &Profile, today: (i32, u32, u32)) {
    let (year, month, day) = today;
    for file in &mut manifest.files {
        if profile.staging.layer_of(&file.path) == Some(Layer::Data)
            && !file.purpose.contains("Today is ")
        {
            file.purpose.push_str(&format!(
                " Today is {year:04}-{month:02}-{day:02}: put the dates of the records in the \
                 three weeks before and the three weeks after this day, some in the past and \
                 some in the future."
            ));
        }
    }
}

/// The sentence that `stamp_today` adds, from its start up to its end in
/// `purpose`: `(start, end)` byte offsets, or `None`.
fn today_stamp(purpose: &str) -> Option<(usize, usize)> {
    const END: &str = "some in the future.";
    let start = purpose.find(" Today is ")?;
    let end = purpose[start..]
        .find(END)
        .map_or(purpose.len(), |offset| start + offset + END.len());
    Some((start, end))
}

/// Ask each Data-layer file (seed records) of `profile` for dates computed
/// when the data loads, relative to the current date: its purpose gets the
/// profile's `staging.quality.date_note` in place of a fixed day (a
/// `stamp_today` sentence already there is removed). Fixed dates go stale,
/// and dates around one day leave a 12-month chart empty. Does nothing
/// without a date note. Idempotent.
pub fn stamp_relative_dates(manifest: &mut Manifest, profile: &Profile) {
    let note = profile.staging.quality.date_note;
    if note.is_empty() {
        return;
    }
    for file in &mut manifest.files {
        if profile.staging.layer_of(&file.path) != Some(Layer::Data) {
            continue;
        }
        if let Some((start, end)) = today_stamp(&file.purpose) {
            file.purpose.replace_range(start..end, "");
        }
        if !file.purpose.contains(note) {
            file.purpose.push_str(note);
        }
    }
}

/// The profile's `staging.quality.rewrites` on every purpose and export
/// line (NEXTJS: the dark theme selector `.theme-dark` becomes
/// `:root[data-theme="dark"]`, also in the CSS split's part 1). Idempotent
/// when no replacement contains its own pattern.
pub fn apply_quality_rewrites(manifest: &mut Manifest, staging: &Staging) {
    let rewrites = staging.quality.rewrites;
    if rewrites.is_empty() {
        return;
    }
    for file in manifest
        .files
        .iter_mut()
        .chain(manifest.test_files.iter_mut())
    {
        for &(from, to) in rewrites {
            if file.purpose.contains(from) {
                file.purpose = file.purpose.replace(from, to);
            }
            for line in &mut file.exports {
                if line.contains(from) {
                    *line = line.replace(from, to);
                }
            }
        }
    }
}

/// A string union member reduced to what spelling cannot change: lower
/// case letters and digits only (`'In Progress'`, `'in-progress'` and
/// `'in_progress'` agree).
fn literal_key(text: &str) -> String {
    text.chars()
        .filter(|ch| ch.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

/// The quoted string literals in `text`: `(start, end)` byte offsets of
/// their contents (without the quotes). A literal is short (at most 40
/// bytes), on one line, has a letter, and holds only letters, digits,
/// spaces, `-` and `_`; an opening quote right after a letter (`member's`)
/// is an apostrophe, not a literal.
pub(crate) fn string_literals(text: &str) -> Vec<(usize, usize)> {
    let bytes = text.as_bytes();
    let mut found = Vec::new();
    let mut index = 0;
    while index < bytes.len() {
        let quote = bytes[index];
        if quote != b'\'' && quote != b'"' {
            index += 1;
            continue;
        }
        if index > 0 && bytes[index - 1].is_ascii_alphanumeric() {
            index += 1;
            continue;
        }
        let start = index + 1;
        let close = text[start..]
            .char_indices()
            .take_while(|&(offset, ch)| offset <= 40 && ch != '\n')
            .find(|&(_, ch)| ch as u32 == u32::from(quote))
            .map(|(offset, _)| start + offset);
        let Some(end) = close else {
            index += 1;
            continue;
        };
        let inner = &text[start..end];
        let plain = !inner.is_empty()
            && inner.chars().any(char::is_alphabetic)
            && inner
                .chars()
                .all(|ch| ch.is_alphanumeric() || ch == ' ' || ch == '-' || ch == '_');
        if plain {
            found.push((start, end));
            index = end + 1;
        } else {
            index += 1;
        }
    }
    found
}

/// The members of the string unions that `line` declares: each run of two
/// or more quoted literals joined by `|` in the code before its `//` note
/// (`type Status = 'todo' | 'done'`, `theme: 'light' | 'dark'`).
pub(crate) fn union_members(line: &str) -> Vec<String> {
    let head = line.split("//").next().unwrap_or(line);
    let literals = string_literals(head);
    let mut members = Vec::new();
    let mut run: Vec<String> = Vec::new();
    let mut last_end: Option<usize> = None;
    for (start, end) in literals {
        let joined = last_end.is_some_and(|last| head[last + 1..start - 1].trim() == "|");
        if !joined {
            if run.len() >= 2 {
                members.append(&mut run);
            }
            run.clear();
        }
        run.push(head[start..end].to_string());
        last_end = Some(end);
    }
    if run.len() >= 2 {
        members.append(&mut run);
    }
    members
}

/// Words that mark display text: a literal after one of them in the same
/// clause is a label, not a union value.
const LABEL_WORDS: &[&str] = &[
    "label", "title", "display", "heading", "caption", "header", "button", "format",
];

/// Whether the literal at `start..end` of `text` (contents, without the
/// quotes) is used as a value, the only places where its spelling must be
/// the types' one: compared (`=== 'Done'`, `'Done' !== s`), a `case` label,
/// a member of a union (`'a' | 'b'`), or an item of a list of literals only
/// (`['Backlog','Done']`). A literal after a mapping arrow (`'done' ->
/// 'Done'`, `=> 'Done'`) is the mapped text, never a value; anything else
/// (prose, a button's text) is left alone.
pub(crate) fn in_value_context(text: &str, start: usize, end: usize) -> bool {
    let before = text[..start - 1].trim_end();
    let after = text.get(end + 1..).unwrap_or("").trim_start();
    if before.ends_with("->") || before.ends_with("=>") {
        return false;
    }
    let compared = |side: &str, at_end: bool| {
        ["===", "!==", "==", "!="].iter().any(|op| {
            if at_end {
                side.ends_with(op)
            } else {
                side.starts_with(op)
            }
        })
    };
    if compared(before, true) || compared(after, false) {
        return true;
    }
    let union = |side: &str, at_end: bool| {
        if at_end {
            side.ends_with('|') && !side.ends_with("||")
        } else {
            side.starts_with('|') && !side.starts_with("||")
        }
    };
    if union(before, true) || union(after, false) {
        return true;
    }
    if before.ends_with("case")
        && !before[..before.len() - 4]
            .chars()
            .next_back()
            .is_some_and(char::is_alphanumeric)
    {
        return true;
    }
    // A list of literals only: from the last `[` up to this literal, only
    // other literals, commas and spaces.
    let Some(open) = before.rfind('[') else {
        return false;
    };
    let list = &text[open + 1..start - 1];
    let mut rest = String::with_capacity(list.len());
    let mut copied = 0;
    for (a, b) in string_literals(list) {
        rest.push_str(&list[copied..a - 1]);
        copied = b + 1;
    }
    rest.push_str(&list[copied.min(list.len())..]);
    rest.chars().all(|ch| ch == ',' || ch.is_whitespace())
}

/// Whether the literal at `start..end` of `text` is display text: the value
/// of a `key: 'Label'` entry whose key is the same member (a labels map),
/// or a literal in a clause (since the last `;`) that names labels.
fn is_label(text: &str, start: usize, key: &str) -> bool {
    let before = text[..start - 1].trim_end();
    if let Some(entry) = before.strip_suffix(':') {
        let entry = entry.trim_end();
        let entry = entry.trim_end_matches(['\'', '"']);
        let word_start = entry
            .rfind(|ch: char| !(ch.is_alphanumeric() || ch == '-' || ch == '_' || ch == ' '))
            .map_or(0, |at| at + 1);
        if literal_key(&entry[word_start..]) == key {
            return true;
        }
    }
    let clause_start = before.rfind(';').map_or(0, |at| at + 1);
    let clause = before[clause_start..].to_lowercase();
    LABEL_WORDS.iter().any(|word| clause.contains(word))
}

/// The string literals of `text` that equal a member of `unions` apart
/// from spelling, rewritten to the member; `None` when nothing changes.
fn align_text(
    text: &str,
    unions: &std::collections::HashMap<String, Option<String>>,
) -> Option<String> {
    let mut out = String::with_capacity(text.len());
    let mut copied = 0;
    let mut changed = false;
    for (start, end) in string_literals(text) {
        let inner = &text[start..end];
        let key = literal_key(inner);
        let Some(Some(member)) = unions.get(&key) else {
            continue;
        };
        if member == inner || !in_value_context(text, start, end) || is_label(text, start, &key) {
            continue;
        }
        out.push_str(&text[copied..start]);
        out.push_str(member);
        copied = end;
        changed = true;
    }
    changed.then(|| {
        out.push_str(&text[copied..]);
        out
    })
}

/// Give the literal values in the plan the spelling of the shared types:
/// a quoted literal in a purpose or export line of a file outside the
/// Types layer that equals a member of a string union declared in a
/// Types-layer file apart from case, spaces and separators is rewritten to
/// that member (`status!=='Done'` becomes `status!=='done'` when the types
/// say `'backlog' | … | 'done'`). Only a literal used as a value is
/// rewritten (`in_value_context`: compared, a `case`, a union member, an
/// item of a list of literals); display text stays: a mapped text (`'done'
/// -> 'Done'`), prose, the value of a `member: 'Label'` entry and a literal
/// in a clause that names labels, titles, buttons or formats. Two members with the same letters (`'in-progress'`,
/// `'In Progress'`) make that spelling ambiguous: it is left alone.
/// Returns the number of literals rewritten. Idempotent.
pub fn align_union_literals(manifest: &mut Manifest, staging: &Staging) -> usize {
    let mut unions: std::collections::HashMap<String, Option<String>> =
        std::collections::HashMap::new();
    for file in &manifest.files {
        if staging.layer_of(&file.path) != Some(Layer::Types) {
            continue;
        }
        for member in file.exports.iter().flat_map(|line| union_members(line)) {
            let key = literal_key(&member);
            match unions.get(&key) {
                Some(Some(known)) if *known != member => {
                    unions.insert(key, None);
                }
                Some(_) => {}
                None => {
                    unions.insert(key, Some(member));
                }
            }
        }
    }
    if unions.is_empty() {
        return 0;
    }
    let mut count = 0;
    for file in &mut manifest.files {
        if staging.layer_of(&file.path) == Some(Layer::Types) {
            continue;
        }
        let texts = std::iter::once(&mut file.purpose).chain(file.exports.iter_mut());
        for text in texts {
            if let Some(aligned) = align_text(text, &unions) {
                count += string_literals(text)
                    .iter()
                    .zip(string_literals(&aligned))
                    .filter(|&(&(a, b), (c, d))| text[a..b] != aligned[c..d])
                    .count();
                *text = aligned;
            }
        }
    }
    count
}

/// A file's purpose as the planner wrote it, without the notes that the
/// manifest passes add (the profile's `data_note` and `quality.date_note`,
/// the `stamp_today` sentence, the "Imports X from …" notes of the type
/// passes, the local components of `PlanOptions::page_local`); the purpose
/// of a part of a split file is cut to its first
/// sentence ("Part 1 of 3 of the global style sheet."). For the README and
/// the module head of `PlanOptions::objective_pages`.
pub fn planner_purpose(purpose: &str, staging: &Staging) -> String {
    let mut text = purpose.to_string();
    // The page-local note ends the purpose (`pagelocal`).
    let local = crate::pagelocal::local_mark(&staging.quality);
    if !local.is_empty() {
        if let Some(at) = text.find(local) {
            text.truncate(at);
        }
    }
    // The notes of a split page and of its parts end the purpose too
    // (`splitpage`).
    for mark in crate::splitpage::note_marks(&staging.quality) {
        if let Some(at) = text.find(mark) {
            text.truncate(at);
        }
    }
    for note in [staging.data_note, staging.quality.date_note] {
        if !note.is_empty() {
            text = text.replace(note, "");
        }
    }
    if let Some((start, end)) = today_stamp(&text) {
        text.replace_range(start..end, "");
    }
    const MARK: &str = ": this file does not declare";
    while let Some(mark) = text.find(MARK) {
        let Some(start) = text[..mark].rfind(" Imports ") else {
            break;
        };
        let rest = &text[mark..];
        let end = match rest.find("with that name") {
            Some(at) if rest[at..].starts_with("with that name.") => mark + at + 15,
            Some(at) => rest[at..]
                .find("`.")
                .map_or(text.len(), |close| mark + at + close + 2),
            None => text.len(),
        };
        text.replace_range(start..end, "");
    }
    let text = text.trim();
    if text.starts_with("Part ") && text.contains(" of ") {
        if let Some(cut) = text.find([':', '.']) {
            return format!("{}.", &text[..cut]);
        }
    }
    text.to_string()
}

/// Where `plan_project_with` gets its manifest and what runs on it before
/// the plan is built. The default calls the architect and changes nothing.
#[derive(Default)]
pub struct ManifestSource<'a, 'b> {
    /// Plan this saved manifest instead of calling the architect.
    pub saved: Option<Manifest>,
    /// Runs on the manifest (generated or saved) before the plan is built,
    /// and before the profile's `normalize_manifest`. An error stops planning.
    pub prepare: Option<&'a mut ManifestPass<'b>>,
}

/// A pass over the manifest before it is planned (see `ManifestSource`).
pub type ManifestPass<'b> = dyn FnMut(&mut Manifest) -> Result<()> + Send + 'b;

/// Plan a greenfield build the way the CLI does (the CLI calls
/// `plan_project_with`; `RunManager` calls this when a request names a
/// profile or plan options): the architect
/// writes the manifest, then `plan_from_manifest` builds the wave. `profile`
/// wins over `architect.profile`; with neither, it is detected from the
/// objective. Returns the manifest as planned (before the profile's
/// `normalize_manifest`, which runs on a copy) and the plan.
pub async fn plan_project(
    client: &dyn ModelClient,
    objective: &str,
    max_tasks: usize,
    steering: &[String],
    architect: &ArchitectOptions,
    options: &PlanOptions,
    profile: Option<&'static Profile>,
) -> Result<(Manifest, Plan)> {
    plan_project_with(
        client,
        objective,
        max_tasks,
        steering,
        architect,
        options,
        profile,
        ManifestSource::default(),
    )
    .await
}

/// `plan_project` with a saved manifest or a pass over the manifest (see
/// `ManifestSource`).
#[allow(clippy::too_many_arguments)]
pub async fn plan_project_with(
    client: &dyn ModelClient,
    objective: &str,
    max_tasks: usize,
    steering: &[String],
    architect: &ArchitectOptions,
    options: &PlanOptions,
    profile: Option<&'static Profile>,
    source: ManifestSource<'_, '_>,
) -> Result<(Manifest, Plan)> {
    let profile = profile
        .or(architect.profile)
        .unwrap_or_else(|| Profile::detect(objective));
    let mut manifest = match source.saved {
        Some(manifest) => manifest,
        None => {
            let architect = ArchitectOptions {
                profile: Some(profile),
                ..architect.clone()
            };
            generate_manifest(client, objective, max_tasks, steering, &architect).await?
        }
    };
    if let Some(prepare) = source.prepare {
        prepare(&mut manifest)?;
    }
    // `split_pages`: the local components of the final plan's pages get
    // files of their own (the manifest then holds the passes too; they are
    // idempotent, so the plan below runs them again on the same text).
    if let Some(split) = &architect.split_pages {
        crate::splitpage::split_pages(
            client,
            objective,
            &mut manifest,
            max_tasks,
            profile,
            options,
            split,
            architect,
        )
        .await;
    }
    let plan = plan_from_manifest(objective, &manifest, max_tasks, Some(profile), options)?;
    Ok((manifest, plan))
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
    /// The app contract (the profile's `staging.quality.contract_rule`): the
    /// planner rules ask the Types-layer file for the shared facts as code
    /// (unions with labels, catalogs with ids, the storage key, the whole
    /// app state, the settings with defaults, meanings and readers) and one
    /// owner for each collection and UI control. The Types-layer fragment
    /// sees every planned file (path: purpose), and the fragments' export
    /// limit is the profile's `contract_cap`. After the export fragments
    /// (and `wiring`), `contract::contract_problems` checks the merged
    /// manifest; each fragment that owns a problem is asked once more with
    /// its problems (best effort: a failed repair keeps the first answer).
    /// No effect for a profile without a contract rule (PYTHON, RUST).
    pub contract: bool,
    /// Pages own their features: the planner rules get the profile's
    /// `staging.quality.page_local_rule` (a component that only one page
    /// renders is written in that page's file; shared components are
    /// presentational; a page names the shared components it renders). The
    /// plan side is `PlanOptions::page_local`. No effect for a profile
    /// without the rule (PYTHON, RUST).
    pub page_local: bool,
    /// The conventions (`semantics`): the planner rules get the profile's
    /// `staging.quality.semantics_rule` (each money field's unit and sign,
    /// each date field's format, each status union's transitions with the
    /// one lib function that makes them, stated in the Types-layer file; one
    /// state owner; the display rules), and with `page_local` its
    /// `semantic_page_local_rule` in place of `page_local_rule`. The
    /// Types-layer fragment sees every planned file (as with `contract`);
    /// without `contract` the fragments' export limit is the profile's
    /// `semantics_cap`. After the export fragments,
    /// `semantics::semantic_problems` checks the merged manifest and each
    /// fragment that owns a problem is asked once more (with `contract`, in
    /// the same round as the contract problems). The plan side is
    /// `PlanOptions::semantics`. No effect for a profile without the rule
    /// (PYTHON, RUST).
    pub semantics: bool,
    /// Types first: the Types-layer files are planned alone in a first
    /// round (with the files that have no layer, which need no types), and
    /// every other export fragment then gets their exports to use verbatim
    /// (same unions, record shapes and id fields). Costs one more fragment
    /// round (the types fragment's latency). No effect when the skeleton has
    /// no Types-layer file.
    pub types_first: bool,
    /// Layered planning (implies `types_first`): three fragment rounds. One:
    /// the Types-layer files alone (with the layerless files). Two: the
    /// Core- and Data-layer files in pairs, each with the planned types and
    /// the path and purpose of every round-two file (`reference_list`), so
    /// two pairs do not each plan the same helper or rule. Three: every
    /// other file in pairs, each with the planned types, the planned core
    /// exports (`core_block`) and the path and purpose of every round-three
    /// file (`consumer_reference`), so a file calls the core as planned and
    /// leaves a job that another file in the list owns to that file. Round
    /// three waits for the Core-layer fragments only; the Data-layer ones
    /// run beside it. Costs one more fragment round. No effect when the
    /// skeleton has no Core- or Data-layer file (then as `types_first`).
    pub layered: bool,
    /// With `layered`: round two plans the Core-layer files together in the
    /// fewest fragments of at most `CORE_WHOLE_MAX` files instead of pairs,
    /// each with an output budget and a character limit scaled by its file
    /// count (`fragment_budget`); its critical path is one long fragment
    /// (compare its per-round trace times with `layered` alone). No effect
    /// without `layered`.
    pub core_whole: bool,
    /// Dangling references:`references::dangling_problems` joins the
    /// merged-manifest check (a `lib/x` path or a `via name` that no file
    /// plans), and its problems go to the owning fragments in the same
    /// single repair round as the contract's.
    pub references: bool,
    /// Whole-plan review: one best-effort model call over the merged
    /// contracts asks for cross-file problems (a missing rule parameter, a
    /// narrowed type, one record modelled two ways, an unowned cross-record
    /// effect, a named but unplanned export); the problems join the single
    /// repair round. A failed, late or unparsable answer changes nothing.
    pub review: bool,
    /// Pipelined planning: the skeleton call streams into an incremental
    /// parser, and each first-round export fragment whose prompt is already
    /// settled (its files listed; with `types_first` a Types-layer file
    /// alone; a fragment whose prompt lists every planned file waits for the
    /// closed file list) starts before the skeleton call returns. Once the
    /// skeleton is final, a started fragment is kept only when its request
    /// is byte-identical to the one today's path would send (same prompt,
    /// budget and files); any other is dropped and sent as before. No effect
    /// when the skeleton comes from the objective (no call to stream).
    pub plan_pipe: bool,
    /// Types from the paths (with `plan_pipe` and `types_first`; default
    /// off): the skeleton call writes the path of every file first
    /// (`"paths"`), then the files with their purposes, and the Types-layer
    /// fragment starts as soon as that list has streamed, beside the rest of
    /// the skeleton, instead of after it. For that its request no longer
    /// depends on the purposes: it lists the planned files by path alone and
    /// names its own file with a fixed purpose (the skeleton's purpose is
    /// put back on the planned file). A fragment started this way is kept
    /// only when the final skeleton sends the same request, as with
    /// `plan_pipe`.
    pub types_from_paths: bool,
    /// Split pages (`crate::splitpage`): once the manifest is final (after
    /// its passes), each page with local components (`PlanOptions::page_local`)
    /// gives them files of their own beside it, each with a typed contract
    /// (the plan's signature, or one cheap call per page for the parts the
    /// plan gives none), and the page imports and composes them; the parts
    /// and the page are then ordinary wave tasks with the same deps, so the
    /// longest page no longer ends the wave alone. `None` (the default): off.
    pub split_pages: Option<crate::splitpage::SplitPages>,
    /// Compact skeleton (`crate::skelcompact`): the skeleton call asks for one
    /// `path: purpose` line per file (purposes of at most 12 words) instead
    /// of pretty-printed JSON, and code builds the same skeleton from the
    /// lines (a JSON answer is still read), so the call decodes about 0.6x
    /// the tokens. A failed call or an answer that is neither form falls back
    /// to today's skeleton request (with `plan_pipe`, streamed as before).
    /// No effect when the skeleton comes from the objective. `None` (the
    /// default): off, today's request only.
    pub skel_compact: Option<crate::skelcompact::SkelCompact>,
    /// Gets the manifest as it stands when the contract repair is about to
    /// ask the model (after the deterministic fixes, before the answers),
    /// once: a host can start the files that the repair is unlikely to change
    /// while the repair round runs (about 3 s), and compare with the manifest
    /// that `generate_manifest` returns. Nothing is sent when the check finds
    /// no problem (no round runs). `None` (the default): off.
    pub early_manifest: Option<EarlyManifest>,
}

/// Where [`ArchitectOptions::early_manifest`] sends the manifest.
#[derive(Debug, Clone)]
pub struct EarlyManifest(
    std::sync::Arc<std::sync::Mutex<Option<tokio::sync::oneshot::Sender<Manifest>>>>,
);

impl EarlyManifest {
    /// The sender for the options and the receiver for the host.
    pub fn channel() -> (Self, tokio::sync::oneshot::Receiver<Manifest>) {
        let (sender, receiver) = tokio::sync::oneshot::channel();
        (
            Self(std::sync::Arc::new(std::sync::Mutex::new(Some(sender)))),
            receiver,
        )
    }

    fn send(&self, manifest: Manifest) {
        if let Some(sender) = self.0.lock().ok().and_then(|mut slot| slot.take()) {
            let _ = sender.send(manifest);
        }
    }
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
            contract: false,
            page_local: false,
            semantics: false,
            types_first: false,
            layered: false,
            core_whole: false,
            references: false,
            review: false,
            plan_pipe: false,
            types_from_paths: false,
            split_pages: None,
            skel_compact: None,
            early_manifest: None,
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
    let shared = architect_shared_context(
        max_tasks,
        profile,
        options.type_ownership,
        options.contract,
        options.page_local,
        options.semantics,
    );
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
    complete_fragment_sized(
        client,
        label,
        user,
        shared,
        options,
        FRAGMENT_BUDGET,
        validate,
    )
    .await
}

/// The first-shot output budget of a fragment (the ladder's low steps).
const FRAGMENT_BUDGET: u32 = 2_560;

/// The system prompt of every manifest fragment call (`complete_fragment`).
const FRAGMENT_SYSTEM: &str = "You are the architect for a greenfield software build. You \
decide the file layout and the exact interface contracts. Return JSON only.";

/// `ArchitectOptions::skel_compact`: one skeleton call that asks for the
/// line form (`skelcompact::compact_prompt`), as the first step of the
/// skeleton ladder would send it (same system prompt and shared context, so
/// the cached prefix is the same; first effort and budget; no schema). The
/// manifest in its answer (the line form, else JSON), or `Err` with the
/// reason (a failed call or an answer that is neither), after which the
/// caller sends today's skeleton request. The report notes the call.
async fn compact_skeleton(
    client: &dyn ModelClient,
    objective: &str,
    shared: &str,
    options: &ArchitectOptions,
    profile: &Profile,
    report: &mut crate::skelcompact::SkelReport,
) -> std::result::Result<Manifest, String> {
    let request = CompletionRequest {
        system: FRAGMENT_SYSTEM.to_string(),
        shared_context: Some(shared.to_string()),
        user: crate::skelcompact::compact_prompt(objective, profile.example_module),
        max_tokens: FRAGMENT_BUDGET,
        temperature: 0.1,
        reasoning_effort: Some(options.fragment_effort.clone()),
        prompt_cache_key: options.cache_key.clone(),
        allow_empty: false,
        response_format: None,
    };
    let started = std::time::Instant::now();
    let outcome = client.complete(request).await;
    report.call_ms = started.elapsed().as_millis() as u64;
    let done = outcome.map_err(|error| format!("compact skeleton call failed: {error:#}"))?;
    report.chars = done.text.len();
    report.output_tokens = done.output_tokens;
    // A cut answer still parses line by line, minus the files after the cut:
    // it falls back, as a cut JSON skeleton fails to parse.
    if done.finish_reason.as_deref() == Some("length") {
        return Err(format!(
            "compact skeleton cut at the output budget ({} tokens)",
            done.output_tokens
        ));
    }
    let (mut manifest, format) = crate::skelcompact::parse_skeleton(&done.text, |text| {
        parse_manifest(text).and_then(|json| manifest_from_json(&json).ok())
    })?;
    // The JSON form has a `test_files` field where the model puts its tests
    // (and they are replaced by the profile's own test layout); the line form
    // has none, so tests and the test runner's config came back as planned
    // files (2/4 runs: a 12 KB `tests/lib.test.ts` that hit the output cap,
    // and a `vitest.config.ts` whose include missed `tests/`, so no test ran).
    manifest.files.retain(|file| {
        let name = file.path.rsplit('/').next().unwrap_or(&file.path);
        !profile.staging.is_test(&file.path)
            && !name.starts_with("vitest.config.")
            && !name.starts_with("jest.config.")
    });
    if manifest.files.is_empty() {
        return Err("compact skeleton had only test files".to_string());
    }
    report.format = format;
    Ok(manifest)
}

/// A skeleton's tests belong in `test_files`, where the profile's test
/// layout replaces them. A test file that the model listed under `files`
/// was written as one more file beside that layout (4 of 68 runs: a
/// `tests/lib.test.ts` "coverage of all lib modules" of 3,900-7,100 tokens,
/// the last file of its wave by 15-25 s). It leaves `files`: into
/// `test_files` when the skeleton has none, else away. A test runner's
/// config leaves too (the profile has its own). A skeleton of only such
/// files is left as it is.
fn stray_tests_out(skeleton: &mut Manifest, profile: &Profile) {
    let stray = |file: &ManifestFile| {
        let name = file.path.rsplit('/').next().unwrap_or(&file.path);
        profile.staging.is_test(&file.path)
            || name.starts_with("vitest.config.")
            || name.starts_with("jest.config.")
    };
    if !skeleton.files.iter().any(stray) || skeleton.files.iter().all(stray) {
        return;
    }
    let (out, keep): (Vec<ManifestFile>, Vec<ManifestFile>) = std::mem::take(&mut skeleton.files)
        .into_iter()
        .partition(stray);
    skeleton.files = keep;
    tracing::info!(
        files = ?out.iter().map(|file| file.path.as_str()).collect::<Vec<_>>(),
        "skeleton: test files under `files` taken out"
    );
    if skeleton.test_files.is_empty() {
        skeleton.test_files = out
            .into_iter()
            .filter(|file| profile.staging.is_test(&file.path))
            .collect();
    }
}

/// The most files in one `ArchitectOptions::core_whole` fragment: two
/// fragments of up to 8 decode side by side faster than one of 14, and the
/// round-two reference list tells each about the other's files.
const CORE_WHOLE_MAX: usize = 8;

/// The first-shot output budget of a fragment that plans `files` files
/// together (`ArchitectOptions::core_whole`): 700 tokens a file, at least
/// `FRAGMENT_BUDGET`, at most 8_192.
fn fragment_budget(files: usize) -> u32 {
    (700 * files.min(64) as u32).clamp(FRAGMENT_BUDGET, 8_192)
}

/// The ladder's medium step for a first-shot `budget`: 4_096 for the
/// standard budget, else 1.6x the budget (at most 12_288).
fn medium_budget(budget: u32) -> u32 {
    if budget <= FRAGMENT_BUDGET {
        4_096
    } else {
        (budget * 8 / 5).min(12_288)
    }
}

/// `complete_fragment` with a first-shot output `budget` (the low steps;
/// the medium step is `medium_budget`).
async fn complete_fragment_sized<F>(
    client: &dyn ModelClient,
    label: &str,
    user: &str,
    shared: &str,
    options: &ArchitectOptions,
    budget: u32,
    validate: F,
) -> Result<String>
where
    F: Fn(&Manifest) -> bool,
{
    // Cleared when the server rejects json_schema (e.g. DeepSeek: "This
    // response_format type is unavailable now"); the step then reruns free.
    let mut schema = options.schema && label != "skeleton";
    let request = |effort: &str, max_tokens: u32, schema: bool| CompletionRequest {
        system: FRAGMENT_SYSTEM.to_string(),
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
    // The 3 s pause before a retry follows a failed call only: an answer
    // that did not parse or validate is no sign of load, and every later
    // planning round waits for this fragment.
    let mut pause = false;
    for (index, (effort, budget)) in [
        (first, budget),
        ("medium", medium_budget(budget)),
        (first, budget),
    ]
    .iter()
    .enumerate()
    {
        if index > 0 && pause {
            tokio::time::sleep(std::time::Duration::from_secs(3)).await;
        }
        pause = false;
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
                pause = true;
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
    // `types_from_paths`: the paths first, so the types fragment can start
    // while the purposes are written.
    let (paths_rule, paths_key) = if options.types_from_paths && options.plan_pipe {
        (
            "\n(first `paths`: the path of every file, in order; then `files`: the same \
             paths in the same order, each with its purpose)",
            format!("  \"paths\": [\"{}\"],\n", profile.example_module),
        )
    } else {
        ("", String::new())
    };
    let skeleton_user = format!(
        r#"Design the file manifest for this objective.

<objective>
{objective}
</objective>

Return only this JSON shape — paths and one-sentence purposes ONLY, no
exports (they come in a later pass), no test files, readme false:{paths_rule}
{{
{paths_key}  "files": [
    {{"path": "{module}", "purpose": "one sentence: what this file is responsible for"}}
  ],
  "test_files": [],
  "readme": false
}}
"#,
        module = profile.example_module,
    );
    let split_started = std::time::Instant::now();
    // `plan_pipe`: the export fragments started from the streaming skeleton.
    let early = EarlyFragments::new(split_started);
    let listed = options
        .objective_skeleton
        .then(|| skeleton_from_objective(objective, profile.listed_module_exts))
        .flatten();
    // `skel_compact`: the line-form skeleton first; on a failed call or an
    // answer that is not a skeleton, today's request below as before.
    let mut compact_report = crate::skelcompact::SkelReport::default();
    let compact = match (&listed, &options.skel_compact) {
        (None, Some(_)) => {
            match compact_skeleton(
                client,
                objective,
                shared,
                options,
                profile,
                &mut compact_report,
            )
            .await
            {
                Ok(manifest) => Some(manifest),
                Err(reason) => {
                    tracing::warn!(
                        call_ms = compact_report.call_ms,
                        chars = compact_report.chars,
                        "skel compact: {reason}; sending today's skeleton request"
                    );
                    compact_report.format = crate::skelcompact::SkelFormat::Fallback;
                    compact_report.reason = reason;
                    None
                }
            }
        }
        _ => None,
    };
    let mut skeleton = match (listed, compact) {
        (Some(skeleton), _) => {
            tracing::info!(
                files = skeleton.files.len(),
                "skeleton taken from the objective's module list (no model call)"
            );
            skeleton
        }
        (None, Some(skeleton)) => skeleton,
        // `plan_pipe`: the same call, read while it streams; settled export
        // fragments start in `early` before it returns.
        (None, None) if options.plan_pipe => manifest_from_json(
            &streamed_skeleton(
                client,
                objective,
                &skeleton_user,
                shared,
                options,
                profile,
                &early,
            )
            .await?,
        )?,
        (None, None) => manifest_from_json(
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
    let skeleton_ms = split_started.elapsed().as_millis() as u64;
    if let Some(compact) = &options.skel_compact {
        compact_report.skeleton_ms = skeleton_ms;
        compact_report.files = skeleton.files.len();
        tracing::info!(
            format = compact_report.format.as_str(),
            call_ms = compact_report.call_ms,
            skeleton_ms,
            chars = compact_report.chars,
            output_tokens = compact_report.output_tokens,
            files = compact_report.files,
            "skel compact: skeleton done"
        );
        if let Some(sink) = &compact.sink {
            if let Ok(mut slot) = sink.lock() {
                *slot = Some(compact_report);
            }
        }
    }
    stray_tests_out(&mut skeleton, profile);
    // Before export fragments, so contracts are written against final paths.
    (profile.normalize_skeleton)(&mut skeleton);

    let layout = fragment_layout(&skeleton, options, profile);
    let FragmentLayout {
        ref round_one,
        ref round_two,
        ref round_three,
        types_chunks,
        core_chunks,
        layered,
        types_first,
        contract,
        semantics,
        ..
    } = layout;
    // `referenced`: the chunk's round carries a reference list of other
    // files (`reference_list`), so the chunk names its own paths once more
    // after its listing (built per chunk: the shared prefix up to the listing
    // stays the same for the round).
    let exports_prompt = |chunk: &[ManifestFile], types_note: &str, referenced: bool| {
        exports_fragment(
            &layout, objective, options, profile, chunk, types_note, referenced,
        )
    };
    let mut fragments: Vec<ExportFragment> = round_one
        .iter()
        .map(|chunk| exports_prompt(chunk, "", false))
        .collect();
    // `plan_pipe`: keep only the early fragments whose request is the one
    // sent below (the others are dropped here, closing their streams).
    let (early_launched, early_kept) = early.keep_matching(&fragments);
    let early = &early;
    let started = std::time::Instant::now();
    let first_fragment_start_ms = early
        .first_launch_ms()
        .unwrap_or_else(|| split_started.elapsed().as_millis() as u64);
    let mut files = if round_two.is_empty() {
        run_fragments(client, shared, options, &fragments, 0, early).await?
    } else {
        let from = fragments.len();
        let (types_prompts, layerless_prompts) = fragments.split_at(types_chunks);
        // Round two waits only for the Types-layer fragments: the layerless
        // ones (package.json and the like) need no planned types, so they
        // run beside the types and round two instead of holding it back.
        let layerless = run_fragments(
            client,
            shared,
            options,
            layerless_prompts,
            types_chunks,
            early,
        );
        let typed_then_rest = async {
            let typed = run_fragments(client, shared, options, types_prompts, 0, early).await?;
            let block = types_block(&typed, &profile.staging);
            let types_wait_ms = started.elapsed().as_millis() as u64;
            // `layered`: every round-two file (Core and Data) sees the path
            // and job of the others, so two pairs do not each plan the same
            // helper or rule.
            let second_note = if layered {
                let listed: Vec<&ManifestFile> = round_two.iter().flatten().collect();
                format!(
                    "{block}{}",
                    reference_list(
                        &listed,
                        CORE_REFERENCE,
                        "Do not re-plan a job that a file in the reference list owns (no second \
                         helper, rule or calculation for it): import and call that file's \
                         function instead."
                    )
                )
            } else {
                block.clone()
            };
            let mut prompts: Vec<ExportFragment> = round_two
                .iter()
                .map(|chunk| exports_prompt(chunk, &second_note, layered))
                .collect();
            let second = std::time::Instant::now();
            if round_three.is_empty() {
                let rest = run_fragments(client, shared, options, &prompts, from, early).await?;
                tracing::info!(
                    types_wait_ms,
                    round_two_ms = second.elapsed().as_millis() as u64,
                    round_one = from,
                    round_two = prompts.len(),
                    types_chars = block.len(),
                    note_chars = second_note.len(),
                    "types first: round two planned against the planned types"
                );
                return Ok::<_, anyhow::Error>((typed, prompts, rest));
            }
            // `layered`: round three waits for the Core-layer fragments only;
            // the Data-layer ones (seed files, which consumers rarely need
            // signatures of) run on beside it and join the core block only
            // when they finish first.
            let (core_prompts, data_prompts) = prompts.split_at(core_chunks);
            let core_run = Box::pin(run_fragments(
                client,
                shared,
                options,
                core_prompts,
                from,
                early,
            ));
            let data_run = Box::pin(run_fragments(
                client,
                shared,
                options,
                data_prompts,
                from + core_chunks,
                early,
            ));
            let (mut rest, data_left) = match futures::future::select(core_run, data_run).await {
                futures::future::Either::Left((core, data_run)) => (core?, Some(data_run)),
                futures::future::Either::Right((data, core_run)) => {
                    let mut planned = core_run.await?;
                    planned.extend(data?);
                    (planned, None)
                }
            };
            let core_wait_ms = started.elapsed().as_millis() as u64;
            tracing::info!(
                types_wait_ms,
                core_round_ms = second.elapsed().as_millis() as u64,
                round_one = from,
                round_two = prompts.len(),
                data_in_core_block = data_left.is_none(),
                types_chars = block.len(),
                note_chars = second_note.len(),
                "layered: round two planned against the planned types"
            );
            // The rest against the planned types and core, with every
            // round-three file's job listed.
            let core = core_block(&rest, &profile.staging, profile.comment);
            let note = format!("{block}{core}{}", consumer_reference(round_three));
            let third_prompts: Vec<ExportFragment> = round_three
                .iter()
                .map(|chunk| exports_prompt(chunk, &note, true))
                .collect();
            let third = std::time::Instant::now();
            let third_run = run_fragments(
                client,
                shared,
                options,
                &third_prompts,
                from + prompts.len(),
                early,
            );
            let data_rest = async {
                match data_left {
                    Some(data_run) => data_run.await,
                    None => Ok(Vec::new()),
                }
            };
            let (planned, data) = futures::future::try_join(third_run, data_rest).await?;
            tracing::info!(
                core_wait_ms,
                ui_round_ms = third.elapsed().as_millis() as u64,
                round_two = prompts.len(),
                round_three = third_prompts.len(),
                core_chars = core.len(),
                note_chars = note.len(),
                "layered: round three planned against the planned types and core"
            );
            rest.extend(data);
            rest.extend(planned);
            prompts.extend(third_prompts);
            Ok::<_, anyhow::Error>((typed, prompts, rest))
        };
        let (mut planned, (typed, prompts, rest)) =
            futures::future::try_join(layerless, typed_then_rest).await?;
        planned.extend(typed);
        planned.extend(rest);
        fragments.extend(prompts);
        // Back to skeleton order (the plan and the later passes list files
        // in it).
        let order: std::collections::HashMap<&str, usize> = skeleton
            .files
            .iter()
            .enumerate()
            .map(|(index, file)| (file.path.as_str(), index))
            .collect();
        planned.sort_by_key(|file| order.get(file.path.as_str()).copied().unwrap_or(usize::MAX));
        planned
    };
    // `types_from_paths`: the types fragment named its file with a fixed
    // purpose; the planned file keeps the skeleton's (here and after the
    // repair round, whose answer names the file the same way).
    let types_purposes: Vec<(String, String)> = if options.types_from_paths {
        skeleton
            .files
            .iter()
            .filter(|file| profile.staging.layer_of(&file.path) == Some(Layer::Types))
            .map(|file| (file.path.clone(), file.purpose.clone()))
            .collect()
    } else {
        Vec::new()
    };
    let keep_purposes = |files: &mut Vec<ManifestFile>| {
        for file in files.iter_mut() {
            if let Some((_, purpose)) = types_purposes.iter().find(|(path, _)| *path == file.path) {
                file.purpose = purpose.clone();
            }
        }
    };
    keep_purposes(&mut files);
    // The planning timeline (`plan_pipe` A/B), from the split's start.
    tracing::info!(
        plan_pipe = options.plan_pipe,
        skeleton_ms,
        first_fragment_start_ms,
        fragments_done_ms = split_started.elapsed().as_millis() as u64,
        early_launched,
        early_kept,
        "planning timeline: export fragments planned"
    );
    // The review reads purposes and exports only, never `uses`: it runs
    // beside the wiring pass on a snapshot, so it costs no wall time unless
    // it outlasts the wiring.
    let snapshot = options.review.then(|| files.clone());
    let wiring = async {
        let wiring_started = std::time::Instant::now();
        if options.wiring {
            wire_manifest(client, objective, shared, options, &mut files).await;
        }
        wiring_started.elapsed().as_millis() as u64
    };
    let review = async {
        match &snapshot {
            Some(snapshot) => review_manifest(client, objective, shared, options, snapshot).await,
            None => Vec::new(),
        }
    };
    let (wiring_ms, review) = futures::future::join(wiring, review).await;
    if options.review && options.wiring {
        tracing::info!(wiring_ms, "plan review ran beside the wiring pass");
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
    if contract || semantics || options.references || !review.is_empty() {
        let types_before = types_first.then(|| types_block(&files, &profile.staging));
        // `early_manifest`: the manifest that the repair round starts from.
        let early = |files: &[ManifestFile]| {
            if let Some(sink) = &options.early_manifest {
                sink.send(Manifest {
                    files: files.to_vec(),
                    test_files: test_files.clone(),
                    readme: lower.contains("readme"),
                });
            }
        };
        repair_contract(
            client, shared, options, profile, &fragments, &mut files, review, &early,
        )
        .await;
        keep_purposes(&mut files);
        if let Some(before) = types_before {
            let changed = types_block(&files, &profile.staging) != before;
            tracing::info!(
                types_changed_in_repair = changed,
                "types first: types after the repair round"
            );
        }
    }

    tracing::info!(
        plan_pipe = options.plan_pipe,
        manifest_done_ms = split_started.elapsed().as_millis() as u64,
        "planning timeline: manifest done (wiring, review and repair included)"
    );
    Ok(Manifest {
        files,
        test_files,
        readme: lower.contains("readme"),
    })
}

/// The export fragments' layout for a final (or, with
/// `ArchitectOptions::plan_pipe`, a partial) skeleton: the rounds' chunks and
/// what every fragment prompt is built from besides its chunk.
struct FragmentLayout<'p> {
    round_one: Vec<Vec<ManifestFile>>,
    round_two: Vec<Vec<ManifestFile>>,
    round_three: Vec<Vec<ManifestFile>>,
    types_chunks: usize,
    core_chunks: usize,
    layered: bool,
    types_first: bool,
    core_whole: bool,
    contract: bool,
    semantics: bool,
    every_file: String,
    cap: &'p str,
}

impl FragmentLayout<'_> {
    /// True when `chunk`'s prompt lists every planned file (the Types-layer
    /// fragment with `contract` or `semantics`).
    fn lists_every_file(&self, chunk: &[ManifestFile], profile: &Profile) -> bool {
        (self.contract || self.semantics)
            && chunk
                .iter()
                .any(|file| profile.staging.layer_of(&file.path) == Some(Layer::Types))
    }
}

/// The rounds of export fragments for `skeleton` (`split_manifest`).
fn fragment_layout<'p>(
    skeleton: &Manifest,
    options: &ArchitectOptions,
    profile: &'p Profile,
) -> FragmentLayout<'p> {
    // Two files per exports fragment: measured bursts cut ~1k+ char
    // completions but consistently let sub-1k ones through, and 2 files x
    // ~4 annotated exports land around 700-900 chars.
    let pair = options.files_per_fragment.max(1);
    // `contract`: the Types-layer fragment sees every planned file, and the
    // export limit is the profile's.
    let quality = &profile.staging.quality;
    let contract = options.contract && !quality.contract_rule.is_empty();
    // `semantics`: the Types-layer fragment sees every planned file too, to
    // name the owner of each status change.
    let semantics = options.semantics && !quality.semantics_rule.is_empty();
    // `types_from_paths`: the paths alone, which is what the types fragment
    // has when it starts before the skeleton's purposes are written. (Set B,
    // 4 alternating pairs: the planned types, the contract problems left and
    // the first check were the same with and without the purposes.)
    let paths_only = options.types_from_paths;
    let every_file = skeleton
        .files
        .iter()
        .map(|file| {
            if paths_only {
                format!("- `{}`", file.path)
            } else {
                format!("- `{}`: {}", file.path, file.purpose)
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    let cap = if contract && !quality.contract_cap.is_empty() {
        quality.contract_cap
    } else if semantics && !quality.semantics_cap.is_empty() {
        quality.semantics_cap
    } else {
        "At most 4 exports per file, each ONE line"
    };
    // Fragments are independent (each sees only its own files), so they run
    // concurrently: planning depth is skeleton + ONE fragment round instead
    // of one round per file pair. `types_first`: the Types-layer files (each
    // alone) and the files with no layer form round one; the rest wait for
    // the planned types (`types_block`, right after the objective so every
    // round-two prompt shares one prefix).
    let is_types = |file: &ManifestFile| profile.staging.layer_of(&file.path) == Some(Layer::Types);
    // `layered`: round two is the Core- and Data-layer files, round three
    // the rest; round two waits for the types, round three for the Core
    // fragments of round two (the Data ones run beside it).
    let is_core = |file: &ManifestFile| {
        matches!(
            profile.staging.layer_of(&file.path),
            Some(Layer::Core | Layer::Data)
        )
    };
    let layered = options.layered && skeleton.files.iter().any(is_core);
    let types_first =
        (options.types_first || options.layered) && skeleton.files.iter().any(is_types);
    // `core_whole`: the Core-layer files of round two in the fewest
    // fragments of at most `CORE_WHOLE_MAX` files.
    let core_whole = layered && options.core_whole;
    // `types_chunks`: how many leading chunks of round one are Types-layer
    // ones (round two waits only for those; the layerless rest runs beside).
    let mut types_chunks = 0;
    // `layered`: how many leading chunks of round two are Core-layer ones
    // (round three waits only for those; the Data-layer rest runs beside).
    let mut core_chunks = 0;
    let mut round_three: Vec<Vec<ManifestFile>> = Vec::new();
    let (round_one, round_two): (Vec<Vec<ManifestFile>>, Vec<Vec<ManifestFile>>) = if layered {
        let mut first: Vec<Vec<ManifestFile>> = skeleton
            .files
            .iter()
            .filter(|file| is_types(file))
            .map(|file| vec![file.clone()])
            .collect();
        types_chunks = first.len();
        let layerless: Vec<ManifestFile> = skeleton
            .files
            .iter()
            .filter(|file| profile.staging.layer_of(&file.path).is_none())
            .cloned()
            .collect();
        first.extend(layerless.chunks(pair).map(<[ManifestFile]>::to_vec));
        let of_layer = |layer: Layer| -> Vec<ManifestFile> {
            skeleton
                .files
                .iter()
                .filter(|file| profile.staging.layer_of(&file.path) == Some(layer))
                .cloned()
                .collect()
        };
        let core = of_layer(Layer::Core);
        let mut second: Vec<Vec<ManifestFile>> = if core_whole {
            let size = core
                .len()
                .div_ceil(core.len().div_ceil(CORE_WHOLE_MAX).max(1));
            core.chunks(size.max(1))
                .map(<[ManifestFile]>::to_vec)
                .collect()
        } else {
            core.chunks(pair).map(<[ManifestFile]>::to_vec).collect()
        };
        core_chunks = second.len();
        second.extend(
            of_layer(Layer::Data)
                .chunks(pair)
                .map(<[ManifestFile]>::to_vec),
        );
        let rest: Vec<ManifestFile> = skeleton
            .files
            .iter()
            .filter(|file| {
                !is_types(file) && !is_core(file) && profile.staging.layer_of(&file.path).is_some()
            })
            .cloned()
            .collect();
        round_three = rest.chunks(pair).map(<[ManifestFile]>::to_vec).collect();
        (first, second)
    } else if types_first {
        let mut first: Vec<Vec<ManifestFile>> = skeleton
            .files
            .iter()
            .filter(|file| is_types(file))
            .map(|file| vec![file.clone()])
            .collect();
        types_chunks = first.len();
        let layerless: Vec<ManifestFile> = skeleton
            .files
            .iter()
            .filter(|file| profile.staging.layer_of(&file.path).is_none())
            .cloned()
            .collect();
        first.extend(layerless.chunks(pair).map(<[ManifestFile]>::to_vec));
        let rest: Vec<ManifestFile> = skeleton
            .files
            .iter()
            .filter(|file| !is_types(file) && profile.staging.layer_of(&file.path).is_some())
            .cloned()
            .collect();
        (
            first,
            rest.chunks(pair).map(<[ManifestFile]>::to_vec).collect(),
        )
    } else {
        (
            skeleton
                .files
                .chunks(pair)
                .map(<[ManifestFile]>::to_vec)
                .collect(),
            Vec::new(),
        )
    };
    FragmentLayout {
        round_one,
        round_two,
        round_three,
        types_chunks,
        core_chunks,
        layered,
        types_first,
        core_whole,
        contract,
        semantics,
        every_file,
        cap,
    }
}

/// One export fragment's prompt for `chunk` of `layout` (`split_manifest`).
/// `types_from_paths`: the purpose a Types-layer file has in its own
/// fragment's request.
const TYPES_PURPOSE: &str = "The shared types and constants of the app.";

fn exports_fragment(
    layout: &FragmentLayout<'_>,
    objective: &str,
    options: &ArchitectOptions,
    profile: &Profile,
    chunk: &[ManifestFile],
    types_note: &str,
    referenced: bool,
) -> ExportFragment {
    let quality = &profile.staging.quality;
    let FragmentLayout {
        types_first,
        core_whole,
        contract,
        semantics,
        ref every_file,
        cap,
        ..
    } = *layout;
    let mut listing = chunk
        .iter()
        .map(|file| {
            // `types_from_paths`: a Types-layer file under a fixed purpose,
            // so the request is the same before and after the skeleton.
            let fixed = options.types_from_paths
                && profile.staging.layer_of(&file.path) == Some(Layer::Types);
            format!(
                "- `{}`: {}",
                file.path,
                if fixed { TYPES_PURPOSE } else { &file.purpose }
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    if referenced {
        listing.push_str(&format!(
            "\n(Reference only: plan the exports of {} and nothing else; the other files in \
                 the reference list are planned by other passes.)",
            chunk
                .iter()
                .map(|file| format!("`{}`", file.path))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    let has_types = chunk
        .iter()
        .any(|file| profile.staging.layer_of(&file.path) == Some(Layer::Types));
    let types_chunk = (contract || semantics) && has_types;
    // `types_first`: the Types-layer file is planned alone, and without
    // this line the model read the reference list as files to plan (all
    // of them, past the output cap: 2/2 Fleet and Gradebook runs failed).
    let reference_only = if types_first {
        format!(
            "\n(Reference only: plan the exports of {} and nothing else; \
                 the files above are planned by later passes.)",
            chunk
                .iter()
                .map(|file| format!("`{}`", file.path))
                .collect::<Vec<_>>()
                .join(", ")
        )
    } else {
        String::new()
    };
    let contract_note = match (types_chunk, contract) {
        (true, true) => format!(
            "{}{every_file}{reference_only}",
            quality.contract_types_note
        ),
        (true, false) => format!(
            "{}{every_file}{reference_only}",
            quality.semantics_types_note
        ),
        (false, _) => String::new(),
    };
    let contract_chars = match (contract, types_chunk) {
        (false, false) => 0,
        (true, false) => 400,
        // 14 commented one-line exports (~150 chars each) plus the
        // other file of the pair and the JSON frame.
        (true, true) => 3_000,
        // `semantics` alone: up to 12 commented exports.
        (false, true) => 2_400,
    };
    // `semantics`: the conventions make the types' comments longer.
    let contract_chars = contract_chars + if semantics && has_types { 800 } else { 0 };
    let pair_limit = 1000
        + if options.edges { 600 } else { 0 }
        + if options.examples { 600 } else { 0 }
        + contract_chars;
    // `core_whole`: a fragment of Core-layer files planned together gets
    // the pair's limit and budget scaled by its file count.
    let whole = core_whole
        && chunk
            .iter()
            .all(|file| profile.staging.layer_of(&file.path) == Some(Layer::Core));
    let (limit, budget) = if whole && chunk.len() > 2 {
        (pair_limit * chunk.len() / 2, fragment_budget(chunk.len()))
    } else {
        (pair_limit, FRAGMENT_BUDGET)
    };
    let exports_user = format!(
        r#"Design the interface contracts for this objective.

<objective>
{objective}
</objective>

{types_note}These files exist (path + purpose). Fill in the exact exports for EVERY
file listed — keep each file's given path and purpose verbatim:

{listing}{contract_note}

Fragment discipline (a later pass handles the other files — do NOT list
them, do NOT add files, do NOT add prose):
- ONLY the {count} file(s) listed above, nothing else.
- {cap}{css_rule}.{edge_rule}{example_rule}
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
    );
    let expected: std::collections::HashSet<String> =
        chunk.iter().map(|file| file.path.clone()).collect();
    ExportFragment {
        user: exports_user,
        expected,
        budget,
    }
}

/// One concurrent round of export fragments (`(prompt, paths)`), labelled
/// `exports-{first + i}`; the files of every answer, in fragment order. A
/// fragment that `early` already sent (`plan_pipe`: the same request, from
/// the streaming skeleton) is awaited instead of sent again.
async fn run_fragments(
    client: &dyn ModelClient,
    shared: &str,
    options: &ArchitectOptions,
    fragments: &[ExportFragment],
    first: usize,
    early: &EarlyFragments<'_>,
) -> Result<Vec<ManifestFile>> {
    let requests = fragments
        .iter()
        .enumerate()
        .map(|(index, fragment)| async move {
            if let Some(started) = early.take(fragment) {
                return started.await;
            }
            let text = complete_fragment_sized(
                client,
                &format!("exports-{}", first + index),
                &fragment.user,
                shared,
                options,
                fragment.budget,
                |manifest| fragment_paths(manifest) == fragment.expected,
            )
            .await?;
            manifest_from_json(&text)
        });
    let mut files = Vec::new();
    for fragment in futures::future::try_join_all(requests).await? {
        files.extend(fragment.files);
    }
    Ok(files)
}

/// One export fragment: its prompt, the paths it must answer, and its
/// first-shot output budget (the repair round asks it again with the same).
#[derive(Clone)]
struct ExportFragment {
    user: String,
    expected: std::collections::HashSet<String>,
    budget: u32,
}

impl ExportFragment {
    /// True when `other` sends the same request: the same prompt, budget and
    /// files (the system prompt, shared context and options are the split's).
    fn same_request(&self, other: &ExportFragment) -> bool {
        self.user == other.user && self.budget == other.budget && self.expected == other.expected
    }
}

/// The most export fragments one streaming skeleton may start early
/// (`plan_pipe`): a retried or garbled stream cannot fan out without limit.
const EARLY_FRAGMENTS_MAX: usize = 48;

/// An export fragment's call started from the streaming skeleton.
type EarlyCall<'a> = futures::future::BoxFuture<'a, Result<Manifest>>;

enum EarlyState<'a> {
    Running(EarlyCall<'a>),
    Done(Result<Manifest>),
}

/// One early fragment: the request it sent and its call.
struct EarlySlot<'a> {
    fragment: ExportFragment,
    state: EarlyState<'a>,
}

impl EarlySlot<'_> {
    /// Drive the call while the skeleton streams; its result waits here.
    fn poll(&mut self, cx: &mut std::task::Context<'_>) {
        if let EarlyState::Running(call) = &mut self.state {
            if let std::task::Poll::Ready(result) = std::future::Future::poll(call.as_mut(), cx) {
                self.state = EarlyState::Done(result);
            }
        }
    }
}

/// `ArchitectOptions::plan_pipe`: the export fragments started from the
/// streaming skeleton. The first round takes each one whose request it
/// would send (`take`); the rest are dropped once the skeleton is final
/// (`keep_matching`), which closes their streams.
struct EarlyFragments<'a> {
    started: std::time::Instant,
    slots: std::sync::Mutex<Vec<EarlySlot<'a>>>,
    first_launch_ms: std::sync::Mutex<Option<u64>>,
}

impl<'a> EarlyFragments<'a> {
    fn new(started: std::time::Instant) -> Self {
        Self {
            started,
            slots: std::sync::Mutex::new(Vec::new()),
            first_launch_ms: std::sync::Mutex::new(None),
        }
    }

    fn slots(&self) -> std::sync::MutexGuard<'_, Vec<EarlySlot<'a>>> {
        self.slots
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Note that an early fragment is being sent now (the timeline's
    /// `first_fragment_start_ms`).
    fn note_launch(&self) {
        let mut first = self
            .first_launch_ms
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        first.get_or_insert(self.started.elapsed().as_millis() as u64);
    }

    /// When the first early fragment was sent, from the split's start.
    fn first_launch_ms(&self) -> Option<u64> {
        *self
            .first_launch_ms
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn extend(&self, slots: Vec<EarlySlot<'a>>) {
        self.slots().extend(slots);
    }

    /// Keep the early fragments whose request is one of `fragments` (the
    /// first round of the final skeleton) and drop the others; returns
    /// (sent, kept).
    fn keep_matching(&self, fragments: &[ExportFragment]) -> (usize, usize) {
        let mut slots = self.slots();
        let launched = slots.len();
        slots.retain(|slot| {
            fragments
                .iter()
                .any(|fragment| slot.fragment.same_request(fragment))
        });
        let kept = slots.len();
        if launched > 0 {
            tracing::info!(
                launched,
                kept,
                dropped = launched - kept,
                "plan pipe: early export fragments checked against the final skeleton"
            );
        }
        (launched, kept)
    }

    /// The early call that sent `fragment`'s request, if any.
    fn take(&self, fragment: &ExportFragment) -> Option<EarlyCall<'a>> {
        let mut slots = self.slots();
        let index = slots
            .iter()
            .position(|slot| slot.fragment.same_request(fragment))?;
        let slot = slots.remove(index);
        Some(match slot.state {
            EarlyState::Running(call) => call,
            EarlyState::Done(result) => Box::pin(async move { result }),
        })
    }
}

/// `plan_pipe`: one export fragment's call, sent from the streaming
/// skeleton; the same call `run_fragments` makes.
fn early_call<'a>(
    client: &'a dyn ModelClient,
    shared: &'a str,
    options: &'a ArchitectOptions,
    index: usize,
    fragment: ExportFragment,
) -> EarlyCall<'a> {
    Box::pin(async move {
        let text = complete_fragment_sized(
            client,
            &format!("exports-{index}"),
            &fragment.user,
            shared,
            options,
            fragment.budget,
            |manifest| fragment_paths(manifest) == fragment.expected,
        )
        .await?;
        manifest_from_json(&text)
    })
}

/// `plan_pipe`: the skeleton call of `split_manifest`, read while it
/// streams. Each time its text grows, the first-round export fragments that
/// are settled on the files listed so far (`settled_fragments`) are sent
/// and driven beside the call; they land in `early`. The call itself and
/// its answer are unchanged.
async fn streamed_skeleton<'a>(
    client: &'a dyn ModelClient,
    objective: &'a str,
    user: &str,
    shared: &'a str,
    options: &'a ArchitectOptions,
    profile: &'a Profile,
    early: &EarlyFragments<'a>,
) -> Result<String> {
    let (sender, mut texts) = futures::channel::mpsc::unbounded::<String>();
    let hook: crate::client::TextHook = std::sync::Arc::new(move |text: &str| {
        let _ = sender.unbounded_send(text.to_string());
    });
    let call = crate::client::with_text_hook(
        hook,
        complete_fragment(client, "skeleton", user, shared, options, |manifest| {
            !manifest.files.is_empty()
        }),
    );
    let mut call = std::pin::pin!(call);
    let mut launched = std::collections::HashSet::<String>::new();
    let mut sent = 0usize;
    let mut unparsed = 0usize;
    let mut slots: Vec<EarlySlot<'a>> = Vec::new();
    let mut previous: Option<String> = None;
    let text = futures::future::poll_fn(|cx| {
        // Each text is the whole answer so far (a retry starts over), so
        // only the latest one counts.
        let mut latest = None;
        let mut restarted = false;
        while let std::task::Poll::Ready(Some(text)) = texts.poll_next_unpin(cx) {
            // A text that does not extend the one before is a new attempt
            // (the skeleton call was retried).
            let last = latest.as_ref().or(previous.as_ref());
            if last.is_some_and(|last: &String| !text.starts_with(last.as_str())) {
                restarted = true;
            }
            latest = Some(text);
        }
        if let Some(text) = latest {
            let parsed = streamed_files(&text);
            if restarted {
                // Close the streams of every running fragment the new
                // attempt has not settled on; finished ones stay for
                // `keep_matching`.
                let settled: Vec<ExportFragment> = parsed
                    .clone()
                    .map(|(files, closed)| {
                        settled_fragments(files, closed, objective, options, profile)
                            .into_iter()
                            .map(|(_, fragment)| fragment)
                            .collect()
                    })
                    .unwrap_or_default();
                let before = slots.len();
                slots.retain(|slot| {
                    matches!(slot.state, EarlyState::Done(_))
                        || settled
                            .iter()
                            .any(|fragment| slot.fragment.same_request(fragment))
                });
                launched = slots
                    .iter()
                    .map(|slot| slot.fragment.user.clone())
                    .collect();
                tracing::info!(
                    dropped = before - slots.len(),
                    kept = slots.len(),
                    after_ms = early.started.elapsed().as_millis() as u64,
                    "plan pipe: skeleton call restarted; dropped the stale early fragments"
                );
            }
            // `types_from_paths`: the Types-layer fragments, once the path
            // list has streamed.
            if options.types_from_paths {
                for (index, fragment) in streamed_paths(&text)
                    .map(|paths| types_fragments_from_paths(paths, objective, options, profile))
                    .unwrap_or_default()
                {
                    if sent >= EARLY_FRAGMENTS_MAX || !launched.insert(fragment.user.clone()) {
                        continue;
                    }
                    sent += 1;
                    early.note_launch();
                    tracing::info!(
                        fragment = index,
                        after_ms = early.started.elapsed().as_millis() as u64,
                        "plan pipe: types fragment sent from the skeleton's path list"
                    );
                    let call = early_call(client, shared, options, index, fragment.clone());
                    slots.push(EarlySlot {
                        fragment,
                        state: EarlyState::Running(call),
                    });
                }
            }
            match parsed {
                Some((files, closed)) => {
                    for (index, fragment) in
                        settled_fragments(files, closed, objective, options, profile)
                    {
                        if sent >= EARLY_FRAGMENTS_MAX || !launched.insert(fragment.user.clone()) {
                            continue;
                        }
                        sent += 1;
                        early.note_launch();
                        tracing::info!(
                            fragment = index,
                            closed,
                            after_ms = early.started.elapsed().as_millis() as u64,
                            "plan pipe: export fragment sent from the streaming skeleton"
                        );
                        let call = early_call(client, shared, options, index, fragment.clone());
                        slots.push(EarlySlot {
                            fragment,
                            state: EarlyState::Running(call),
                        });
                    }
                }
                // Not a skeleton shape (yet): nothing starts early from this
                // text; the fragments wait for the whole skeleton as before.
                None => unparsed += 1,
            }
            previous = Some(text);
        }
        for slot in &mut slots {
            slot.poll(cx);
        }
        std::future::Future::poll(call.as_mut(), cx)
    })
    .await;
    if unparsed > 0 {
        tracing::warn!(
            unparsed,
            "plan pipe: streaming skeleton text did not parse; those fragments wait for the whole skeleton"
        );
    }
    early.extend(slots);
    text
}

/// `plan_pipe`: the first-round export fragments (index, prompt) that are
/// settled on the skeleton files listed so far, `closed` once the `files`
/// array is complete. Each prompt is built exactly as `split_manifest`
/// builds it for that file list. Before the list closes, a fragment is
/// settled when later files cannot change it: a full chunk (or, with
/// `types_first`, a Types-layer file alone) whose prompt does not list
/// every planned file; with `types_first` or `layered` asked for, nothing
/// before a Types-layer file is listed (the round's shape is unknown).
/// Whatever starts here is kept only when the final skeleton sends the same
/// request (`EarlyFragments::keep_matching`).
fn settled_fragments(
    files: Vec<ManifestFile>,
    closed: bool,
    objective: &str,
    options: &ArchitectOptions,
    profile: &Profile,
) -> Vec<(usize, ExportFragment)> {
    if files.is_empty() {
        return Vec::new();
    }
    let mut partial = Manifest {
        files,
        test_files: Vec::new(),
        readme: false,
    };
    // As `split_manifest` does before it normalizes the skeleton.
    stray_tests_out(&mut partial, profile);
    (profile.normalize_skeleton)(&mut partial);
    let layout = fragment_layout(&partial, options, profile);
    if !closed && (options.types_first || options.layered) && !layout.types_first {
        return Vec::new();
    }
    let pair = options.files_per_fragment.max(1);
    layout
        .round_one
        .iter()
        .enumerate()
        .filter(|(index, chunk)| {
            closed
                || (!layout.lists_every_file(chunk, profile)
                    && (chunk.len() == pair
                        || (layout.types_first && *index < layout.types_chunks)))
        })
        .map(|(index, chunk)| {
            (
                index,
                exports_fragment(&layout, objective, options, profile, chunk, "", false),
            )
        })
        .collect()
}

/// `types_from_paths`: the `paths` array of a streaming skeleton, once it
/// is closed (and not empty).
fn streamed_paths(text: &str) -> Option<Vec<String>> {
    const KEY: &str = "\"paths\"";
    let key = text.find(KEY)?;
    let rest = text[key + KEY.len()..]
        .trim_start()
        .strip_prefix(':')?
        .trim_start();
    if !rest.starts_with('[') {
        return None;
    }
    let end = json_value_end(rest.as_bytes(), 0)?;
    serde_json::from_str::<Vec<String>>(&rest[..end])
        .ok()
        .filter(|paths| !paths.is_empty())
}

/// `types_from_paths`: the Types-layer fragments (index, prompt) of a
/// skeleton with these `paths`, built as `split_manifest` builds them: their
/// requests name no purpose of the skeleton.
fn types_fragments_from_paths(
    paths: Vec<String>,
    objective: &str,
    options: &ArchitectOptions,
    profile: &Profile,
) -> Vec<(usize, ExportFragment)> {
    let mut partial = Manifest {
        files: paths
            .into_iter()
            .map(|path| ManifestFile {
                path,
                purpose: String::new(),
                exports: Vec::new(),
                uses: Vec::new(),
            })
            .collect(),
        test_files: Vec::new(),
        readme: false,
    };
    // As `split_manifest` does before it normalizes the skeleton.
    stray_tests_out(&mut partial, profile);
    (profile.normalize_skeleton)(&mut partial);
    let layout = fragment_layout(&partial, options, profile);
    if !layout.types_first {
        return Vec::new();
    }
    layout
        .round_one
        .iter()
        .take(layout.types_chunks)
        .enumerate()
        .map(|(index, chunk)| {
            (
                index,
                exports_fragment(&layout, objective, options, profile, chunk, "", false),
            )
        })
        .collect()
}

/// The complete entries of a streaming skeleton's `files` array, and
/// whether the array is closed. `Some((vec![], false))` before the array
/// starts; `None` when the text is not that shape.
fn streamed_files(text: &str) -> Option<(Vec<ManifestFile>, bool)> {
    const KEY: &str = "\"files\"";
    let Some(key) = text.find(KEY) else {
        return Some((Vec::new(), false));
    };
    let bytes = text.as_bytes();
    let skip = |at: &mut usize, commas: bool| {
        while *at < bytes.len()
            && (bytes[*at].is_ascii_whitespace() || (commas && bytes[*at] == b','))
        {
            *at += 1;
        }
    };
    let mut at = key + KEY.len();
    for expected in *b":[" {
        skip(&mut at, false);
        match bytes.get(at) {
            None => return Some((Vec::new(), false)),
            Some(&byte) if byte == expected => at += 1,
            Some(_) => return None,
        }
    }
    let mut files = Vec::new();
    loop {
        skip(&mut at, true);
        match bytes.get(at) {
            None => return Some((files, false)),
            Some(b']') => return Some((files, true)),
            Some(b'{' | b'"') => {
                let Some(end) = json_value_end(bytes, at) else {
                    return Some((files, false));
                };
                files.push(serde_json::from_str::<ManifestFile>(&text[at..end]).ok()?);
                at = end;
            }
            Some(_) => return None,
        }
    }
}

/// The end (exclusive) of the JSON object or string that starts at `start`;
/// `None` while it is incomplete.
fn json_value_end(bytes: &[u8], start: usize) -> Option<usize> {
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    for (offset, &byte) in bytes[start..].iter().enumerate() {
        if in_string {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                in_string = false;
                if depth == 0 {
                    return Some(start + offset + 1);
                }
            }
            continue;
        }
        match byte {
            b'"' => in_string = true,
            b'{' | b'[' => depth += 1,
            b'}' | b']' => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    return Some(start + offset + 1);
                }
            }
            _ => {}
        }
    }
    None
}

/// An export past this many characters keeps only the first clause of its
/// comment in `core_block` (at most `CORE_CLAUSE_CHARS`).
const CORE_EXPORT_CHARS: usize = 180;

/// The most characters of a comment's first clause `core_block` keeps.
const CORE_CLAUSE_CHARS: usize = 80;

/// The most characters of a comment's first clause `core_block` keeps once
/// the block is past `CORE_BLOCK_CHARS`.
const CORE_CLAUSE_CHARS_TIGHT: usize = 48;

/// The hard cap of `core_block`'s export lines: past it the shortened
/// comments of every export, then the remaining files by path only.
const CORE_BLOCK_CHARS: usize = 6_000;

/// A reference list's purpose is cut to this many characters.
const REFERENCE_PURPOSE_CHARS: usize = 100;

/// Past this many characters a reference list names the remaining files by
/// path only.
const REFERENCE_CHARS: usize = 3_000;

/// The header of the round-two reference list (`ArchitectOptions::layered`).
const CORE_REFERENCE: &str =
    "Every core and data file planned in this pass (reference list: path and the job it owns):";

/// The byte offset of the trailing `comment` marker of `export`: the first
/// one after whitespace and outside string quotes, so a literal such as
/// `"https://x"` or `"#fff"` is not taken for a comment. `None` when there is
/// no comment.
fn comment_start(export: &str, comment: &str) -> Option<usize> {
    if comment.is_empty() {
        return None;
    }
    let mut quote: Option<char> = None;
    let mut escaped = false;
    let mut prev = None;
    for (at, ch) in export.char_indices() {
        if let Some(open) = quote {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == open {
                quote = None;
            }
        } else if matches!(ch, '"' | '\'' | '`') {
            quote = Some(ch);
        } else if prev.is_some_and(char::is_whitespace) && export[at..].starts_with(comment) {
            return Some(at);
        }
        prev = Some(ch);
    }
    None
}

/// `text` cut to at most `max` characters, with `…` when cut.
fn cut_chars(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let kept: String = text.chars().take(max).collect();
    format!("{}…", kept.trim_end())
}

/// `export` with its comment cut to the first clause (up to the first `; `
/// or `. `), at most `max` characters: the convention a comment opens with
/// (week start, sign, unit, default) survives, the explanation after it
/// does not.
fn with_short_comment(export: &str, comment: &str, max: usize) -> String {
    let Some(at) = comment_start(export, comment) else {
        return export.to_string();
    };
    let signature = export[..at].trim_end();
    let text = export[at + comment.len()..].trim();
    let clause = [";", ". "]
        .iter()
        .filter_map(|mark| text.find(mark))
        .min()
        .map_or(text, |end| &text[..end])
        .trim();
    if clause.is_empty() {
        signature.to_string()
    } else {
        format!("{signature}  {comment} {}", cut_chars(clause, max))
    }
}

/// A reference list: `header`, then `* path: purpose` for every file (each
/// purpose cut to `REFERENCE_PURPOSE_CHARS`; past `REFERENCE_CHARS` the
/// remaining files by path only), then `tail`.
fn reference_list(files: &[&ManifestFile], header: &str, tail: &str) -> String {
    let mut listing = String::new();
    let mut rest: Vec<String> = Vec::new();
    for file in files {
        let line = format!(
            "* `{}`: {}\n",
            file.path,
            cut_chars(&file.purpose, REFERENCE_PURPOSE_CHARS)
        );
        if rest.is_empty() && listing.len() + line.len() <= REFERENCE_CHARS {
            listing.push_str(&line);
        } else {
            rest.push(format!("`{}`", file.path));
        }
    }
    if !rest.is_empty() {
        listing.push_str(&format!("* also: {}\n", rest.join(", ")));
    }
    format!("{header}\n{listing}{tail}\n\n")
}

/// `ArchitectOptions::layered`: the block that round-three export fragments
/// get after the types block, the planned Core-layer exports by path (and
/// the Data-layer ones planned by then). An export longer than
/// `CORE_EXPORT_CHARS` keeps the first clause of its comment; past
/// `CORE_BLOCK_CHARS` every export keeps a shorter one, and the files past
/// the cap are named by path only. Empty when `files` plans no such
/// exports.
fn core_block(files: &[ManifestFile], staging: &crate::profile::Staging, comment: &str) -> String {
    let core: Vec<&ManifestFile> = files
        .iter()
        .filter(|file| {
            matches!(
                staging.layer_of(&file.path),
                Some(Layer::Core | Layer::Data)
            )
        })
        .filter(|file| !file.exports.is_empty())
        .collect();
    if core.is_empty() {
        return String::new();
    }
    // One rendered entry per file: exports longer than `limit` keep the
    // first `clause` characters of their comment's first clause.
    let render = |limit: usize, clause: usize| -> Vec<String> {
        core.iter()
            .map(|file| {
                let lines = file
                    .exports
                    .iter()
                    .map(|export| {
                        if export.chars().count() > limit {
                            format!("- {}", with_short_comment(export, comment, clause))
                        } else {
                            format!("- {export}")
                        }
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                format!("`{}`:\n{lines}", file.path)
            })
            .collect()
    };
    let size = |entries: &[String]| entries.iter().map(|entry| entry.len() + 1).sum::<usize>();
    let mut entries = render(CORE_EXPORT_CHARS, CORE_CLAUSE_CHARS);
    if size(&entries) > CORE_BLOCK_CHARS {
        entries = render(0, CORE_CLAUSE_CHARS_TIGHT);
    }
    // Hard cap: whole files while they fit, then the rest by path.
    let mut kept = Vec::new();
    let mut used = 0;
    let mut paths_only = Vec::new();
    for (file, entry) in core.iter().zip(entries) {
        if paths_only.is_empty() && used + entry.len() < CORE_BLOCK_CHARS {
            used += entry.len() + 1;
            kept.push(entry);
        } else {
            paths_only.push(format!("`{}`", file.path));
        }
    }
    let mut lines = kept.join("\n");
    if !paths_only.is_empty() {
        lines.push_str(&format!(
            "\n({} more core files, paths only: {})",
            paths_only.len(),
            paths_only.join(", ")
        ));
    }
    format!(
        "The core modules are already planned (path, then its exports). Use these functions as \
         planned: import them, call them with the same names and arguments, and pass every \
         setting or value they take; never re-implement their logic:\n{lines}\n\n"
    )
}

/// `ArchitectOptions::layered`: the list of every round-three file (path:
/// purpose) each round-three fragment gets, so a file knows which other
/// file owns each job (bounded as `reference_list`).
fn consumer_reference(round: &[Vec<ManifestFile>]) -> String {
    let files: Vec<&ManifestFile> = round.iter().flatten().collect();
    reference_list(
        &files,
        "Every file planned in this pass (reference list: path and the job it owns):",
        "Use the core functions above as planned (same names and arguments, pass the settings \
         they need). Do not re-plan a job that a file in the reference list owns (no second \
         control, input or calculation for it): render or call that file instead.",
    )
}

/// `ArchitectOptions::types_first`: the block that round-two export
/// fragments get right after the objective, the planned Types-layer
/// exports to use verbatim; empty when `files` plans no types.
fn types_block(files: &[ManifestFile], staging: &crate::profile::Staging) -> String {
    let types: Vec<&ManifestFile> = files
        .iter()
        .filter(|file| staging.layer_of(&file.path) == Some(Layer::Types))
        .filter(|file| !file.exports.is_empty())
        .collect();
    if types.is_empty() {
        return String::new();
    }
    let paths = types
        .iter()
        .map(|file| format!("`{}`", file.path))
        .collect::<Vec<_>>()
        .join(", ");
    let lines = types
        .iter()
        .flat_map(|file| file.exports.iter())
        .map(|export| format!("- {export}"))
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "The shared types are already planned ({paths} exports). Use these names, value sets \
         and record shapes exactly and import them, never redeclare them; do not narrow a union \
         or re-model a record (refer to a record by its id field as the types do):\n{lines}\n\n"
    )
}

/// The kinds of problem the plan review asks for (`review_prompt`).
const REVIEW_KINDS: &[&str] = &[
    "missing_param",
    "conflicting_type",
    "two_models",
    "unowned_effect",
    "missing_export",
];

/// The review answer's wait bound: past it the plan goes on unreviewed.
const REVIEW_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);

/// The review answer's token cap: 6 problems of at most 140 characters plus
/// the JSON frame fit well inside it, and at ~25-40 tok/s it ends inside
/// `REVIEW_TIMEOUT`.
const REVIEW_MAX_TOKENS: u32 = 700;

/// The most review problems kept, and the longest fix text kept (a little
/// over the 140 asked for, so a slightly long fix is cut, not lost).
const REVIEW_MAX_PROBLEMS: usize = 6;
const REVIEW_MAX_CHARS: usize = 160;

/// The review kinds that name a function (its owner is the file that
/// exports it), as opposed to a type or record.
const REVIEW_FUNCTION_KINDS: &[&str] = &["missing_param", "unowned_effect", "missing_export"];

/// The whole-plan review question over the merged contracts (see
/// `review_manifest`).
pub fn review_prompt(objective: &str, files: &[ManifestFile]) -> String {
    let contracts = files
        .iter()
        .map(|file| {
            format!(
                "- `{}`: {} | {}",
                file.path,
                file.purpose,
                file.exports.join(" | ")
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        r#"Review this plan for problems BETWEEN its files.

<objective>
{objective}
</objective>

Contracts (path: purpose | exports):
{contracts}

Report only problems of these kinds:
- missing_param: a function must respect a rule stated elsewhere (a schedule, a price list, a setting) but none of its parameters carries it;
- conflicting_type: one value is typed differently in two files (a union narrowed, another unit or format);
- two_models: one record is referred to by name in one file and by id in another;
- unowned_effect: removing or changing a record of one collection should change another collection, and no export does it;
- missing_export: a function is called or named in one contract but no planned file exports it.
Rules: `path` must be a file listed above, never a new file. For conflicting_type and two_models it is the file that declares the type or record (the shared types file when it declares it); for missing_param, unowned_effect and missing_export it is the file that exports (or should export) that function. `text` states the fix in at most 140 characters; at most 6 problems, at most 2 per file; an empty list when there are none. No style, naming or new-feature remarks.

Return only this JSON object:
{{"problems": [{{"kind": "<kind>", "path": "<file path>", "text": "<the fix>"}}]}}
"#
    )
}

/// Strict json_schema for the review answer.
fn review_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "json_schema",
        "json_schema": {
            "name": "plan_review",
            "strict": true,
            "schema": {
                "type": "object",
                "properties": {
                    "problems": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "kind": {"type": "string", "enum": REVIEW_KINDS},
                                "path": {"type": "string"},
                                "text": {"type": "string"}
                            },
                            "required": ["kind", "path", "text"],
                            "additionalProperties": false
                        }
                    }
                },
                "required": ["problems"],
                "additionalProperties": false
            }
        }
    })
}

/// `ArchitectOptions::review`: one best-effort call over the merged
/// contracts for cross-file problems (`review_prompt`), parsed and checked by
/// `parse_review`. A failed call, an answer later than `REVIEW_TIMEOUT` or
/// one that does not parse gives no problems.
async fn review_manifest(
    client: &dyn ModelClient,
    objective: &str,
    shared: &str,
    options: &ArchitectOptions,
    files: &[ManifestFile],
) -> Vec<crate::contract::ContractProblem> {
    let user = review_prompt(objective, files);
    let request = |schema: bool| CompletionRequest {
        system: "You are the architect for a greenfield software build. You \
decide the file layout and the exact interface contracts. Return JSON only."
            .to_string(),
        shared_context: Some(shared.to_string()),
        user: user.clone(),
        max_tokens: REVIEW_MAX_TOKENS,
        temperature: 0.1,
        reasoning_effort: Some(options.fragment_effort.clone()),
        prompt_cache_key: options.cache_key.clone(),
        allow_empty: false,
        response_format: schema.then(review_schema),
    };
    let started = std::time::Instant::now();
    let call = async {
        let mut outcome = client.complete(request(options.schema)).await;
        if let Err(error) = &outcome {
            if options.schema && format!("{error:#}").contains("response_format") {
                tracing::warn!("server rejected the review json_schema; asking without it");
                outcome = client.complete(request(false)).await;
            }
        }
        outcome
    };
    let review_ms = || started.elapsed().as_millis() as u64;
    let done = match tokio::time::timeout(REVIEW_TIMEOUT, call).await {
        Ok(Ok(done)) => done,
        Ok(Err(error)) => {
            tracing::warn!(review_ms = review_ms(), "plan review failed: {error:#}");
            return Vec::new();
        }
        Err(_) => {
            tracing::warn!(review_ms = review_ms(), "plan review timed out");
            return Vec::new();
        }
    };
    let problems = parse_review(&done.text, files);
    tracing::info!(
        review_ms = review_ms(),
        output_tokens = done.output_tokens,
        chars = done.text.len(),
        problems = problems.as_ref().map_or(0, Vec::len),
        parsed = problems.is_some(),
        "plan review returned"
    );
    let problems = problems.unwrap_or_default();
    for problem in &problems {
        tracing::info!(path = ?problem.path, problem = %problem.text, "plan review problem");
    }
    problems
}

/// The review answer as problems: `None` when it does not parse. Kept only
/// a known kind with text whose path is a planned file; a function problem
/// goes to the file that exports the function it names (`function_owner`);
/// deduplicated, at most 2 per file and `REVIEW_MAX_PROBLEMS` in all, each
/// text cut to `REVIEW_MAX_CHARS` and led by its kind.
fn parse_review(
    text: &str,
    files: &[ManifestFile],
) -> Option<Vec<crate::contract::ContractProblem>> {
    #[derive(Deserialize)]
    struct Item {
        #[serde(default)]
        kind: String,
        #[serde(default)]
        path: String,
        #[serde(default)]
        text: String,
    }
    #[derive(Deserialize)]
    struct Review {
        problems: Vec<Item>,
    }
    let review: Review = serde_json::from_str(extract_json_object(text).ok()?).ok()?;
    let mut problems: Vec<crate::contract::ContractProblem> = Vec::new();
    for item in review.problems {
        let kind = item.kind.trim().to_ascii_lowercase();
        let path = item.path.trim().trim_matches('`').trim();
        let fix = item.text.split_whitespace().collect::<Vec<_>>().join(" ");
        if !REVIEW_KINDS.contains(&kind.as_str())
            || fix.is_empty()
            || !files.iter().any(|file| file.path == path)
        {
            continue;
        }
        let path = if REVIEW_FUNCTION_KINDS.contains(&kind.as_str()) {
            function_owner(path, &fix, files)
        } else {
            path
        };
        let fix: String = fix.chars().take(REVIEW_MAX_CHARS).collect();
        let problem = crate::contract::ContractProblem {
            path: Some(path.to_string()),
            text: format!("{kind}: {fix}"),
        };
        let per_file = problems
            .iter()
            .filter(|known| known.path == problem.path)
            .count();
        if problems.contains(&problem) || per_file >= 2 {
            continue;
        }
        problems.push(problem);
        if problems.len() == REVIEW_MAX_PROBLEMS {
            break;
        }
    }
    Some(problems)
}

/// The names an export line declares (`declared_names`, plus the name after
/// `def` / `fn`), from its code part only.
fn export_names(line: &str) -> Vec<String> {
    let code = bare_signature(line);
    let mut names = declared_names(&code);
    names.extend(
        identifiers(&code)
            .windows(2)
            .filter(|pair| matches!(pair[0].1, "def" | "fn"))
            .map(|pair| pair[1].1.to_string()),
    );
    names
}

/// The owner of a function problem of the review: `path` when it exports a
/// function the fix names (a lower-case name: functions, not types), else
/// the one other planned file that does; `path` when none or several do.
fn function_owner<'a>(path: &'a str, fix: &str, files: &'a [ManifestFile]) -> &'a str {
    let named: Vec<&str> = identifiers(fix)
        .into_iter()
        .map(|(_, word)| word)
        .filter(|word| word.len() >= 3 && word.starts_with(|ch: char| ch.is_ascii_lowercase()))
        .collect();
    let declares = |file: &ManifestFile| {
        file.exports.iter().any(|line| {
            export_names(line)
                .iter()
                .any(|name| named.contains(&name.as_str()))
        })
    };
    if files.iter().any(|file| file.path == path && declares(file)) {
        return path;
    }
    let mut owners = files
        .iter()
        .filter(|file| file.path != path && declares(file));
    match (owners.next(), owners.next()) {
        (Some(owner), None) => &owner.path,
        _ => path,
    }
}

/// The paths of a fragment's files.
fn fragment_paths(manifest: &Manifest) -> std::collections::HashSet<String> {
    manifest
        .files
        .iter()
        .map(|file| file.path.clone())
        .collect()
}

/// How many exports over the per-file limit a contract repair may add to a
/// file with a problem (a create with its update and remove, and one more).
const REPAIR_EXTRA_EXPORTS: usize = 4;

/// `ArchitectOptions::contract` / `semantics`: check the merged `files`
/// against the app contract (`contract::contract_problems`, with
/// `contract`) and the conventions (`semantics::semantic_problems`, with
/// `semantics`), and ask each export fragment
/// that owns a problem once more, concurrently, with its first prompt and
/// its problems at the end (so the shared prefix stays). A fragment answer
/// that fails replaces nothing. The problems left are logged.
#[allow(clippy::too_many_arguments)]
async fn repair_contract(
    client: &dyn ModelClient,
    shared: &str,
    options: &ArchitectOptions,
    profile: &Profile,
    fragments: &[ExportFragment],
    files: &mut [ManifestFile],
    extra: Vec<crate::contract::ContractProblem>,
    early: &(dyn Fn(&[ManifestFile]) + Sync),
) {
    let check = |files: &[ManifestFile]| {
        let manifest = Manifest {
            files: files.to_vec(),
            test_files: Vec::new(),
            readme: false,
        };
        let mut problems = if options.contract {
            crate::contract::contract_problems(&manifest, profile)
        } else {
            Vec::new()
        };
        if options.semantics {
            problems.extend(crate::semantics::semantic_problems(&manifest, profile));
        }
        if options.references {
            problems.extend(crate::references::dangling_problems(&manifest));
        }
        problems
    };
    if options.contract {
        // A setting that no file reads gets its likeliest reader without a
        // second planner call (see `contract::assign_setting_readers`).
        let mut manifest = Manifest {
            files: files.to_vec(),
            test_files: Vec::new(),
            readme: false,
        };
        let given = crate::contract::assign_setting_readers(&mut manifest, profile);
        if given > 0 {
            tracing::info!(given, "settings given a reader without a model call");
            files.clone_from_slice(&manifest.files);
        }
    }
    // `extra` (the plan review's problems) is asked in the same round but not
    // re-checked: only the deterministic checks gate a repair.
    let mut problems = check(files);
    problems.extend(extra);
    if problems.is_empty() {
        tracing::info!("contract check passed");
        return;
    }
    let mut asks: std::collections::BTreeMap<usize, Vec<String>> = Default::default();
    // The files of each asked fragment that own a problem: only these take
    // the repaired answer (a sibling with no problem keeps its first one).
    let mut owners: std::collections::BTreeMap<usize, std::collections::HashSet<String>> =
        Default::default();
    for problem in &problems {
        let Some(path) = &problem.path else {
            tracing::warn!(problem = %problem.text, "contract problem with no owner file");
            continue;
        };
        if let Some(index) = fragments
            .iter()
            .position(|fragment| fragment.expected.contains(path))
        {
            asks.entry(index)
                .or_default()
                .push(format!("- `{path}`: {}", problem.text));
            owners.entry(index).or_default().insert(path.clone());
        }
    }
    tracing::info!(
        problems = problems.len(),
        fragments = asks.len(),
        "contract check found problems; asking their fragments once more"
    );
    if !asks.is_empty() {
        early(files);
    }
    let requests = asks.iter().map(|(&index, lines)| {
        let ExportFragment {
            user,
            expected,
            budget,
        } = &fragments[index];
        // The first answer's exports of the problem files: without them the
        // model plans the files anew, under other names, and the answer is
        // refused for the names it lost (no repaired lib file was kept in 32
        // bench runs). A full file needs room for what a problem adds.
        use std::fmt::Write as _;
        let mut first = String::new();
        for file in files.iter().filter(|file| {
            owners
                .get(&index)
                .is_some_and(|owned| owned.contains(&file.path))
        }) {
            let _ = writeln!(first, "- `{}`:", file.path);
            for line in &file.exports {
                let _ = writeln!(first, "  {line}");
            }
        }
        let user = format!(
            "{user}\nThe first answer for these files had these problems. Return the same \
             files with each problem fixed (same paths and purposes). Keep every export of the \
             first answer exactly as it is, with the same name and signature, unless a problem \
             names it: other files already call these names. To fix a problem, change the line \
             it names or add exports; a file with a problem may have up to \
             {REPAIR_EXTRA_EXPORTS} more exports than the limit above.\nThe first answer's \
             exports:\n{first}Problems:\n{}\n",
            lines.join("\n")
        );
        async move {
            complete_fragment_sized(
                client,
                &format!("contract-{index}"),
                &user,
                shared,
                options,
                *budget,
                |manifest| fragment_paths(manifest) == *expected,
            )
            .await
        }
    });
    let mut repaired = false;
    for (index, answer) in asks
        .keys()
        .copied()
        .zip(futures::future::join_all(requests).await)
    {
        let Some(fixed) = answer.ok().and_then(|text| manifest_from_json(&text).ok()) else {
            continue;
        };
        // Kept only when it does not make things worse: no problem file
        // loses an export or a declared name (a repair can add and retype,
        // never rename or drop what other contracts and the wired `uses`
        // already call), and the whole check finds no more problems than
        // before.
        let owned = owners.get(&index).cloned().unwrap_or_default();
        let (candidate, lost) = repair_candidate(files, fixed.files, &owned);
        let before = check(files).len();
        let after = check(&candidate).len();
        if !lost.is_empty() || after > before {
            tracing::warn!(
                fragment = index,
                before,
                after,
                lost = ?lost,
                "contract repair made the fragment worse; keeping the first answer"
            );
            continue;
        }
        files.clone_from_slice(&candidate);
        repaired = true;
    }
    if repaired {
        renormalize_uses(files);
    }
    let left = check(files);
    if left.is_empty() {
        tracing::info!("contract check passed after the fragment repair");
    } else {
        for problem in &left {
            tracing::warn!(path = ?problem.path, problem = %problem.text, "contract problem left");
        }
    }
}

/// `files` with the repaired `fixed` files of `owned` paths swapped in (each
/// keeps its wired `uses` unless the answer brings its own), and the
/// `path: name` of every export name an owned file declared before and no
/// longer does (or `path: N -> M exports` when its export count falls).
/// Files of the answer outside `owned` are ignored.
fn repair_candidate(
    files: &[ManifestFile],
    fixed: Vec<ManifestFile>,
    owned: &std::collections::HashSet<String>,
) -> (Vec<ManifestFile>, Vec<String>) {
    let mut candidate = files.to_vec();
    let mut lost = Vec::new();
    for file in fixed {
        if !owned.contains(&file.path) {
            continue;
        }
        let Some(slot) = candidate.iter_mut().find(|slot| slot.path == file.path) else {
            continue;
        };
        let after: std::collections::HashSet<String> = file
            .exports
            .iter()
            .flat_map(|line| export_names(line))
            .collect();
        for name in slot.exports.iter().flat_map(|line| export_names(line)) {
            let entry = format!("{}: {name}", slot.path);
            if !after.contains(&name) && !lost.contains(&entry) {
                lost.push(entry);
            }
        }
        if file.exports.len() < slot.exports.len() {
            lost.push(format!(
                "{}: {} -> {} exports",
                slot.path,
                slot.exports.len(),
                file.exports.len()
            ));
        }
        let uses = std::mem::take(&mut slot.uses);
        *slot = file;
        if slot.uses.is_empty() {
            slot.uses = uses;
        }
    }
    (candidate, lost)
}

/// After a repair round: every wired `uses` entry normalized again against
/// the repaired exports (`normalize_use`); an entry that no longer resolves
/// is dropped, so no worker is told to call an export that is gone.
fn renormalize_uses(files: &mut [ManifestFile]) {
    let provided: std::collections::HashMap<String, Vec<String>> = files
        .iter()
        .map(|file| (file.path.clone(), file.exports.clone()))
        .collect();
    let mut dropped = 0;
    for file in files.iter_mut() {
        let before = file.uses.len();
        let consumer = file.path.clone();
        file.uses = file
            .uses
            .iter()
            .filter_map(|entry| normalize_use(entry, &consumer, &provided))
            .collect();
        dropped += before - file.uses.len();
    }
    if dropped > 0 {
        tracing::info!(dropped, "wired calls dropped after the repair round");
    }
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
    wiring_prompt_with(objective, files, "")
}

/// `wiring_prompt` with `extra` (`PlanOptions::wiring_rules`) before the
/// answer shape; empty `extra` gives `wiring_prompt`'s text.
pub fn wiring_prompt_with(objective: &str, files: &[ManifestFile], extra: &str) -> String {
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
nothing. Keep the JSON compact.{extra}

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

/// The model tasks that `manifest` plans (its files and test files, and the
/// test shards that `min_test_shards` adds) and the most a plan may have.
///
/// The budget sizes the planner's layout; it is not a hard wall. A plan a
/// task or two over it (nextjs-memory-5: 21 vs 20, planning already paid
/// for) runs with a warning; only a runaway layout aborts. The README is
/// rendered (zero model cost) and never counts. Shards the
/// `min_test_shards` floor adds over the manifest's test files count as test
/// files, so a floor and a padded manifest share the budget. The contract
/// asks for one owner file per collection and per control, so its plans run
/// larger (q123-1: 61 against 48, aborted after planning was paid for). It
/// gets half the budget as headroom instead of a quarter.
pub(crate) fn task_count_and_ceiling(
    manifest: &Manifest,
    max_tasks: usize,
    options: &PlanOptions,
) -> (usize, usize) {
    let floor_shards = if manifest.test_files.is_empty() {
        0
    } else {
        options
            .min_test_shards
            .min(MAX_TEST_SHARDS)
            .saturating_sub(manifest.test_files.len())
    };
    let model_tasks = manifest.files.len() + manifest.test_files.len() + floor_shards;
    let ceiling = if options.contract {
        max_tasks + max_tasks.div_ceil(2)
    } else {
        max_tasks + max_tasks.div_ceil(4)
    };
    (model_tasks, ceiling)
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
    let (model_tasks, ceiling) = task_count_and_ceiling(manifest, max_tasks, options);
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
    let quality = &profile.staging.quality;
    // `objective_pages`: the head lists purposes, and route files and
    // components get their own lines in the tail.
    let objective_pages = options.objective_pages && !quality.page_glob.is_empty();
    let purposes = objective_pages.then_some(&profile.staging);
    let all = if objective_pages {
        manifest
            .files
            .iter()
            .map(|file| manifest.module_line(file, purposes))
            .collect::<Vec<_>>()
            .join("\n")
    } else {
        manifest.all_modules()
    };
    // `semantics`: the conventions block, read from the plan.
    let conventions = if options.semantics {
        crate::semantics::conventions(manifest, profile)
    } else {
        String::new()
    };
    // The rules after the contracts, the same for every module worker.
    let head_rules = |head: &mut String| {
        if options.density {
            head.push_str(profile.density_rule);
        }
        if options.quality_rules {
            head.push_str(quality.head_rule);
        }
        if objective_pages {
            head.push_str(quality.purposes_rule);
        }
        head.push_str(&conventions);
    };
    let mut module_head = profile.module_head.replace("{all}", &all);
    head_rules(&mut module_head);
    // `page_local`: pages own their features (the fold ran in the passes).
    let page_local = options.page_local && !quality.local_note.is_empty();
    let page_lines = if objective_pages {
        objective_page_lines(objective, manifest, profile, page_local)
    } else {
        Vec::new()
    };
    let local_lines = if page_local {
        crate::pagelocal::component_lines(manifest, profile)
    } else {
        Vec::new()
    };
    // `semantics`: the components of a layout use the store hook.
    let layout_lines = if options.semantics {
        crate::pagelocal::layout_component_lines(manifest, profile)
    } else {
        Vec::new()
    };
    // `shell_rule`: a page does not draw the layout's frame again.
    let shell_lines = if options.shell_rule {
        crate::pagelocal::page_shell_lines(manifest, profile)
    } else {
        Vec::new()
    };
    // `contract` / `wiring_rules`: the settings fields with their readers.
    let settings = if options.contract || options.wiring_rules {
        crate::contract::settings_fields(manifest, &profile.staging)
    } else {
        Vec::new()
    };
    let wiring_extra = if options.wiring_rules && !quality.wiring_rule.is_empty() {
        crate::contract::wiring_lines(&settings, quality.wiring_rule)
    } else {
        String::new()
    };
    // `wiring_rules`: only staged files wait for the in-wave wiring task.
    let wiring_consumer = |path: &str| {
        (profile.is_consumer)(path)
            && !(options.wiring_rules
                && !profile.staging.layers.is_empty()
                && profile.staging.layer_of(path).is_none())
    };
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
            .map(|f| manifest.module_line(f, purposes))
            .collect::<Vec<_>>()
            .join("\n")
    };
    // One prime per unique worker head so every family's prefix is stored
    // before the wave (unscoped: the single shared head, as before).
    let mut module_primes: Vec<String> = Vec::new();
    // `split_pages`: a part is presentational (it takes no wiring), and the
    // `uses` entries of a page's parts are not wiring either.
    let parts: std::collections::HashSet<&str> = manifest
        .files
        .iter()
        .filter(|file| crate::splitpage::page_of_part(&file.purpose, quality).is_some())
        .map(|file| file.path.as_str())
        .collect();
    let unwired = |file: &ManifestFile| {
        !parts.contains(file.path.as_str())
            && file.uses.iter().all(|entry| {
                entry
                    .split(':')
                    .next()
                    .is_some_and(|path| parts.contains(path))
            })
    };
    let wired_in_wave = options.wave_wiring
        && manifest
            .files
            .iter()
            .any(|file| unwired(file) && wiring_consumer(&file.path));
    if wired_in_wave {
        tasks.push(Task {
            id: WIRING_TASK.to_string(),
            title: "wiring plan".to_string(),
            prompt: wiring_prompt_with(objective, &manifest.files, &wiring_extra),
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
            prefix_split: None,
        });
    }
    for (index, file) in manifest.files.iter().enumerate() {
        let head = if options.scope_contracts {
            let mut head = profile.module_head.replace("{all}", &scoped_modules(file));
            head_rules(&mut head);
            // Cache primes are byte-prefixes of the family prompts; dedup
            // identical heads (files with the same own+called contracts).
            if !module_primes.iter().any(|prime| prime == &head) {
                module_primes.push(head.clone());
            }
            head
        } else {
            module_head.clone()
        };
        // Rendered, not written: a toolchain file, or (with `split`) a file
        // that only joins other files of the manifest.
        let static_output = (profile.scaffold)(&file.path).or_else(|| {
            options
                .split
                .then(|| (profile.staging.aggregate)(manifest, &file.path))
                .flatten()
        });
        let wave_consumer = wired_in_wave
            && unwired(file)
            && wiring_consumer(&file.path)
            && static_output.is_none();
        let reads = if options.contract {
            crate::contract::reader_line(
                &file.path,
                &settings,
                quality.settings_type,
                profile.staging.layer_of(&file.path) == Some(Layer::Core),
            )
        } else {
            String::new()
        };
        let prompt = format!(
            "{head}Now write the COMPLETE contents of `{path}` — ONLY that file.\n\
             - Purpose: {purpose}\n- Exports (implement exactly these): {exports}{uses}{page}{reads}{local}{layout}{shell}",
            page = page_lines.get(index).map_or("", String::as_str),
            local = local_lines.get(index).map_or("", String::as_str),
            layout = layout_lines.get(index).map_or("", String::as_str),
            shell = shell_lines.get(index).map_or("", String::as_str),
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
            // one); wide builds need the headroom. A page that holds its
            // local components writes their code too (a task page with three
            // of them: about 4.2k tokens on the saved runs). With
            // `page_local` that is every route file: a plan that follows the
            // planner rule has its pages' parts inline with nothing to fold.
            // The fold's note alone also raises it (a folded manifest
            // replayed with `page_local` off, as `rerepair` does; the note
            // only comes from the fold).
            max_tokens: if crate::pagelocal::has_local_components(&file.purpose, quality)
                || (page_local && glob_match(quality.page_glob, &file.path))
            {
                8_192
            } else {
                4_096
            },
            temperature: Some(0.2),
            require_closed_fences: true,
            validate_artifact: (profile.validates)(&file.path),
            prefix_split: (options.deps_first && static_output.is_none()).then_some(head.len()),
            static_output,
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
    for shard in test_shards(objective, manifest, profile, options) {
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
            prefix_split: options.deps_first.then_some(test_head.len()),
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
            prefix_split: None,
        });
    }

    let see_parts = (options.consumers_see_css && options.split).then_some(profile.is_consumer);
    let staged = options.staged
        && stage_tasks(
            &mut tasks,
            &profile.staging,
            see_parts,
            options.types_from_plan,
        );
    // `deps_first`: the deps now come before the tail, so the staged rule
    // points up.
    if options.deps_first && !profile.staging.staged_rule.is_empty() {
        for task in &mut tasks {
            let Some(at) = task.prefix_split else {
                continue;
            };
            if task.prompt[at..].contains(DEPS_BELOW) {
                let tail = task.prompt[at..].replace(DEPS_BELOW, DEPS_ABOVE);
                task.prompt.truncate(at);
                task.prompt.push_str(&tail);
            }
        }
    }
    let mut cache_primes = Vec::new();
    // A staged wave's first stage (two or three types files) warms the
    // prefix cache for the stages after it; primes would only add their wait.
    if !staged {
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

/// Order the wave in stages so later files are written against the real
/// text of the files they build on, not only against the plan. Each task
/// with a layer (and no static output, not internal) gets the import rule
/// and its path rule; then Types get no deps, Data and Core depend on the
/// Types, and Consumers (tests too) on the Types and Core. A task that got
/// deps also gets the staged rule, and a Core task the types-win rule.
/// Files in one stage are still written at the same time. Returns whether
/// any task was staged. The NEXTJS test globs match the file name, where the
/// bench's `stage_plan` matched `.test.` anywhere in the path, so a file in
/// a directory such as `app/a.test.x/` stages differently; accepted.
/// With `see_parts` (`PlanOptions::consumers_see_css`), each Consumer task
/// that is not a test and whose path `see_parts` accepts also depends on
/// the tasks that write the parts of a split file (`staging.split`).
/// With `types_from_plan` (`PlanOptions::types_from_plan`), Data and Core do
/// not depend on the Types: they get [`TYPES_FROM_PLAN_RULE`] instead of the
/// staged rule (a Core task the types-win rule after it) and start with the
/// wave.
fn stage_tasks(
    tasks: &mut [Task],
    staging: &Staging,
    see_parts: Option<fn(&str) -> bool>,
    types_from_plan: bool,
) -> bool {
    let staged = |task: &Task| {
        if task.static_output.is_some() || task.internal {
            return None;
        }
        staging.layer_of(&task.title)
    };
    let part_globs: Vec<String> = staging
        .split
        .iter()
        .map(|split| format!("**/{}", split.part_pattern.replace("{n}", "*")))
        .collect();
    let parts: Vec<String> = match see_parts {
        Some(_) => tasks
            .iter()
            .filter(|task| task.static_output.is_none() && !task.internal)
            .filter(|task| part_globs.iter().any(|glob| glob_match(glob, &task.title)))
            .map(|task| task.id.clone())
            .collect(),
        None => Vec::new(),
    };
    let ids_of = |layer: Layer| -> Vec<String> {
        tasks
            .iter()
            .filter(|task| staged(task) == Some(layer))
            .map(|task| task.id.clone())
            .collect()
    };
    let types = ids_of(Layer::Types);
    let core = ids_of(Layer::Core);
    let mut any = false;
    for task in tasks.iter_mut() {
        let Some(layer) = staged(task) else {
            continue;
        };
        any = true;
        task.prompt.push_str(staging.import_rule);
        if let Some(rule) = staging.path_rule(&task.title) {
            task.prompt.push_str(rule);
        }
        if layer == Layer::Types {
            continue;
        }
        // `types_from_plan`: the plan already holds the types file's
        // declarations, so Data and Core start with it, not after it.
        let early = types_from_plan && layer != Layer::Consumer && !types.is_empty();
        let mut wanted = if early { Vec::new() } else { types.clone() };
        // (Measured in the bench: pages that start before the Core code is
        // written save 2 s and then fail the build in 4 of 6 runs.)
        if layer == Layer::Consumer {
            wanted.extend(core.iter().cloned());
            if see_parts.is_some_and(|see| see(&task.title)) && !staging.is_test(&task.title) {
                wanted.extend(parts.iter().cloned());
            }
        }
        let before = task.deps.len();
        for id in wanted {
            if id != task.id && !task.deps.contains(&id) {
                task.deps.push(id);
            }
        }
        if task.deps.len() > before {
            task.prompt.push_str(staging.staged_rule);
            if layer == Layer::Core {
                task.prompt.push_str(staging.types_win_rule);
            }
        } else if early {
            task.prompt.push_str(TYPES_FROM_PLAN_RULE);
            if layer == Layer::Core {
                task.prompt.push_str(staging.types_win_rule);
            }
        }
    }
    any
}

/// Added to a Data or Core task that starts with the Types files
/// (`PlanOptions::types_from_plan`), in place of the staged rule. It also
/// says how to read a profile's types-win rule, which speaks of the attached
/// types file.
pub const TYPES_FROM_PLAN_RULE: &str = "\n\nThe types file is written at the same time as this \
file, exactly as the plan declares it, so it is not attached: wherever a rule speaks of the \
attached types, read the plan's declarations of the types file. Import from the types file. Use \
exactly the names, fields, signatures and literal values that the plan declares for it, and do \
not declare again a type that it exports.";

/// The words of the staged rule that `PlanOptions::deps_first` turns around
/// (the NEXTJS rule; a profile rule without them stays as it is).
const DEPS_BELOW: &str = "attached below as prerequisite outputs";
const DEPS_ABOVE: &str = "attached above as prerequisite outputs";

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
/// `options.min_test_shards` raises the count before the clamp.
fn test_shards(
    objective: &str,
    manifest: &Manifest,
    profile: &Profile,
    options: &PlanOptions,
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
        .max(options.min_test_shards)
        .clamp(1, MAX_TEST_SHARDS)
        .min(modules.len());
    // Headroom over the demand so a few weak tests don't miss it; the
    // measured value is a plan-build knob (A/B,
    // `--architect-test-quota`).
    let quota = demand
        .saturating_mul(options.quota_pct)
        .div_ceil(100 * count as u64)
        .max(3);
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

/// The lines that `PlanOptions::objective_pages` adds to the tail of each
/// file's prompt, in manifest order (empty for a file that gets none). A
/// route file (`quality.page_glob`) gets the bullet of the objective's page
/// list that it owns (`objective::match_pages`), verbatim, the objective's
/// quality-bar paragraph and the planned routes with their links; a
/// component (`quality.component_glob`) gets `quality.component_rule`.
/// With `page_local` (`PlanOptions::page_local`), a page that took in local
/// components owns their features too, and each page gets
/// `pagelocal::page_lines`.
fn objective_page_lines(
    objective: &str,
    manifest: &Manifest,
    profile: &Profile,
    page_local: bool,
) -> Vec<String> {
    use crate::profile::glob_match;
    let quality = &profile.staging.quality;
    let is_page = |path: &str| glob_match(quality.page_glob, path);
    let paths: Vec<&str> = manifest
        .files
        .iter()
        .map(|file| file.path.as_str())
        .collect();
    let bullets = crate::objective::page_bullets(objective);
    let owners = crate::objective::match_pages(&paths, &bullets);
    let quality_bar = crate::objective::quality_bar_for_one_file(objective)
        .map(|text| format!("\n- Quality bar (from the objective): {text}"))
        .unwrap_or_default();
    let routes: Vec<String> = paths
        .iter()
        .zip(&owners)
        .filter(|(path, _)| is_page(path))
        .filter_map(|(path, owner)| {
            let route = crate::objective::route_of(path)?;
            // A static page that owns a detail bullet reads its record's id
            // from the query string (`/task?id=`).
            let query_detail =
                owner.is_some_and(|index| crate::objective::is_detail_bullet(&bullets[index]));
            Some(match crate::objective::href_of(&route) {
                Some(href) => format!("`{route}` ({path}; link as `{href}`)"),
                None if query_detail => {
                    format!("`{route}` ({path}; link as `{route}?id=${{id}}`)")
                }
                None => format!("`{route}` ({path})"),
            })
        })
        .collect();
    let routes = if routes.is_empty() {
        String::new()
    } else {
        format!(
            "\n- Routes of the app (link only to these): {}",
            routes.join(", ")
        )
    };
    let local = if page_local {
        crate::pagelocal::page_lines(manifest, profile, &bullets, &owners)
    } else {
        Vec::new()
    };
    paths
        .iter()
        .zip(&owners)
        .enumerate()
        .map(|(file, (path, owner))| {
            // `split_pages`: a part renders a share of its page's features,
            // so it reads its page's requirement, the quality bar and the
            // routes (the links it renders) too.
            if let Some(page) =
                crate::splitpage::page_of_part(&manifest.files[file].purpose, quality)
            {
                let bullet = paths
                    .iter()
                    .position(|other| *other == page)
                    .and_then(|at| owners[at])
                    .map(|index| {
                        format!(
                            "\n- Requirement of the page `{page}` (from the objective, \
                             verbatim); this part renders its share of it: {}",
                            bullets[index].text
                        )
                    })
                    .unwrap_or_default();
                return format!("{bullet}{quality_bar}{routes}");
            }
            if is_page(path) {
                let has_local = page_local
                    && crate::pagelocal::has_local_components(
                        &manifest.files[file].purpose,
                        quality,
                    );
                let owns = if has_local {
                    "You own every feature in this bullet, the local components of this file \
                     included"
                } else {
                    "You own every feature in this bullet"
                };
                let bullet = owner
                    .map(|index| {
                        format!(
                            "\n- Requirement (from the objective, verbatim). {owns}: {}",
                            bullets[index].text
                        )
                    })
                    .unwrap_or_default();
                let extra = local.get(file).map_or("", String::as_str);
                format!("{bullet}{quality_bar}{routes}{extra}")
            } else if !quality.component_glob.is_empty()
                && !quality.component_rule.is_empty()
                && glob_match(quality.component_glob, path)
            {
                format!("\n- {}", quality.component_rule)
            } else {
                String::new()
            }
        })
        .collect()
}

/// Deterministic README from the manifest and the objective, in the same
/// single fenced Markdown block a README worker returns: the app's name
/// (`objective::app_name`, else "Project"), a summary paragraph, the
/// objective's page list as features, the profile's `readme_sections`
/// (setup and scripts; their code blocks use `~~~` fences, so they do not
/// close this block) and the files with the planner's purposes, without
/// the notes of the manifest passes (`planner_purpose`).
fn render_readme(objective: &str, manifest: &Manifest, profile: &Profile) -> String {
    use std::fmt::Write as _;
    let name = crate::objective::app_name(objective).unwrap_or_else(|| "Project".to_string());
    let summary = crate::objective::summary(objective);
    let mut readme = String::new();
    let _ = writeln!(readme, "```markdown\n# {name}\n\n{summary}");
    let features = crate::objective::page_bullets(objective);
    if !features.is_empty() {
        readme.push_str("\n## Features\n\n");
        for feature in &features {
            let rest = feature
                .text
                .split_once(':')
                .map_or("", |(_, rest)| rest.trim());
            let _ = writeln!(readme, "- **{}**: {rest}", feature.name);
        }
    }
    readme.push_str(&(profile.readme_sections)(manifest));
    readme.push_str("\n## Files\n\n");
    for file in &manifest.files {
        let purpose = planner_purpose(&file.purpose, &profile.staging);
        if purpose.is_empty() {
            let _ = writeln!(readme, "- `{}`", file.path);
        } else {
            let _ = writeln!(readme, "- `{}` — {purpose}", file.path);
        }
    }
    readme.push_str("```");
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
    fn min_test_shards_raises_the_shard_count_up_to_the_modules() {
        let file = |path: &str| ManifestFile {
            path: path.into(),
            purpose: "x".into(),
            exports: vec![],
            uses: Vec::new(),
        };
        let manifest = |modules: usize| Manifest {
            files: (1..=modules)
                .map(|index| file(&format!("lib/m{index}.ts")))
                .collect(),
            test_files: vec![file("tests/all.test.ts")],
            readme: false,
        };
        let objective = "Build a Next.js app with vitest tests.";
        let shards = |modules: usize, min_test_shards: usize| {
            let options = PlanOptions {
                min_test_shards,
                ..PlanOptions::default()
            };
            plan_from_manifest(objective, &manifest(modules), 40, None, &options)
                .unwrap()
                .tasks
                .iter()
                .filter(|task| task.title.starts_with("tests/"))
                .count()
        };
        // No floor: one test file in the manifest, one shard.
        assert_eq!(shards(8, 0), 1);
        // The floor gives 6 shards when there are at least 6 modules.
        assert_eq!(shards(8, 6), 6);
        assert_eq!(shards(6, 6), 6);
        // Never more shards than testable modules.
        assert_eq!(shards(4, 6), 4);
        assert_eq!(shards(1, 6), 1);
        // The clamp still holds above the floor.
        assert_eq!(shards(8, 9), MAX_TEST_SHARDS);
    }

    #[test]
    fn min_test_shards_matches_extra_manifest_test_files() {
        // A floor of 6 plans the same wave as a manifest padded to six test
        // files (the bench's former `spread_tests`).
        let file = |path: &str| ManifestFile {
            path: path.into(),
            purpose: "x".into(),
            exports: vec![],
            uses: Vec::new(),
        };
        let floor = PlanOptions {
            min_test_shards: 6,
            ..PlanOptions::default()
        };
        let cases: [(&Profile, &str, &str, usize); 4] = [
            (
                &crate::profile::NEXTJS,
                "Build a Next.js app with vitest tests.",
                "lib/m{}.ts",
                1,
            ),
            (
                &crate::profile::NEXTJS,
                "Build a Next.js app with vitest tests.",
                "lib/m{}.ts",
                2,
            ),
            (
                &crate::profile::PYTHON,
                "Build a Python CLI with pytest tests.",
                "pkg/m{}.py",
                1,
            ),
            (
                &crate::profile::PYTHON,
                "Build a Python CLI with pytest tests.",
                "pkg/m{}.py",
                3,
            ),
        ];
        for (profile, objective, pattern, tests) in cases {
            let manifest = |modules: usize, tests: usize| Manifest {
                files: (1..=modules)
                    .map(|index| file(&pattern.replace("{}", &index.to_string())))
                    .collect(),
                test_files: (1..=tests)
                    .map(|index| file(&format!("tests/t{index}_test.txt")))
                    .collect(),
                readme: false,
            };
            let plan = |manifest: &Manifest, max_tasks: usize, options: &PlanOptions| {
                plan_from_manifest(objective, manifest, max_tasks, Some(profile), options)
                    .map(|plan| serde_json::to_string(&plan).unwrap())
            };
            let floored = plan(&manifest(8, tests), 40, &floor).unwrap();
            let padded = plan(&manifest(8, 6), 40, &PlanOptions::default()).unwrap();
            assert_eq!(floored, padded, "{} with {tests} test files", profile.name);
            // Near the ceiling (40 + 10) the floor's shards count as the
            // padded test files do: 45 modules + 6 test files is over it.
            assert!(plan(&manifest(45, tests), 40, &PlanOptions::default()).is_ok());
            assert!(plan(&manifest(45, 6), 40, &PlanOptions::default()).is_err());
            assert!(plan(&manifest(45, tests), 40, &floor).is_err());
        }
    }

    #[test]
    fn a_huge_test_demand_saturates_the_quota() {
        let file = |path: &str| ManifestFile {
            path: path.into(),
            purpose: "x".into(),
            exports: vec![],
            uses: Vec::new(),
        };
        let manifest = Manifest {
            files: vec![file("lib/a.ts"), file("lib/b.ts")],
            test_files: vec![file("tests/all.test.ts")],
            readme: false,
        };
        let objective = "Build a Next.js app with 18000000000000000000 tests.";
        assert!(
            plan_from_manifest(objective, &manifest, 40, None, &PlanOptions::default()).is_ok()
        );
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
        // One block: the assemble extractor must see it closed, and no
        // inner fence may close it early (the sections use `~~~`).
        assert_eq!(text.lines().filter(|l| l.starts_with("```")).count(), 2);
        assert!(text.contains("~~~bash\npython -m pytest -q\n~~~"));
    }

    #[test]
    fn readme_has_the_app_name_features_setup_and_plain_purposes() {
        let objective = "Build \"TaskFlow\", a project management web app with Next.js (App \
                         Router) and TypeScript. No backend.\n\nPages:\n- Board: a kanban \
                         board.\n- Settings: theme.\n\nQuality bar: complete pages.";
        let mut manifest = Manifest {
            files: vec![
                ManifestFile {
                    path: "lib/seed.ts".into(),
                    purpose: "Demo records.".into(),
                    exports: vec!["export function seed(): Task[]".into()],
                    uses: Vec::new(),
                },
                ManifestFile {
                    path: "app/board/page.tsx".into(),
                    purpose: "Kanban board.".into(),
                    exports: vec!["export default function Board()".into()],
                    uses: Vec::new(),
                },
            ],
            test_files: Vec::new(),
            readme: true,
        };
        let nextjs = &crate::profile::NEXTJS;
        let options = PlanOptions {
            tidy: true,
            today: Some((2026, 11, 15)),
            ..PlanOptions::default()
        };
        apply_manifest_passes(&mut manifest, nextjs, &options);
        assert!(manifest.files[0].purpose.contains("Today is 2026-11-15"));
        let text = render_readme(objective, &manifest, nextjs);
        assert!(
            text.starts_with("```markdown\n# TaskFlow\n\nTaskFlow is a project management"),
            "{text}"
        );
        assert!(
            text.contains("## Features\n\n- **Board**: a kanban board.\n- **Settings**: theme.\n"),
            "{text}"
        );
        assert!(text.contains("## Setup\n\n~~~bash\nnpm install\nnpm run dev\n~~~"));
        assert!(text.contains("- `npm run build`"));
        assert!(text.contains("- `lib/seed.ts` — Demo records.\n"), "{text}");
        assert!(!text.contains("Today is") && !text.contains("compact object literal"));
        assert_eq!(text.lines().filter(|l| l.starts_with("```")).count(), 2);
        assert!(text.ends_with("```"));
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

    #[tokio::test]
    async fn plan_project_replays_a_saved_manifest_through_the_pass() {
        // No model call: the saved manifest is used, the pass runs first,
        // and the plan equals plan_from_manifest on the passed manifest.
        let client = ScriptedClient::new(Vec::new());
        let mut renamed = |manifest: &mut Manifest| -> Result<()> {
            manifest.files[1].purpose = "argparse front end".into();
            Ok(())
        };
        let (planned, plan) = plan_project_with(
            &client,
            "obj",
            16,
            &[],
            &ArchitectOptions::default(),
            &PlanOptions::default(),
            None,
            ManifestSource {
                saved: Some(manifest()),
                prepare: Some(&mut renamed),
            },
        )
        .await
        .unwrap();
        assert_eq!(client.call_count(), 0);
        assert_eq!(planned.files[1].purpose, "argparse front end");
        let expected =
            plan_from_manifest("obj", &planned, 16, None, &PlanOptions::default()).unwrap();
        assert_eq!(
            serde_json::to_string(&plan).unwrap(),
            serde_json::to_string(&expected).unwrap()
        );

        let mut failing = |_: &mut Manifest| -> Result<()> { bail!("pass failed") };
        let error = plan_project_with(
            &client,
            "obj",
            16,
            &[],
            &ArchitectOptions::default(),
            &PlanOptions::default(),
            None,
            ManifestSource {
                saved: Some(manifest()),
                prepare: Some(&mut failing),
            },
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("pass failed"));
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

#[cfg(test)]
mod stage_tests {
    use super::*;
    use crate::profile::{NEXTJS, PYTHON};

    const OBJECTIVE: &str = "Build a task tracker with projects, a board, a calendar and reports.";

    fn fixture() -> Manifest {
        serde_json::from_str(include_str!("../tests/fixtures/plan-manifest.json")).unwrap()
    }

    fn plan(profile: &'static Profile, options: &PlanOptions) -> Plan {
        plan_from_manifest(OBJECTIVE, &fixture(), 64, Some(profile), options).unwrap()
    }

    fn staged() -> PlanOptions {
        PlanOptions {
            staged: true,
            ..PlanOptions::default()
        }
    }

    fn task<'a>(plan: &'a Plan, title: &str) -> &'a Task {
        plan.tasks
            .iter()
            .find(|task| task.title == title)
            .unwrap_or_else(|| panic!("no task {title}"))
    }

    /// The text `stage_tasks` appended to a task's prompt.
    fn suffix<'a>(staged: &'a Plan, flat: &Plan, title: &str) -> &'a str {
        let flat = &task(flat, title).prompt;
        let prompt = &task(staged, title).prompt;
        prompt
            .strip_prefix(flat.as_str())
            .unwrap_or_else(|| panic!("{title}: staging must only append"))
    }

    #[test]
    fn staged_nextjs_plan_orders_types_then_core_then_consumers() {
        let flat = plan(&NEXTJS, &PlanOptions::default());
        let plan = plan(&NEXTJS, &staged());
        let rules = &NEXTJS.staging;
        let types = vec![task_id("lib/types.ts")];
        let mut core: Vec<String> = ["lib/dates.ts", "lib/storage.ts", "lib/tasks.ts"]
            .iter()
            .map(|path| task_id(path))
            .collect();
        let mut types_and_core = types.clone();
        types_and_core.append(&mut core);

        assert!(task(&plan, "lib/types.ts").deps.is_empty());
        assert_eq!(
            suffix(&plan, &flat, "lib/types.ts"),
            rules.import_rule,
            "types get the import rule only"
        );
        // The seed data needs the types only.
        assert_eq!(task(&plan, "lib/seed.ts").deps, types);
        assert_eq!(
            suffix(&plan, &flat, "lib/seed.ts"),
            format!("{}{}", rules.import_rule, rules.staged_rule)
        );
        assert_eq!(task(&plan, "lib/tasks.ts").deps, types);
        assert_eq!(
            suffix(&plan, &flat, "lib/tasks.ts"),
            format!(
                "{}{}{}",
                rules.import_rule, rules.staged_rule, rules.types_win_rule
            )
        );
        let next = rules.path_rule("app/page.tsx").unwrap();
        assert_eq!(task(&plan, "app/page.tsx").deps, types_and_core);
        assert_eq!(
            suffix(&plan, &flat, "app/page.tsx"),
            format!("{}{next}{}", rules.import_rule, rules.staged_rule)
        );
        let shards: Vec<&Task> = plan
            .tasks
            .iter()
            .filter(|task| rules.is_test(&task.title))
            .collect();
        assert!(!shards.is_empty());
        let test_rule = rules.path_rule("tests/01_tasks.test.ts").unwrap();
        assert_ne!(test_rule, next);
        assert_eq!(rules.path_rule("app/x.test.tsx"), Some(test_rule));
        for shard in shards {
            assert_eq!(shard.deps, types_and_core, "{}", shard.title);
            assert_eq!(
                suffix(&plan, &flat, &shard.title),
                format!("{}{test_rule}{}", rules.import_rule, rules.staged_rule)
            );
        }
        // Files with no layer (config, CSS) and the README are not staged.
        for title in ["package.json", "app/globals.css", "README.md"] {
            assert_eq!(
                serde_json::to_string(task(&plan, title)).unwrap(),
                serde_json::to_string(task(&flat, title)).unwrap(),
                "{title}"
            );
        }
        assert!(
            plan.cache_primes.is_empty(),
            "the first stage warms the cache"
        );
        assert!(!flat.cache_primes.is_empty());
    }

    /// `types_from_plan`: Data and Core start with the Types (no deps, the
    /// plan rule in place of the staged rule); Consumers are as before; off,
    /// the plan is the staged plan.
    #[test]
    fn types_from_plan_starts_core_and_data_with_the_types() {
        let flat = plan(&NEXTJS, &PlanOptions::default());
        let staged_plan = plan(&NEXTJS, &staged());
        let early = plan(
            &NEXTJS,
            &PlanOptions {
                types_from_plan: true,
                ..staged()
            },
        );
        let rules = &NEXTJS.staging;
        assert!(task(&early, "lib/types.ts").deps.is_empty());
        assert!(task(&early, "lib/seed.ts").deps.is_empty());
        assert_eq!(
            suffix(&early, &flat, "lib/seed.ts"),
            format!("{}{TYPES_FROM_PLAN_RULE}", rules.import_rule)
        );
        for title in ["lib/dates.ts", "lib/storage.ts", "lib/tasks.ts"] {
            assert!(task(&early, title).deps.is_empty(), "{title}");
            assert_eq!(
                suffix(&early, &flat, title),
                format!(
                    "{}{TYPES_FROM_PLAN_RULE}{}",
                    rules.import_rule, rules.types_win_rule
                ),
                "{title}"
            );
        }
        // Consumers (pages, tests) and the unstaged files are untouched.
        for task in &early.tasks {
            let layer = rules.layer_of(&task.title);
            if matches!(layer, Some(Layer::Core) | Some(Layer::Data)) && !rules.is_test(&task.title)
            {
                continue;
            }
            assert_eq!(
                serde_json::to_string(task).unwrap(),
                serde_json::to_string(self::task(&staged_plan, &task.title)).unwrap(),
                "{}",
                task.title
            );
        }
        // Without `staged` the switch does nothing.
        assert_eq!(
            serde_json::to_string(&plan(
                &NEXTJS,
                &PlanOptions {
                    types_from_plan: true,
                    ..PlanOptions::default()
                }
            ))
            .unwrap(),
            serde_json::to_string(&flat).unwrap()
        );
    }

    #[test]
    fn staged_prompts_keep_the_shared_prefix() {
        // Staging appends at the end: every module and test prompt still
        // starts with the head the unstaged plan primes.
        let flat = plan(&NEXTJS, &PlanOptions::default());
        let plan = plan(&NEXTJS, &staged());
        assert_eq!(flat.cache_primes.len(), 2, "module + test heads");
        for staged in plan
            .tasks
            .iter()
            .filter(|task| task.static_output.is_none())
        {
            assert!(
                flat.cache_primes
                    .iter()
                    .any(|head| staged.prompt.starts_with(head.as_str())),
                "{} lost the shared prefix",
                staged.title
            );
        }
    }

    #[test]
    fn staged_off_and_profiles_without_staging_plan_as_before() {
        let default = PlanOptions::default();
        // Default plans match the pre-staging goldens in tests/plan_golden.rs.
        assert!(!default.staged, "staging is never on by default");
        // PYTHON has no staging layers: `staged` changes nothing, primes too.
        assert_eq!(
            serde_json::to_string(&plan(&PYTHON, &staged())).unwrap(),
            serde_json::to_string(&plan(&PYTHON, &default)).unwrap()
        );
    }

    #[test]
    fn staged_deps_name_existing_tasks_and_validate() {
        let plan = plan(&NEXTJS, &staged());
        let ids: Vec<&str> = plan.tasks.iter().map(|task| task.id.as_str()).collect();
        for task in &plan.tasks {
            assert!(
                !task.deps.contains(&task.id),
                "{} depends on itself",
                task.id
            );
            for dep in &task.deps {
                assert!(ids.contains(&dep.as_str()), "{} -> {dep}", task.id);
            }
        }
        plan.validate().unwrap();
    }
}

#[cfg(test)]
mod manifest_pass_tests {
    use super::*;
    use crate::profile::{NEXTJS, PYTHON};

    fn manifest(text: &str) -> Manifest {
        serde_json::from_str(text).unwrap()
    }

    const DUPLICATES: &str = r#"{"files":[
      {"path":"lib/types.ts","purpose":"types","exports":["export interface Task { id: string }"]},
      {"path":"lib/tasks.ts","purpose":"tasks.","exports":["export interface Task  // other","export function all(): Task[]"]},
      {"path":"lib/storage.ts","purpose":"storage.","exports":["export interface AppState { tasks: Task[] }","export function load(): AppState"]},
      {"path":"lib/store.ts","purpose":"store.","exports":["  export type AppState = { tasks: Task[] }","export enum Mode { A }"]},
      {"path":"components/A.tsx","purpose":"a.","exports":["export interface Props { a: string }"]},
      {"path":"components/B.tsx","purpose":"b.","exports":["export interface Props { b: string }"]},
      {"path":"lib/seed.ts","purpose":"demo records","exports":["export const seed: Task[]"]}
    ]}"#;

    #[test]
    fn types_files_own_their_types_and_the_first_file_owns_the_rest() {
        let mut manifest = manifest(DUPLICATES);
        dedupe_type_owners(&mut manifest, &NEXTJS);
        let files = &manifest.files;
        assert_eq!(files[0].exports.len(), 1);
        assert_eq!(files[1].exports, ["export function all(): Task[]"]);
        assert!(files[1].purpose.ends_with(
            " Imports Task from `lib/types.ts`: this file does not declare or export a type \
             with that name, and it uses the fields and literal values of `lib/types.ts`."
        ));
        // `lib/storage.ts` declares `AppState` first and keeps it.
        assert_eq!(files[2].exports.len(), 2);
        assert_eq!(files[3].exports, ["export enum Mode { A }"]);
        assert_eq!(
            files[3].purpose,
            "store. Imports AppState from `lib/storage.ts`: this file does not declare a type \
             with that name."
        );
        // Components keep their own `Props`.
        assert_eq!(files[4].exports.len(), 1);
        assert_eq!(files[5].exports.len(), 1);
        assert_eq!(files[5].purpose, "b.");
    }

    const UNDECLARED: &str = r#"{"files":[
      {"path":"lib/types.ts","purpose":"types","exports":["export type Status = 'todo' | 'done'","export interface Task { id: string; status: Status }"]},
      {"path":"lib/team.ts","purpose":"team","exports":["export interface Member { id: string }"]},
      {"path":"lib/storage.ts","purpose":"storage","exports":["export function loadState(): AppState  // {tasks: Task[], members: Member[], settings: Settings}; corrupt -> seed","export function saveState(state: AppState): void"]},
      {"path":"lib/seed.ts","purpose":"seed","exports":["export function seedState(): AppState  // {tasks: 3, settings: {theme: 'light'}}"]},
      {"path":"lib/stats.ts","purpose":"stats","exports":["export function progress(tasks: Task[]): Progress  // {done: number, total: number, label: 'a' | 'b'}","export function name(): Owner  // {owner: first member}"]},
      {"path":"components/Column.tsx","purpose":"column","exports":["export default function Column(props: {status: TaskStatus; tasks: Task[]; node: React.ReactNode; Title: string}): JSX.Element  // 'Done' -> green","export function Card(props: CardProps)"]},
      {"path":"tests/stats.test.ts","purpose":"tests","exports":["export const cases: Fixture[]"]}
    ]}"#;

    fn exports_of<'a>(manifest: &'a Manifest, path: &str) -> &'a [String] {
        &manifest
            .files
            .iter()
            .find(|file| file.path == path)
            .unwrap()
            .exports
    }

    #[test]
    fn undeclared_signature_types_get_a_declaration_in_an_owner_file() {
        let mut manifest = manifest(UNDECLARED);
        declare_signature_types(&mut manifest, &NEXTJS.staging);
        let types = exports_of(&manifest, "lib/types.ts");
        // Shapeless types go to the types file, after its declarations; a
        // type that only an added shape names (`Settings`) in a second round.
        assert_eq!(types.len(), 6, "{types:?}");
        let line = |name: &str| {
            types
                .iter()
                .find(|line| line.starts_with(&format!("export type {name}  // declared here")))
                .unwrap_or_else(|| panic!("{name} in {types:?}"))
        };
        // `TaskStatus` ends with `Status` alone of the types file's types.
        assert!(
            types.contains(
                &"export type TaskStatus = Status;  // declared here for the signatures that name it"
                    .to_string()
            ),
            "{types:?}"
        );
        assert!(line("Owner").contains("(in `lib/stats.ts`)"));
        assert!(line("Settings").contains("(in `lib/storage.ts`)"));
        assert_eq!(types.last().unwrap(), line("Settings"));
        // A shape that names a type declared outside the types file stays
        // with the signature that gives it.
        let storage = exports_of(&manifest, "lib/storage.ts");
        assert_eq!(
            storage[0],
            "export interface AppState { tasks: Task[]; members: Member[]; settings: Settings }  \
             // declared here for the signatures that name it"
        );
        // A shape of types declared in the types file goes there; a union of
        // literals is a type, and a shape of values gives no shape.
        assert_eq!(exports_of(&manifest, "lib/stats.ts").len(), 2);
        assert_eq!(
            types[2],
            "export interface Progress { done: number; total: number; label: 'a' | 'b' }  \
             // declared here for the signatures that name it"
        );
        // Never a `...Props`, a namespace, an object key, a test's type, a
        // literal or a known type.
        let all: String = manifest
            .files
            .iter()
            .flat_map(|file| file.exports.iter())
            .cloned()
            .collect::<Vec<_>>()
            .join("\n");
        for name in [
            "CardProps",
            "React",
            "ReactNode",
            "Title",
            "Fixture",
            "Done",
            "JSX",
        ] {
            assert!(
                !all.contains(&format!("type {name} ")),
                "{name} must not be declared:\n{all}"
            );
        }
        // Idempotent.
        let once = serde_json::to_string(&manifest).unwrap();
        declare_signature_types(&mut manifest, &NEXTJS.staging);
        assert_eq!(serde_json::to_string(&manifest).unwrap(), once);
    }

    #[test]
    fn a_shape_with_types_of_the_types_file_goes_to_the_types_file() {
        let mut manifest = manifest(
            r#"{"files":[
              {"path":"lib/types.ts","purpose":"types","exports":["export interface Task { id: string }"]},
              {"path":"lib/storage.ts","purpose":"storage","exports":["export function loadState(): AppState  // {tasks: Task[], filter?: 'all' | 'open', at: string | null, on: (t: Task) => void}"]}
            ]}"#,
        );
        declare_signature_types(&mut manifest, &NEXTJS.staging);
        assert_eq!(
            manifest.files[0].exports[1],
            "export interface AppState { tasks: Task[]; filter?: 'all' | 'open'; at: string | \
             null; on: (t: Task) => void }  // declared here for the signatures that name it"
        );
        assert_eq!(manifest.files[1].exports.len(), 1);
    }

    #[test]
    fn a_comment_of_default_values_gives_no_shape() {
        // Seen in 94 saved bench manifests: `[]` is the empty tuple type and
        // `'light'` and `null` are literal types, so this is no interface.
        let mut manifest = manifest(
            r#"{"files":[
              {"path":"lib/types.ts","purpose":"types","exports":["export interface Task { id: string }"]},
              {"path":"lib/storage.ts","purpose":"storage","exports":["export function loadState(): AppState  // {projects: [], tasks: [], members: [], comments: [], settings: {theme: 'light', defaultProjectId: null}} when empty; corrupt JSON -> seeded state"]}
            ]}"#,
        );
        declare_signature_types(&mut manifest, &NEXTJS.staging);
        assert!(
            manifest.files[0].exports[1].starts_with(
                "export type AppState  // declared here for the signatures that name it (in `lib/storage.ts`)"
            ),
            "{:?}",
            manifest.files[0].exports
        );
        assert_eq!(manifest.files[1].exports.len(), 1);
        for value in [
            "[]",
            "[ ]",
            "{}",
            "null",
            "true",
            "light",
            "['a', 'b']",
            "{theme: 'light'}",
            "{a: {b: []}}",
        ] {
            assert!(!reads_as_type(value), "{value}");
        }
        for value in [
            "Task[]",
            "[string, number]",
            "[start: Date, end?: Date]",
            "string | null",
            "{a: {b: Task[]}}",
            "'light' | 'dark'",
            "undefined",
        ] {
            assert!(reads_as_type(value), "{value}");
        }
    }

    #[test]
    fn an_alias_needs_one_type_that_ends_the_name() {
        let mut manifest = manifest(
            r#"{"files":[
              {"path":"lib/types.ts","purpose":"types","exports":["export type Status = 'a' | 'b'","export type TaskStatus2 = 'x'","export interface Id { v: string }","export interface Item { id: Id }","export interface WorkItem { id: Id }"]},
              {"path":"lib/a.ts","purpose":"a","exports":["export function f(s: TaskStatus, i: TaskId, w: BigWorkItem): void"]}
            ]}"#,
        );
        declare_signature_types(&mut manifest, &NEXTJS.staging);
        let types = &manifest.files[0].exports;
        // `TaskStatus`: `Status` alone; `TaskId`: `Id` is under four letters;
        // `BigWorkItem`: both `Item` and `WorkItem` end it.
        assert!(types
            .iter()
            .any(|line| line.starts_with("export type TaskStatus = Status;")));
        assert!(types
            .iter()
            .any(|line| line.starts_with("export type TaskId  //")));
        assert!(types
            .iter()
            .any(|line| line.starts_with("export type BigWorkItem  //")));
    }

    #[test]
    fn declare_types_runs_only_with_tidy_and_the_option() {
        let profile = &NEXTJS;
        let mut plain = manifest(UNDECLARED);
        apply_manifest_passes(&mut plain, profile, &PlanOptions::default());
        let mut declared_without_tidy = manifest(UNDECLARED);
        apply_manifest_passes(
            &mut declared_without_tidy,
            profile,
            &PlanOptions {
                declare_types: true,
                ..PlanOptions::default()
            },
        );
        let before = serde_json::to_string(&manifest(UNDECLARED)).unwrap();
        assert_eq!(serde_json::to_string(&plain).unwrap(), before);
        assert_eq!(
            serde_json::to_string(&declared_without_tidy).unwrap(),
            before
        );
        let mut tidy = manifest(UNDECLARED);
        apply_manifest_passes(
            &mut tidy,
            profile,
            &PlanOptions {
                tidy: true,
                declare_types: true,
                ..PlanOptions::default()
            },
        );
        assert!(exports_of(&tidy, "lib/types.ts")
            .iter()
            .any(|line| line.starts_with("export type TaskStatus")));
        // A profile without `export interface ` declarations: no change.
        let mut python = manifest(UNDECLARED);
        declare_signature_types(&mut python, &PYTHON.staging);
        assert_eq!(serde_json::to_string(&python).unwrap(), before);
    }

    #[test]
    fn dedupe_does_nothing_without_type_prefixes() {
        let mut manifest = manifest(DUPLICATES);
        dedupe_type_owners(&mut manifest, &PYTHON);
        let before = self::manifest(DUPLICATES);
        assert_eq!(
            serde_json::to_string(&manifest).unwrap(),
            serde_json::to_string(&before).unwrap()
        );
    }

    #[test]
    fn data_note_and_date_go_on_data_files_once() {
        let mut manifest = manifest(DUPLICATES);
        let options = PlanOptions {
            tidy: true,
            today: Some((2026, 3, 7)),
            ..PlanOptions::default()
        };
        apply_manifest_passes(&mut manifest, &NEXTJS, &options);
        apply_manifest_passes(&mut manifest, &NEXTJS, &options);
        let seed = &manifest.files[6].purpose;
        assert!(seed.starts_with(&format!("demo records{}", NEXTJS.staging.data_note)));
        assert!(seed.ends_with(
            " Today is 2026-03-07: put the dates of the records in the three weeks before and \
             the three weeks after this day, some in the past and some in the future."
        ));
        assert_eq!(seed.matches("Today is ").count(), 1);
        assert_eq!(
            manifest.files[1].purpose, "tasks.",
            "no dedupe without one_type_owner"
        );
    }

    #[test]
    fn relative_dates_replace_the_fixed_day_once() {
        let mut manifest = manifest(DUPLICATES);
        let dated = PlanOptions {
            tidy: true,
            today: Some((2026, 3, 7)),
            ..PlanOptions::default()
        };
        apply_manifest_passes(&mut manifest, &NEXTJS, &dated);
        assert!(manifest.files[6].purpose.contains("Today is 2026-03-07"));
        let relative = PlanOptions {
            relative_dates: true,
            ..dated
        };
        apply_manifest_passes(&mut manifest, &NEXTJS, &relative);
        apply_manifest_passes(&mut manifest, &NEXTJS, &relative);
        let seed = &manifest.files[6].purpose;
        let note = NEXTJS.staging.quality.date_note;
        assert!(!seed.contains("Today is "), "{seed}");
        assert_eq!(seed.matches(note).count(), 1);
        assert_eq!(
            *seed,
            format!("demo records{}{note}", NEXTJS.staging.data_note)
        );
        assert!(note.contains("daysAgo(n)") && note.contains("12 months"));
        // No date note: the fixed day as before.
        let mut python = self::manifest(DUPLICATES);
        stamp_relative_dates(&mut python, &PYTHON);
        assert_eq!(python.files[6].purpose, "demo records");
    }

    #[test]
    fn quality_rules_set_the_theme_selector_everywhere() {
        let mut manifest = manifest(
            r#"{"files":[
              {"path":"app/styles/part1.css","purpose":"Part 1 of 2 of the global style sheet. The variables on `:root` with their dark values on `.theme-dark`.","exports":[".nav  // bar"]},
              {"path":"app/styles/part2.css","purpose":"Part 2.","exports":[".theme-dark  // dark theme root override"]}
            ]}"#,
        );
        let before = serde_json::to_string(&manifest).unwrap();
        apply_manifest_passes(&mut manifest, &NEXTJS, &PlanOptions::default());
        assert_eq!(
            serde_json::to_string(&manifest).unwrap(),
            before,
            "off: no change"
        );
        let options = PlanOptions {
            quality_rules: true,
            ..PlanOptions::default()
        };
        apply_manifest_passes(&mut manifest, &NEXTJS, &options);
        apply_manifest_passes(&mut manifest, &NEXTJS, &options);
        assert!(manifest.files[0]
            .purpose
            .ends_with("dark values on `:root[data-theme=\"dark\"]`."));
        assert_eq!(
            manifest.files[1].exports[0],
            ":root[data-theme=\"dark\"]  // dark theme root override"
        );
        let text = serde_json::to_string(&manifest).unwrap();
        assert!(!text.contains("theme-dark"));
    }

    const LITERALS: &str = r#"{"files":[
      {"path":"lib/types.ts","purpose":"types","exports":[
        "export type Status = 'backlog' | 'in-progress' | 'review' | 'done'  // exactly these",
        "export interface Settings { theme: 'light' | 'dark' }"]},
      {"path":"lib/board.ts","purpose":"board","exports":[
        "export const STATUSES: Status[]  // ['Backlog','In Progress','Review','Done']",
        "export const STATUS_LABELS: Record<Status, string>  // {backlog: 'Backlog', 'in-progress': 'In Progress'}"]},
      {"path":"lib/team.ts","purpose":"Counts tasks with status !== \"Done\".","exports":[
        "export function workload(tasks: { status: string }[]): number  // openCount counts status!=='Done'; column title 'Review' shown; member's 'Dark' mode"]},
      {"path":"app/page.tsx","purpose":"Dashboard","exports":["export default function Home()  // theme 'Dark' -> 'dark' class"]},
      {"path":"lib/format.ts","purpose":"Two buttons 'Week' and 'Done' switch the view.","exports":[
        "export function formatStatus(s: Status): string  // 'in-progress' -> 'In Progress'; 'done' -> 'Done'",
        "export const COLUMN_NAMES: Record<Status, string>  // backlog -> 'Backlog', review => 'Review'",
        "export function isOpen(s: Status): boolean  // 'Done' === s -> false; case 'Review': true"]}
    ]}"#;

    #[test]
    fn literals_take_the_spelling_of_the_types() {
        let mut manifest = manifest(LITERALS);
        assert_eq!(align_union_literals(&mut manifest, &NEXTJS.staging), 8);
        let files = &manifest.files;
        assert_eq!(
            files[1].exports[0],
            "export const STATUSES: Status[]  // ['backlog','in-progress','review','done']"
        );
        // A labels map keeps its display text.
        assert_eq!(
            files[1].exports[1],
            "export const STATUS_LABELS: Record<Status, string>  // {backlog: 'Backlog', \
             'in-progress': 'In Progress'}"
        );
        assert_eq!(files[2].purpose, "Counts tasks with status !== \"done\".");
        // A clause that names a title keeps its literal; an apostrophe is
        // not a quote.
        assert_eq!(
            files[2].exports[0],
            "export function workload(tasks: { status: string }[]): number  // openCount \
             counts status!=='done'; column title 'Review' shown; member's 'Dark' mode"
        );
        // Prose and mapped text keep their spelling: only values change.
        assert_eq!(
            files[3].exports[0],
            "export default function Home()  // theme 'Dark' -> 'dark' class"
        );
        assert_eq!(
            files[4].purpose,
            "Two buttons 'Week' and 'Done' switch the view."
        );
        assert_eq!(
            files[4].exports[0],
            "export function formatStatus(s: Status): string  // 'in-progress' -> 'In Progress'; \
             'done' -> 'Done'"
        );
        assert_eq!(
            files[4].exports[1],
            "export const COLUMN_NAMES: Record<Status, string>  // backlog -> 'Backlog', review => \
             'Review'"
        );
        assert_eq!(
            files[4].exports[2],
            "export function isOpen(s: Status): boolean  // 'done' === s -> false; case 'review': \
             true"
        );
        // The types file itself does not change, and a second run changes
        // nothing.
        assert!(files[0].exports[0].starts_with("export type Status = 'backlog'"));
        let once = serde_json::to_string(&manifest).unwrap();
        assert_eq!(align_union_literals(&mut manifest, &NEXTJS.staging), 0);
        assert_eq!(serde_json::to_string(&manifest).unwrap(), once);
        // Only with the option, and never for a profile without layers.
        let mut off = self::manifest(LITERALS);
        apply_manifest_passes(&mut off, &NEXTJS, &PlanOptions::default());
        assert!(off.files[2].purpose.contains("\"Done\""));
        let mut python = self::manifest(LITERALS);
        assert_eq!(align_union_literals(&mut python, &PYTHON.staging), 0);
    }

    #[test]
    fn two_members_with_the_same_letters_are_left_alone() {
        let mut manifest = manifest(
            r#"{"files":[
              {"path":"lib/types.ts","purpose":"types","exports":["export type Kind = 'in-progress' | 'In Progress'"]},
              {"path":"lib/a.ts","purpose":"a","exports":["export const K = 'in_progress'"]}
            ]}"#,
        );
        assert_eq!(align_union_literals(&mut manifest, &NEXTJS.staging), 0);
        assert_eq!(
            manifest.files[1].exports[0],
            "export const K = 'in_progress'"
        );
    }

    #[test]
    fn planner_purpose_drops_the_notes_of_the_passes() {
        let staging = &NEXTJS.staging;
        let purpose = format!(
            "Seed records.{} Today is 2026-03-07: put the dates of the records in the three \
             weeks before and the three weeks after this day, some in the past and some in the \
             future. Imports Task, Status from `lib/types.ts`: this file does not declare or \
             export a type with that name, and it uses the fields and literal values of \
             `lib/types.ts`. Imports AppState from `lib/store.ts`: this file does not declare a \
             type with that name.",
            staging.data_note
        );
        assert_eq!(planner_purpose(&purpose, staging), "Seed records.");
        let relative = format!("Seed records.{}", staging.quality.date_note);
        assert_eq!(planner_purpose(&relative, staging), "Seed records.");
        assert_eq!(
            planner_purpose(
                "Part 2 of 3 of the global style sheet: only the rules of the classes.",
                staging
            ),
            "Part 2 of 3 of the global style sheet."
        );
        assert_eq!(planner_purpose("Kanban board.", staging), "Kanban board.");
    }

    const PAGES_OBJECTIVE: &str = "Build \"TaskFlow\", a task app.\n\nPages:\n- Dashboard: \
        summary cards.\n- Board: a kanban board with columns; move tasks.\n- Task detail: edit \
        title; comments.\n\nQuality bar: every page is complete.";

    const PAGES: &str = r#"{"files":[
      {"path":"lib/types.ts","purpose":"Shared types.","exports":["export interface Task { id: string }"]},
      {"path":"lib/seed.ts","purpose":"Demo records.","exports":["export function seed(): Task[]"]},
      {"path":"app/page.tsx","purpose":"Dashboard page.","exports":["export default function Home()"]},
      {"path":"app/board/page.tsx","purpose":"Kanban board.","exports":["export default function BoardPage()"]},
      {"path":"app/tasks/[id]/page.tsx","purpose":"Task detail.","exports":["export default function TaskPage()  // edits one task"]},
      {"path":"components/TaskCard.tsx","purpose":"Renders one task card.","exports":["export default function TaskCard(p: { task: Task })"]}
    ],"test_files":[],"readme":false}"#;

    fn prompt<'a>(plan: &'a Plan, path: &str) -> &'a str {
        &plan
            .tasks
            .iter()
            .find(|task| task.title == path)
            .unwrap()
            .prompt
    }

    #[test]
    fn objective_pages_give_each_page_its_bullet_and_the_routes() {
        let manifest = manifest(PAGES);
        let off = plan_from_manifest(
            PAGES_OBJECTIVE,
            &manifest,
            16,
            Some(&NEXTJS),
            &PlanOptions::default(),
        )
        .unwrap();
        assert!(!prompt(&off, "app/board/page.tsx").contains("Requirement"));
        assert!(!prompt(&off, "app/board/page.tsx").contains("(Kanban board)"));
        let options = PlanOptions {
            objective_pages: true,
            tidy: true,
            ..PlanOptions::default()
        };
        let plan =
            plan_from_manifest(PAGES_OBJECTIVE, &manifest, 16, Some(&NEXTJS), &options).unwrap();
        let board = prompt(&plan, "app/board/page.tsx");
        assert!(board.contains(
            "\n- Requirement (from the objective, verbatim). You own every feature in this \
             bullet: Board: a kanban board with columns; move tasks."
        ));
        assert!(board.contains("\n- Quality bar (from the objective): every page is complete."));
        assert!(board.contains(
            "\n- Routes of the app (link only to these): `/` (app/page.tsx), `/board` \
             (app/board/page.tsx), `/tasks/[id]` (app/tasks/[id]/page.tsx; link as `/tasks/${id}`)"
        ));
        assert!(prompt(&plan, "app/page.tsx").contains("this bullet: Dashboard: summary cards."));
        let detail = prompt(&plan, "app/tasks/[id]/page.tsx");
        assert!(detail.contains("this bullet: Task detail: edit title; comments."));
        // `next_dyn_route` keeps the planner's note.
        assert!(detail.contains(
            "all the code of the page is in this file, apart from the planned components it \
             renders; edits one task"
        ));
        let card = prompt(&plan, "components/TaskCard.tsx");
        assert!(card.contains(
            "\n- Scope: the page owns the layout; you own only what your contract lists."
        ));
        assert!(!card.contains("Requirement"));
        // The head lists purposes (without the seed note) and the rule, the
        // same bytes for every worker.
        let lib = prompt(&plan, "lib/types.ts");
        assert!(lib.contains("- `components/TaskCard.tsx` (Renders one task card): export default"));
        assert!(lib.contains("- `lib/seed.ts` (Demo records): export function seed()"));
        assert!(lib.contains("- If you import a component, do not render again what its purpose"));
        let head = |text: &str| {
            text.split("Now write the COMPLETE")
                .next()
                .unwrap()
                .to_string()
        };
        assert_eq!(head(lib), head(card));
        assert_eq!(head(lib), head(board));
        // Python has no page glob: nothing changes.
        let python =
            plan_from_manifest(PAGES_OBJECTIVE, &manifest, 16, Some(&PYTHON), &options).unwrap();
        let python_off = plan_from_manifest(
            PAGES_OBJECTIVE,
            &manifest,
            16,
            Some(&PYTHON),
            &PlanOptions {
                tidy: true,
                ..PlanOptions::default()
            },
        )
        .unwrap();
        assert_eq!(
            serde_json::to_string(&python).unwrap(),
            serde_json::to_string(&python_off).unwrap()
        );
    }

    #[test]
    fn quality_rules_end_every_module_head() {
        let manifest = manifest(PAGES);
        let options = PlanOptions {
            quality_rules: true,
            ..PlanOptions::default()
        };
        let plan =
            plan_from_manifest(PAGES_OBJECTIVE, &manifest, 16, Some(&NEXTJS), &options).unwrap();
        for task in plan
            .tasks
            .iter()
            .filter(|task| task.static_output.is_none())
        {
            let head = task.prompt.split("Now write the COMPLETE").next().unwrap();
            assert!(
                head.ends_with(NEXTJS.staging.quality.head_rule),
                "{}",
                task.title
            );
        }
        assert!(NEXTJS
            .staging
            .quality
            .head_rule
            .contains("document.documentElement.dataset.theme"));
        assert!(NEXTJS
            .staging
            .quality
            .head_rule
            .contains("crypto.randomUUID()"));
        assert_eq!(plan.cache_primes.len(), 1);
        assert!(plan.cache_primes[0].ends_with(NEXTJS.staging.quality.head_rule));
    }

    #[test]
    fn client_pages_take_any_page_that_reads_search_params() {
        let mut manifest = manifest(
            r#"{"files":[
              {"path":"app/search/page.tsx","purpose":"Search.","exports":["export default function SearchPage({ searchParams })  // reads q from awaited searchParams","export function Hit()"]},
              {"path":"app/tasks/[id]/page.tsx","purpose":"Task.","exports":["export default function TaskPage({ searchParams })"]}
            ]}"#,
        );
        crate::profile::next_client_pages(&mut manifest);
        let once = serde_json::to_string(&manifest).unwrap();
        let search = &manifest.files[0].exports;
        assert!(search[0].starts_with("export default function SearchPage()  // Client Component"));
        assert!(search[0].ends_with("<Suspense>; reads q from the query string"));
        assert_eq!(
            crate::profile::client_page_note(
                "client page; awaits searchParams Promise {project?: string}; awaits q; \
                 awaits searchParams; no project -> first project"
            ),
            "query parameters {project?: string}; query parameter q; no project -> first project"
        );
        assert_eq!(search[1], "export function Hit()", "only the page line");
        assert!(
            !manifest.files[1].exports[0].contains("Client Component"),
            "dynamic route"
        );
        crate::profile::next_client_pages(&mut manifest);
        assert_eq!(
            serde_json::to_string(&manifest).unwrap(),
            once,
            "idempotent"
        );
    }
}

#[cfg(test)]
mod q2_tests {
    //! Quality step Q2: the app contract, the wiring rules, consumers that
    //! see the CSS parts and the deps before the per-file tail.
    use super::*;
    use crate::profile::NEXTJS;
    use std::collections::HashMap;
    use std::sync::Mutex;

    const OBJECTIVE: &str = "Build a Next.js app with tests.";

    #[test]
    fn a_test_file_under_files_leaves_the_skeleton() {
        let skeleton = |files: &[&str], tests: &[&str]| Manifest {
            files: files.iter().map(|path| file(path, "", &[])).collect(),
            test_files: tests.iter().map(|path| file(path, "", &[])).collect(),
            readme: false,
        };
        let paths = |files: &[ManifestFile]| -> Vec<String> {
            files.iter().map(|file| file.path.clone()).collect()
        };
        // The skeleton has its tests: the stray one and the config go.
        let mut with = skeleton(
            &["lib/a.ts", "tests/lib.test.ts", "vitest.config.ts"],
            &["tests/a.test.ts"],
        );
        stray_tests_out(&mut with, &NEXTJS);
        assert_eq!(paths(&with.files), ["lib/a.ts"]);
        assert_eq!(paths(&with.test_files), ["tests/a.test.ts"]);
        // It has none: the stray test is its test file.
        let mut without = skeleton(&["lib/a.ts", "tests/lib.test.ts", "jest.config.js"], &[]);
        stray_tests_out(&mut without, &NEXTJS);
        assert_eq!(paths(&without.files), ["lib/a.ts"]);
        assert_eq!(paths(&without.test_files), ["tests/lib.test.ts"]);
        // Only tests, or none: as it is.
        let mut only = skeleton(&["tests/lib.test.ts"], &[]);
        stray_tests_out(&mut only, &NEXTJS);
        assert_eq!(paths(&only.files), ["tests/lib.test.ts"]);
    }

    fn file(path: &str, purpose: &str, exports: &[&str]) -> ManifestFile {
        ManifestFile {
            path: path.to_string(),
            purpose: purpose.to_string(),
            exports: exports.iter().map(|line| line.to_string()).collect(),
            uses: Vec::new(),
        }
    }

    fn manifest() -> Manifest {
        let classes: Vec<String> = (1..=14).map(|n| format!(".c{n}  // class {n}")).collect();
        let classes: Vec<&str> = classes.iter().map(String::as_str).collect();
        Manifest {
            files: vec![
                file("package.json", "Package.", &[]),
                file("app/globals.css", "Styles.", &classes),
                file(
                    "lib/types.ts",
                    "Types.",
                    &[
                        "export interface Task { id: string; title: string }",
                        "export interface Settings { defaultProjectId: string | null }  // defaultProjectId: project the board opens -> app/board/page.tsx",
                        "export interface AppState { tasks: Task[]; settings: Settings }",
                    ],
                ),
                file(
                    "lib/tasks.ts",
                    "Tasks.",
                    &["export function addTask(state: AppState, title: string): AppState"],
                ),
                file(
                    "app/page.tsx",
                    "Home.",
                    &["export default function Home()  // renders <TaskCard/>"],
                ),
                file(
                    "app/board/page.tsx",
                    "Board.",
                    &["export default function BoardPage()"],
                ),
                file(
                    "components/TaskCard.tsx",
                    "Card.",
                    &["export default function TaskCard()"],
                ),
            ],
            test_files: vec![file("tests/all.test.ts", "Tests.", &[])],
            readme: false,
        }
    }

    fn base() -> PlanOptions {
        PlanOptions {
            staged: true,
            split: true,
            ..PlanOptions::default()
        }
    }

    fn plan(options: &PlanOptions) -> Plan {
        plan_from_manifest(OBJECTIVE, &manifest(), 32, Some(&NEXTJS), options).unwrap()
    }

    fn task<'a>(plan: &'a Plan, title: &str) -> &'a Task {
        plan.tasks
            .iter()
            .find(|task| task.title == title)
            .unwrap_or_else(|| panic!("no task {title}"))
    }

    fn shared_prefix(a: &str, b: &str) -> usize {
        a.bytes().zip(b.bytes()).take_while(|(x, y)| x == y).count()
    }

    #[test]
    fn the_options_are_off_by_default() {
        let options = PlanOptions::default();
        assert!(!options.contract && !options.wiring_rules);
        assert!(!options.consumers_see_css && !options.deps_first);
        assert!(!ArchitectOptions::default().contract);
        let plan = plan(&base());
        assert!(plan.tasks.iter().all(|task| task.prefix_split.is_none()));
        assert!(!plan
            .tasks
            .iter()
            .any(|task| task.prompt.contains("Settings this file reads")));
    }

    #[test]
    fn contract_gives_each_reader_its_settings() {
        let off = plan(&base());
        let on = plan(&PlanOptions {
            contract: true,
            ..base()
        });
        let board = &task(&on, "app/board/page.tsx").prompt;
        assert!(
            board.contains(
                "- Settings this file reads (from the app's `Settings`; apply each one, do not \
                 only save it): `defaultProjectId` (project the board opens)"
            ),
            "{board}"
        );
        // Only the reader changes.
        for task in &on.tasks {
            if task.title != "app/board/page.tsx" {
                assert_eq!(
                    task.prompt,
                    self::task(&off, &task.title).prompt,
                    "{}",
                    task.title
                );
            }
        }
    }

    #[test]
    fn wiring_rules_extend_the_prompt_and_leave_the_css_parts_alone() {
        let plain = plan(&PlanOptions {
            wave_wiring: true,
            ..base()
        });
        let wiring = task(&plain, "wiring plan");
        assert!(!wiring.prompt.contains(NEXTJS.staging.quality.wiring_rule));
        // Without the rules the CSS parts (`app/` files) wait for the wiring.
        assert_eq!(task(&plain, "app/styles/part1.css").deps, [WIRING_TASK]);

        let ruled = plan(&PlanOptions {
            wave_wiring: true,
            wiring_rules: true,
            ..base()
        });
        let wiring = &task(&ruled, "wiring plan").prompt;
        assert!(
            wiring.contains(NEXTJS.staging.quality.wiring_rule),
            "{wiring}"
        );
        assert!(
            wiring.contains("- `defaultProjectId` -> app/board/page.tsx"),
            "{wiring}"
        );
        assert!(task(&ruled, "app/styles/part1.css").deps.is_empty());
        let board = task(&ruled, "app/board/page.tsx");
        assert_eq!(board.deps[0], WIRING_TASK);
        assert!(board.deps.contains(&task_id("lib/types.ts")));
        assert!(board.deps.contains(&task_id("lib/tasks.ts")));
    }

    #[test]
    fn consumers_see_the_css_parts() {
        let parts = [
            task_id("app/styles/part1.css"),
            task_id("app/styles/part2.css"),
        ];
        let off = plan(&base());
        let on = plan(&PlanOptions {
            consumers_see_css: true,
            ..base()
        });
        for path in [
            "app/page.tsx",
            "app/board/page.tsx",
            "components/TaskCard.tsx",
        ] {
            let deps = &task(&on, path).deps;
            assert!(
                parts.iter().all(|part| deps.contains(part)),
                "{path}: {deps:?}"
            );
            assert_eq!(deps[..deps.len() - 2], task(&off, path).deps[..], "{path}");
            assert_eq!(task(&on, path).prompt, task(&off, path).prompt, "{path}");
        }
        for path in ["lib/tasks.ts", "lib/types.ts", "app/styles/part1.css"] {
            assert_eq!(task(&on, path).deps, task(&off, path).deps, "{path}");
        }
        for shard in on
            .tasks
            .iter()
            .filter(|task| task.title.starts_with("tests/"))
        {
            assert!(
                !parts.iter().any(|part| shard.deps.contains(part)),
                "{}",
                shard.title
            );
        }
        // Without `split` there are no parts and nothing changes.
        let flat = PlanOptions {
            split: false,
            ..base()
        };
        let plan_off = plan(&flat);
        let plan_on = plan(&PlanOptions {
            consumers_see_css: true,
            ..flat
        });
        for task in &plan_on.tasks {
            assert_eq!(
                task.deps,
                self::task(&plan_off, &task.title).deps,
                "{}",
                task.title
            );
        }
    }

    #[test]
    fn deps_first_marks_the_head_and_siblings_share_the_deps() {
        let off = plan(&base());
        let on = plan(&PlanOptions {
            deps_first: true,
            ..base()
        });
        let board = task(&on, "app/board/page.tsx");
        let at = board.prefix_split.expect("split");
        assert!(board.prompt[at..]
            .starts_with("Now write the COMPLETE contents of `app/board/page.tsx`"));
        assert!(board.prompt[at..].contains(DEPS_ABOVE));
        assert!(!board.prompt.contains(DEPS_BELOW));
        assert_eq!(
            board.prompt.replace(DEPS_ABOVE, DEPS_BELOW),
            task(&off, "app/board/page.tsx").prompt
        );
        assert!(
            task(&on, "package.json").prefix_split.is_none(),
            "rendered files"
        );
        let shard = on
            .tasks
            .iter()
            .find(|task| task.title.starts_with("tests/"))
            .unwrap();
        assert!(shard.prompt[shard.prefix_split.unwrap()..].starts_with("Write a Vitest suite"));

        let outputs: HashMap<String, String> = on
            .tasks
            .iter()
            .map(|task| (task.id.clone(), format!("```ts\n// {}\n```", task.title)))
            .collect();
        let render = |plan: &Plan, title: &str| {
            crate::scheduler::render_task_prompt(task(plan, title), &outputs, 12_000)
        };
        let (a, b) = (
            render(&on, "app/page.tsx"),
            render(&on, "app/board/page.tsx"),
        );
        let tail = a.find("# This task").unwrap();
        assert!(
            shared_prefix(&a, &b) > tail,
            "siblings share the head and the deps"
        );
        assert!(a[..tail].contains("## Output from lib-tasks-ts"));
        // Off: the deps follow the whole prompt.
        let (a, b) = (
            render(&off, "app/page.tsx"),
            render(&off, "app/board/page.tsx"),
        );
        assert!(shared_prefix(&a, &b) < a.find("# Prerequisite outputs").unwrap());
    }

    #[test]
    fn render_sorts_the_deps_and_ignores_a_bad_split() {
        let outputs: HashMap<String, String> = [("b", "B"), ("a", "A")]
            .iter()
            .map(|(id, text)| (id.to_string(), text.to_string()))
            .collect();
        let task = Task {
            id: "t".to_string(),
            title: "t.ts".to_string(),
            prompt: "HEAD\nTAIL".to_string(),
            deps: vec!["b".to_string(), "a".to_string()],
            prefix_split: Some(5),
            ..Task::default()
        };
        let text = crate::scheduler::render_task_prompt(&task, &outputs, 100);
        let (a, b) = (
            text.find("Output from a").unwrap(),
            text.find("Output from b").unwrap(),
        );
        assert!(
            text.starts_with("# Assignment\nHEAD\n\n# Prerequisite outputs"),
            "{text}"
        );
        assert!(
            a < b && b < text.find("# This task\nTAIL").unwrap(),
            "{text}"
        );
        assert!(text.find("TAIL").unwrap() < text.find("Title: t.ts").unwrap());
        let plain = Task {
            prefix_split: None,
            ..task.clone()
        };
        let old = crate::scheduler::render_task_prompt(&plain, &outputs, 100);
        assert!(old.find("Output from b").unwrap() < old.find("Output from a").unwrap());
        // A split past the end of the prompt: rendered as before.
        let bad = Task {
            prefix_split: Some(99),
            ..task.clone()
        };
        assert_eq!(
            crate::scheduler::render_task_prompt(&bad, &outputs, 100),
            old
        );
        // No deps: the split changes nothing.
        let alone = Task {
            deps: Vec::new(),
            ..task.clone()
        };
        let none = Task {
            prefix_split: None,
            ..alone.clone()
        };
        assert_eq!(
            crate::scheduler::render_task_prompt(&alone, &outputs, 100),
            crate::scheduler::render_task_prompt(&none, &outputs, 100)
        );
    }

    /// A NEXTJS planner client: a skeleton of four files, two export
    /// fragments, and repaired fragments when the prompt names problems.
    struct Planner {
        seen: Mutex<Vec<CompletionRequest>>,
        /// The repaired page fragment drops the component's exports (a
        /// repair that makes things worse).
        drop: bool,
    }

    fn reply(text: &str) -> Completion {
        Completion {
            text: text.to_string(),
            provider: None,
            input_tokens: 10,
            output_tokens: 10,
            reasoning_tokens: None,
            cached_input_tokens: None,
            finish_reason: Some("stop".to_string()),
            usage_is_estimated: true,
            elapsed: std::time::Duration::ZERO,
            time_to_first_token: None,
        }
    }

    #[async_trait::async_trait]
    impl ModelClient for Planner {
        async fn complete(&self, request: CompletionRequest) -> Result<Completion> {
            self.seen.lock().unwrap().push(request.clone());
            let user = &request.user;
            if user.contains("paths and one-sentence purposes ONLY") {
                return Ok(reply(
                    r#"{"files":[{"path":"lib/types.ts","purpose":"Types"},{"path":"lib/team.ts","purpose":"Team"},{"path":"app/board/page.tsx","purpose":"Board"},{"path":"components/BoardChart.tsx","purpose":"Chart"}],"test_files":[],"readme":false}"#,
                ));
            }
            let fixed = user.contains("had these problems");
            if user.contains("- `lib/types.ts`: Types") {
                let settings = if fixed {
                    "export interface Settings { theme: string }  // theme: colour scheme -> app/board/page.tsx"
                } else {
                    "export interface Settings { theme: string }"
                };
                let compare = if fixed { "done" } else { "Done" };
                return Ok(reply(&format!(
                    r#"{{"files":[{{"path":"lib/types.ts","purpose":"Types","exports":["export type Status = 'todo' | 'done'","export interface Task {{ id: string; status: Status }}","{settings}","export interface AppState {{ tasks: Task[]; settings: Settings }}"]}},{{"path":"lib/team.ts","purpose":"Team","exports":["export function addTask(tasks: Task[]): Task[]  // status!=='{compare}'"]}}],"test_files":[],"readme":false}}"#
                )));
            }
            let renders = if fixed {
                "  // renders <BoardChart/>"
            } else {
                ""
            };
            let chart = if fixed && self.drop {
                ""
            } else {
                r#""export default function BoardChart()""#
            };
            Ok(reply(&format!(
                r#"{{"files":[{{"path":"app/board/page.tsx","purpose":"Board","exports":["export default function BoardPage(){renders}"]}},{{"path":"components/BoardChart.tsx","purpose":"Chart","exports":[{chart}]}}],"test_files":[],"readme":false}}"#
            )))
        }
    }

    async fn generate(contract: bool) -> (Manifest, Vec<CompletionRequest>) {
        generate_with(contract, false).await
    }

    async fn generate_with(contract: bool, drop: bool) -> (Manifest, Vec<CompletionRequest>) {
        generate_full(contract, drop, false).await
    }

    async fn generate_full(
        contract: bool,
        drop: bool,
        semantics: bool,
    ) -> (Manifest, Vec<CompletionRequest>) {
        let client = Planner {
            seen: Mutex::new(Vec::new()),
            drop,
        };
        let options = ArchitectOptions {
            profile: Some(&NEXTJS),
            contract,
            semantics,
            ..ArchitectOptions::default()
        };
        let manifest = generate_manifest(&client, "obj", 16, &[], &options)
            .await
            .unwrap();
        let seen = client.seen.into_inner().unwrap();
        (manifest, seen)
    }

    fn fragments(seen: &[CompletionRequest]) -> Vec<&CompletionRequest> {
        seen.iter()
            .filter(|request| request.user.contains("Design the interface contracts"))
            .collect()
    }

    #[tokio::test]
    async fn contract_planning_asks_the_owning_fragments_once() {
        let (manifest, seen) = generate(true).await;
        let quality = &NEXTJS.staging.quality;
        assert!(seen.iter().all(|request| request
            .shared_context
            .as_deref()
            .is_some_and(|shared| shared.contains(quality.contract_rule))));
        let fragments = fragments(&seen);
        // Two export fragments, then one repair for each (both own a problem).
        assert_eq!(fragments.len(), 4);
        let (first, repairs) = fragments.split_at(2);
        for request in first {
            assert!(request.user.contains(quality.contract_cap));
            assert!(!request.user.contains("had these problems"));
        }
        let types = first
            .iter()
            .find(|request| request.user.contains("- `lib/types.ts`: Types"))
            .unwrap();
        assert!(types.user.contains(quality.contract_types_note));
        assert!(types.user.contains("- `components/BoardChart.tsx`: Chart"));
        assert!(types.user.contains("under 4600 characters"));
        let page = first
            .iter()
            .find(|request| !request.user.contains("- `lib/types.ts`: Types"))
            .unwrap();
        assert!(!page.user.contains(quality.contract_types_note));
        assert!(page.user.contains("under 2000 characters"));
        for request in repairs {
            assert!(request.user.contains("had these problems"));
        }
        assert!(repairs.iter().any(|request| request
            .user
            .contains("`'Done'` is not a value of `status`")
            && request.user.contains("`Settings.theme` has no reader")));
        assert!(repairs
            .iter()
            .any(|request| request.user.contains("no page renders `BoardChart`")));
        assert_eq!(
            crate::contract::contract_problems(&manifest, &NEXTJS),
            Vec::new(),
            "the repaired manifest passes"
        );
        let board = manifest
            .files
            .iter()
            .find(|file| file.path == "app/board/page.tsx")
            .unwrap();
        assert!(board.exports[0].contains("renders <BoardChart/>"));
    }

    /// A repaired fragment whose sibling (a file with no problem) drops its
    /// exports: only the problem file takes the repair, the sibling keeps
    /// its first answer.
    #[tokio::test]
    async fn a_contract_repair_replaces_only_the_problem_files() {
        let (manifest, seen) = generate_with(true, true).await;
        assert_eq!(fragments(&seen).len(), 4);
        let file = |path: &str| {
            manifest
                .files
                .iter()
                .find(|file| file.path == path)
                .unwrap()
        };
        assert_eq!(
            file("components/BoardChart.tsx").exports,
            ["export default function BoardChart()"]
        );
        assert!(file("app/board/page.tsx").exports[0].contains("renders <BoardChart/>"));
        // The types fragment's repair was kept.
        assert!(file("lib/team.ts").exports[0].contains("'done'"));
        assert_eq!(
            crate::contract::contract_problems(&manifest, &NEXTJS),
            Vec::new()
        );
    }

    /// A repair that drops or renames a declared name of a problem file is
    /// not kept; one that adds and retypes is.
    #[test]
    fn a_repair_must_keep_every_declared_name_of_its_files() {
        let file = |path: &str, exports: &[&str]| ManifestFile {
            path: path.to_string(),
            purpose: "x".to_string(),
            exports: exports.iter().map(|line| line.to_string()).collect(),
            uses: vec!["b.ts:other".to_string()],
        };
        let files = vec![
            file(
                "a.ts",
                &[
                    "export type Mode = 'a' | 'b'",
                    "export function addEntry(): void",
                ],
            ),
            file("b.ts", &["export function other(): void"]),
        ];
        let owned: std::collections::HashSet<String> = ["a.ts".to_string()].into();
        // Renamed: lost.
        let renamed = vec![file(
            "a.ts",
            &[
                "export type Kind = 'a' | 'b'",
                "export function addEntry(): void",
            ],
        )];
        let (_, lost) = repair_candidate(&files, renamed, &owned);
        assert_eq!(lost, ["a.ts: Mode"]);
        // Retyped and grown, the sibling's answer ignored: kept.
        let grown = vec![
            file(
                "a.ts",
                &[
                    "export type Mode = 'a' | 'b' | 'c'",
                    "export function addEntry(mode: Mode): void",
                    "export function dropEntry(): void",
                ],
            ),
            file("b.ts", &[]),
        ];
        let (candidate, lost) = repair_candidate(&files, grown, &owned);
        assert!(lost.is_empty(), "{lost:?}");
        assert_eq!(candidate[0].exports.len(), 3);
        assert_eq!(candidate[0].uses, ["b.ts:other"], "the wired calls stay");
        assert_eq!(candidate[1].exports, ["export function other(): void"]);
    }

    /// After a repair, wired calls of exports that are gone are dropped.
    #[test]
    fn wired_calls_are_normalized_again_after_a_repair() {
        let mut files = vec![
            ManifestFile {
                path: "lib/a.ts".to_string(),
                purpose: "x".to_string(),
                exports: vec!["export function kept(): void".to_string()],
                uses: Vec::new(),
            },
            ManifestFile {
                path: "app/page.tsx".to_string(),
                purpose: "x".to_string(),
                exports: vec!["export default function Page()".to_string()],
                uses: vec!["lib/a.ts:kept".to_string(), "lib/a.ts:gone".to_string()],
            },
        ];
        renormalize_uses(&mut files);
        assert_eq!(files[1].uses, ["lib/a.ts:kept"]);
    }

    /// A function problem the review sent to a file that does not export
    /// the function goes to the one file that does.
    #[test]
    fn a_review_function_problem_goes_to_the_exporting_file() {
        let file = |path: &str, exports: &[&str]| ManifestFile {
            path: path.to_string(),
            purpose: "x".to_string(),
            exports: exports.iter().map(|line| line.to_string()).collect(),
            uses: Vec::new(),
        };
        let files = vec![
            file("lib/types.ts", &["export interface Entry { id: string }"]),
            file(
                "lib/list.ts",
                &["export function addEntry(entries: Entry[]): Entry[]"],
            ),
        ];
        let answer = r#"{"problems":[
            {"kind":"missing_param","path":"lib/types.ts","text":"addEntry must take the limit"},
            {"kind":"two_models","path":"lib/types.ts","text":"refer to an Entry by id"},
            {"kind":"unowned_effect","path":"lib/types.ts","text":"nothing names a function here"}
        ]}"#;
        let parsed = parse_review(answer, &files).unwrap();
        let paths: Vec<&str> = parsed
            .iter()
            .map(|problem| problem.path.as_deref().unwrap())
            .collect();
        assert_eq!(paths, ["lib/list.ts", "lib/types.ts", "lib/types.ts"]);
    }

    #[tokio::test]
    async fn contract_off_plans_as_before() {
        let (manifest, seen) = generate(false).await;
        assert!(seen.iter().all(|request| !request
            .shared_context
            .as_deref()
            .unwrap_or("")
            .contains("App contract")));
        let fragments = fragments(&seen);
        assert_eq!(fragments.len(), 2);
        for request in fragments {
            assert!(request
                .user
                .contains("- At most 4 exports per file, each ONE line"));
            assert!(!request.user.contains("This is the app contract file"));
            assert!(request.user.contains("under 1600 characters"));
        }
        let board = manifest
            .files
            .iter()
            .find(|file| file.path == "app/board/page.tsx")
            .unwrap();
        assert!(!board.exports[0].contains("renders"));
    }

    #[test]
    fn page_local_adds_its_planner_rule_only_when_on() {
        let rule = NEXTJS.staging.quality.page_local_rule;
        assert!(!rule.is_empty());
        let off = architect_shared_context(48, &NEXTJS, true, true, false, false);
        let on = architect_shared_context(48, &NEXTJS, true, true, true, false);
        assert!(!off.contains(rule));
        assert!(on.contains(rule));
        // The rule comes right after the contract rule; the rest is the same.
        assert_eq!(on.replacen(rule, "", 1), off);
        let python = crate::profile::Profile::named("python").unwrap();
        assert_eq!(
            architect_shared_context(48, python, false, false, true, false),
            architect_shared_context(48, python, false, false, false, false)
        );
    }

    #[test]
    fn semantics_adds_its_planner_rule_and_the_page_local_variant() {
        let quality = &NEXTJS.staging.quality;
        let rule = quality.semantics_rule;
        assert!(!rule.is_empty());
        let off = architect_shared_context(48, &NEXTJS, true, true, false, false);
        let on = architect_shared_context(48, &NEXTJS, true, true, false, true);
        assert_eq!(on.replacen(rule, "", 1), off);
        // With `page_local`: the variant with one state owner in its place.
        let local = architect_shared_context(48, &NEXTJS, true, true, true, false);
        let both = architect_shared_context(48, &NEXTJS, true, true, true, true);
        assert!(both.contains(quality.semantic_page_local_rule) && both.contains(rule));
        assert!(!both.contains(quality.page_local_rule));
        assert_eq!(
            both.replacen(quality.semantic_page_local_rule, quality.page_local_rule, 1)
                .replacen(rule, "", 1),
            local
        );
        // The two page-local rules share their first bullets.
        let head = quality.page_local_rule.rsplit_once(" only a").unwrap().0;
        assert!(quality.semantic_page_local_rule.starts_with(head));
        let python = crate::profile::Profile::named("python").unwrap();
        assert_eq!(
            architect_shared_context(48, python, false, false, true, true),
            architect_shared_context(48, python, false, false, true, false)
        );
    }

    /// `semantics` without `contract`: the types fragment sees every file
    /// with the semantics note and cap, and its convention problem goes back
    /// to it once.
    #[tokio::test]
    async fn semantics_planning_asks_the_types_fragment_once() {
        let (_, seen) = generate_full(false, false, true).await;
        let quality = &NEXTJS.staging.quality;
        assert!(seen.iter().all(|request| request
            .shared_context
            .as_deref()
            .is_some_and(|shared| shared.contains(quality.semantics_rule)
                && !shared.contains(quality.contract_rule))));
        let fragments = fragments(&seen);
        assert_eq!(fragments.len(), 3, "two fragments, one repair");
        let types = fragments
            .iter()
            .find(|request| request.user.contains("- `lib/types.ts`: Types"))
            .unwrap();
        assert!(types.user.contains(quality.semantics_types_note));
        assert!(!types.user.contains(quality.contract_types_note));
        assert!(types.user.contains(quality.semantics_cap));
        assert!(types.user.contains("- `components/BoardChart.tsx`: Chart"));
        assert!(types.user.contains("under 4800 characters"));
        let page = fragments
            .iter()
            .find(|request| !request.user.contains("- `lib/types.ts`: Types"))
            .unwrap();
        assert!(page.user.contains(quality.semantics_cap));
        assert!(page.user.contains("under 1600 characters"));
        let repair = fragments[2];
        assert!(repair.user.contains("had these problems"));
        assert!(repair.user.contains("`Status` has no transition owner"));
        // The contract's problems are not asked for.
        assert!(!repair.user.contains("has no reader"));
    }

    /// `semantics` with `contract`: one repair round for both checks.
    #[tokio::test]
    async fn semantics_and_contract_share_one_repair_round() {
        let (_, seen) = generate_full(true, false, true).await;
        let fragments = fragments(&seen);
        assert_eq!(fragments.len(), 4);
        let types = fragments[0..2]
            .iter()
            .find(|request| request.user.contains("- `lib/types.ts`: Types"))
            .unwrap();
        assert!(types
            .user
            .contains(NEXTJS.staging.quality.contract_types_note));
        assert!(!types
            .user
            .contains(NEXTJS.staging.quality.semantics_types_note));
        assert!(types.user.contains("under 5400 characters"));
        assert!(fragments[2..].iter().any(|request| request
            .user
            .contains("`Status` has no transition owner")
            && request.user.contains("`Settings.theme` has no reader")));
    }

    /// A NEXTJS planner for the cross-shard switches: four files (types, a
    /// layerless package.json, a lib module, a page); answers each export
    /// fragment for exactly the files it lists. With `dangling`, the page's
    /// first answer calls an unplanned file; `review` is the plan review's
    /// answer (`None`: the call fails).
    struct Shards {
        seen: Mutex<Vec<CompletionRequest>>,
        dangling: bool,
        review: Option<&'static str>,
    }

    #[async_trait::async_trait]
    impl ModelClient for Shards {
        async fn complete(&self, request: CompletionRequest) -> Result<Completion> {
            self.seen.lock().unwrap().push(request.clone());
            let user = &request.user;
            if user.contains("paths and one-sentence purposes ONLY") {
                return Ok(reply(
                    r#"{"files":[{"path":"lib/types.ts","purpose":"Types"},{"path":"package.json","purpose":"Package"},{"path":"lib/list.ts","purpose":"List"},{"path":"app/page.tsx","purpose":"Page"}],"test_files":[],"readme":false}"#,
                ));
            }
            if user.contains("Review this plan for problems BETWEEN") {
                return match self.review {
                    Some(answer) => Ok(reply(answer)),
                    None => Err(anyhow::anyhow!("review down")),
                };
            }
            let fixed = user.contains("had these problems");
            let listed = user
                .split("These files exist")
                .nth(1)
                .unwrap_or("")
                .split("Fragment discipline")
                .next()
                .unwrap_or("");
            let mut files = Vec::new();
            for line in listed.lines() {
                let Some(path) = line
                    .strip_prefix("- `")
                    .and_then(|rest| rest.split_once('`'))
                    .map(|(path, _)| path)
                else {
                    continue;
                };
                let (purpose, exports) = match path {
                    "lib/types.ts" => (
                        "Types",
                        vec![
                            "export type Mode = 'a' | 'b' | 'c'".to_string(),
                            "export interface Entry { id: string; mode: Mode }".to_string(),
                        ],
                    ),
                    "package.json" => ("Package", vec!["dependencies next react".to_string()]),
                    "lib/list.ts" => (
                        "List",
                        vec![
                            "export function addEntry(entries: Entry[], mode: Mode): Entry[]"
                                .to_string(),
                        ],
                    ),
                    _ => {
                        let call = if self.dangling && !fixed {
                            "adds via lib/extra.ts addExtra"
                        } else {
                            "adds via lib/list.ts addEntry"
                        };
                        (
                            "Page",
                            vec![format!("export default function Page()  // {call}")],
                        )
                    }
                };
                files.push(
                    serde_json::json!({"path": path, "purpose": purpose, "exports": exports}),
                );
            }
            Ok(reply(
                &serde_json::json!({"files": files, "test_files": [], "readme": false}).to_string(),
            ))
        }
    }

    async fn shards(
        options: ArchitectOptions,
        dangling: bool,
        review: Option<&'static str>,
    ) -> (Manifest, Vec<CompletionRequest>) {
        let client = Shards {
            seen: Mutex::new(Vec::new()),
            dangling,
            review,
        };
        let options = ArchitectOptions {
            profile: Some(&NEXTJS),
            ..options
        };
        let manifest = generate_manifest(&client, "obj", 16, &[], &options)
            .await
            .unwrap();
        (manifest, client.seen.into_inner().unwrap())
    }

    /// `skel_compact`: `Shards`, with the compact skeleton prompt answered by
    /// `compact` (an `Err` when `None`).
    struct CompactShards {
        inner: Shards,
        compact: Option<&'static str>,
    }

    const COMPACT_MARK: &str = "as plain text lines";

    #[async_trait::async_trait]
    impl ModelClient for CompactShards {
        async fn complete(&self, request: CompletionRequest) -> Result<Completion> {
            if request.user.contains(COMPACT_MARK) {
                self.inner.seen.lock().unwrap().push(request.clone());
                return match self.compact {
                    // `CUT:`: the answer stopped at the output budget.
                    Some(text) => match text.strip_prefix("CUT:") {
                        Some(cut) => Ok(Completion {
                            finish_reason: Some("length".into()),
                            ..reply(cut)
                        }),
                        None => Ok(reply(text)),
                    },
                    None => Err(anyhow::anyhow!("compact call down")),
                };
            }
            self.inner.complete(request).await
        }
    }

    async fn compact_shards(
        compact: Option<&'static str>,
        on: bool,
    ) -> (
        Manifest,
        Vec<CompletionRequest>,
        Option<crate::skelcompact::SkelReport>,
    ) {
        let client = CompactShards {
            inner: Shards {
                seen: Mutex::new(Vec::new()),
                dangling: false,
                review: None,
            },
            compact,
        };
        let sink = crate::skelcompact::SkelSink::default();
        let options = ArchitectOptions {
            profile: Some(&NEXTJS),
            skel_compact: on.then(|| crate::skelcompact::SkelCompact {
                sink: Some(sink.clone()),
            }),
            ..ArchitectOptions::default()
        };
        let manifest = generate_manifest(&client, "obj", 16, &[], &options)
            .await
            .unwrap();
        let report = sink.lock().unwrap().clone();
        (manifest, client.inner.seen.into_inner().unwrap(), report)
    }

    fn json_skeleton_calls(seen: &[CompletionRequest]) -> usize {
        seen.iter()
            .filter(|request| {
                request.user.contains("Return only this JSON shape")
                    && request.user.contains(tap_marker())
            })
            .count()
    }

    fn tap_marker() -> &'static str {
        "paths and one-sentence purposes ONLY"
    }

    /// The export fragment requests (sorted), which the skeleton shapes.
    fn fragment_requests(seen: &[CompletionRequest]) -> Vec<String> {
        let mut users: Vec<String> = seen
            .iter()
            .filter(|request| !request.user.contains(tap_marker()))
            .map(|request| request.user.clone())
            .collect();
        users.sort();
        users
    }

    const COMPACT_SKELETON: &str =
        "lib/types.ts: Types\npackage.json: Package\nlib/list.ts: List\napp/page.tsx: Page\n";

    /// Off: no compact call; the skeleton request is today's.
    #[tokio::test]
    async fn skel_compact_off_sends_todays_skeleton_only() {
        let (_, seen, report) = compact_shards(Some(COMPACT_SKELETON), false).await;
        assert!(seen
            .iter()
            .all(|request| !request.user.contains(COMPACT_MARK)));
        assert_eq!(json_skeleton_calls(&seen), 1);
        assert!(report.is_none());
    }

    /// On, answered in lines: one compact call replaces the JSON skeleton,
    /// and every later request and the manifest are those of the off run.
    #[tokio::test]
    async fn skel_compact_lines_plan_as_the_json_skeleton_does() {
        let (off_manifest, off_seen, _) = compact_shards(None, false).await;
        let (manifest, seen, report) = compact_shards(Some(COMPACT_SKELETON), true).await;
        let compact: Vec<&CompletionRequest> = seen
            .iter()
            .filter(|request| request.user.contains(COMPACT_MARK))
            .collect();
        assert_eq!(compact.len(), 1);
        assert!(compact[0].response_format.is_none());
        assert_eq!(compact[0].system, FRAGMENT_SYSTEM);
        assert_eq!(json_skeleton_calls(&seen), 0, "no JSON skeleton call");
        assert_eq!(fragment_requests(&seen), fragment_requests(&off_seen));
        assert_eq!(
            serde_json::to_string(&manifest).unwrap(),
            serde_json::to_string(&off_manifest).unwrap()
        );
        let report = report.unwrap();
        assert_eq!(report.format, crate::skelcompact::SkelFormat::Lines);
        assert_eq!(report.files, 4);
        assert_eq!(report.chars, COMPACT_SKELETON.len());
        assert!(report.reason.is_empty());
    }

    /// On, answered in JSON anyway: read as today's skeleton, no fallback.
    #[tokio::test]
    async fn skel_compact_reads_a_json_answer() {
        let json = r#"{"files":[{"path":"lib/types.ts","purpose":"Types"},{"path":"package.json","purpose":"Package"},{"path":"lib/list.ts","purpose":"List"},{"path":"app/page.tsx","purpose":"Page"}],"test_files":[],"readme":false}"#;
        let (_, off_seen, _) = compact_shards(None, false).await;
        let (_, seen, report) = compact_shards(Some(json), true).await;
        assert_eq!(json_skeleton_calls(&seen), 0);
        assert_eq!(fragment_requests(&seen), fragment_requests(&off_seen));
        assert_eq!(report.unwrap().format, crate::skelcompact::SkelFormat::Json);
    }

    /// On, answered with garbage or failed: today's skeleton request follows,
    /// byte-identical to the off run's, and the plan is the off run's.
    #[tokio::test]
    async fn skel_compact_garbage_or_failure_falls_back_to_todays_request() {
        let (off_manifest, off_seen, _) = compact_shards(None, false).await;
        let off_skeleton = off_seen
            .iter()
            .find(|request| request.user.contains(tap_marker()))
            .unwrap()
            .clone();
        for answer in [
            Some("Sure! Here is the plan.\nlib/types.ts: Types"),
            None,
            // Cut at the budget: the lines before the cut parse, but files
            // after it would be lost.
            Some("CUT:lib/types.ts: Types\npackage.json: Package\nlib/list.ts: List\n"),
        ] {
            let (manifest, seen, report) = compact_shards(answer, true).await;
            let skeletons: Vec<&CompletionRequest> = seen
                .iter()
                .filter(|request| {
                    request.user.contains(tap_marker()) && !request.user.contains(COMPACT_MARK)
                })
                .collect();
            assert_eq!(skeletons.len(), 1, "{answer:?}");
            assert_eq!(skeletons[0].user, off_skeleton.user);
            assert_eq!(skeletons[0].system, off_skeleton.system);
            assert_eq!(skeletons[0].max_tokens, off_skeleton.max_tokens);
            assert_eq!(fragment_requests(&seen), fragment_requests(&off_seen));
            assert_eq!(
                serde_json::to_string(&manifest).unwrap(),
                serde_json::to_string(&off_manifest).unwrap()
            );
            let report = report.unwrap();
            assert_eq!(report.format, crate::skelcompact::SkelFormat::Fallback);
            assert!(!report.reason.is_empty());
        }
    }

    const TYPES_BLOCK: &str = "The shared types are already planned (`lib/types.ts` exports)";

    /// `types_first`: the types fragment alone first (package.json beside
    /// it and round two); the other fragments then carry the planned types
    /// after the objective.
    #[tokio::test]
    async fn types_first_shards_see_the_planned_types() {
        let options = ArchitectOptions {
            types_first: true,
            ..ArchitectOptions::default()
        };
        let (manifest, seen) = shards(options, false, None).await;
        let fragments = fragments(&seen);
        assert_eq!(fragments.len(), 3, "types, package.json, then list + page");
        let find = |path: &str| {
            seen.iter()
                .position(|request| request.user.contains(&format!("- `{path}`: ")))
                .unwrap()
        };
        let (types, package, list) = (
            find("lib/types.ts"),
            find("package.json"),
            find("lib/list.ts"),
        );
        assert_eq!(find("app/page.tsx"), list, "list and page share a fragment");
        // Round two waits for the types only; package.json runs beside.
        assert!(types < list, "the types are planned first");
        assert!(package != list);
        assert!(!seen[types].user.contains(TYPES_BLOCK));
        assert!(!seen[types].user.contains("- `package.json`"));
        assert!(!seen[package].user.contains(TYPES_BLOCK));
        let second = &seen[list].user;
        assert!(second.contains(TYPES_BLOCK));
        assert!(second.contains("- export type Mode = 'a' | 'b' | 'c'"));
        assert!(second.contains("- export interface Entry { id: string; mode: Mode }"));
        // Right after the objective, before the listing.
        assert!(second.contains(&format!("</objective>\n\n{TYPES_BLOCK}")));
        assert!(second.find(TYPES_BLOCK) < second.find("These files exist"));
        // The files keep the skeleton's order.
        let paths: Vec<&str> = manifest
            .files
            .iter()
            .map(|file| file.path.as_str())
            .collect();
        assert_eq!(
            paths,
            [
                "lib/types.ts",
                "package.json",
                "lib/list.ts",
                "app/page.tsx"
            ]
        );
    }

    /// `types_first` with the contract: the types file, planned alone, is
    /// told that the every-file list is for reference only (without it the
    /// model planned every listed file and ran past the output cap).
    #[tokio::test]
    async fn the_lone_types_fragment_plans_only_the_types() {
        // The mock answers every path a prompt lists, the reference list
        // included, so only the prompts are checked here, not the plan.
        let prompts = |options: ArchitectOptions| async move {
            let client = Shards {
                seen: Mutex::new(Vec::new()),
                dangling: false,
                review: None,
            };
            let options = ArchitectOptions {
                profile: Some(&NEXTJS),
                ..options
            };
            let _ = generate_manifest(&client, "obj", 16, &[], &options).await;
            client.seen.into_inner().unwrap()
        };
        let types_request = |seen: &[CompletionRequest]| {
            fragments(seen)
                .into_iter()
                .find(|request| request.user.contains("- `lib/types.ts`: "))
                .unwrap()
                .user
                .clone()
        };
        let seen = prompts(ArchitectOptions {
            types_first: true,
            contract: true,
            ..ArchitectOptions::default()
        })
        .await;
        assert!(types_request(&seen)
            .contains("(Reference only: plan the exports of `lib/types.ts` and nothing else"));
        let seen = prompts(ArchitectOptions {
            contract: true,
            ..ArchitectOptions::default()
        })
        .await;
        assert!(!types_request(&seen).contains("Reference only"));
    }

    /// `types_first` off: the skeleton's pairs in one round, no types block.
    #[tokio::test]
    async fn types_first_off_keeps_the_pairs() {
        let (_, seen) = shards(ArchitectOptions::default(), false, None).await;
        let fragments = fragments(&seen);
        assert_eq!(fragments.len(), 2);
        assert!(fragments
            .iter()
            .any(|request| request.user.contains("- `lib/types.ts`: ")
                && request.user.contains("- `package.json`: ")));
        assert!(fragments.iter().all(|request| !request
            .user
            .contains("The shared types are already planned")
            && request.user.contains("</objective>\n\nThese files exist")));
    }

    /// A NEXTJS planner for `layered`: types, a layerless package.json,
    /// `core` lib modules, a page and a component; answers each export
    /// fragment for exactly the files it lists (a lib module `lib/mN.ts`
    /// exports `fnN(entry: Entry, limit: number)` with a long comment).
    struct Layers {
        seen: Mutex<Vec<CompletionRequest>>,
        /// "page asked" / "seed done", in the order they happened.
        events: Mutex<Vec<&'static str>>,
        core: usize,
        /// The skeleton has `lib/types.ts`.
        types: bool,
        /// The skeleton has the page and the component.
        ui: bool,
        /// The skeleton has `lib/seed.ts` (Data layer); `slow_seed` makes
        /// its fragment answer late.
        seed: bool,
        slow_seed: bool,
        /// First answers of `lib/m0.ts` and the component call an unplanned
        /// file (`references`); a repair answer does not.
        dangling: bool,
    }

    impl Layers {
        fn new(core: usize) -> Self {
            Layers {
                seen: Mutex::new(Vec::new()),
                events: Mutex::new(Vec::new()),
                core,
                types: true,
                ui: true,
                seed: false,
                slow_seed: false,
                dangling: false,
            }
        }
    }

    #[async_trait::async_trait]
    impl ModelClient for Layers {
        async fn complete(&self, request: CompletionRequest) -> Result<Completion> {
            self.seen.lock().unwrap().push(request.clone());
            let user = &request.user;
            if user.contains("paths and one-sentence purposes ONLY") {
                let mut files = Vec::new();
                if self.types {
                    files.push(serde_json::json!({"path": "lib/types.ts", "purpose": "Types"}));
                }
                files.push(serde_json::json!({"path": "package.json", "purpose": "Package"}));
                for index in 0..self.core {
                    files.push(serde_json::json!({
                        "path": format!("lib/m{index}.ts"),
                        "purpose": format!("Module {index}")
                    }));
                }
                if self.seed {
                    files.push(serde_json::json!({"path": "lib/seed.ts", "purpose": "Seed data"}));
                }
                if self.ui {
                    files.push(serde_json::json!({"path": "app/page.tsx", "purpose": "Page"}));
                    files.push(serde_json::json!({
                        "path": "components/Limit.tsx",
                        "purpose": "Owns the limit input"
                    }));
                }
                return Ok(reply(
                    &serde_json::json!({"files": files, "test_files": [], "readme": false})
                        .to_string(),
                ));
            }
            let fixed = user.contains("had these problems");
            let dangling = self.dangling && !fixed;
            if user.contains("- `app/page.tsx`: ") {
                self.events.lock().unwrap().push("page asked");
            }
            if self.slow_seed && user.contains("- `lib/seed.ts`: ") {
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                self.events.lock().unwrap().push("seed done");
            }
            let listed = user
                .split("These files exist")
                .nth(1)
                .unwrap_or("")
                .split("Fragment discipline")
                .next()
                .unwrap_or("");
            let mut files = Vec::new();
            for line in listed.lines() {
                let Some((path, purpose)) = line
                    .strip_prefix("- `")
                    .and_then(|rest| rest.split_once("`: "))
                else {
                    continue;
                };
                let exports = match path {
                    "lib/types.ts" => {
                        vec!["export interface Entry { id: string; size: number }".to_string()]
                    }
                    "package.json" => vec!["dependencies next react".to_string()],
                    "lib/seed.ts" => vec!["export const seedEntries: Entry[]".to_string()],
                    _ if path.starts_with("lib/m") => {
                        let index = &path["lib/m".len()..path.len() - ".ts".len()];
                        let call = if dangling && index == "0" {
                            "; adds via lib/extra.ts addExtra"
                        } else {
                            ""
                        };
                        vec![format!(
                            "export function fn{index}(entry: Entry, limit: number): number  // \
                             weeks start on Monday{call}; {}",
                            "the long explanation of the rule ".repeat(8)
                        )]
                    }
                    "components/Limit.tsx" if dangling => vec![
                        "export default function View()  // reads via lib/ghost.ts useGhost"
                            .to_string(),
                    ],
                    _ => vec!["export default function View()".to_string()],
                };
                files.push(
                    serde_json::json!({"path": path, "purpose": purpose, "exports": exports}),
                );
            }
            Ok(reply(
                &serde_json::json!({"files": files, "test_files": [], "readme": false}).to_string(),
            ))
        }
    }

    async fn layers(options: ArchitectOptions, core: usize) -> (Manifest, Vec<CompletionRequest>) {
        let (manifest, seen, _) = layers_with(Layers::new(core), options).await;
        (manifest, seen)
    }

    async fn layers_with(
        client: Layers,
        options: ArchitectOptions,
    ) -> (Manifest, Vec<CompletionRequest>, Vec<&'static str>) {
        let options = ArchitectOptions {
            profile: Some(&NEXTJS),
            ..options
        };
        let manifest = generate_manifest(&client, "obj", 32, &[], &options)
            .await
            .unwrap();
        (
            manifest,
            client.seen.into_inner().unwrap(),
            client.events.into_inner().unwrap(),
        )
    }

    const CORE_BLOCK: &str = "The core modules are already planned";
    const CONSUMER_REFERENCE: &str = "Every file planned in this pass";

    /// `layered`: types, then the lib modules in pairs with the types, then
    /// the page and component with the types, the core exports and the
    /// reference list; the plan keeps the skeleton's order.
    #[tokio::test]
    async fn layered_plans_types_then_core_then_the_rest() {
        let options = ArchitectOptions {
            layered: true,
            ..ArchitectOptions::default()
        };
        let (manifest, seen) = layers(options, 3).await;
        let fragments = fragments(&seen);
        // types, package.json, two core pairs, one page + component pair.
        assert_eq!(fragments.len(), 5);
        let find = |path: &str| {
            seen.iter()
                .position(|request| request.user.contains(&format!("- `{path}`: ")))
                .unwrap()
        };
        let (types, core_a, core_b, page) = (
            find("lib/types.ts"),
            find("lib/m0.ts"),
            find("lib/m2.ts"),
            find("app/page.tsx"),
        );
        assert_eq!(find("lib/m1.ts"), core_a, "core files in pairs");
        assert_ne!(core_a, core_b);
        assert_eq!(find("components/Limit.tsx"), page);
        assert!(types < core_a && types < core_b, "types before core");
        assert!(core_a < page && core_b < page, "core before the rest");
        for core in [core_a, core_b] {
            let user = &seen[core].user;
            assert!(user.contains(&format!("</objective>\n\n{TYPES_BLOCK}")));
            assert!(!user.contains(CORE_BLOCK));
            assert!(!user.contains(CONSUMER_REFERENCE));
            assert_eq!(seen[core].max_tokens, 2_560);
            // Every round-two file's job, then this pair's own paths.
            assert!(user.contains(CORE_REFERENCE));
            assert!(user.contains(
                "* `lib/m0.ts`: Module 0\n* `lib/m1.ts`: Module 1\n* `lib/m2.ts`: Module 2\n"
            ));
            assert!(user.find(CORE_REFERENCE) < user.find("These files exist"));
            assert!(!user.contains("* `app/page.tsx`"));
        }
        assert!(seen[core_a].user.contains(
            "(Reference only: plan the exports of `lib/m0.ts`, `lib/m1.ts` and nothing else; \
             the other files in the reference list are planned by other passes.)"
        ));
        assert!(seen[core_b]
            .user
            .contains("(Reference only: plan the exports of `lib/m2.ts` and nothing else;"));
        // The two pairs share the prompt up to their listings.
        let head = |user: &str| user[..user.find("These files exist").unwrap()].to_string();
        assert_eq!(head(&seen[core_a].user), head(&seen[core_b].user));
        let ui = &seen[page].user;
        assert!(ui.contains(&format!("</objective>\n\n{TYPES_BLOCK}")));
        assert!(ui.contains(CORE_BLOCK));
        assert!(
            ui.contains("`lib/m2.ts`:\n- export function fn2(entry: Entry, limit: number): number")
        );
        // The long comment keeps its first clause (the convention) only.
        assert!(ui.contains("number  // weeks start on Monday\n"));
        assert!(!ui.contains("the long explanation"));
        assert!(ui.contains(CONSUMER_REFERENCE));
        assert!(ui.contains("* `components/Limit.tsx`: Owns the limit input"));
        assert!(ui.contains("Do not re-plan a job that a file in the reference list owns"));
        assert!(!ui.contains(CORE_REFERENCE));
        // The page's own paths, not the reference list's.
        assert!(ui.contains(
            "(Reference only: plan the exports of `app/page.tsx`, `components/Limit.tsx` and \
             nothing else;"
        ));
        assert!(ui.find(CORE_BLOCK) < ui.find("These files exist"));
        assert!(ui.find(CONSUMER_REFERENCE) < ui.find("These files exist"));
        let paths: Vec<&str> = manifest
            .files
            .iter()
            .map(|file| file.path.as_str())
            .collect();
        assert_eq!(
            paths,
            [
                "lib/types.ts",
                "package.json",
                "lib/m0.ts",
                "lib/m1.ts",
                "lib/m2.ts",
                "app/page.tsx",
                "components/Limit.tsx"
            ]
        );
        // The comment is kept in the plan itself.
        assert!(manifest.files[2].exports[0].contains("the long explanation"));
    }

    /// `core_whole`: the lib modules in one fragment with a budget and a
    /// character limit scaled by its file count; past `CORE_WHOLE_MAX` files
    /// the fewest fragments that fit.
    #[tokio::test]
    async fn core_whole_plans_the_core_in_one_fragment() {
        let options = ArchitectOptions {
            layered: true,
            core_whole: true,
            ..ArchitectOptions::default()
        };
        let (_, seen) = layers(options.clone(), 6).await;
        let core: Vec<&CompletionRequest> = fragments(&seen)
            .into_iter()
            .filter(|request| request.user.contains("- `lib/m0.ts`: "))
            .collect();
        assert_eq!(core.len(), 1);
        assert!(core[0].user.contains("- `lib/m5.ts`: "));
        assert!(core[0].user.contains("ONLY the 6 file(s)"));
        assert_eq!(core[0].max_tokens, fragment_budget(6));
        assert_eq!(fragment_budget(6), 4_200);
        let pair = fragments(&seen)
            .into_iter()
            .find(|request| request.user.contains("- `app/page.tsx`: "))
            .unwrap();
        let limit = |user: &str| -> usize {
            user.split("stay under ")
                .nth(1)
                .unwrap()
                .split(' ')
                .next()
                .unwrap()
                .parse()
                .unwrap()
        };
        assert_eq!(limit(&core[0].user), limit(&pair.user) * 3);
        assert_eq!(pair.max_tokens, 2_560);
        // 20 lib modules: three fragments of 7, 7 and 6, each told the
        // jobs of the others.
        let (_, seen) = layers(options, 20).await;
        let core: Vec<&CompletionRequest> = fragments(&seen)
            .into_iter()
            .filter(|request| request.user.contains("- `lib/m"))
            .collect();
        assert_eq!(core.len(), 3);
        let sizes: Vec<bool> = core
            .iter()
            .map(|request| request.user.contains("ONLY the 7 file(s)"))
            .collect();
        assert_eq!(sizes.iter().filter(|&&seven| seven).count(), 2);
        assert!(core
            .iter()
            .all(|request| request.user.contains("* `lib/m19.ts`: Module 19")));
        assert_eq!(core[0].max_tokens, fragment_budget(7));
        assert_eq!(fragment_budget(7), 4_900);
        assert_eq!(fragment_budget(40), 8_192);
        assert_eq!(medium_budget(2_560), 4_096);
        assert_eq!(medium_budget(8_192), 12_288);
    }

    /// `layered` off: no core block, no reference list, the lib modules in
    /// the same round as the page.
    #[tokio::test]
    async fn layered_off_leaves_the_prompts_unchanged() {
        for options in [
            ArchitectOptions::default(),
            ArchitectOptions {
                types_first: true,
                core_whole: true,
                ..ArchitectOptions::default()
            },
        ] {
            let (_, seen) = layers(options, 3).await;
            assert!(fragments(&seen)
                .iter()
                .all(|request| !request.user.contains(CORE_BLOCK)
                    && !request.user.contains(CONSUMER_REFERENCE)
                    && !request.user.contains(CORE_REFERENCE)
                    && !request
                        .user
                        .contains("the other files in the reference list")
                    && request.max_tokens == 2_560));
        }
    }

    /// `layered` + `core_whole` + `references`: a dangling call in the
    /// whole-core fragment and one in a round-three fragment each go back
    /// to their own fragment, with its own first prompt and budget.
    #[tokio::test]
    async fn a_layered_repair_asks_each_fragment_with_its_own_prompt() {
        let client = Layers {
            dangling: true,
            ..Layers::new(6)
        };
        let options = ArchitectOptions {
            layered: true,
            core_whole: true,
            references: true,
            ..ArchitectOptions::default()
        };
        let (manifest, seen, _) = layers_with(client, options).await;
        let all = fragments(&seen);
        let repairs: Vec<&CompletionRequest> = all
            .iter()
            .copied()
            .filter(|request| request.user.contains("had these problems"))
            .collect();
        assert_eq!(repairs.len(), 2, "one repair per owning fragment");
        let original = |repair: &CompletionRequest| -> CompletionRequest {
            all.iter()
                .copied()
                .find(|request| {
                    !request.user.contains("had these problems")
                        && repair.user.starts_with(&format!("{}\n", request.user))
                })
                .cloned()
                .unwrap()
        };
        let core_repair = repairs
            .iter()
            .find(|request| {
                request
                    .user
                    .contains("- `lib/m0.ts`: `lib/extra.ts` is not a planned file")
            })
            .unwrap();
        let core_first = original(core_repair);
        assert!(core_first.user.contains("ONLY the 6 file(s)"));
        assert!(core_first.user.contains(CORE_REFERENCE));
        assert!(!core_first.user.contains(CORE_BLOCK));
        assert_eq!(core_first.max_tokens, fragment_budget(6));
        assert_eq!(core_repair.max_tokens, fragment_budget(6));
        let ui_repair = repairs
            .iter()
            .find(|request| {
                request
                    .user
                    .contains("- `components/Limit.tsx`: `lib/ghost.ts` is not a planned file")
            })
            .unwrap();
        let ui_first = original(ui_repair);
        assert!(ui_first.user.contains(CORE_BLOCK));
        assert!(ui_first.user.contains(CONSUMER_REFERENCE));
        assert!(ui_first.user.contains(
            "(Reference only: plan the exports of `app/page.tsx`, `components/Limit.tsx`"
        ));
        assert_eq!(ui_first.max_tokens, 2_560);
        assert_eq!(ui_repair.max_tokens, 2_560);
        assert_eq!(crate::references::dangling_problems(&manifest), Vec::new());
    }

    /// `layered`: a Data-layer file is planned in round two (with the
    /// round-two reference list), and round three does not wait for it.
    #[tokio::test]
    async fn a_seed_file_plans_in_round_two_without_holding_round_three() {
        let client = Layers {
            seed: true,
            slow_seed: true,
            ..Layers::new(2)
        };
        let options = ArchitectOptions {
            layered: true,
            ..ArchitectOptions::default()
        };
        let (manifest, seen, events) = layers_with(client, options).await;
        let find = |path: &str| {
            seen.iter()
                .position(|request| request.user.contains(&format!("- `{path}`: ")))
                .unwrap()
        };
        let (core, seed, page) = (find("lib/m0.ts"), find("lib/seed.ts"), find("app/page.tsx"));
        assert_ne!(core, seed, "the seed is not paired with the core");
        let seed_user = &seen[seed].user;
        assert!(seed_user.contains(TYPES_BLOCK));
        assert!(seed_user.contains(CORE_REFERENCE));
        assert!(seed_user.contains("* `lib/m0.ts`: Module 0"));
        assert!(seed_user.contains("* `lib/seed.ts`: Seed data"));
        assert!(!seed_user.contains(CORE_BLOCK));
        assert!(seen[core].user.contains("* `lib/seed.ts`: Seed data"));
        // The page is asked while the seed is still planning, so its core
        // block has the core only.
        assert_eq!(events, ["page asked", "seed done"]);
        assert!(seen[page].user.contains(CORE_BLOCK));
        assert!(!seen[page].user.contains("seedEntries"));
        let paths: Vec<&str> = manifest
            .files
            .iter()
            .map(|file| file.path.as_str())
            .collect();
        assert_eq!(
            paths,
            [
                "lib/types.ts",
                "package.json",
                "lib/m0.ts",
                "lib/m1.ts",
                "lib/seed.ts",
                "app/page.tsx",
                "components/Limit.tsx"
            ]
        );
        assert_eq!(
            manifest.files[4].exports,
            ["export const seedEntries: Entry[]"]
        );
    }

    /// `layered` with no Types-layer file: the core still plans before the
    /// rest (no types block); with no consumer file: two rounds, no core
    /// block and no consumer reference.
    #[tokio::test]
    async fn layered_without_types_or_consumers() {
        let options = ArchitectOptions {
            layered: true,
            ..ArchitectOptions::default()
        };
        let client = Layers {
            types: false,
            ..Layers::new(3)
        };
        let (manifest, seen, _) = layers_with(client, options.clone()).await;
        assert_eq!(
            fragments(&seen).len(),
            4,
            "package.json, two core pairs, the ui pair"
        );
        let find = |path: &str| {
            seen.iter()
                .position(|request| request.user.contains(&format!("- `{path}`: ")))
                .unwrap()
        };
        let (core, page) = (find("lib/m0.ts"), find("app/page.tsx"));
        assert!(core < page);
        assert!(!seen[core].user.contains(TYPES_BLOCK));
        assert!(seen[core].user.contains(CORE_REFERENCE));
        assert!(seen[page].user.contains(CORE_BLOCK));
        assert!(seen[page].user.contains(CONSUMER_REFERENCE));
        assert_eq!(manifest.files.len(), 6);

        let client = Layers {
            ui: false,
            ..Layers::new(3)
        };
        let (manifest, seen, _) = layers_with(client, options).await;
        let fragments = fragments(&seen);
        assert_eq!(fragments.len(), 4, "types, package.json, two core pairs");
        assert!(fragments
            .iter()
            .all(|request| !request.user.contains(CORE_BLOCK)
                && !request.user.contains(CONSUMER_REFERENCE)));
        assert!(fragments
            .iter()
            .filter(|request| request.user.contains("- `lib/m"))
            .all(|request| request.user.contains(CORE_REFERENCE)));
        let paths: Vec<&str> = manifest
            .files
            .iter()
            .map(|file| file.path.as_str())
            .collect();
        assert_eq!(
            paths,
            [
                "lib/types.ts",
                "package.json",
                "lib/m0.ts",
                "lib/m1.ts",
                "lib/m2.ts"
            ]
        );
    }

    fn core_file(path: &str, exports: Vec<String>) -> ManifestFile {
        ManifestFile {
            path: path.to_string(),
            purpose: "Core".to_string(),
            exports,
            uses: Vec::new(),
        }
    }

    /// The comment marker inside a string literal is not a comment.
    #[test]
    fn a_comment_starts_outside_string_literals() {
        let tail = |export: &'static str, comment: &str| {
            comment_start(export, comment).map(|at| &export[at..])
        };
        assert_eq!(
            tail(
                r#"export const API = "https://x.dev/api"  // base url"#,
                "//"
            ),
            Some("// base url")
        );
        assert_eq!(
            tail(r##"ACCENT = "#fff"  # accent colour"##, "#"),
            Some("# accent colour")
        );
        assert_eq!(
            tail(r#"export const API = "https://x.dev/api""#, "//"),
            None
        );
        assert_eq!(tail("export const LINK = `a // b`", "//"), None);
        assert_eq!(
            with_short_comment(
                r#"export const API = "https://x.dev/api"  // base url; read once at start"#,
                "//",
                80
            ),
            r#"export const API = "https://x.dev/api"  // base url"#
        );
    }

    /// `core_block`: a long export keeps its comment's first clause; a big
    /// core keeps the clauses shorter, then names the files past the cap by
    /// path only.
    #[test]
    fn core_block_keeps_the_conventions_under_its_cap() {
        let staging = &NEXTJS.staging;
        let long = format!(
            "export function weekOf(date: Date): string  // weeks start on Monday; {}",
            "more detail ".repeat(20)
        );
        let block = core_block(&[core_file("lib/week.ts", vec![long])], staging, "//");
        assert!(block
            .contains("- export function weekOf(date: Date): string  // weeks start on Monday\n"));
        assert!(!block.contains("more detail"));
        // A clause with no mark is cut with an ellipsis.
        let long = format!(
            "export function signOf(amount: number): number  // {}",
            "negative means money out ".repeat(10)
        );
        let block = core_block(&[core_file("lib/sign.ts", vec![long])], staging, "//");
        assert!(block.contains("// negative means money out"));
        assert!(block.contains("…\n"));

        let files: Vec<ManifestFile> = (0..15)
            .map(|file| {
                core_file(
                    &format!("lib/module{file}.ts"),
                    (0..4)
                        .map(|export| {
                            format!(
                                "export function helper{file}x{export}(entry: Entry, limit: number, \
                                 options: {{ strict: boolean; fallback: number }}): number  // \
                                 weeks start on Monday; {}",
                                "detail ".repeat(20)
                            )
                        })
                        .collect(),
                )
            })
            .collect();
        let block = core_block(&files, staging, "//");
        assert!(block.len() < CORE_BLOCK_CHARS + 1_000, "{}", block.len());
        assert!(block.contains("// weeks start on Monday\n"));
        assert!(block.contains("more core files, paths only: "));
        assert!(block.contains("`lib/module14.ts`"));
        assert!(!block.contains("detail"));
    }

    /// `consumer_reference`: purposes are cut and past the cap the files
    /// are named by path only.
    #[test]
    fn the_reference_list_is_bounded() {
        let round: Vec<Vec<ManifestFile>> = (0..40)
            .map(|index| {
                vec![ManifestFile {
                    path: format!("components/Widget{index}.tsx"),
                    purpose: "renders the widget and owns its controls ".repeat(4),
                    exports: Vec::new(),
                    uses: Vec::new(),
                }]
            })
            .collect();
        let reference = consumer_reference(&round);
        assert!(
            reference.len() < REFERENCE_CHARS + 1_600,
            "{}",
            reference.len()
        );
        assert!(!reference.contains(&"renders the widget and owns its controls ".repeat(3)));
        assert!(reference.contains("* `components/Widget0.tsx`: renders the widget"));
        assert!(reference.contains("* also: "));
        assert!(reference.contains("`components/Widget39.tsx`"));
    }

    /// `references`: a page that calls an unplanned file is asked once more
    /// with the problem (its fragment only), and the repaired plan is clean.
    #[tokio::test]
    async fn a_dangling_reference_goes_back_to_the_referring_fragment() {
        let options = ArchitectOptions {
            references: true,
            ..ArchitectOptions::default()
        };
        let (manifest, seen) = shards(options, true, None).await;
        let repairs: Vec<&CompletionRequest> = fragments(&seen)
            .into_iter()
            .filter(|request| request.user.contains("had these problems"))
            .collect();
        assert_eq!(repairs.len(), 1, "one repair, for the page's fragment");
        assert!(repairs[0]
            .user
            .contains("- `app/page.tsx`: `lib/extra.ts` is not a planned file"));
        assert!(repairs[0].user.contains("- `lib/list.ts`: List"));
        assert_eq!(crate::references::dangling_problems(&manifest), Vec::new());
        let page = manifest
            .files
            .iter()
            .find(|file| file.path == "app/page.tsx")
            .unwrap();
        assert!(page.exports[0].contains("via lib/list.ts addEntry"));
        // A real export is no problem: no repair at all.
        let options = ArchitectOptions {
            references: true,
            ..ArchitectOptions::default()
        };
        let (_, seen) = shards(options, false, None).await;
        assert_eq!(fragments(&seen).len(), 2);
        // Off: the dangling call is not checked.
        let (manifest, seen) = shards(ArchitectOptions::default(), true, None).await;
        assert_eq!(fragments(&seen).len(), 2);
        assert_eq!(crate::references::dangling_problems(&manifest).len(), 1);
    }

    /// `review`: the answer's valid problems reach the owning fragment's
    /// repair; one for an unplanned path is dropped.
    #[tokio::test]
    async fn the_plan_review_answer_goes_to_the_repair_round() {
        let answer = r#"{"problems":[
            {"kind":"missing_param","path":"lib/list.ts","text":"addEntry must take the mode limit stated in the objective"},
            {"kind":"two_models","path":"`app/page.tsx`","text":"refer to an entry by its id"},
            {"kind":"missing_param","path":"lib/nowhere.ts","text":"not a planned file"}
        ]}"#;
        let options = ArchitectOptions {
            review: true,
            ..ArchitectOptions::default()
        };
        let (_, seen) = shards(options, false, Some(answer)).await;
        let reviews: Vec<&CompletionRequest> = seen
            .iter()
            .filter(|request| {
                request
                    .user
                    .contains("Review this plan for problems BETWEEN")
            })
            .collect();
        assert_eq!(reviews.len(), 1);
        assert!(reviews[0]
            .user
            .contains("- `lib/list.ts`: List | export function addEntry"));
        assert_eq!(reviews[0].max_tokens, REVIEW_MAX_TOKENS);
        assert!(reviews[0]
            .user
            .contains("at most 6 problems, at most 2 per file"));
        assert!(reviews[0].user.contains("never a new file"));
        assert!(reviews[0].response_format.is_some());
        let repairs: Vec<&CompletionRequest> = fragments(&seen)
            .into_iter()
            .filter(|request| request.user.contains("had these problems"))
            .collect();
        assert_eq!(repairs.len(), 1);
        let user = &repairs[0].user;
        assert!(user.contains(
            "- `lib/list.ts`: missing_param: addEntry must take the mode limit stated in the objective"
        ));
        assert!(user.contains("- `app/page.tsx`: two_models: refer to an entry by its id"));
        assert!(!user.contains("nowhere"));
    }

    /// `review`: a garbage answer or a failed call changes nothing; off, no
    /// review is asked.
    #[tokio::test]
    async fn a_garbage_or_failed_review_changes_nothing() {
        let (plain, seen) = shards(ArchitectOptions::default(), false, None).await;
        assert!(seen.iter().all(|request| !request
            .user
            .contains("Review this plan for problems BETWEEN")));
        for review in [
            Some("I think the plan looks fine overall."),
            Some("{\"problems\": 3}"),
            None,
        ] {
            let options = ArchitectOptions {
                review: true,
                ..ArchitectOptions::default()
            };
            let (manifest, seen) = shards(options, false, review).await;
            assert_eq!(fragments(&seen).len(), 2, "{review:?}: no repair");
            assert_eq!(
                serde_json::to_string(&manifest).unwrap(),
                serde_json::to_string(&plain).unwrap()
            );
        }
    }

    #[test]
    fn the_review_answer_is_filtered_deduplicated_and_capped() {
        let files: Vec<ManifestFile> = ["a.ts", "b.ts"]
            .iter()
            .map(|path| ManifestFile {
                path: path.to_string(),
                purpose: "x".to_string(),
                exports: Vec::new(),
                uses: Vec::new(),
            })
            .collect();
        let item = |kind: &str, path: &str, text: &str| serde_json::json!({"kind": kind, "path": path, "text": text});
        let answer = serde_json::json!({"problems": [
            item("missing_param", "a.ts", "one"),
            item("missing_param", "a.ts", "one"),
            item("Conflicting_Type", "a.ts", "two"),
            item("unowned_effect", "a.ts", "three"),
            item("style", "b.ts", "rename"),
            item("missing_export", "b.ts", "   "),
            item("missing_export", "b.ts", "add  it"),
        ]});
        let parsed = parse_review(&format!("Here:\n{answer}"), &files).unwrap();
        let texts: Vec<(&str, &str)> = parsed
            .iter()
            .map(|problem| (problem.path.as_deref().unwrap(), problem.text.as_str()))
            .collect();
        assert_eq!(
            texts,
            [
                ("a.ts", "missing_param: one"),
                ("a.ts", "conflicting_type: two"),
                ("b.ts", "missing_export: add it"),
            ]
        );
        assert_eq!(parse_review("no json here", &files), None);
        assert_eq!(parse_review("{\"problems\": []}", &files), Some(Vec::new()));
    }

    /// `PlanOptions::semantics`: the conventions end the module head (the
    /// same for every worker), a layout's component gets the layout rule,
    /// and the plan is unchanged with the option off.
    #[test]
    fn semantics_ends_the_module_head_with_the_conventions() {
        let file = |path: &str, purpose: &str, export: &str| ManifestFile {
            path: path.to_string(),
            purpose: purpose.to_string(),
            exports: vec![export.to_string()],
            uses: Vec::new(),
        };
        let manifest = Manifest {
            files: vec![
                file(
                    "lib/types.ts",
                    "Types.",
                    "export interface Transaction { id: string; amount: number; date: string }  // amount: integer cents, always > 0, kind says expense or income",
                ),
                file(
                    "components/AppShell.tsx",
                    "Shell.",
                    "export default function AppShell({ children })",
                ),
                file(
                    "app/layout.tsx",
                    "Root layout.",
                    "export default function RootLayout({ children })  // renders <AppShell>{children}</AppShell>",
                ),
                file("app/page.tsx", "Home.", "export default function Home()"),
            ],
            test_files: Vec::new(),
            readme: false,
        };
        let plan = |profile: &'static Profile, semantics: bool| {
            plan_from_manifest(
                "obj",
                &manifest,
                16,
                Some(profile),
                &PlanOptions {
                    semantics,
                    ..PlanOptions::default()
                },
            )
            .unwrap()
        };
        let off = plan(&NEXTJS, false);
        let on = plan(&NEXTJS, true);
        let block = crate::semantics::conventions(&manifest, &NEXTJS);
        assert!(block.contains("- Money: `Transaction.amount`: amount: integer cents, always > 0"));
        assert!(block.contains("- Shell: `app/layout.tsx` renders <AppShell/>"));
        // No store provider in this plan: the shell gets the rule with no hook.
        let rule = NEXTJS.staging.quality.layout_component_bare_rule;
        for (before, after) in off.tasks.iter().zip(on.tasks.iter()) {
            if before.static_output.is_some() {
                assert_eq!(before.prompt, after.prompt);
                continue;
            }
            let (head, tail) = before.prompt.split_once("Now write the COMPLETE").unwrap();
            let layout = if before.title == "components/AppShell.tsx" {
                format!("\n- {rule}")
            } else {
                String::new()
            };
            let expected = format!("{head}{block}Now write the COMPLETE{tail}{layout}");
            assert_eq!(after.prompt, expected, "{}", before.title);
        }
        assert_eq!(
            serde_json::to_string(&off).unwrap(),
            serde_json::to_string(
                &plan_from_manifest("obj", &manifest, 16, Some(&NEXTJS), &PlanOptions::default())
                    .unwrap()
            )
            .unwrap()
        );
        // With a store provider the shell uses its hook.
        let mut provided = manifest.clone();
        provided.files.push(file(
            "components/StoreProvider.tsx",
            "Store.",
            "export function StoreProvider({ children })",
        ));
        let lines = crate::pagelocal::layout_component_lines(&provided, &NEXTJS);
        let shell = provided
            .files
            .iter()
            .position(|file| file.path == "components/AppShell.tsx")
            .unwrap();
        assert_eq!(
            lines[shell],
            format!(
                "
- {}",
                NEXTJS.staging.quality.layout_component_rule
            )
        );
        let python = crate::profile::Profile::named("python").unwrap();
        assert_eq!(
            serde_json::to_string(&plan(python, true)).unwrap(),
            serde_json::to_string(&plan(python, false)).unwrap()
        );
    }

    /// `PlanOptions::shell_rule`: each page's tail says that the layout's
    /// shell already draws the frame (with the stylesheet's shell class); the
    /// other files and the plan with the option off are unchanged.
    #[test]
    fn shell_rule_tells_each_page_the_layout_draws_the_frame() {
        let file = |path: &str, purpose: &str, exports: &[&str]| ManifestFile {
            path: path.to_string(),
            purpose: purpose.to_string(),
            exports: exports.iter().map(|line| line.to_string()).collect(),
            uses: Vec::new(),
        };
        let manifest = Manifest {
            files: vec![
                file(
                    "app/globals.css",
                    "Styles.",
                    &[".shell  // page wrapper, max width", ".nav  // top bar", ".card  // panel"],
                ),
                file(
                    "components/AppShell.tsx",
                    "Shell.",
                    &["export default function AppShell({ children })  // renders nav links"],
                ),
                file(
                    "app/layout.tsx",
                    "Root layout.",
                    &["export default function RootLayout({ children })  // renders <AppShell>{children}</AppShell>"],
                ),
                file("app/page.tsx", "Home.", &["export default function Home()"]),
                file(
                    "app/settings/page.tsx",
                    "Settings.",
                    &["export default function SettingsPage()"],
                ),
            ],
            test_files: Vec::new(),
            readme: false,
        };
        let plan = |profile: &'static Profile, shell_rule: bool| {
            plan_from_manifest(
                "obj",
                &manifest,
                16,
                Some(profile),
                &PlanOptions {
                    shell_rule,
                    ..PlanOptions::default()
                },
            )
            .unwrap()
        };
        let (off, on) = (plan(&NEXTJS, false), plan(&NEXTJS, true));
        let rule = NEXTJS
            .staging
            .quality
            .page_shell_rule
            .replace("{shell}", "<AppShell>")
            .replace("{wrapper}", "the page wrapper with class `shell`");
        assert!(rule.contains(
            "never wrap it in <AppShell> or in the page wrapper with class `shell` again"
        ));
        let frame_rule = NEXTJS
            .staging
            .quality
            .shell_frame_rule
            .replace("{main}", "<main className=\"shell\">{children}</main>");
        for (before, after) in off.tasks.iter().zip(on.tasks.iter()) {
            if before.title.ends_with("page.tsx") {
                assert_eq!(
                    after.prompt,
                    format!("{}\n- {rule}", before.prompt),
                    "{}",
                    before.title
                );
            } else if before.title.ends_with("AppShell.tsx") {
                // The shell is told to draw the wrapper the pages leave out.
                assert_eq!(
                    after.prompt,
                    format!("{}\n- {frame_rule}", before.prompt),
                    "{}",
                    before.title
                );
            } else {
                assert_eq!(after.prompt, before.prompt, "{}", before.title);
            }
        }
        assert_eq!(
            serde_json::to_string(&off).unwrap(),
            serde_json::to_string(
                &plan_from_manifest("obj", &manifest, 16, Some(&NEXTJS), &PlanOptions::default())
                    .unwrap()
            )
            .unwrap()
        );
        // No shell in the layout: no line; no shell class: the plain wrapper.
        let mut bare = manifest.clone();
        bare.files[2].exports =
            vec!["export default function RootLayout({ children })".to_string()];
        assert!(crate::pagelocal::page_shell_lines(&bare, &NEXTJS)
            .iter()
            .all(String::is_empty));
        let mut plain = manifest.clone();
        plain.files[0].exports = vec![".page  // page wrapper".to_string()];
        let lines = crate::pagelocal::page_shell_lines(&plain, &NEXTJS);
        assert!(
            lines[3].contains("or in the page wrapper again"),
            "{}",
            lines[3]
        );
        assert!(
            lines[1].contains("`<main>{children}</main>`"),
            "{}",
            lines[1]
        );
        // A layout that names two stateless components: only the one that
        // frames `{children}` is the shell; the Toaster gets no line and
        // the pages may still import it.
        let mut two = manifest.clone();
        two.files.insert(
            1,
            file(
                "components/Toaster.tsx",
                "Toasts.",
                &["export default function Toaster()  // shows the queued toasts"],
            ),
        );
        two.files[3].exports = vec![
            "export default function RootLayout({ children })  // renders <Toaster /> and <AppShell>{children}</AppShell>"
                .to_string(),
        ];
        let lines = crate::pagelocal::page_shell_lines(&two, &NEXTJS);
        assert!(lines[1].is_empty(), "{}", lines[1]);
        assert!(lines[2].contains("`<main className=\"shell\">{children}</main>`"));
        assert!(
            lines[4].contains("renders <AppShell> around"),
            "{}",
            lines[4]
        );
        assert!(!lines[4].contains("Toaster"), "{}", lines[4]);
        // A layout whose only stateless component frames nothing: no rule.
        let mut toaster_only = two.clone();
        toaster_only.files.remove(2);
        toaster_only.files[2].exports = vec![
            "export default function RootLayout({ children })  // renders <Toaster />".to_string(),
        ];
        assert!(crate::pagelocal::page_shell_lines(&toaster_only, &NEXTJS)
            .iter()
            .all(String::is_empty));
        let python = crate::profile::Profile::named("python").unwrap();
        assert_eq!(
            serde_json::to_string(&plan(python, true)).unwrap(),
            serde_json::to_string(&plan(python, false)).unwrap()
        );
    }

    /// `plan_pipe` tests: `Shards` behind a skeleton that streams in small
    /// pieces (`report_text`, yielding between them). `skeletons` scripts
    /// the skeleton answers (the last repeats); `log` keeps the order of
    /// skeleton start/end and each fragment request's first listed file.
    struct Streamed {
        inner: Shards,
        skeletons: Mutex<Vec<&'static str>>,
        log: Mutex<Vec<String>>,
        /// Every fragment request as sent (prompt, budget).
        seen: Mutex<Vec<(String, u32)>>,
    }

    const STREAMED_SKELETON: &str = r#"{"files":[{"path":"lib/types.ts","purpose":"Types"},{"path":"package.json","purpose":"Package"},{"path":"lib/list.ts","purpose":"List"},{"path":"app/page.tsx","purpose":"Page"}],"test_files":[],"readme":false}"#;

    #[async_trait::async_trait]
    impl ModelClient for Streamed {
        async fn complete(&self, request: CompletionRequest) -> Result<Completion> {
            let user = &request.user;
            if user.contains("paths and one-sentence purposes ONLY") {
                let text = {
                    let mut scripted = self.skeletons.lock().unwrap();
                    if scripted.len() > 1 {
                        scripted.remove(0)
                    } else {
                        scripted[0]
                    }
                };
                self.log.lock().unwrap().push("skeleton-start".to_string());
                let mut end = 0;
                while end < text.len() {
                    end = (end + 12).min(text.len());
                    crate::client::report_text(&text[..end]);
                    tokio::task::yield_now().await;
                }
                self.log.lock().unwrap().push("skeleton-end".to_string());
                return Ok(reply(text));
            }
            if user.contains("Design the interface contracts") {
                let first = user
                    .split("These files exist")
                    .nth(1)
                    .and_then(|rest| rest.split("- `").nth(1))
                    .and_then(|rest| rest.split('`').next())
                    .unwrap_or("")
                    .to_string();
                self.log.lock().unwrap().push(format!("fragment:{first}"));
                self.seen
                    .lock()
                    .unwrap()
                    .push((request.user.clone(), request.max_tokens));
                // `Shards` answers every listed file; a fragment that names
                // its own files (`Reference only: ...`) gets those alone.
                let named: Vec<String> = user
                    .split("(Reference only: plan the exports of ")
                    .nth(1)
                    .and_then(|rest| rest.split(" and nothing else").next())
                    .map(|list| {
                        list.split(", ")
                            .map(|path| path.trim_matches('`').to_string())
                            .collect()
                    })
                    .unwrap_or_default();
                if named.is_empty() {
                    return self.inner.complete(request).await;
                }
                let listing: String = named
                    .iter()
                    .map(|path| format!("- `{path}`: x\n"))
                    .collect();
                let narrowed = CompletionRequest {
                    user: format!("These files exist\n{listing}Fragment discipline"),
                    ..request
                };
                return self.inner.complete(narrowed).await;
            }
            Err(anyhow::anyhow!("not scripted"))
        }
    }

    struct PipeRun {
        manifest: String,
        /// Every fragment request (prompt, budget), sorted.
        requests: Vec<(String, u32)>,
        log: Vec<String>,
    }

    async fn pipe_run(
        options: ArchitectOptions,
        plan_pipe: bool,
        skeletons: Vec<&'static str>,
    ) -> PipeRun {
        let client = Streamed {
            inner: Shards {
                seen: Mutex::new(Vec::new()),
                dangling: false,
                review: None,
            },
            skeletons: Mutex::new(skeletons),
            log: Mutex::new(Vec::new()),
            seen: Mutex::new(Vec::new()),
        };
        let options = ArchitectOptions {
            profile: Some(&NEXTJS),
            plan_pipe,
            ..options
        };
        let manifest = generate_manifest(&client, "obj", 16, &[], &options)
            .await
            .unwrap();
        let mut requests = client.seen.into_inner().unwrap();
        requests.sort();
        PipeRun {
            manifest: serde_json::to_string(&manifest).unwrap(),
            requests,
            log: client.log.into_inner().unwrap(),
        }
    }

    fn position(log: &[String], entry: &str) -> usize {
        log.iter()
            .position(|line| line == entry)
            .unwrap_or_else(|| panic!("{entry} in {log:?}"))
    }

    /// `plan_pipe` with `types_first`: the types fragment starts once its
    /// entry has streamed and package.json once the file list closes, both
    /// before the skeleton call returns; every request and the manifest are
    /// the same as with the switch off.
    #[tokio::test]
    async fn plan_pipe_starts_fragments_while_the_skeleton_streams() {
        let options = || ArchitectOptions {
            types_first: true,
            ..ArchitectOptions::default()
        };
        let off = pipe_run(options(), false, vec![STREAMED_SKELETON]).await;
        let on = pipe_run(options(), true, vec![STREAMED_SKELETON]).await;
        let end = position(&on.log, "skeleton-end");
        assert!(
            position(&on.log, "fragment:lib/types.ts") < end,
            "{:?}",
            on.log
        );
        assert!(
            position(&on.log, "fragment:package.json") < end,
            "{:?}",
            on.log
        );
        // The types fragment went out before the skeleton listed the page.
        assert!(
            position(&on.log, "fragment:lib/types.ts") < position(&on.log, "fragment:package.json")
        );
        // Round two waits for the planned types, as before.
        assert!(position(&on.log, "fragment:lib/list.ts") > end);
        // Off: nothing starts before the skeleton returns.
        assert!(position(&off.log, "fragment:lib/types.ts") > position(&off.log, "skeleton-end"));
        assert_eq!(on.requests, off.requests, "the same requests, none twice");
        assert_eq!(on.manifest, off.manifest);
    }

    /// Without `types_first`: each full pair starts as soon as its second
    /// file has streamed.
    #[tokio::test]
    async fn plan_pipe_starts_each_pair_once_listed() {
        let off = pipe_run(ArchitectOptions::default(), false, vec![STREAMED_SKELETON]).await;
        let on = pipe_run(ArchitectOptions::default(), true, vec![STREAMED_SKELETON]).await;
        let end = position(&on.log, "skeleton-end");
        assert!(
            position(&on.log, "fragment:lib/types.ts") < end,
            "{:?}",
            on.log
        );
        assert!(
            position(&on.log, "fragment:lib/list.ts") < end,
            "{:?}",
            on.log
        );
        assert_eq!(on.requests, off.requests);
        assert_eq!(on.manifest, off.manifest);
    }

    /// With `contract` the types fragment lists every planned file, so it
    /// waits for the closed file list (still before the call returns); the
    /// requests, the repair round's included, match the switch off.
    #[tokio::test]
    async fn plan_pipe_types_fragment_waits_for_the_whole_file_list() {
        let options = || ArchitectOptions {
            types_first: true,
            contract: true,
            ..ArchitectOptions::default()
        };
        let off = pipe_run(options(), false, vec![STREAMED_SKELETON]).await;
        let on = pipe_run(options(), true, vec![STREAMED_SKELETON]).await;
        let types = on
            .requests
            .iter()
            .find(|(user, _)| {
                user.contains("- `lib/types.ts`: Types") && !user.contains("had these problems")
            })
            .unwrap();
        assert!(
            types.0.contains("- `app/page.tsx`: Page"),
            "every file listed"
        );
        assert!(position(&on.log, "fragment:lib/types.ts") < position(&on.log, "skeleton-end"));
        assert_eq!(on.requests, off.requests);
        assert_eq!(on.manifest, off.manifest);
    }

    /// A fragment started from a skeleton answer that is then retried does
    /// not match the final skeleton's request: it is dropped and the right
    /// one is sent; the plan is the same as with the switch off.
    #[tokio::test]
    async fn plan_pipe_drops_fragments_the_final_skeleton_does_not_send() {
        const BROKEN: &str = r#"{"files":[{"path":"lib/types.ts","purpose":"Old types"},{"path":"package.json","purpose":"Package"} oops"#;
        let options = || ArchitectOptions {
            types_first: true,
            ..ArchitectOptions::default()
        };
        let off = pipe_run(options(), false, vec![BROKEN, STREAMED_SKELETON]).await;
        let on = pipe_run(options(), true, vec![BROKEN, STREAMED_SKELETON]).await;
        assert_eq!(
            on.log
                .iter()
                .filter(|line| *line == "skeleton-start")
                .count(),
            2
        );
        let (stale, kept): (Vec<_>, Vec<_>) = on
            .requests
            .iter()
            .cloned()
            .partition(|(user, _)| user.contains("- `lib/types.ts`: Old types"));
        assert_eq!(stale.len(), 1, "the early fragment of the first answer");
        assert_eq!(kept, off.requests);
        assert_eq!(on.manifest, off.manifest);
    }

    /// The fragment started from a skeleton attempt that is then retried is
    /// dropped (its stream closed) as soon as the retry streams, not when
    /// the retried skeleton returns.
    #[tokio::test]
    async fn plan_pipe_drops_a_stale_fragment_when_the_skeleton_restarts() {
        const BROKEN: &str = r#"{"files":[{"path":"lib/types.ts","purpose":"Old types"},{"path":"package.json","purpose":"Package"} oops"#;
        struct Stalling(Streamed);
        struct Dropped<'a>(&'a Mutex<Vec<String>>);
        impl Drop for Dropped<'_> {
            fn drop(&mut self) {
                self.0.lock().unwrap().push("stale-dropped".to_string());
            }
        }
        #[async_trait::async_trait]
        impl ModelClient for Stalling {
            async fn complete(&self, request: CompletionRequest) -> Result<Completion> {
                if request.user.contains("Design the interface contracts")
                    && request.user.contains("- `lib/types.ts`: Old types")
                {
                    let _dropped = Dropped(&self.0.log);
                    tokio::time::sleep(std::time::Duration::from_secs(60)).await;
                    anyhow::bail!("the stale fragment ran to the end");
                }
                self.0.complete(request).await
            }
        }
        let client = Stalling(Streamed {
            inner: Shards {
                seen: Mutex::new(Vec::new()),
                dangling: false,
                review: None,
            },
            skeletons: Mutex::new(vec![BROKEN, STREAMED_SKELETON]),
            log: Mutex::new(Vec::new()),
            seen: Mutex::new(Vec::new()),
        });
        let options = ArchitectOptions {
            profile: Some(&NEXTJS),
            plan_pipe: true,
            types_first: true,
            ..ArchitectOptions::default()
        };
        generate_manifest(&client, "obj", 16, &[], &options)
            .await
            .unwrap();
        let log = client.0.log.into_inner().unwrap();
        let dropped = position(&log, "stale-dropped");
        let last_end = log.iter().rposition(|line| line == "skeleton-end").unwrap();
        let second_start = log
            .iter()
            .rposition(|line| line == "skeleton-start")
            .unwrap();
        assert!(
            dropped > second_start && dropped < last_end,
            "dropped while the retry streamed: {log:?}"
        );
    }

    #[test]
    fn streamed_files_reads_complete_entries_only() {
        assert_eq!(
            streamed_files("").map(|(files, closed)| (files.len(), closed)),
            Some((0, false))
        );
        assert_eq!(
            streamed_files("```json\n{\"files\": [ {\"path\": \"a.ts\", \"purpose\": \"A {x}\"}, {\"path\": \"b")
                .map(|(files, closed)| (files.len(), closed)),
            Some((1, false))
        );
        let (files, closed) =
            streamed_files(r#"{"files":[{"path":"a.ts","purpose":"say \"}\""},"b.ts"],"test"#)
                .unwrap();
        assert!(closed);
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].purpose, "say \"}\"");
        assert_eq!(files[1].path, "b.ts");
        assert!(streamed_files(r#"{"files": 3}"#).is_none());
        assert!(streamed_files(r#"{"files":[{"path": 3}]}"#).is_none());
    }
}

#[cfg(test)]
mod types_paths_tests {
    use super::*;

    /// `types_from_paths`: the types fragment built from the streamed path
    /// list is the request that the final skeleton sends, whatever the
    /// purposes say; the list counts only once it is closed.
    #[test]
    fn the_types_fragment_from_the_paths_is_the_final_request() {
        let options = ArchitectOptions {
            types_first: true,
            contract: true,
            plan_pipe: true,
            types_from_paths: true,
            ..ArchitectOptions::default()
        };
        let profile = &crate::profile::NEXTJS;
        let text = r#"{"paths": ["lib/types.ts", "lib/tasks.ts", "app/page.tsx"],
            "files": [{"path": "lib/types.ts", "purpose": "Types."}"#;
        assert_eq!(streamed_paths(r#"{"paths": ["lib/types.ts", "lib"#), None);
        assert_eq!(streamed_paths(r#"{"files": []}"#), None);
        let paths = streamed_paths(text).expect("a closed list");
        assert_eq!(paths, ["lib/types.ts", "lib/tasks.ts", "app/page.tsx"]);
        let early = types_fragments_from_paths(paths.clone(), "Build it.", &options, profile);
        assert_eq!(early.len(), 1);

        let mut skeleton = Manifest {
            files: paths
                .iter()
                .map(|path| ManifestFile {
                    path: path.clone(),
                    purpose: format!("Purpose of {path}."),
                    exports: Vec::new(),
                    uses: Vec::new(),
                })
                .collect(),
            test_files: Vec::new(),
            readme: false,
        };
        (profile.normalize_skeleton)(&mut skeleton);
        let layout = fragment_layout(&skeleton, &options, profile);
        assert!(layout.types_first);
        let last = exports_fragment(
            &layout,
            "Build it.",
            &options,
            profile,
            &layout.round_one[0],
            "",
            false,
        );
        assert!(early[0].1.same_request(&last));
        assert!(last.user.contains("- `lib/tasks.ts`\n"), "{}", last.user);
        assert!(!last.user.contains("Purpose of"), "{}", last.user);
        assert!(last.user.contains(TYPES_PURPOSE));

        // Off: the purposes are in the request, as before.
        let off = ArchitectOptions {
            types_from_paths: false,
            ..options.clone()
        };
        let layout = fragment_layout(&skeleton, &off, profile);
        let request = exports_fragment(
            &layout,
            "Build it.",
            &off,
            profile,
            &layout.round_one[0],
            "",
            false,
        );
        assert!(request.user.contains("Purpose of lib/tasks.ts."));
    }
}
