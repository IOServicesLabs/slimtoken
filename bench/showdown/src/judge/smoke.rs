//! The runtime smoke layer of the judge (Q weight 0.2), and the local
//! delivery checks it shares a build with.
//!
//! For each app: build it with the shared toolchain (`next build` in a copy
//! under `<deps>/.check`, so module resolution finds `<deps>/node_modules`;
//! an app that already has `.next/BUILD_ID` and its own `node_modules` is
//! served in place), start `next start` on a free port, request each
//! static route, then run `sidecars/qa/lib/qa-judge-smoke.mjs`: it opens
//! every route in headless Chrome or Edge (uncaught exceptions, console
//! errors) and runs the requirement-driven flows of the objective
//! (`smoke.flows` in `objectives/<name>.reqs.json`).
//!
//! smoke = 100 · passed / attempted, over the flows plus one check that
//! every route renders without an uncaught exception. A flow whose control
//! is not found counts as failed (its requirement is not usable), and is
//! reported apart from a flow that found its controls and failed. A flow
//! the time budget cut off (`skipped`), a route it never loaded, and a flow
//! missing from the report also count as failed, so a slow or hanging app
//! is not scored on fewer flows than a fast one; a built app whose browser
//! run wrote no report at all (it hung the browser) scores 0. No browser,
//! no node, or an app that does not build: no smoke score (Q is
//! renormalised as before).
//!
//! Both arms of a pair are always built the same way: a copy of the app's
//! files with the shared toolchain, never the arm's own `node_modules` or
//! `.next` (whose next version can differ); the toolchain's next version is
//! recorded in the notes and the report.

use super::delivery::Delivered;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// The step kinds `qa-judge-smoke.mjs` runs.
pub const STEP_KINDS: &[&str] = &[
    "goto",
    "reload",
    "click",
    "form",
    "expectText",
    "clickText",
    "pickItem",
    "pickLink",
    "move",
    "read",
    "expect",
    "snapshot",
    "changed",
    "nonzero",
    "expectNear",
];

/// One requirement-driven flow of an objective.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SmokeFlow {
    pub id: String,
    #[serde(default)]
    pub reqs: Vec<String>,
    pub steps: Vec<serde_json::Value>,
}

/// The `smoke` section of a requirement list.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct SmokeSpec {
    #[serde(default)]
    pub note: String,
    #[serde(default)]
    pub flows: Vec<SmokeFlow>,
}

/// The kind of a step: its first key other than `optional`.
pub fn step_kind(step: &serde_json::Value) -> Option<&str> {
    step.as_object()?
        .keys()
        .map(String::as_str)
        .find(|key| *key != "optional")
}

/// Problems of a flow list: unknown step kinds, empty flows, duplicate ids,
/// requirement ids that the list does not have.
pub fn check_flows(flows: &[SmokeFlow], req_ids: &[&str]) -> Vec<String> {
    let mut problems = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    for flow in flows {
        if !seen.insert(flow.id.as_str()) {
            problems.push(format!("{}: duplicate flow id", flow.id));
        }
        if flow.steps.is_empty() {
            problems.push(format!("{}: no steps", flow.id));
        }
        for req in &flow.reqs {
            if !req_ids.contains(&req.as_str()) {
                problems.push(format!("{}: unknown requirement {req}", flow.id));
            }
        }
        for (index, step) in flow.steps.iter().enumerate() {
            match step_kind(step) {
                Some(kind) if STEP_KINDS.contains(&kind) => {}
                other => problems.push(format!(
                    "{}: step {} has an unknown kind {:?}",
                    flow.id,
                    index + 1,
                    other
                )),
            }
        }
    }
    problems
}

/// One flow's result.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
pub struct FlowResult {
    pub id: String,
    #[serde(default)]
    pub reqs: Vec<String>,
    /// `passed`, `failed`, `not_found` or `skipped`.
    pub status: String,
    #[serde(default)]
    pub detail: Option<String>,
    #[serde(default)]
    pub ms: u64,
}

