//! SlimToken's check-and-repair step for the showdown.
//!
//! Parallel workers write files that can disagree at the seams (a property
//! name, a literal value, an import of a file that nobody wrote). This step
//! finds those places with the gate (type check and tests) and repairs the
//! named files, again in parallel:
//!
//! 1. Pipe the deliverable to the gate and read the problems for each file.
//! 2. For each file with problems, ask for a corrected file. The request
//!    carries the file's first prompt (served from the prefix cache), the
//!    problems, and the current text of the project files it imports.
//! 3. Run the gate again and repeat with the problems that are left. A
//!    round can make the count worse for a moment (a corrected shared file
//!    breaks its users until they are corrected too), so the loop goes on
//!    until the gate passes, two rounds in sequence bring no new best
//!    result, or the round limit is reached. The files of the best result
//!    (fewest problems, no fewer tests) are the ones kept.
//!
//! A long file with few problems is repaired with search-and-replace edits
//! (a few hundred tokens) and not written again in full (some thousand
//! tokens). If the edits cannot be placed, the next round writes the file
//! in full.
//!
//! A file that still has problems after its own repair gets a joint
//! repair: one request sees that file, the files that the problems name and
//! the shared types, and it can change all of them. The cause is then
//! usually between two files (two declarations of one type that differ, a
//! property that the shared type does not have), and a worker that can
//! change one file only cannot remove it.
//!
//! Before each gate run, one fix needs no model: a relative import with the
//! wrong number of `../` is rewritten when exactly one project file can be
//! meant.
//!
//! All of this time is on SlimToken's clock.
//!
//! With the switch `engcheck` on (the default), the gate runs through the
//! engine's `slimtoken::check::CommandChecker` (the files as the engine's
//! `project::render_doc`, the mode and directory in `SLIMTOKEN_CHECK_*`
//! and `SHOWDOWN_GATE_*`, the output read by `check::parse_check`), and the
//! edit protocol, the reading of joint answers and the ranking of results
//! come from `slimtoken::project` and `slimtoken::check`. A `PATCH` block
//! in the gate output is put in place with no model call and the gate runs
//! again. `SHOWDOWN_OFF=engcheck` keeps the bench copies below.
//!
//! What `engcheck` changes besides the code path: the gate reads the files
//! as written (`file_text`: also an answer with no code block, also a
//! shell-tagged block that is the file), in path order with plain fences,
//! not the answers in task order. On the gate's own output the two parsers
//! agree; `parse_check` also reads lines the gate does not print (see the
//! tests). The repair prompt keeps the bench's wording.
//!
//! With the switch `engrepair` on (the default), [`run`] calls the engine's
//! `slimtoken::repair::check_and_repair`: the rounds, the stale limit, the
//! best-state restore, edit blocks, joint repairs and stuck tests are the
//! engine's, with the NEXTJS repair hooks (`profile::NEXTJS_REPAIR`) and the
//! gate below as the engine's `Checker` ([`GateChecker`]: the speculative
//! deep build, `specdeep`, and the kept directories, `keepdir`, stay here).
//! The fixes that need no model call (`fix_import_paths`, and with
//! `autofix` `fix_mechanical`) run as the hooks' `prepare`, before every
//! gate run as before. `testonly` maps to `RepairPolicy::stuck_tests_alone`
//! and `think` to `RepairPolicy::escalate_effort`. With `engrepair` on the
//! gate always reads the engine's files (as with `engcheck` on).
//! `SHOWDOWN_OFF=engrepair` keeps the bench copy, [`check_and_repair`].
//!
//! With the switch `guard` on (the default, under `engrepair`) the engine's
//! repair guards run (`RepairPolicy::guard`: export locks, root first,
//! knock-on files; see `slimtoken::guard`), and `prepare` also fixes the
//! root layout for the gate's TS9004 / TS9005 lines (`guard.rs`). The
//! guards look up who uses what in the engine's text index, or in indexio
//! with `SHOWDOWN_INDEX=indexio` ([`run_guarded`], `codeindex.rs`).

use crate::arms::shell;
use anyhow::{Context, Result};
use slimtoken::check::{CheckMode, CheckResult, Checker, CommandChecker};
use slimtoken::dag::{Plan, Task};
use slimtoken::profile::{RepairHooks, NEXTJS_REPAIR};
use slimtoken::repair::RepairPolicy;
use slimtoken::scheduler::{execute_plan, RunReport, SchedulerConfig, TaskStatus};
use slimtoken::ModelClient;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::hash::{Hash, Hasher};
use std::io::Write as _;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Instant;

/// Most characters of imported files that one repair request carries.
const CONTEXT_CHARS: usize = 48_000;
/// Most characters of gate output that one repair request carries.
const EXCERPT_CHARS: usize = 4_000;

/// A file is repaired with edits if it has at least this number of lines...
const EDIT_MIN_LINES: usize = 60;
/// ...and the gate reports at most this number of lines about it.
const EDIT_MAX_PROBLEM_LINES: usize = 60;

/// The output contract of an edit repair.
const EDIT_FORMAT: &str = "Output ONLY search/replace edit blocks, never the whole file:\n\
<<<<<<< SEARCH\n(lines copied exactly from your current version)\n=======\n\
(their replacement)\n>>>>>>> REPLACE\nEach SEARCH must match exactly one place in the current \
version: include enough lines around the change to make it unique. Use as many blocks as the \
correction needs.";

/// Apply search/replace edit blocks to `current`. `None` unless there is at
/// least one block and each SEARCH matches exactly one place.
fn apply_edit_blocks(current: &str, edits: &str) -> Option<String> {
    let edits = edits.replace("\r\n", "\n");
    let mut text = current.replace("\r\n", "\n");
    let mut applied = 0;
    let mut lines = edits.lines();
    while let Some(line) = lines.next() {
        if line.trim_end() != "<<<<<<< SEARCH" {
            continue;
        }
        let mut search = Vec::new();
        let mut replace = Vec::new();
        let mut in_replace = false;
        let mut closed = false;
        for line in lines.by_ref() {
            match line.trim_end() {
                "=======" if !in_replace => in_replace = true,
                ">>>>>>> REPLACE" if in_replace => {
                    closed = true;
                    break;
                }
                _ if in_replace => replace.push(line),
                _ => search.push(line),
            }
        }
        if !closed || search.is_empty() {
            return None;
        }
        let search = search.join("\n");
        if text.matches(&search).count() != 1 {
            return None;
        }
        text = text.replacen(&search, &replace.join("\n"), 1);
        applied += 1;
    }
    (applied > 0).then_some(text)
}

/// What the gate said about one deliverable: the engine's check result
/// (the same fields; `patches` is empty for the bench's own parser and is
/// not serialized when empty).
pub type GateResult = slimtoken::check::CheckResult;

/// True unless `SHOWDOWN_OFF` holds `engcheck`: the gate and its helpers
/// come from the engine's `check` and `project` modules.
pub(crate) fn engcheck() -> bool {
    crate::arms::step_on("engcheck")
}

/// `apply_edit_blocks` of the engine (`engcheck`) or of the bench.
fn place_edits(current: &str, edits: &str) -> Option<String> {
    if engcheck() {
        slimtoken::project::apply_edit_blocks(current, edits)
    } else {
        apply_edit_blocks(current, edits)
    }
}

/// One repair round: the engine's type (the same fields as the bench's
/// before the port).
pub type Round = slimtoken::repair::Round;

/// The result of the repair step: the engine's type (the same fields as the
/// bench's before the port).
pub type RepairSummary = slimtoken::repair::RepairSummary;

/// True unless `SHOWDOWN_OFF` holds `engrepair`: the repair loop is the
/// engine's `repair::check_and_repair`.
pub(crate) fn engrepair() -> bool {
    crate::arms::step_on("engrepair")
}

/// The repair step: the engine's loop (`engrepair`) or the bench copy.
pub async fn run(
    plan: &mut Plan,
    report: &mut RunReport,
    client: Arc<dyn ModelClient>,
    config: SchedulerConfig,
    gate_cmd: &str,
    max_rounds: usize,
) -> Result<RepairSummary> {
    // `pchk`: a checker of its own, warm from the second check on.
    let sidecar = (crate::pchk::on() && engrepair())
        .then(|| crate::pchk::Sidecar::for_gate(gate_cmd).ok())
        .flatten();
    run_with(plan, report, client, config, gate_cmd, max_rounds, sidecar)
        .await
        .map(|(summary, _)| summary)
}

/// [`run`] with the persistent checker `sidecar` (`pchk`; the arm feeds it
/// during the wave), and the checker's report.
#[allow(clippy::too_many_arguments)]
pub async fn run_with(
    plan: &mut Plan,
    report: &mut RunReport,
    client: Arc<dyn ModelClient>,
    config: SchedulerConfig,
    gate_cmd: &str,
    max_rounds: usize,
    sidecar: Option<Arc<crate::pchk::Sidecar>>,
) -> Result<(RepairSummary, Option<crate::pchk::PchkReport>)> {
    run_guarded(
        plan, report, client, config, gate_cmd, max_rounds, sidecar, None,
    )
    .await
}

/// [`run_with`] whose guards (`guard`) look up who uses what in `index`
/// (`None`: the engine's text index).
#[allow(clippy::too_many_arguments)]
pub async fn run_guarded(
    plan: &mut Plan,
    report: &mut RunReport,
    client: Arc<dyn ModelClient>,
    config: SchedulerConfig,
    gate_cmd: &str,
    max_rounds: usize,
    sidecar: Option<Arc<crate::pchk::Sidecar>>,
    index: Option<Arc<dyn slimtoken::guard::CodeIndex>>,
) -> Result<(RepairSummary, Option<crate::pchk::PchkReport>)> {
    run_seeded(
        plan,
        report,
        client,
        config,
        gate_cmd,
        max_rounds,
        sidecar,
        index,
        &slimtoken::repair::RepairSeed::default(),
    )
    .await
}

/// [`run_guarded`] after the in-wave repairs of `seed` (`inwave`; the
/// engine's loop only: the bench copy takes no seed).
#[allow(clippy::too_many_arguments)]
pub async fn run_seeded(
    plan: &mut Plan,
    report: &mut RunReport,
    client: Arc<dyn ModelClient>,
    config: SchedulerConfig,
    gate_cmd: &str,
    max_rounds: usize,
    sidecar: Option<Arc<crate::pchk::Sidecar>>,
    index: Option<Arc<dyn slimtoken::guard::CodeIndex>>,
    seed: &slimtoken::repair::RepairSeed,
) -> Result<(RepairSummary, Option<crate::pchk::PchkReport>)> {
    if !engrepair() {
        return check_and_repair(plan, report, client, config, gate_cmd, max_rounds)
            .await
            .map(|summary| (summary, None));
    }
    let checker = GateChecker::with_sidecar(gate_cmd, sidecar);
    let policy = engine_policy(max_rounds, crate::arms::step_on);
    let summary = slimtoken::repair::check_and_repair_seeded(
        plan,
        report,
        client,
        config,
        &checker,
        policy,
        &BENCH_HOOKS,
        index.as_deref(),
        seed,
    )
    .await?;
    Ok((summary, checker.pchk_report()))
}

/// The engine's repair policy for the bench switches read from `on`.
pub(crate) fn engine_policy(max_rounds: usize, on: impl Fn(&str) -> bool) -> RepairPolicy {
    RepairPolicy {
        max_rounds,
        stuck_tests_alone: on("testonly"),
        escalate_effort: on("think").then(|| "medium".to_string()),
        escalate_tests: on("thinktests"),
        check_key: if on("keepdir") {
            keep_key()
        } else {
            String::new()
        },
        answer_log: std::env::var_os("SHOWDOWN_REPAIR_LOG").map(Into::into),
        guard: on("guard"),
        joint_edits: on("jointedit"),
        test_edits: on("testedit"),
        contract_side: on("contractside"),
        ..RepairPolicy::default()
    }
}

/// A name for the directories that the gate keeps between rounds.
pub(crate) fn keep_key() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |time| time.subsec_nanos());
    format!("{}-{nanos}", std::process::id())
}

/// The NEXTJS repair hooks with the bench's fixes that need no model call.
pub(crate) const BENCH_HOOKS: RepairHooks = RepairHooks {
    prepare,
    ..NEXTJS_REPAIR
};

/// The fixes before each gate run, in the bench's order: import paths,
/// then (`autofix`) the import forms and the "use client" line, then
/// (`guard`) the root layout for unrendered providers and shells, then
/// (`shellfix`) the pages that draw the layout's frame again (TS9021).
fn prepare(plan: &Plan, report: &mut RunReport) -> usize {
    fix_import_paths(plan, report)
        + fix_mechanical(plan, report)
        + crate::guard::fix_layout(plan, report)
        + crate::shellfix::fix_pages(plan, report)
}

/// The bench gate as the engine's `Checker`. A full check is [`Gate`]'s:
/// with `specdeep` the real build starts beside the fast checks and is
/// kept while only tests change; `key` (the engine's
/// `RepairPolicy::check_key`, set with `keepdir`) names the kept
/// directories. `finish` stops a build that runs and removes them.
///
/// With a persistent checker (`pchk`, [`Self::with_sidecar`]) a full check
/// asks it and the command-line gate at the same time; see
/// [`Gate::race`]. A result that the checker gave early is replaced by
/// the command line's in [`Checker::confirm`].
pub struct GateChecker {
    cmd: String,
    gate: std::sync::Mutex<Option<Gate>>,
    pchk: Option<Pchk>,
}

