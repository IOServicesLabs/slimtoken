//! Delivery status: does the app the judge reads actually install, build,
//! pass its tests and render its pages?
//!
//! The model lenses read the code only, so a project whose `next build`
//! fails can still score 90. The judge therefore reads each arm's real
//! check results and scores the app as delivered:
//!
//! - **Recorded** results come first: the race's `results.json` (the
//!   `install`, `build`, `test` and `pages` checks that `showdown run`
//!   records for each arm), else a `bench-slim` run's
//!   `_meta/slimtoken-repair.json` (the gate's last result: `[deep]` means
//!   the real build and the pages passed).
//! - **Local** checks otherwise (see `smoke.rs`): the arm's files are built
//!   with the shared toolchain (`slimtoken-out-showdown/_deps`), its tests
//!   run, and its static pages are requested. No install: the toolchain is
//!   shared.
//!
//! `q_delivered` (see [`q_delivered`]): a build (or install) that fails, or
//! no page that renders, gives `0.25 * q_if_built` and the flag "does not
//! run"; each failed test costs 2 points (at most 10); pages that fail
//! scale the rest by the share that renders (at least 0.25). The winner as
//! delivered (`pick_winners`) is the app that runs when only one does, and
//! the higher q_delivered otherwise: the numbers alone could let a high-Q
//! app that does not build beat a running app with few pages.

use serde::{Deserialize, Serialize};
use std::path::Path;

/// The four checks of one arm, with where they came from.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
pub struct Delivered {
    /// `results.json`, `slimtoken-repair.json` or `local`.
    pub source: String,
    pub install: Option<bool>,
    pub build: Option<bool>,
    pub tests: Option<bool>,
    pub pages: Option<bool>,
    pub failed_tests: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tests_total: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pages_passed: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pages_total: Option<usize>,
    /// The checks' summary lines (`build: Type error: ...`).
    #[serde(default)]
    pub notes: Vec<String>,
}

/// The penalty per failed test and its cap.
pub const TEST_PENALTY: f64 = 2.0;
pub const TEST_PENALTY_CAP: f64 = 10.0;
/// The share of Q that an app keeps when it does not run.
pub const NOT_RUNNING: f64 = 0.25;

impl Delivered {
    /// True when the app cannot be used: no install, no build, or no page
    /// that renders.
    pub fn does_not_run(&self) -> bool {
        self.install == Some(false)
            || self.build == Some(false)
            || (self.pages == Some(false) && self.pages_passed == Some(0))
    }
}

/// Q as delivered, from Q as if built, and the "does not run" flag.
pub fn q_delivered(q_if_built: f64, delivered: &Delivered) -> (f64, bool) {
    if delivered.does_not_run() {
        return (round1(NOT_RUNNING * q_if_built), true);
    }
    let penalty = (TEST_PENALTY * delivered.failed_tests as f64).min(TEST_PENALTY_CAP);
    let mut q = (q_if_built - penalty).max(0.0);
    if let (Some(passed), Some(total)) = (delivered.pages_passed, delivered.pages_total) {
        if total > 0 && passed < total {
            q *= (passed as f64 / total as f64).max(NOT_RUNNING);
        }
    }
    (round1(q), false)
}

fn round1(value: f64) -> f64 {
    (value * 10.0).round() / 10.0
}

/// The number written immediately before `word` in `text` (`3 failed`).
fn count_before(text: &str, word: &str) -> Option<usize> {
    let lower = text.to_lowercase();
    let at = lower.find(word)?;
    let digits: String = lower[..at]
        .trim_end()
        .chars()
        .rev()
        .take_while(char::is_ascii_digit)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    digits.parse().ok()
}

/// The number written immediately after `words` in `text` (`Failed Tests 2`).
fn count_after(text: &str, words: &str) -> Option<usize> {
    let lower = text.to_lowercase();
    let at = lower.find(words)? + words.len();
    let digits: String = lower[at..]
        .trim_start()
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    digits.parse().ok()
}

/// (passed, failed) counts in a summary line (`Tests 2 failed | 30 passed
/// (32)`, `0 passed | 8 failed (...)`, `8 passed (8 pages)`, vitest's
/// `⎯⎯ Failed Tests 2 ⎯⎯` banner).
pub fn counts(summary: &str) -> (Option<usize>, Option<usize>) {
    (
        count_before(summary, "passed"),
        count_before(summary, "failed").or_else(|| count_after(summary, "failed tests")),
    )
}

