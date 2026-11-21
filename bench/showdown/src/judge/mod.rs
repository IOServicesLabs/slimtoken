//! `showdown judge`: a blind quality judge for a finished run.
//!
//! Three layers, each reported on its own:
//! - **Static lint** (no model call): the `qlint` lints on each app, as
//!   counts. They are not part of the score.
//! - **Model judge**: for each app, `--samples` coverage calls (one status per
//!   requirement of `objectives/<name>.reqs.json`) and `--samples` working
//!   calls (the defects a user meets), with the numbered app bundle as the
//!   shared context; then two pairwise calls with the apps in both orders.
//!   Every quote is checked against the code: a bug whose quote does not
//!   stand at the cited line is dropped, a "full" without one verified quote
//!   becomes "partial", and a "missing" claim whose search terms occur in the
//!   UI code is disputed. Samples that disagree (full against missing, or a
//!   disputed claim) get one tiebreak call over those requirements.
//! - **Runtime smoke** (`smoke.rs`, no model call): the app is built with
//!   the shared toolchain, served, and driven in headless Chrome/Edge by
//!   the requirement-driven flows of the objective; smoke = 100 · passed /
//!   attempted. Without a browser (or with `--no-smoke`) it is absent and
//!   Q is renormalised over the model lenses (and says so).
//! - **Delivery** (`delivery.rs`, no model call): the arm's real install,
//!   build, test and pages results (recorded by the race, else checked
//!   locally). They turn Q (as if built) into Q as delivered.
//!
//! Score: cov = 100·Σ(w·s)/Σw (full 1, partial 0.5, missing/stub 0);
//! work = max(0, 100 − 8·broken − 3·wrong − 1·cosmetic), the bug counts the
//! mean over the samples; q_if_built = Q = 0.4·cov + 0.4·work + 0.2·smoke,
//! and without smoke Q = 0.5·cov + 0.5·work. q_delivered: a build that
//! fails, or no page that renders, gives 0.25·Q ("does not run"); each
//! failed test −2 (at most −10); failing pages scale it by the share that
//! renders. The winner is the higher q_delivered (`winner_if_built`: the
//! higher Q), "confirmed" when the pairwise calls (which read the code
//! only) agree in both orders and pick the winner if built.
//!
//! The apps are blind: neutral labels X and Y in an order drawn from the
//! run's name and `--seed`, no folder or run names in the text, and a guard
//! that refuses a bundle naming a generator or the run. `_judge/mapping.json`
//! (written last) unblinds the labels.

pub mod bundle;
pub mod delivery;
pub mod prompts;
pub mod smoke;
pub mod validate;
pub mod verify;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use slimtoken::{CompletionRequest, ModelClient};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use verify::{Bug, Checked, ReqClaim, Sources};

/// The checked-in requirement lists.
const TASKFLOW_REQS: &str = include_str!("../../objectives/taskflow.reqs.json");
const LEDGERLY_REQS: &str = include_str!("../../objectives/ledgerly.reqs.json");
const TASKFLOW_OBJECTIVE: &str = include_str!("../../objectives/taskflow.txt");
const LEDGERLY_OBJECTIVE: &str = include_str!("../../objectives/ledgerly.txt");

/// The pinned single-stream reference app of each objective, for a
/// `bench-slim` run (which has no single arm): runs of the 2026-11-15 audit.
const DEFAULT_REFS: &[(&str, &str)] = &[
    ("taskflow", "slimtoken-out-showdown/20261115-083407/single"),
    ("ledgerly", "slimtoken-out-showdown/20261115-083658/single"),
];

/// Approximate DeepSeek list prices in USD per million tokens: input,
/// cached input, output (check the provider's price page).
const PRICES: (f64, f64, f64) = (0.40, 0.013, 1.20);

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Requirement {
    pub id: String,
    #[serde(default)]
    pub area: String,
    #[serde(default = "one")]
    pub weight: f64,
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ui: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub implied: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub items: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scoring: Option<String>,
}

