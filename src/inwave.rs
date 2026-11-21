//! Repair during the wave's tail (`inwave`).
//!
//! The wave ends with its largest file. The files that landed before it are
//! often done seconds earlier, and the after-wave repair round only starts
//! when the last one closes. [`execute_with_in_wave_repair`] runs the wave
//! and, beside it, checks the landed files whose imports have all landed
//! (the import closure: every project file that a file imports, again and
//! again, is planned and has landed). A file of that set with problems gets
//! the repair request that the first round of the repair loop would send for
//! it ([`file_repair`], the same prompt), while the rest of the wave is
//! still streaming. Its answer is put in place once the wave ends, and the
//! repair loop ([`check_and_repair_seeded`](crate::repair::check_and_repair_seeded))
//! still checks and repairs the whole app after the wave: quality is gated
//! as before, the loop usually finds less left to do, and it takes the
//! in-wave repairs as its round before ([`RepairSeed`]).
//!
//! The in-wave repair only takes the plain case of the loop's first round:
//! a file whose problems call for its own repair. A file whose problems
//! would make the loop repair it jointly with others (a render failure,
//! problems that name another file as the place to change, a missing
//! module, a warning about a second copy of a type), or that the guards of
//! the loop would fix or hold ([`RepairPolicy::guard`]), waits for the loop.
//! With the guards on, a file is also only repaired once every task that
//! depends on it has landed, so that its export lock lists every user.
//!
//! The checker of the landed files should report only problems that depend
//! on a file and the files it imports (a type check, the tests): a rule
//! over the whole app would see files that are not there yet. A test file
//! whose check ran no test (`CheckResult::tests` is 0) and named no problem
//! is checked again after the next landing (a check runs only when a file
//! landed or a repair answered since the last one).
//!
//! A repaired file replaces the landed one for every later check, and a file
//! that imports a file whose repair runs (or is imported by one) waits for a
//! check of the repaired text, so two files are never repaired against each
//! other's old text. The repairs share the wave's admission budget
//! (`SchedulerConfig::shared_limits`). The loop gets the wave's own text of
//! each repaired file ([`RepairSeed::unrepaired`]) and keeps it when it
//! checks nearer to a pass than the repaired one.

use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Instant;

use anyhow::Result;
use futures::stream::FuturesUnordered;
use futures::StreamExt;
use serde::{Deserialize, Serialize};

use crate::check::{CheckMode, CheckResult, Checker};
use crate::client::ModelClient;
use crate::control::RunControl;
use crate::dag::{Plan, Task};
use crate::guard::{lock_block, only_knock_on, root_first, CodeIndex, Lock};
use crate::profile::{Problem, RepairHooks};
use crate::project::{apply_edit_blocks, report_files};
use crate::repair::{file_repair, imports, log_answer, named_files, RepairPolicy, RepairSeed};
use crate::scheduler::{
    execute_plan, execute_plan_with_control, PhaseBreakdown, RunReport, SchedulerConfig,
    SharedLimits, TaskRecord, TaskStatus,
};

/// Called with a file's path and its repaired output when an in-wave
/// repair answers (a host that feeds a checker of its own).
pub type RepairedHook = Arc<dyn Fn(&str, &str) + Send + Sync>;

/// How a build repairs during the wave (`BuildOptions::in_wave`).
#[derive(Clone)]
pub struct InWaveOptions<'a> {
    /// The checker of the landed files; `None`: `BuildHooks::in_wave_checker`,
    /// else the build's checker when it checks landed files
    /// (`Checker::checks_landed_files`), else no in-wave repair.
    pub checker: Option<&'a dyn Checker>,
    /// The mode of the in-wave checks.
    pub mode: CheckMode,
    /// The client of the in-wave repairs; `None`: the wave's client.
    pub client: Option<Arc<dyn ModelClient>>,
    /// Told each repaired file as its repair answers.
    pub on_repaired: Option<RepairedHook>,
}

impl Default for InWaveOptions<'_> {
    fn default() -> Self {
        Self {
            checker: None,
            mode: CheckMode::Fast,
            client: None,
            on_repaired: None,
        }
    }
}

/// What the in-wave repair did, with its timings (milliseconds from the
/// wave's start unless named otherwise).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct InWaveReport {
    /// Repair requests sent during the wave.
    pub in_wave_repairs: usize,
    /// Repaired files put in place.
    pub applied: usize,
    /// The files that a request was sent for.
    pub files: Vec<String>,
    /// Files with problems left to the repair loop (see the module docs).
    pub skipped: usize,
    /// Checks of the landed files, and their time in all.
    pub checks: usize,
    pub check_ms: u64,
    /// When the first repair request was sent.
    pub first_repair_ms: Option<u64>,
    /// From the first repair request to the last answer.
    pub in_wave_ms: u64,
    pub wave_ms: u64,
    /// After the wave: the wait for repairs that still ran.
    pub tail_wait_ms: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
}

/// The wave's report (with the repaired files in place), what the in-wave
/// repair did, and the seed for the repair loop.
#[derive(Debug)]
pub struct InWaveOutcome {
    pub report: RunReport,
    pub in_wave: InWaveReport,
    pub seed: RepairSeed,
}

/// The parts of an in-wave repair.
pub struct InWave<'a> {
    pub checker: &'a dyn Checker,
    pub mode: CheckMode,
    pub policy: &'a RepairPolicy,
    pub hooks: &'a RepairHooks,
    /// The guards' index (`None`: the sources), as for the repair loop.
    pub index: Option<&'a dyn CodeIndex>,
    pub client: Arc<dyn ModelClient>,
    pub on_repaired: Option<RepairedHook>,
}

/// One repair request of the wave.
struct Sent {
    id: String,
    title: String,
    edits: bool,
    /// The output that the repair saw (its edits apply to this text).
    base: String,
}

/// One answered repair.
struct Answered {
    id: String,
    title: String,
    /// The repaired output; `None`: no answer, or edits that did not apply.
    output: Option<String>,
    edits_failed: bool,
    record: Option<TaskRecord>,
}

type CheckFuture<'a> = Pin<Box<dyn Future<Output = Result<CheckResult>> + Send + 'a>>;
type RepairFuture = Pin<Box<dyn Future<Output = Answered> + Send>>;