/// One arm's checks from a race's `results.json` value.
pub fn from_results(results: &serde_json::Value, arm: &str) -> Option<Delivered> {
    let checks = results[arm]["checks"].as_array()?;
    if checks.is_empty() {
        return None;
    }
    let mut delivered = Delivered {
        source: "results.json".to_string(),
        ..Delivered::default()
    };
    for check in checks {
        let name = check["name"].as_str().unwrap_or_default();
        let pass = match check["state"].as_str() {
            Some("pass") => Some(true),
            Some("fail") => Some(false),
            _ => None,
        };
        let summary = check["summary"].as_str().unwrap_or_default();
        if !summary.is_empty() {
            delivered.notes.push(format!(
                "{name}: {}",
                summary.chars().take(160).collect::<String>()
            ));
        }
        let (passed, failed) = counts(summary);
        match name {
            "install" => delivered.install = pass,
            "build" => delivered.build = pass,
            "test" => {
                delivered.tests = pass;
                delivered.failed_tests = match (pass, failed) {
                    (_, Some(failed)) => failed,
                    // A failed test step with no count: one failure (no
                    // test file, or the runner did not start).
                    (Some(false), None) => 1,
                    _ => 0,
                };
                delivered.tests_total = match (passed, failed) {
                    (None, None) => None,
                    (passed, failed) => Some(passed.unwrap_or(0) + failed.unwrap_or(0)),
                };
            }
            "pages" => {
                delivered.pages = pass;
                delivered.pages_passed = passed.or(match pass {
                    Some(false) => Some(0),
                    _ => None,
                });
                delivered.pages_total = match (passed, failed) {
                    (None, None) => None,
                    (passed, failed) => Some(passed.unwrap_or(0) + failed.unwrap_or(0)),
                };
            }
            _ => {}
        }
    }
    Some(delivered)
}

/// A `bench-slim` run's checks from `_meta/slimtoken-repair.json`: the
/// gate's last result. `[deep]` (the real build ran) with no failure means
/// the build and the pages passed; a gate that stopped before the build
/// leaves them unknown.
pub fn from_repair(repair: &serde_json::Value) -> Option<Delivered> {
    let after = repair.get("after").filter(|after| after.is_object())?;
    let passed = after["passed"].as_bool()?;
    let deep = after["deep"].as_bool().unwrap_or(false);
    let tests = after["tests"].as_u64().map(|n| n as usize);
    let tests_passed = after["tests_passed"].as_u64().map(|n| n as usize);
    let failed_tests = match (tests, tests_passed) {
        (Some(total), Some(ok)) => total.saturating_sub(ok),
        _ => 0,
    };
    let summary = after["summary"].as_str().unwrap_or_default().to_string();
    Some(Delivered {
        source: "slimtoken-repair.json".to_string(),
        install: None,
        // A deep gate that failed failed the build or a page: which one is
        // not recorded, so both stay unknown.
        build: (deep && passed).then_some(true),
        tests: tests.map(|_| failed_tests == 0),
        pages: (deep && passed).then_some(true),
        failed_tests,
        tests_total: tests,
        pages_passed: None,
        pages_total: None,
        notes: vec![format!("gate: {summary}")],
    })
}

