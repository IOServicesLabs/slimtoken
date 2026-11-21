//! `pipeline::build` with mock clients and fixture manifests: the plans of
//! the default options are the golden plans of `plan_golden.rs`, its report
//! has the shape of `execute_plan`'s, and `BuildOptions::for_stack` stages
//! a stack only when its staging is measured.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use slimtoken::architect::{Manifest, ManifestSource};
use slimtoken::check::{CheckMode, CheckResult, Checker};
use slimtoken::client::MockClient;
use slimtoken::pipeline::{build, BuildHooks, BuildOptions, Cancelled, PlanSource, Sink};
use slimtoken::profile::{Profile, Staging, NEXTJS, PYTHON, RUST};
use slimtoken::provider::{DEEPSEEK_PRESET, MUSE_PRESET};
use slimtoken::scheduler::{execute_plan, SchedulerConfig};
use slimtoken::{async_trait, Completion, CompletionRequest, ModelClient, Plan, RunReport};

const OBJECTIVE: &str = "Build a task tracker with projects, a board, a calendar and reports.";

fn fixtures() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

fn manifest(name: &str) -> Manifest {
    let text = std::fs::read_to_string(fixtures().join(name)).unwrap();
    serde_json::from_str(&text).unwrap()
}

fn golden(name: &str) -> String {
    std::fs::read_to_string(fixtures().join(format!("golden/{name}.json")))
        .unwrap()
        .replace("\r\n", "\n")
}

fn mock() -> Arc<dyn ModelClient> {
    Arc::new(MockClient::new(Duration::ZERO))
}

/// A fresh directory under the system temp directory.
fn temp_dir(name: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "slimtoken-pipeline-{name}-{}-{nanos}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Default options that replay `manifest` with `profile` (64 tasks, as the
/// golden plans).
fn replay<'a>(profile: &'static Profile, manifest: Manifest) -> BuildOptions<'a> {
    BuildOptions {
        max_tasks: 64,
        profile: Some(profile),
        manifest: ManifestSource {
            saved: Some(manifest),
            prepare: None,
        },
        ..BuildOptions::default()
    }
}

/// The keys of a JSON value, by path: what a host that reads the report
/// can rely on, without the values (times differ between runs).
fn shape(value: &serde_json::Value, path: &str, out: &mut BTreeSet<String>) {
    match value {
        serde_json::Value::Object(map) => {
            for (key, value) in map {
                // Task ids are data, not shape.
                let key = if path.ends_with(".tasks") { "*" } else { key };
                let path = format!("{path}.{key}");
                out.insert(path.clone());
                shape(value, &path, out);
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                shape(item, &format!("{path}[]"), out);
            }
        }
        _ => {}
    }
}

fn report_shape(report: &RunReport) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    shape(&serde_json::to_value(report).unwrap(), "", &mut out);
    out
}

async fn same_plan_and_report_shape(profile: &'static Profile, fixture: &str, golden_name: &str) {
    let outcome = build(OBJECTIVE, mock(), replay(profile, manifest(fixture)))
        .await
        .unwrap();
    let plan = serde_json::to_string_pretty(&outcome.plan).unwrap() + "\n";
    assert!(
        plan == golden(golden_name),
        "the plan of {} differs from golden/{golden_name}.json",
        profile.name
    );
    assert!(outcome.manifest.is_some());
    assert!(outcome.repair.is_none() && outcome.files.is_empty());
    let report = outcome.report.expect("the wave ran");
    let direct = execute_plan(&outcome.plan, mock(), SchedulerConfig::default())
        .await
        .unwrap();
    assert_eq!(report_shape(&report), report_shape(&direct));
    assert_eq!(report.task_count, direct.task_count);
    assert!(report.is_success());
}

#[tokio::test]
async fn python_builds_plan_the_golden_plan_and_report_like_execute_plan() {
    same_plan_and_report_shape(&PYTHON, "plan-manifest-python.json", "plan-python-native").await;
    same_plan_and_report_shape(&PYTHON, "plan-manifest.json", "plan-python").await;
}

#[tokio::test]
async fn rust_builds_plan_the_golden_plan_and_report_like_execute_plan() {
    same_plan_and_report_shape(&RUST, "plan-manifest-rust.json", "plan-rust-native").await;
    same_plan_and_report_shape(&RUST, "plan-manifest.json", "plan-rust").await;
}

#[tokio::test]
async fn nextjs_default_options_plan_the_golden_plan() {
    // NEXTJS staging is not measured yet: `for_stack` with the DeepSeek
    // preset changes no plan option and keeps the model sink.
    let options = BuildOptions::for_stack(&NEXTJS, Some(&DEEPSEEK_PRESET));
    assert!(!NEXTJS.staging.measured);
    assert!(!options.plan.staged && options.plan.min_test_shards == 0);
    assert_eq!(options.sink, Sink::Model);
    same_plan_and_report_shape(&NEXTJS, "plan-manifest.json", "plan-nextjs").await;
}

