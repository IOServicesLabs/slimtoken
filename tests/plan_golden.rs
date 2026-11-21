//! Golden plans: the plan built from a fixed manifest must not change byte
//! for byte. A change here changes what every host sends to the model. Set
//! `SLIMTOKEN_BLESS=1` to rewrite the golden files after an intended change.

use std::path::PathBuf;

use slimtoken::architect::{plan_from_manifest, Manifest, PlanOptions};
use slimtoken::profile::{Profile, NEXTJS, PYTHON, RUST};

const OBJECTIVE: &str = "Build a task tracker with projects, a board, a calendar and reports.";

fn fixtures() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

/// Plan `manifest` (a fixture file) with `profile` and `options` and compare
/// it with `golden/<golden>.json`.
fn check(profile: &'static Profile, manifest: &str, options: &PlanOptions, golden: &str) {
    check_with(OBJECTIVE, profile, manifest, options, golden);
}

/// [`check`] with another objective.
fn check_with(
    objective: &str,
    profile: &'static Profile,
    manifest: &str,
    options: &PlanOptions,
    golden: &str,
) {
    let text = std::fs::read_to_string(fixtures().join(manifest)).unwrap();
    let manifest: Manifest = serde_json::from_str(&text).unwrap();
    let plan = plan_from_manifest(objective, &manifest, 64, Some(profile), options).unwrap();
    let actual = serde_json::to_string_pretty(&plan).unwrap() + "\n";
    let golden = fixtures().join(format!("golden/{golden}.json"));
    if std::env::var_os("SLIMTOKEN_BLESS").is_some() {
        std::fs::write(&golden, &actual).unwrap();
        return;
    }
    let expected = std::fs::read_to_string(&golden)
        .unwrap_or_else(|err| panic!("cannot read {}: {err}", golden.display()))
        .replace("\r\n", "\n");
    assert!(
        actual == expected,
        "plan for profile {} differs from {}",
        profile.name,
        golden.display()
    );
}

/// Plan the shared TypeScript manifest with default options.
fn check_default(profile: &'static Profile) {
    let golden = format!("plan-{}", profile.name);
    check(
        profile,
        "plan-manifest.json",
        &PlanOptions::default(),
        &golden,
    );
}

#[test]
fn python_plan_matches_golden() {
    check_default(&PYTHON);
}

#[test]
fn rust_plan_matches_golden() {
    check_default(&RUST);
}

#[test]
fn nextjs_plan_matches_golden() {
    check_default(&NEXTJS);
}

#[test]
fn python_native_plan_matches_golden() {
    check(
        &PYTHON,
        "plan-manifest-python.json",
        &PlanOptions::default(),
        "plan-python-native",
    );
}

#[test]
fn rust_native_plan_matches_golden() {
    check(
        &RUST,
        "plan-manifest-rust.json",
        &PlanOptions::default(),
        "plan-rust-native",
    );
}

#[test]
fn python_native_plan_with_options_matches_golden() {
    let options = PlanOptions {
        tests_see_code: true,
        scope_contracts: true,
        ..PlanOptions::default()
    };
    check(
        &PYTHON,
        "plan-manifest-python.json",
        &options,
        "plan-python-native-options",
    );
}

/// The bench's TaskFlow objective (its page list feeds `objective_pages`).
const TASKFLOW: &str = "Build \"TaskFlow\", a complete project management web app with Next.js \
(App Router) and TypeScript. No backend: all data lives in local storage, with realistic seed data \
on first load.\n\nPages:\n- Dashboard: summary cards (open tasks, overdue, completed this week), a \
burndown chart, a tasks-by-status chart and a recent activity feed.\n- Projects: list of projects \
with progress bars; create, rename, archive.\n- Board: a kanban board for one project with columns \
Backlog, In Progress, Review, Done; move tasks between columns, filter by assignee and priority.\n- \
Task detail: edit title, description, priority, due date, assignee, labels; subtasks with a \
checklist; comments.\n- Calendar: month view with tasks on their due dates, previous and next \
month.\n- Team: members with avatar initials, role, and workload (open tasks per member).\n- \
Reports: velocity per week, cycle time statistics, and CSV export of tasks.\n- Search: global \
search across tasks, projects and comments with highlighted matches.\n- Settings: theme (light, \
dark), default project, and reset demo data.\n\nQuality bar: every page is complete and usable, \
with real controls wired to the lib modules.\n";

/// Every quality switch of the bench on (`qrules`, `qobjective`,
/// `qliteral`, `qcontract`, `qwiring`, `qcss`, `prefix` with the staged
/// steps), with `page_local` as given.
fn quality_options(page_local: bool) -> PlanOptions {
    PlanOptions {
        staged: true,
        split: true,
        tidy: true,
        one_type_owner: true,
        declare_types: true,
        relative_dates: true,
        quality_rules: true,
        objective_pages: true,
        align_literals: true,
        contract: true,
        wave_wiring: true,
        wiring_rules: true,
        consumers_see_css: true,
        deps_first: true,
        page_local,
        ..PlanOptions::default()
    }
}

/// With `page_local` off, the quality switches plan as they did before it
/// existed (every `page_local` branch is gated on the option; this golden
/// was blessed with the first `page_local` change and guards the later ones).
#[test]
fn nextjs_plan_with_the_quality_switches_matches_golden() {
    check_with(
        TASKFLOW,
        &NEXTJS,
        "showdown-manifest.json",
        &quality_options(false),
        "plan-nextjs-quality",
    );
}
