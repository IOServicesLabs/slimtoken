//! In-run integration testing with contract-arbitrated repair.
//!
//! Architect workers write modules and test suites blind to each other from
//! shared interface contracts. Where the contracts leave behavior open, a
//! test and its module can each be reasonable and still disagree. After the
//! wave, `reconcile` pipes the assembled deliverable to a test command; when
//! it fails, every artifact implicated in the failure report is re-attempted
//! once with the failures as context, under side-specific rules so the two
//! sides cannot chase each other:
//!
//! - a module changes only where it violates contract-stated behavior or
//!   crashes (e.g. calls something that does not exist);
//! - a test changes only where it contradicts the contracts or expects
//!   behavior they leave open — it then aligns with the implementation.
//!
//! The round is kept only if the test command reports no more failures than
//! before; otherwise the original artifacts are restored.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::sync::Arc;

use anyhow::Result;
use serde::Serialize;

use crate::client::ModelClient;
use crate::dag::{Plan, Task};
use crate::scheduler::{
    escalate_effort, execute_plan, run_shell_piped, RunReport, SchedulerConfig, TaskStatus,
};

/// Failure-report excerpt handed to each repaired artifact.
const EXCERPT_CHARS: usize = 4_000;
/// Module sources shown to a repaired test file.
const CONTEXT_CHARS: usize = 12_000;

#[derive(Debug, Clone, Serialize)]
pub struct ReconcileSummary {
    pub before: TestOutcome,
    pub after: Option<TestOutcome>,
    pub repaired: Vec<String>,
    /// False when the round made things worse and was rolled back.
    pub kept: bool,
    pub output_tokens: u64,
    pub elapsed_ms: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct TestOutcome {
    pub passed: bool,
    pub failed: Option<u64>,
    /// Tests collected (passed + failing tests, type errors excluded).
    pub tests: Option<u64>,
    pub summary: String,
}

async fn run_tests(command: &str, document: &str) -> Result<(TestOutcome, String)> {
    let output = tokio::time::timeout(
        std::time::Duration::from_secs(300),
        run_shell_piped(command, document),
    )
    .await
    .map_err(|_| anyhow::anyhow!("test command timed out after 300s"))??;
    let mut report = String::from_utf8_lossy(&output.stdout).into_owned();
    report.push_str(&String::from_utf8_lossy(&output.stderr));
    let summary = report
        .lines()
        .rev()
        .find(|line| line.contains(" passed") || line.contains(" failed"))
        .unwrap_or("")
        .trim()
        .to_string();
    Ok((
        TestOutcome {
            passed: output.status.success(),
            failed: count_before(&summary, " failed"),
            tests: collected_tests(&summary),
            summary,
        },
        report,
    ))
}

/// Tests the summary counts: passed + failed, minus failures that are type
/// errors (check-ts-project reports "N failed, M passed (K type errors)").
fn collected_tests(summary: &str) -> Option<u64> {
    let passed = count_before(summary, " passed")?;
    let failed = count_before(summary, " failed").unwrap_or(0);
    let type_errors = count_before(summary, " type errors").unwrap_or(0);
    Some(passed + failed.saturating_sub(type_errors))
}

/// The number written immediately before `word` (e.g. "3 failed" -> 3).
fn count_before(text: &str, word: &str) -> Option<u64> {
    let head = &text[..text.find(word)?];
    let digits: String = head
        .chars()
        .rev()
        .take_while(char::is_ascii_digit)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    digits.parse().ok()
}

fn is_test_file(path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path);
    path.starts_with("tests/") || name.starts_with("test_")
}

fn mentions(text: &str, path: &str) -> bool {
    text.contains(path) || text.contains(&path.replace('/', "\\"))
}

/// Failure-report blocks (pytest `____ name ____` sections and `FAILED`
/// lines) that mention `path`, capped.
fn excerpt_for(report: &str, path: &str) -> String {
    let mut blocks: Vec<String> = Vec::new();
    let mut current = String::new();
    for line in report.lines() {
        let starts_block =
            line.starts_with("____") || line.starts_with("FAILED ") || line.starts_with("ERROR ");
        if starts_block && !current.is_empty() {
            blocks.push(std::mem::take(&mut current));
        }
        current.push_str(line);
        current.push('\n');
    }
    blocks.push(current);
    let mut excerpt = String::new();
    for block in blocks.iter().filter(|block| mentions(block, path)) {
        if excerpt.len() + block.len() > EXCERPT_CHARS {
            break;
        }
        excerpt.push_str(block);
    }
    excerpt
}

