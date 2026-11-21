//! `showdown judge-validate`: run the judge on the pairs of the blind
//! Claude audits and compare its verdicts with the audits' (the ground
//! truth the judge should track). Two fixtures are built in: the audit of
//! 2026-11-15 (`audit-20261115.json`, p1..p5) and the audit of 2026-11-16
//! (`audit-20261116.json`, a1..a5, with the score of each app as delivered
//! and as if built). Per fixture it reports the winner of each pair as
//! delivered and as if built, the winner of each lens, the correlation of
//! the scores (all apps, and within each arm), the pairwise agreement, and
//! the recall of the audits' cited defects.
//!
//! The layers that do not call a model run too: the static lints, the
//! delivery status (the runs' recorded checks) and the runtime smoke
//! (`--no-smoke` skips it). `--no-llm` stops there and reports how those
//! layers alone agree with the audits; `--from-judge` instead takes cov and
//! work from each run's saved `judge.json` (no model call) and scores them
//! with the delivery and smoke layers.

use super::{
    apply_delivery, builtin, bundle, judge_apps, pick_winners, quality, runtime_layers, App,
    AppVerdict, Options, RuntimeArgs, RuntimeTarget, Verdict,
};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// The built-in fixtures: (name, JSON).
const FIXTURES: &[(&str, &str)] = &[
    (
        "audit-20261115",
        include_str!("../../judge/fixtures/audit-20261115.json"),
    ),
    (
        "audit-20261116",
        include_str!("../../judge/fixtures/audit-20261116.json"),
    ),
];

#[derive(Clone, Debug, Deserialize)]
pub struct LensSide {
    /// The score as delivered (an app that does not build scores low).
    pub score: f64,
    /// The score if it built (the audit read the code as if it did).
    #[serde(default)]
    pub score_if_built: Option<f64>,
}

impl LensSide {
    fn if_built(&self) -> f64 {
        self.score_if_built.unwrap_or(self.score)
    }
}

#[derive(Clone, Debug, Deserialize)]
pub struct Lens {
    pub better: String,
    #[serde(default)]
    pub better_if_built: Option<String>,
    #[serde(default)]
    pub margin: String,
    #[serde(rename = "X")]
    pub x: LensSide,
    #[serde(rename = "Y")]
    pub y: LensSide,
}

impl Lens {
    fn side(&self, label: &str) -> &LensSide {
        if label == "X" {
            &self.x
        } else {
            &self.y
        }
    }

    fn better_if_built(&self) -> &str {
        self.better_if_built.as_deref().unwrap_or(&self.better)
    }
}