/// The smoke layer's result on one app.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct SmokeReport {
    pub browser: Option<String>,
    /// 100 · passed / attempted; `None` when nothing ran.
    pub score: Option<f64>,
    pub attempted: usize,
    pub passed: usize,
    pub failed: usize,
    pub not_found: usize,
    pub skipped: usize,
    pub routes_ok: usize,
    pub routes_total: usize,
    pub flows: Vec<FlowResult>,
    pub exceptions: Vec<String>,
    pub console_errors: usize,
    pub build_seconds: Option<f64>,
    pub smoke_seconds: Option<f64>,
    /// The time budget ran out before every route and flow ran (what it cut
    /// off counts as failed).
    #[serde(default)]
    pub budget_exhausted: bool,
    /// The toolchain the app was built and served with.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub toolchain: Option<String>,
    /// Why there is no score (no browser, no build, ...), or why the run
    /// scored 0 (the browser run wrote no report).
    pub error: Option<String>,
}

impl SmokeReport {
    pub fn absent(why: impl Into<String>) -> Self {
        SmokeReport {
            error: Some(why.into()),
            ..SmokeReport::default()
        }
    }

    /// A built, started app whose browser run wrote no report (the script
    /// timed out: the app hung the browser): every flow and the routes check
    /// failed, score 0.
    pub fn no_report(why: impl Into<String>, flows: usize) -> Self {
        SmokeReport {
            score: Some(0.0),
            attempted: flows + 1,
            failed: flows,
            budget_exhausted: true,
            error: Some(why.into()),
            ..SmokeReport::default()
        }
    }
}

/// Add to a raw report what it lacks: a route it never loaded (`ok: false`)
/// and a flow it never ran (`skipped`), so that the score's denominator is
/// the whole request. A report the watchdog cut short has both.
pub fn complete_report(raw: &mut serde_json::Value, routes: &[String], flows: &[SmokeFlow]) {
    if raw.get("error").is_some_and(|error| !error.is_null()) {
        return;
    }
    let Some(object) = raw.as_object_mut() else {
        return;
    };
    let mut added = false;
    let listed = |value: Option<&serde_json::Value>, key: &str| -> Vec<String> {
        value
            .and_then(serde_json::Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|item| item[key].as_str().map(str::to_string))
            .collect()
    };
    let seen_routes = listed(object.get("routes"), "route");
    let seen_flows = listed(object.get("flows"), "id");
    let mut route_list = object
        .get("routes")
        .and_then(serde_json::Value::as_array)
        .cloned()
        .unwrap_or_default();
    for route in routes.iter().filter(|route| !seen_routes.contains(route)) {
        added = true;
        route_list.push(serde_json::json!({
            "route": route, "ok": false, "error": "not in the report (not visited)"
        }));
    }
    let mut flow_list = object
        .get("flows")
        .and_then(serde_json::Value::as_array)
        .cloned()
        .unwrap_or_default();
    for flow in flows.iter().filter(|flow| !seen_flows.contains(&flow.id)) {
        added = true;
        flow_list.push(serde_json::json!({
            "id": flow.id, "reqs": flow.reqs, "status": "skipped",
            "detail": "not in the report (not run)"
        }));
    }
    object.insert("routes".into(), route_list.into());
    object.insert("flows".into(), flow_list.into());
    if added {
        object.insert("budget_exhausted".into(), true.into());
    }
}