/// Module stem as it appears in an import line (`games/sequence.py` ->
/// `sequence`).
fn stem(path: &str) -> &str {
    let name = path.rsplit('/').next().unwrap_or(path);
    [".py", ".tsx", ".ts", ".jsx", ".js"]
        .iter()
        .find_map(|ext| name.strip_suffix(ext))
        .unwrap_or(name)
}

/// Generated source the test command can implicate. Selection is by file
/// type, not `validate_artifact`: that flag means "has an in-run validator",
/// which TypeScript files lack, so a Next.js run had zero repair candidates
/// (nextjs-memory-4: 2 failures, 0 artifacts reconciled).
fn is_code_artifact(path: &str) -> bool {
    [".py", ".ts", ".tsx", ".js", ".jsx", ".mjs"]
        .iter()
        .any(|ext| path.ends_with(ext))
}

fn imports_module(test_source: &str, module_path: &str) -> bool {
    let stem = stem(module_path);
    test_source.lines().any(|line| {
        let line = line.trim_start();
        (line.starts_with("from ") || line.starts_with("import "))
            && line
                .split(|c: char| !c.is_alphanumeric() && c != '_')
                .any(|token| token == stem)
    })
}

fn reconcile_prompt(
    task: &Task,
    current: &str,
    excerpt: &str,
    context: &str,
    edits: bool,
) -> String {
    let path = &task.title;
    let mut prompt = task.prompt.clone();
    let _ = write!(
        prompt,
        "\n\n# Integration test results\nThe assembled project ran its test \
         suite. Failures involving `{path}`:\n```\n{excerpt}```\n"
    );
    if !context.is_empty() {
        let _ = write!(
            prompt,
            "\nThe implementation these tests exercise:\n{context}\n"
        );
    }
    let rules = if is_test_file(path) {
        "Fix tests that contradict the interface contracts or that expect \
         behavior the contracts leave open: align those with the \
         implementation shown. Keep every test of contract-stated behavior. \
         Never delete, skip or merge a test: fix its assertion instead (the \
         suite must collect at least as many tests as before). Change nothing \
         unrelated to these failures."
    } else {
        "Fix this module ONLY where it violates contract-stated behavior or \
         fails outright (an exception the contracts do not call for, a call \
         to something that does not exist). Behavior the contracts leave \
         open stays exactly as it is — the tests will be aligned to it. \
         Change nothing unrelated to these failures."
    };
    let output_rule = if edits {
        EDIT_FORMAT
    } else {
        "Output the COMPLETE corrected file in exactly one fenced code block."
    };
    let _ = write!(
        prompt,
        "\n{rules}\n\nYour current version of `{path}`:\n```\n{}\n```\n{output_rule}",
        current.trim()
    );
    prompt
}

/// Token budget for one edit-block repair (output plus reasoning).
const EDIT_REPAIR_MAX_TOKENS: u32 = 4096;

/// Edit-mode output contract: a repair usually changes a few assertions,
/// and regenerating the whole file cost ~2.5k tokens (+~1k reasoning) and
/// ~9s per test file on the Recall build.
const EDIT_FORMAT: &str = "Output ONLY search/replace edit blocks, never the whole \
file:\n<<<<<<< SEARCH\n(lines copied exactly from your current version)\n=======\n\
(their replacement)\n>>>>>>> REPLACE\nEach SEARCH must match exactly one place in \
the current version; include enough surrounding lines to make it unique. Use as \
many blocks as the fix needs.";

/// Apply search/replace edit blocks to `current`. None unless there is at
/// least one block and every SEARCH matches exactly once (an ambiguous or
/// missing anchor means the edit cannot be placed safely).
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