/// The landed files and which of them are ready for a check.
struct Tracker<'p> {
    plan: &'p Plan,
    planned: BTreeSet<&'p str>,
    /// Title to the titles of the tasks that depend on it (transitively).
    dependents: BTreeMap<&'p str, BTreeSet<&'p str>>,
    /// Landed records, by task id.
    landed: BTreeMap<String, TaskRecord>,
    /// Titles with a decision: a repair was sent, the file passed, or it
    /// waits for the repair loop.
    decided: BTreeSet<String>,
}

impl<'p> Tracker<'p> {
    fn new(plan: &'p Plan) -> Self {
        let planned = plan.tasks.iter().map(|task| task.title.as_str()).collect();
        let by_id: BTreeMap<&str, &Task> = plan
            .tasks
            .iter()
            .map(|task| (task.id.as_str(), task))
            .collect();
        // Direct dependents, then their closure.
        let mut direct: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
        for task in &plan.tasks {
            for dep in &task.deps {
                if let Some(owner) = by_id.get(dep.as_str()) {
                    direct
                        .entry(owner.title.as_str())
                        .or_default()
                        .insert(task.title.as_str());
                }
            }
        }
        let mut dependents = BTreeMap::new();
        for task in &plan.tasks {
            let mut seen: BTreeSet<&str> = BTreeSet::new();
            let mut queue: Vec<&str> = vec![task.title.as_str()];
            while let Some(next) = queue.pop() {
                for child in direct.get(next).into_iter().flatten() {
                    if seen.insert(child) {
                        queue.push(child);
                    }
                }
            }
            dependents.insert(task.title.as_str(), seen);
        }
        Self {
            plan,
            planned,
            dependents,
            landed: BTreeMap::new(),
            decided: BTreeSet::new(),
        }
    }

    fn land(&mut self, record: TaskRecord) {
        if record.status == TaskStatus::Succeeded && record.output.is_some() {
            self.landed.insert(record.id.clone(), record);
        }
    }

    /// Put the repaired output of `answer` in place of the landed one, so
    /// later checks see it; false when it has none (or the file never
    /// landed).
    fn repaired(&mut self, answer: &Answered) -> bool {
        let (Some(output), Some(record)) = (&answer.output, self.landed.get_mut(&answer.id)) else {
            return false;
        };
        record.output = Some(output.clone());
        true
    }

    fn landed_titles(&self) -> BTreeSet<&str> {
        self.landed
            .values()
            .map(|record| record.title.as_str())
            .collect()
    }

    /// The landed files that a repair may change, with no decision yet,
    /// whose import closure has landed (with `guard`: and every task that
    /// depends on them).
    fn ready(&self, hooks: &RepairHooks, guard: bool) -> BTreeSet<String> {
        let outputs: BTreeMap<&str, &str> = self
            .landed
            .values()
            .filter_map(|record| Some((record.title.as_str(), record.output.as_deref()?)))
            .collect();
        let landed = self.landed_titles();
        let mut memo: BTreeMap<String, bool> = BTreeMap::new();
        self.plan
            .tasks
            .iter()
            .filter(|task| task.static_output.is_none() && !task.internal)
            .filter(|task| landed.contains(task.title.as_str()))
            .filter(|task| !self.decided.contains(&task.title))
            .filter(|task| {
                !guard
                    || self
                        .dependents
                        .get(task.title.as_str())
                        .is_none_or(|users| users.iter().all(|user| landed.contains(user)))
            })
            .filter(|task| {
                closed(
                    &task.title,
                    &outputs,
                    &self.planned,
                    hooks,
                    &mut memo,
                    &mut BTreeSet::new(),
                )
            })
            .map(|task| task.title.clone())
            .collect()
    }

    /// The landed records as a report, with the hooks' fixes that need no
    /// model call (`prepare`) applied, as the repair loop sees them.
    fn prepared(&self, hooks: &RepairHooks) -> RunReport {
        let mut report = partial_report(&self.plan.objective, &self.landed);
        (hooks.prepare)(self.plan, &mut report);
        report
    }
}

/// True if `title` has landed and every project file it imports, again and
/// again, is planned and has landed.
fn closed(
    title: &str,
    outputs: &BTreeMap<&str, &str>,
    planned: &BTreeSet<&str>,
    hooks: &RepairHooks,
    memo: &mut BTreeMap<String, bool>,
    visiting: &mut BTreeSet<String>,
) -> bool {
    if let Some(known) = memo.get(title) {
        return *known;
    }
    let Some(text) = outputs.get(title) else {
        return false;
    };
    if !visiting.insert(title.to_string()) {
        // An import cycle: decided where it started.
        return true;
    }
    let mut ok = true;
    for spec in (hooks.local_specifiers)(text) {
        let resolved = (hooks.resolve_import)(title, &spec, planned);
        if !resolved.is_some_and(|file| closed(&file, outputs, planned, hooks, memo, visiting)) {
            ok = false;
            break;
        }
    }
    visiting.remove(title);
    memo.insert(title.to_string(), ok);
    ok
}

/// Each file of `report` (its succeeded records) to the project files it
/// imports.
fn import_graph(report: &RunReport, hooks: &RepairHooks) -> BTreeMap<String, BTreeSet<String>> {
    let outputs: BTreeMap<&str, &str> = report
        .tasks
        .values()
        .filter(|record| record.status == TaskStatus::Succeeded)
        .filter_map(|record| Some((record.title.as_str(), record.output.as_deref()?)))
        .collect();
    outputs
        .iter()
        .map(|(title, text)| {
            let found = imports(title, text, &outputs, hooks).0;
            (title.to_string(), found.into_iter().collect())
        })
        .collect()
}

/// True if `title` imports a file of `changing`, or a file of `changing`
/// imports it.
fn near_a_change(
    title: &str,
    graph: &BTreeMap<String, BTreeSet<String>>,
    changing: &BTreeSet<&str>,
) -> bool {
    let imported = graph
        .get(title)
        .is_some_and(|files| files.iter().any(|file| changing.contains(file.as_str())));
    imported
        || changing.iter().any(|other| {
            *other != title && graph.get(*other).is_some_and(|files| files.contains(title))
        })
}

/// A report that holds `tasks` and nothing else.
fn partial_report(objective: &str, tasks: &BTreeMap<String, TaskRecord>) -> RunReport {
    RunReport {
        objective: objective.to_string(),
        steering: Vec::new(),
        elapsed_ms: 0,
        phases: PhaseBreakdown::default(),
        task_count: tasks.len(),
        peak_concurrency: 0,
        total_input_tokens: 0,
        total_output_tokens: 0,
        total_reasoning_tokens: None,
        total_cached_input_tokens: None,
        total_wasted_input_tokens: 0,
        total_wasted_output_tokens: 0,
        mean_time_to_first_token_ms: None,
        aggregate_output_tokens_per_second: 0.0,
        aggregate_total_tokens_per_second: 0.0,
        concurrency_curve: Vec::new(),
        controller_stats: None,
        rate_tune: None,
        usage_is_estimated: false,
        tasks: tasks.clone(),
    }
}

