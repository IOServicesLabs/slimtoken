//! The manifest passes (`PlanOptions::one_type_owner`, `tidy`, `today`) on
//! a manifest that `bench/showdown` wrote. The golden file is the output of
//! the bench's own steps (`tidy_manifest`, `seedcompact`, `seeddates`) on
//! the same manifest; the bench's test `engine_tidy_matches_the_golden_manifest`
//! checks its steps against the same file. Set `SLIMTOKEN_BLESS=1` to rewrite
//! the golden file after an intended change.

use std::path::PathBuf;

use slimtoken::architect::{
    apply_manifest_passes, plan_from_manifest, stamp_today, Manifest, PlanOptions,
};
use slimtoken::profile::{Profile, NEXTJS, PYTHON, RUST};

const OBJECTIVE: &str = "Build a task tracker with projects, a board, a calendar and reports.";

/// The date the golden file was written with.
const TODAY: (i32, u32, u32) = (2026, 11, 14);

fn fixtures() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

fn manifest(name: &str) -> Manifest {
    let text = std::fs::read_to_string(fixtures().join(name)).unwrap();
    serde_json::from_str(&text).unwrap()
}

fn json(manifest: &Manifest) -> String {
    serde_json::to_string_pretty(manifest).unwrap() + "\n"
}

/// Every pass on.
fn all_passes() -> PlanOptions {
    PlanOptions {
        one_type_owner: true,
        tidy: true,
        today: Some(TODAY),
        ..PlanOptions::default()
    }
}

fn passed(name: &str, profile: &Profile, options: &PlanOptions) -> Manifest {
    let mut manifest = manifest(name);
    apply_manifest_passes(&mut manifest, profile, options);
    manifest
}

#[test]
fn engine_passes_match_the_bench_steps() {
    let actual = json(&passed("showdown-manifest.json", &NEXTJS, &all_passes()));
    let golden = fixtures().join("golden/showdown-tidy.json");
    if std::env::var_os("SLIMTOKEN_BLESS").is_some() {
        std::fs::write(&golden, &actual).unwrap();
        return;
    }
    let expected = std::fs::read_to_string(&golden)
        .unwrap()
        .replace("\r\n", "\n");
    assert!(actual == expected, "differs from {}", golden.display());
}

#[test]
fn engine_passes_change_the_bench_manifest() {
    let before = manifest("showdown-manifest.json");
    let after = passed("showdown-manifest.json", &NEXTJS, &all_passes());
    let file = |manifest: &Manifest, path: &str| {
        manifest
            .files
            .iter()
            .find(|file| file.path == path)
            .cloned()
            .unwrap()
    };
    // `lib/tasks.ts` planned its own `Task`: it now imports the types file's.
    let tasks = file(&after, "lib/tasks.ts");
    assert!(!tasks
        .exports
        .iter()
        .any(|line| line.starts_with("export interface Task")));
    assert!(tasks.purpose.contains("Imports Task"));
    assert!(tasks.purpose.contains("`lib/types.ts`"));
    // The dynamic route and the `searchParams` page are Client Components.
    for path in ["app/tasks/[id]/page.tsx", "app/calendar/page.tsx"] {
        let page = file(&after, path);
        assert!(
            page.exports
                .iter()
                .all(|line| !line.contains("async function")),
            "{path}"
        );
        assert!(page.exports[0].contains("Client Component"), "{path}");
    }
    // The seed data gets the compact note and the date.
    let seed = file(&after, "lib/seed.ts");
    assert!(seed.purpose.contains("one compact object literal"));
    assert!(seed
        .purpose
        .ends_with("some in the past and some in the future."));
    assert!(seed.purpose.contains("Today is 2026-11-14:"));
    // Files the passes have nothing to say about keep their plan.
    assert_eq!(
        json_of(&file(&after, "lib/id.ts")),
        json_of(&file(&before, "lib/id.ts"))
    );
}

fn json_of<T: serde::Serialize>(value: &T) -> String {
    serde_json::to_string(value).unwrap()
}

#[test]
fn engine_passes_are_idempotent() {
    for name in ["showdown-manifest.json", "plan-manifest.json"] {
        let once = passed(name, &NEXTJS, &all_passes());
        let mut twice = once.clone();
        apply_manifest_passes(&mut twice, &NEXTJS, &all_passes());
        assert_eq!(json(&once), json(&twice), "{name}");
        // A later date does not stamp a second day.
        stamp_today(&mut twice, &NEXTJS, (2027, 1, 2));
        assert_eq!(json(&once), json(&twice), "{name}");
    }
}

#[test]
fn default_options_change_nothing() {
    for name in ["showdown-manifest.json", "plan-manifest.json"] {
        for profile in [&NEXTJS, &PYTHON, &RUST] {
            let unchanged = passed(name, profile, &PlanOptions::default());
            assert_eq!(json(&unchanged), json(&manifest(name)), "{name}");
        }
    }
}

#[test]
fn python_and_rust_manifests_are_unchanged() {
    let cases: [(&'static Profile, &str); 4] = [
        (&PYTHON, "plan-manifest-python.json"),
        (&PYTHON, "showdown-manifest.json"),
        (&RUST, "plan-manifest-rust.json"),
        (&RUST, "showdown-manifest.json"),
    ];
    for (profile, name) in cases {
        let after = passed(name, profile, &all_passes());
        assert_eq!(json(&after), json(&manifest(name)), "{}", profile.name);
        // So the plan is the default plan too.
        let plan = |options: &PlanOptions| {
            let plan =
                plan_from_manifest(OBJECTIVE, &manifest(name), 64, Some(profile), options).unwrap();
            serde_json::to_string(&plan).unwrap()
        };
        assert_eq!(
            plan(&all_passes()),
            plan(&PlanOptions::default()),
            "{}",
            profile.name
        );
    }
}

#[test]
fn each_option_runs_only_its_passes() {
    let base = manifest("showdown-manifest.json");
    let only = |options: PlanOptions| passed("showdown-manifest.json", &NEXTJS, &options);
    let owner = only(PlanOptions {
        one_type_owner: true,
        ..PlanOptions::default()
    });
    let tidy = only(PlanOptions {
        tidy: true,
        ..PlanOptions::default()
    });
    let dated = only(PlanOptions {
        today: Some(TODAY),
        ..PlanOptions::default()
    });
    let changed = |after: &Manifest| -> Vec<String> {
        after
            .files
            .iter()
            .zip(&base.files)
            .filter(|(a, b)| json_of(a) != json_of(b))
            .map(|(a, _)| a.path.clone())
            .collect()
    };
    let owner_changed = changed(&owner);
    assert!(owner_changed.contains(&"lib/tasks.ts".to_string()));
    assert!(owner_changed.iter().all(|path| path.ends_with(".ts")));
    let tidy_changed = changed(&tidy);
    assert!(tidy_changed.contains(&"app/tasks/[id]/page.tsx".to_string()));
    assert!(tidy_changed.contains(&"lib/seed.ts".to_string()));
    assert!(!tidy_changed.contains(&"lib/tasks.ts".to_string()));
    assert_eq!(changed(&dated), ["lib/seed.ts"]);
}
