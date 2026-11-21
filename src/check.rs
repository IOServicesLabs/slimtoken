//! Check a project's files with a type checker, tests or a build.
//!
//! A [`Checker`] reads the files of a run and says which files have
//! problems. The shipped [`CommandChecker`] pipes the files as one markdown
//! document ([`project::render_doc`](crate::project::render_doc)) to a
//! shell command and reads the command's output with [`parse_check`]:
//!
//! - `ERROR <path>(line,col): <message>` names a problem of `<path>`; a
//!   line in the same shape with no prefix (`<path>(line,col): ...`, as
//!   `tsc` prints it) does too. The path must look like a file (no
//!   whitespace), so an `ERROR gate: ...` line whose prose quotes a
//!   location names no file;
//! - `WARN <path>(line,col): <message>` is a hint, not a failure;
//! - `FAILED <test file> > <name>` starts a failed test;
//! - lines that start with a space continue the entry above;
//! - the last line `N failed, M passed (K type errors)` is the summary,
//!   with ` [deep]` at its end when the real build ran. No summary line
//!   means the command did not work: never a pass;
//! - `PATCH <path>` followed by a fenced block is a corrected file that
//!   the checker proposes; [`apply_patches`] puts it in a report without a
//!   model call.
//!
//! The command sees the mode in `SLIMTOKEN_CHECK_MODE` (`fast`, `deep`,
//! `clean`, or empty for a full check) and a directory key in
//! `SLIMTOKEN_CHECK_DIR`. The same values are set as `SHOWDOWN_GATE_MODE`
//! and `SHOWDOWN_GATE_DIR` for the `bench/showdown` gate.
//!
//! Ported from the `bench/showdown` repair step (`repair::parse_gate`).

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::AtomicU32;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::project::{fence_of, fenced, looks_like_path, render_doc, safe_path};
use crate::scheduler::{run_shell_piped_with, RunReport, TaskStatus};

/// Most characters of a checker's output kept for one file.
pub const EXCERPT_CHARS: usize = 4_000;

/// The environment variable that holds the [`CheckMode`].
pub const MODE_ENV: &str = "SLIMTOKEN_CHECK_MODE";
/// The environment variable that holds the directory key.
pub const DIR_ENV: &str = "SLIMTOKEN_CHECK_DIR";
/// Alias of [`MODE_ENV`] for the `bench/showdown` gate.
pub const GATE_MODE_ENV: &str = "SHOWDOWN_GATE_MODE";
/// Alias of [`DIR_ENV`] for the `bench/showdown` gate.
pub const GATE_DIR_ENV: &str = "SHOWDOWN_GATE_DIR";

/// What a check runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckMode {
    /// The fast checks and then, if they pass, the deep ones.
    #[default]
    Full,
    /// Only the fast checks (type check, tests).
    Fast,
    /// Only the deep checks (the real build).
    Deep,
    /// No check: remove what the checker keeps under the key.
    Clean,
}

impl CheckMode {
    /// The value of [`MODE_ENV`] for this mode.
    pub fn as_env(self) -> &'static str {
        match self {
            CheckMode::Full => "",
            CheckMode::Fast => "fast",
            CheckMode::Deep => "deep",
            CheckMode::Clean => "clean",
        }
    }
}

/// What a checker said about one state of the files.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckResult {
    pub passed: bool,
    /// Type errors plus failed tests.
    pub failed: u64,
    pub tests_passed: u64,
    /// Tests that ran (passed or failed).
    pub tests: u64,
    pub summary: String,
    /// True if the deep checks ran (the summary ends with `[deep]`).
    pub deep: bool,
    /// The checker's lines for each file that it names.
    pub problems: BTreeMap<String, String>,
    /// The checker's warnings for each file: not failures, but worth a
    /// repair while other problems are open.
    #[serde(skip)]
    pub hints: BTreeMap<String, String>,
    /// Corrected files that the checker proposes (`PATCH path` blocks).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub patches: BTreeMap<String, String>,
    /// Files that a checker of the landed files (`Checker::checks_landed_files`)
    /// could not check in full yet: a rule of theirs reads a file that has not
    /// landed. With no problem named, the in-wave repair checks such a file
    /// again after the next landing instead of taking it as clean. Empty
    /// (the default) for every other checker.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub undecided: BTreeSet<String>,
}

impl CheckResult {
    /// True if `self` is nearer to a pass than `old`: never with fewer
    /// tests; a result of the deep checks is nearer than one that the fast
    /// checks stopped; else fewer failures.
    pub fn nearer_than(&self, old: &CheckResult) -> bool {
        if self.tests < old.tests {
            return false;
        }
        match (self.deep, old.deep) {
            (true, false) => true,
            (false, true) => false,
            _ => self.failed < old.failed,
        }
    }
}