/// What to do with one ready file after a check.
#[derive(Debug)]
enum Decision {
    /// Send this repair (edit blocks or not).
    Repair(Task, bool),
    /// No problems: done.
    Clean,
    /// Problems for the repair loop.
    Skip,
    /// No decision yet (a test that ran no test): check again later.
    Later,
}

/// The decision for the ready file `task` after the check `result` of the
/// prepared files `report` (`roots`: what the root-first guard would touch,
/// with the guards on).
#[allow(clippy::too_many_arguments)]
async fn decide(
    task: &Task,
    result: &CheckResult,
    report: &RunReport,
    landed: &BTreeSet<&str>,
    planned: &BTreeSet<&str>,
    roots: Option<&RootTouch<'_>>,
    in_wave: &InWave<'_>,
) -> Decision {
    let (policy, hooks) = (in_wave.policy, in_wave.hooks);
    let title = task.title.as_str();
    let problems = result.problems.get(title).cloned().unwrap_or_default();
    if problems.trim().is_empty() {
        if (hooks.is_test)(title) && result.tests == 0 {
            return Decision::Later;
        }
        // A rule of the checker reads a file that has not landed yet.
        if result.undecided.contains(title) {
            return Decision::Later;
        }
        return Decision::Clean;
    }
    let outputs: BTreeMap<&str, &str> = report
        .tasks
        .values()
        .filter(|record| record.status == TaskStatus::Succeeded)
        .filter_map(|record| Some((record.title.as_str(), record.output.as_deref()?)))
        .collect();
    let Some(text) = outputs.get(title).copied() else {
        return Decision::Skip;
    };
    // The loop's other paths: a render failure (stuck at once), a missing
    // module, problems that name another file (a joint repair).
    let lines = problems.lines();
    let mut kinds = lines.map(|line| (hooks.classify)(line));
    if kinds.any(|kind| kind == Problem::Render || kind == Problem::MissingModule) {
        return Decision::Skip;
    }
    if !(hooks.is_test)(title) && !named_files(title, &problems, &outputs, hooks.is_test).is_empty()
    {
        return Decision::Skip;
    }
    // A file of the plan that has not landed yet: the problem is not about
    // this file and what it imports alone.
    if planned
        .iter()
        .any(|file| *file != title && !landed.contains(file) && problems.contains(file))
    {
        return Decision::Skip;
    }
    // The warning about a second copy of a type that the loop would add.
    let module = title.rsplit_once('.').map_or(title, |(stem, _)| stem);
    let conflict = format!("import(\"{module}\")");
    let in_conflict = result
        .problems
        .values()
        .any(|text| text.contains(&conflict));
    if in_conflict && result.hints.contains_key(title) {
        return Decision::Skip;
    }
    let (_, missing) = imports(title, text, &outputs, hooks);
    if !missing.is_empty() {
        return Decision::Skip;
    }
    if let Some(roots) = roots {
        let imported = imports(title, text, &outputs, hooks).0;
        if roots.touched.contains(title)
            || imported.iter().any(|file| roots.fixed.contains(file))
            || only_knock_on(&problems, hooks)
        {
            return Decision::Skip;
        }
    }
    let (mut repair, edits) =
        file_repair(task, &problems, text, &outputs, &[], false, policy, hooks);
    if let Some(roots) = roots {
        let users = roots.index.users(title, &roots.texts, hooks).await;
        let source = roots.texts.get(title).map_or("", String::as_str);
        if let Some(lock) = Lock::new(title, source, users, hooks) {
            repair.prompt.push_str(&lock_block(&lock));
        }
    }
    Decision::Repair(repair, edits)
}

/// What the root-first guard would do with the problems of one check.
struct RootTouch<'a> {
    /// Files that it fixes, holds or repairs.
    touched: BTreeSet<String>,
    /// Files that it fixes.
    fixed: BTreeSet<String>,
    texts: BTreeMap<String, String>,
    index: &'a dyn CodeIndex,
}

fn root_touch<'a>(
    result: &CheckResult,
    report: &RunReport,
    plan: &Plan,
    hooks: &RepairHooks,
    index: &'a dyn CodeIndex,
) -> RootTouch<'a> {
    let texts = report_files(report);
    let repairable: BTreeSet<String> = plan
        .tasks
        .iter()
        .filter(|task| task.static_output.is_none() && !task.internal)
        .map(|task| task.title.clone())
        .collect();
    let root = root_first(
        &result.problems,
        &texts,
        hooks,
        &BTreeSet::new(),
        &repairable,
    );
    let fixed: BTreeSet<String> = root.fixes.keys().cloned().collect();
    let touched = root
        .fixes
        .keys()
        .chain(root.fixed.keys())
        .chain(root.held.keys())
        .chain(root.repairs.keys())
        .cloned()
        .collect();
    RootTouch {
        touched,
        fixed,
        texts,
        index,
    }
}

/// Send `sent`'s repair request: one task, run on its own.
fn send_repair(
    sent: Sent,
    task: Task,
    objective: String,
    client: Arc<dyn ModelClient>,
    config: SchedulerConfig,
) -> RepairFuture {
    Box::pin(async move {
        let plan = Plan {
            objective,
            tasks: vec![task],
            cache_primes: Vec::new(),
        };
        let run = execute_plan(&plan, client, config).await;
        let record = match run {
            Ok(mut report) => report.tasks.remove(&sent.id),
            Err(error) => {
                tracing::warn!(%error, file = %sent.title, "in-wave repair failed");
                None
            }
        };
        let answer = record
            .as_ref()
            .filter(|record| record.status == TaskStatus::Succeeded)
            .and_then(|record| record.output.clone());
        let (output, edits_failed) = match answer {
            Some(blocks) if sent.edits => match apply_edit_blocks(&sent.base, &blocks) {
                Some(text) => (Some(text), false),
                None => (None, true),
            },
            other => (other, false),
        };
        Answered {
            id: sent.id,
            title: sent.title,
            output,
            edits_failed,
            record,
        }
    })
}