/// NEXTJS with its staging marked as measured.
fn measured_nextjs() -> &'static Profile {
    Box::leak(Box::new(Profile {
        staging: Staging {
            measured: true,
            ..NEXTJS.staging
        },
        ..NEXTJS
    }))
}

#[tokio::test]
async fn a_measured_stack_builds_a_staged_plan_and_writes_its_files() {
    let profile = measured_nextjs();
    let dir = temp_dir("nextjs");
    let mut options = BuildOptions::for_stack(profile, Some(&DEEPSEEK_PRESET));
    assert!(options.plan.staged && options.plan.one_type_owner);
    assert_eq!(options.plan.min_test_shards, 6);
    assert_eq!(options.sink, Sink::Assemble);
    assert_eq!(options.scheduler.reasoning_effort.as_deref(), Some("none"));
    // A preset that is not DeepSeek, or no preset, stages nothing and keeps
    // the model sink (an unstaged wave writes fragments).
    for preset in [None, Some(&MUSE_PRESET)] {
        let unstaged = BuildOptions::for_stack(profile, preset);
        assert!(!unstaged.plan.staged);
        assert_eq!(unstaged.sink, Sink::Model);
    }
    options.max_tasks = 64;
    options.manifest = ManifestSource {
        saved: Some(manifest("plan-manifest.json")),
        prepare: None,
    };
    options.out_dir = Some(dir.clone());
    let outcome = build(OBJECTIVE, mock(), options).await.unwrap();
    let plan = &outcome.plan;
    let id = |title: &str| {
        plan.tasks
            .iter()
            .find(|task| task.title == title)
            .unwrap_or_else(|| panic!("no task {title}"))
    };
    assert!(id("lib/types.ts").deps.is_empty(), "types come first");
    assert_eq!(id("lib/tasks.ts").deps, vec![id("lib/types.ts").id.clone()]);
    assert!(
        id("app/page.tsx").deps.contains(&id("lib/tasks.ts").id),
        "pages wait for the lib"
    );
    assert!(
        plan.cache_primes.is_empty(),
        "the types stage warms the cache"
    );
    let report = outcome.report.expect("the wave ran");
    assert!(report.is_success());
    // Files on disk: every path task, as written.
    assert!(!outcome.files.is_empty());
    for file in &outcome.files {
        let text = std::fs::read_to_string(dir.join(&file.path)).unwrap();
        assert_eq!(text.len(), file.bytes, "{}", file.path);
    }
    for title in ["lib/types.ts", "app/page.tsx", "package.json"] {
        assert!(dir.join(title).is_file(), "{title} on disk");
    }
    let _ = std::fs::remove_dir_all(dir);
}

/// Passes every check and counts them.
#[derive(Default)]
struct Passing {
    checks: std::sync::Mutex<usize>,
}

#[async_trait]
impl Checker for Passing {
    async fn check(
        &self,
        files: &BTreeMap<String, String>,
        _: CheckMode,
        _: &str,
    ) -> Result<CheckResult> {
        *self.checks.lock().unwrap() += 1;
        assert!(!files.is_empty());
        Ok(CheckResult {
            passed: true,
            ..CheckResult::default()
        })
    }
}

/// Fails every check with an error (a checker that cannot run).
struct Broken;

#[async_trait]
impl Checker for Broken {
    async fn check(
        &self,
        _: &BTreeMap<String, String>,
        _: CheckMode,
        _: &str,
    ) -> Result<CheckResult> {
        anyhow::bail!("the checker cannot run")
    }
}