/// Checks the files of a project.
#[async_trait]
pub trait Checker: Send + Sync {
    /// Check `files` (path to text) in `mode`. `key` names state that the
    /// checker may keep between checks of one run (a directory whose
    /// caches make a later check faster); empty means start cold.
    /// [`CheckMode::Clean`] removes that state.
    async fn check(
        &self,
        files: &BTreeMap<String, String>,
        mode: CheckMode,
        key: &str,
    ) -> Result<CheckResult>;

    /// The result of the checker's exact tools for `files`, given `result`,
    /// the answer that a [`CheckMode::Full`] check of `files` gave. A
    /// checker that may answer a full check early from a faster source that
    /// can be wrong (to start the next repair sooner) gives the exact
    /// result here; the repair loop calls it on a result after which no
    /// round follows and on the results it reports. The default returns
    /// `result`.
    async fn confirm(
        &self,
        files: &BTreeMap<String, String>,
        result: CheckResult,
        key: &str,
    ) -> Result<CheckResult> {
        let _ = (files, key);
        Ok(result)
    }

    /// Called once when a repair loop that checked under `key` ends: stop
    /// work that still runs and remove the state kept under `key`. The
    /// default runs a [`CheckMode::Clean`] check when `key` is not empty,
    /// and nothing when it is.
    async fn finish(&self, key: &str) -> Result<()> {
        if key.is_empty() {
            return Ok(());
        }
        self.check(&BTreeMap::new(), CheckMode::Clean, key)
            .await
            .map(|_| ())
    }

    /// True if this checker reports only problems that depend on a file and
    /// the files it imports (a type check, the tests), so that it can check
    /// the files of an app that has not all landed (the in-wave repair,
    /// `crate::inwave`). The default is false: a checker with rules over the
    /// whole app would see files that are not there yet.
    fn checks_landed_files(&self) -> bool {
        false
    }
}

/// A [`Checker`] that runs a shell command (`cmd /C` on Windows, `sh -c`
/// elsewhere) with the files as a markdown document on stdin, and reads
/// its stdout with [`parse_check`]. The exit code must be 0 for a pass.
/// Stderr is captured but not read.
#[derive(Debug, Clone)]
pub struct CommandChecker {
    command: String,
    timeout: Option<Duration>,
    pid: Option<Arc<AtomicU32>>,
}

impl CommandChecker {
    pub fn new(command: impl Into<String>) -> Self {
        Self {
            command: command.into(),
            timeout: None,
            pid: None,
        }
    }

    /// Stop a check that runs longer than `timeout` (an error). No limit by
    /// default. On a timeout only the shell is killed, not the processes it
    /// started; to stop those too, pass a pid slot ([`Self::with_pid`]) and
    /// kill the process tree of that id.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }

    /// Store the shell's process id in `pid` when a check starts, so the
    /// caller can stop the whole process tree.
    pub fn with_pid(mut self, pid: Arc<AtomicU32>) -> Self {
        self.pid = Some(pid);
        self
    }

    pub fn command(&self) -> &str {
        &self.command
    }
}

#[async_trait]
impl Checker for CommandChecker {
    async fn check(
        &self,
        files: &BTreeMap<String, String>,
        mode: CheckMode,
        key: &str,
    ) -> Result<CheckResult> {
        let doc = if mode == CheckMode::Clean {
            String::new()
        } else {
            render_doc(files)
        };
        let envs = [
            (MODE_ENV, mode.as_env()),
            (DIR_ENV, key),
            (GATE_MODE_ENV, mode.as_env()),
            (GATE_DIR_ENV, key),
        ];
        let run = run_shell_piped_with(&self.command, &doc, &envs, self.pid.as_deref());
        let output = match self.timeout {
            Some(limit) => tokio::time::timeout(limit, run)
                .await
                .map_err(|_| anyhow!("the check command timed out after {limit:?}"))?,
            None => run.await,
        }
        .context("could not run the check command")?;
        Ok(parse_check(
            output.status.success(),
            &String::from_utf8_lossy(&output.stdout),
        ))
    }
}

/// The number written immediately before `word` in `line`; 0 if none.
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

/// The file of `path(line,col): error TS...` (the gate's own form).
fn named(rest: &str) -> Option<String> {
    rest.find("): error TS")
        .and_then(|end| rest[..end].rfind('(').map(|open| rest[..open].to_string()))
}