impl GateChecker {
    /// The gate with the persistent checker `sidecar` beside it.
    pub fn with_sidecar(cmd: &str, sidecar: Option<Arc<crate::pchk::Sidecar>>) -> Self {
        let pchk = sidecar.map(|sidecar| (sidecar, Arc::default()));
        // The engine's `check_key` names the kept directories (`take`).
        let mut gate = Gate::with_dir(cmd, String::new());
        gate.pchk = pchk.clone();
        Self {
            cmd: cmd.to_string(),
            gate: std::sync::Mutex::new(Some(gate)),
            pchk,
        }
    }

    /// What the persistent checker did (complete after `finish`).
    pub fn pchk_report(&self) -> Option<crate::pchk::PchkReport> {
        let (_, report) = self.pchk.as_ref()?;
        report.lock().ok().map(|report| report.clone())
    }

    /// The gate, taken out while a check runs (checks run one at a time).
    fn take(&self, key: &str) -> Result<Gate> {
        let mut gate = self
            .gate
            .lock()
            .map_err(|_| anyhow::anyhow!("the gate state is poisoned"))?
            .take()
            .context("the gate runs already")?;
        gate.dir = key.to_string();
        Ok(gate)
    }

    fn put(&self, gate: Gate) {
        if let Ok(mut slot) = self.gate.lock() {
            *slot = Some(gate);
        }
    }
}

/// The files as the gate's deliverable: all of them (`with_tests`), or
/// without the tests, for the build.
fn files_deliverable(files: &BTreeMap<String, String>, with_tests: bool) -> Deliverable {
    Deliverable::Files(
        files
            .iter()
            .filter(|(path, _)| with_tests || !is_test_file(path))
            .map(|(path, text)| (path.clone(), text.clone()))
            .collect(),
    )
}

/// A hash of the files of one check.
fn state_of(files: &BTreeMap<String, String>) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    files.hash(&mut hasher);
    hasher.finish()
}

#[slimtoken::async_trait]
impl Checker for GateChecker {
    async fn check(
        &self,
        files: &BTreeMap<String, String>,
        mode: CheckMode,
        key: &str,
    ) -> Result<CheckResult> {
        if mode != CheckMode::Full {
            return run_gate_mode(
                &self.cmd,
                Deliverable::Files(files.clone()),
                mode,
                key.to_string(),
                Arc::new(AtomicU32::new(0)),
            )
            .await;
        }
        let mut gate = self.take(key)?;
        let result = gate
            .check_with(
                |with_tests| files_deliverable(files, with_tests),
                self.pchk
                    .is_some()
                    .then(|| (files.clone(), state_of(files))),
            )
            .await;
        self.put(gate);
        result
    }

    async fn confirm(
        &self,
        files: &BTreeMap<String, String>,
        result: CheckResult,
        key: &str,
    ) -> Result<CheckResult> {
        if self.pchk.is_none() {
            return Ok(result);
        }
        let mut gate = self.take(key)?;
        let exact = gate
            .confirm(
                state_of(files),
                |with_tests| files_deliverable(files, with_tests),
                result,
            )
            .await;
        self.put(gate);
        exact
    }

    async fn finish(&self, key: &str) -> Result<()> {
        let mut gate = self.take(key)?;
        gate.finish().await;
        self.put(gate);
        Ok(())
    }
}

/// The number written immediately before `word` in `line`.
fn count_before(line: &str, word: &str) -> u64 {
    let Some(index) = line.find(word) else {
        return 0;
    };
    let digits: String = line[..index]
        .trim_end()
        .chars()
        .rev()
        .take_while(char::is_ascii_digit)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    digits.parse().unwrap_or(0)
}

/// Read the gate's output (the format is in `gate.rs`).
pub fn parse_gate(passed: bool, out: &str) -> GateResult {
    let mut problems: BTreeMap<String, String> = BTreeMap::new();
    let mut hints: BTreeMap<String, String> = BTreeMap::new();
    // The file that the lines belong to, and whether they are a warning.
    let mut current: Option<(String, bool)> = None;
    let mut summary = String::new();
    // `path(line,col): error TSxxxx: message`
    let named = |rest: &str| {
        rest.find("): error TS")
            .and_then(|end| rest[..end].rfind('(').map(|open| rest[..open].to_string()))
    };
    for line in out.lines() {
        if let Some(rest) = line.strip_prefix("ERROR ") {
            current = named(rest).map(|file| (file, false));
        } else if let Some(rest) = line.strip_prefix("WARN ") {
            current = named(rest).map(|file| (file, true));
        } else if let Some(rest) = line.strip_prefix("FAILED ") {
            current = rest
                .split(" > ")
                .next()
                .map(|file| (file.trim().to_string(), false));
        } else if !line.starts_with(' ') {
            current = None;
            if line.contains(" failed, ") && line.contains(" passed") {
                summary = line.trim().to_string();
            }
        }
        if let Some((file, warning)) = &current {
            let entry = if *warning {
                hints.entry(file.clone()).or_default()
            } else {
                problems.entry(file.clone()).or_default()
            };
            if entry.len() + line.len() < EXCERPT_CHARS {
                entry.push_str(&line.replacen("WARN ", "ERROR ", 1));
                entry.push('\n');
            }
        }
    }
    let failed = count_before(&summary, " failed");
    let tests_passed = count_before(&summary, " passed");
    let type_errors = count_before(&summary, " type errors");
    GateResult {
        // No summary line means the gate itself did not work: never a pass.
        passed: passed && !summary.is_empty(),
        failed: if summary.is_empty() { 1 } else { failed },
        tests_passed,
        tests: failed.saturating_sub(type_errors) + tests_passed,
        deep: summary.ends_with("[deep]"),
        summary: if summary.is_empty() {
            "the gate gave no result".to_string()
        } else {
            summary
        },
        problems,
        hints,
        patches: BTreeMap::new(),
        undecided: Default::default(),
    }
}

/// True if the gate result `new` is nearer to a pass than `old`. A result
/// from the real build is nearer than a result that the fast checks stopped.
fn better(new: &GateResult, old: &GateResult) -> bool {
    if engcheck() {
        new.nearer_than(old)
    } else {
        bench_better(new, old)
    }
}

/// The bench copy of `better` (`engcheck` off).
fn bench_better(new: &GateResult, old: &GateResult) -> bool {
    if new.tests < old.tests {
        return false;
    }
    match (new.deep, old.deep) {
        (true, false) => true,
        (false, true) => false,
        _ => new.failed < old.failed,
    }
}

/// What the gate reads: the bench's assembled document, or (`engcheck`)
/// the files, which the engine's checker renders.
#[derive(Hash)]
enum Deliverable {
    Doc(String),
    Files(BTreeMap<String, String>),
}

/// The deliverable of `report` with the files that `keep` accepts.
fn deliverable(report: &RunReport, keep: impl Fn(&str) -> bool) -> Deliverable {
    if engcheck() {
        Deliverable::Files(slimtoken::project::report_files_where(report, keep))
    } else {
        Deliverable::Doc(assemble_where(report, keep))
    }
}

/// Pipe the deliverable to the gate command and read the result.
async fn run_gate(gate_cmd: &str, input: Deliverable) -> Result<GateResult> {
    run_gate_mode(
        gate_cmd,
        input,
        CheckMode::Full,
        String::new(),
        Arc::new(AtomicU32::new(0)),
    )
    .await
}