#[derive(Clone, Debug, Deserialize)]
pub struct Audit {
    pub coverage: Lens,
    pub working: Lens,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Pair {
    pub pair: String,
    pub run: String,
    pub objective: String,
    #[serde(rename = "X")]
    pub x: String,
    #[serde(rename = "Y")]
    pub y: String,
    pub audit: Audit,
    #[serde(default)]
    pub problems: BTreeMap<String, Vec<String>>,
}

impl Pair {
    fn arm(&self, label: &str) -> &str {
        if label == "X" {
            &self.x
        } else {
            &self.y
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
pub struct Fixture {
    #[serde(skip)]
    pub name: String,
    /// In-sample or held out: what of the judge was written or tuned from
    /// this audit (printed with its report).
    #[serde(default)]
    pub sample: String,
    pub pairs: Vec<Pair>,
}

/// A built-in fixture by name.
pub fn builtin_fixture(name: &str) -> Option<Fixture> {
    FIXTURES
        .iter()
        .find(|(fixture, _)| *fixture == name)
        .map(|(name, text)| {
            let mut fixture: Fixture =
                serde_json::from_str(text).expect("the built-in fixture parses");
            fixture.name = name.to_string();
            fixture
        })
}

#[cfg(test)]
pub fn default_fixture() -> Fixture {
    builtin_fixture("audit-20261115").expect("built in")
}

/// The fixture at `path`: a built-in name (`audit-20261116`) or a file.
pub fn load_fixture(path: &Path) -> Result<Fixture> {
    let text = path.to_string_lossy();
    if let Some(fixture) = builtin_fixture(text.trim_end_matches(".json")) {
        if !path.is_file() {
            return Ok(fixture);
        }
    }
    let mut fixture: Fixture = serde_json::from_str(
        &std::fs::read_to_string(path)
            .with_context(|| format!("failed to read {}", path.display()))?,
    )
    .with_context(|| format!("{} is not an audit fixture", path.display()))?;
    fixture.name = path
        .file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_default();
    Ok(fixture)
}

#[derive(clap::Args, Clone)]
pub struct ValidateArgs {
    /// An audit fixture: a file, or a built-in name (`audit-20261115`,
    /// `audit-20261116`). Repeat it for several; default: both built-in ones.
    #[arg(long)]
    pub fixture: Vec<PathBuf>,
    /// Directory of the runs (`<runs>/<run>/<arm>`).
    #[arg(long, default_value = "slimtoken-out-showdown")]
    pub runs: PathBuf,
    /// The audit's blind copies instead (`<apps>/<pair>/<label>`).
    #[arg(long)]
    pub apps: Option<PathBuf>,
    #[arg(long, default_value = "deepseek")]
    pub provider: String,
    #[arg(long)]
    pub model: Option<String>,
    #[arg(long, default_value = "high")]
    pub effort: String,
    #[arg(long, default_value_t = 2)]
    pub samples: usize,
    #[arg(long, default_value_t = 0)]
    pub seed: u64,
    /// Judge each pair this many times (seeds `seed`, `seed + 1`, ...).
    #[arg(long, default_value_t = 1)]
    pub repeats: usize,
    /// No model calls: the static lint, delivery and smoke layers only.
    #[arg(long)]
    pub no_llm: bool,
    /// No model calls: cov and work from each run's saved `judge.json`
    /// (unblinded through its `_judge/mapping.json`), with the delivery and
    /// smoke layers of this judge.
    #[arg(long, conflicts_with = "no_llm")]
    pub from_judge: bool,
    #[arg(long, default_value_t = 32_000)]
    pub max_tokens: u32,
    /// Where the report goes (default: `<runs>/judge-validate.json`).
    #[arg(long)]
    pub out: Option<PathBuf>,
    #[command(flatten)]
    pub runtime: RuntimeArgs,
}

/// Pearson's r (`None` with fewer than three points or no spread).
pub fn pearson(xs: &[f64], ys: &[f64]) -> Option<f64> {
    let n = xs.len().min(ys.len());
    if n < 3 {
        return None;
    }
    let mean = |values: &[f64]| values.iter().take(n).sum::<f64>() / n as f64;
    let (mx, my) = (mean(xs), mean(ys));
    let (mut sxy, mut sxx, mut syy) = (0.0, 0.0, 0.0);
    for index in 0..n {
        let (dx, dy) = (xs[index] - mx, ys[index] - my);
        sxy += dx * dy;
        sxx += dx * dx;
        syy += dy * dy;
    }
    (sxx > 0.0 && syy > 0.0).then(|| sxy / (sxx * syy).sqrt())
}

/// Ranks with ties averaged.
fn ranks(values: &[f64]) -> Vec<f64> {
    let mut order: Vec<usize> = (0..values.len()).collect();
    order.sort_by(|a, b| values[*a].total_cmp(&values[*b]));
    let mut out = vec![0.0; values.len()];
    let mut index = 0;
    while index < order.len() {
        let mut end = index;
        while end + 1 < order.len() && values[order[end + 1]] == values[order[index]] {
            end += 1;
        }
        let rank = (index + end) as f64 / 2.0 + 1.0;
        for slot in &order[index..=end] {
            out[*slot] = rank;
        }
        index = end + 1;
    }
    out
}

pub fn spearman(xs: &[f64], ys: &[f64]) -> Option<f64> {
    pearson(&ranks(xs), &ranks(ys))
}

/// `path:line` references in an audit problem (`app/task/page.tsx:108`,
/// `lib/types.ts:8`, `lib/seed.ts:26-32`).
pub fn cited_lines(problem: &str) -> Vec<(String, usize)> {
    let mut found = Vec::new();
    for (at, _) in problem.match_indices(':') {
        let before = &problem[..at];
        let start = before
            .rfind(|ch: char| !(ch.is_ascii_alphanumeric() || "./_-[]()".contains(ch)))
            .map_or(0, |index| index + 1);
        let path = before[start..].trim_start_matches('(');
        let is_path = [".ts", ".tsx", ".css", ".md", ".json", ".mjs"]
            .iter()
            .any(|ext| path.ends_with(ext))
            && path.contains('/')
            || path == "README.md";
        let digits: String = problem[at + 1..]
            .chars()
            .take_while(char::is_ascii_digit)
            .collect();
        if is_path && !digits.is_empty() {
            if let Ok(line) = digits.parse() {
                found.push((path.to_string(), line));
            }
        }
    }
    found
}

/// Share of the audit's cited lines (problems of one app) that a verified
/// judge bug hits: same file, within 10 lines (either side of a cross-file
/// bug). (probes, found).
pub fn defect_recall(problems: &[String], verdict: &AppVerdict) -> (usize, usize) {
    let mut probes: Vec<(String, usize)> = problems
        .iter()
        .flat_map(|problem| cited_lines(problem))
        .collect();
    probes.sort();
    probes.dedup();
    let hits = |file: &str, line: &serde_json::Value, probe: &(String, usize)| {
        file.trim_start_matches("./") == probe.0
            && super::verify::line_number(line).is_some_and(|at| at.abs_diff(probe.1) <= 10)
    };
    let found = probes
        .iter()
        .filter(|probe| {
            verdict.bugs.iter().any(|(_, bug)| {
                hits(&bug.file, &bug.line, probe)
                    || bug
                        .other
                        .as_ref()
                        .is_some_and(|other| hits(&other.file, &other.line, probe))
            })
        })
        .count();
    (probes.len(), found)
}

#[derive(Debug, Default, Serialize)]
struct AppRow {
    fixture: String,
    pair: String,
    label: String,
    arm: String,
    repeat: usize,
    cov: Option<f64>,
    work: Option<f64>,
    smoke: Option<f64>,
    q_if_built: Option<f64>,
    q_delivered: Option<f64>,
    does_not_run: bool,
    delivered: Option<String>,
    audit_coverage: f64,
    audit_working: f64,
    audit_coverage_if_built: f64,
    audit_working_if_built: f64,
    probes: usize,
    found: usize,
    lint_errors: usize,
    lint_warnings: usize,
    lint: BTreeMap<String, usize>,
    smoke_flows: Vec<String>,
    smoke_details: Vec<String>,
}

/// The directory of one app of a pair.
fn app_dir(args: &ValidateArgs, pair: &Pair, label: &str) -> PathBuf {
    match &args.apps {
        Some(apps) => apps.join(&pair.pair).join(label),
        None => args.runs.join(&pair.run).join(pair.arm(label)),
    }
}

/// A verdict from a run's saved `judge.json`, relabelled to the fixture's
/// labels: cov, work, the verified bugs and the pairwise winner.
fn saved_verdict(run: &Path, pair: &Pair, apps: &[App]) -> Result<Verdict> {
    let read = |path: PathBuf| -> Result<serde_json::Value> {
        Ok(serde_json::from_str(
            &std::fs::read_to_string(&path)
                .with_context(|| format!("failed to read {}", path.display()))?,
        )?)
    };
    let judged = read(run.join("judge.json"))?;
    let mapping = read(run.join("_judge").join("mapping.json"))?;
    // arm -> the saved judge's label.
    let saved_label = |arm: &str| -> Option<String> {
        mapping
            .as_object()?
            .iter()
            .find(|(_, value)| value.as_str() == Some(arm))
            .map(|(label, _)| label.clone())
    };
    let mut verdict = Verdict {
        objective: judged["objective"].as_str().unwrap_or_default().to_string(),
        score_note: super::score_note(apps),
        ..Verdict::default()
    };
    for app in apps {
        let arm = pair.arm(&app.label);
        let label = saved_label(arm).with_context(|| format!("no {arm} in the mapping"))?;
        let saved = judged["apps"]
            .as_array()
            .and_then(|list| list.iter().find(|entry| entry["label"] == label.as_str()))
            .with_context(|| format!("no app {label} in judge.json"))?;
        let bugs: Vec<(usize, super::verify::Bug)> =
            serde_json::from_value(saved["bugs"].clone()).unwrap_or_default();
        let (lint, lint_lines) = super::lint_counts(&app.files);
        let mut entry = AppVerdict {
            label: app.label.clone(),
            cov: saved["cov"].as_f64(),
            work: saved["work"].as_f64(),
            smoke: app.smoke.as_ref().and_then(|smoke| smoke.score),
            smoke_report: app.smoke.clone(),
            delivered: app.delivered.clone(),
            bugs,
            lint,
            lint_lines,
            ..AppVerdict::default()
        };
        entry.q = quality(entry.cov, entry.work, entry.smoke);
        apply_delivery(&mut entry);
        verdict.apps.push(entry);
    }
    if let Some(winner) = judged["pairwise"]["winner"].as_str() {
        let unblinded = mapping[winner]
            .as_str()
            .and_then(|arm| ["X", "Y"].into_iter().find(|label| pair.arm(label) == arm));
        verdict.pairwise = Some(super::Pairwise {
            winner: unblinded.map_or(winner.to_string(), str::to_string),
            flips: judged["pairwise"]["flips"].as_u64().unwrap_or(0) as usize,
            orders: Vec::new(),
        });
    }
    pick_winners(&mut verdict);
    Ok(verdict)
}

pub fn run(args: &ValidateArgs) -> Result<()> {
    let fixtures: Vec<Fixture> = if args.fixture.is_empty() {
        FIXTURES
            .iter()
            .filter_map(|(name, _)| builtin_fixture(name))
            .collect()
    } else {
        args.fixture
            .iter()
            .map(|path| load_fixture(path))
            .collect::<Result<_>>()?
    };
    let client = if args.no_llm || args.from_judge {
        None
    } else {
        Some(super::preset_client(&args.provider, args.model.as_deref())?.0)
    };
    // Every app's files and runtime layers first (the smoke builds them).
    struct Job {
        fixture: usize,
        pair: usize,
        apps: Vec<App>,
    }
    let mut jobs: Vec<Job> = Vec::new();
    let mut targets: Vec<RuntimeTarget> = Vec::new();
    for (fixture_index, fixture) in fixtures.iter().enumerate() {
        for (pair_index, pair) in fixture.pairs.iter().enumerate() {
            let mut apps = Vec::new();
            for label in ["X", "Y"] {
                let dir = app_dir(args, pair, label);
                let files = bundle::load_app(&dir)?;
                bundle::guard(&bundle::bundle(&files), Some(&pair.run))
                    .with_context(|| format!("{} {label}", pair.pair))?;
                targets.push(RuntimeTarget {
                    dir: dir.clone(),
                    record: args.runs.join(&pair.run).join(pair.arm(label)),
                    tag: format!("{}-{label}", pair.pair),
                });
                apps.push(App {
                    label: label.to_string(),
                    files,
                    ..App::default()
                });
            }
            jobs.push(Job {
                fixture: fixture_index,
                pair: pair_index,
                apps,
            });
        }
    }
    // One run of the layers per app across fixtures (the same run can be in two).
    let reqs_of = |name: &str| builtin(name).map(|(reqs, _)| reqs);
    let mut layers: Vec<(
        Option<super::delivery::Delivered>,
        Option<super::smoke::SmokeReport>,
    )> = Vec::new();
    {
        let mut by_objective: BTreeMap<String, Vec<usize>> = BTreeMap::new();
        for (index, job) in jobs.iter().enumerate() {
            let pair = &fixtures[job.fixture].pairs[job.pair];
            by_objective
                .entry(pair.objective.clone())
                .or_default()
                .extend([2 * index, 2 * index + 1]);
        }
        layers.resize_with(targets.len(), || (None, None));
        for (objective, indexes) in by_objective {
            let reqs =
                reqs_of(&objective).with_context(|| format!("no requirements for {objective}"))?;
            let mut unique: Vec<usize> = Vec::new();
            for index in &indexes {
                if !unique
                    .iter()
                    .any(|seen| targets[*seen].dir == targets[*index].dir)
                {
                    unique.push(*index);
                }
            }
            let chosen: Vec<RuntimeTarget> = unique
                .iter()
                .map(|index| RuntimeTarget {
                    dir: targets[*index].dir.clone(),
                    record: targets[*index].record.clone(),
                    tag: targets[*index].tag.clone(),
                })
                .collect();
            let results = runtime_layers(&chosen, &reqs, &args.runtime);
            for index in indexes {
                let source = unique
                    .iter()
                    .position(|seen| targets[*seen].dir == targets[index].dir)
                    .unwrap_or(0);
                layers[index] = results[source].clone();
            }
        }
    }
    for (index, job) in jobs.iter_mut().enumerate() {
        for (side, app) in job.apps.iter_mut().enumerate() {
            let (delivered, smoke) = layers[2 * index + side].clone();
            app.delivered = delivered;
            app.smoke = smoke;
        }
    }
    let runtime = tokio::runtime::Runtime::new().context("failed to start the async runtime")?;
    let mut verdicts: Vec<(usize, usize, usize, Verdict)> = Vec::new();
    if args.from_judge {
        for job in &jobs {
            let pair = &fixtures[job.fixture].pairs[job.pair];
            match saved_verdict(&args.runs.join(&pair.run), pair, &job.apps) {
                Ok(verdict) => verdicts.push((job.fixture, 0, job.pair, verdict)),
                Err(error) => eprintln!("judge-validate: {}: {error:#}", pair.pair),
            }
        }
    } else {
        let mut set = tokio::task::JoinSet::new();
        let jobs = std::sync::Arc::new(jobs);
        for repeat in 0..args.repeats.max(1) {
            for (index, job) in jobs.iter().enumerate() {
                let fixture = &fixtures[job.fixture];
                let pair = &fixture.pairs[job.pair];
                let (reqs, objective) = builtin(&pair.objective)
                    .with_context(|| format!("no requirements for {}", pair.objective))?;
                let options = Options {
                    effort: (args.effort != "none").then(|| args.effort.clone()),
                    samples: args.samples.max(1),
                    seed: args.seed + repeat as u64,
                    llm: !args.no_llm,
                    max_tokens: args.max_tokens,
                    raw_dir: args
                        .out
                        .as_ref()
                        .and_then(|out| out.parent().map(Path::to_path_buf))
                        .or_else(|| Some(args.runs.clone()))
                        .map(|dir| {
                            dir.join("judge-validate-raw")
                                .join(format!("{}-{}-r{repeat}", fixture.name, pair.pair))
                        }),
                    ..Options::default()
                };
                let client = client.clone();
                let objective = objective.trim().to_string();
                let jobs = jobs.clone();
                let (fixture_index, pair_index) = (job.fixture, job.pair);
                set.spawn_on(
                    async move {
                        let apps = &jobs[index].apps;
                        let verdict = judge_apps(client, &objective, &reqs, apps, &options).await;
                        (fixture_index, repeat, pair_index, verdict)
                    },
                    runtime.handle(),
                );
            }
        }
        runtime.block_on(async {
            while let Some(joined) = set.join_next().await {
                if let Ok((fixture, repeat, pair, verdict)) = joined {
                    match verdict {
                        Ok(verdict) => verdicts.push((fixture, repeat, pair, verdict)),
                        Err(error) => {
                            eprintln!("judge-validate: pair {pair} repeat {repeat}: {error:#}")
                        }
                    }
                }
            }
        });
    }
    verdicts.sort_by_key(|(fixture, repeat, pair, _)| (*fixture, *repeat, *pair));
    let model = !args.no_llm;
    let mut reports = serde_json::Map::new();
    for (index, fixture) in fixtures.iter().enumerate() {
        let mine: Vec<(usize, usize, Verdict)> = verdicts
            .iter()
            .filter(|(fixture, ..)| *fixture == index)
            .map(|(_, repeat, pair, verdict)| (*repeat, *pair, verdict.clone()))
            .collect();
        println!("== {} ({} pairs)", fixture.name, fixture.pairs.len());
        if !fixture.sample.is_empty() {
            println!("  sample: {}", fixture.sample);
        }
        let mut one = report(fixture, &mine, model);
        one["summary"]["sample"] = serde_json::json!(fixture.sample);
        reports.insert(fixture.name.clone(), one);
    }
    let out = args
        .out
        .clone()
        .unwrap_or_else(|| args.runs.join("judge-validate.json"));
    if let Some(parent) = out.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let (mode, note) = if args.from_judge {
        (
            "from-judge",
            "the model lenses (cov, work, Q's model part) come from the saved judge.json files, \
             made with the prompts of that time; only the delivery and smoke layers are current",
        )
    } else if args.no_llm {
        (
            "no-llm",
            "static, delivery and smoke layers only (no model lens)",
        )
    } else {
        ("live", "every layer current")
    };
    println!("mode {mode}: {note}");
    std::fs::write(
        &out,
        serde_json::to_string_pretty(
            &serde_json::json!({"mode": mode, "note": note, "fixtures": reports}),
        )?,
    )?;
    println!("report {}", out.display());
    Ok(())
}

/// "X", "Y" or "tie" by the higher value (equal within `slack`).
fn by_higher(x: Option<f64>, y: Option<f64>, slack: f64) -> Option<String> {
    let (x, y) = (x?, y?);
    Some(
        if (x - y).abs() <= slack {
            "tie"
        } else if x > y {
            "X"
        } else {
            "Y"
        }
        .to_string(),
    )
}

/// The comparison with one audit, printed and returned as JSON. `model`:
/// the verdicts have model lenses (a live run or `--from-judge`).
fn report(
    fixture: &Fixture,
    verdicts: &[(usize, usize, Verdict)],
    model: bool,
) -> serde_json::Value {
    let mut rows: Vec<AppRow> = Vec::new();
    let mut pair_rows = Vec::new();
    for (repeat, index, verdict) in verdicts {
        let pair = &fixture.pairs[*index];
        let find = |label: &str| verdict.apps.iter().find(|app| app.label == label);
        for app in &verdict.apps {
            let label = app.label.as_str();
            let problems = pair.problems.get(label).cloned().unwrap_or_default();
            let (probes, found) = defect_recall(&problems, app);
            let errors = app
                .lint_lines
                .iter()
                .filter(|line| line.starts_with("ERROR"))
                .count();
            rows.push(AppRow {
                fixture: fixture.name.clone(),
                pair: pair.pair.clone(),
                label: label.to_string(),
                arm: pair.arm(label).to_string(),
                repeat: *repeat,
                cov: app.cov,
                work: app.work,
                smoke: app.smoke,
                q_if_built: app.q_if_built,
                q_delivered: app.q_delivered,
                does_not_run: app.does_not_run,
                delivered: app.delivered.as_ref().map(super::delivered_line),
                audit_coverage: pair.audit.coverage.side(label).score,
                audit_working: pair.audit.working.side(label).score,
                audit_coverage_if_built: pair.audit.coverage.side(label).if_built(),
                audit_working_if_built: pair.audit.working.side(label).if_built(),
                probes,
                found,
                lint_errors: errors,
                lint_warnings: app.lint_lines.len() - errors,
                lint: app.lint.clone(),
                smoke_flows: app
                    .smoke_report
                    .iter()
                    .flat_map(|smoke| smoke.flows.iter())
                    .map(|flow| format!("{} {}", flow.id, flow.status))
                    .collect(),
                smoke_details: app
                    .smoke_report
                    .iter()
                    .flat_map(|smoke| smoke.flows.iter())
                    .filter(|flow| flow.status != "passed")
                    .map(|flow| format!("{}: {}", flow.id, flow.detail.as_deref().unwrap_or("-")))
                    .chain(
                        app.smoke_report
                            .iter()
                            .filter_map(|smoke| smoke.error.clone()),
                    )
                    .collect(),
            });
        }
        let pick = |get: &dyn Fn(&AppVerdict) -> Option<f64>, slack: f64| {
            by_higher(find("X").and_then(get), find("Y").and_then(get), slack)
        };
        let lint_errors = |label: &str| {
            find(label).map(|app| {
                -(app
                    .lint_lines
                    .iter()
                    .filter(|line| line.starts_with("ERROR"))
                    .count() as f64)
            })
        };
        // Delivery alone: the app that runs beats the one that does not.
        let runs = |label: &str| {
            find(label).and_then(|app| {
                app.delivered
                    .as_ref()
                    .map(|_| f64::from(u8::from(!app.does_not_run)))
            })
        };
        pair_rows.push(serde_json::json!({
            "pair": pair.pair,
            "repeat": repeat,
            "audit_better": pair.audit.coverage.better,
            "audit_better_if_built": pair.audit.coverage.better_if_built(),
            "audit_margin": pair.audit.coverage.margin,
            "audit_working_better": pair.audit.working.better,
            "audit_working_better_if_built": pair.audit.working.better_if_built(),
            "q_winner": verdict.winner,
            "q_winner_basis": verdict.winner_basis,
            "q_winner_if_built": verdict.winner_if_built,
            "verdict": verdict.verdict,
            "cov_winner": pick(&|app| app.cov, 0.0),
            "work_winner": pick(&|app| app.work, 0.0),
            "smoke_winner": pick(&|app| app.smoke, 0.0),
            "delivery_winner": by_higher(runs("X"), runs("Y"), 0.0),
            "lint_winner": by_higher(lint_errors("X"), lint_errors("Y"), 0.0),
            "pairwise": verdict.pairwise.as_ref().map(|pair| pair.winner.clone()),
            "flips": verdict.pairwise.as_ref().map_or(0, |pair| pair.flips),
            "cost_usd": verdict.usage.cost_usd,
        }));
    }
    let first: Vec<&AppRow> = rows.iter().filter(|row| row.repeat == 0).collect();
    println!("static, delivery and smoke layers (no model):");
    for row in &first {
        let hits: Vec<String> = row
            .lint
            .iter()
            .filter(|(_, count)| **count > 0)
            .map(|(name, count)| format!("{name} {count}"))
            .collect();
        println!(
            "  {} {} {:<9} lint errors {} warnings {} | {} | smoke {} [{}] | audit {}/{} (if built {}/{}) [{}]",
            row.pair,
            row.label,
            row.arm,
            row.lint_errors,
            row.lint_warnings,
            row.delivered.as_deref().unwrap_or("no delivery status"),
            row.smoke.map_or_else(|| "-".to_string(), |v| format!("{v:.0}")),
            row.smoke_flows.join(", "),
            row.audit_coverage,
            row.audit_working,
            row.audit_coverage_if_built,
            row.audit_working_if_built,
            hits.join(", ")
        );
    }
    let sum_errors = |arm: &str| -> usize {
        first
            .iter()
            .filter(|row| row.arm == arm)
            .map(|row| row.lint_errors)
            .sum()
    };
    let single_flagged = first
        .iter()
        .filter(|row| row.arm == "single" && row.lint_errors > 0)
        .count();
    println!(
        "  ERROR lines: slimtoken {}, single {} (single apps with an ERROR: {single_flagged} of {})",
        sum_errors("slimtoken"),
        sum_errors("single"),
        first.iter().filter(|row| row.arm == "single").count()
    );
    // Winner agreement of a pair-row key with an audit key: the pairs where
    // the layer picks a side (a tie is not a pick).
    let count = |key: &str, audit_key: &str| {
        let decided: Vec<&serde_json::Value> = pair_rows
            .iter()
            .filter(|row| row[key].as_str().is_some_and(|pick| pick != "tie"))
            .collect();
        serde_json::json!({
            "matches": decided.iter().filter(|row| row[key] == row[audit_key]).count(),
            "of": decided.len(),
            "ties": pair_rows.iter().filter(|row| row[key] == "tie").count(),
        })
    };
    let points =
        |pick: &dyn Fn(&AppRow) -> Option<(f64, f64)>, arm: Option<&str>| -> (Vec<f64>, Vec<f64>) {
            rows.iter()
                .filter(|row| arm.is_none_or(|arm| row.arm == arm))
                .filter_map(pick)
                .unzip()
        };
    let corr = |pick: &dyn Fn(&AppRow) -> Option<(f64, f64)>, arm: Option<&str>| {
        let (judge, audit) = points(pick, arm);
        serde_json::json!({"pearson": pearson(&judge, &audit), "spearman": spearman(&judge, &audit), "n": judge.len()})
    };
    let audit_mean = |row: &AppRow| (row.audit_coverage + row.audit_working) / 2.0;
    let smoke = |row: &AppRow| row.smoke.map(|smoke| (smoke, audit_mean(row)));
    let mut summary = serde_json::json!({
        "pairs": pair_rows.len(),
        "lint": {
            "slimtoken_errors": sum_errors("slimtoken"),
            "single_errors": sum_errors("single"),
            "single_apps_flagged": single_flagged,
            "winner_matches": count("lint_winner", "audit_better"),
        },
        "delivery_winner_matches": count("delivery_winner", "audit_better"),
        "smoke_winner_matches": count("smoke_winner", "audit_better"),
        "smoke_winner_matches_if_built": count("smoke_winner", "audit_better_if_built"),
        "smoke_correlation": corr(&smoke, None),
    });
    if model {
        let cov = |row: &AppRow| row.cov.map(|cov| (cov, row.audit_coverage_if_built));
        let work = |row: &AppRow| row.work.map(|work| (work, row.audit_working_if_built));
        let delivered = |row: &AppRow| row.q_delivered.map(|q| (q, audit_mean(row)));
        // Without the apps that do not run: their low q_delivered and low
        // audit scores sit far from the rest and alone lift r.
        let delivered_running = |row: &AppRow| {
            row.q_delivered
                .filter(|_| !row.does_not_run)
                .map(|q| (q, audit_mean(row)))
        };
        let built = |row: &AppRow| {
            row.q_if_built.map(|q| {
                (
                    q,
                    (row.audit_coverage_if_built + row.audit_working_if_built) / 2.0,
                )
            })
        };
        let probes: usize = rows.iter().map(|row| row.probes).sum();
        let found: usize = rows.iter().map(|row| row.found).sum();
        let recall_of = |arm: &str| {
            let (p, f) = rows
                .iter()
                .filter(|row| row.arm == arm)
                .fold((0, 0), |(p, f), row| (p + row.probes, f + row.found));
            serde_json::json!({"probes": p, "found": f, "recall": (p > 0).then(|| f as f64 / p as f64)})
        };
        summary["q_winner_matches"] = count("q_winner", "audit_better");
        summary["q_winner_if_built_matches"] = count("q_winner_if_built", "audit_better_if_built");
        summary["cov_winner_matches"] = count("cov_winner", "audit_better_if_built");
        summary["work_winner_matches"] = count("work_winner", "audit_working_better_if_built");
        summary["pairwise_matches"] = count("pairwise", "audit_better_if_built");
        summary["flips"] = serde_json::json!(pair_rows
            .iter()
            .map(|row| row["flips"].as_u64().unwrap_or(0))
            .sum::<u64>());
        summary["correlation"] = serde_json::json!({
            "coverage": corr(&cov, None),
            "working": corr(&work, None),
            "q_delivered_vs_audit_delivered": corr(&delivered, None),
            "q_delivered_vs_audit_delivered_running_only": corr(&delivered_running, None),
            "q_if_built_vs_audit_if_built": corr(&built, None),
            "coverage_within_single": corr(&cov, Some("single")),
            "coverage_within_slimtoken": corr(&cov, Some("slimtoken")),
            "working_within_single": corr(&work, Some("single")),
            "working_within_slimtoken": corr(&work, Some("slimtoken")),
        });
        summary["defect_recall"] = serde_json::json!({
            "all": {"probes": probes, "found": found, "recall": (probes > 0).then(|| found as f64 / probes as f64)},
            "single": recall_of("single"),
            "slimtoken": recall_of("slimtoken"),
        });
        summary["cost_usd"] = serde_json::json!(pair_rows
            .iter()
            .map(|row| row["cost_usd"].as_f64().unwrap_or(0.0))
            .sum::<f64>());
        println!("judge vs audit:");
        let show =
            |value: Option<f64>| value.map_or_else(|| "-".to_string(), |v| format!("{v:.1}"));
        for row in &rows {
            println!(
                "  {} {} {:<9} r{} cov {:>5} (audit {:>3}) work {:>5} (audit {:>3}) smoke {:>5} q_if_built {:>5} q_delivered {:>5}{} (audit {:.0}) defects {}/{}",
                row.pair,
                row.label,
                row.arm,
                row.repeat,
                show(row.cov),
                row.audit_coverage_if_built,
                show(row.work),
                row.audit_working_if_built,
                show(row.smoke),
                show(row.q_if_built),
                show(row.q_delivered),
                if row.does_not_run { " (does not run)" } else { "" },
                audit_mean(row),
                row.found,
                row.probes,
            );
        }
    }
    let text = |row: &serde_json::Value, key: &str| row[key].as_str().unwrap_or("-").to_string();
    for row in &pair_rows {
        println!(
            "  {} r{}: audit {} (if built {}) | q {} by {} (if built {}, {}) cov {} work {} pairwise {} | delivery {} smoke {} lint {}",
            text(row, "pair"),
            row["repeat"],
            text(row, "audit_better"),
            text(row, "audit_better_if_built"),
            text(row, "q_winner"),
            text(row, "q_winner_basis"),
            text(row, "q_winner_if_built"),
            text(row, "verdict"),
            text(row, "cov_winner"),
            text(row, "work_winner"),
            text(row, "pairwise"),
            text(row, "delivery_winner"),
            text(row, "smoke_winner"),
            text(row, "lint_winner"),
        );
    }
    println!(
        "  summary {}",
        serde_json::to_string(&summary).unwrap_or_default()
    );
    serde_json::json!({ "summary": summary, "pairs": pair_rows, "apps": rows })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn statistics() {
        let xs = [1.0, 2.0, 3.0, 4.0];
        assert!((pearson(&xs, &[2.0, 4.0, 6.0, 8.0]).unwrap() - 1.0).abs() < 1e-9);
        assert!((pearson(&xs, &[8.0, 6.0, 4.0, 2.0]).unwrap() + 1.0).abs() < 1e-9);
        assert!(pearson(&[1.0, 1.0, 1.0], &[1.0, 2.0, 3.0]).is_none());
        assert!((spearman(&xs, &[1.0, 10.0, 100.0, 1000.0]).unwrap() - 1.0).abs() < 1e-9);
        assert_eq!(ranks(&[3.0, 1.0, 3.0]), vec![2.5, 1.0, 2.5]);
    }

    #[test]
    fn cited_lines_read_the_audit_references() {
        let problem = "Task detail: app/task/page.tsx:108 passes labels={[]}, see components/TaskForm.tsx:117 and (lib/seed.ts:26-32); README.md:42 too. Time 10:30.";
        assert_eq!(
            cited_lines(problem),
            vec![
                ("app/task/page.tsx".to_string(), 108),
                ("components/TaskForm.tsx".to_string(), 117),
                ("lib/seed.ts".to_string(), 26),
                ("README.md".to_string(), 42),
            ]
        );
        assert_eq!(
            cited_lines("app/tasks/[id]/page.tsx:12-14")[0].0,
            "app/tasks/[id]/page.tsx"
        );
    }

    #[test]
    fn the_fixture_has_five_pairs_with_the_audit_winner() {
        let fixture = default_fixture();
        assert_eq!(fixture.pairs.len(), 5);
        for pair in &fixture.pairs {
            assert_eq!(
                pair.arm(&pair.audit.coverage.better),
                "single",
                "{}",
                pair.pair
            );
            assert!(pair.problems.values().all(|list| !list.is_empty()));
        }
    }

    #[test]
    fn the_second_fixture_has_delivered_and_if_built_winners() {
        let fixture = builtin_fixture("audit-20261116").unwrap();
        assert_eq!(fixture.name, "audit-20261116");
        let pairs: Vec<&str> = fixture
            .pairs
            .iter()
            .map(|pair| pair.pair.as_str())
            .collect();
        assert_eq!(pairs, ["a1", "a2", "a3", "a4", "a5"]);
        for pair in &fixture.pairs {
            assert!(pair.run.starts_with("20261116-"), "{}", pair.pair);
            assert!(pair.problems.values().all(|list| !list.is_empty()));
            for lens in [&pair.audit.coverage, &pair.audit.working] {
                assert!(lens.better_if_built.is_some(), "{}", pair.pair);
            }
        }
        // a2 and a5: the single-stream app does not build, so the audit's
        // winner as delivered differs from its winner if built.
        for id in ["a2", "a5"] {
            let pair = fixture.pairs.iter().find(|pair| pair.pair == id).unwrap();
            let lens = &pair.audit.coverage;
            assert_eq!(pair.arm(&lens.better), "slimtoken", "{id}");
            assert_eq!(pair.arm(lens.better_if_built()), "single", "{id}");
            let single = if pair.x == "single" { "X" } else { "Y" };
            assert!(lens.side(single).score < 20.0 && lens.side(single).if_built() > 70.0);
        }
        // A fixture loads by its built-in name and from a file.
        assert_eq!(
            load_fixture(Path::new("audit-20261115"))
                .unwrap()
                .pairs
                .len(),
            5
        );
        let file =
            std::env::temp_dir().join(format!("showdown-fixture-{}.json", std::process::id()));
        std::fs::write(
            &file,
            include_str!("../../judge/fixtures/audit-20261116.json"),
        )
        .unwrap();
        let loaded = load_fixture(&file).unwrap();
        assert_eq!(loaded.pairs.len(), 5);
        assert!(loaded.name.starts_with("showdown-fixture-"));
        let _ = std::fs::remove_file(&file);
        assert!(load_fixture(Path::new("no-such-fixture.json")).is_err());
    }

    #[tokio::test]
    async fn validation_report_with_a_mock_judge() {
        let fixture = default_fixture();
        let (reqs, objective) = builtin("taskflow").unwrap();
        let apps = vec![
            App {
                label: "X".into(),
                files: vec![("app/task/page.tsx".into(), "export default 1;\n".into())],
                ..App::default()
            },
            App {
                label: "Y".into(),
                files: vec![(
                    "app/page.tsx".into(),
                    "// better\nexport default 2;\n".into(),
                )],
                ..App::default()
            },
        ];
        let client: std::sync::Arc<dyn slimtoken::ModelClient> =
            std::sync::Arc::new(super::super::tests::MockJudge);
        let verdict = judge_apps(Some(client), objective, &reqs, &apps, &Options::default())
            .await
            .unwrap();
        let report = report(&fixture, &[(0, 0, verdict)], true);
        assert_eq!(report["summary"]["pairs"], 1);
        assert_eq!(
            report["summary"]["pairwise_matches"]["matches"], 1,
            "the mock picks Y, the audit's winner of p1"
        );
        assert_eq!(report["apps"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn saved_judge_json_is_relabelled_and_scored_with_delivery() {
        let dir = std::env::temp_dir().join(format!("showdown-saved-judge-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("_judge")).unwrap();
        // The saved judge called single "X" and slimtoken "Y".
        std::fs::write(
            dir.join("_judge/mapping.json"),
            r#"{"X": "single", "Y": "slimtoken"}"#,
        )
        .unwrap();
        std::fs::write(
            dir.join("judge.json"),
            serde_json::json!({
                "objective": "ledgerly",
                "apps": [
                    {"label": "X", "cov": 96.0, "work": 88.0, "bugs": []},
                    {"label": "Y", "cov": 84.0, "work": 64.0, "bugs": [[0, {"severity": "wrong", "file": "lib/a.ts", "line": 3, "quote": "x"}]]}
                ],
                "pairwise": {"winner": "X", "flips": 0}
            })
            .to_string(),
        )
        .unwrap();
        let pair: Pair = serde_json::from_value(serde_json::json!({
            "pair": "a2", "run": "r", "objective": "ledgerly", "X": "slimtoken", "Y": "single",
            "audit": {
                "coverage": {"better": "X", "better_if_built": "Y", "X": {"score": 60}, "Y": {"score": 10, "score_if_built": 84}},
                "working": {"better": "X", "better_if_built": "Y", "X": {"score": 52}, "Y": {"score": 5, "score_if_built": 76}}
            }
        }))
        .unwrap();
        let failed_build = super::super::delivery::Delivered {
            source: "results.json".into(),
            install: Some(true),
            build: Some(false),
            ..Default::default()
        };
        let built = super::super::delivery::Delivered {
            source: "results.json".into(),
            build: Some(true),
            ..Default::default()
        };
        let apps = vec![
            App {
                label: "X".into(),
                delivered: Some(built),
                ..App::default()
            },
            App {
                label: "Y".into(),
                delivered: Some(failed_build),
                ..App::default()
            },
        ];
        let verdict = saved_verdict(&dir, &pair, &apps).unwrap();
        // Fixture X is slimtoken, which the saved judge called Y.
        assert_eq!(verdict.apps[0].cov, Some(84.0));
        assert_eq!(verdict.apps[0].bugs.len(), 1);
        assert_eq!(verdict.apps[1].q_if_built, Some(92.0));
        assert_eq!(verdict.apps[1].q_delivered, Some(23.0));
        assert!(verdict.apps[1].does_not_run);
        assert_eq!(verdict.winner.as_deref(), Some("X"), "the app that runs");
        assert_eq!(verdict.winner_if_built.as_deref(), Some("Y"));
        assert_eq!(verdict.pairwise.as_ref().unwrap().winner, "Y");
        assert_eq!(verdict.verdict.as_deref(), Some("confirmed"));
        let fixture = Fixture {
            name: "t".into(),
            sample: String::new(),
            pairs: vec![pair],
        };
        let report = report(&fixture, &[(0, 0, verdict)], true);
        assert_eq!(report["summary"]["q_winner_matches"]["matches"], 1);
        assert_eq!(report["summary"]["q_winner_if_built_matches"]["matches"], 1);
        assert_eq!(report["summary"]["delivery_winner_matches"]["matches"], 1);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