/// Test the assembled deliverable and repair implicated artifacts once.
/// `assemble` renders the deliverable from a report (the CLI's final-output
/// builder). Returns `None` when the tests already pass.
pub async fn reconcile(
    plan: &Plan,
    report: &mut RunReport,
    client: Arc<dyn ModelClient>,
    config: SchedulerConfig,
    test_cmd: &str,
    assemble: impl Fn(&RunReport) -> String,
) -> Result<Option<ReconcileSummary>> {
    reconcile_with(plan, report, client, config, test_cmd, assemble, false).await
}

/// `reconcile`, with `edits` asking repairs for search/replace blocks that
/// are applied to the current artifact instead of a regenerated file.
pub async fn reconcile_with(
    plan: &Plan,
    report: &mut RunReport,
    client: Arc<dyn ModelClient>,
    config: SchedulerConfig,
    test_cmd: &str,
    assemble: impl Fn(&RunReport) -> String,
    edits: bool,
) -> Result<Option<ReconcileSummary>> {
    let started = std::time::Instant::now();
    let (before, failures) = run_tests(test_cmd, &assemble(report)).await?;
    if before.passed {
        return Ok(None);
    }
    let outputs: BTreeMap<&str, &str> = plan
        .tasks
        .iter()
        .filter_map(|task| {
            let record = report.tasks.get(&task.id)?;
            (record.status == TaskStatus::Succeeded)
                .then_some(record.output.as_deref())
                .flatten()
                .map(|output| (task.id.as_str(), output))
        })
        .collect();
    let code_tasks: Vec<&Task> = plan
        .tasks
        .iter()
        .filter(|task| is_code_artifact(&task.title) && task.static_output.is_none())
        .filter(|task| outputs.contains_key(task.id.as_str()))
        .collect();
    let mut repairs = Vec::new();
    for task in &code_tasks {
        if !mentions(&failures, &task.title) {
            continue;
        }
        let excerpt = excerpt_for(&failures, &task.title);
        if excerpt.is_empty() {
            continue;
        }
        let current = outputs[task.id.as_str()];
        let mut context = String::new();
        if is_test_file(&task.title) {
            for module in code_tasks
                .iter()
                .filter(|other| !is_test_file(&other.title))
                .filter(|other| imports_module(current, &other.title))
            {
                let source = outputs[module.id.as_str()].trim();
                if context.len() + source.len() > CONTEXT_CHARS {
                    break;
                }
                let _ = write!(context, "\n`{}`:\n{}\n", module.title, source);
            }
        }
        let mut repair = (*task).clone();
        repair.prompt = reconcile_prompt(task, current, &excerpt, &context, edits);
        repair.deps.clear();
        if edits {
            // The output is edit blocks, not an artifact: the fence check and
            // the artifact validator apply to the edited file, which the
            // re-run suite tests anyway.
            repair.require_closed_fences = false;
            repair.validate_artifact = false;
            // Edit rounds spend 0.5-1.1k tokens incl. reasoning (run), but
            // one reasoned 7.8k at the test file's 8k budget (49s). A 4k cap
            // cuts such a runaway short; a hit retries with a doubled budget.
            repair.max_tokens = repair.max_tokens.min(EDIT_REPAIR_MAX_TOKENS);
        }
        // A failing integration suite is the evidence that reasoning is
        // needed: reconcile one effort level above the artifact's first pass
        // (at effort none, one round fixed only 3 of 12 failures).
        repair.reasoning_effort = escalate_effort(
            task.reasoning_effort
                .clone()
                .or_else(|| config.reasoning_effort.clone()),
        );
        repairs.push(repair);
    }
    tracing::warn!(
        failing = before.summary.as_str(),
        artifacts = repairs.len(),
        "integration tests failed; reconciling implicated artifacts"
    );
    let repaired: Vec<String> = repairs.iter().map(|task| task.title.clone()).collect();
    if repairs.is_empty() {
        return Ok(Some(ReconcileSummary {
            before,
            after: None,
            repaired,
            kept: false,
            output_tokens: 0,
            elapsed_ms: started.elapsed().as_millis() as u64,
        }));
    }
    let round = execute_plan(
        &Plan {
            objective: plan.objective.clone(),
            tasks: repairs,
            cache_primes: Vec::new(),
        },
        client,
        config,
    )
    .await?;

    let previous = report.clone();
    let mut output_tokens = 0;
    for (id, fixed) in &round.tasks {
        output_tokens += fixed.output_tokens;
        // Tokens are spent whether or not the round is kept.
        report.total_input_tokens += fixed.input_tokens;
        report.total_output_tokens += fixed.output_tokens;
        if let Some(reasoning) = fixed.reasoning_tokens {
            *report.total_reasoning_tokens.get_or_insert(0) += reasoning;
        }
        let Some(record) = report.tasks.get_mut(id) else {
            continue;
        };
        record.attempts += fixed.attempts;
        record.input_tokens += fixed.input_tokens;
        record.output_tokens += fixed.output_tokens;
        if let Some(reasoning) = fixed.reasoning_tokens {
            *record.reasoning_tokens.get_or_insert(0) += reasoning;
        }
        if fixed.status == TaskStatus::Succeeded {
            if !edits {
                record.output = fixed.output.clone();
            } else if let Some(edited) = record
                .output
                .as_deref()
                .zip(fixed.output.as_deref())
                .and_then(|(current, blocks)| apply_edit_blocks(current, blocks))
            {
                record.output = Some(edited);
            } else {
                tracing::warn!(task_id = %id, "repair edits did not apply; keeping the artifact");
            }
        }
    }
    let (after, _) = run_tests(test_cmd, &assemble(report)).await?;
    let worse = match (before.failed, after.failed) {
        (Some(was), Some(now)) => now > was,
        // A suite that stopped collecting (no count) after one that ran.
        (Some(_), None) => !after.passed,
        _ => false,
    } || matches!(
        // Fewer tests is not a fix: a repaired suite that deletes its failing
        // test reads green (TS replay ts-recon-1: 71 -> 70 collected).
        (before.tests, after.tests),
        (Some(was), Some(now)) if now < was
    );
    if worse {
        tracing::warn!(
            before = before.summary.as_str(),
            after = after.summary.as_str(),
            "reconcile round made tests worse; restoring previous artifacts"
        );
        for (id, record) in report.tasks.iter_mut() {
            if let Some(old) = previous.tasks.get(id) {
                record.output = old.output.clone();
            }
        }
    }
    Ok(Some(ReconcileSummary {
        before,
        after: Some(after),
        repaired,
        kept: !worse,
        output_tokens,
        elapsed_ms: started.elapsed().as_millis() as u64,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_failures_in_pytest_summaries() {
        assert_eq!(
            count_before("3 failed, 140 passed in 1.2s", " failed"),
            Some(3)
        );
        assert_eq!(count_before("140 passed in 1.2s", " failed"), None);
    }

    #[test]
    fn excerpts_keep_only_blocks_naming_the_file() {
        let report = "____ test_a ____\nE   in games\\span.py line 3\n____ test_b ____\nE   in stats.py\nFAILED tests/test_01.py::test_a - boom\n";
        let excerpt = excerpt_for(report, "games/span.py");
        assert!(excerpt.contains("test_a") && !excerpt.contains("stats.py"));
        assert!(excerpt_for(report, "tests/test_01.py").contains("FAILED"));
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn failing_suite_repairs_implicated_artifacts_and_keeps_the_fix() {
        use crate::client::{Completion, CompletionRequest};
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct Fixer {
            reconciles: AtomicUsize,
        }
        #[async_trait::async_trait]
        impl ModelClient for Fixer {
            async fn complete(&self, request: CompletionRequest) -> Result<Completion> {
                let text = if request.user.contains("# Integration test results") {
                    self.reconciles.fetch_add(1, Ordering::SeqCst);
                    "FIXED"
                } else {
                    "broken"
                };
                Ok(Completion {
                    text: text.into(),
                    provider: None,
                    input_tokens: 3,
                    output_tokens: 2,
                    reasoning_tokens: None,
                    cached_input_tokens: None,
                    finish_reason: Some("stop".into()),
                    usage_is_estimated: false,
                    elapsed: std::time::Duration::from_millis(1),
                    time_to_first_token: None,
                })
            }
        }
        let artifact = |id: &str, path: &str| Task {
            id: id.into(),
            title: path.into(),
            prompt: format!("Write {path}."),
            deps: Vec::new(),
            max_tokens: 64,
            temperature: None,
            require_closed_fences: false,
            validate_artifact: true,
            static_output: None,
            reasoning_effort: None,
            internal: false,
        };
        let plan = Plan {
            objective: "reconcile test".into(),
            tasks: vec![
                artifact("mod", "mod.py"),
                artifact("test", "tests/test_a.py"),
                artifact("other", "other.py"),
            ],
            cache_primes: Vec::new(),
        };
        let client: Arc<dyn ModelClient> = Arc::new(Fixer {
            reconciles: AtomicUsize::new(0),
        });
        let mut report = execute_plan(&plan, client.clone(), SchedulerConfig::default())
            .await
            .unwrap();
        // Passes only once no artifact is "broken"; the failure report names
        // mod.py and the test file, not other.py.
        let command = "findstr broken >nul && (echo FAILED tests/test_a.py::t - boom in mod.py & echo 1 failed & exit /b 1) || (echo 3 passed & exit /b 0)";
        let summary = reconcile(
            &plan,
            &mut report,
            client,
            SchedulerConfig::default(),
            command,
            |report| {
                ["mod", "test", "other"]
                    .iter()
                    .map(|id| report.tasks[*id].output.clone().unwrap_or_default())
                    .collect::<Vec<_>>()
                    .join(
                        "
",
                    )
            },
        )
        .await
        .unwrap()
        .expect("suite failed, so a round ran");
        let mut repaired = summary.repaired.clone();
        repaired.sort();
        assert_eq!(repaired, ["mod.py", "tests/test_a.py"]);
        assert!(summary.kept);
        assert_eq!(report.tasks["mod"].output.as_deref(), Some("FIXED"));
        assert_eq!(report.tasks["other"].output.as_deref(), Some("broken"));
        assert_eq!(summary.before.failed, Some(1));
    }

    #[test]
    fn edit_blocks_apply_only_with_unique_anchors() {
        let current = "```ts\nit(\"a\", () => {\n  expect(f()).toBe(1);\n});\n```";
        let edits = "<<<<<<< SEARCH\n  expect(f()).toBe(1);\n=======\n  expect(f()).toBe(2);\n>>>>>>> REPLACE\n";
        assert_eq!(
            apply_edit_blocks(current, edits).as_deref(),
            Some("```ts\nit(\"a\", () => {\n  expect(f()).toBe(2);\n});\n```")
        );
        // Missing anchor, ambiguous anchor, no blocks: refuse.
        let missing = "<<<<<<< SEARCH\nnot there\n=======\nx\n>>>>>>> REPLACE";
        assert_eq!(apply_edit_blocks(current, missing), None);
        let twice = "```\nx\nx\n```";
        let ambiguous = "<<<<<<< SEARCH\nx\n=======\ny\n>>>>>>> REPLACE";
        assert_eq!(apply_edit_blocks(twice, ambiguous), None);
        assert_eq!(apply_edit_blocks(current, "the whole file again"), None);
    }

    #[test]
    fn collected_tests_exclude_type_errors() {
        assert_eq!(
            collected_tests("4 failed, 71 passed (3 type errors)"),
            Some(72)
        );
        assert_eq!(
            collected_tests("0 failed, 70 passed (0 type errors)"),
            Some(70)
        );
        assert_eq!(collected_tests("3 failed, 40 passed in 1.2s"), Some(43));
        assert_eq!(collected_tests("collection error"), None);
    }

    #[test]
    fn imports_match_module_stems() {
        let test = "import os\nfrom games import sequence as sq\nimport stats\n";
        assert!(imports_module(test, "games/sequence.py"));
        assert!(imports_module(test, "stats.py"));
        assert!(!imports_module(test, "engine.py"));
        let ts =
            "import { describe, it } from \"vitest\";\nimport { grade } from \"../lib/srs\";\n";
        assert!(imports_module(ts, "lib/srs.ts"));
        assert!(!imports_module(ts, "lib/games.ts"));
        assert!(is_code_artifact("app/page.tsx") && is_code_artifact("tests/a.test.ts"));
        assert!(!is_code_artifact("README.md") && !is_code_artifact("app/globals.css"));
    }
}