/// Run the gate in one mode (`fast`, `deep`, or `Full` for the two in
/// sequence). `pid` receives the process id, for [`kill_tree`].
async fn run_gate_mode(
    gate_cmd: &str,
    input: Deliverable,
    mode: CheckMode,
    dir: String,
    pid: Arc<AtomicU32>,
) -> Result<GateResult> {
    let doc = match input {
        Deliverable::Files(files) => {
            return CommandChecker::new(gate_cmd)
                .with_pid(pid)
                .check(&files, mode, &dir)
                .await
                .context("could not run the gate");
        }
        Deliverable::Doc(doc) => doc,
    };
    let mode = mode.as_env();
    let gate_cmd = gate_cmd.to_string();
    tokio::task::spawn_blocking(move || -> Result<GateResult> {
        let mut child = shell(&gate_cmd)
            .env("SHOWDOWN_GATE_MODE", mode)
            .env("SHOWDOWN_GATE_DIR", dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .context("could not start the gate")?;
        pid.store(child.id(), Ordering::Relaxed);
        let mut stdin = child.stdin.take().context("the gate has no stdin")?;
        // The gate reads all of stdin before it prints, so the write cannot
        // block on a full output pipe.
        let writer = std::thread::spawn(move || {
            let _ = stdin.write_all(doc.as_bytes());
        });
        let output = child
            .wait_with_output()
            .context("the gate did not finish")?;
        let _ = writer.join();
        Ok(parse_gate(
            output.status.success(),
            &String::from_utf8_lossy(&output.stdout),
        ))
    })
    .await
    .context("the gate task stopped")?
}

/// A real build that runs, or that has run, for one state of the files.
struct DeepRun {
    /// Hash of the files that the build reads (all files but the tests).
    key: u64,
    pid: Arc<AtomicU32>,
    job: Option<tokio::task::JoinHandle<Result<GateResult>>>,
    result: Option<GateResult>,
    /// True once the build's process has ended.
    done: Arc<AtomicBool>,
    /// `pchk` (a gate with a checker): the builds of earlier states that
    /// this one waits for inside its job.
    chain: Option<Chain>,
}

/// The builds that a build waits for (`pchk`).
#[derive(Default)]
struct Chain {
    /// Set by [`DeepRun::stop`]: no build of the chain starts after it.
    stopped: Arc<AtomicBool>,
    /// For each build before: its process id, its `done` flag and its job.
    before: Vec<(Arc<AtomicU32>, Arc<AtomicBool>, tokio::task::AbortHandle)>,
}

impl DeepRun {
    /// Stop the build if it runs.
    fn stop(self) {
        let Some(job) = &self.job else {
            return;
        };
        // `pchk`: a build that waits for the builds before it has no process
        // yet; none of them may start, and those that run must stop too.
        if let Some(chain) = &self.chain {
            chain.stopped.store(true, Ordering::Relaxed);
            job.abort();
            for (_, _, waiting) in &chain.before {
                waiting.abort();
            }
        }
        let pid = self.pid.load(Ordering::Relaxed);
        if pid != 0 {
            crate::launch::kill_tree(pid);
        }
        if let Some(chain) = &self.chain {
            for (pid, done, _) in &chain.before {
                let pid = pid.load(Ordering::Relaxed);
                if pid != 0 && !done.load(Ordering::Relaxed) {
                    crate::launch::kill_tree(pid);
                }
            }
        }
    }
}

/// The persistent checker of a [`Gate`] and the report of what it did.
type Pchk = (
    Arc<crate::pchk::Sidecar>,
    Arc<std::sync::Mutex<crate::pchk::PchkReport>>,
);

/// How long [`Gate::finish`] waits for the checker's answers to checks that
/// the command line answered first (they are only compared).
const LATE_LIMIT: std::time::Duration = std::time::Duration::from_secs(2);

/// Count one comparison in `report`: the checker's result `quick` (with its
/// milliseconds; `None` if it gave none) and the command line's `cli`.
fn record(
    report: &std::sync::Mutex<crate::pchk::PchkReport>,
    quick: Option<(&GateResult, u64)>,
    cli: &GateResult,
    cli_ms: u64,
) {
    if let Ok(mut report) = report.lock() {
        report.cli_ms.push(cli_ms);
        match quick {
            Some((quick, quick_ms)) => {
                report.checks += 1;
                report.quick_ms.push(quick_ms);
                report.compare(quick, cli);
            }
            None => report.unavailable += 1,
        }
    }
}

/// The command line's result for a state that the checker answered early:
/// `None` until it comes. `fast`: it is the result of the fast checks
/// alone (`specdeep`).
struct EarlyState {
    cli: tokio::sync::watch::Receiver<Option<GateResult>>,
    fast: bool,
}

/// The gate of the repair step. It starts the real build at the same time
/// as the fast checks, not after them: a first check with no problem then
/// takes the time of the build alone. The build does not read the tests, so
/// while repairs change only test files the same build stays valid and its
/// result is ready when the tests pass.
struct Gate {
    cmd: String,
    deep: Option<DeepRun>,
    /// Name of the directories that the gate keeps between rounds (one for
    /// the fast checks, one for the build), or empty: then each gate run
    /// starts cold. With kept directories the compiler and the build of a
    /// later round work from their caches (about 1 s less for each round).
    dir: String,
    /// `pchk`: the persistent checker (only through [`GateChecker`]).
    pchk: Option<Pchk>,
    /// `pchk`: the command-line check whose result came after the
    /// checker's, with its comparison. The next command-line check in the
    /// same kept directory starts when it ends.
    cli_before: Option<tokio::task::JoinHandle<()>>,
    /// `pchk`: such checks in no kept directory (they run side by side).
    cli_cold: Vec<tokio::task::JoinHandle<()>>,
    /// `pchk`: the comparisons that wait for the checker's answer to a
    /// check that the command line answered first.
    late: Vec<tokio::task::JoinHandle<()>>,
    /// `pchk`: the states (by [`state_of`]) answered early by the checker.
    early: BTreeMap<u64, EarlyState>,
    /// `pchk`: the exact results that [`Self::confirm`] found.
    confirmed: BTreeMap<u64, GateResult>,
}

impl Gate {
    fn new(cmd: &str) -> Self {
        let dir = if crate::arms::step_on("keepdir") {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |time| time.subsec_nanos());
            format!("{}-{nanos}", std::process::id())
        } else {
            String::new()
        };
        Self::with_dir(cmd, dir)
    }

    fn with_dir(cmd: &str, dir: String) -> Self {
        Self {
            cmd: cmd.to_string(),
            deep: None,
            dir,
            pchk: None,
            cli_before: None,
            cli_cold: Vec::new(),
            late: Vec::new(),
            early: BTreeMap::new(),
            confirmed: BTreeMap::new(),
        }
    }

    fn stop(&mut self) {
        if let Some(run) = self.deep.take() {
            run.stop();
        }
    }

    /// Stop a build that runs and remove the kept directories.
    async fn finish(&mut self) {
        self.stop();
        // The last comparisons, before the directories go.
        if let Some(before) = self.cli_before.take() {
            let _ = before.await;
        }
        for cold in std::mem::take(&mut self.cli_cold) {
            let _ = cold.await;
        }
        for late in std::mem::take(&mut self.late) {
            let waiting = late.abort_handle();
            if tokio::time::timeout(LATE_LIMIT, late).await.is_err() {
                waiting.abort();
                if let Some((_, report)) = &self.pchk {
                    if let Ok(mut report) = report.lock() {
                        report.unavailable += 1;
                    }
                }
            }
        }
        self.early.clear();
        self.confirmed.clear();
        if self.dir.is_empty() {
            return;
        }
        for kind in ["f", "d"] {
            // `GateChecker` (`engrepair`) checks with the files: clean the
            // same way.
            let empty = if engcheck() || engrepair() {
                Deliverable::Files(BTreeMap::new())
            } else {
                Deliverable::Doc(String::new())
            };
            let _ = run_gate_mode(
                &self.cmd,
                empty,
                CheckMode::Clean,
                format!("{}-{kind}", self.dir),
                Arc::new(AtomicU32::new(0)),
            )
            .await;
        }
    }

    /// `check`, then (`engcheck`) the files that the gate output gives in
    /// `PATCH` blocks are put in `report` with no model call, and the gate
    /// runs once more on the patched files.
    async fn check_and_patch(&mut self, report: &mut RunReport) -> Result<GateResult> {
        let result = self.check(report).await?;
        if !engcheck() || result.patches.is_empty() {
            return Ok(result);
        }
        if slimtoken::check::apply_patches(report, &result.patches).is_empty() {
            return Ok(result);
        }
        self.check(report).await
    }

    async fn check(&mut self, report: &RunReport) -> Result<GateResult> {
        self.check_with(
            |with_tests| {
                if with_tests {
                    deliverable(report, |_| true)
                } else {
                    deliverable(report, |path| !is_test_file(path))
                }
            },
            None,
        )
        .await
    }

    /// The command-line check `cli` (spawned), raced against the persistent
    /// checker on `files` (the state `state`): the first result decides. If
    /// the command line's comes first, it comes back and the checker's is
    /// compared with it later. If the checker's comes first and names
    /// problems, it comes back at once and the command line's is compared
    /// with it in the background (and kept for [`Self::confirm`]). Else the
    /// command line's result comes back: a pass is never the checker's.
    /// `fast`: `cli` runs the fast checks alone; `kept`: in a kept directory.
    async fn race(
        &mut self,
        (sidecar, report): Pchk,
        (files, state): (BTreeMap<String, String>, u64),
        (fast, kept): (bool, bool),
        mut cli: tokio::task::JoinHandle<Result<GateResult>>,
    ) -> Result<GateResult> {
        let started = Instant::now();
        let since = move || started.elapsed().as_millis() as u64;
        let mut quick = tokio::spawn(async move {
            let result = sidecar.check(files).await;
            (result, since())
        });
        tokio::select! {
            // Both ready: the command line's result wins.
            biased;
            done = &mut cli => {
                let cli = done.context("the gate task stopped")??;
                let cli_ms = since();
                let compared = cli.clone();
                self.late.push(tokio::spawn(async move {
                    let answer = quick.await.ok();
                    let quick = answer
                        .as_ref()
                        .and_then(|(result, ms)| result.as_ref().ok().map(|quick| (quick, *ms)));
                    record(&report, quick, &compared, cli_ms);
                }));
                Ok(cli)
            }
            answer = &mut quick => {
                let (answer, quick_ms) = answer.unwrap_or_else(|error| {
                    (Err(anyhow::anyhow!("the checker task stopped: {error}")), 0)
                });
                match answer {
                    // Problems with no file to repair (the checker could not
                    // run) wait for the command line.
                    Ok(quick) if !quick.passed && !quick.problems.is_empty() => {
                        if let Ok(mut report) = report.lock() {
                            report.early += 1;
                        }
                        let (send, cli_result) = tokio::sync::watch::channel(None);
                        self.early.insert(state, EarlyState { cli: cli_result, fast });
                        let early = quick.clone();
                        let compare = tokio::spawn(async move {
                            if let Ok(Ok(cli)) = cli.await {
                                record(&report, Some((&early, quick_ms)), &cli, since());
                                let _ = send.send(Some(cli));
                            }
                        });
                        if !kept {
                            self.cli_cold.push(compare);
                        } else {
                            self.cli_before = Some(compare);
                        }
                        Ok(quick)
                    }
                    answer => {
                        let cli = cli.await.context("the gate task stopped")??;
                        record(
                            &report,
                            answer.as_ref().ok().map(|quick| (quick, quick_ms)),
                            &cli,
                            since(),
                        );
                        Ok(cli)
                    }
                }
            }
        }
    }

    /// The command line's result for the state `state` (by [`state_of`]),
    /// given `result`, the answer of its check: `result` unless the checker
    /// gave it early. Then the command line's result of that check, or,
    /// if that passed the fast checks or never came, a check of `of` with
    /// the command line alone (the build included).
    async fn confirm(
        &mut self,
        state: u64,
        of: impl Fn(bool) -> Deliverable,
        result: GateResult,
    ) -> Result<GateResult> {
        if let Some(exact) = self.confirmed.get(&state) {
            return Ok(exact.clone());
        }
        let Some(early) = self.early.get(&state) else {
            return Ok(result);
        };
        let (mut cli, fast) = (early.cli.clone(), early.fast);
        let cli = cli
            .wait_for(Option::is_some)
            .await
            .ok()
            .and_then(|cli| cli.clone());
        let exact = match cli {
            // As `check_with` gives it: the fast checks failed (or ran with
            // the build).
            Some(cli) if !cli.passed || !fast => cli,
            _ => self.check_with(of, None).await?,
        };
        self.confirmed.insert(state, exact.clone());
        Ok(exact)
    }

    /// The command-line check of `doc` in `mode`, spawned: with `pchk` it
    /// starts when the one before it in the same kept directory (still
    /// compared in the background) ends.
    fn spawn_cli(
        &mut self,
        doc: Deliverable,
        mode: CheckMode,
        dir: String,
    ) -> tokio::task::JoinHandle<Result<GateResult>> {
        let before = if dir.is_empty() {
            None
        } else {
            self.cli_before.take()
        };
        let cmd = self.cmd.clone();
        tokio::spawn(async move {
            if let Some(before) = before {
                let _ = before.await;
            }
            run_gate_mode(&cmd, doc, mode, dir, Arc::new(AtomicU32::new(0))).await
        })
    }

    /// `check` on the deliverable that `of` gives: with all files
    /// (`of(true)`), or without the tests, for the build (`of(false)`).
    /// `quick` (`pchk`, through [`GateChecker`]): the same files for the
    /// persistent checker and the hash of their state, raced against the
    /// command-line checks ([`Self::race`]).
    async fn check_with(
        &mut self,
        of: impl Fn(bool) -> Deliverable,
        quick: Option<(BTreeMap<String, String>, u64)>,
    ) -> Result<GateResult> {
        let quick = quick.and_then(|files| self.pchk.clone().map(|pchk| (pchk, files)));
        if !crate::arms::step_on("specdeep") {
            let Some((pchk, files)) = quick else {
                return run_gate(&self.cmd, of(true)).await;
            };
            let cli = self.spawn_cli(of(true), CheckMode::Full, String::new());
            return self.race(pchk, files, (false, false), cli).await;
        }
        let build_doc = of(false);
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        build_doc.hash(&mut hasher);
        let key = hasher.finish();
        let (fast_dir, deep_dir) = if self.dir.is_empty() {
            (String::new(), String::new())
        } else {
            (format!("{}-f", self.dir), format!("{}-d", self.dir))
        };
        if self.deep.as_ref().map(|run| run.key) != Some(key) {
            let mut before = None;
            // `pchk` with a checker: the new build waits for the build
            // before it inside its own job, so the fast checks start at once.
            let mut chain = self.pchk.is_some().then(Chain::default);
            match self.deep.take() {
                // The build of the state before uses the kept directory:
                // let it end. Its cache makes the next build faster.
                Some(mut old) if !self.dir.is_empty() => {
                    if let Some(job) = old.job.take() {
                        match (&mut chain, old.chain.take()) {
                            (Some(chain), Some(mut old_chain)) => {
                                old_chain
                                    .before
                                    .retain(|(_, done, _)| !done.load(Ordering::Relaxed));
                                old_chain
                                    .before
                                    .push((old.pid, old.done, job.abort_handle()));
                                *chain = old_chain;
                                before = Some(job);
                            }
                            _ => {
                                let _ = job.await;
                            }
                        }
                    }
                }
                Some(old) => old.stop(),
                None => {}
            }
            let pid = Arc::new(AtomicU32::new(0));
            let done = Arc::new(AtomicBool::new(false));
            let stopped = chain.as_ref().map(|chain| chain.stopped.clone());
            let (cmd, their_pid, their_done) = (self.cmd.clone(), pid.clone(), done.clone());
            let job = tokio::spawn(async move {
                if let Some(before) = before {
                    let _ = before.await;
                }
                if stopped.is_some_and(|stopped| stopped.load(Ordering::Relaxed)) {
                    anyhow::bail!("the build was stopped");
                }
                let result =
                    run_gate_mode(&cmd, build_doc, CheckMode::Deep, deep_dir, their_pid).await;
                their_done.store(true, Ordering::Relaxed);
                result
            });
            self.deep = Some(DeepRun {
                key,
                pid,
                job: Some(job),
                result: None,
                done,
                chain,
            });
        }
        let fast = match quick {
            Some((pchk, files)) => {
                let kept = !fast_dir.is_empty();
                let cli = self.spawn_cli(of(true), CheckMode::Fast, fast_dir);
                self.race(pchk, files, (true, kept), cli).await?
            }
            None => {
                // `pchk`: a command-line check that is still compared uses
                // the same kept directory.
                if let Some(before) = self.cli_before.take() {
                    let _ = before.await;
                }
                run_gate_mode(
                    &self.cmd,
                    of(true),
                    CheckMode::Fast,
                    fast_dir,
                    Arc::new(AtomicU32::new(0)),
                )
                .await?
            }
        };
        if !fast.problems.keys().all(|path| is_test_file(path)) {
            // The code changes in the next round: the result of this build
            // is of no use. With a kept directory it runs to its end, to
            // fill the cache for the build of the next round.
            if self.dir.is_empty() {
                self.stop();
            }
            return Ok(fast);
        }
        if !fast.passed {
            // Only tests fail. The build goes on while they are repaired.
            return Ok(fast);
        }
        let Some(run) = self.deep.as_mut() else {
            return Ok(fast);
        };
        if let Some(job) = run.job.take() {
            run.result = Some(job.await.context("the build task stopped")??);
        }
        let mut deep = run.result.clone().unwrap_or_else(|| fast.clone());
        deep.tests = fast.tests;
        deep.tests_passed = fast.tests_passed;
        deep.hints = fast.hints;
        // Corrected files that the fast checks proposed still apply.
        for (path, content) in fast.patches {
            deep.patches.entry(path).or_insert(content);
        }
        if deep.passed {
            deep.summary = format!("{} [deep]", fast.summary.trim_end_matches(" [deep]"));
            deep.deep = true;
        }
        Ok(deep)
    }
}

pub(crate) fn is_test_file(path: &str) -> bool {
    path.contains(".test.") || path.contains(".spec.") || path.contains("__tests__/")
}

/// Join an import specifier to the directory of the importing file.
fn join(dir: &str, spec: &str) -> String {
    let mut parts: Vec<&str> = dir.split('/').filter(|part| !part.is_empty()).collect();
    for part in spec.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                if parts.pop().is_none() {
                    // Above the project root: no project file is there.
                    return format!("../{spec}");
                }
            }
            other => parts.push(other),
        }
    }
    parts.join("/")
}

/// The import specifiers in `source` that point into the project.
pub(crate) fn local_specifiers(source: &str) -> Vec<String> {
    let mut found = Vec::new();
    for keyword in ["from", "import"] {
        let mut rest = source;
        while let Some(index) = rest.find(keyword) {
            rest = &rest[index + keyword.len()..];
            let after = rest.trim_start_matches([' ', '(']);
            let Some(quote) = after.chars().next().filter(|ch| matches!(ch, '\'' | '"')) else {
                continue;
            };
            let body = &after[1..];
            let Some(end) = body.find(quote) else {
                continue;
            };
            let spec = &body[..end];
            let local = spec.starts_with("./") || spec.starts_with("../") || spec.starts_with("@/");
            if local && !spec.contains('\n') && !found.iter().any(|known| known == spec) {
                found.push(spec.to_string());
            }
        }
    }
    found
}

/// The project file that `spec` (imported from `path`) names, if it exists.
pub(crate) fn resolve(path: &str, spec: &str, known: &BTreeSet<&str>) -> Option<String> {
    let bases: Vec<String> = match spec.strip_prefix("@/") {
        Some(rest) => vec![rest.to_string(), format!("src/{rest}")],
        None => {
            let dir = path.rsplit_once('/').map_or("", |(dir, _)| dir);
            vec![join(dir, spec)]
        }
    };
    for base in bases {
        for suffix in ["", ".ts", ".tsx", "/index.ts", "/index.tsx", ".js", ".jsx"] {
            let candidate = format!("{base}{suffix}");
            if known.contains(candidate.as_str()) {
                return Some(candidate);
            }
        }
    }
    None
}

/// True for a file that owns shared types (`lib/types.ts`).
fn is_types_file(path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path);
    name == "types.ts" || path.contains("/types/")
}