/// The file of `path(line,col): ...`, any message. The path must look like
/// a file ([`looks_like_path`]: no whitespace, path characters only), so
/// prose that mentions a location (`ERROR gate: build failed at
/// ./a.tsx(3,4): ...`) is not a file.
fn located(rest: &str) -> Option<String> {
    let digits = |text: &str| !text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit());
    let mut from = 0;
    while let Some(at) = rest[from..].find("):") {
        let end = from + at;
        let before = &rest[..end];
        if let Some(open) = before.rfind('(') {
            let position = before[open + 1..].split_once(',');
            let path = &before[..open];
            if position.is_some_and(|(line, column)| digits(line) && digits(column))
                && looks_like_path(path)
            {
                return Some(path.to_string());
            }
        }
        from = end + 2;
    }
    None
}

/// Read a checker's output (the format is in the module docs). `passed` is
/// whether the command exited with 0.
pub fn parse_check(passed: bool, out: &str) -> CheckResult {
    let mut problems: BTreeMap<String, String> = BTreeMap::new();
    let mut hints: BTreeMap<String, String> = BTreeMap::new();
    let mut patches: BTreeMap<String, String> = BTreeMap::new();
    // The file that the lines belong to, and whether they are a warning.
    let mut current: Option<(String, bool)> = None;
    let mut summary = String::new();
    let mut lines = out.lines().peekable();
    while let Some(line) = lines.next() {
        if let Some(rest) = line.strip_prefix("PATCH ") {
            current = None;
            // The corrected file is the fenced block on the next line.
            let Some((width, _)) = lines.peek().and_then(|next| fence_of(next)) else {
                continue;
            };
            lines.next();
            let mut content = String::new();
            let mut closed = false;
            for inner in lines.by_ref() {
                if fence_of(inner).is_some_and(|(ticks, info)| ticks >= width && info.is_empty()) {
                    closed = true;
                    break;
                }
                content.push_str(inner);
                content.push('\n');
            }
            if let (Some(path), true) = (safe_path(rest), closed) {
                patches.insert(path, content);
            }
            continue;
        }
        if let Some(rest) = line.strip_prefix("ERROR ") {
            current = named(rest)
                .or_else(|| located(rest))
                .map(|file| (file, false));
        } else if let Some(rest) = line.strip_prefix("WARN ") {
            current = named(rest)
                .or_else(|| located(rest))
                .map(|file| (file, true));
        } else if let Some(rest) = line.strip_prefix("FAILED ") {
            current = rest
                .split(" > ")
                .next()
                .map(|file| (file.trim().to_string(), false));
        } else if !line.starts_with(' ') {
            current = located(line).map(|file| (file, false));
            if current.is_none() && line.contains(" failed, ") && line.contains(" passed") {
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
    CheckResult {
        // No summary line means the checker itself did not work: never a pass.
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
        patches,
        undecided: BTreeSet::new(),
    }
}

/// Put the checker's corrected files into `report`: each patch replaces the
/// output of the succeeded task with that title (as a fenced block, the
/// form a worker gives), with no model call. Returns the paths applied; a
/// patch for a file that no task wrote is left out.
pub fn apply_patches(report: &mut RunReport, patches: &BTreeMap<String, String>) -> Vec<String> {
    let mut applied = Vec::new();
    for (path, content) in patches {
        let record = report
            .tasks
            .values_mut()
            .find(|record| record.status == TaskStatus::Succeeded && record.title == *path);
        if let Some(record) = record {
            record.output = Some(fenced(content));
            applied.push(path.clone());
        }
    }
    applied
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Deserialize)]
    struct Recorded {
        name: String,
        exit_ok: bool,
        output: String,
        expect: Expected,
    }

    #[derive(Deserialize)]
    struct Expected {
        passed: bool,
        failed: u64,
        deep: bool,
        problems: BTreeMap<String, String>,
    }

    /// Gate results recorded by `bench/showdown` runs (`_meta/
    /// slimtoken-repair.json`), each with a gate output rebuilt from the
    /// stored problem text and summary line (not the raw gate stdout).
    const RECORDED: &str = include_str!("../tests/fixtures/gate-results.json");

    #[test]
    fn recorded_gate_outputs_give_the_recorded_verdicts() {
        let recorded: Vec<Recorded> = serde_json::from_str(RECORDED).unwrap();
        assert!(recorded.len() >= 10);
        for case in recorded {
            let result = parse_check(case.exit_ok, &case.output);
            assert_eq!(result.passed, case.expect.passed, "{}", case.name);
            assert_eq!(result.failed, case.expect.failed, "{}", case.name);
            assert_eq!(result.deep, case.expect.deep, "{}", case.name);
            assert_eq!(result.problems, case.expect.problems, "{}", case.name);
            assert!(result.patches.is_empty());
        }
    }

    #[test]
    fn output_is_grouped_by_file() {
        let out = "ERROR app/page.tsx(28,18): error TS2339: Property 'activity' does not exist.\n\
                   ERROR app/tasks/[id]/page.tsx(2,30): error TS2307: Cannot find module './X'.\n\
                   FAILED tests/all.test.ts > store > reduces actions\n    AssertionError: expected +0 to be 1\n\
                   3 failed, 15 passed (2 type errors)\n";
        let result = parse_check(false, out);
        assert!(!result.passed);
        assert_eq!(
            (result.failed, result.tests_passed, result.tests),
            (3, 15, 16)
        );
        assert_eq!(result.problems.len(), 3);
        assert!(result.problems["app/tasks/[id]/page.tsx"].contains("TS2307"));
        assert!(result.problems["tests/all.test.ts"].contains("AssertionError"));
        assert!(parse_check(true, "0 failed, 4 passed (0 type errors)\n").passed);
        // A warning is a hint, not a problem.
        let fast = parse_check(
            false,
            "WARN lib/a.ts(3,1): error TS9003: again\nERROR lib/b.ts(1,1): error TS1: x\n1 failed, 4 passed (1 type errors)\n",
        );
        assert_eq!(
            (fast.problems.len(), fast.hints.len(), fast.deep),
            (1, 1, false)
        );
        assert!(fast.hints["lib/a.ts"].starts_with("ERROR lib/a.ts(3,1)"));
        // `[deep]`: the real build ran, and it ranks above the fast checks.
        let deep = parse_check(
            false,
            "ERROR app/page.tsx(1,1): error TS9100: build\nERROR app/x.tsx(1,1): error TS9100: build\n2 failed, 4 passed (2 type errors) [deep]\n",
        );
        assert!(deep.deep && deep.nearer_than(&fast) && !fast.nearer_than(&deep));
        assert_eq!(deep.problems.len(), 2);
        // Fewer tests is never nearer.
        let fewer = CheckResult {
            tests: deep.tests - 1,
            deep: true,
            ..CheckResult::default()
        };
        assert!(!fewer.nearer_than(&deep));
    }

    #[test]
    fn no_summary_line_is_never_a_pass() {
        for out in ["", "all good\n", "ERROR lib/a.ts(1,1): error TS1: x\n"] {
            let result = parse_check(true, out);
            assert!(!result.passed, "{out:?}");
            assert_eq!(result.failed, 1);
            assert_eq!(result.summary, "the gate gave no result");
        }
        // A summary with a failing exit code is not a pass either.
        assert!(!parse_check(false, "0 failed, 4 passed (0 type errors)\n").passed);
    }

    #[test]
    fn each_file_keeps_at_most_the_excerpt_cap() {
        let mut out = String::new();
        for line in 1..=200 {
            out.push_str(&format!(
                "ERROR lib/big.ts({line},1): error TS2339: Property 'x' does not exist on type 'Y'.\n"
            ));
        }
        out.push_str(
            "ERROR lib/small.ts(1,1): error TS1: x\n201 failed, 0 passed (201 type errors)\n",
        );
        let result = parse_check(false, &out);
        let big = &result.problems["lib/big.ts"];
        assert!(big.len() < EXCERPT_CHARS, "{}", big.len());
        assert!(big.len() > EXCERPT_CHARS - 100);
        assert!(big.starts_with("ERROR lib/big.ts(1,1)"));
        assert_eq!(
            result.problems["lib/small.ts"],
            "ERROR lib/small.ts(1,1): error TS1: x\n"
        );
        assert_eq!(result.failed, 201);
    }

    #[test]
    fn files_are_named_with_or_without_the_error_prefix() {
        let out = "app/page.tsx(3,4): error TS2339: x\n  continuation of the message\n\
                   lib/a.py(10,2): E501 line too long\n\
                   ERROR lib/b.py(3,1): E302 expected 2 blank lines\n\
                   ERROR app/(main)/page.tsx(2,5): error TS2304: Cannot find name 'y'.\n\
                   some prose (1,2): is not a file\n\
                   ERROR gate: the type check did not run: x\n\
                   ERROR gate: `next build` failed: Error at ./app/a.tsx(3,4): boom\n\
                   WARN see ./lib/c.ts(1,1): prose\n\
                   3 failed, 0 passed (3 type errors)\n";
        let result = parse_check(false, out);
        let files: Vec<&str> = result.problems.keys().map(String::as_str).collect();
        assert_eq!(
            files,
            vec![
                "app/(main)/page.tsx",
                "app/page.tsx",
                "lib/a.py",
                "lib/b.py"
            ]
        );
        assert!(result.hints.is_empty());
        assert!(result.problems["app/page.tsx"].contains("continuation"));
        assert_eq!(result.summary, "3 failed, 0 passed (3 type errors)");
    }

    #[test]
    fn patch_blocks_are_corrected_files_not_problems() {
        let out = "PATCH lib/a.ts\n```ts\nERROR lib/a.ts(1,1): error TS1: inside\n0 failed, 9 passed\n```\n\
                   PATCH ../escape.ts\n```\nx\n```\n\
                   PATCH lib/no-fence.ts\n\
                   ERROR lib/b.ts(1,1): error TS1: x\n\
                   1 failed, 2 passed (1 type errors)\n\
                   PATCH lib/open.ts\n```\nnever closed\n";
        let result = parse_check(false, out);
        assert_eq!(result.patches.len(), 1);
        assert_eq!(
            result.patches["lib/a.ts"],
            "ERROR lib/a.ts(1,1): error TS1: inside\n0 failed, 9 passed\n"
        );
        let files: Vec<&str> = result.problems.keys().map(String::as_str).collect();
        assert_eq!(files, vec!["lib/b.ts"]);
        assert_eq!(result.summary, "1 failed, 2 passed (1 type errors)");
        // Serialized only when there are patches.
        let json = serde_json::to_string(&parse_check(true, "0 failed, 1 passed\n")).unwrap();
        assert!(!json.contains("patches") && !json.contains("hints"));
    }

    #[tokio::test]
    async fn patches_replace_the_output_of_the_named_task() {
        use crate::dag::{Plan, Task};
        let task = |id: &str, title: &str| Task {
            id: id.into(),
            title: title.into(),
            prompt: format!("Write {title}."),
            static_output: Some("```ts\nold\n```".into()),
            ..Task::default()
        };
        let plan = Plan {
            objective: "o".into(),
            tasks: vec![task("a", "lib/a.ts"), task("b", "lib/b.ts")],
            cache_primes: Vec::new(),
        };
        let client: Arc<dyn crate::ModelClient> =
            Arc::new(crate::client::MockClient::new(Duration::ZERO));
        let mut report = crate::scheduler::execute_plan(
            &plan,
            client,
            crate::scheduler::SchedulerConfig::default(),
        )
        .await
        .unwrap();
        let mut patches = BTreeMap::new();
        patches.insert("lib/a.ts".to_string(), "new\n".to_string());
        patches.insert("lib/none.ts".to_string(), "x\n".to_string());
        assert_eq!(apply_patches(&mut report, &patches), vec!["lib/a.ts"]);
        assert_eq!(report.tasks["a"].output.as_deref(), Some("```\nnew\n```"));
        assert_eq!(report.tasks["b"].output.as_deref(), Some("```ts\nold\n```"));
        assert_eq!(crate::project::report_files(&report)["lib/a.ts"], "new\n");
    }

    #[tokio::test]
    async fn command_checker_pipes_the_files_and_sets_the_mode() {
        let mut files = BTreeMap::new();
        files.insert(
            "lib/a.ts".to_string(),
            "ERROR lib/a.ts(1,1): error TS1: from stdin\n".to_string(),
        );
        let command = if cfg!(windows) {
            "findstr /r .& echo ERROR %SLIMTOKEN_CHECK_MODE%-%SHOWDOWN_GATE_DIR%.ts(1,1): error TS1: env& echo 2 failed, 0 passed (2 type errors)& exit /b 1"
        } else {
            "cat; echo \"ERROR $SLIMTOKEN_CHECK_MODE-$SHOWDOWN_GATE_DIR.ts(1,1): error TS1: env\"; echo '2 failed, 0 passed (2 type errors)'; exit 1"
        };
        let pid = Arc::new(AtomicU32::new(0));
        let checker = CommandChecker::new(command)
            .with_timeout(Duration::from_secs(60))
            .with_pid(pid.clone());
        let result = checker.check(&files, CheckMode::Fast, "k1").await.unwrap();
        assert!(!result.passed);
        assert_eq!(result.failed, 2);
        let names: Vec<&str> = result.problems.keys().map(String::as_str).collect();
        assert_eq!(names, vec!["fast-k1.ts", "lib/a.ts"]);
        assert_ne!(pid.load(std::sync::atomic::Ordering::Relaxed), 0);
        assert_eq!(CheckMode::Full.as_env(), "");
        assert_eq!(CheckMode::Clean.as_env(), "clean");
    }
}
