//! `pchk`: a persistent checker that is fed each file as it lands.
//!
//! The first gate run after the wave starts the compiler cold (about 1.5 s
//! for a 45-file app). This checker is a Node process (`pchk.cjs`) that
//! keeps a TypeScript builder program for the whole run: the bench's model
//! client hands it each wave file when its request closes ([`Sidecar::land`],
//! through `file_text`, so the text is what the gate reads), and the checker
//! re-checks in the background. When the wave ends, only the files of the
//! last landings are left to check (about 0.35 s). Vitest runs on the test
//! files as soon as every planned test and the project files they import
//! have landed (tests never import pages), so its result is usually ready
//! too.
//!
//! [`Sidecar::check`] gives the gate's output for a set of files: the
//! compiler's diagnostics as `tsc --noEmit -p .` prints them, the gate's
//! own rules (`gate::lint_files`), the Vitest blocks and the summary line,
//! read by the same parser as the command-line gate.
//!
//! The command-line gate still runs exactly as before; see
//! `repair::GateChecker`. The two race: the first result decides. The
//! checker's result is used only when it comes first and names problems:
//! the repair round then starts at once while the command-line check
//! finishes in the background and is compared with it ([`PchkReport`]). A
//! pass always comes from the command-line tools, so a wrong pass of this
//! checker cannot end the repair step, and the engine's
//! `Checker::confirm` replaces an early result with the command line's
//! before it ends the step on it or reports it.
//!
//! So the checker saves time only on a check that fails (the first check
//! of an app that passes gives the command line's result, then the
//! build's). The replay ([`replay`]) times the command line beside the
//! checker and the build, and once alone before the wave as a baseline.
//! Its agreement set is the saved (repaired) apps and changed versions of
//! them ([`mutations`]); the files as they stood at the end of the wave
//! are not saved, so the states and errors of a real first check are not
//! in it.