/// The text of project files for a prompt: `wanted` in order, within the
/// character budget, each file once.
fn sources(wanted: &[String], skip: &str, files: &BTreeMap<&str, &str>) -> String {
    let mut context = String::new();
    let mut seen: BTreeSet<&str> = BTreeSet::new();
    for file in wanted {
        let Some((path, source)) = files.get_key_value(file.as_str()) else {
            continue;
        };
        let source = source.trim();
        if *path == skip
            || path.ends_with(".css")
            || !seen.insert(path)
            || context.len() + source.len() > CONTEXT_CHARS
        {
            continue;
        }
        let _ = write!(context, "\n`{path}`:\n{source}\n");
    }
    context
}

/// The imports of `current` (the file `path`): the project files they name,
/// and the specifiers that name no project file.
fn imports(path: &str, current: &str, files: &BTreeMap<&str, &str>) -> (Vec<String>, Vec<String>) {
    let known: BTreeSet<&str> = files.keys().copied().collect();
    let mut found = Vec::new();
    let mut missing = Vec::new();
    for spec in local_specifiers(current) {
        match resolve(path, &spec, &known) {
            Some(file) => found.push(file),
            None => missing.push(spec),
        }
    }
    (found, missing)
}

/// The path of the file that the missing import `spec` of `path` would be.
/// `None` for an import that is not a relative code module.
fn missing_module_path(path: &str, spec: &str) -> Option<String> {
    if !(spec.starts_with("./") || spec.starts_with("../")) {
        return None;
    }
    let name = spec.rsplit('/').next()?;
    if name.contains('.') || name.is_empty() {
        return None;
    }
    let dir = path.rsplit_once('/').map_or("", |(dir, _)| dir);
    let extension = if path.ends_with(".tsx") { "tsx" } else { "ts" };
    Some(format!("{}.{extension}", join(dir, spec)))
}

/// The repair request for one file. `added` lists the missing imports that
/// another worker writes in this round.
fn repair_prompt(
    task: &Task,
    problems: &str,
    current: &str,
    files: &BTreeMap<&str, &str>,
    added: &[String],
    edits: bool,
) -> String {
    let path = task.title.as_str();
    let (mut wanted, missing) = imports(path, current, files);
    // The files that the problems name.
    wanted.extend(
        files
            .keys()
            .filter(|file| problems.contains(**file))
            .map(|file| (*file).to_string()),
    );
    // The shared types always: a file that declares its own copy of a type
    // does not import the owner.
    wanted.extend(
        files
            .keys()
            .filter(|file| is_types_file(file))
            .map(|file| (*file).to_string()),
    );
    let context = sources(&wanted, path, files);
    let mut prompt = task.prompt.clone();
    let _ = write!(
        prompt,
        "\n\n# Check results\nThe assembled project was type-checked and its tests were run. \
         Problems in `{path}`:\n```\n{problems}```\n"
    );
    if !context.is_empty() {
        let _ = write!(
            prompt,
            "\nProject files as they are now (the files that `{path}` imports, and the shared \
             types). They do not change: use only the names, properties, signatures and literal \
             values that they declare.\n{context}"
        );
    }
    for spec in &missing {
        if added.contains(spec) {
            let _ = writeln!(
                prompt,
                "\nThe file behind the import `{spec}` is being written now by another worker. \
                 Keep that import and the way you use it."
            );
        } else {
            let _ = writeln!(
                prompt,
                "\nThe import `{spec}` names a file that is not in the project and will not be \
                 added. Write what you need from it inside `{path}`, in full."
            );
        }
    }
    let rules = if is_test_file(path) {
        "Correct the tests so that they compile against the files shown. Where a test expects \
         behavior that the implementation shown does not have, change the expectation to the \
         behavior shown. Never delete, skip or merge a test."
    } else {
        "Correct this file so that it compiles against the files shown and the problems above \
         are gone. If it declares a type that the shared types export, delete that declaration \
         and import the type. Keep every feature and every export of the file. Do not shorten \
         it and do not leave a placeholder."
    };
    // The bench's wording also under `engcheck`: the engine's
    // `project::EDIT_FORMAT` asks for the same blocks in other words, and a
    // prompt change would be measured as part of the port.
    let output_rule = if edits {
        EDIT_FORMAT
    } else {
        "Output the COMPLETE corrected file in exactly one fenced code block."
    };
    let _ = write!(
        prompt,
        "\n{rules}\n\nYour current version of `{path}`:\n{}\n\n{output_rule}",
        current.trim()
    );
    prompt
}

/// The request for a file that `importer` imports but that nobody wrote.
fn missing_module_task(
    importer: &Task,
    current: &str,
    spec: &str,
    new_path: &str,
    files: &BTreeMap<&str, &str>,
) -> Task {
    let path = importer.title.as_str();
    let (mut wanted, _) = imports(path, current, files);
    wanted.extend(
        files
            .keys()
            .filter(|file| is_types_file(file) || file.starts_with("lib/"))
            .filter(|file| !is_test_file(file))
            .map(|file| (*file).to_string()),
    );
    let context = sources(&wanted, path, files);
    let mut task = importer.clone();
    task.id = format!(
        "added-{}",
        new_path
            .chars()
            .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '-' })
            .collect::<String>()
    );
    task.title = new_path.to_string();
    task.deps.clear();
    let _ = write!(
        task.prompt,
        "\n\n# Change of assignment\nDo NOT write `{path}`. Write the new file `{new_path}`. \
         `{path}` (shown below) imports it as `{spec}`, but nobody wrote it. Write `{new_path}` \
         in full, so that `{path}` compiles and the feature that `{path}` hands to it works \
         completely: export exactly what `{path}` imports, give it the props that `{path}` \
         passes, and make its first line \"use client\" if it uses hooks or event handlers. A \
         `.ts` file has no JSX: it holds types, data and functions only. \
         Import only project files that are shown below.\n\n`{path}`:\n{}\n\nProject files as \
         they are now:\n{context}\nOutput the COMPLETE file `{new_path}` in exactly one fenced \
         code block.",
        current.trim()
    );
    task
}

/// The relative import specifier from the file `from` to the file `to`
/// (without the code extension).
pub(crate) fn relative_specifier(from: &str, to: &str) -> String {
    let from_dir: Vec<&str> = from.split('/').collect();
    let from_dir = &from_dir[..from_dir.len() - 1];
    let to_parts: Vec<&str> = to.split('/').collect();
    let common = from_dir
        .iter()
        .zip(to_parts.iter())
        .take_while(|(left, right)| left == right)
        .count();
    let mut parts: Vec<&str> = vec![".."; from_dir.len() - common];
    parts.extend(&to_parts[common..]);
    let joined = parts.join("/");
    let joined = [".tsx", ".ts", ".jsx", ".js"]
        .iter()
        .find_map(|extension| joined.strip_suffix(extension))
        .unwrap_or(&joined);
    if joined.starts_with("..") {
        joined.to_string()
    } else {
        format!("./{joined}")
    }
}

/// The project file that the unresolved import `spec` most probably means:
/// the one file whose path ends with the specifier's own path.
fn intended_file<'a>(spec: &str, known: &BTreeSet<&'a str>) -> Option<&'a str> {
    let tail = spec
        .trim_start_matches("@/")
        .trim_start_matches("./")
        .trim_start_matches("../");
    if tail.is_empty() {
        return None;
    }
    let mut matches = known.iter().filter(|file| {
        let stem = [".tsx", ".ts"]
            .iter()
            .find_map(|extension| file.strip_suffix(extension))
            .unwrap_or(file);
        stem == tail || stem.ends_with(&format!("/{tail}"))
    });
    let first = matches.next()?;
    matches.next().is_none().then_some(*first)
}

/// Rewrite imports that point beside an existing project file. Returns the
/// number of imports changed.
fn fix_import_paths(plan: &Plan, report: &mut RunReport) -> usize {
    let paths: Vec<(String, String)> = plan
        .tasks
        .iter()
        .filter(|task| {
            report.tasks.get(&task.id).is_some_and(|record| {
                record.status == TaskStatus::Succeeded && record.output.is_some()
            })
        })
        .map(|task| (task.id.clone(), task.title.clone()))
        .collect();
    let known: BTreeSet<&str> = paths.iter().map(|(_, path)| path.as_str()).collect();
    let mut changed = 0;
    for (id, path) in &paths {
        if !(path.ends_with(".ts") || path.ends_with(".tsx")) {
            continue;
        }
        let Some(record) = report.tasks.get_mut(id) else {
            continue;
        };
        let Some(mut text) = record.output.clone() else {
            continue;
        };
        for spec in local_specifiers(&text) {
            if resolve(path, &spec, &known).is_some() {
                continue;
            }
            let Some(target) = intended_file(&spec, &known).filter(|target| target != path) else {
                continue;
            };
            let right = relative_specifier(path, target);
            for quote in ['\'', '"'] {
                text = text.replace(
                    &format!("{quote}{spec}{quote}"),
                    &format!("{quote}{right}{quote}"),
                );
            }
            changed += 1;
        }
        record.output = Some(text);
    }
    changed
}

/// True if `source` has `prefix` and then `name` as a full word.
fn declares(source: &str, prefix: &str, name: &str) -> bool {
    let wanted = format!("{prefix}{name}");
    source.match_indices(&wanted).any(|(index, _)| {
        !source[index + wanted.len()..].starts_with(|ch: char| ch.is_alphanumeric() || ch == '_')
    })
}

/// True if `source` has a named export `name`.
pub(crate) fn has_named_export(source: &str, name: &str) -> bool {
    [
        "export function ",
        "export async function ",
        "export const ",
        "export let ",
        "export class ",
        "export type ",
        "export interface ",
        "export enum ",
    ]
    .iter()
    .any(|prefix| declares(source, prefix, name))
        || source.lines().any(|line| {
            let line = line.trim_start();
            line.starts_with("export {") && declares(line, "", name) && !line.contains("default")
        })
}

/// True if the default export of `source` has the name `name`.
pub(crate) fn is_default_export(source: &str, name: &str) -> bool {
    [
        "export default function ",
        "export default async function ",
        "export default class ",
        "export default ",
    ]
    .iter()
    .any(|prefix| declares(source, prefix, name))
}

/// Give one import line the form that its target file exports: `import
/// Name from` for a default export, `import { Name } from` for a named one.
fn fix_import_form(line: &str, target: &str) -> Option<String> {
    let trimmed = line.trim_start();
    let clause = trimmed.strip_prefix("import ")?;
    let (clause, tail) = clause.split_once(" from ")?;
    let clause = clause.trim();
    if clause.starts_with("type ") || clause.contains('*') {
        return None;
    }
    if let Some(inner) = clause
        .strip_prefix('{')
        .and_then(|rest| rest.strip_suffix('}'))
    {
        let names: Vec<&str> = inner
            .split(',')
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .collect();
        let is_wrong = |name: &&str| {
            !name.contains(' ')
                && !has_named_export(target, name)
                && is_default_export(target, name)
        };
        let wrong: Vec<&str> = names
            .iter()
            .copied()
            .filter(|name| is_wrong(name))
            .collect();
        let [default] = wrong.as_slice() else {
            return None;
        };
        let rest: Vec<&str> = names
            .iter()
            .copied()
            .filter(|name| name != default)
            .collect();
        return Some(if rest.is_empty() {
            format!("import {default} from {tail}")
        } else {
            format!("import {default}, {{ {} }} from {tail}", rest.join(", "))
        });
    }
    let plain = clause.chars().all(|ch| ch.is_alphanumeric() || ch == '_');
    if plain && !target.contains("export default") && has_named_export(target, clause) {
        return Some(format!("import {{ {clause} }} from {tail}"));
    }
    None
}

/// The one project file that exports every name of the import `line`.
fn sole_exporter<'a>(
    line: &str,
    path: &str,
    files: &'a BTreeMap<String, String>,
) -> Option<&'a str> {
    let clause = line.trim_start().strip_prefix("import ")?;
    let (clause, _) = clause.split_once(" from ")?;
    let inner = clause.trim().trim_start_matches("type ").trim();
    let inner = inner.strip_prefix('{')?.strip_suffix('}')?;
    let names: Vec<&str> = inner
        .split(',')
        .map(|name| name.trim().trim_start_matches("type ").trim())
        .map(|name| name.split(" as ").next().unwrap_or(name).trim())
        .filter(|name| !name.is_empty())
        .collect();
    if names.is_empty() {
        return None;
    }
    let mut owners = files.iter().filter(|(file, text)| {
        file.as_str() != path
            && !is_test_file(file)
            && names.iter().all(|name| has_named_export(text, name))
    });
    let first = owners.next()?;
    owners.next().is_none().then_some(first.0.as_str())
}

/// The one project file, not `path`, that exports `name`.
fn exporter_of<'a>(name: &str, path: &str, files: &'a BTreeMap<String, String>) -> Option<&'a str> {
    let mut owners = files.iter().filter(|(file, text)| {
        file.as_str() != path && !is_test_file(file) && has_named_export(text, name)
    });
    let first = owners.next()?;
    owners.next().is_none().then_some(first.0.as_str())
}