/// Score the report that `qa-judge-smoke.mjs` wrote.
pub fn score_report(raw: &serde_json::Value) -> SmokeReport {
    let mut report = SmokeReport {
        browser: raw["browser"].as_str().map(str::to_string),
        console_errors: raw["console_errors"].as_u64().unwrap_or(0) as usize,
        smoke_seconds: raw["ms"].as_f64().map(|ms| (ms / 100.0).round() / 10.0),
        budget_exhausted: raw["budget_exhausted"].as_bool().unwrap_or(false),
        error: raw["error"].as_str().map(str::to_string),
        ..SmokeReport::default()
    };
    let routes = raw["routes"].as_array().cloned().unwrap_or_default();
    report.routes_total = routes.len();
    report.routes_ok = routes
        .iter()
        .filter(|route| route["ok"].as_bool() == Some(true))
        .count();
    for route in &routes {
        for exception in route["exceptions"].as_array().into_iter().flatten() {
            if report.exceptions.len() < 5 {
                report.exceptions.push(format!(
                    "{}: {}",
                    route["route"].as_str().unwrap_or("?"),
                    exception.as_str().unwrap_or_default()
                ));
            }
        }
    }
    report.flows = raw["flows"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|flow| FlowResult {
            id: flow["id"].as_str().unwrap_or_default().to_string(),
            reqs: serde_json::from_value(flow["reqs"].clone()).unwrap_or_default(),
            status: match flow["status"].as_str() {
                Some(status @ ("passed" | "not_found" | "skipped")) => status.to_string(),
                _ => "failed".to_string(),
            },
            detail: flow["detail"].as_str().map(str::to_string),
            ms: flow["ms"].as_u64().unwrap_or(0),
        })
        .collect();
    let count = |status: &str| {
        report
            .flows
            .iter()
            .filter(|flow| flow.status == status)
            .count()
    };
    report.passed = count("passed");
    report.failed = count("failed");
    report.not_found = count("not_found");
    report.skipped = count("skipped");
    // The routes are one check: every route renders (a route the budget cut
    // off is `ok: false`).
    let routes_check = usize::from(report.routes_total > 0);
    let routes_pass =
        usize::from(report.routes_total > 0 && report.routes_ok == report.routes_total);
    // A flow the budget cut off counts as failed: a slow app is scored on
    // the same flows as a fast one.
    report.attempted =
        report.passed + report.failed + report.not_found + report.skipped + routes_check;
    let passed = report.passed + routes_pass;
    report.score = (report.attempted > 0 && report.error.is_none())
        .then(|| (1000.0 * passed as f64 / report.attempted as f64).round() / 10.0);
    report
}

/// Where and how the layer runs.
#[derive(Clone, Debug)]
pub struct SmokeConfig {
    /// The shared toolchain (`node_modules` with next, typescript, vitest).
    pub deps: PathBuf,
    /// The browser part's time budget per app.
    pub budget: Duration,
    pub build_timeout: Duration,
    /// Run the smoke flows (else only the local checks).
    pub smoke: bool,
    /// Run the local delivery checks (tests and pages) too.
    pub checks: bool,
}

/// A Chrome or Edge binary: `CHROME_PATH`, else the usual places.
pub fn find_browser() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("CHROME_PATH").map(PathBuf::from) {
        if path.is_file() {
            return Some(path);
        }
    }
    let local = std::env::var("LOCALAPPDATA").unwrap_or_default();
    let candidates = [
        r"C:\Program Files\Google\Chrome\Application\chrome.exe".to_string(),
        r"C:\Program Files (x86)\Google\Chrome\Application\chrome.exe".to_string(),
        format!(r"{local}\Google\Chrome\Application\chrome.exe"),
        r"C:\Program Files (x86)\Microsoft\Edge\Application\msedge.exe".to_string(),
        r"C:\Program Files\Microsoft\Edge\Application\msedge.exe".to_string(),
        "/usr/bin/google-chrome".to_string(),
        "/usr/bin/chromium".to_string(),
        "/usr/bin/chromium-browser".to_string(),
        "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome".to_string(),
    ];
    candidates
        .into_iter()
        .map(PathBuf::from)
        .find(|path| path.is_file())
}

/// Run `command` for at most `timeout`: (exit 0, output, timed out).
fn run_for(mut command: Command, timeout: Duration) -> (bool, String, bool) {
    let child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn();
    let Ok(mut child) = child else {
        return (false, "could not start the command".to_string(), false);
    };
    let mut readers = Vec::new();
    for stream in [
        child
            .stdout
            .take()
            .map(|s| Box::new(s) as Box<dyn Read + Send>),
        child
            .stderr
            .take()
            .map(|s| Box::new(s) as Box<dyn Read + Send>),
    ]
    .into_iter()
    .flatten()
    {
        readers.push(std::thread::spawn(move || {
            let mut stream = stream;
            let mut text = String::new();
            let _ = stream.read_to_string(&mut text);
            text
        }));
    }
    let started = Instant::now();
    let (ok, timed_out) = loop {
        match child.try_wait() {
            Ok(Some(status)) => break (status.success(), false),
            Ok(None) if started.elapsed() > timeout => {
                crate::launch::kill_tree(child.id());
                let _ = child.wait();
                break (false, true);
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(100)),
            Err(_) => break (false, false),
        }
    };
    let text: String = readers
        .into_iter()
        .filter_map(|reader| reader.join().ok())
        .collect::<Vec<_>>()
        .join("\n");
    (ok, text, timed_out)
}