fn one() -> f64 {
    1.0
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Reqs {
    pub objective: String,
    pub objective_sha256: String,
    #[serde(default)]
    pub reviewed: bool,
    pub requirements: Vec<Requirement>,
    /// The runtime smoke flows (`smoke.rs`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub smoke: Option<smoke::SmokeSpec>,
}

impl Reqs {
    /// The requirement list as the prompt shows it.
    fn listing(&self, ids: &[&str]) -> String {
        self.requirements
            .iter()
            .filter(|req| ids.contains(&req.id.as_str()))
            .map(|req| {
                let mut line = format!("{}: {}", req.id, req.text);
                if let Some(items) = &req.items {
                    line.push_str(&format!(" (items: {})", items.join(", ")));
                }
                if let Some(scoring) = &req.scoring {
                    line.push_str(&format!(" [{scoring}]"));
                }
                line
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// The built-in requirement list and objective text of `name`.
pub fn builtin(name: &str) -> Option<(Reqs, &'static str)> {
    let (reqs, objective) = match name {
        "taskflow" => (TASKFLOW_REQS, TASKFLOW_OBJECTIVE),
        "ledgerly" => (LEDGERLY_REQS, LEDGERLY_OBJECTIVE),
        _ => return None,
    };
    serde_json::from_str(reqs)
        .ok()
        .map(|reqs| (reqs, objective))
}

/// The requirement list for `objective`: the built-in one whose fingerprint
/// matches, else `name`'s (refused on a fingerprint mismatch).
pub fn reqs_for(objective: &str, name: Option<&str>) -> Result<Reqs> {
    let sha = bundle::objective_sha(objective);
    for candidate in ["taskflow", "ledgerly"] {
        if let Some((reqs, _)) = builtin(candidate) {
            if reqs.objective_sha256 == sha {
                return Ok(reqs);
            }
        }
    }
    match name.and_then(builtin) {
        Some((reqs, _)) => bail!(
            "the objective changed: its fingerprint {sha} is not the one in {}.reqs.json ({}); \
             review the requirement list and update its objective_sha256",
            reqs.objective,
            reqs.objective_sha256
        ),
        None => bail!("no requirement list for this objective (fingerprint {sha}); pass --objective taskflow|ledgerly"),
    }
}

/// The judge's settings.
#[derive(Clone, Debug)]
pub struct Options {
    pub effort: Option<String>,
    pub samples: usize,
    pub seed: u64,
    pub pairwise: bool,
    pub llm: bool,
    pub max_tokens: u32,
    pub temperature: f32,
    /// Where the raw answers go (`None`: not written).
    pub raw_dir: Option<PathBuf>,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            effort: Some("high".to_string()),
            samples: 2,
            seed: 0,
            pairwise: true,
            llm: true,
            max_tokens: 32_000,
            temperature: 0.3,
            raw_dir: None,
        }
    }
}

/// One app to judge: its blind label, its files, and the layers that do
/// not read the code (filled before the judge runs, `None` when absent).
#[derive(Default)]
pub struct App {
    pub label: String,
    pub files: Vec<(String, String)>,
    pub delivered: Option<delivery::Delivered>,
    pub smoke: Option<smoke::SmokeReport>,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct Usage {
    pub calls: usize,
    pub failed_calls: usize,
    pub input_tokens: u64,
    pub cached_input_tokens: u64,
    pub output_tokens: u64,
    pub cost_usd: f64,
}

impl Usage {
    fn add(&mut self, completion: &slimtoken::Completion) {
        self.calls += 1;
        let cached = completion.cached_input_tokens.unwrap_or(0);
        self.input_tokens += completion.input_tokens;
        self.cached_input_tokens += cached;
        self.output_tokens += completion.output_tokens;
        let (input, hit, output) = PRICES;
        self.cost_usd += (completion.input_tokens.saturating_sub(cached) as f64 * input
            + cached as f64 * hit
            + completion.output_tokens as f64 * output)
            / 1e6;
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct ReqResult {
    pub id: String,
    pub weight: f64,
    pub status: String,
    /// The status of each sample, after the quote checks.
    pub samples: Vec<String>,
    pub tiebreak: bool,
}

/// The verdict on one app.
#[derive(Clone, Debug, Default, Serialize)]
pub struct AppVerdict {
    pub label: String,
    pub cov: Option<f64>,
    pub work: Option<f64>,
    /// Runtime smoke score (absent without a browser or with `--no-smoke`).
    pub smoke: Option<f64>,
    /// Q as if the app built (the model lenses and the smoke).
    pub q: Option<f64>,
    pub q_if_built: Option<f64>,
    /// Q as delivered: `q_if_built` with the real checks applied.
    pub q_delivered: Option<f64>,
    /// The build (or install) fails, or no page renders.
    pub does_not_run: bool,
    pub delivered: Option<delivery::Delivered>,
    pub smoke_report: Option<smoke::SmokeReport>,
    /// Mean verified bugs per sample.
    pub broken: f64,
    pub wrong: f64,
    pub cosmetic: f64,
    pub unverified_bugs: usize,
    pub req_agreement: Option<f64>,
    pub kappa: Option<f64>,
    pub low_confidence: bool,
    pub bug_overlap: Option<f64>,
    pub tiebreaks: usize,
    pub requirements: Vec<ReqResult>,
    /// The verified bugs of every sample (sample index first).
    pub bugs: Vec<(usize, Bug)>,
    /// Static lint counts by lint name, and the lines.
    pub lint: BTreeMap<String, usize>,
    pub lint_lines: Vec<String>,
    pub errors: Vec<String>,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct PairOrder {
    /// The label in position A.
    pub first: String,
    /// The label the call chose (`tie` or empty when it failed).
    pub better: String,
    pub margin: String,
    pub why: String,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct Pairwise {
    /// The label both orders chose, `tie`, or `uncertain`.
    pub winner: String,
    pub orders: Vec<PairOrder>,
    /// 1 when the two orders disagree.
    pub flips: usize,
}

/// The judge's result on two apps.
#[derive(Clone, Debug, Default, Serialize)]
pub struct Verdict {
    pub objective: String,
    pub reqs_reviewed: bool,
    pub score_note: String,
    pub apps: Vec<AppVerdict>,
    pub pairwise: Option<Pairwise>,
    /// The label with the higher q_delivered (`tie` within 0.5 points; an
    /// app that runs always beats one that does not), or the higher Q when
    /// an app has no delivery status.
    pub winner: Option<String>,
    /// `q_delivered` or `q`: what `winner` compares.
    pub winner_basis: Option<String>,
    /// The label with the higher Q (as if both built).
    pub winner_if_built: Option<String>,
    /// `confirmed`, `contested`, or `unchecked` (no pairwise call): the
    /// pairwise calls against `winner_if_built` (they read the code only).
    pub verdict: Option<String>,
    pub usage: Usage,
}

/// The working lens's checklist names the same defect classes as the `qlint`
/// lints (settings, literal case, theme, dead links, seed dates, README), so
/// a `qlint` run's gain on that lens is partly by construction.
pub const LENS_OVERLAP_NOTE: &str = "the working lens checks the qlint defect classes (settings, case, theme, dead links, seed dates, README): when comparing qlint runs, read its gain alongside the coverage lens and the pairwise verdict";

pub const SCORE_NOTE: &str = "smoke absent: Q = (0.4*cov + 0.4*work) / 0.8 = 0.5*cov + 0.5*work (renormalised over the model lenses)";

/// The note on how Q was formed, from the layers the apps have.
pub fn score_note(apps: &[App]) -> String {
    let with_smoke = apps
        .iter()
        .filter(|app| {
            app.smoke
                .as_ref()
                .is_some_and(|smoke| smoke.score.is_some())
        })
        .count();
    let smoke = if with_smoke == apps.len() && !apps.is_empty() {
        "Q = 0.4*cov + 0.4*work + 0.2*smoke".to_string()
    } else if with_smoke == 0 {
        SCORE_NOTE.to_string()
    } else {
        format!(
            "smoke on {with_smoke} of {} apps: Q renormalised per app over the layers it has",
            apps.len()
        )
    };
    let delivered = apps.iter().filter(|app| app.delivered.is_some()).count();
    format!(
        "{smoke}; q_delivered = Q with the checks: build fails or no page renders -> 0.25*Q, -2 per failed test (max -10), pages scale by the share that renders ({delivered} of {} apps have a delivery status)",
        apps.len()
    )
}

/// The static lint layer of one app: counts by lint name and the lines
/// (with `qlint2` in `SHOWDOWN_ON`, the convention lints too).
pub fn lint_counts(files: &[(String, String)]) -> (BTreeMap<String, usize>, Vec<String>) {
    let conventions = crate::arms::step_on("qlint2");
    let lints = crate::qlint::lint_app_with(files, true, conventions, false);
    let extra: &[&str] = if conventions {
        crate::qlint::NAMES2
    } else {
        &[]
    };
    let names = crate::qlint::NAMES.iter().chain(extra);
    let mut counts: BTreeMap<String, usize> = names.map(|name| (name.to_string(), 0)).collect();
    for lint in &lints {
        *counts.entry(lint.name.to_string()).or_default() += 1;
    }
    (
        counts,
        lints.iter().map(crate::qlint::Lint::render).collect(),
    )
}

/// What a call is for.
#[derive(Clone, Debug)]
enum Job {
    Coverage { app: usize, sample: usize },
    Working { app: usize, sample: usize },
    Pair { order: usize },
    Tiebreak { app: usize, ids: Vec<String> },
}

impl Job {
    fn name(&self) -> String {
        match self {
            Job::Coverage { app, sample } => format!("coverage-{app}-{sample}"),
            Job::Working { app, sample } => format!("working-{app}-{sample}"),
            Job::Pair { order } => format!("pairwise-{order}"),
            Job::Tiebreak { app, .. } => format!("tiebreak-{app}"),
        }
    }
}

/// One call: the reply as JSON, retried once on an error or an answer that
/// is not JSON.
async fn call(
    client: Arc<dyn ModelClient>,
    request: CompletionRequest,
    usage: Arc<std::sync::Mutex<Usage>>,
) -> Result<(serde_json::Value, String)> {
    let mut last = anyhow::anyhow!("no answer");
    for _ in 0..2 {
        match client.complete(request.clone()).await {
            Ok(completion) => {
                if let Ok(mut usage) = usage.lock() {
                    usage.add(&completion);
                }
                match verify::parse_json(&completion.text) {
                    Some(value) => return Ok((value, completion.text)),
                    None => last = anyhow::anyhow!("the answer is not JSON"),
                }
            }
            Err(error) => last = error,
        }
    }
    if let Ok(mut usage) = usage.lock() {
        usage.failed_calls += 1;
    }
    Err(last)
}

/// Judge two apps (or one, with no pairwise call).
pub async fn judge_apps(
    client: Option<Arc<dyn ModelClient>>,
    objective: &str,
    reqs: &Reqs,
    apps: &[App],
    options: &Options,
) -> Result<Verdict> {
    let bundles: Vec<Arc<String>> = apps
        .iter()
        .map(|app| Arc::new(bundle::bundle(&app.files)))
        .collect();
    let mut verdict = Verdict {
        objective: reqs.objective.clone(),
        reqs_reviewed: reqs.reviewed,
        score_note: score_note(apps),
        ..Verdict::default()
    };
    let mut verdicts: Vec<AppVerdict> = apps
        .iter()
        .map(|app| {
            let (lint, lint_lines) = lint_counts(&app.files);
            AppVerdict {
                label: app.label.clone(),
                lint,
                lint_lines,
                smoke: app.smoke.as_ref().and_then(|smoke| smoke.score),
                smoke_report: app.smoke.clone(),
                does_not_run: app
                    .delivered
                    .as_ref()
                    .is_some_and(delivery::Delivered::does_not_run),
                delivered: app.delivered.clone(),
                ..AppVerdict::default()
            }
        })
        .collect();
    let Some(client) = client.filter(|_| options.llm) else {
        verdict.apps = verdicts;
        return Ok(verdict);
    };
    let usage = Arc::new(std::sync::Mutex::new(Usage::default()));
    let all_ids: Vec<&str> = reqs
        .requirements
        .iter()
        .map(|req| req.id.as_str())
        .collect();
    let request = |shared: String, user: String, key: String| CompletionRequest {
        system: prompts::SYSTEM.to_string(),
        shared_context: Some(shared),
        user,
        max_tokens: options.max_tokens,
        temperature: options.temperature,
        reasoning_effort: options.effort.clone(),
        prompt_cache_key: Some(key),
        allow_empty: false,
        response_format: None,
    };
    let key_of = |index: usize| format!("showdown-judge-{:016x}", bundle::seed_of(&bundles[index]));
    let mut jobs: Vec<(Job, CompletionRequest)> = Vec::new();
    for (index, app) in apps.iter().enumerate() {
        for sample in 0..options.samples.max(1) {
            let mut ids = all_ids.clone();
            bundle::Rng::new(options.seed ^ bundle::seed_of(&app.label) ^ (sample as u64 + 1))
                .shuffle(&mut ids);
            // The listing keeps the shuffled order.
            let listing = ids
                .iter()
                .map(|id| reqs.listing(&[id]))
                .collect::<Vec<_>>()
                .join("\n");
            jobs.push((
                Job::Coverage { app: index, sample },
                request(
                    (*bundles[index]).clone(),
                    prompts::coverage(objective, &listing),
                    key_of(index),
                ),
            ));
            jobs.push((
                Job::Working { app: index, sample },
                request(
                    (*bundles[index]).clone(),
                    prompts::working(objective),
                    key_of(index),
                ),
            ));
        }
    }
    if options.pairwise && apps.len() == 2 {
        for order in 0..2 {
            let (first, second) = if order == 0 { (0, 1) } else { (1, 0) };
            jobs.push((
                Job::Pair { order },
                request(
                    prompts::pair_context(&bundles[first], &bundles[second]),
                    prompts::pairwise(objective),
                    format!("{}-{}", key_of(first), key_of(second)),
                ),
            ));
        }
    }
    let answers = run_jobs(&client, jobs, &usage, options).await;
    let sources: Vec<Sources> = apps.iter().map(|app| Sources::new(&app.files)).collect();
    // Per app: the checked claims and bugs of each sample.
    let mut claims: Vec<Vec<Vec<Checked>>> = vec![Vec::new(); apps.len()];
    let mut raw_claims: Vec<Vec<Vec<ReqClaim>>> = vec![Vec::new(); apps.len()];
    let mut bugs: Vec<Vec<Vec<Bug>>> = vec![Vec::new(); apps.len()];
    let mut orders: Vec<PairOrder> = Vec::new();
    for (job, answer) in &answers {
        match (job, answer) {
            (Job::Coverage { app, .. }, Ok(value)) => {
                let parsed: Vec<ReqClaim> =
                    serde_json::from_value(value["requirements"].clone()).unwrap_or_default();
                let parsed: Vec<ReqClaim> = parsed
                    .into_iter()
                    .filter(|claim| all_ids.contains(&claim.id.as_str()))
                    .collect();
                claims[*app].push(verify::check_claims(&parsed, &sources[*app]));
                raw_claims[*app].push(parsed);
            }
            (Job::Working { app, .. }, Ok(value)) => {
                let parsed: Vec<Bug> =
                    serde_json::from_value(value["bugs"].clone()).unwrap_or_default();
                let (kept, dropped) = verify::check_bugs(&parsed, &sources[*app]);
                verdicts[*app].unverified_bugs += dropped;
                bugs[*app].push(kept);
            }
            (Job::Pair { order }, Ok(value)) => {
                let (first, second) = if *order == 0 { (0, 1) } else { (1, 0) };
                let better = match value["better"]
                    .as_str()
                    .unwrap_or("")
                    .trim()
                    .to_uppercase()
                    .as_str()
                {
                    "A" => apps[first].label.clone(),
                    "B" => apps[second].label.clone(),
                    _ => "tie".to_string(),
                };
                orders.push(PairOrder {
                    first: apps[first].label.clone(),
                    better,
                    margin: value["margin"].as_str().unwrap_or("").to_string(),
                    why: value["why"].as_str().unwrap_or("").to_string(),
                });
            }
            (job, Err(error)) => {
                let message = format!("{}: {error:#}", job.name());
                match job {
                    Job::Coverage { app, .. }
                    | Job::Working { app, .. }
                    | Job::Tiebreak { app, .. } => verdicts[*app].errors.push(message),
                    Job::Pair { .. } => orders.push(PairOrder {
                        why: message,
                        ..PairOrder::default()
                    }),
                }
            }
            (Job::Tiebreak { .. }, Ok(_)) => {}
        }
    }
    // Merge the samples; disagreements go to one tiebreak call per app.
    let mut merged: Vec<BTreeMap<String, Option<String>>> = vec![BTreeMap::new(); apps.len()];
    let mut tiebreak_jobs = Vec::new();
    for (index, samples) in claims.iter().enumerate() {
        if samples.is_empty() {
            continue;
        }
        let mut open: Vec<String> = Vec::new();
        for id in &all_ids {
            let statuses: Vec<&Checked> = samples
                .iter()
                .filter_map(|sample| sample.iter().find(|checked| checked.id == *id))
                .collect();
            let status = merge_or_missing(&statuses, samples.len());
            if status.is_none() {
                open.push(id.to_string());
            }
            merged[index].insert(id.to_string(), status);
        }
        if !open.is_empty() {
            let ids: Vec<&str> = open.iter().map(String::as_str).collect();
            let earlier = raw_claims[index]
                .iter()
                .enumerate()
                .flat_map(|(sample, list)| {
                    list.iter()
                        .filter(|claim| open.contains(&claim.id))
                        .map(move |claim| {
                            format!(
                                "review {}: {} {} ({})",
                                sample + 1,
                                claim.id,
                                claim.status,
                                claim.note
                            )
                        })
                })
                .collect::<Vec<_>>()
                .join("\n");
            tiebreak_jobs.push((
                Job::Tiebreak {
                    app: index,
                    ids: open.clone(),
                },
                request(
                    (*bundles[index]).clone(),
                    prompts::tiebreak(objective, &reqs.listing(&ids), &earlier),
                    key_of(index),
                ),
            ));
        }
    }
    let tiebreaks = run_jobs(&client, tiebreak_jobs, &usage, options).await;
    let mut tiebroken: Vec<std::collections::BTreeSet<String>> =
        vec![Default::default(); apps.len()];
    for (job, answer) in tiebreaks {
        let Job::Tiebreak { app, ids, .. } = job else {
            continue;
        };
        verdicts[app].tiebreaks = ids.len();
        tiebroken[app].extend(ids.iter().cloned());
        let decided: Vec<Checked> = match answer {
            Ok(value) => {
                let parsed: Vec<ReqClaim> =
                    serde_json::from_value(value["requirements"].clone()).unwrap_or_default();
                verify::check_claims(&parsed, &sources[app])
            }
            Err(error) => {
                verdicts[app]
                    .errors
                    .push(format!("tiebreak-{app}: {error:#}"));
                Vec::new()
            }
        };
        for id in ids {
            // A disputed "missing" that the tiebreak repeats stays partial:
            // the words it searched for are in the UI code.
            let status = decided
                .iter()
                .find(|checked| checked.id == id)
                .map(|checked| {
                    if checked.disputed {
                        "partial".to_string()
                    } else {
                        checked.status.clone()
                    }
                })
                .unwrap_or_else(|| "partial".to_string());
            merged[app].insert(id, Some(status));
        }
    }
    // Scores.
    for (index, app_verdict) in verdicts.iter_mut().enumerate() {
        if !claims[index].is_empty() {
            let mut total = 0.0;
            let mut got = 0.0;
            for req in &reqs.requirements {
                let status = merged[index]
                    .get(&req.id)
                    .cloned()
                    .flatten()
                    .unwrap_or_else(|| "partial".to_string());
                let value = match status.as_str() {
                    "full" => 1.0,
                    "partial" => 0.5,
                    _ => 0.0,
                };
                total += req.weight;
                got += req.weight * value;
                app_verdict.requirements.push(ReqResult {
                    id: req.id.clone(),
                    weight: req.weight,
                    status,
                    samples: claims[index]
                        .iter()
                        .map(|sample| {
                            sample
                                .iter()
                                .find(|checked| checked.id == req.id)
                                .map_or_else(|| "-".to_string(), |checked| checked.status.clone())
                        })
                        .collect(),
                    tiebreak: tiebroken[index].contains(&req.id),
                });
            }
            app_verdict.cov = (total > 0.0).then(|| round1(100.0 * got / total));
            if claims[index].len() >= 2 {
                let (share, kappa) = verify::agreement(&claims[index][0], &claims[index][1]);
                app_verdict.req_agreement = Some(round3(share));
                app_verdict.kappa = Some(round3(kappa));
                app_verdict.low_confidence = kappa < 0.7;
            }
        }
        if !bugs[index].is_empty() {
            let samples = bugs[index].len() as f64;
            let count = |severity: &str| {
                bugs[index]
                    .iter()
                    .map(|sample| sample.iter().filter(|bug| bug.severity == severity).count())
                    .sum::<usize>() as f64
                    / samples
            };
            app_verdict.broken = round3(count("broken"));
            app_verdict.wrong = round3(count("wrong"));
            app_verdict.cosmetic = round3(count("cosmetic"));
            app_verdict.work = Some(round1(work_score(
                app_verdict.broken,
                app_verdict.wrong,
                app_verdict.cosmetic,
            )));
            if bugs[index].len() >= 2 {
                app_verdict.bug_overlap = Some(round3(verify::bug_overlap(
                    &bugs[index][0],
                    &bugs[index][1],
                )));
            }
            app_verdict.bugs = bugs[index]
                .iter()
                .enumerate()
                .flat_map(|(sample, list)| list.iter().cloned().map(move |bug| (sample, bug)))
                .collect();
        }
        app_verdict.q = quality(app_verdict.cov, app_verdict.work, app_verdict.smoke);
        apply_delivery(app_verdict);
    }
    if !orders.is_empty() {
        let chosen: Vec<&str> = orders.iter().map(|order| order.better.as_str()).collect();
        let agree = chosen.len() == 2 && chosen[0] == chosen[1] && !chosen[0].is_empty();
        verdict.pairwise = Some(Pairwise {
            winner: if agree {
                chosen[0].to_string()
            } else {
                "uncertain".to_string()
            },
            flips: usize::from(chosen.len() == 2 && chosen[0] != chosen[1]),
            orders,
        });
    }
    verdict.apps = verdicts;
    pick_winners(&mut verdict);
    verdict.usage = usage.lock().map(|usage| usage.clone()).unwrap_or_default();
    Ok(verdict)
}

/// [`verify::merge_status`], with a requirement that a sample left out
/// counting as that sample's "missing".
fn merge_or_missing(statuses: &[&Checked], samples: usize) -> Option<String> {
    if statuses.len() < samples && statuses.iter().any(|checked| checked.status == "full") {
        return None;
    }
    verify::merge_status(statuses)
}

/// Run `jobs` at the same time; the answers in the order of the jobs.
async fn run_jobs(
    client: &Arc<dyn ModelClient>,
    jobs: Vec<(Job, CompletionRequest)>,
    usage: &Arc<std::sync::Mutex<Usage>>,
    options: &Options,
) -> Vec<(Job, Result<serde_json::Value>)> {
    let mut set = tokio::task::JoinSet::new();
    for (index, (job, request)) in jobs.into_iter().enumerate() {
        let client = client.clone();
        let usage = usage.clone();
        set.spawn(async move {
            let answer = call(client, request, usage).await;
            (index, job, answer)
        });
    }
    let mut answers = Vec::new();
    while let Some(joined) = set.join_next().await {
        if let Ok((index, job, answer)) = joined {
            if let (Some(dir), Ok((_, text))) = (&options.raw_dir, &answer) {
                let _ = std::fs::create_dir_all(dir);
                let _ = std::fs::write(dir.join(format!("{}.txt", job.name())), text);
            }
            answers.push((index, job, answer.map(|(value, _)| value)));
        }
    }
    answers.sort_by_key(|(index, _, _)| *index);
    answers
        .into_iter()
        .map(|(_, job, answer)| (job, answer))
        .collect()
}

/// q_if_built and q_delivered of one app from its Q and delivery status.
pub fn apply_delivery(app: &mut AppVerdict) {
    app.q_if_built = app.q;
    app.q_delivered = match (app.q, &app.delivered) {
        (Some(q), Some(delivered)) => {
            let (q, does_not_run) = delivery::q_delivered(q, delivered);
            app.does_not_run = does_not_run;
            Some(q)
        }
        _ => None,
    };
}

/// The higher of two scores by label (`tie` within 0.5 points).
fn higher(apps: &[AppVerdict], pick: impl Fn(&AppVerdict) -> Option<f64>) -> Option<String> {
    let [a, b] = apps else {
        return None;
    };
    let (x, y) = (pick(a)?, pick(b)?);
    Some(if (x - y).abs() < 0.5 {
        "tie".to_string()
    } else if x > y {
        a.label.clone()
    } else {
        b.label.clone()
    })
}

/// The winner (by q_delivered, else Q), the winner if built (Q), and the
/// pairwise check of the winner if built.
pub fn pick_winners(verdict: &mut Verdict) {
    verdict.winner_if_built = higher(&verdict.apps, |app| app.q);
    // An app that runs always beats one that does not, whatever their
    // q_delivered (a running app with few pages or many failed tests can
    // score below 0.25 Q of a high-Q app that does not build).
    let delivered = match &verdict.apps[..] {
        [a, b]
            if a.q_delivered.is_some()
                && b.q_delivered.is_some()
                && a.does_not_run != b.does_not_run =>
        {
            Some(if a.does_not_run {
                b.label.clone()
            } else {
                a.label.clone()
            })
        }
        _ => higher(&verdict.apps, |app| app.q_delivered),
    };
    verdict.winner_basis = if delivered.is_some() {
        Some("q_delivered".to_string())
    } else {
        verdict.winner_if_built.as_ref().map(|_| "q".to_string())
    };
    verdict.winner = delivered.or_else(|| verdict.winner_if_built.clone());
    verdict.verdict = verdict
        .winner_if_built
        .as_ref()
        .map(|winner| match &verdict.pairwise {
            Some(pair) if &pair.winner == winner => "confirmed".to_string(),
            Some(_) => "contested".to_string(),
            None => "unchecked".to_string(),
        });
}

pub fn work_score(broken: f64, wrong: f64, cosmetic: f64) -> f64 {
    (100.0 - 8.0 * broken - 3.0 * wrong - cosmetic).max(0.0)
}

/// Q from the layers present: 0.4 cov + 0.4 work + 0.2 smoke, renormalised
/// over the layers that exist.
pub fn quality(cov: Option<f64>, work: Option<f64>, smoke: Option<f64>) -> Option<f64> {
    let parts: Vec<(f64, f64)> = [(0.4, cov), (0.4, work), (0.2, smoke)]
        .into_iter()
        .filter_map(|(weight, value)| value.map(|value| (weight, value)))
        .collect();
    if cov.is_none() && work.is_none() {
        return None;
    }
    let total: f64 = parts.iter().map(|(weight, _)| weight).sum();
    Some(round1(
        parts
            .iter()
            .map(|(weight, value)| weight * value)
            .sum::<f64>()
            / total,
    ))
}

fn round1(value: f64) -> f64 {
    (value * 10.0).round() / 10.0
}

fn round3(value: f64) -> f64 {
    (value * 1000.0).round() / 1000.0
}

// ---- CLI ---------------------------------------------------------------------

#[derive(clap::Args, Clone)]
pub struct JudgeArgs {
    /// Directory of the run (it holds `slimtoken/`, and `single/` for a race).
    pub run: PathBuf,
    /// Provider preset of the judge.
    #[arg(long, default_value = "deepseek")]
    pub provider: String,
    /// The preset's model, replaced.
    #[arg(long)]
    pub model: Option<String>,
    /// Reasoning effort of the judge calls (`none` for no hint).
    #[arg(long, default_value = "high")]
    pub effort: String,
    /// Coverage and working calls per app.
    #[arg(long, default_value_t = 2)]
    pub samples: usize,
    /// Changes the blind order and the requirement shuffles.
    #[arg(long, default_value_t = 0)]
    pub seed: u64,
    /// The single-stream reference app for a `bench-slim` run (default: the
    /// pinned app of the objective, under `slimtoken-out-showdown`).
    #[arg(long, value_name = "DIR")]
    pub r#ref: Option<PathBuf>,
    /// The objective (`taskflow` or `ledgerly`) when the run has no
    /// `results.json`.
    #[arg(long)]
    pub objective: Option<String>,
    /// No pairwise calls.
    #[arg(long)]
    pub no_pairwise: bool,
    /// No model calls: the static lint, delivery and smoke layers only.
    #[arg(long)]
    pub no_llm: bool,
    /// Output token limit of each call.
    #[arg(long, default_value_t = 32_000)]
    pub max_tokens: u32,
    #[command(flatten)]
    pub runtime: RuntimeArgs,
}

/// The layers that run the app (no model call).
#[derive(clap::Args, Clone, Debug)]
pub struct RuntimeArgs {
    /// No runtime smoke (no build, no browser).
    #[arg(long)]
    pub no_smoke: bool,
    /// No local delivery checks for an app whose checks were not recorded.
    #[arg(long)]
    pub no_checks: bool,
    /// The shared toolchain (`node_modules` with next, typescript, vitest).
    #[arg(
        long,
        value_name = "DIR",
        default_value = "slimtoken-out-showdown/_deps"
    )]
    pub deps: PathBuf,
    /// The smoke runner (default: `sidecars/qa/lib/qa-judge-smoke.mjs`).
    #[arg(long, value_name = "PATH")]
    pub smoke_script: Option<PathBuf>,
    /// Time budget of the browser part, per app.
    #[arg(long, value_name = "SECS", default_value_t = 90)]
    pub smoke_budget: u64,
    /// Apps built and smoked at the same time.
    #[arg(long, default_value_t = 2)]
    pub smoke_parallel: usize,
}

impl Default for RuntimeArgs {
    fn default() -> Self {
        RuntimeArgs {
            no_smoke: true,
            no_checks: true,
            deps: PathBuf::from("slimtoken-out-showdown/_deps"),
            smoke_script: None,
            smoke_budget: 90,
            smoke_parallel: 2,
        }
    }
}

/// One app for the runtime layers: its files, the run directory entry its
/// checks were recorded under (`<run>/<arm>`), and a tag for its scratch copy.
pub struct RuntimeTarget {
    pub dir: PathBuf,
    pub record: PathBuf,
    pub tag: String,
}

/// `path` made absolute (without the `\\?\` prefix that node rejects).
fn absolute(path: &Path) -> PathBuf {
    match path.canonicalize() {
        Ok(full) => match full.to_str().and_then(|text| text.strip_prefix(r"\\?\")) {
            Some(rest) if !rest.starts_with("UNC") => PathBuf::from(rest),
            _ => full,
        },
        Err(_) => std::env::current_dir()
            .map(|dir| dir.join(path))
            .unwrap_or_else(|_| path.to_path_buf()),
    }
}

/// True when `node` runs.
fn node_works() -> bool {
    std::process::Command::new("node")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// The delivery status and the smoke report of each target, in order.
pub fn runtime_layers(
    targets: &[RuntimeTarget],
    reqs: &Reqs,
    args: &RuntimeArgs,
) -> Vec<(Option<delivery::Delivered>, Option<smoke::SmokeReport>)> {
    // The checks run in other directories: the paths must be absolute.
    let deps = absolute(&default_ref(&args.deps.to_string_lossy()));
    let script = absolute(&args.smoke_script.clone().unwrap_or_else(|| {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../sidecars/qa/lib/qa-judge-smoke.mjs")
    }));
    let flows: Vec<smoke::SmokeFlow> = reqs
        .smoke
        .as_ref()
        .map(|spec| spec.flows.clone())
        .unwrap_or_default();
    let ids: Vec<&str> = reqs
        .requirements
        .iter()
        .map(|req| req.id.as_str())
        .collect();
    for problem in smoke::check_flows(&flows, &ids) {
        eprintln!("judge: warning: smoke flow {problem}");
    }
    let node = node_works();
    let browser = smoke::find_browser();
    let smoke_off = if args.no_smoke {
        None
    } else if !node {
        Some("no node on PATH")
    } else if browser.is_none() {
        Some("no Chrome or Edge (set CHROME_PATH)")
    } else if !script.is_file() {
        Some("no smoke script")
    } else if flows.is_empty() {
        Some("the requirement list has no smoke flows")
    } else if !deps.join("node_modules").join("next").is_dir() {
        Some("no shared toolchain (--deps)")
    } else {
        None
    };
    let smoke_on = !args.no_smoke && smoke_off.is_none();
    let runner = smoke::script_runner(script.clone());
    let one = |target: &RuntimeTarget| {
        let recorded = delivery::recorded(&target.record);
        let need_local =
            !args.no_checks && node && recorded.as_ref().is_none_or(|found| found.build.is_none());
        let mut report = smoke_off.map(smoke::SmokeReport::absent);
        let mut local = None;
        if smoke_on || need_local {
            let config = smoke::SmokeConfig {
                deps: deps.clone(),
                budget: std::time::Duration::from_secs(args.smoke_budget.max(10)),
                build_timeout: std::time::Duration::from_secs(300),
                smoke: smoke_on,
                checks: need_local,
            };
            let run = smoke::run_app(
                &absolute(&target.dir),
                &target.tag,
                &flows,
                &config,
                &runner,
            );
            local = run.local;
            if smoke_on {
                report = run.smoke;
            }
        }
        if let Some(report) = report.as_mut() {
            if report.browser.is_none() {
                report.browser = browser.as_ref().map(|path| path.display().to_string());
            }
        }
        (smoke::merge_delivered(recorded, local.as_ref()), report)
    };
    let mut out = Vec::new();
    for chunk in targets.chunks(args.smoke_parallel.max(1)) {
        let done: Vec<_> = std::thread::scope(|scope| {
            let jobs: Vec<_> = chunk
                .iter()
                .map(|target| scope.spawn(|| one(target)))
                .collect();
            jobs.into_iter()
                .map(|job| job.join().unwrap_or((None, None)))
                .collect()
        });
        out.extend(done);
    }
    out
}

/// The objective text of a run: `results.json`, else `--objective`.
fn run_objective(run: &Path, name: Option<&str>) -> Result<String> {
    let results = std::fs::read_to_string(run.join("results.json"))
        .ok()
        .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok());
    if let Some(objective) = results
        .as_ref()
        .and_then(|value| value["objective"].as_str())
    {
        return Ok(objective.trim().to_string());
    }
    if let Ok(text) = std::fs::read_to_string(run.join("_meta").join("objective.txt")) {
        return Ok(text.trim().to_string());
    }
    // A `bench-slim` run: the objective's app name in its saved manifest.
    let manifest = std::fs::read_to_string(run.join("_meta").join("slimtoken-manifest.json"))
        .unwrap_or_default()
        .to_lowercase();
    let guessed = ["taskflow", "ledgerly"]
        .into_iter()
        .filter(|candidate| manifest.contains(candidate))
        .collect::<Vec<_>>();
    let name = name.or(match guessed.as_slice() {
        [one] => Some(*one),
        _ => None,
    });
    match name.and_then(builtin) {
        Some((_, objective)) => Ok(objective.trim().to_string()),
        None => bail!(
            "{} has no results.json: pass --objective taskflow|ledgerly",
            run.display()
        ),
    }
}

/// A client for the judge from a provider preset.
pub fn preset_client(
    provider: &str,
    model: Option<&str>,
) -> Result<(Arc<dyn ModelClient>, String)> {
    let preset = slimtoken::provider::provider_preset(provider)
        .with_context(|| format!("no provider preset {provider}"))?;
    let key = crate::find_key(Some(preset.key_env), None, provider)
        .with_context(|| format!("no API key for {provider}"))?;
    let model = model.unwrap_or(preset.model).to_string();
    let client = slimtoken::client::OpenAiClient::new(preset.base_url, model.clone(), Some(key))?
        .with_stream_usage(true)
        .with_stream_idle_timeout(Some(std::time::Duration::from_secs(600)))?;
    Ok((Arc::new(client), model))
}

impl JudgeArgs {
    fn options(&self, raw_dir: Option<PathBuf>) -> Options {
        Options {
            effort: (self.effort != "none").then(|| self.effort.clone()),
            samples: self.samples.max(1),
            seed: self.seed,
            pairwise: !self.no_pairwise,
            llm: !self.no_llm,
            max_tokens: self.max_tokens,
            raw_dir,
            ..Options::default()
        }
    }
}

/// A default reference path (relative to the repository root): as given
/// when it exists from the current directory, else under the repository
/// root this bench was built in (`bench/showdown/../..`).
fn default_ref(path: &str) -> PathBuf {
    let given = PathBuf::from(path);
    if given.is_dir() {
        return given;
    }
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("..");
    let under_root = root.join(path);
    if under_root.is_dir() {
        under_root
    } else {
        given
    }
}

/// `showdown judge <run>`.
pub fn run(args: &JudgeArgs) -> Result<()> {
    let run = &args.run;
    let objective = run_objective(run, args.objective.as_deref())?;
    let reqs = reqs_for(&objective, args.objective.as_deref())?;
    if !reqs.reviewed {
        eprintln!(
            "judge: warning: {}.reqs.json is not reviewed yet (\"reviewed\": false)",
            reqs.objective
        );
    }
    let stamp = run
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let slim_dir = run.join("slimtoken");
    let single_dir = if run.join("single").is_dir() {
        if let Some(ignored) = &args.r#ref {
            eprintln!(
                "judge: warning: --ref {} ignored: the run has its own single/ app",
                ignored.display()
            );
        }
        run.join("single")
    } else {
        let default = DEFAULT_REFS
            .iter()
            .find(|(name, _)| *name == reqs.objective)
            .map(|(_, path)| default_ref(path));
        args.r#ref
            .clone()
            .or(default)
            .context("no single-stream app: pass --ref DIR")?
    };
    let mut arms = [("single", single_dir), ("slimtoken", slim_dir)];
    let mut rng = bundle::Rng::new(bundle::seed_of(&stamp) ^ args.seed);
    if rng.next() % 2 == 1 {
        arms.swap(0, 1);
    }
    let mut apps = Vec::new();
    let out = run.join("_judge");
    std::fs::create_dir_all(&out)?;
    for ((_, dir), label) in arms.iter().zip(["X", "Y"]) {
        let files = bundle::load_app(dir)?;
        let text = bundle::bundle(&files);
        bundle::guard(&text, Some(&stamp)).with_context(|| format!("app {label}"))?;
        std::fs::write(out.join(format!("bundle-{label}.txt")), &text)?;
        apps.push(App {
            label: label.to_string(),
            files,
            ..App::default()
        });
    }
    // The layers that run the apps: delivery status and smoke.
    let targets: Vec<RuntimeTarget> = arms
        .iter()
        .zip(["X", "Y"])
        .map(|((_, dir), label)| RuntimeTarget {
            dir: dir.clone(),
            record: dir.clone(),
            tag: label.to_string(),
        })
        .collect();
    for (app, (delivered, smoke)) in
        apps.iter_mut()
            .zip(runtime_layers(&targets, &reqs, &args.runtime))
    {
        app.delivered = delivered;
        app.smoke = smoke;
    }
    let client = if args.no_llm {
        None
    } else {
        Some(preset_client(&args.provider, args.model.as_deref())?.0)
    };
    let options = args.options(Some(out.join("raw")));
    let runtime = tokio::runtime::Runtime::new().context("failed to start the async runtime")?;
    let verdict = runtime.block_on(judge_apps(client, &objective, &reqs, &apps, &options))?;
    let mut json = serde_json::to_value(&verdict)?;
    json["provider"] = serde_json::json!(args.provider);
    json["samples"] = serde_json::json!(options.samples);
    json["seed"] = serde_json::json!(args.seed);
    json["effort"] = serde_json::json!(options.effort);
    json["lens_note"] = serde_json::json!(LENS_OVERLAP_NOTE);
    std::fs::write(run.join("judge.json"), serde_json::to_string_pretty(&json)?)?;
    let mapping: BTreeMap<&str, &str> = ["X", "Y"]
        .into_iter()
        .zip(arms.iter().map(|(arm, _)| *arm))
        .collect();
    std::fs::write(
        out.join("mapping.json"),
        serde_json::to_string_pretty(&mapping)?,
    )?;
    print_summary(&verdict, &mapping);
    Ok(())
}

/// One line on an app's delivery status.
pub fn delivered_line(delivered: &delivery::Delivered) -> String {
    let state = |value: Option<bool>| match value {
        Some(true) => "ok",
        Some(false) => "FAIL",
        None => "?",
    };
    let pages = match (delivered.pages_passed, delivered.pages_total) {
        (Some(passed), Some(total)) => format!("{passed}/{total}"),
        _ => state(delivered.pages).to_string(),
    };
    format!(
        "install {} build {} tests {} (failed {}) pages {} [{}]",
        state(delivered.install),
        state(delivered.build),
        state(delivered.tests),
        delivered.failed_tests,
        pages,
        delivered.source
    )
}

/// One line on an app's smoke report.
pub fn smoke_line(smoke: &smoke::SmokeReport) -> String {
    match (&smoke.error, smoke.score) {
        (Some(error), None) => format!("absent: {error}"),
        _ => format!(
            "{} = {}/{} (failed {}, not found {}, skipped {}) routes {}/{} console errors {} build {}s browser {}s",
            smoke.score.map_or("-".to_string(), |score| format!("{score:.1}")),
            smoke.passed + usize::from(smoke.routes_total > 0 && smoke.routes_ok == smoke.routes_total),
            smoke.attempted,
            smoke.failed,
            smoke.not_found,
            smoke.skipped,
            smoke.routes_ok,
            smoke.routes_total,
            smoke.console_errors,
            smoke.build_seconds.map_or("-".to_string(), |s| format!("{s:.1}")),
            smoke.smoke_seconds.map_or("-".to_string(), |s| format!("{s:.1}")),
        ),
    }
}

/// The printed summary, unblinded through `mapping` (label -> arm).
pub fn print_summary(verdict: &Verdict, mapping: &BTreeMap<&str, &str>) {
    let name = |label: &str| mapping.get(label).copied().unwrap_or("?").to_string();
    let show =
        |value: Option<f64>| value.map_or_else(|| "-".to_string(), |value| format!("{value:.1}"));
    for app in &verdict.apps {
        let lints: Vec<String> = app
            .lint
            .iter()
            .filter(|(_, count)| **count > 0)
            .map(|(lint, count)| format!("{lint} {count}"))
            .collect();
        println!(
            "quality {} ({}) cov {} work {} smoke {} q_if_built {} q_delivered {}{} bugs {}/{}/{} unverified {} agreement {} kappa {}{} lint [{}]{}",
            name(&app.label),
            app.label,
            show(app.cov),
            show(app.work),
            show(app.smoke),
            show(app.q),
            show(app.q_delivered),
            if app.does_not_run { " (does not run)" } else { "" },
            app.broken,
            app.wrong,
            app.cosmetic,
            app.unverified_bugs,
            app.req_agreement.map_or_else(|| "-".to_string(), |v| format!("{v:.2}")),
            app.kappa.map_or_else(|| "-".to_string(), |v| format!("{v:.2}")),
            if app.low_confidence { " (low confidence)" } else { "" },
            lints.join(", "),
            if app.errors.is_empty() {
                String::new()
            } else {
                format!(" errors {}", app.errors.len())
            },
        );
    }
    if let Some(pair) = &verdict.pairwise {
        let winner = match pair.winner.as_str() {
            "X" | "Y" => name(&pair.winner),
            other => other.to_string(),
        };
        println!("pairwise {winner} (flips {})", pair.flips);
    }
    for app in &verdict.apps {
        if let Some(delivered) = &app.delivered {
            println!(
                "delivered {} ({}) {}",
                name(&app.label),
                app.label,
                delivered_line(delivered)
            );
        }
        if let Some(smoke) = &app.smoke_report {
            println!(
                "smoke {} ({}) {}",
                name(&app.label),
                app.label,
                smoke_line(smoke)
            );
        }
    }
    let unblind = |label: &str| match label {
        "X" | "Y" => name(label),
        other => other.to_string(),
    };
    if let Some(winner) = &verdict.winner {
        println!(
            "winner {} by {}; if built {} ({}); {}",
            unblind(winner),
            verdict.winner_basis.as_deref().unwrap_or("q"),
            verdict
                .winner_if_built
                .as_deref()
                .map_or("-".to_string(), unblind),
            verdict.verdict.as_deref().unwrap_or("unchecked"),
            verdict.score_note
        );
    }
    println!("note: {LENS_OVERLAP_NOTE}");
    let usage = &verdict.usage;
    if usage.calls > 0 {
        println!(
            "judge calls {} failed {} tokens in {} (cached {}) out {} cost ~${:.3}",
            usage.calls,
            usage.failed_calls,
            usage.input_tokens,
            usage.cached_input_tokens,
            usage.output_tokens,
            usage.cost_usd
        );
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use slimtoken::Completion;
    use std::time::Duration;

    /// A judge client with canned answers: the coverage calls mark every
    /// requirement full with a quote from the bundle's first line (and one
    /// requirement missing in one sample only), the working calls report one
    /// real and one invented bug, the pairwise calls pick the app whose
    /// bundle has `better` in it.
    pub(crate) struct MockJudge;

    pub(crate) fn completion(text: String) -> Completion {
        Completion {
            text,
            provider: None,
            input_tokens: 1000,
            output_tokens: 100,
            reasoning_tokens: None,
            cached_input_tokens: Some(500),
            finish_reason: None,
            usage_is_estimated: false,
            elapsed: Duration::ZERO,
            time_to_first_token: None,
        }
    }

    #[slimtoken::async_trait]
    impl ModelClient for MockJudge {
        async fn complete(&self, request: CompletionRequest) -> anyhow::Result<Completion> {
            let shared = request.shared_context.clone().unwrap_or_default();
            // The first numbered line: `=== path (N lines) ===` then `    1| code`.
            let mut lines = shared.lines();
            let header = lines.next().unwrap_or_default();
            let path = header
                .trim_start_matches("=== ")
                .split(" (")
                .next()
                .unwrap_or_default()
                .to_string();
            let first = lines
                .next()
                .unwrap_or_default()
                .split("| ")
                .nth(1)
                .unwrap_or_default()
                .to_string();
            let user = &request.user;
            let text = if user.starts_with("COVERAGE") || user.starts_with("TIEBREAK") {
                let ids: Vec<String> = user
                    .lines()
                    .filter_map(|line| {
                        let (id, _) = line.split_once(": ")?;
                        (id.len() <= 3 && id.chars().all(|ch| ch.is_ascii_alphanumeric()))
                            .then(|| id.to_string())
                    })
                    .collect();
                let flip = user.starts_with("COVERAGE")
                    && request.temperature > 0.0
                    && user.find("G1:") < user.find("G2:");
                let entries: Vec<serde_json::Value> = ids
                    .iter()
                    .map(|id| {
                        if id == "G2" && flip {
                            serde_json::json!({"id": id, "status": "missing", "searched": ["zzzz-not-there"]})
                        } else {
                            serde_json::json!({"id": id, "status": "full", "evidence": [{"file": path, "line": 1, "quote": first}]})
                        }
                    })
                    .collect();
                serde_json::json!({ "requirements": entries }).to_string()
            } else if user.starts_with("WORKING") {
                serde_json::json!({"bugs": [
                    {"severity": "wrong", "class": 2, "req": "", "file": path, "line": 1, "quote": first, "effect": "x"},
                    {"severity": "broken", "class": 3, "req": "", "file": path, "line": 1, "quote": "not in the code", "effect": "y"}
                ]})
                .to_string()
            } else {
                let b_at = shared.find("##### APP B").unwrap_or(0);
                let better_at = shared.find("better").unwrap_or(usize::MAX);
                let better = if better_at < b_at { "A" } else { "B" };
                format!("```json\n{{\"better\": \"{better}\", \"margin\": \"clear\", \"why\": \"w\"}}\n```")
            };
            Ok(completion(text))
        }
    }

    fn reqs() -> Reqs {
        serde_json::from_value(serde_json::json!({
            "objective": "t", "objective_sha256": "x", "reviewed": false,
            "requirements": [
                {"id": "G1", "weight": 1, "text": "one"},
                {"id": "G2", "weight": 1, "text": "two"},
                {"id": "Q1", "weight": 0.5, "text": "three"}
            ]
        }))
        .unwrap()
    }

    #[test]
    fn builtin_requirement_lists_match_their_objectives() {
        for name in ["taskflow", "ledgerly"] {
            let (reqs, objective) = builtin(name).expect(name);
            assert_eq!(reqs.objective, name);
            assert_eq!(
                reqs.objective_sha256,
                bundle::objective_sha(objective),
                "{name}"
            );
            assert!(reqs.requirements.len() >= 30);
            let ids: std::collections::BTreeSet<&str> = reqs
                .requirements
                .iter()
                .map(|req| req.id.as_str())
                .collect();
            assert_eq!(ids.len(), reqs.requirements.len(), "unique ids in {name}");
            assert!(reqs_for(objective, None).is_ok());
        }
        assert!(reqs_for("something else", Some("taskflow")).is_err());
    }

    #[test]
    fn score_formula() {
        assert_eq!(work_score(1.0, 2.0, 3.0), 83.0);
        assert_eq!(work_score(20.0, 0.0, 0.0), 0.0);
        assert_eq!(quality(Some(80.0), Some(60.0), None), Some(70.0));
        assert_eq!(quality(Some(80.0), Some(60.0), Some(100.0)), Some(76.0));
        assert_eq!(quality(None, None, Some(1.0)), None);
    }

    #[tokio::test]
    async fn the_judge_verifies_merges_and_scores_with_a_mock_client() {
        let apps = vec![
            App {
                label: "X".to_string(),
                files: vec![(
                    "app/page.tsx".to_string(),
                    "export default function P() { return 1; }\n".to_string(),
                )],
                ..App::default()
            },
            App {
                label: "Y".to_string(),
                files: vec![(
                    "app/page.tsx".to_string(),
                    "// the better app\nexport default 2;\n".to_string(),
                )],
                ..App::default()
            },
        ];
        let client: Arc<dyn ModelClient> = Arc::new(MockJudge);
        let verdict = judge_apps(
            Some(client),
            "objective",
            &reqs(),
            &apps,
            &Options::default(),
        )
        .await
        .unwrap();
        assert_eq!(verdict.apps.len(), 2);
        for app in &verdict.apps {
            // The invented bug is dropped; the real one counts.
            assert_eq!(app.unverified_bugs, 2, "{}", app.label);
            assert_eq!((app.broken, app.wrong), (0.0, 1.0));
            assert_eq!(app.work, Some(97.0));
            assert!(app.cov.is_some());
            assert_eq!(app.requirements.len(), 3);
            assert!(app.errors.is_empty(), "{:?}", app.errors);
        }
        let pair = verdict.pairwise.as_ref().unwrap();
        assert_eq!(pair.winner, "Y");
        assert_eq!(pair.flips, 0);
        assert!(verdict.usage.calls >= 10);
        assert!(verdict.usage.cost_usd > 0.0);
        // Without a client: lint only.
        let lint_only = judge_apps(None, "objective", &reqs(), &apps, &Options::default())
            .await
            .unwrap();
        assert!(lint_only
            .apps
            .iter()
            .all(|app| app.cov.is_none() && app.lint.contains_key("dead-route")));
        assert!(lint_only.pairwise.is_none());
    }

    #[tokio::test]
    async fn a_build_that_fails_loses_as_delivered_but_not_if_built() {
        // Same code in both apps; X's smoke passes, Y's build failed.
        let files = vec![(
            "app/page.tsx".to_string(),
            "export default function P() { return 1; }\n".to_string(),
        )];
        let smoke = smoke::score_report(&serde_json::json!({
            "browser": "fake",
            "routes": [{"route": "/", "ok": true}],
            "flows": [{"id": "a", "status": "passed"}, {"id": "b", "status": "not_found"}]
        }));
        let apps = vec![
            App {
                label: "X".to_string(),
                files: files.clone(),
                delivered: Some(delivery::Delivered {
                    source: "results.json".into(),
                    build: Some(true),
                    failed_tests: 1,
                    ..Default::default()
                }),
                smoke: Some(smoke),
            },
            App {
                label: "Y".to_string(),
                files,
                delivered: Some(delivery::Delivered {
                    source: "results.json".into(),
                    build: Some(false),
                    ..Default::default()
                }),
                smoke: Some(smoke::SmokeReport::absent("the app does not build")),
            },
        ];
        let client: Arc<dyn ModelClient> = Arc::new(MockJudge);
        let options = Options {
            pairwise: false,
            ..Options::default()
        };
        let verdict = judge_apps(Some(client), "objective", &reqs(), &apps, &options)
            .await
            .unwrap();
        let (x, y) = (&verdict.apps[0], &verdict.apps[1]);
        // X: smoke 2 of 3 = 66.7 enters Q at 0.2; Y has no smoke.
        assert_eq!(x.smoke, Some(66.7));
        assert_eq!(y.smoke, None);
        let qx = quality(x.cov, x.work, Some(66.7)).unwrap();
        assert_eq!(x.q_if_built, Some(qx));
        assert_eq!(x.q_delivered, Some(((qx - 2.0) * 10.0).round() / 10.0));
        assert!(y.does_not_run);
        assert_eq!(
            y.q_delivered,
            Some(((0.25 * y.q.unwrap()) * 10.0).round() / 10.0)
        );
        assert_eq!(verdict.winner.as_deref(), Some("X"));
        assert_eq!(verdict.winner_basis.as_deref(), Some("q_delivered"));
        assert!(verdict.score_note.contains("smoke on 1 of 2 apps"));
        assert_eq!(verdict.verdict.as_deref(), Some("unchecked"));
    }

    #[test]
    fn an_app_that_does_not_run_never_wins_as_delivered() {
        // X does not build (Q 92 -> 23.0); Y runs but renders 1 of 8 pages
        // (Q 85 -> 21.3). By the numbers X is higher; Y still wins.
        let app = |label: &str, q: f64, delivered: delivery::Delivered| {
            let mut app = AppVerdict {
                label: label.to_string(),
                q: Some(q),
                delivered: Some(delivered),
                ..AppVerdict::default()
            };
            apply_delivery(&mut app);
            app
        };
        let x = app(
            "X",
            92.0,
            delivery::Delivered {
                build: Some(false),
                ..Default::default()
            },
        );
        let y = app(
            "Y",
            85.0,
            delivery::Delivered {
                build: Some(true),
                pages: Some(false),
                pages_passed: Some(1),
                pages_total: Some(8),
                ..Default::default()
            },
        );
        assert!(x.does_not_run && !y.does_not_run);
        assert!(x.q_delivered > y.q_delivered);
        let mut verdict = Verdict {
            apps: vec![x, y],
            ..Verdict::default()
        };
        pick_winners(&mut verdict);
        assert_eq!(verdict.winner.as_deref(), Some("Y"));
        assert_eq!(verdict.winner_if_built.as_deref(), Some("X"));
        // Both run (or neither): the numbers decide.
        verdict.apps[0].does_not_run = false;
        pick_winners(&mut verdict);
        assert_eq!(verdict.winner.as_deref(), Some("X"));
    }

    #[test]
    fn judge_writes_judge_json_for_a_run_without_a_model() {
        let dir = std::env::temp_dir().join(format!("showdown-judge-run-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        for arm in ["single", "slimtoken"] {
            std::fs::create_dir_all(dir.join(arm).join("app")).unwrap();
            std::fs::write(
                dir.join(arm).join("app/page.tsx"),
                "export default function P() { return <a href=\"/nowhere\">x</a>; }\n",
            )
            .unwrap();
        }
        std::fs::write(
            dir.join("results.json"),
            serde_json::json!({ "objective": TASKFLOW_OBJECTIVE.trim() }).to_string(),
        )
        .unwrap();
        let args = JudgeArgs {
            run: dir.clone(),
            provider: "deepseek".into(),
            model: None,
            effort: "high".into(),
            samples: 2,
            seed: 0,
            r#ref: None,
            objective: None,
            no_pairwise: false,
            no_llm: true,
            max_tokens: 1000,
            runtime: RuntimeArgs::default(),
        };
        run(&args).unwrap();
        let json: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join("judge.json")).unwrap())
                .unwrap();
        assert_eq!(json["objective"], "taskflow");
        assert_eq!(json["apps"][0]["lint"]["dead-route"], 1);
        let mapping: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(dir.join("_judge/mapping.json")).unwrap(),
        )
        .unwrap();
        assert_ne!(mapping["X"], mapping["Y"]);
        assert!(dir.join("_judge/bundle-X.txt").is_file());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