/// An import that takes a name from a file that does not export it
/// (`import { AppState } from './types'` when `lib/storage.ts` declares
/// `AppState`): take that name from the one file that exports it.
fn move_names(
    line: &str,
    path: &str,
    target: &str,
    files: &BTreeMap<String, String>,
) -> Option<String> {
    let indent = &line[..line.len() - line.trim_start().len()];
    let clause = line.trim_start().strip_prefix("import ")?;
    let (clause, tail) = clause.split_once(" from ")?;
    let clause = clause.trim();
    let (word, inner) = match clause.strip_prefix("type ") {
        Some(rest) => ("type ", rest.trim()),
        None => ("", clause),
    };
    let inner = inner.strip_prefix('{')?.strip_suffix('}')?;
    let mut kept: Vec<&str> = Vec::new();
    let mut moved: Vec<(&str, &str)> = Vec::new();
    for item in inner
        .split(',')
        .map(str::trim)
        .filter(|item| !item.is_empty())
    {
        let name = item.trim_start_matches("type ").trim();
        let name = name.split(" as ").next().unwrap_or(name).trim();
        if has_named_export(target, name) || is_default_export(target, name) {
            kept.push(item);
            continue;
        }
        match exporter_of(name, path, files) {
            Some(owner) => moved.push((item, owner)),
            None => kept.push(item),
        }
    }
    if moved.is_empty() {
        return None;
    }
    let mut out: Vec<String> = Vec::new();
    if !kept.is_empty() {
        out.push(format!(
            "{indent}import {word}{{ {} }} from {tail}",
            kept.join(", ")
        ));
    }
    let mut owners: Vec<&str> = moved.iter().map(|(_, owner)| *owner).collect();
    owners.dedup();
    owners.sort_unstable();
    owners.dedup();
    for owner in owners {
        let names: Vec<&str> = moved
            .iter()
            .filter(|(_, other)| *other == owner)
            .map(|(item, _)| *item)
            .collect();
        out.push(format!(
            "{indent}import {word}{{ {} }} from '{}';",
            names.join(", "),
            relative_specifier(path, owner)
        ));
    }
    Some(out.join("\n"))
}

/// The text of `source` after the fixes that need no model call: the right
/// import form for each project file, and the "use client" line.
fn mechanical(path: &str, source: &str, files: &BTreeMap<String, String>) -> String {
    let known: BTreeSet<&str> = files.keys().map(String::as_str).collect();
    let mut lines: Vec<String> = Vec::new();
    for line in source.lines() {
        let spec = local_specifiers(line).into_iter().next();
        // An import of a file that is not there, when one project file
        // exports all its names: import from that file.
        let moved = spec
            .as_deref()
            .filter(|spec| resolve(path, spec, &known).is_none())
            .and_then(|spec| {
                let target = sole_exporter(line, path, files)?;
                let right = relative_specifier(path, target);
                Some(
                    line.replace(&format!("'{spec}'"), &format!("'{right}'"))
                        .replace(&format!("\"{spec}\""), &format!("\"{right}\"")),
                )
            });
        if let Some(moved) = moved {
            lines.push(moved);
            continue;
        }
        let target = spec
            .and_then(|spec| resolve(path, &spec, &known))
            .and_then(|target| files.get(&target));
        let fixed = target.and_then(|target| {
            fix_import_form(line, target).or_else(|| move_names(line, path, target, files))
        });
        lines.push(fixed.unwrap_or_else(|| line.to_string()));
    }
    let mut text = lines.join("\n");
    text.push('\n');
    let server_only = text.contains("export const metadata")
        || text.contains("generateMetadata")
        || text.contains("export default async");
    if path.ends_with(".tsx")
        && !is_test_file(path)
        && !server_only
        && crate::gate::needs_client(&text)
        && !crate::gate::is_client(&text)
    {
        text = format!("\"use client\";\n\n{text}");
    }
    // A file that exports `metadata` must stay a Server Component: drop a
    // "use client" line that it does not need (a model adds one to a layout
    // when it renders a client component there).
    if path.ends_with(".tsx")
        && server_only
        && crate::gate::is_client(&text)
        && !crate::gate::needs_client(&text)
    {
        let mut dropped = false;
        let kept: Vec<&str> = text
            .lines()
            .filter(|line| {
                let directive = !dropped
                    && matches!(
                        line.trim().trim_end_matches(';'),
                        "\"use client\"" | "'use client'"
                    );
                dropped |= directive;
                !directive
            })
            .collect();
        text = kept.join("\n").trim_start().to_string();
        text.push('\n');
    }
    // A component declared to return `unknown`/`any`/`void` cannot be used
    // as JSX (TS2786 lands in every file that renders it, not in this one):
    // drop the annotation so TypeScript infers the JSX type.
    if path.ends_with(".tsx") && !is_test_file(path) {
        text = drop_bad_component_return_types(&text);
    }
    // A client component that really needs the client (hooks, handlers) may
    // not export `metadata` either: drop the export (the page only loses its
    // tab title) rather than ask the model, which tends to loop on this.
    if path.ends_with(".tsx")
        && crate::gate::is_client(&text)
        && crate::gate::needs_client(&text)
        && text.contains("export const metadata")
    {
        text = drop_metadata_export(&text);
    }
    text
}

/// `export (default) function Name(...): unknown {` -> `... {` for a
/// capitalised (component) name; `any` and `void` likewise.
fn drop_bad_component_return_types(text: &str) -> String {
    let mut changed = false;
    let lines: Vec<String> = text
        .lines()
        .map(|line| {
            let trimmed = line.trim_start();
            let name = trimmed
                .strip_prefix("export default function ")
                .or_else(|| trimmed.strip_prefix("export function "))
                .or_else(|| trimmed.strip_prefix("function "));
            let is_component = name
                .and_then(|name| name.chars().next())
                .is_some_and(|first| first.is_ascii_uppercase());
            if !is_component {
                return line.to_string();
            }
            for bad in ["): unknown {", "): any {", "): void {"] {
                if line.trim_end().ends_with(bad) {
                    changed = true;
                    let cut = line.rfind(bad).expect("suffix found");
                    return format!("{}) {{", &line[..cut]);
                }
            }
            line.to_string()
        })
        .collect();
    if !changed {
        return text.to_string();
    }
    let mut result = lines.join("\n");
    if text.ends_with('\n') {
        result.push('\n');
    }
    result
}

/// Remove an `export const metadata ... ;` statement (one or more lines,
/// braces balanced) and a `Metadata` type import that is no longer used.
fn drop_metadata_export(text: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    let mut depth: i32 = 0;
    let mut skipping = false;
    for line in text.lines() {
        if !skipping && line.trim_start().starts_with("export const metadata") {
            skipping = true;
            depth = 0;
        }
        if skipping {
            depth += line.matches('{').count() as i32 - line.matches('}').count() as i32;
            let trimmed = line.trim_end();
            if depth <= 0 && (trimmed.ends_with(';') || trimmed.ends_with('}')) {
                skipping = false;
            }
            continue;
        }
        out.push(line);
    }
    let rest = out.join("\n");
    let uses_metadata = rest
        .lines()
        .filter(|line| !line.trim_start().starts_with("import"))
        .any(|line| line.contains("Metadata"));
    let kept: Vec<&str> = rest
        .lines()
        .filter(|line| {
            let metadata_import = line.trim_start().starts_with("import type")
                && line.contains("{ Metadata }")
                && (line.contains("\"next\"") || line.contains("'next'"));
            uses_metadata || !metadata_import
        })
        .collect();
    let mut result = kept.join("\n");
    result.push('\n');
    result
}

/// Apply [`mechanical`] to each written file. Returns the number of files
/// that changed.
fn fix_mechanical(plan: &Plan, report: &mut RunReport) -> usize {
    if !crate::arms::step_on("autofix") {
        return 0;
    }
    let mut files: BTreeMap<String, String> = BTreeMap::new();
    let mut ids: Vec<(String, String)> = Vec::new();
    for task in &plan.tasks {
        let path = &task.title;
        if !(path.ends_with(".ts") || path.ends_with(".tsx")) {
            continue;
        }
        let Some(record) = report.tasks.get(&task.id) else {
            continue;
        };
        if record.status != TaskStatus::Succeeded {
            continue;
        }
        if let Some(output) = &record.output {
            files.insert(path.clone(), crate::arms::file_text(path, output));
            ids.push((task.id.clone(), path.clone()));
        }
    }
    let mut changed = 0;
    for (id, path) in &ids {
        let old = &files[path];
        let new = mechanical(path, old, &files);
        if new.trim() != old.trim() {
            if let Some(record) = report.tasks.get_mut(id) {
                record.output = Some(format!("```\n{}\n```", new.trim_end()));
                changed += 1;
            }
        }
    }
    changed
}

/// Added to the repair of a test file that failed again after a repair.
const STUCK_TEST_RULE: &str = "\n\nThis test file failed again after one repair. For each test \
case that fails, read the code of the function and work out what it returns for the input of \
the test, step by step, and change the expectation to that value. If the right value is not \
certain from the code, delete that one test case and keep all the others. Do not change a \
test case that passes.";

/// Added to the joint repair of a test and the modules that it tests.
const TRACE_RULE: &str = "\n\n# Which side is wrong\nTwo repairs of the test did not help, so \
do not write the same test again. Follow each failing case through the code of the module, \
line by line, with the values of the test (for example: which id does a new record get, and \
does it collide with an id that the test data has?). If the module does not do what its plan \
line and its function names promise, the module is wrong: output the corrected module, and \
the test only if it must change too. If the module is right, output the test with the \
expectation that the code gives.";

/// Token limit of a joint repair: its answer can hold some files.
const JOINT_MAX_TOKENS: u32 = 16_000;

/// The files that a joint repair of `path` can change: the file, the
/// modules that its problems name (`import("lib/storage").AppState`), and
/// the shared types.
fn joint_files(path: &str, problems: &str, files: &BTreeMap<&str, &str>) -> Vec<String> {
    if is_test_file(path) {
        // The test and the modules that it imports and that the failing
        // cases name (`lib/board > moveTask ...`): at most two.
        let mut editable = vec![path.to_string()];
        let text = files.get(path).copied().unwrap_or_default();
        let lower = problems.to_lowercase();
        for file in imports(path, text, files).0 {
            let stem = file
                .rsplit('/')
                .next()
                .and_then(|name| name.split('.').next())
                .unwrap_or_default()
                .to_lowercase();
            if stem.len() > 2
                && lower.contains(&stem)
                && editable.len() < 3
                && !is_types_file(&file)
            {
                editable.push(file);
            }
        }
        return editable;
    }
    let mut editable = vec![path.to_string()];
    for part in problems.split("import(\"").skip(1) {
        let Some(module) = part.split('"').next() else {
            continue;
        };
        for extension in [".ts", ".tsx"] {
            let candidate = format!("{module}{extension}");
            if files.contains_key(candidate.as_str()) && !editable.contains(&candidate) {
                editable.push(candidate);
            }
        }
    }
    // The file that declares a type that a problem names (`does not exist
    // in type 'Action'`): the fix can be a field of that type.
    for part in problems.split("type '").skip(1) {
        let name: String = part
            .chars()
            .take_while(|ch| ch.is_alphanumeric() || *ch == '_')
            .collect();
        if name.is_empty() || !name.starts_with(|ch: char| ch.is_uppercase()) {
            continue;
        }
        for (file, text) in files {
            let declares_it =
                declares(text, "export interface ", &name) || declares(text, "export type ", &name);
            if declares_it
                && !is_test_file(file)
                && editable.len() < 5
                && !editable.iter().any(|known| known == file)
            {
                editable.push((*file).to_string());
            }
        }
    }
    // The files of a call stack (`lib/dates.ts:13`).
    for file in files.keys() {
        let named =
            problems.contains(&format!(" {file}:")) || problems.contains(&format!("< {file}:"));
        if named && !editable.iter().any(|known| known == file) {
            editable.push((*file).to_string());
        }
    }
    let render = problems.contains("when it renders the page") || problems.contains("TS9101");
    let wanted = |file: &str| {
        is_types_file(file)
            || (render && (file.ends_with("app/layout.tsx") || file.contains("Provider")))
    };
    for file in files.keys().filter(|file| wanted(file)) {
        if !editable.iter().any(|known| known == file) {
            editable.push((*file).to_string());
        }
    }
    editable
}

/// Types that the problems show with two declarations: the compiler writes
/// `import("lib/storage").AppState` and `import("components/Store").AppState`.
/// Returns (type name, modules that declare it).
fn double_declarations(problems: &str) -> Vec<(String, Vec<String>)> {
    let mut found: Vec<(String, Vec<String>)> = Vec::new();
    for part in problems.split("import(\"").skip(1) {
        let Some((module, rest)) = part.split_once("\").") else {
            continue;
        };
        let name: String = rest
            .chars()
            .take_while(|ch| ch.is_ascii_alphanumeric() || *ch == '_')
            .collect();
        if name.is_empty() {
            continue;
        }
        match found.iter_mut().find(|(known, _)| *known == name) {
            Some((_, modules)) => {
                if !modules.iter().any(|known| known == module) {
                    modules.push(module.to_string());
                }
            }
            None => found.push((name, vec![module.to_string()])),
        }
    }
    found.retain(|(_, modules)| modules.len() > 1);
    found
}

/// The request of a joint repair.
fn joint_prompt(
    task: &Task,
    problems: &str,
    editable: &[String],
    files: &BTreeMap<&str, &str>,
) -> String {
    let path = task.title.as_str();
    let text = files.get(path).copied().unwrap_or_default();
    let wanted: Vec<String> = imports(path, text, files)
        .0
        .into_iter()
        .filter(|file| !editable.contains(file))
        .collect();
    let mut doubles = String::new();
    for (name, modules) in double_declarations(problems) {
        let _ = writeln!(
            doubles,
            "The type `{name}` has a declaration in each of these files: `{}`. They differ. \
             Keep ONE declaration, give it every field that the other one has, delete the \
             other declaration, and import `{name}` there from the file that keeps it.",
            modules.join("`, `")
        );
    }
    let mut prompt = task.prompt.clone();
    let _ = write!(
        prompt,
        "\n\n# Joint repair\nA repair of `{path}` alone did not remove these problems:\n\
         ```\n{problems}```\nThe cause can be between two files: two files declare the same type \
         differently, or a file uses a property that the shared type does not have. Decide \
         which side is right for the app, and make the files agree. If the problem says that \
         a hook must be used within a provider, the page is not at fault: change the root \
         layout so that the provider is around `{{children}}`. If the layout has the provider \
         around `{{children}}` already, the provider file is at fault: its context value is \
         empty on the first render (for example while it loads from storage). Change the \
         provider so that the context has a complete, usable value on the very first render, \
         on the server too (the seed data or an empty state), and so that it never renders \
         `children` outside the context; it can load the stored state after that in an \
         effect. If two files declare the \
         same type, keep one declaration (in the shared types file if there is one) and import \
         it in the other file. Keep every feature.\n{doubles}\nYou may change these files:\n{}\n\
         Other project files, which do not change:\n{}\nOutput each file that you change, \
         complete: a heading line with the path in backticks (### `path/to/file`), then one \
         fenced code block with the whole file. Do not output a file that you do not change.",
        sources(editable, "", files),
        sources(&wanted, path, files),
    );
    prompt
}