use crate::extract::Found;
use crate::gate;
use crate::repair::{is_test_file, local_specifiers, resolve, GateResult};
use anyhow::{bail, Context, Result};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};
use std::hash::{Hash, Hasher};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::{mpsc, Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

/// The Node side of the checker.
const SCRIPT: &str = include_str!("pchk.cjs");

/// Longest wait for one answer of the Node process.
const ANSWER_LIMIT: Duration = Duration::from_secs(30);

/// True unless `SHOWDOWN_OFF` holds `pchk`.
pub fn on() -> bool {
    crate::arms::step_on("pchk")
}

/// The dependency directory of a gate command (`... gate --deps <dir>`).
pub fn deps_of(gate_cmd: &str) -> Option<PathBuf> {
    let rest = gate_cmd.split_once(" --deps ")?.1.trim();
    let rest = rest.trim_matches('"');
    (!rest.is_empty()).then(|| PathBuf::from(rest))
}

/// What the checker did in one repair step, for the repair report.
#[derive(Debug, Clone, Default, Serialize)]
pub struct PchkReport {
    /// Full checks that the checker answered.
    pub checks: u32,
    /// Checks whose problems started the repair round before the command
    /// line had a result.
    pub early: u32,
    /// Checks with both results.
    pub compared: u32,
    pub agree: u32,
    /// Results that differ in the verdict, the count or the files named.
    pub disagree: u32,
    /// The checker named problems and the command line passed the fast
    /// checks (a round that was not needed).
    pub false_fail: u32,
    /// The checker passed and the command line did not. Never used as a
    /// pass: the command-line result is the one that counts.
    pub missed: u32,
    /// Checks where the checker gave no result (an error, a timeout).
    pub unavailable: u32,
    /// For each check: milliseconds from its start to the checker's result.
    pub quick_ms: Vec<u64>,
    /// For each check: milliseconds from its start to the command line's.
    pub cli_ms: Vec<u64>,
    /// The first few disagreements, one line each.
    pub notes: Vec<String>,
}

impl PchkReport {
    /// Count one comparison of the checker's `quick` result with the
    /// command line's `cli` result of the same files.
    pub fn compare(&mut self, quick: &GateResult, cli: &GateResult) {
        // A command-line result of the deep checks passed the fast checks.
        let cli_fast_passed = cli.passed || cli.deep;
        self.compared += 1;
        let same_verdict = quick.passed == cli_fast_passed;
        let same_files = cli.deep && !cli.passed
            || quick.problems.keys().eq(cli.problems.keys()) && quick.failed == cli.failed;
        if same_verdict && same_files {
            self.agree += 1;
            return;
        }
        self.disagree += 1;
        if !quick.passed && cli_fast_passed {
            self.false_fail += 1;
        }
        if quick.passed && !cli_fast_passed {
            self.missed += 1;
        }
        if self.notes.len() < 8 {
            let only = |a: &GateResult, b: &GateResult| {
                a.problems
                    .keys()
                    .filter(|path| !b.problems.contains_key(*path))
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(" ")
            };
            self.notes.push(format!(
                "checker `{}` [{}] / command line `{}` [{}]",
                quick.summary,
                only(quick, cli),
                cli.summary,
                only(cli, quick)
            ));
        }
    }
}

/// The Vitest runs of the checker, by the key of the files they read.
#[derive(Default)]
struct TestRuns {
    slot: Mutex<TestSlot>,
    done: Condvar,
}

#[derive(Default)]
struct TestSlot {
    running: BTreeSet<u64>,
    /// The process ids of the Vitest runs (for [`Sidecar::stop`]).
    pids: BTreeSet<u32>,
    /// The latest results, newest last.
    results: Vec<(u64, String)>,
}

/// The persistent checker of one benchmark run.
pub struct Sidecar {
    deps: PathBuf,
    /// The directory that holds the files (inside `deps`, so that module
    /// resolution finds its `node_modules`).
    root: PathBuf,
    script: PathBuf,
    state: Mutex<State>,
    /// The Node process's answer lines (apart from `state`, so that a check
    /// of the landed files waits for its answer without holding the files).
    answers: Mutex<Answers>,
    tests: Arc<TestRuns>,
}

/// The answer lines, and the answers read for another check than the
/// reader's (a check of the landed files that sent its request, let go of
/// `state` and then lost the race for this lock finds its answer here).
struct Answers {
    lines: mpsc::Receiver<String>,
    early: BTreeMap<u64, serde_json::Value>,
}

/// The most answers kept for other checks (the oldest go first).
const EARLY_ANSWERS: usize = 16;

struct State {
    child: Option<Child>,
    stdin: Option<ChildStdin>,
    next_id: u64,
    /// The gate's copy of each file on disk, by gate path.
    on_disk: BTreeMap<String, String>,
    /// The test files of the plan: Vitest runs during the wave once all of
    /// them have landed.
    expected_tests: BTreeSet<String>,
    /// The Node process failed: no more checks.
    broken: bool,
}

impl Sidecar {
    /// Start the checker for the gate command `gate_cmd`.
    pub fn for_gate(gate_cmd: &str) -> Result<Arc<Self>> {
        let deps = deps_of(gate_cmd).context("the gate command names no --deps directory")?;
        Self::start(&deps)
    }

    /// Start the checker with the toolchain in `deps`.
    pub fn start(deps: &Path) -> Result<Arc<Self>> {
        let deps = plain(deps.canonicalize().context("no dependency directory")?);
        let typescript = deps.join("node_modules").join("typescript");
        if !typescript.is_dir() {
            bail!("no node_modules/typescript under {}", deps.display());
        }
        let base = deps.join(".pchk");
        remove_old(&base);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |time| time.subsec_nanos());
        let id = format!("{}-{nanos}", std::process::id());
        let root = base.join(&id);
        std::fs::create_dir_all(&root)?;
        let script = base.join(format!("{id}.cjs"));
        std::fs::write(&script, SCRIPT)?;
        let mut child = Command::new("node")
            .arg(&script)
            .arg(&typescript)
            .current_dir(&root)
            .env("NO_COLOR", "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .context("could not start node")?;
        let stdin = child.stdin.take().context("no stdin")?;
        let stdout = child.stdout.take().context("no stdout")?;
        let (send, answers) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if send.send(line).is_err() {
                    break;
                }
            }
        });
        Ok(Arc::new(Self {
            deps,
            root,
            script,
            state: Mutex::new(State {
                child: Some(child),
                stdin: Some(stdin),
                next_id: 0,
                on_disk: BTreeMap::new(),
                expected_tests: BTreeSet::new(),
                broken: false,
            }),
            answers: Mutex::new(Answers {
                lines: answers,
                early: BTreeMap::new(),
            }),
            tests: Arc::new(TestRuns::default()),
        }))
    }

    /// The test files of the plan.
    pub fn expect_tests(&self, paths: impl IntoIterator<Item = String>) {
        if let Ok(mut state) = self.state.lock() {
            state.expected_tests = paths.into_iter().collect();
        }
    }

    /// A wave request for the file `title` closed with `output` (the
    /// worker's answer; a rendered file's text also works). Writes the
    /// file and starts the background work it allows.
    pub fn land(&self, title: &str, output: &str) {
        let text = crate::arms::file_text(title, output);
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        let Some(path) = gate::gate_path(title) else {
            return;
        };
        let content = gate::gate_copy(title, text);
        if state.on_disk.get(&path) == Some(&content) {
            return;
        }
        if self.write(&path, &content).is_err() {
            return;
        }
        state.on_disk.insert(path.clone(), content);
        send(
            &mut state,
            &serde_json::json!({"op": "changed", "paths": [path]}),
        );
        // Vitest, once every planned test and the files it imports are here.
        let landed = state
            .expected_tests
            .iter()
            .all(|test| state.on_disk.contains_key(test));
        let (key, resolved) = test_key(&state.on_disk);
        if landed && resolved && state.on_disk.keys().any(|path| is_test_file(path)) {
            self.want_tests(key);
        }
    }

    fn write(&self, path: &str, content: &str) -> std::io::Result<()> {
        let target = self.root.join(path);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(target, content)
    }

    /// Start a Vitest run for the files of `key` unless one has run or runs.
    fn want_tests(&self, key: u64) {
        let Ok(mut slot) = self.tests.slot.lock() else {
            return;
        };
        if slot.running.contains(&key) || slot.results.iter().any(|(done, _)| *done == key) {
            return;
        }
        slot.running.insert(key);
        drop(slot);
        let (tests, root) = (self.tests.clone(), self.root.clone());
        let script = self
            .deps
            .join("node_modules")
            .join("vitest")
            .join("vitest.mjs");
        std::thread::spawn(move || {
            let mut pid = None;
            let (_, out) = gate::node_with_pid(
                script,
                &["run", "--root", ".", "--reporter=verbose"],
                &root,
                |id| {
                    pid = Some(id);
                    if let Ok(mut slot) = tests.slot.lock() {
                        slot.pids.insert(id);
                    }
                },
            );
            if let Ok(mut slot) = tests.slot.lock() {
                if let Some(id) = pid {
                    slot.pids.remove(&id);
                }
                slot.running.remove(&key);
                slot.results.push((key, out));
                if slot.results.len() > 4 {
                    slot.results.remove(0);
                }
            }
            tests.done.notify_all();
        });
    }

    /// The Vitest output for the files of `key` (waits for its run, at most
    /// [`ANSWER_LIMIT`]).
    fn tests_of(&self, key: u64) -> Option<String> {
        self.want_tests(key);
        let deadline = Instant::now() + ANSWER_LIMIT;
        let mut slot = self.tests.slot.lock().ok()?;
        loop {
            if let Some((_, out)) = slot.results.iter().rev().find(|(done, _)| *done == key) {
                return Some(out.clone());
            }
            if !slot.running.contains(&key) {
                return None;
            }
            let left = deadline.checked_duration_since(Instant::now())?;
            slot = self.tests.done.wait_timeout(slot, left).ok()?.0;
        }
    }

    /// Put exactly `wanted` (gate path to the gate's copy) on disk and tell
    /// the Node process what changed.
    fn sync(&self, state: &mut State, wanted: &BTreeMap<String, String>) -> Result<()> {
        let mut changed = Vec::new();
        for (path, content) in wanted {
            if state.on_disk.get(path) != Some(content) {
                self.write(path, content)?;
                state.on_disk.insert(path.clone(), content.clone());
                changed.push(path.clone());
            }
        }
        let gone: Vec<String> = state
            .on_disk
            .keys()
            .filter(|path| !wanted.contains_key(*path))
            .cloned()
            .collect();
        for path in gone {
            let _ = std::fs::remove_file(self.root.join(&path));
            state.on_disk.remove(&path);
            changed.push(path);
        }
        if !changed.is_empty() {
            send(
                state,
                &serde_json::json!({"op": "changed", "paths": changed}),
            );
        }
        Ok(())
    }

    /// The compiler's answer for the files on disk: (exit code 0, output).
    fn compile(&self, state: &mut State) -> Result<(bool, String)> {
        let id = Self::ask_check(state)?;
        let answer = self.answer(id);
        if answer.is_err() {
            state.broken = true;
        }
        answer.map_err(|error| anyhow::anyhow!("{error}"))?
    }

    /// Ask the Node process to check the files on disk; returns the id of
    /// the answer.
    fn ask_check(state: &mut State) -> Result<u64> {
        if state.broken {
            bail!("the checker stopped");
        }
        state.next_id += 1;
        let id = state.next_id;
        send(state, &serde_json::json!({"op": "check", "id": id}));
        Ok(id)
    }

    /// The answer `id`: the outer error when no answer came (the checker
    /// is broken from then on), else the checker's result.
    fn answer(&self, id: u64) -> std::result::Result<Result<(bool, String)>, String> {
        let mut answers = self
            .answers
            .lock()
            .map_err(|_| "the answers are poisoned".to_string())?;
        let deadline = Instant::now() + ANSWER_LIMIT;
        loop {
            let answer = match answers.early.remove(&id) {
                Some(answer) => answer,
                None => {
                    let left = deadline.saturating_duration_since(Instant::now());
                    let line = match answers.lines.recv_timeout(left) {
                        Ok(line) => line,
                        Err(_) => return Err("the checker gave no answer".to_string()),
                    };
                    let Ok(answer) = serde_json::from_str::<serde_json::Value>(&line) else {
                        continue;
                    };
                    match answer["id"].as_u64() {
                        Some(other) if other != id => {
                            // Another check's answer: keep it for its reader.
                            answers.early.insert(other, answer);
                            while answers.early.len() > EARLY_ANSWERS {
                                answers.early.pop_first();
                            }
                            continue;
                        }
                        Some(_) => answer,
                        None => continue,
                    }
                }
            };
            if answer["failed"].as_bool() == Some(true) {
                return Ok(Err(anyhow::anyhow!("the checker failed")));
            }
            return Ok(Ok((
                answer["ok"].as_bool().unwrap_or(false),
                answer["out"].as_str().unwrap_or_default().to_string(),
            )));
        }
    }

    /// The gate's result for `files` (path to text, as the engine's checker
    /// gets them), from the persistent compiler and the Vitest runs.
    pub fn check_files(&self, files: &BTreeMap<String, String>) -> Result<GateResult> {
        let found: Vec<Found> = gate::scan_doc(&slimtoken::project::render_doc(files));
        let (lints, warnings) = gate::lint_files(&found);
        let mut wanted = BTreeMap::new();
        let mut has_tests = false;
        for file in found {
            let Some(path) = gate::gate_path(&file.path) else {
                continue;
            };
            has_tests |= is_test_file(&file.path);
            wanted.insert(path, gate::gate_copy(&file.path, file.content));
        }
        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("the checker state is poisoned"))?;
        self.sync(&mut state, &wanted)?;
        let key = has_tests.then(|| test_key(&wanted).0);
        if let Some(key) = key {
            self.want_tests(key);
        }
        let (tsc_ok, tsc_out) = self.compile(&mut state)?;
        drop(state);
        let vitest_out = match key {
            Some(key) => Some(self.tests_of(key).context("the test run stopped")?),
            None => None,
        };
        let gate::Fast {
            mut lines,
            type_errors,
            failed_tests,
            passed,
        } = gate::fast_lines(&self.root, tsc_ok, &tsc_out, lints, vitest_out.as_deref());
        let failed = type_errors + failed_tests;
        lines.extend(warnings);
        lines.push(format!(
            "{failed} failed, {passed} passed ({type_errors} type errors)"
        ));
        Ok(slimtoken::check::parse_check(
            failed == 0,
            &lines.join("\n"),
        ))
    }

    /// The checker's result for the files that landed so far (`inwave`):
    /// `files` are written beside the files on disk (none is removed, so a
    /// Vitest run of the wave keeps its files), the compiler checks them,
    /// and once every planned test and what the tests import are on disk,
    /// the Vitest output for them joins (the run that the landings started).
    /// No lints of the gate: they read the whole app, which is not there yet;
    /// with `inwavelint`, the gate's per-file lints and the app lints whose
    /// files have all landed (`gate::landed_lints`), and a page whose lint
    /// waits for a file is `undecided`.
    pub fn check_landed(&self, files: &BTreeMap<String, String>) -> Result<GateResult> {
        let found: Vec<Found> = gate::scan_doc(&slimtoken::project::render_doc(files));
        let (lints, undecided) = if crate::arms::step_on("inwavelint") {
            gate::landed_lints(&found)
        } else {
            (Vec::new(), BTreeSet::new())
        };
        let mut has_tests = false;
        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("the checker state is poisoned"))?;
        let mut changed = Vec::new();
        for file in found {
            let Some(path) = gate::gate_path(&file.path) else {
                continue;
            };
            has_tests |= is_test_file(&file.path);
            let content = gate::gate_copy(&file.path, file.content);
            if state.on_disk.get(&path) != Some(&content) {
                self.write(&path, &content)?;
                state.on_disk.insert(path.clone(), content);
                changed.push(path);
            }
        }
        if !changed.is_empty() {
            send(
                &mut state,
                &serde_json::json!({"op": "changed", "paths": changed}),
            );
        }
        let id = Self::ask_check(&mut state)?;
        let landed = state
            .expected_tests
            .iter()
            .all(|test| state.on_disk.contains_key(test));
        let (key, resolved) = test_key(&state.on_disk);
        // The wave's landings go on while the compiler answers.
        drop(state);
        let (tsc_ok, tsc_out) = match self.answer(id) {
            Ok(answer) => answer?,
            Err(error) => {
                if let Ok(mut state) = self.state.lock() {
                    state.broken = true;
                }
                bail!("{error}");
            }
        };
        let vitest_out = (has_tests && landed && resolved)
            .then(|| self.tests_of(key))
            .flatten();
        let gate::Fast {
            lines,
            type_errors,
            failed_tests,
            passed,
        } = gate::fast_lines(&self.root, tsc_ok, &tsc_out, lints, vitest_out.as_deref());
        let failed = type_errors + failed_tests;
        let mut lines = lines;
        lines.push(format!(
            "{failed} failed, {passed} passed ({type_errors} type errors)"
        ));
        let mut result = slimtoken::check::parse_check(failed == 0, &lines.join("\n"));
        result.undecided = undecided;
        Ok(result)
    }

    /// [`Self::check_files`] off the async threads.
    pub async fn check(self: &Arc<Self>, files: BTreeMap<String, String>) -> Result<GateResult> {
        let me = self.clone();
        tokio::task::spawn_blocking(move || me.check_files(&files))
            .await
            .context("the checker task stopped")?
    }

    /// Stop the Node process and remove the checker's files.
    pub fn stop(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.stdin = None;
            if let Some(mut child) = state.child.take() {
                let _ = child.kill();
                let _ = child.wait();
            }
            state.broken = true;
        }
        // Vitest runs that still read the files.
        if let Ok(slot) = self.tests.slot.lock() {
            for pid in &slot.pids {
                crate::launch::kill_tree(*pid);
            }
        }
        let _ = std::fs::remove_dir_all(&self.root);
        let _ = std::fs::remove_file(&self.script);
    }
}