/// Run the wave of `plan` (as `execute_plan_with_control`) and repair
/// landed files beside it (see the module docs). The repairs run with
/// `config` without its journal and trace (they must not open the wave's
/// files again) and with a control of their own.
pub async fn execute_with_in_wave_repair(
    plan: &Plan,
    client: Arc<dyn ModelClient>,
    config: SchedulerConfig,
    control: RunControl,
    in_wave: &InWave<'_>,
) -> Result<InWaveOutcome> {
    let started = Instant::now();
    let since = || started.elapsed().as_millis() as u64;
    // One admission budget for the wave and its repairs: the repairs run
    // while the wave streams, so they share its in-flight ceiling and its
    // start and token rates instead of each building budgets of their own.
    let mut config = config;
    if config.shared_limits.is_none() {
        config.shared_limits = Some(SharedLimits::new(
            config.max_concurrency,
            config.max_starts_per_minute,
            config.max_tokens_per_minute,
        ));
    }
    let repair_config = SchedulerConfig {
        journal_path: None,
        trace_path: None,
        trace_append: false,
        planning: None,
        ..config.clone()
    };
    let key = in_wave.policy.check_key.as_str();
    let guard = in_wave.policy.guard;
    let text_index = crate::guard::TextIndex;
    let index: &dyn CodeIndex = in_wave.index.unwrap_or(&text_index);
    let mut landings = control.subscribe_landings();
    let wave = execute_plan_with_control(plan, client, config, control.clone());
    tokio::pin!(wave);

    let mut tracker = Tracker::new(plan);
    let by_title: BTreeMap<&str, &Task> = plan
        .tasks
        .iter()
        .map(|task| (task.title.as_str(), task))
        .collect();
    let mut summary = InWaveReport::default();
    // The running check: its future, its prepared files, its ready files,
    // when it started and the generation of the files it saw.
    let mut check: Option<(CheckFuture<'_>, RunReport, BTreeSet<String>, Instant, u64)> = None;
    // Bumped by each landing and each repaired file put in the tracker: a
    // check runs only for a generation that no check has seen yet (a file
    // that waits, `Decision::Later`, is checked again only after a change).
    let mut generation: u64 = 0;
    let mut checked: u64 = 0;
    // Titles whose repair runs, and the generation at which each repaired
    // file was put in the tracker.
    let mut pending: BTreeSet<String> = BTreeSet::new();
    let mut applied_at: BTreeMap<String, u64> = BTreeMap::new();
    let mut repairs: FuturesUnordered<RepairFuture> = FuturesUnordered::new();
    let mut answers: Vec<Answered> = Vec::new();
    let mut last_answer_ms = 0;
    let mut seed = RepairSeed::default();

    let wave_result = loop {
        if check.is_none() && generation != checked {
            checked = generation;
            let ready = tracker.ready(in_wave.hooks, guard);
            if !ready.is_empty() {
                let prepared = tracker.prepared(in_wave.hooks);
                let files = report_files(&prepared);
                let (checker, mode) = (in_wave.checker, in_wave.mode);
                let future: CheckFuture<'_> =
                    Box::pin(async move { checker.check(&files, mode, key).await });
                check = Some((future, prepared, ready, Instant::now(), generation));
            }
        }
        tokio::select! {
            biased;
            report = &mut wave => break report,
            Some(record) = landings.recv() => {
                tracker.land(record);
                generation += 1;
            }
            result = async { check.as_mut().expect("a check runs").0.as_mut().await }, if check.is_some() => {
                let (_, prepared, ready, check_started, seen) = check.take().expect("a check ran");
                summary.checks += 1;
                summary.check_ms += check_started.elapsed().as_millis() as u64;
                let result = match result {
                    Ok(result) => result,
                    Err(error) => {
                        tracing::warn!(%error, "in-wave check failed");
                        continue;
                    }
                };
                let landed = tracker.landed_titles();
                let roots = guard.then(|| root_touch(&result, &prepared, plan, in_wave.hooks, index));
                // Files whose repair runs, or whose repaired text this check
                // did not see: a file that imports one of them, or that one
                // of them imports, waits for a check of the new text.
                let changing: BTreeSet<&str> = pending
                    .iter()
                    .map(String::as_str)
                    .chain(
                        applied_at
                            .iter()
                            .filter(|(_, at)| **at > seen)
                            .map(|(title, _)| title.as_str()),
                    )
                    .collect();
                let graph = (!changing.is_empty()).then(|| import_graph(&prepared, in_wave.hooks));
                let mut decisions = Vec::new();
                for title in &ready {
                    let Some(task) = by_title.get(title.as_str()) else { continue };
                    if let Some(graph) = &graph {
                        if near_a_change(title, graph, &changing) {
                            decisions.push((*task, Decision::Later));
                            continue;
                        }
                    }
                    let decision = decide(
                        task,
                        &result,
                        &prepared,
                        &landed,
                        &tracker.planned,
                        roots.as_ref(),
                        in_wave,
                    )
                    .await;
                    decisions.push((*task, decision));
                }
                for (task, decision) in decisions {
                    match decision {
                        Decision::Later => continue,
                        Decision::Clean => {}
                        Decision::Skip => summary.skipped += 1,
                        Decision::Repair(repair, edits) => {
                            let base = prepared
                                .tasks
                                .get(&task.id)
                                .and_then(|record| record.output.clone())
                                .unwrap_or_default();
                            summary.in_wave_repairs += 1;
                            summary.files.push(task.title.clone());
                            summary.first_repair_ms.get_or_insert_with(since);
                            pending.insert(task.title.clone());
                            tracing::info!(
                                file = %task.title,
                                edits,
                                after_ms = since(),
                                "in-wave repair sent"
                            );
                            repairs.push(send_repair(
                                Sent { id: task.id.clone(), title: task.title.clone(), edits, base },
                                repair,
                                plan.objective.clone(),
                                in_wave.client.clone(),
                                repair_config.clone(),
                            ));
                        }
                    }
                    tracker.decided.insert(task.title.clone());
                }
                // Files that landed during the check start the next one
                // (their generation is newer than `seen`).
            }
            Some(answer) = repairs.next(), if !repairs.is_empty() => {
                last_answer_ms = since();
                pending.remove(&answer.title);
                // Later checks (and the host's checker) see the repaired
                // text, and the files that waited for it are checked again.
                generation += 1;
                if tracker.repaired(&answer) {
                    applied_at.insert(answer.title.clone(), generation);
                }
                if let (Some(hook), Some(output)) = (&in_wave.on_repaired, &answer.output) {
                    hook(&answer.title, output);
                }
                answers.push(answer);
            }
        }
    };
    control.unsubscribe_landings();
    drop(check);
    let mut report = wave_result?;
    summary.wave_ms = since();
    // The repairs that still run: they started before the loop could.
    let tail = Instant::now();
    if !repairs.is_empty() {
        loop {
            tokio::select! {
                _ = control.cancelled() => break,
                answer = repairs.next() => {
                    let Some(answer) = answer else { break };
                    last_answer_ms = since();
                    if let (Some(hook), Some(output)) = (&in_wave.on_repaired, &answer.output) {
                        hook(&answer.title, output);
                    }
                    answers.push(answer);
                }
            }
        }
    }
    summary.tail_wait_ms = tail.elapsed().as_millis() as u64;
    if let Some(first) = summary.first_repair_ms {
        summary.in_wave_ms = last_answer_ms.saturating_sub(first);
    }
    for answer in answers {
        if let Some(fixed) = &answer.record {
            log_answer(
                in_wave.policy,
                &format!(
                    "==== in-wave {} status {:?}\n{}\n",
                    answer.id,
                    fixed.status,
                    fixed.output.as_deref().unwrap_or("(no output)")
                ),
            );
            report.total_input_tokens += fixed.input_tokens;
            report.total_output_tokens += fixed.output_tokens;
            summary.input_tokens += fixed.input_tokens;
            summary.output_tokens += fixed.output_tokens;
        }
        // A file whose repair answered is the loop's round before: one whose
        // answer was put in place, or whose edit blocks could not be placed
        // (from now on written in full). A repair that failed or gave no
        // answer leaves the file as the wave wrote it, for the loop's plain
        // first-round repair.
        if answer.edits_failed {
            seed.full_only.insert(answer.title.clone());
            seed.repaired.insert(answer.title.clone());
        }
        let Some(record) = report.tasks.get_mut(&answer.id) else {
            continue;
        };
        if let Some(fixed) = &answer.record {
            record.attempts += fixed.attempts;
            record.input_tokens += fixed.input_tokens;
            record.output_tokens += fixed.output_tokens;
        }
        if let Some(output) = answer.output {
            if let Some(wave) = record.output.replace(output) {
                seed.unrepaired.insert(answer.id.clone(), wave);
            }
            seed.repaired.insert(answer.title.clone());
            summary.applied += 1;
        }
    }
    tracing::info!(
        in_wave_repairs = summary.in_wave_repairs,
        applied = summary.applied,
        skipped = summary.skipped,
        checks = summary.checks,
        check_ms = summary.check_ms,
        first_repair_ms = ?summary.first_repair_ms,
        in_wave_ms = summary.in_wave_ms,
        wave_ms = summary.wave_ms,
        tail_wait_ms = summary.tail_wait_ms,
        "in-wave repair"
    );
    Ok(InWaveOutcome {
        report,
        in_wave: summary,
        seed,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::{Completion, CompletionRequest};
    use crate::profile::NO_REPAIR_HOOKS;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;
    use std::time::Duration;

    /// `import "x"` lines name project files.
    fn specifiers(source: &str) -> Vec<String> {
        source
            .lines()
            .filter_map(|line| line.trim().strip_prefix("import \""))
            .filter_map(|rest| rest.split('"').next())
            .map(str::to_string)
            .collect()
    }

    fn resolve(_: &str, spec: &str, known: &BTreeSet<&str>) -> Option<String> {
        known.contains(spec).then(|| spec.to_string())
    }

    fn is_test(path: &str) -> bool {
        path.starts_with("tests/")
    }

    const HOOKS: RepairHooks = RepairHooks {
        local_specifiers: specifiers,
        resolve_import: resolve,
        is_test,
        ..NO_REPAIR_HOOKS
    };

    fn task(id: &str, path: &str, deps: &[&str]) -> Task {
        Task {
            id: id.into(),
            title: path.into(),
            prompt: format!("Title: {path}\nWrite {path}."),
            deps: deps.iter().map(|dep| dep.to_string()).collect(),
            max_tokens: 64,
            temperature: None,
            require_closed_fences: false,
            validate_artifact: false,
            static_output: None,
            reasoning_effort: None,
            internal: false,
            prefix_split: None,
        }
    }

    /// Wave answers: `types` and `core` first, `page` (imports both, broken)
    /// next, `big` last and slow. Repairs (prompts with `# Check results`)
    /// answer `fixed`. Records the order of the events.
    struct Mock {
        events: Mutex<Vec<String>>,
        prompts: Mutex<Vec<String>>,
        big_ms: u64,
    }

    #[async_trait::async_trait]
    impl ModelClient for Mock {
        async fn complete(&self, request: CompletionRequest) -> Result<Completion> {
            let user = request.user.clone();
            let title = user
                .lines()
                .find_map(|line| line.strip_prefix("Title: "))
                .unwrap_or_default()
                .trim()
                .to_string();
            let repair = user.contains("# Check results");
            if repair {
                self.prompts.lock().unwrap().push(user.clone());
            }
            let (text, wait) = match (repair, title.as_str()) {
                (true, _) => ("```\nfixed\n```".to_string(), 5),
                (false, "big.ts") => ("```\nbig\n```".to_string(), self.big_ms),
                (false, "page.ts") => (
                    "```\nimport \"types.ts\"\nimport \"core.ts\"\nbroken\n```".to_string(),
                    5,
                ),
                (false, other) => (format!("```\n{other}\n```"), 1),
            };
            tokio::time::sleep(Duration::from_millis(wait)).await;
            self.events.lock().unwrap().push(format!(
                "{} {title}",
                if repair { "repaired" } else { "wrote" }
            ));
            Ok(Completion {
                text,
                provider: None,
                input_tokens: 3,
                output_tokens: 2,
                reasoning_tokens: None,
                cached_input_tokens: None,
                finish_reason: Some("stop".into()),
                usage_is_estimated: false,
                elapsed: Duration::from_millis(wait),
                time_to_first_token: None,
            })
        }
    }

    /// Fails each file whose text holds `broken`; counts the checks.
    struct Checks {
        count: AtomicUsize,
        seen: Mutex<Vec<BTreeSet<String>>>,
    }

    #[async_trait::async_trait]
    impl Checker for Checks {
        async fn check(
            &self,
            files: &BTreeMap<String, String>,
            _: CheckMode,
            _: &str,
        ) -> Result<CheckResult> {
            self.count.fetch_add(1, Ordering::SeqCst);
            self.seen
                .lock()
                .unwrap()
                .push(files.keys().cloned().collect());
            let problems: BTreeMap<String, String> = files
                .iter()
                .filter(|(_, text)| text.contains("broken"))
                .map(|(path, _)| (path.clone(), format!("ERROR {path}(3,1): broken\n")))
                .collect();
            Ok(CheckResult {
                passed: problems.is_empty(),
                failed: problems.len() as u64,
                summary: format!("{} failed", problems.len()),
                problems,
                ..CheckResult::default()
            })
        }
    }

    fn plan() -> Plan {
        Plan {
            objective: "app".into(),
            tasks: vec![
                task("types", "types.ts", &[]),
                task("core", "core.ts", &["types"]),
                task("page", "page.ts", &["types", "core"]),
                task("big", "big.ts", &["types", "core"]),
            ],
            cache_primes: Vec::new(),
        }
    }

    #[tokio::test]
    async fn a_landed_file_is_repaired_while_the_wave_still_streams() {
        let plan = plan();
        let client = Arc::new(Mock {
            events: Mutex::new(Vec::new()),
            prompts: Mutex::new(Vec::new()),
            big_ms: 400,
        });
        let checker = Checks {
            count: AtomicUsize::new(0),
            seen: Mutex::new(Vec::new()),
        };
        let policy = RepairPolicy::default();
        let in_wave = InWave {
            checker: &checker,
            mode: CheckMode::Fast,
            policy: &policy,
            hooks: &HOOKS,
            index: None,
            client: client.clone(),
            on_repaired: None,
        };
        let outcome = execute_with_in_wave_repair(
            &plan,
            client.clone(),
            SchedulerConfig::default(),
            RunControl::default(),
            &in_wave,
        )
        .await
        .unwrap();
        let events = client.events.lock().unwrap().clone();
        let repaired = events.iter().position(|e| e == "repaired page.ts").unwrap();
        let big = events.iter().position(|e| e == "wrote big.ts").unwrap();
        assert!(
            repaired < big,
            "the repair answered before the wave ended: {events:?}"
        );
        assert_eq!(outcome.in_wave.in_wave_repairs, 1);
        assert_eq!(outcome.in_wave.applied, 1);
        assert_eq!(outcome.in_wave.files, vec!["page.ts".to_string()]);
        assert_eq!(
            outcome.report.tasks["page"].output.as_deref(),
            Some("```\nfixed\n```")
        );
        assert!(outcome.seed.repaired.contains("page.ts"));
        assert_eq!(outcome.report.tasks["page"].attempts, 2);
        // The request is the repair loop's request for that file.
        let prepared = {
            let mut tracker = Tracker::new(&plan);
            for record in outcome.report.tasks.values() {
                let mut record = record.clone();
                if record.id == "page" {
                    record.output =
                        Some("```\nimport \"types.ts\"\nimport \"core.ts\"\nbroken\n```".into());
                }
                tracker.land(record);
            }
            tracker.prepared(&HOOKS)
        };
        let outputs: BTreeMap<&str, &str> = prepared
            .tasks
            .values()
            .filter(|record| record.title != "big.ts")
            .map(|record| (record.title.as_str(), record.output.as_deref().unwrap()))
            .collect();
        let (expected, _) = file_repair(
            &plan.tasks[2],
            "ERROR page.ts(3,1): broken\n",
            outputs["page.ts"],
            &outputs,
            &[],
            false,
            &policy,
            &HOOKS,
        );
        // (The scheduler wraps the task prompt in its assignment.)
        let sent = client.prompts.lock().unwrap().clone();
        assert_eq!(sent.len(), 1);
        assert!(sent[0].contains(&expected.prompt), "{sent:?}");
        // No check before the page's imports had landed.
        for seen in checker.seen.lock().unwrap().iter() {
            assert!(seen.contains("types.ts"));
        }
    }

    #[test]
    fn a_file_whose_imports_have_not_landed_waits() {
        // `page.ts` imports `core.ts`, which has not landed.
        let plan = plan();
        let mut tracker = Tracker::new(&plan);
        for (id, title, output) in [
            ("types", "types.ts", "types"),
            (
                "page",
                "page.ts",
                "import \"types.ts\"\nimport \"core.ts\"\nbroken",
            ),
            ("big", "big.ts", "import \"nowhere.ts\""),
        ] {
            tracker.land(TaskRecord {
                output: Some(output.to_string()),
                ..record(id, title)
            });
        }
        let ready = tracker.ready(&HOOKS, false);
        assert!(ready.contains("types.ts"));
        assert!(!ready.contains("page.ts"));
        // An import of no planned file never makes a file ready.
        assert!(!ready.contains("big.ts"));
        // With the guards on, a file waits for its dependents too.
        assert!(!tracker.ready(&HOOKS, true).contains("types.ts"));
        tracker.land(TaskRecord {
            output: Some("core".into()),
            ..record("core", "core.ts")
        });
        let ready = tracker.ready(&HOOKS, true);
        assert!(ready.contains("types.ts") && ready.contains("page.ts"));
    }

    #[tokio::test]
    async fn a_test_that_ran_no_test_is_checked_again() {
        let plan = Plan {
            objective: "app".into(),
            tasks: vec![task("t", "tests/a.test.ts", &[])],
            cache_primes: Vec::new(),
        };
        let checker = Checks {
            count: AtomicUsize::new(0),
            seen: Mutex::new(Vec::new()),
        };
        let policy = RepairPolicy::default();
        let client = Arc::new(Mock {
            events: Mutex::new(Vec::new()),
            prompts: Mutex::new(Vec::new()),
            big_ms: 0,
        });
        let in_wave = InWave {
            checker: &checker,
            mode: CheckMode::Fast,
            policy: &policy,
            hooks: &HOOKS,
            index: None,
            client,
            on_repaired: None,
        };
        let mut report = partial_report("app", &BTreeMap::new());
        report.tasks.insert(
            "t".into(),
            TaskRecord {
                output: Some("ok".into()),
                ..record("t", "tests/a.test.ts")
            },
        );
        let result = CheckResult::default();
        let decision = decide(
            &plan.tasks[0],
            &result,
            &report,
            &BTreeSet::new(),
            &BTreeSet::new(),
            None,
            &in_wave,
        )
        .await;
        assert!(matches!(decision, Decision::Later));
        let ran = CheckResult {
            tests: 2,
            ..CheckResult::default()
        };
        let decision = decide(
            &plan.tasks[0],
            &ran,
            &report,
            &BTreeSet::new(),
            &BTreeSet::new(),
            None,
            &in_wave,
        )
        .await;
        assert!(matches!(decision, Decision::Clean));
        // A file that the checker could not check in full yet waits too.
        let undecided = CheckResult {
            tests: 2,
            undecided: [plan.tasks[0].title.clone()].into_iter().collect(),
            ..CheckResult::default()
        };
        let decision = decide(
            &plan.tasks[0],
            &undecided,
            &report,
            &BTreeSet::new(),
            &BTreeSet::new(),
            None,
            &in_wave,
        )
        .await;
        assert!(matches!(decision, Decision::Later));
    }

    #[tokio::test]
    async fn problems_that_name_a_file_not_landed_wait_for_the_loop() {
        let plan = plan();
        let checker = Checks {
            count: AtomicUsize::new(0),
            seen: Mutex::new(Vec::new()),
        };
        let policy = RepairPolicy::default();
        let client = Arc::new(Mock {
            events: Mutex::new(Vec::new()),
            prompts: Mutex::new(Vec::new()),
            big_ms: 0,
        });
        let in_wave = InWave {
            checker: &checker,
            mode: CheckMode::Fast,
            policy: &policy,
            hooks: &HOOKS,
            index: None,
            client,
            on_repaired: None,
        };
        let mut report = partial_report("app", &BTreeMap::new());
        report.tasks.insert(
            "page".into(),
            TaskRecord {
                output: Some("page".into()),
                ..record("page", "page.ts")
            },
        );
        let result = CheckResult {
            problems: [(
                "page.ts".to_string(),
                "ERROR page.ts: see big.ts\n".to_string(),
            )]
            .into_iter()
            .collect(),
            ..CheckResult::default()
        };
        let landed: BTreeSet<&str> = ["page.ts"].into_iter().collect();
        let planned: BTreeSet<&str> = plan.tasks.iter().map(|t| t.title.as_str()).collect();
        let decision = decide(
            &plan.tasks[2],
            &result,
            &report,
            &landed,
            &planned,
            None,
            &in_wave,
        )
        .await;
        assert!(matches!(decision, Decision::Skip));
    }

    #[tokio::test]
    async fn the_loop_takes_an_in_wave_repair_as_its_round_before() {
        let prompts = |seed: RepairSeed| async move {
            let mut plan = Plan {
                objective: "app".into(),
                tasks: vec![task("t", "tests/a.test.ts", &[])],
                cache_primes: Vec::new(),
            };
            let mut tasks = BTreeMap::new();
            tasks.insert(
                "t".to_string(),
                TaskRecord {
                    output: Some(
                        "```
broken
```"
                        .into(),
                    ),
                    ..record("t", "tests/a.test.ts")
                },
            );
            let mut report = partial_report("app", &tasks);
            let client = Arc::new(Mock {
                events: Mutex::new(Vec::new()),
                prompts: Mutex::new(Vec::new()),
                big_ms: 0,
            });
            let checker = Checks {
                count: AtomicUsize::new(0),
                seen: Mutex::new(Vec::new()),
            };
            crate::repair::check_and_repair_seeded(
                &mut plan,
                &mut report,
                client.clone(),
                SchedulerConfig::default(),
                &checker,
                RepairPolicy::default(),
                &HOOKS,
                None,
                &seed,
            )
            .await
            .unwrap();
            let sent = client.prompts.lock().unwrap().clone();
            sent
        };
        let plain = prompts(RepairSeed::default()).await;
        assert!(!plain[0].contains(crate::repair::STUCK_TEST_RULE));
        let seeded = prompts(RepairSeed {
            repaired: ["tests/a.test.ts".to_string()].into_iter().collect(),
            ..RepairSeed::default()
        })
        .await;
        assert!(seeded[0].contains(crate::repair::STUCK_TEST_RULE));
    }

    /// Wave answers by title (`(text, ms)`); repairs answer `repair` after
    /// 5 ms, or fail with `None`.
    struct Timed {
        wave: BTreeMap<&'static str, (&'static str, u64)>,
        repair: Option<&'static str>,
    }

    #[async_trait::async_trait]
    impl ModelClient for Timed {
        async fn complete(&self, request: CompletionRequest) -> Result<Completion> {
            let title = request
                .user
                .lines()
                .find_map(|line| line.strip_prefix("Title: "))
                .unwrap_or_default()
                .trim()
                .to_string();
            let (text, wait) = if request.user.contains("# Check results") {
                tokio::time::sleep(Duration::from_millis(5)).await;
                match self.repair {
                    Some(text) => (text, 5),
                    None => anyhow::bail!("the repair failed"),
                }
            } else {
                self.wave
                    .get(title.as_str())
                    .copied()
                    .unwrap_or(("```\nx\n```", 1))
            };
            tokio::time::sleep(Duration::from_millis(wait)).await;
            Ok(Completion {
                text: text.to_string(),
                provider: None,
                input_tokens: 3,
                output_tokens: 2,
                reasoning_tokens: None,
                cached_input_tokens: None,
                finish_reason: Some("stop".into()),
                usage_is_estimated: false,
                elapsed: Duration::from_millis(wait),
                time_to_first_token: None,
            })
        }
    }

    /// Fails each file whose text holds `broken`; records the texts of
    /// each check; never runs a test.
    #[derive(Default)]
    struct Texts {
        checks: Mutex<Vec<BTreeMap<String, String>>>,
    }

    #[async_trait::async_trait]
    impl Checker for Texts {
        async fn check(
            &self,
            files: &BTreeMap<String, String>,
            _: CheckMode,
            _: &str,
        ) -> Result<CheckResult> {
            self.checks.lock().unwrap().push(files.clone());
            let problems: BTreeMap<String, String> = files
                .iter()
                .filter(|(_, text)| text.contains("broken"))
                .map(|(path, _)| (path.clone(), format!("ERROR {path}(3,1): broken\n")))
                .collect();
            Ok(CheckResult {
                passed: problems.is_empty(),
                failed: problems.len() as u64,
                problems,
                ..CheckResult::default()
            })
        }
    }

    async fn run_timed(plan: &Plan, client: Timed, checker: &Texts) -> InWaveOutcome {
        let client = Arc::new(client);
        let policy = RepairPolicy::default();
        let in_wave = InWave {
            checker,
            mode: CheckMode::Fast,
            policy: &policy,
            hooks: &HOOKS,
            index: None,
            client: client.clone(),
            on_repaired: None,
        };
        let config = SchedulerConfig {
            max_retries: 0,
            ..SchedulerConfig::default()
        };
        execute_with_in_wave_repair(plan, client, config, RunControl::default(), &in_wave)
            .await
            .unwrap()
    }

    const PAGE: &str = "```\nimport \"types.ts\"\nimport \"core.ts\"\nbroken\n```";

    #[tokio::test]
    async fn a_failed_repair_is_not_a_round_before() {
        let client = Timed {
            wave: [("page.ts", (PAGE, 5)), ("big.ts", ("```\nbig\n```", 300))]
                .into_iter()
                .collect(),
            repair: None,
        };
        let checker = Texts::default();
        let outcome = run_timed(&plan(), client, &checker).await;
        assert_eq!(outcome.in_wave.in_wave_repairs, 1);
        assert_eq!(outcome.in_wave.applied, 0);
        assert!(outcome.seed.repaired.is_empty(), "{:?}", outcome.seed);
        assert!(outcome.seed.unrepaired.is_empty());
        assert_eq!(outcome.report.tasks["page"].output.as_deref(), Some(PAGE));
    }

    #[tokio::test]
    async fn a_later_check_sees_the_repaired_text() {
        let mut plan = plan();
        plan.tasks.push(task("mid", "mid.ts", &[]));
        let client = Timed {
            wave: [
                ("page.ts", (PAGE, 5)),
                ("mid.ts", ("```\nmid\n```", 150)),
                ("big.ts", ("```\nbig\n```", 400)),
            ]
            .into_iter()
            .collect(),
            repair: Some("```\nimport \"types.ts\"\nimport \"core.ts\"\nfixed\n```"),
        };
        let checker = Texts::default();
        let outcome = run_timed(&plan, client, &checker).await;
        assert_eq!(outcome.in_wave.applied, 1);
        assert!(outcome.seed.repaired.contains("page.ts"));
        assert_eq!(
            outcome.seed.unrepaired.get("page").map(String::as_str),
            Some(PAGE)
        );
        let checks = checker.checks.lock().unwrap().clone();
        let last = checks.last().expect("a check ran");
        assert!(
            last.get("page.ts")
                .is_some_and(|text| text.contains("fixed")),
            "{checks:?}"
        );
    }

    #[tokio::test]
    async fn a_test_that_ran_no_test_is_not_checked_in_a_busy_loop() {
        let plan = Plan {
            objective: "app".into(),
            tasks: vec![
                task("t", "tests/a.test.ts", &[]),
                task("big", "big.ts", &[]),
            ],
            cache_primes: Vec::new(),
        };
        let client = Timed {
            wave: [("big.ts", ("```\nbig\n```", 300))].into_iter().collect(),
            repair: None,
        };
        let checker = Texts::default();
        let outcome = run_timed(&plan, client, &checker).await;
        // At most one check for each landing.
        assert!(outcome.in_wave.checks <= 2, "{:?}", outcome.in_wave);
    }

    #[test]
    fn a_file_near_a_running_repair_waits() {
        // `x.ts` imports `d.ts`; `y.ts` imports nothing.
        let mut tasks = BTreeMap::new();
        for (id, title, output) in [
            ("d", "d.ts", "broken"),
            ("x", "x.ts", "import \"d.ts\"\nbroken"),
            ("y", "y.ts", "y"),
        ] {
            tasks.insert(
                id.to_string(),
                TaskRecord {
                    output: Some(output.to_string()),
                    ..record(id, title)
                },
            );
        }
        let graph = import_graph(&partial_report("app", &tasks), &HOOKS);
        // `x.ts` is repaired: `d.ts`, which it imports, waits.
        let changing: BTreeSet<&str> = ["x.ts"].into_iter().collect();
        assert!(near_a_change("d.ts", &graph, &changing));
        assert!(!near_a_change("y.ts", &graph, &changing));
        // `d.ts` is repaired: `x.ts`, which imports it, waits.
        let changing: BTreeSet<&str> = ["d.ts"].into_iter().collect();
        assert!(near_a_change("x.ts", &graph, &changing));
        assert!(!near_a_change("y.ts", &graph, &changing));
    }

    #[tokio::test]
    async fn the_loop_keeps_the_wave_text_when_the_repair_made_it_worse() {
        let mut plan = Plan {
            objective: "app".into(),
            tasks: vec![task("a", "a.ts", &[]), task("b", "b.ts", &[])],
            cache_primes: Vec::new(),
        };
        let mut tasks = BTreeMap::new();
        for (id, title, output) in [
            ("a", "a.ts", "```\na\n```"),
            ("b", "b.ts", "```\nbroken\n```"),
        ] {
            tasks.insert(
                id.to_string(),
                TaskRecord {
                    output: Some(output.to_string()),
                    ..record(id, title)
                },
            );
        }
        let mut report = partial_report("app", &tasks);
        let seed = RepairSeed {
            repaired: ["b.ts".to_string()].into_iter().collect(),
            unrepaired: [("b".to_string(), "```\nb\n```".to_string())]
                .into_iter()
                .collect(),
            ..RepairSeed::default()
        };
        let client = Arc::new(Timed {
            wave: BTreeMap::new(),
            repair: None,
        });
        let checker = Texts::default();
        let summary = crate::repair::check_and_repair_seeded(
            &mut plan,
            &mut report,
            client,
            SchedulerConfig {
                max_retries: 0,
                ..SchedulerConfig::default()
            },
            &checker,
            RepairPolicy::default(),
            &HOOKS,
            None,
            &seed,
        )
        .await
        .unwrap();
        assert!(summary.after.passed);
        assert_eq!(report.tasks["b"].output.as_deref(), Some("```\nb\n```"));
    }

    fn record(id: &str, title: &str) -> TaskRecord {
        TaskRecord {
            id: id.into(),
            title: title.into(),
            status: TaskStatus::Succeeded,
            output: None,
            error: None,
            attempts: 1,
            duration_ms: 0,
            input_tokens: 0,
            output_tokens: 0,
            reasoning_tokens: None,
            cached_input_tokens: None,
            provider: None,
            rate_limited: false,
            finish_reason: None,
            usage_is_estimated: false,
            time_to_first_token_ms: None,
            failure_class: None,
            wasted_input_tokens: 0,
            wasted_output_tokens: 0,
        }
    }
}