fn node_command(script: &Path, args: &[&str], cwd: &Path) -> Command {
    let mut command = Command::new("node");
    command
        .arg(script)
        .args(args)
        .current_dir(cwd)
        .env("CI", "1")
        .env("NO_COLOR", "1")
        .env("NEXT_TELEMETRY_DISABLED", "1");
    command
}

/// Copy the app's files (not its dependencies, build output or run data).
fn copy_app(from: &Path, to: &Path) -> Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)?.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let path = entry.path();
        if path.is_dir() {
            if !super::bundle::SKIP_DIRS.contains(&name.as_str()) {
                copy_app(&path, &to.join(&name))?;
            }
        } else if !name.ends_with(".log") && !name.ends_with(".tsbuildinfo") {
            std::fs::copy(&path, to.join(&name))?;
        }
    }
    Ok(())
}

/// The static routes of the app in `dir`.
fn static_routes(dir: &Path) -> Vec<String> {
    let mut found = Vec::new();
    for app in ["app", "src/app"] {
        if dir.join(app).is_dir() {
            crate::pages::routes(&dir.join(app), "", &mut found);
            break;
        }
    }
    found.sort();
    found
}

/// What the browser part gets.
pub struct SmokeRequest<'a> {
    pub url: String,
    pub routes: &'a [String],
    pub flows: &'a [SmokeFlow],
    pub budget: Duration,
}

/// The browser part: `qa-judge-smoke.mjs`, as a function so that tests can
/// replace it.
pub type Runner<'a> = &'a (dyn Fn(&SmokeRequest) -> Result<serde_json::Value> + Sync);

/// The real browser part: run the script and read its report.
pub fn script_runner(
    script: PathBuf,
) -> impl Fn(&SmokeRequest) -> Result<serde_json::Value> + Sync {
    move |request: &SmokeRequest| {
        let scratch = std::env::temp_dir().join(format!(
            "showdown-smoke-{}-{}",
            std::process::id(),
            crate::launch::free_port().unwrap_or(0)
        ));
        std::fs::create_dir_all(&scratch)?;
        let flows_file = scratch.join("flows.json");
        let out_file = scratch.join("smoke.json");
        std::fs::write(
            &flows_file,
            serde_json::to_string(&serde_json::json!({ "flows": request.flows }))?,
        )?;
        let cdp_port = crate::launch::free_port().unwrap_or(9340).to_string();
        let budget_ms = request.budget.as_millis().to_string();
        let routes = request.routes.join(",");
        let flows_arg = flows_file.display().to_string();
        let out_arg = out_file.display().to_string();
        let args = [
            "--url",
            request.url.as_str(),
            "--routes",
            routes.as_str(),
            "--flows",
            flows_arg.as_str(),
            "--out",
            out_arg.as_str(),
            "--budget-ms",
            budget_ms.as_str(),
            "--cdp-port",
            cdp_port.as_str(),
        ];
        // The script bounds itself (a CDP timeout per call, a watchdog 10 s
        // after the budget that writes the report and exits); this is the
        // last resort. A report it wrote before being killed is still read.
        let (_, output, timed_out) = run_for(
            node_command(&script, &args, &scratch),
            request.budget + Duration::from_secs(30),
        );
        let text = std::fs::read_to_string(&out_file);
        let _ = std::fs::remove_dir_all(&scratch);
        match text {
            Ok(text) => Ok(serde_json::from_str(&text)?),
            Err(_) if timed_out => anyhow::bail!("the smoke script timed out"),
            Err(_) => anyhow::bail!(
                "the smoke script wrote no report: {}",
                output.lines().last().unwrap_or("no output")
            ),
        }
    }
}