impl Drop for Sidecar {
    fn drop(&mut self) {
        self.stop();
    }
}

/// The persistent checker as the engine's checker of the landed files
/// (`inwave`, [`Sidecar::check_landed`]); the mode is not read.
pub struct LandedChecker(pub Arc<Sidecar>);

#[slimtoken::async_trait]
impl slimtoken::check::Checker for LandedChecker {
    async fn check(
        &self,
        files: &BTreeMap<String, String>,
        _: slimtoken::check::CheckMode,
        _: &str,
    ) -> Result<GateResult> {
        let (sidecar, files) = (self.0.clone(), files.clone());
        tokio::task::spawn_blocking(move || sidecar.check_landed(&files))
            .await
            .context("the checker task stopped")?
    }

    /// Nothing to clean: the run's checker stays for the gate.
    async fn finish(&self, _: &str) -> Result<()> {
        Ok(())
    }

    /// The compiler and the tests only, no lints of the whole app.
    fn checks_landed_files(&self) -> bool {
        true
    }
}

/// The checker of the landed files when no persistent checker runs
/// (`inwave` without one): no result, so nothing is repaired in the wave.
pub struct NoLandedChecker;

#[slimtoken::async_trait]
impl slimtoken::check::Checker for NoLandedChecker {
    async fn check(
        &self,
        _: &BTreeMap<String, String>,
        _: slimtoken::check::CheckMode,
        _: &str,
    ) -> Result<GateResult> {
        bail!("no persistent checker runs")
    }