#[tokio::test]
async fn a_checker_checks_the_files_and_they_are_written_again() {
    let dir = temp_dir("check");
    let checker = Passing::default();
    let options = BuildOptions {
        checker: Some(&checker),
        out_dir: Some(dir.clone()),
        ..replay(&PYTHON, manifest("plan-manifest-python.json"))
    };
    let outcome = build(OBJECTIVE, mock(), options).await.unwrap();
    let summary = outcome.repair.expect("the check ran");
    assert!(summary.after.passed && summary.rounds.is_empty());
    assert!(*checker.checks.lock().unwrap() >= 1);
    let report = outcome.report.unwrap();
    assert!(report.phases.reconcile_ms.is_some());
    assert!(!outcome.files.is_empty());
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn a_check_error_fails_the_build_unless_it_is_kept() {
    let failing = BuildOptions {
        checker: Some(&Broken),
        ..replay(&PYTHON, manifest("plan-manifest-python.json"))
    };
    let error = build(OBJECTIVE, mock(), failing).await.unwrap_err();
    assert!(format!("{error:#}").contains("cannot run"));

    let dir = temp_dir("kept");
    let kept = BuildOptions {
        checker: Some(&Broken),
        keep_check_errors: true,
        out_dir: Some(dir.clone()),
        ..replay(&PYTHON, manifest("plan-manifest-python.json"))
    };
    let outcome = build(OBJECTIVE, mock(), kept).await.unwrap();
    assert!(outcome.repair.is_none());
    assert!(outcome.repair_error.is_some());
    assert!(outcome.report.unwrap().is_success());
    assert!(!outcome.files.is_empty(), "the wave's files stay on disk");
    let _ = std::fs::remove_dir_all(dir);
}

/// Records what the hooks saw; `stop` ends the build after planning,
/// `skip_check` skips the check.
#[derive(Default)]
struct Recorder {
    stop: bool,
    skip_check: bool,
    planned: Option<(bool, usize)>,
    executed: bool,
}

#[async_trait]
impl BuildHooks for Recorder {
    async fn planned(
        &mut self,
        manifest: Option<&Manifest>,
        plan: &mut Plan,
        scheduler: &mut SchedulerConfig,
        _: u64,
    ) -> Result<bool> {
        self.planned = Some((manifest.is_some(), plan.tasks.len()));
        scheduler.journal_path = None;
        Ok(!self.stop)
    }

    async fn executed(
        &mut self,
        _: &mut Plan,
        report: &mut RunReport,
        scheduler: &mut SchedulerConfig,
    ) -> Result<bool> {
        assert!(scheduler.journal_path.is_none(), "repairs write no journal");
        assert!(report.is_success());
        self.executed = true;
        Ok(!self.skip_check)
    }
}

#[tokio::test]
async fn hooks_can_stop_after_planning_and_skip_the_check() {
    let mut stop = Recorder {
        stop: true,
        ..Recorder::default()
    };
    let options = BuildOptions {
        hooks: Some(&mut stop),
        ..replay(&PYTHON, manifest("plan-manifest-python.json"))
    };
    let outcome = build(OBJECTIVE, mock(), options).await.unwrap();
    assert!(outcome.report.is_none());
    let tasks = outcome.plan.tasks.len();
    assert_eq!(stop.planned, Some((true, tasks)));
    assert!(!stop.executed);

    let checker = Passing::default();
    let mut skip = Recorder {
        skip_check: true,
        ..Recorder::default()
    };
    let given = outcome.plan;
    let options = BuildOptions {
        source: PlanSource::Given(given),
        checker: Some(&checker),
        hooks: Some(&mut skip),
        ..BuildOptions::default()
    };
    let outcome = build("", mock(), options).await.unwrap();
    assert!(outcome.manifest.is_none(), "a given plan has no manifest");
    assert_eq!(skip.planned, Some((false, tasks)));
    assert!(skip.executed);
    assert!(outcome.repair.is_none());
    assert_eq!(*checker.checks.lock().unwrap(), 0);
}

/// A planner that never answers.
struct Silent;

#[async_trait]
impl ModelClient for Silent {
    async fn complete(&self, _: CompletionRequest) -> Result<Completion> {
        std::future::pending().await
    }
}

#[tokio::test]
async fn a_cancelled_control_stops_planning() {
    let options = BuildOptions {
        profile: Some(&PYTHON),
        ..BuildOptions::default()
    };
    options.control.cancel();
    let error = build(OBJECTIVE, Arc::new(Silent), options)
        .await
        .unwrap_err();
    assert!(error.downcast_ref::<Cancelled>().is_some(), "{error:#}");
}

/// Records whether the files were on disk when `finished` ran.
struct Finish {
    dir: PathBuf,
    on_disk: Option<bool>,
    passed: Option<bool>,
}

#[async_trait]
impl BuildHooks for Finish {
    async fn finished(
        &mut self,
        _: &Plan,
        _: &RunReport,
        repair: Option<&slimtoken::repair::RepairSummary>,
        repair_error: Option<&anyhow::Error>,
    ) -> Result<()> {
        assert!(repair_error.is_none());
        self.on_disk = Some(std::fs::read_dir(&self.dir).unwrap().next().is_some());
        self.passed = repair.map(|summary| summary.after.passed);
        Ok(())
    }
}

#[tokio::test]
async fn without_the_early_write_the_files_go_to_disk_once_after_finished() {
    let dir = temp_dir("late");
    let checker = Passing::default();
    let mut finish = Finish {
        dir: dir.clone(),
        on_disk: None,
        passed: None,
    };
    let options = BuildOptions {
        checker: Some(&checker),
        out_dir: Some(dir.clone()),
        write_before_check: false,
        hooks: Some(&mut finish),
        ..replay(&PYTHON, manifest("plan-manifest-python.json"))
    };
    let outcome = build(OBJECTIVE, mock(), options).await.unwrap();
    assert_eq!(
        finish.on_disk,
        Some(false),
        "nothing written before the check"
    );
    assert_eq!(finish.passed, Some(true));
    assert!(!outcome.files.is_empty());
    for file in &outcome.files {
        assert!(dir.join(&file.path).is_file(), "{}", file.path);
    }
    let _ = std::fs::remove_dir_all(dir);
}