/// The result on one app: the local checks and the smoke report.
#[derive(Clone, Debug, Default)]
pub struct AppRun {
    pub local: Option<Delivered>,
    pub smoke: Option<SmokeReport>,
}

/// Build, serve and smoke the app in `dir` (`tag` names its scratch copy).
pub fn run_app(
    dir: &Path,
    tag: &str,
    flows: &[SmokeFlow],
    config: &SmokeConfig,
    runner: Runner,
) -> AppRun {
    match run_app_inner(dir, tag, flows, config, runner) {
        Ok(run) => run,
        Err(error) => AppRun {
            local: None,
            smoke: config
                .smoke
                .then(|| SmokeReport::absent(format!("{error:#}"))),
        },
    }
}

fn run_app_inner(
    dir: &Path,
    tag: &str,
    flows: &[SmokeFlow],
    config: &SmokeConfig,
    runner: Runner,
) -> Result<AppRun> {
    let modules = config.deps.join("node_modules");
    if !modules.join("next").is_dir() {
        anyhow::bail!(
            "no toolchain: {} has no node_modules/next",
            config.deps.display()
        );
    }
    let toolchain = toolchain_name(&modules);
    let routes = static_routes(dir);
    let mut local = Delivered {
        source: "local".to_string(),
        notes: vec![format!("built with {toolchain}")],
        ..Delivered::default()
    };
    // Every arm is built the same way: a copy of its files with the shared
    // toolchain (an arm's own node_modules or .next would mean another next
    // version for one arm of the pair).
    let copy = config
        .deps
        .join(".check")
        .join(format!("judge-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&copy);
    copy_app(dir, &copy).with_context(|| format!("failed to copy {}", dir.display()))?;
    let (root, next) = (copy.clone(), modules.join("next/dist/bin/next"));
    let result = (|| -> Result<AppRun> {
        let started = Instant::now();
        let (ok, out, timed_out) =
            run_for(node_command(&next, &["build"], &root), config.build_timeout);
        let build_seconds = Some((started.elapsed().as_secs_f64() * 10.0).round() / 10.0);
        local.build = Some(ok);
        if !ok {
            let reason = if timed_out {
                "timed out".to_string()
            } else {
                out.lines()
                    .map(str::trim)
                    .find(|line| line.contains("error") || line.contains("Error"))
                    .unwrap_or("failed")
                    .chars()
                    .take(160)
                    .collect()
            };
            local.notes.push(format!("build: {reason}"));
        }
        if config.checks {
            let has_tests = super::bundle::load_app(&root)
                .map(|files| files.iter().any(|(path, _)| path.contains(".test.")))
                .unwrap_or(false);
            if has_tests && modules.join("vitest").is_dir() {
                let (ok, out, _) = run_for(
                    node_command(
                        &modules.join("vitest/vitest.mjs"),
                        &["run", "--root", ".", "--reporter=verbose"],
                        &root,
                    ),
                    Duration::from_secs(180),
                );
                let (_, failed, passed) = crate::gate::vitest_report(&out);
                local.tests = Some(ok);
                local.failed_tests = if ok { 0 } else { (failed as usize).max(1) };
                local.tests_total = Some((failed + passed) as usize);
            }
        }
        if local.build != Some(true) {
            let smoke = config.smoke.then(|| {
                SmokeReport::absent(format!(
                    "the app does not build with the shared toolchain ({})",
                    local.notes.last().map_or("no reason", String::as_str)
                ))
            });
            return Ok(AppRun {
                local: Some(local),
                smoke,
            });
        }
        let port = crate::launch::free_port().context("no free port")?;
        let child = node_command(&next, &["start", "-p", &port.to_string()], &root)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .context("could not start the built app")?;
        let pid = child.id();
        let mut child = child;
        let started = Instant::now();
        let up = loop {
            if crate::launch::port_open(port) {
                break true;
            }
            if matches!(child.try_wait(), Ok(Some(_)))
                || started.elapsed() > Duration::from_secs(45)
            {
                break false;
            }
            std::thread::sleep(Duration::from_millis(150));
        };
        let mut smoke = None;
        if up {
            // No static route found (a pages/ router, an app/ elsewhere): the
            // pages stay unknown rather than "none renders".
            if config.checks && !routes.is_empty() {
                let passed = routes
                    .iter()
                    .filter(|route| crate::pages::status(port, route) == Some(200))
                    .count();
                local.pages = Some(passed == routes.len());
                local.pages_passed = Some(passed);
                local.pages_total = Some(routes.len());
            }
            if config.smoke {
                let request = SmokeRequest {
                    url: format!("http://127.0.0.1:{port}"),
                    routes: &routes,
                    flows,
                    budget: config.budget,
                };
                let mut report = match runner(&request) {
                    Ok(mut raw) => {
                        complete_report(&mut raw, &routes, flows);
                        score_report(&raw)
                    }
                    // The app built and started, and the browser run wrote
                    // nothing: it hung the browser. That is a failure of the
                    // app, not a missing layer.
                    Err(error) => SmokeReport::no_report(format!("{error:#}"), flows.len()),
                };
                report.build_seconds = build_seconds;
                report.toolchain = Some(toolchain.clone());
                smoke = Some(report);
            }
        } else {
            if !routes.is_empty() {
                local.pages = Some(false);
                local.pages_passed = Some(0);
                local.pages_total = Some(routes.len());
            }
            smoke = config
                .smoke
                .then(|| SmokeReport::absent("the built app did not start"));
        }
        crate::launch::kill_tree(pid);
        let _ = child.wait();
        Ok(AppRun {
            local: Some(local),
            smoke,
        })
    })();
    for _ in 0..10 {
        if std::fs::remove_dir_all(&copy).is_ok() || !copy.exists() {
            break;
        }
        std::thread::sleep(Duration::from_millis(300));
    }
    result
}

/// `next <version> (shared toolchain)` from the toolchain's package.json.
fn toolchain_name(modules: &Path) -> String {
    let version = std::fs::read_to_string(modules.join("next").join("package.json"))
        .ok()
        .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
        .and_then(|package| package["version"].as_str().map(str::to_string))
        .unwrap_or_else(|| "?".to_string());
    format!("next {version} (shared toolchain)")
}

/// Fill the checks that `recorded` does not know from `local`.
pub fn merge_delivered(
    recorded: Option<Delivered>,
    local: Option<&Delivered>,
) -> Option<Delivered> {
    match (recorded, local) {
        (Some(mut recorded), Some(local)) => {
            // Which toolchain the local checks used.
            recorded.notes.extend(
                local
                    .notes
                    .iter()
                    .filter(|note| note.starts_with("built with"))
                    .map(|note| format!("local checks {note}")),
            );
            if recorded.build.is_none() {
                recorded.build = local.build;
            }
            if recorded.tests.is_none() && local.tests.is_some() {
                recorded.tests = local.tests;
                recorded.failed_tests = local.failed_tests;
                recorded.tests_total = local.tests_total;
            }
            if recorded.pages.is_none() && local.pages.is_some() {
                recorded.pages = local.pages;
                recorded.pages_passed = local.pages_passed;
                recorded.pages_total = local.pages_total;
            }
            if local.build.is_some() && local.build != recorded.build {
                recorded.notes.push(format!(
                    "the shared toolchain's build {} (the recorded one {})",
                    if local.build == Some(true) {
                        "passes"
                    } else {
                        "fails"
                    },
                    if recorded.build == Some(true) {
                        "passed"
                    } else {
                        "failed"
                    },
                ));
            }
            Some(recorded)
        }
        (recorded, local) => recorded.or_else(|| local.cloned()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_builtin_flows_are_valid() {
        for name in ["taskflow", "ledgerly"] {
            let (reqs, _) = super::super::builtin(name).unwrap();
            let spec = reqs.smoke.as_ref().expect(name);
            assert!(spec.flows.len() >= 4, "{name}");
            let ids: Vec<&str> = reqs
                .requirements
                .iter()
                .map(|req| req.id.as_str())
                .collect();
            assert_eq!(
                check_flows(&spec.flows, &ids),
                Vec::<String>::new(),
                "{name}"
            );
        }
        let bad: Vec<SmokeFlow> = serde_json::from_value(serde_json::json!([
            {"id": "a", "reqs": ["ZZ"], "steps": [{"optional": true, "teleport": "/"}]},
            {"id": "a", "steps": []}
        ]))
        .unwrap();
        let problems = check_flows(&bad, &["G1"]);
        assert_eq!(problems.len(), 4, "{problems:?}");
        assert_eq!(
            step_kind(&serde_json::json!({"optional": true, "read": {}})),
            Some("read")
        );
    }

    #[test]
    fn not_found_counts_as_failed_but_is_reported_apart() {
        let raw = serde_json::json!({
            "browser": "chrome",
            "ms": 12345,
            "console_errors": 2,
            "routes": [
                {"route": "/", "ok": true, "exceptions": []},
                {"route": "/board", "ok": false, "exceptions": ["TypeError: x is undefined"]}
            ],
            "flows": [
                {"id": "create-project", "reqs": ["P2"], "status": "passed", "ms": 900},
                {"id": "create-task", "reqs": ["TC"], "status": "not_found", "detail": "form: no field"},
                {"id": "board-move", "reqs": ["B3"], "status": "failed", "detail": "move: not in done"},
                {"id": "late", "status": "skipped"},
                {"id": "odd", "status": "weird"}
            ]
        });
        let report = score_report(&raw);
        assert_eq!(
            (
                report.passed,
                report.failed,
                report.not_found,
                report.skipped
            ),
            (1, 2, 1, 1)
        );
        // 5 flows (the skipped one counts as failed) + the routes check, which
        // fails: 1 of 6.
        assert_eq!(report.attempted, 6);
        assert_eq!(report.score, Some(16.7));
        assert_eq!((report.routes_ok, report.routes_total), (1, 2));
        assert_eq!(report.exceptions, vec!["/board: TypeError: x is undefined"]);
        assert_eq!(report.smoke_seconds, Some(12.3));
        // A report with an error has no score.
        let broken = score_report(
            &serde_json::json!({"error": "no Chrome/Edge binary found", "routes": [], "flows": []}),
        );
        assert_eq!(broken.score, None);
        assert_eq!(broken.attempted, 0);
    }

    #[test]
    fn what_the_budget_cut_off_counts_as_failed() {
        let flows: Vec<SmokeFlow> = serde_json::from_value(serde_json::json!([
            {"id": "a", "steps": [{"goto": "/"}]},
            {"id": "b", "steps": [{"goto": "/"}]},
            {"id": "c", "steps": [{"goto": "/"}]}
        ]))
        .unwrap();
        let routes = vec!["/".to_string(), "/board".to_string()];
        // A fast app: everything ran, one flow failed.
        let mut fast = serde_json::json!({
            "routes": [{"route": "/", "ok": true}, {"route": "/board", "ok": true}],
            "flows": [{"id": "a", "status": "passed"}, {"id": "b", "status": "passed"},
                      {"id": "c", "status": "failed"}]
        });
        // A slow app: the budget cut off a route and two flows, and the
        // watchdog wrote a report without the last flow.
        let mut slow = serde_json::json!({
            "budget_exhausted": true,
            "routes": [{"route": "/", "ok": true},
                       {"route": "/board", "ok": false, "error": "not visited"}],
            "flows": [{"id": "a", "status": "passed"}, {"id": "b", "status": "skipped"}]
        });
        complete_report(&mut fast, &routes, &flows);
        complete_report(&mut slow, &routes, &flows);
        let fast = score_report(&fast);
        let slow = score_report(&slow);
        // The same denominator: 3 flows + the routes check.
        assert_eq!((fast.attempted, slow.attempted), (4, 4));
        assert_eq!(fast.score, Some(75.0));
        assert_eq!(slow.score, Some(25.0));
        assert_eq!(slow.skipped, 2);
        assert!(slow.budget_exhausted && !fast.budget_exhausted);
        // A route missing from the report fails the routes check.
        let mut partial = serde_json::json!({"routes": [{"route": "/", "ok": true}], "flows": []});
        complete_report(&mut partial, &routes, &[]);
        let partial = score_report(&partial);
        assert_eq!((partial.routes_ok, partial.routes_total), (1, 2));
        assert_eq!(partial.score, Some(0.0));
        // A report with an error (no browser) is left alone: no score.
        let mut broken = serde_json::json!({"error": "no Chrome", "routes": [], "flows": []});
        complete_report(&mut broken, &routes, &flows);
        assert_eq!(score_report(&broken).score, None);
        // No report at all from a started app: 0, not absent.
        let none = SmokeReport::no_report("the smoke script timed out", 3);
        assert_eq!((none.score, none.attempted), (Some(0.0), 4));
    }

    #[test]
    fn a_hanging_runner_scores_zero_not_absent() {
        // run_app's runner error path, through the public pieces: a runner
        // that fails is turned into no_report by run_app_inner; this checks
        // the report a judge then folds into Q.
        let report = SmokeReport::no_report("the smoke script timed out", 5);
        assert_eq!(report.failed, 5);
        assert!(report.error.unwrap().contains("timed out"));
        assert!(super::super::quality(Some(80.0), Some(80.0), report.score).unwrap() < 80.0);
    }

    #[test]
    fn a_fake_runner_gets_the_routes_and_flows() {
        // The browser part is a function: a fake one sees the request.
        let flows: Vec<SmokeFlow> =
            serde_json::from_value(serde_json::json!([{"id": "f", "steps": [{"goto": "/"}]}]))
                .unwrap();
        let routes = vec!["/".to_string(), "/board".to_string()];
        let fake = |request: &SmokeRequest| -> Result<serde_json::Value> {
            assert_eq!(request.routes.len(), 2);
            Ok(serde_json::json!({
                "browser": "fake",
                "routes": request.routes.iter().map(|route| serde_json::json!({"route": route, "ok": true})).collect::<Vec<_>>(),
                "flows": request.flows.iter().map(|flow| serde_json::json!({"id": flow.id, "status": "passed"})).collect::<Vec<_>>(),
            }))
        };
        let runner: Runner = &fake;
        let request = SmokeRequest {
            url: "http://127.0.0.1:1".into(),
            routes: &routes,
            flows: &flows,
            budget: Duration::from_secs(1),
        };
        let report = score_report(&runner(&request).unwrap());
        assert_eq!(report.score, Some(100.0));
        assert_eq!(report.attempted, 2);
    }

    #[test]
    fn local_checks_fill_what_is_not_recorded() {
        let recorded = Delivered {
            source: "slimtoken-repair.json".into(),
            tests: Some(true),
            ..Delivered::default()
        };
        let local = Delivered {
            source: "local".into(),
            build: Some(false),
            pages: Some(false),
            pages_passed: Some(0),
            pages_total: Some(8),
            ..Delivered::default()
        };
        let merged = merge_delivered(Some(recorded), Some(&local)).unwrap();
        assert_eq!(merged.build, Some(false));
        assert_eq!(merged.pages_total, Some(8));
        assert_eq!(merged.source, "slimtoken-repair.json");
        assert!(merge_delivered(None, None).is_none());
        assert_eq!(merge_delivered(None, Some(&local)).unwrap().source, "local");
    }

    #[test]
    fn an_app_without_a_toolchain_has_no_smoke_score() {
        let dir = std::env::temp_dir().join(format!("showdown-smoke-none-{}", std::process::id()));
        let _ = std::fs::create_dir_all(dir.join("app"));
        let config = SmokeConfig {
            deps: dir.join("no-deps"),
            budget: Duration::from_secs(1),
            build_timeout: Duration::from_secs(1),
            smoke: true,
            checks: true,
        };
        let never = |_: &SmokeRequest| -> Result<serde_json::Value> { panic!("not reached") };
        let run = run_app(&dir, "t", &[], &config, &never);
        let smoke = run.smoke.unwrap();
        assert!(smoke.score.is_none());
        assert!(smoke.error.unwrap().contains("no toolchain"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