/// The deliverable document that the gate reads (`engcheck` off), with
/// only the files that `keep` accepts.
fn assemble_where(report: &RunReport, keep: impl Fn(&str) -> bool) -> String {
    let mut doc = String::new();
    for record in report.tasks.values() {
        if record.status != TaskStatus::Succeeded || !keep(&record.title) {
            continue;
        }
        if let Some(output) = &record.output {
            let _ = write!(doc, "## {}\n\n{}\n\n", record.title, output.trim());
        }
    }
    doc
}

/// Check the deliverable in `report` and repair it, for at most `max_rounds`
/// rounds. `report` ends with the best files that the gate saw. A round can
/// add a file to `plan`: a module that a file imports but that nobody wrote.
/// The bench copy of the engine's loop (`engrepair` off; see [`run`]).
pub async fn check_and_repair(
    plan: &mut Plan,
    report: &mut RunReport,
    client: Arc<dyn ModelClient>,
    config: SchedulerConfig,
    gate_cmd: &str,
    max_rounds: usize,
) -> Result<RepairSummary> {
    let started = Instant::now();
    fix_import_paths(plan, report);
    fix_mechanical(plan, report);
    crate::shellfix::fix_pages(plan, report);
    let mut gate = Gate::new(gate_cmd);
    let before = gate.check_and_patch(report).await?;
    // `current` is the state that the next round repairs; `best` is the
    // state that is kept at the end.
    let mut current = before.clone();
    let mut best = before.clone();
    let mut best_report = report.clone();
    let mut best_planned = plan.tasks.len();
    let mut rounds: Vec<Round> = Vec::new();
    let mut changed: Vec<String> = Vec::new();
    let mut repaired: BTreeSet<String> = BTreeSet::new();
    let mut stale = 0;
    // Files that are written in full from now on: their edits could not be
    // placed, or a repair did not remove their problems.
    let mut full_only: BTreeSet<String> = BTreeSet::new();
    // Files whose warnings were given to a repair (one time for each file).
    let mut hinted: BTreeSet<String> = BTreeSet::new();
    // Files that the round before repaired with a joint repair.
    let mut joint_before: BTreeSet<String> = BTreeSet::new();
    // Files that the round before repaired.
    let mut last_repaired: BTreeSet<String> = BTreeSet::new();
    // Test files that the round before repaired alone, as stuck tests.
    let mut test_alone_before: BTreeSet<String> = BTreeSet::new();

    while !current.passed && rounds.len() < max_rounds && stale < 3 {
        let round_started = Instant::now();
        let mut repairs: Vec<Task> = Vec::new();
        let mut added: Vec<Task> = Vec::new();
        let mut edited: BTreeSet<String> = BTreeSet::new();
        let mut test_alone: BTreeSet<String> = BTreeSet::new();
        // Joint repairs: task id -> the files that the answer can change.
        let mut joint: BTreeMap<String, Vec<String>> = BTreeMap::new();
        let ids: BTreeMap<String, String> = plan
            .tasks
            .iter()
            .map(|task| (task.title.clone(), task.id.clone()))
            .collect();
        {
            let outputs: BTreeMap<&str, &str> = plan
                .tasks
                .iter()
                .filter_map(|task| {
                    let record = report.tasks.get(&task.id)?;
                    (record.status == TaskStatus::Succeeded)
                        .then_some(record.output.as_deref())
                        .flatten()
                        .map(|output| (task.title.as_str(), output))
                })
                .collect();
            // The files that a repair may change, and the files that a joint
            // repair of this round may change (a named file joins one).
            let repairable: BTreeSet<&str> = plan
                .tasks
                .iter()
                .filter(|task| task.static_output.is_none() && !task.internal)
                .map(|task| task.title.as_str())
                .collect();
            let mut claimed: BTreeSet<String> = BTreeSet::new();
            for task in plan
                .tasks
                .iter()
                .filter(|task| task.static_output.is_none() && !task.internal)
            {
                let Some(text) = outputs.get(task.title.as_str()) else {
                    continue;
                };
                let mut problems = current
                    .problems
                    .get(&task.title)
                    .cloned()
                    .unwrap_or_default();
                // A page that does not render often fails for a reason outside
                // the page (a provider that the layout does not set up).
                let render =
                    problems.contains("when it renders the page") || problems.contains("TS9101");
                let stuck = !problems.is_empty() && (render || last_repaired.contains(&task.title));
                // A warning about a second copy of a type matters only when
                // the compiler reports a conflict with that copy: its message
                // names the module, as in `import("lib/tasks").Task`.
                let module = task
                    .title
                    .rsplit_once('.')
                    .map_or(task.title.as_str(), |(stem, _)| stem);
                let conflict = format!("import(\"{module}\")");
                let in_conflict = current
                    .problems
                    .values()
                    .any(|text| text.contains(&conflict));
                if let Some(hint) = current.hints.get(&task.title).filter(|_| in_conflict) {
                    if hinted.insert(task.title.clone()) {
                        problems.push_str(hint);
                    }
                }
                if problems.is_empty() {
                    continue;
                }
                // A test that failed after its own repair and after the
                // repair below: the code can be the wrong side. Repair it
                // together with the modules that the failing cases name.
                let twice =
                    joint_before.contains(&task.title) || test_alone_before.contains(&task.title);
                if stuck && is_test_file(&task.title) && crate::arms::step_on("testonly") && !twice
                {
                    test_alone.insert(task.title.clone());
                    // A test changes to agree with the code. Its repair
                    // never changes the code: that was slow and it made
                    // other files worse.
                    let mut repair = task.clone();
                    repair.prompt = repair_prompt(task, &problems, text, &outputs, &[], false);
                    repair.prompt.push_str(STUCK_TEST_RULE);
                    // The same request at the same temperature gave the
                    // same wrong answer three times: ask for another one.
                    repair.temperature = Some(0.8);
                    repair.deps.clear();
                    repairs.push(repair);
                    continue;
                }
                // The files that the problems name as the place to change
                // (`lib/search.ts` in a lint) join the repair at once, unless
                // they have problems of their own (their own repair writes
                // them this round). Race 20261116-154325 lost a round to a
                // page repaired without the module that the lint named.
                // Never a scaffold or internal file, never a file that an
                // earlier joint repair of this round may change; a test that
                // is not stuck yet is repaired alone and names nothing.
                let named: Vec<String> = if is_test_file(&task.title) && !stuck {
                    Vec::new()
                } else {
                    slimtoken::repair::named_files(&task.title, &problems, &outputs, is_test_file)
                        .into_iter()
                        .filter(|file| {
                            repairable.contains(file.as_str())
                                && !claimed.contains(file)
                                && current
                                    .problems
                                    .get(file)
                                    .is_none_or(|text| text.trim().is_empty())
                        })
                        .collect()
                };
                let missing = problems.contains("TS2307") || problems.contains("Module not found");
                let named_now = !stuck && !named.is_empty() && !missing;
                if stuck || named_now {
                    // Its own repair did not help, or its problems name
                    // another file as the place to change: repair it together
                    // with the files that its problems name.
                    let mut editable = if stuck {
                        joint_files(&task.title, &problems, &outputs)
                    } else {
                        vec![task.title.clone()]
                    };
                    for file in named {
                        if !editable.contains(&file) {
                            editable.push(file);
                        }
                    }
                    let mut repair = task.clone();
                    repair.prompt = joint_prompt(task, &problems, &editable, &outputs);
                    if named_now {
                        repair.prompt =
                            slimtoken::repair::named_joint_prompt(&repair.prompt, &task.title);
                    }
                    repair.deps.clear();
                    repair.validate_artifact = false;
                    repair.max_tokens = repair.max_tokens.max(JOINT_MAX_TOKENS);
                    if is_test_file(&task.title) {
                        // The test can be right and the module wrong. To
                        // see which, the model must follow the code: that
                        // needs reasoning, which the workers do not use.
                        repair.prompt.push_str(TRACE_RULE);
                        // Reasoning on a test repair ran 213 s and 47 s in
                        // race 20261116-051055 (16k hidden tokens, no answer,
                        // then a retry): off unless `thinktests` is on.
                        if crate::arms::step_on("think") && crate::arms::step_on("thinktests") {
                            repair.reasoning_effort = Some("medium".to_string());
                        }
                    }
                    if joint_before.contains(&task.title) {
                        // The same request gave an answer that did not
                        // help: ask for a different one, with reasoning.
                        repair.temperature = Some(0.8);
                        let test = is_test_file(&task.title);
                        if crate::arms::step_on("think")
                            && (!test || crate::arms::step_on("thinktests"))
                        {
                            repair.reasoning_effort = Some("medium".to_string());
                        }
                    }
                    claimed.extend(editable.iter().cloned());
                    joint.insert(task.id.clone(), editable);
                    repairs.push(repair);
                    continue;
                }
                // A missing relative import becomes a new file.
                let mut writing = Vec::new();
                for spec in imports(&task.title, text, &outputs).1 {
                    let Some(new_path) = missing_module_path(&task.title, &spec) else {
                        continue;
                    };
                    if !added.iter().any(|extra| extra.title == new_path) {
                        added.push(missing_module_task(task, text, &spec, &new_path, &outputs));
                    }
                    writing.push(spec);
                }
                // The file itself needs no repair if its only problem is the
                // import of a file that is written now.
                let other_problems = problems.lines().any(|line| {
                    !line.contains("TS2307")
                        && !line.contains("Module not found")
                        && !line.trim().is_empty()
                });
                if writing.is_empty() || other_problems {
                    let edits = writing.is_empty()
                        && !full_only.contains(&task.title)
                        && text.lines().count() >= EDIT_MIN_LINES
                        && problems.lines().count() <= EDIT_MAX_PROBLEM_LINES;
                    let mut repair = task.clone();
                    repair.prompt = repair_prompt(task, &problems, text, &outputs, &writing, edits);
                    repair.deps.clear();
                    if edits {
                        // The answer is edit blocks, not a file.
                        repair.require_closed_fences = false;
                        repair.validate_artifact = false;
                        edited.insert(task.id.clone());
                    }
                    repairs.push(repair);
                }
            }
        }
        if repairs.is_empty() && added.is_empty() {
            break;
        }
        last_repaired = repairs.iter().map(|task| task.title.clone()).collect();
        test_alone_before = test_alone;
        joint_before = repairs
            .iter()
            .filter(|task| joint.contains_key(&task.id))
            .map(|task| task.title.clone())
            .collect();
        let mut files: Vec<String> = repairs.iter().map(|task| task.title.clone()).collect();
        files.extend(added.iter().map(|task| task.title.clone()));
        let mut tasks = repairs;
        tasks.extend(added.iter().cloned());
        let round = execute_plan(
            &Plan {
                objective: plan.objective.clone(),
                tasks,
                cache_primes: Vec::new(),
            },
            client.clone(),
            config.clone(),
        )
        .await?;

        let mut output_tokens = 0;
        let mut answers: Vec<(String, String, Vec<String>)> = Vec::new();
        // For work on this step: SHOWDOWN_REPAIR_LOG names a file that
        // receives the answer of each repair request.
        if let Ok(path) = std::env::var("SHOWDOWN_REPAIR_LOG") {
            if let Ok(mut log) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
            {
                for (id, fixed) in &round.tasks {
                    let kind = if joint.contains_key(id) {
                        "joint"
                    } else if edited.contains(id) {
                        "edits"
                    } else {
                        "full"
                    };
                    let _ = writeln!(
                        log,
                        "==== round {} {kind} {id} status {:?} may change {:?}\n{}\n",
                        rounds.len() + 1,
                        fixed.status,
                        joint.get(id),
                        fixed.output.as_deref().unwrap_or("(no output)")
                    );
                }
            }
        }
        for (id, fixed) in &round.tasks {
            output_tokens += fixed.output_tokens;
            // Tokens are spent whether or not the round is kept.
            report.total_input_tokens += fixed.input_tokens;
            report.total_output_tokens += fixed.output_tokens;
            let done = fixed.status == TaskStatus::Succeeded && fixed.output.is_some();
            if let Some(record) = report.tasks.get_mut(id) {
                record.attempts += fixed.attempts;
                record.input_tokens += fixed.input_tokens;
                record.output_tokens += fixed.output_tokens;
                if let Some(editable) = joint.get(id) {
                    if let Some(answer) = fixed.output.clone().filter(|_| done) {
                        answers.push((record.title.clone(), answer, editable.clone()));
                    }
                } else if done && edited.contains(id) {
                    let placed = record
                        .output
                        .as_deref()
                        .zip(fixed.output.as_deref())
                        .and_then(|(current, blocks)| place_edits(current, blocks));
                    match placed {
                        Some(text) => record.output = Some(text),
                        None => {
                            full_only.insert(record.title.clone());
                        }
                    }
                } else if done {
                    record.output = fixed.output.clone();
                }
            } else if let Some(extra) = added.iter().find(|task| task.id == *id).filter(|_| done) {
                // A new file: it joins the plan and the report.
                report.tasks.insert(id.clone(), fixed.clone());
                plan.tasks.push(extra.clone());
            }
        }
        // The answers of joint repairs: each names the files that it changes.
        for (stuck, answer, editable) in answers {
            // For work on this step: SHOWDOWN_REPAIR_LOG names a file that
            // receives each joint answer.
            if let Ok(path) = std::env::var("SHOWDOWN_REPAIR_LOG") {
                if let Ok(mut log) = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(path)
                {
                    let _ = writeln!(
                        log,
                        "==== joint repair of {stuck}; may change {editable:?}
{answer}
"
                    );
                }
            }
            let mut applied = false;
            let found = if engcheck() {
                slimtoken::project::scan(&answer)
            } else {
                crate::extract::scan(&answer)
            };
            for file in found {
                if !file.closed || !editable.contains(&file.path) {
                    continue;
                }
                if let Some(record) = ids.get(&file.path).and_then(|id| report.tasks.get_mut(id)) {
                    record.output = Some(format!("```\n{}\n```", file.content.trim_end()));
                    applied = true;
                }
            }
            // One code block with no path is the stuck file itself.
            let fences = answer
                .lines()
                .filter(|line| line.trim_start().starts_with("```"))
                .count();
            if !applied && fences == 2 {
                if let Some(record) = ids.get(&stuck).and_then(|id| report.tasks.get_mut(id)) {
                    record.output = Some(answer);
                }
            }
        }
        fix_import_paths(plan, report);
        fix_mechanical(plan, report);
        crate::shellfix::fix_pages(plan, report);
        let after = gate.check_and_patch(report).await?;
        changed.extend(files.iter().cloned());
        let kept = after.passed || better(&after, &best);
        if kept {
            best = after.clone();
            best_report = report.clone();
            best_planned = plan.tasks.len();
            repaired.extend(changed.drain(..));
            stale = 0;
        } else {
            stale += 1;
        }
        current = after.clone();
        rounds.push(Round {
            files,
            after,
            kept,
            output_tokens,
            seconds: round_started.elapsed().as_secs_f64(),
        });
    }
    gate.finish().await;
    // Keep the best files; the tokens of every round stay counted.
    let (input_tokens, output_tokens) = (report.total_input_tokens, report.total_output_tokens);
    *report = best_report;
    report.total_input_tokens = input_tokens;
    report.total_output_tokens = output_tokens;
    plan.tasks.truncate(best_planned);
    Ok(RepairSummary {
        before,
        after: best,
        rounds,
        repaired: repaired.into_iter().collect(),
        seconds: started.elapsed().as_secs_f64(),
        guard: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_import_above_the_root_does_not_resolve() {
        let known: BTreeSet<&str> = ["lib/types.ts", "app/page.tsx"].into_iter().collect();
        assert_eq!(
            resolve("app/page.tsx", "../lib/types", &known).as_deref(),
            Some("lib/types.ts")
        );
        assert_eq!(resolve("app/page.tsx", "../../lib/types", &known), None);
        assert_eq!(
            intended_file("../../lib/types", &known),
            Some("lib/types.ts")
        );
    }

    #[test]
    fn mechanical_drops_metadata_from_a_layout_that_needs_the_client() {
        let layout = "\"use client\";\nimport type { ReactNode } from \"react\";\nimport type { Metadata } from \"next\";\nimport { useState } from \"react\";\nimport Nav from \"../components/Nav\";\n\nexport const metadata: Metadata = {\n  title: \"TaskFlow\",\n};\n\nexport default function RootLayout(p: { children: ReactNode }) {\n  const [q, setQ] = useState(\"\");\n  return <html><body><Nav q={q} onQ={setQ} />{p.children}</body></html>;\n}\n";
        let files = BTreeMap::new();
        let fixed = mechanical("app/layout.tsx", layout, &files);
        assert!(!fixed.contains("metadata"), "{fixed}");
        assert!(!fixed.contains("Metadata"), "{fixed}");
        assert!(fixed.starts_with("\"use client\""), "{fixed}");
        assert!(fixed.contains("useState(\"\")"), "{fixed}");
        assert!(
            fixed.contains("export default function RootLayout"),
            "{fixed}"
        );
    }

    #[test]
    fn mechanical_drops_an_unknown_return_type_from_a_component() {
        let nav = "\"use client\";\nimport Link from \"next/link\";\n\nexport default function Nav(): unknown {\n  return <nav><Link href=\"/\">Home</Link></nav>;\n}\n\nfunction helper(): unknown {\n  return 1;\n}\n";
        let files = BTreeMap::new();
        let fixed = mechanical("components/Nav.tsx", nav, &files);
        assert!(fixed.contains("export default function Nav() {"), "{fixed}");
        assert!(fixed.contains("function helper(): unknown {"), "{fixed}");
    }

    #[test]
    fn mechanical_drops_use_client_from_a_layout_with_metadata() {
        let layout = "\"use client\";\nimport AppShell from '../components/AppShell';\n\nexport const metadata = { title: 'X' };\n\nexport default function RootLayout({ children }) {\n  return <AppShell>{children}</AppShell>;\n}\n";
        let files = BTreeMap::new();
        let fixed = mechanical("app/layout.tsx", layout, &files);
        assert!(!fixed.contains("use client"), "{fixed}");
        assert!(fixed.starts_with("import AppShell"));
    }

    #[test]
    fn mechanical_fixes_the_import_form_and_the_client_line() {
        let mut files = BTreeMap::new();
        files.insert(
            "components/Card.tsx".to_string(),
            "export default function Card() { return null; }\n".to_string(),
        );
        files.insert(
            "lib/a.ts".to_string(),
            "export function total(): number { return 1; }\n".to_string(),
        );
        let page = "import { Card } from '../components/Card';\nimport total from '../lib/a';\n\
                    export default function Page() {\n  return <button onClick={() => total()}><Card /></button>;\n}\n";
        let fixed = mechanical("app/page.tsx", page, &files);
        assert!(fixed.starts_with("\"use client\";"), "{fixed}");
        assert!(
            fixed.contains("import Card from '../components/Card';"),
            "{fixed}"
        );
        assert!(
            fixed.contains("import { total } from '../lib/a';"),
            "{fixed}"
        );
        // A name that one other file exports, imported from a file that is not there.
        let seed = "import type { total } from './totals';\n";
        assert_eq!(
            mechanical("lib/seed.ts", seed, &files),
            "import type { total } from './a';\n"
        );
        // A name that the named file does not export, and one other file does.
        files.insert(
            "lib/types.ts".to_string(),
            "export interface Task { id: string }\n".to_string(),
        );
        let mixed = "import type { Task, total } from './types';\n";
        assert_eq!(
            mechanical("lib/seed.ts", mixed, &files),
            "import type { Task } from './types';\nimport type { total } from './a';\n"
        );
        // A right file does not change.
        assert_eq!(mechanical("app/page.tsx", &fixed, &files), fixed);
        // A layout with metadata stays a Server Component.
        let layout = "export const metadata = {};\nexport default function L() { return <a onClick={() => {}} />; }\n";
        assert_eq!(mechanical("app/layout.tsx", layout, &files), layout);
    }

    #[test]
    fn gate_output_is_grouped_by_file() {
        let out = "ERROR app/page.tsx(28,18): error TS2339: Property 'activity' does not exist.\n\
                   ERROR app/tasks/[id]/page.tsx(2,30): error TS2307: Cannot find module './X'.\n\
                   FAILED tests/all.test.ts > store > reduces actions\n    AssertionError: expected +0 to be 1\n\
                   3 failed, 15 passed (2 type errors)\n";
        let result = parse_gate(false, out);
        assert!(!result.passed);
        assert_eq!(
            (result.failed, result.tests_passed, result.tests),
            (3, 15, 16)
        );
        assert_eq!(result.problems.len(), 3);
        assert!(result.problems["app/tasks/[id]/page.tsx"].contains("TS2307"));
        assert!(result.problems["tests/all.test.ts"].contains("AssertionError"));
        // Output with no summary line is never a pass.
        assert!(!parse_gate(true, "").passed);
        assert!(parse_gate(true, "0 failed, 4 passed (0 type errors)\n").passed);
        // A warning is a hint, not a problem; the real build ranks above the fast checks.
        let fast = parse_gate(
            false,
            "WARN lib/a.ts(3,1): error TS9003: again\nERROR lib/b.ts(1,1): error TS1: x\n1 failed, 4 passed (1 type errors)\n",
        );
        assert_eq!(
            (fast.problems.len(), fast.hints.len(), fast.deep),
            (1, 1, false)
        );
        let deep = parse_gate(
            false,
            "ERROR app/page.tsx(1,1): error TS9100: build\nERROR app/x.tsx(1,1): error TS9100: build\n2 failed, 4 passed (2 type errors) [deep]\n",
        );
        assert!(deep.deep && better(&deep, &fast) && !better(&fast, &deep));
    }

    /// Gate outputs rebuilt from the results that bench runs recorded
    /// (`_meta/slimtoken-repair.json`): the problem text that `parse_gate`
    /// stored plus the summary line, not the raw gate stdout. They hold
    /// only lines that `parse_gate` attributed; raw shapes and the rules
    /// where `parse_check` differs are tested below. Shared with the
    /// engine's tests.
    const RECORDED: &str = include_str!("../../../tests/fixtures/gate-results.json");

    #[test]
    fn engine_and_bench_give_the_same_verdicts_on_recorded_gate_outputs() {
        #[derive(serde::Deserialize)]
        struct Recorded {
            name: String,
            exit_ok: bool,
            output: String,
        }
        let recorded: Vec<Recorded> = serde_json::from_str(RECORDED).unwrap();
        assert!(recorded.len() >= 10);
        let mut outputs: Vec<(bool, String)> = recorded
            .into_iter()
            .map(|case| {
                let bench = parse_gate(case.exit_ok, &case.output);
                let engine = slimtoken::check::parse_check(case.exit_ok, &case.output);
                assert_eq!(bench, engine, "{}", case.name);
                (case.exit_ok, case.output)
            })
            .collect();
        // The outputs of the gate's own format test, and edge cases.
        for out in [
            "WARN lib/a.ts(3,1): error TS9003: again\nERROR lib/b.ts(1,1): error TS1: x\n1 failed, 4 passed (1 type errors)\n",
            "ERROR app/page.tsx(1,1): error TS9100: build\n2 failed, 4 passed (2 type errors) [deep]\n",
            "ERROR gate: no node_modules/typescript under x\n1 failed, 0 passed (toolchain missing)\n",
            "0 failed, 40 passed (0 type errors) [deep]\n",
            "",
            // Raw stdout in the shapes that `gate.rs` prints: build errors
            // with a call stack, `ERROR gate:` tails that quote a location,
            // a failed test with its indented lines, a lint.
            "ERROR app/x.tsx(33,24): error TS9100: `next build` fails: Type error: x | y\n\
             ERROR app/board/page.tsx(1,1): error TS9100: `next build` fails when it renders the page /board: Error: boom | call stack (file:line): lib/a.ts:3 < app/board/page.tsx:9\n\
             ERROR gate: `next build` failed: Error at ./app/a.tsx(3,4): boom | Export encountered errors\n\
             FAILED tests/a.test.ts > store > removes\n    AssertionError: expected 1 to be 2\n    tests/a.test.ts(12,5): at line\n\
             WARN lib/tasks.ts(2,1): error TS9003: `Task` is declared here again\n\
             ERROR gate: the type check did not run: error TS5058: no path 'C:/p/tsconfig.json(1,1): x'.\n\
             4 failed, 3 passed (3 type errors) [deep]\n",
        ] {
            for exit_ok in [false, true] {
                let bench = parse_gate(exit_ok, out);
                let engine = slimtoken::check::parse_check(exit_ok, out);
                assert_eq!(bench, engine, "{out:?}");
                outputs.push((exit_ok, out.to_string()));
            }
        }
        // Which result is kept: the same on both paths.
        let results: Vec<GateResult> = outputs
            .iter()
            .map(|(exit_ok, out)| parse_gate(*exit_ok, out))
            .collect();
        for new in &results {
            for old in &results {
                assert_eq!(bench_better(new, old), new.nearer_than(old));
            }
        }
    }

    /// Where the engine's parser reads more than the bench's: lines that
    /// the gate does not print. The verdict (pass, counts) stays the same.
    #[test]
    fn engine_parser_differs_only_on_lines_the_gate_does_not_print() {
        let wider = [
            // A located line with no `ERROR ` prefix (tsc's own form).
            (
                "app/page.tsx(3,4): error TS2339: x\n1 failed, 0 passed (1 type errors)\n",
                "app/page.tsx",
            ),
            // An `ERROR` line whose message is not `error TS...`.
            (
                "ERROR lib/b.py(3,1): E302 expected 2 blank lines\n1 failed, 0 passed (1 type errors)\n",
                "lib/b.py",
            ),
        ];
        for (out, file) in wider {
            let bench = parse_gate(false, out);
            let engine = slimtoken::check::parse_check(false, out);
            assert!(bench.problems.is_empty(), "{out:?}");
            assert_eq!(engine.problems.keys().collect::<Vec<_>>(), vec![file]);
            assert_eq!(
                (bench.passed, bench.failed, bench.tests, bench.deep),
                (engine.passed, engine.failed, engine.tests, engine.deep)
            );
        }
        // A `PATCH` block: a corrected file for the engine; for the bench
        // its lines are prose (outside a `FAILED`/`ERROR` entry).
        let out =
            "PATCH lib/a.ts\n```ts\nexport const a = 1;\n```\n0 failed, 2 passed (0 type errors)\n";
        let bench = parse_gate(true, out);
        let engine = slimtoken::check::parse_check(true, out);
        assert!(bench.patches.is_empty());
        assert_eq!(engine.patches["lib/a.ts"], "export const a = 1;\n");
        assert_eq!(
            GateResult {
                patches: BTreeMap::new(),
                ..engine
            },
            bench
        );
    }

    #[test]
    fn the_gate_reads_the_same_files_from_the_engine_document() {
        let outputs = [
            (
                "lib/types.ts",
                "```ts\nexport interface Task { id: string }\n```",
            ),
            (
                "app/tasks/[id]/page.tsx",
                "Here is the page:\n```tsx\nexport default function Page() {}\n```\n",
            ),
            (
                "README.md",
                "```markdown\n# App\n\n```bash\nnpm run dev\n```\n\nDone.\n```",
            ),
            (
                "app/(main)/board/page.tsx",
                "```tsx\nconst s = '```';\nexport default s;\n```",
            ),
            (
                "lib/seed.ts",
                "```ts\n// lib/seed.ts\nexport const seed = [];\n```",
            ),
        ];
        // The bench's document (`assemble_where`) and the engine's.
        let mut doc = String::new();
        let mut files = BTreeMap::new();
        for (path, output) in outputs {
            doc.push_str(&format!("## {path}\n\n{}\n\n", output.trim()));
            files.insert(
                path.to_string(),
                slimtoken::project::file_text(path, output),
            );
        }
        let read = |doc: &str| -> BTreeMap<String, (String, bool)> {
            crate::extract::scan(doc)
                .into_iter()
                .map(|file| (file.path, (file.content, file.closed)))
                .collect()
        };
        let bench = read(&doc);
        assert_eq!(bench.len(), outputs.len());
        assert_eq!(read(&slimtoken::project::render_doc(&files)), bench);
        // An answer with no code block: the bench's document gives the gate
        // no file (the run still writes it); the engine's gives the file.
        let bare = "## lib/a.ts\n\nexport const a = 1;\n\n";
        assert!(read(bare).is_empty());
        let mut one = BTreeMap::new();
        one.insert(
            "lib/a.ts".to_string(),
            slimtoken::project::file_text("lib/a.ts", "export const a = 1;"),
        );
        assert_eq!(
            read(&slimtoken::project::render_doc(&one))["lib/a.ts"].0,
            "export const a = 1;\n"
        );
    }

    #[test]
    fn imports_resolve_to_project_files() {
        let source = "import { a } from '@/lib/types';\nimport {\n  b,\n} from \"../store\";\n\
                      import React from 'react';\nimport './TaskDetailClient';\n";
        assert_eq!(
            local_specifiers(source),
            vec!["@/lib/types", "../store", "./TaskDetailClient"]
        );
        let known: BTreeSet<&str> = ["lib/types.ts", "app/store/index.tsx"].into();
        let path = "app/tasks/page.tsx";
        assert_eq!(
            resolve(path, "@/lib/types", &known).as_deref(),
            Some("lib/types.ts")
        );
        assert_eq!(
            resolve(path, "../store", &known).as_deref(),
            Some("app/store/index.tsx")
        );
        assert_eq!(resolve(path, "./TaskDetailClient", &known), None);
        assert_eq!(
            missing_module_path("app/tasks/[id]/page.tsx", "./TaskDetailClient").as_deref(),
            Some("app/tasks/[id]/TaskDetailClient.tsx")
        );
        assert_eq!(missing_module_path("app/page.tsx", "@/lib/nothing"), None);
        assert_eq!(missing_module_path("app/page.tsx", "./styles.css"), None);
        let file = "a\nb\nc\nb2\n";
        let blocks = "<<<<<<< SEARCH\nc\n=======\nC\nD\n>>>>>>> REPLACE\n";
        assert_eq!(
            apply_edit_blocks(file, blocks).as_deref(),
            Some("a\nb\nC\nD\nb2\n")
        );
        // A SEARCH that is not in the file cannot be placed.
        assert_eq!(
            apply_edit_blocks(file, &blocks.replace("\nc\n", "\nzz\n")),
            None
        );
        assert_eq!(apply_edit_blocks(file, "no blocks"), None);
        let files: BTreeMap<&str, &str> = [
            ("lib/types.ts", "x"),
            ("lib/storage.ts", "y"),
            ("components/StoreProvider.tsx", "z"),
        ]
        .into();
        assert_eq!(
            joint_files(
                "components/StoreProvider.tsx",
                "Property 'settings' is missing in type 'import(\"lib/storage\").AppState'",
                &files
            ),
            vec![
                "components/StoreProvider.tsx",
                "lib/storage.ts",
                "lib/types.ts"
            ]
        );
        let doubles = double_declarations(
            "type 'import(\"lib/storage\").AppState' is not assignable to type 'import(\"components/StoreProvider\").AppState'. 'import(\"lib/types\").Task'",
        );
        assert_eq!(
            doubles,
            vec![(
                "AppState".to_string(),
                vec![
                    "lib/storage".to_string(),
                    "components/StoreProvider".to_string()
                ]
            )]
        );
        // One `../` too many: the one file that can be meant, and the right path to it.
        let known: BTreeSet<&str> = ["lib/types.ts", "lib/team.ts", "components/Nav.tsx"].into();
        assert_eq!(
            intended_file("../../../../lib/types", &known),
            Some("lib/types.ts")
        );
        assert_eq!(intended_file("./TaskDetailClient", &known), None);
        assert_eq!(
            relative_specifier("app/tasks/[id]/page.tsx", "lib/types.ts"),
            "../../../lib/types"
        );
        assert_eq!(
            relative_specifier("lib/tasks.ts", "lib/types.ts"),
            "./types"
        );
        assert_eq!(
            relative_specifier("app/page.tsx", "components/Nav.tsx"),
            "../components/Nav"
        );
    }

    /// The project of the parity tests: a page with a missing import, a
    /// test, a store with a second copy of a type, the shared types.
    fn parity_files() -> BTreeMap<&'static str, &'static str> {
        [
            (
                "app/page.tsx",
                "```tsx\nimport { useStore } from '../components/StoreProvider';\nimport Widget from './Widget';\nimport { Task } from '@/lib/types';\nexport default function Page() { return null; }\n```",
            ),
            (
                "components/StoreProvider.tsx",
                "```tsx\nimport { Task } from '../lib/types';\nexport interface AppState { tasks: Task[] }\nexport function useStore() {}\n```",
            ),
            (
                "lib/storage.ts",
                "```ts\nexport interface AppState { items: string[] }\nexport function load() {}\n```",
            ),
            (
                "lib/board.ts",
                "```ts\nimport { Task } from './types';\nexport function moveTask(t: Task) { return t; }\n```",
            ),
            ("lib/types.ts", "```ts\nexport interface Task { id: string }\n```"),
            (
                "tests/board.test.ts",
                "```ts\nimport { moveTask } from '../lib/board';\nimport { Task } from '../lib/types';\n```",
            ),
            ("app/layout.tsx", "```tsx\nexport default function L() {}\n```"),
            ("app/globals.css", ".a {}"),
        ]
        .into_iter()
        .collect()
    }

    const PARITY_PROBLEMS: &[(&str, &str)] = &[
        (
            "app/page.tsx",
            "ERROR app/page.tsx(2,20): error TS2307: Cannot find module './Widget'.\n\
             ERROR app/page.tsx(9,3): error TS2322: Type 'import(\"lib/storage\").AppState' is not \
             assignable to type 'import(\"components/StoreProvider\").AppState'.\n",
        ),
        (
            "app/page.tsx",
            "ERROR app/page.tsx(1,1): error TS9101: useStore must be used within a provider \
             when it renders the page\n    at lib/board.ts:3\n",
        ),
        (
            "components/StoreProvider.tsx",
            "ERROR components/StoreProvider.tsx(2,1): error TS2339: Property 'x' does not \
             exist on type 'Task'.\n",
        ),
        (
            "tests/board.test.ts",
            "FAILED tests/board.test.ts > board > moveTask keeps the id\n    AssertionError\n",
        ),
    ];

    #[test]
    fn engine_hooks_match_the_bench_copy() {
        let files = parity_files();
        let known: BTreeSet<&str> = files.keys().copied().collect();
        for (path, text) in &files {
            assert_eq!(
                slimtoken::profile::next_local_specifiers(text),
                local_specifiers(text),
                "{path}"
            );
            for spec in local_specifiers(text) {
                assert_eq!(
                    slimtoken::profile::next_resolve_import(path, &spec, &known),
                    resolve(path, &spec, &known),
                    "{path} {spec}"
                );
                assert_eq!(
                    (NEXTJS_REPAIR.missing_module_path)(path, &spec),
                    missing_module_path(path, &spec),
                    "{path} {spec}"
                );
            }
            assert_eq!((NEXTJS_REPAIR.is_test)(path), is_test_file(path));
            assert_eq!((NEXTJS_REPAIR.is_shared_types)(path), is_types_file(path));
        }
        for (path, problems) in PARITY_PROBLEMS {
            assert_eq!(
                slimtoken::profile::next_joint_files(path, problems, &files),
                joint_files(path, problems, &files),
                "{path}"
            );
        }
    }

    #[test]
    fn a_lint_that_names_a_module_names_it_for_the_joint_repair() {
        let problems = "ERROR app/search/page.tsx(92,1): error TS9028: search-link: a comment \
            search hit has the comment's id (`id: comment.id`, `lib/search.ts`), but this file \
            links hits to `/tasks/${...id}`: a comment result opens a page for a record that does \
            not exist. Search results link to the record that owns them: give the comment hit its \
            task id in `lib/search.ts` (line 75: `taskId: comment.taskId`) and link it to \
            `/tasks/${hit.taskId}`.\n";
        let files: BTreeMap<&str, &str> = [
            ("app/search/page.tsx", "x"),
            ("lib/search.ts", "y"),
            ("tests/search.test.ts", "z"),
        ]
        .into();
        let named = |problems| {
            slimtoken::repair::named_files("app/search/page.tsx", problems, &files, is_test_file)
        };
        assert_eq!(named(problems), ["lib/search.ts"]);
        assert!(named("ERROR app/search/page.tsx(1,1): TS2339 no file\n").is_empty());
        assert!(named("see `tests/search.test.ts`").is_empty());
    }

    #[test]
    fn engine_prompts_match_the_bench_copy() {
        use slimtoken::repair::{joint_prompt as engine_joint, repair_prompt as engine_repair};
        let files = parity_files();
        for (path, problems) in PARITY_PROBLEMS {
            let task = Task {
                id: "t".into(),
                title: (*path).to_string(),
                prompt: format!("Write {path}."),
                ..Task::default()
            };
            let text = files[path];
            let added = ["./Widget".to_string()];
            for edits in [false, true] {
                for added in [&added[..], &[]] {
                    assert_eq!(
                        engine_repair(
                            &task,
                            problems,
                            text,
                            &files,
                            added,
                            edits,
                            &NEXTJS_REPAIR,
                            CONTEXT_CHARS
                        ),
                        repair_prompt(&task, problems, text, &files, added, edits),
                        "{path} edits {edits}"
                    );
                }
            }
            let editable = joint_files(path, problems, &files);
            assert_eq!(
                engine_joint(
                    &task,
                    problems,
                    &editable,
                    &files,
                    &NEXTJS_REPAIR,
                    CONTEXT_CHARS
                ),
                joint_prompt(&task, problems, &editable, &files),
                "{path}"
            );
        }
        let page = Task {
            id: "page".into(),
            title: "app/page.tsx".into(),
            prompt: "Write the page.".into(),
            deps: vec!["types".into()],
            ..Task::default()
        };
        let text = files["app/page.tsx"];
        let engine = slimtoken::repair::missing_module_task(
            &page,
            text,
            "./Widget",
            "app/Widget.tsx",
            &files,
            &NEXTJS_REPAIR,
            CONTEXT_CHARS,
        );
        let bench = missing_module_task(&page, text, "./Widget", "app/Widget.tsx", &files);
        assert_eq!(
            (engine.id, engine.title, engine.prompt, engine.deps),
            (bench.id, bench.title, bench.prompt, bench.deps)
        );
        assert_eq!(slimtoken::repair::STUCK_TEST_RULE, STUCK_TEST_RULE);
        assert_eq!(slimtoken::repair::TRACE_RULE, TRACE_RULE);
        assert_eq!(slimtoken::repair::EDIT_RULE, EDIT_FORMAT);
        let policy = RepairPolicy::default();
        assert_eq!(policy.joint_max_tokens, JOINT_MAX_TOKENS);
        assert_eq!(policy.context_chars, CONTEXT_CHARS);
        assert_eq!(policy.edit_min_lines, EDIT_MIN_LINES);
        assert_eq!(policy.edit_max_problem_lines, EDIT_MAX_PROBLEM_LINES);
        assert_eq!(policy.stale_limit, 3);
        assert_eq!(policy.escalate_temperature, 0.8);
    }

    #[test]
    fn bench_switches_map_to_the_engine_policy() {
        let all = engine_policy(8, |_| true);
        assert_eq!(all.max_rounds, 8);
        assert!(all.stuck_tests_alone);
        assert_eq!(all.escalate_effort.as_deref(), Some("medium"));
        assert!(!all.check_key.is_empty(), "keepdir");
        assert!(all.escalate_tests);
        assert!(!engine_policy(8, |name| name != "thinktests").escalate_tests);
        let none = engine_policy(2, |name| !matches!(name, "testonly" | "think" | "keepdir"));
        assert_eq!(none.max_rounds, 2);
        assert!(!none.stuck_tests_alone);
        assert_eq!(none.escalate_effort, None);
        assert!(none.check_key.is_empty());
        assert!(all.guard);
        assert!(!engine_policy(2, |name| name != "guard").guard);
    }
}