/// The recorded checks of the app in `dir` (`<run>/<arm>`): the run's
/// `results.json` under the arm's name, else (for the SlimToken arm of a
/// `bench-slim` run) `_meta/slimtoken-repair.json`.
pub fn recorded(dir: &Path) -> Option<Delivered> {
    let arm = dir.file_name()?.to_string_lossy().into_owned();
    let run = dir.parent()?;
    let read = |path: &Path| {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
    };
    if let Some(found) =
        read(&run.join("results.json")).and_then(|results| from_results(&results, &arm))
    {
        return Some(found);
    }
    if arm == "slimtoken" {
        return read(&run.join("_meta").join("slimtoken-repair.json"))
            .and_then(|repair| from_repair(&repair));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn delivered(build: bool, failed_tests: usize, pages: Option<(usize, usize)>) -> Delivered {
        Delivered {
            source: "test".into(),
            install: Some(true),
            build: Some(build),
            tests: Some(failed_tests == 0),
            pages: pages.map(|(passed, total)| passed == total),
            failed_tests,
            tests_total: Some(30),
            pages_passed: pages.map(|(passed, _)| passed),
            pages_total: pages.map(|(_, total)| total),
            notes: Vec::new(),
        }
    }

    #[test]
    fn the_delivery_formula() {
        // All green: Q as built.
        assert_eq!(
            q_delivered(90.0, &delivered(true, 0, Some((8, 8)))),
            (90.0, false)
        );
        // A failed build: a quarter, "does not run".
        assert_eq!(
            q_delivered(92.4, &delivered(false, 0, Some((0, 8)))),
            (23.1, true)
        );
        // Failed tests: 2 points each, at most 10.
        assert_eq!(q_delivered(80.0, &delivered(true, 2, Some((8, 8)))).0, 76.0);
        assert_eq!(q_delivered(80.0, &delivered(true, 9, Some((8, 8)))).0, 70.0);
        // Pages: proportional, at least a quarter; none at all does not run.
        assert_eq!(q_delivered(80.0, &delivered(true, 0, Some((6, 8)))).0, 60.0);
        assert_eq!(q_delivered(80.0, &delivered(true, 0, Some((1, 8)))).0, 20.0);
        assert_eq!(
            q_delivered(80.0, &delivered(true, 0, Some((0, 8)))),
            (20.0, true)
        );
        // A failed install does not run either; unknown checks change nothing.
        let mut install = delivered(true, 0, None);
        install.install = Some(false);
        assert!(q_delivered(80.0, &install).1);
        assert_eq!(q_delivered(80.0, &Delivered::default()), (80.0, false));
    }

    #[test]
    fn summary_counts() {
        assert_eq!(counts("Tests 56 passed (56)"), (Some(56), None));
        assert_eq!(
            counts("Tests  2 failed | 30 passed (32)"),
            (Some(30), Some(2))
        );
        assert_eq!(
            counts("0 passed | 8 failed (no production build: the build check must pass first)"),
            (Some(0), Some(8))
        );
        assert_eq!(counts("8 passed (8 pages)"), (Some(8), None));
        assert_eq!(counts("Type error: x"), (None, None));
        // vitest's banner (20261114-063700 results.json): 2, not 1.
        assert_eq!(counts("⎯⎯⎯⎯⎯⎯⎯ Failed Tests 2 ⎯⎯⎯⎯⎯⎯⎯"), (None, Some(2)));
        let results = serde_json::json!({"slimtoken": {"checks": [
            {"name": "test", "state": "fail", "summary": "⎯⎯⎯⎯⎯⎯⎯ Failed Tests 2 ⎯⎯⎯⎯⎯⎯⎯"}
        ]}});
        assert_eq!(from_results(&results, "slimtoken").unwrap().failed_tests, 2);
    }

    #[test]
    fn results_json_and_repair_json_are_read() {
        let results = serde_json::json!({
            "single": {"checks": [
                {"name": "install", "state": "pass", "summary": "added 60 packages"},
                {"name": "build", "state": "fail", "summary": "Type error: Argument of type"},
                {"name": "test", "state": "fail", "summary": "Tests 3 failed | 53 passed (56)"},
                {"name": "pages", "state": "fail", "summary": "0 passed | 8 failed (no production build)"}
            ]},
            "slimtoken": {"checks": [
                {"name": "install", "state": "pass", "summary": ""},
                {"name": "build", "state": "pass", "summary": ""},
                {"name": "test", "state": "fail", "summary": "No test files found"},
                {"name": "pages", "state": "pass", "summary": "8 passed (8 pages)"}
            ]}
        });
        let single = from_results(&results, "single").unwrap();
        assert_eq!(single.build, Some(false));
        assert_eq!((single.failed_tests, single.tests_total), (3, Some(56)));
        assert_eq!(
            (single.pages_passed, single.pages_total),
            (Some(0), Some(8))
        );
        assert!(single.does_not_run());
        assert!(single.notes[1].starts_with("build: Type error"));
        let slim = from_results(&results, "slimtoken").unwrap();
        assert_eq!(slim.failed_tests, 1, "a failed test step with no count");
        assert!(!slim.does_not_run());
        assert!(from_results(&results, "other").is_none());

        let repair = serde_json::json!({"after": {"deep": true, "failed": 0, "passed": true,
            "summary": "0 failed, 32 passed (0 type errors) [deep]", "tests": 32, "tests_passed": 32}});
        let gate = from_repair(&repair).unwrap();
        assert_eq!(
            (gate.build, gate.pages, gate.failed_tests),
            (Some(true), Some(true), 0)
        );
        let shallow = serde_json::json!({"after": {"deep": false, "passed": false,
            "summary": "2 failed", "tests": 32, "tests_passed": 30}});
        let gate = from_repair(&shallow).unwrap();
        assert_eq!((gate.build, gate.pages, gate.failed_tests), (None, None, 2));
    }
}