    async fn finish(&self, _: &str) -> Result<()> {
        Ok(())
    }
}

/// Send one message to the Node process.
fn send(state: &mut State, message: &serde_json::Value) {
    let Some(stdin) = state.stdin.as_mut() else {
        return;
    };
    let line = format!("{message}\n");
    if stdin
        .write_all(line.as_bytes())
        .and_then(|()| stdin.flush())
        .is_err()
    {
        state.broken = true;
    }
}

/// The key of what Vitest reads in `files` (gate path to text): every test
/// file, the project files that they import (transitively) and the config
/// files. Also true if every local import resolved to a project file.
fn test_key(files: &BTreeMap<String, String>) -> (u64, bool) {
    let known: BTreeSet<&str> = files.keys().map(String::as_str).collect();
    let mut queue: Vec<String> = files
        .keys()
        .filter(|path| is_test_file(path))
        .cloned()
        .collect();
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut missing: BTreeSet<String> = BTreeSet::new();
    while let Some(path) = queue.pop() {
        if !seen.insert(path.clone()) {
            continue;
        }
        let Some(source) = files.get(&path) else {
            continue;
        };
        for spec in local_specifiers(source)
            .into_iter()
            .chain(call_specifiers(source))
        {
            match resolve(&path, &spec, &known) {
                Some(file) => queue.push(file),
                None => {
                    missing.insert(format!("{path}:{spec}"));
                }
            }
        }
    }
    for config in files.keys().filter(|path| {
        let name = path.rsplit('/').next().unwrap_or(path);
        let config = !path.contains('/')
            && (name == "package.json" || name == "tsconfig.json" || name.starts_with("vitest."));
        // Setup files that the Vitest config names (`vitest.setup.ts`,
        // `tests/setup.ts`).
        let setup =
            (name.starts_with("setup.") && name.contains(".ts")) || name.contains(".setup.");
        config || setup
    }) {
        seen.insert(config.clone());
    }
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    for path in &seen {
        path.hash(&mut hasher);
        files.get(path).hash(&mut hasher);
    }
    missing.hash(&mut hasher);
    (hasher.finish(), missing.is_empty())
}

/// The local modules that `require("...")` and `vi.mock("...")` name in
/// `source` (Vitest reads them; `local_specifiers` follows `import` only).
fn call_specifiers(source: &str) -> Vec<String> {
    let mut found = Vec::new();
    for call in ["require(", "mock("] {
        for (index, _) in source.match_indices(call) {
            let after = source[index + call.len()..].trim_start();
            let Some(quote) = after
                .chars()
                .next()
                .filter(|ch| matches!(ch, '\'' | '"' | '`'))
            else {
                continue;
            };
            let Some(spec) = after[1..].split(quote).next() else {
                continue;
            };
            let local = spec.starts_with("./") || spec.starts_with("../") || spec.starts_with("@/");
            if local && !spec.contains('\n') && !found.iter().any(|known| known == spec) {
                found.push(spec.to_string());
            }
        }
    }
    found
}

/// Windows canonical paths start with `\\?\`; Node and the compiler print
/// paths without it.
fn plain(path: PathBuf) -> PathBuf {
    match path.to_str().and_then(|text| text.strip_prefix(r"\\?\")) {
        Some(rest) if !rest.starts_with("UNC") => PathBuf::from(rest),
        _ => path,
    }
}

/// Remove what earlier runs left (a run that was killed) after an hour.
fn remove_old(base: &Path) {
    let Ok(entries) = std::fs::read_dir(base) else {
        return;
    };
    for entry in entries.flatten() {
        let old = entry
            .metadata()
            .and_then(|data| data.modified())
            .ok()
            .and_then(|time| time.elapsed().ok())
            .is_some_and(|age| age.as_secs() > 3_600);
        if old {
            let path = entry.path();
            if path.is_dir() {
                let _ = std::fs::remove_dir_all(path);
            } else {
                let _ = std::fs::remove_file(path);
            }
        }
    }
}

/// One saved run for [`replay`]: its files and the order they landed in.
struct Saved {
    name: String,
    files: BTreeMap<String, String>,
    /// Wave files by close time (milliseconds after the wave started).
    closes: Vec<(u64, String)>,
    /// When the repair step started (milliseconds after the wave started).
    verify: Option<u64>,
}

/// Read the app of a saved run (`<run>/slimtoken`) and its recording
/// (`<run>/events.jsonl`).
fn read_saved(run: &Path) -> Result<Saved> {
    let app = run.join("slimtoken");
    let mut files = BTreeMap::new();
    let mut dirs = vec![app.clone()];
    while let Some(dir) = dirs.pop() {
        for entry in std::fs::read_dir(&dir)? {
            let path = entry?.path();
            let name = path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("");
            if path.is_dir() {
                if !matches!(name, "node_modules" | ".next" | "_meta" | ".git") {
                    dirs.push(path);
                }
                continue;
            }
            let relative = path
                .strip_prefix(&app)?
                .to_string_lossy()
                .replace('\\', "/");
            if let Ok(text) = std::fs::read_to_string(&path) {
                files.insert(relative, text);
            }
        }
    }
    let events = std::fs::read_to_string(run.join("events.jsonl"))
        .with_context(|| format!("no events.jsonl in {}", run.display()))?;
    let mut labels: BTreeMap<u64, String> = BTreeMap::new();
    let mut wave_start = None;
    let mut verify = None;
    let mut closes = Vec::new();
    for line in events.lines() {
        let Ok(event) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if event["arm"] != "slim" {
            continue;
        }
        let at = event["t"].as_u64().unwrap_or(0);
        match event["ev"].as_str() {
            Some("phase") if event["phase"] == "wave" => wave_start = Some(at),
            Some("phase") if event["phase"] == "verify" => verify = Some(at),
            Some("open") if event["kind"] == "file" => {
                if let (Some(id), Some(label)) = (event["id"].as_u64(), event["label"].as_str()) {
                    labels.insert(id, label.to_string());
                }
            }
            Some("close") if event["ok"] == true => {
                if let Some(label) = event["id"].as_u64().and_then(|id| labels.remove(&id)) {
                    if files.contains_key(&label) {
                        closes.push((at, label));
                    }
                }
            }
            _ => {}
        }
    }
    let start = wave_start.unwrap_or(0);
    // A request with a `Title:` line after the wave is a repair.
    let mut closes: Vec<(u64, String)> = closes
        .into_iter()
        .filter(|(at, _)| verify.is_none_or(|verify| *at <= verify))
        .map(|(at, label)| (at.saturating_sub(start), label))
        .collect();
    closes.sort();
    Ok(Saved {
        name: run
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default(),
        files,
        closes,
        verify: verify.map(|at| at.saturating_sub(start)),
    })
}

/// The changed versions of the saved files that [`replay`] checks after
/// them, each made from the saved files: (name, files).
fn mutations(saved: &Saved) -> Vec<(&'static str, BTreeMap<String, String>)> {
    let files = &saved.files;
    let mut out = Vec::new();
    // A type error in the page that landed last.
    if let Some((_, last)) = saved
        .closes
        .iter()
        .rev()
        .find(|(_, path)| path.ends_with(".tsx"))
    {
        let mut changed = files.clone();
        if let Some(text) = changed.get_mut(last) {
            text.push_str("\nexport const pchkWrong: number = \"text\";\n");
            out.push(("type-error", changed));
        }
    }
    // An export of the most imported lib module renamed, then that module
    // gone: its importers break.
    let known: BTreeSet<&str> = files.keys().map(String::as_str).collect();
    let mut importers: BTreeMap<String, usize> = BTreeMap::new();
    for (path, text) in files {
        for spec in local_specifiers(text) {
            if let Some(target) = resolve(path, &spec, &known) {
                *importers.entry(target).or_default() += 1;
            }
        }
    }
    let shared = importers
        .iter()
        .filter(|(path, _)| path.starts_with("lib/") && !path.ends_with("types.ts"))
        .filter(|(path, _)| files[*path].contains("export function "))
        .max_by_key(|(_, count)| **count)
        .map(|(path, _)| path.clone());
    if let Some(path) = shared {
        let text = &files[&path];
        let at = text.find("export function ").unwrap_or(0) + "export function ".len();
        let end = at
            + text[at..]
                .find(|ch: char| !(ch.is_ascii_alphanumeric() || ch == '_'))
                .unwrap_or(0);
        let mut changed = files.clone();
        changed.insert(
            path.clone(),
            format!("{}{}Renamed{}", &text[..at], &text[at..end], &text[end..]),
        );
        out.push(("export-renamed", changed));
        let mut gone = files.clone();
        gone.remove(&path);
        out.push(("module-removed", gone));
    }
    // A wrong expectation in a test.
    if let Some((path, text)) = files
        .iter()
        .find(|(path, text)| path.contains(".test.") && text.contains(".toBe("))
    {
        let at = text.find(".toBe(").unwrap_or(0) + ".toBe(".len();
        let mut depth = 1;
        let mut end = at;
        for (offset, ch) in text[at..].char_indices() {
            match ch {
                '(' => depth += 1,
                ')' => depth -= 1,
                _ => {}
            }
            if depth == 0 {
                end = at + offset;
                break;
            }
        }
        let mut changed = files.clone();
        changed.insert(
            path.clone(),
            format!("{}\"pchk-wrong\"{}", &text[..at], &text[end..]),
        );
        out.push(("test-fails", changed));
    }
    // A syntax error in a component (or, when the pages hold their
    // components, `pagelocal`, in a page): the compiler then reports no
    // semantic error at all.
    if let Some(path) = files
        .keys()
        .find(|path| path.starts_with("components/") && path.ends_with(".tsx"))
        .or_else(|| {
            files
                .keys()
                .find(|path| path.starts_with("app/") && path.ends_with("page.tsx"))
        })
    {
        let mut changed = files.clone();
        if let Some(text) = changed.get_mut(path) {
            text.push_str("\nconst = ;\n");
            out.push(("syntax-error", changed));
        }
    }
    out.push(("saved-again", files.clone()));
    out
}

/// Replay saved runs into the persistent checker, without the model: the
/// files land in the close order (and at the times) of the recording, then
/// the repair step's gate ([`crate::repair::GateChecker`] with the checker)
/// checks the saved files and some changed versions of them ([`mutations`]).
/// Prints, for each run, the time of the command line alone (its fast
/// checks, cold, before the wave and with nothing beside it), the first
/// check's times (checker and command line, which then runs beside the
/// checker and the build) and the agreement of the two; then the totals.
/// The checker's first-check time saves time only when that check fails.
/// `round_ms` stands for the model's part of a repair round: the time
/// between two checks.
pub async fn replay(deps: &Path, runs: &[PathBuf], speed: f64, round_ms: u64) -> Result<()> {
    use slimtoken::check::{CheckMode, Checker};
    // The gate command as `prepare_gate` writes it.
    let quote = |text: String| {
        if text.contains(' ') {
            format!("\"{text}\"")
        } else {
            text
        }
    };
    let exe = quote(std::env::current_exe()?.display().to_string());
    let gate_cmd = format!("{exe} gate --deps {}", quote(deps.display().to_string()));
    let mut total = PchkReport::default();
    let mut firsts: Vec<(u64, u64)> = Vec::new();
    let mut alones: Vec<u64> = Vec::new();
    for run in runs {
        let saved = match read_saved(run) {
            Ok(saved) => saved,
            Err(error) => {
                println!("{}: skipped ({error:#})", run.display());
                continue;
            }
        };
        // The command line alone, cold, with nothing beside it: the
        // baseline of its time (`pchk` off, before the build starts).
        let alone_ms = {
            let alone = crate::repair::GateChecker::with_sidecar(&gate_cmd, None);
            let key = format!("{}-f", crate::repair::keep_key());
            let started = Instant::now();
            alone.check(&saved.files, CheckMode::Fast, &key).await?;
            let ms = started.elapsed().as_millis() as u64;
            let _ = alone.check(&BTreeMap::new(), CheckMode::Clean, &key).await;
            ms
        };
        alones.push(alone_ms);
        let sidecar = Sidecar::start(deps)?;
        let wave: BTreeSet<&str> = saved.closes.iter().map(|(_, path)| path.as_str()).collect();
        sidecar.expect_tests(
            saved
                .files
                .keys()
                .filter(|path| is_test_file(path))
                .cloned(),
        );
        for (path, text) in &saved.files {
            if !wave.contains(path.as_str()) {
                sidecar.land(path, &slimtoken::project::fenced(text));
            }
        }
        let started = Instant::now();
        for (at, path) in &saved.closes {
            let due = Duration::from_millis((*at as f64 / speed) as u64);
            if let Some(wait) = due.checked_sub(started.elapsed()) {
                tokio::time::sleep(wait).await;
            }
            let me = sidecar.clone();
            let text = slimtoken::project::fenced(&saved.files[path]);
            let path = path.clone();
            tokio::task::spawn_blocking(move || me.land(&path, &text)).await?;
        }
        // The repair step starts when the recording's did.
        if let Some(verify) = saved.verify {
            let due = Duration::from_millis((verify as f64 / speed) as u64);
            if let Some(wait) = due.checked_sub(started.elapsed()) {
                tokio::time::sleep(wait).await;
            }
        }
        let wave_ms = started.elapsed().as_millis() as u64;
        let last = saved
            .closes
            .last()
            .map(|(_, path)| path.as_str())
            .unwrap_or_default();
        let checker = crate::repair::GateChecker::with_sidecar(&gate_cmd, Some(sidecar.clone()));
        let key = crate::repair::keep_key();
        let mut verdicts = Vec::new();
        let first = checker.check(&saved.files, CheckMode::Full, &key).await?;
        verdicts.push(format!("saved: {}", first.summary));
        for (name, files) in mutations(&saved) {
            tokio::time::sleep(Duration::from_millis(round_ms)).await;
            let result = checker.check(&files, CheckMode::Full, &key).await?;
            verdicts.push(format!("{name}: {}", result.summary));
        }
        checker.finish(&key).await?;
        let report = checker.pchk_report().unwrap_or_default();
        let later = |values: &[u64]| values.get(1..).unwrap_or_default().to_vec();
        let first_quick = report.quick_ms.first().copied().unwrap_or(0);
        let first_cli = report.cli_ms.first().copied().unwrap_or(0);
        firsts.push((first_quick, first_cli));
        println!(
            "{}: {} files, wave replay {:.1}s (last `{last}`); command line alone {alone_ms} ms; first \
             check: checker {first_quick} ms, command line {first_cli} ms; later checks: checker {:?} ms, command line {:?} ms; agree \
             {}/{}, false fail {}, missed {}, unavailable {}",
            saved.name,
            saved.files.len(),
            wave_ms as f64 / 1000.0,
            later(&report.quick_ms),
            later(&report.cli_ms),
            report.agree,
            report.compared,
            report.false_fail,
            report.missed,
            report.unavailable
        );
        println!("  {}", verdicts.join(" | "));
        for note in &report.notes {
            println!("  differs: {note}");
        }
        total.checks += report.checks;
        total.early += report.early;
        total.compared += report.compared;
        total.agree += report.agree;
        total.disagree += report.disagree;
        total.false_fail += report.false_fail;
        total.missed += report.missed;
        total.unavailable += report.unavailable;
        total.quick_ms.extend(later(&report.quick_ms));
        total.cli_ms.extend(later(&report.cli_ms));
        sidecar.stop();
    }
    let median = |mut values: Vec<u64>| {
        values.sort_unstable();
        values.get(values.len() / 2).copied().unwrap_or(0)
    };
    println!(
        "TOTAL {} runs: command line alone median {} ms; first check median checker {} ms vs \
         command line {} ms; later checks \
         median checker {} ms vs command line {} ms; compared {}, agree {}, disagree {} (false \
         fail {}, missed {}), unavailable {}, early {}",
        firsts.len(),
        median(alones.clone()),
        median(firsts.iter().map(|(quick, _)| *quick).collect()),
        median(firsts.iter().map(|(_, cli)| *cli).collect()),
        median(total.quick_ms.clone()),
        median(total.cli_ms.clone()),
        total.compared,
        total.agree,
        total.disagree,
        total.false_fail,
        total.missed,
        total.unavailable,
        total.early
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn result(passed: bool, failed: u64, files: &[&str], deep: bool) -> GateResult {
        GateResult {
            passed,
            failed,
            deep,
            summary: format!("{failed} failed"),
            problems: files
                .iter()
                .map(|file| (file.to_string(), "ERROR".to_string()))
                .collect(),
            ..GateResult::default()
        }
    }

    #[test]
    fn the_deps_directory_comes_from_the_gate_command() {
        assert_eq!(
            deps_of(r#""C:\a b\showdown.exe" gate --deps "C:\x y\_deps""#),
            Some(PathBuf::from(r"C:\x y\_deps"))
        );
        assert_eq!(
            deps_of("showdown gate --deps /tmp/_deps"),
            Some(PathBuf::from("/tmp/_deps"))
        );
        assert_eq!(deps_of("npm test"), None);
    }

    #[test]
    fn comparisons_count_false_fails_and_misses() {
        let mut report = PchkReport::default();
        report.compare(
            &result(false, 2, &["a.ts"], false),
            &result(false, 2, &["a.ts"], false),
        );
        report.compare(
            &result(false, 1, &["a.ts"], false),
            &result(true, 0, &[], true),
        );
        report.compare(
            &result(true, 0, &[], false),
            &result(false, 1, &["b.ts"], false),
        );
        report.compare(
            &result(false, 1, &["a.ts"], false),
            &result(false, 2, &["a.ts", "b.ts"], false),
        );
        // A failed build: the fast checks passed.
        report.compare(
            &result(true, 0, &[], false),
            &result(false, 1, &["x"], true),
        );
        assert_eq!(report.compared, 5);
        assert_eq!(report.agree, 2);
        assert_eq!(report.disagree, 3);
        assert_eq!(report.false_fail, 1);
        assert_eq!(report.missed, 1);
        assert_eq!(report.notes.len(), 3);
    }

    #[test]
    fn the_test_key_follows_the_imports_of_the_tests() {
        let files: BTreeMap<String, String> = [
            ("tests/a.test.ts", "import { f } from '../lib/a';\n"),
            (
                "lib/a.ts",
                "import { g } from './b';\nexport const f = g;\n",
            ),
            ("lib/b.ts", "export const g = 1;\n"),
            (
                "app/page.tsx",
                "export default function P() { return null; }\n",
            ),
            ("package.json", "{}"),
        ]
        .into_iter()
        .map(|(path, text)| (path.to_string(), text.to_string()))
        .collect();
        let (key, resolved) = test_key(&files);
        assert!(resolved);
        // A page is not read by the tests.
        let mut page = files.clone();
        page.insert("app/page.tsx".into(), "changed".into());
        assert_eq!(test_key(&page).0, key);
        // A module that a test imports through another is.
        let mut module = files.clone();
        module.insert("lib/b.ts".into(), "export const g = 2;\n".into());
        assert_ne!(test_key(&module).0, key);
        // An import that names no file: not ready, another key.
        let mut missing = files.clone();
        missing.remove("lib/b.ts");
        let (other, resolved) = test_key(&missing);
        assert!(!resolved);
        assert_ne!(other, key);
        // A module that a test mocks or requires, and a setup file.
        let mut mocked = files.clone();
        mocked.insert(
            "tests/a.test.ts".into(),
            "vi.mock('../lib/c', () => ({}));\nconst d = require(\"../lib/d\");\n".into(),
        );
        mocked.insert("lib/c.ts".into(), "export const c = 1;\n".into());
        mocked.insert("lib/d.ts".into(), "export const d = 1;\n".into());
        let (base, resolved) = test_key(&mocked);
        assert!(resolved);
        for (path, text) in [
            ("lib/c.ts", "export const c = 2;\n"),
            ("lib/d.ts", "export const d = 2;\n"),
            ("tests/setup.ts", "globalThis.x = 1;\n"),
        ] {
            let mut changed = mocked.clone();
            changed.insert(path.into(), text.into());
            assert_ne!(test_key(&changed).0, base, "{path}");
        }
    }
}
