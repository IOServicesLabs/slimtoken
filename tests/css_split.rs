//! The split of long files (`PlanOptions::split`) and the rendered file
//! that joins the parts (`Staging::aggregate`), on a manifest that
//! `bench/showdown` wrote. The golden file is the output of the bench's own
//! steps (`split_css`, then `css_imports` in a `css` code block); the
//! bench's test `engine_split_matches_the_golden_manifest` checks its steps
//! against the same file. Set `SLIMTOKEN_BLESS=1` to rewrite the golden file
//! after an intended change.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::{bail, Result};
use slimtoken::architect::{apply_manifest_passes, plan_from_manifest, Manifest, PlanOptions};
use slimtoken::dag::Plan;
use slimtoken::profile::{Profile, NEXTJS, PYTHON, RUST};
use slimtoken::scheduler::{execute_plan, SchedulerConfig};
use slimtoken::{async_trait, Completion, CompletionRequest, ModelClient};

const OBJECTIVE: &str = "Build a task tracker with projects, a board, a calendar and reports.";

fn fixtures() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

fn manifest(name: &str) -> Manifest {
    let text = std::fs::read_to_string(fixtures().join(name)).unwrap();
    serde_json::from_str(&text).unwrap()
}

fn split_only() -> PlanOptions {
    PlanOptions {
        split: true,
        ..PlanOptions::default()
    }
}

fn split(mut manifest: Manifest, profile: &Profile) -> Manifest {
    apply_manifest_passes(&mut manifest, profile, &split_only());
    manifest
}

fn plan(manifest: &Manifest, profile: &'static Profile, options: &PlanOptions) -> Plan {
    plan_from_manifest(OBJECTIVE, manifest, 64, Some(profile), options).unwrap()
}

/// The rendered `.css` files of a plan: path to `static_output`.
fn rendered_css(plan: &Plan) -> BTreeMap<String, String> {
    plan.tasks
        .iter()
        .filter(|task| task.title.ends_with(".css"))
        .filter_map(|task| Some((task.title.clone(), task.static_output.clone()?)))
        .collect()
}

#[test]
fn engine_split_matches_the_bench_steps() {
    let fixture = manifest("showdown-manifest.json");
    let manifest = split(fixture.clone(), &NEXTJS);
    let rendered = rendered_css(&plan(&fixture, &NEXTJS, &split_only()));
    let actual = serde_json::to_string_pretty(&serde_json::json!({
        "manifest": manifest,
        "rendered": rendered,
    }))
    .unwrap()
        + "\n";
    let golden = fixtures().join("golden/showdown-split.json");
    if std::env::var_os("SLIMTOKEN_BLESS").is_some() {
        std::fs::write(&golden, &actual).unwrap();
        return;
    }
    let expected = std::fs::read_to_string(&golden)
        .unwrap()
        .replace("\r\n", "\n");
    assert!(actual == expected, "differs from {}", golden.display());
    // The 27 classes of the style sheet go to 4 parts of 7, 7, 7 and 6
    // right after it; the style sheet only imports them.
    let paths: Vec<&str> = manifest
        .files
        .iter()
        .map(|file| file.path.as_str())
        .collect();
    let at = paths
        .iter()
        .position(|path| *path == "app/globals.css")
        .unwrap();
    assert_eq!(
        paths[at..at + 5],
        [
            "app/globals.css",
            "app/styles/part1.css",
            "app/styles/part2.css",
            "app/styles/part3.css",
            "app/styles/part4.css",
        ]
    );
    assert!(manifest.files[at].exports.is_empty());
    let sizes: Vec<usize> = manifest.files[at + 1..at + 5]
        .iter()
        .map(|file| file.exports.len())
        .collect();
    assert_eq!(sizes, [7, 7, 7, 6]);
    assert_eq!(
        rendered["app/globals.css"],
        "```css\n@import \"./styles/part1.css\";\n@import \"./styles/part2.css\";\n\
         @import \"./styles/part3.css\";\n@import \"./styles/part4.css\";\n```"
    );
    assert_eq!(rendered.len(), 1, "the parts are written by workers");
}

#[test]
fn split_is_idempotent_and_the_saved_manifest_plans_the_same() {
    let fixture = manifest("showdown-manifest.json");
    let once = split(fixture.clone(), &NEXTJS);
    let twice = split(once.clone(), &NEXTJS);
    assert_eq!(
        serde_json::to_string(&once).unwrap(),
        serde_json::to_string(&twice).unwrap()
    );
    let from_fixture = plan(&fixture, &NEXTJS, &split_only());
    let from_saved = plan(&once, &NEXTJS, &split_only());
    assert_eq!(
        serde_json::to_string(&from_fixture).unwrap(),
        serde_json::to_string(&from_saved).unwrap()
    );
}

#[test]
fn fewer_than_fourteen_classes_are_not_split() {
    let mut short = manifest("showdown-manifest.json");
    let css = short
        .files
        .iter_mut()
        .find(|file| file.path == "app/globals.css")
        .unwrap();
    css.exports.truncate(13);
    let before = serde_json::to_string(&short).unwrap();
    let after = split(short.clone(), &NEXTJS);
    assert_eq!(serde_json::to_string(&after).unwrap(), before);
    assert!(rendered_css(&plan(&short, &NEXTJS, &split_only())).is_empty());

    // Fourteen: two parts of seven.
    let mut least = manifest("showdown-manifest.json");
    let css = least
        .files
        .iter_mut()
        .find(|file| file.path == "app/globals.css")
        .unwrap();
    css.exports.truncate(14);
    let after = split(least, &NEXTJS);
    let parts: Vec<usize> = after
        .files
        .iter()
        .filter(|file| file.path.starts_with("app/styles/part"))
        .map(|file| file.exports.len())
        .collect();
    assert_eq!(parts, [7, 7]);
}

#[test]
fn split_off_or_without_split_data_changes_nothing() {
    let fixture = manifest("showdown-manifest.json");
    let default = plan(&fixture, &NEXTJS, &PlanOptions::default());
    assert!(rendered_css(&default).is_empty(), "off by default");
    // A manifest saved after the split, planned with the option off: the
    // joined file is written by a worker, as before the option existed.
    let saved = split(fixture.clone(), &NEXTJS);
    assert!(rendered_css(&plan(&saved, &NEXTJS, &PlanOptions::default())).is_empty());
    // PYTHON and RUST have no split data and no aggregator.
    for profile in [&PYTHON, &RUST] {
        let text = serde_json::to_string(&fixture).unwrap();
        assert_eq!(
            serde_json::to_string(&split(fixture.clone(), profile)).unwrap(),
            text,
            "{}",
            profile.name
        );
        assert_eq!(
            serde_json::to_string(&plan(&fixture, profile, &split_only())).unwrap(),
            serde_json::to_string(&plan(&fixture, profile, &PlanOptions::default())).unwrap(),
            "{}",
            profile.name
        );
    }
}

/// Counts requests and fails each one.
struct NoModel {
    calls: Arc<Mutex<usize>>,
}

#[async_trait]
impl ModelClient for NoModel {
    async fn complete(&self, _: CompletionRequest) -> Result<Completion> {
        *self.calls.lock().unwrap() += 1;
        bail!("no model call expected")
    }
}

#[tokio::test]
async fn the_joined_style_sheet_makes_no_model_call() {
    let mut plan = plan(&manifest("showdown-manifest.json"), &NEXTJS, &split_only());
    let expected = rendered_css(&plan)["app/globals.css"].clone();
    plan.tasks.retain(|task| task.title == "app/globals.css");
    plan.tasks[0].deps.clear();
    plan.cache_primes.clear();
    let calls = Arc::new(Mutex::new(0));
    let client: Arc<dyn ModelClient> = Arc::new(NoModel {
        calls: Arc::clone(&calls),
    });
    let report = execute_plan(&plan, client, SchedulerConfig::default())
        .await
        .unwrap();
    assert!(report.is_success());
    assert_eq!(*calls.lock().unwrap(), 0);
    let record = report.tasks.values().next().unwrap();
    assert_eq!(record.output.as_deref(), Some(expected.as_str()));
    assert_eq!(record.output_tokens, 0);
}

/// A small NEXTJS manifest: a page and a style sheet at `path` with
/// `classes` class exports.
fn style_sheet(path: &str, classes: usize) -> Manifest {
    let exports: Vec<String> = (1..=classes).map(|n| format!(".class-{n}")).collect();
    serde_json::from_value(serde_json::json!({
        "files": [
            { "path": "app/page.tsx", "purpose": "The home page.", "exports": ["export default function Home()"] },
            { "path": path, "purpose": "The global style sheet.", "exports": exports },
        ]
    }))
    .unwrap()
}

/// The paths and export counts of a manifest's files.
fn shape(manifest: &Manifest) -> Vec<(String, usize)> {
    manifest
        .files
        .iter()
        .map(|file| (file.path.clone(), file.exports.len()))
        .collect()
}

#[test]
fn a_part_path_already_in_the_manifest_leaves_it_unchanged() {
    let mut taken = manifest("showdown-manifest.json");
    let mut part = taken.files[0].clone();
    part.path = "app/styles/part1.css".to_string();
    part.exports = vec![".kept".to_string()];
    taken.files.push(part);
    let before = serde_json::to_string(&taken).unwrap();
    assert_eq!(
        serde_json::to_string(&split(taken.clone(), &NEXTJS)).unwrap(),
        before
    );
    assert!(rendered_css(&plan(&taken, &NEXTJS, &split_only())).is_empty());
}

#[test]
fn a_root_style_sheet_gets_root_parts() {
    let manifest = split(style_sheet("globals.css", 14), &NEXTJS);
    let shape = shape(&manifest);
    assert_eq!(
        shape,
        [
            ("app/page.tsx".to_string(), 1),
            ("globals.css".to_string(), 0),
            ("styles/part1.css".to_string(), 7),
            ("styles/part2.css".to_string(), 7),
        ]
    );
    let rendered = rendered_css(&plan(
        &style_sheet("globals.css", 14),
        &NEXTJS,
        &split_only(),
    ));
    assert_eq!(
        rendered["globals.css"],
        "```css\n@import \"./styles/part1.css\";\n@import \"./styles/part2.css\";\n```"
    );
}

#[test]
fn forty_classes_give_five_parts_at_most() {
    let manifest = split(style_sheet("app/globals.css", 40), &NEXTJS);
    let parts: Vec<(String, usize)> = shape(&manifest)
        .into_iter()
        .filter(|(path, _)| path.starts_with("app/styles/part"))
        .collect();
    assert_eq!(
        parts,
        (1..=5)
            .map(|n| (format!("app/styles/part{n}.css"), 8))
            .collect::<Vec<_>>()
    );
}

/// Answers every model request with a fixed text and keeps each prompt.
struct Canned {
    prompts: Arc<Mutex<Vec<String>>>,
}

#[async_trait]
impl ModelClient for Canned {
    async fn complete(&self, request: CompletionRequest) -> Result<Completion> {
        self.prompts.lock().unwrap().push(request.user);
        Ok(Completion {
            text: "```tsx\nexport {};\n```".to_string(),
            provider: None,
            input_tokens: 10,
            output_tokens: 5,
            reasoning_tokens: None,
            cached_input_tokens: None,
            finish_reason: Some("stop".to_string()),
            usage_is_estimated: false,
            elapsed: std::time::Duration::from_millis(1),
            time_to_first_token: None,
        })
    }
}

/// Answers a style sheet part with one rule named after the part, and every
/// other request with an empty module.
struct CssWorker;

#[async_trait]
impl ModelClient for CssWorker {
    async fn complete(&self, request: CompletionRequest) -> Result<Completion> {
        let path = request
            .user
            .split("Now write the COMPLETE contents of `")
            .nth(1)
            .and_then(|rest| rest.split('`').next())
            .unwrap_or_default()
            .to_string();
        let text = if path.ends_with(".css") {
            let name = path.rsplit('/').next().unwrap().trim_end_matches(".css");
            format!("```css\n.{name} {{ color: red; }}\n```")
        } else {
            "```tsx\nexport {};\n```".to_string()
        };
        Ok(Completion {
            text,
            provider: None,
            input_tokens: 10,
            output_tokens: 5,
            reasoning_tokens: None,
            cached_input_tokens: None,
            finish_reason: Some("stop".to_string()),
            usage_is_estimated: false,
            elapsed: std::time::Duration::from_millis(1),
            time_to_first_token: None,
        })
    }
}

#[tokio::test]
async fn the_build_delivers_one_style_sheet_with_the_parts_joined() {
    let options = PlanOptions {
        split: true,
        staged: true,
        ..PlanOptions::default()
    };
    let plan = plan(&manifest("showdown-manifest.json"), &NEXTJS, &options);
    let parts = plan
        .tasks
        .iter()
        .filter(|task| task.title.starts_with("app/styles/part"))
        .count();
    assert_eq!(parts, 4);
    let dir = std::env::temp_dir().join(format!("slimtoken-css-join-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let outcome = slimtoken::pipeline::build(
        OBJECTIVE,
        Arc::new(CssWorker),
        slimtoken::pipeline::BuildOptions {
            source: slimtoken::pipeline::PlanSource::Given(plan),
            out_dir: Some(dir.clone()),
            ..slimtoken::pipeline::BuildOptions::default()
        },
    )
    .await
    .unwrap();
    let sheet = std::fs::read_to_string(dir.join("app/globals.css")).unwrap();
    assert!(!dir.join("app/styles").exists(), "no part files on disk");
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(
        sheet,
        ".part1 { color: red; }\n\n.part2 { color: red; }\n\n.part3 { color: red; }\n\n\
         .part4 { color: red; }\n"
    );
    assert!(outcome
        .files
        .iter()
        .all(|file| !file.path.contains("styles/part")));
    assert!(outcome
        .plan
        .tasks
        .iter()
        .all(|task| !task.title.contains("styles/part")));
    // The joined file is a repairable task now, and the bill of the parts
    // stays in the report.
    let task = outcome
        .plan
        .tasks
        .iter()
        .find(|task| task.title == "app/globals.css")
        .unwrap();
    assert!(task.static_output.is_none());
    let report = outcome.report.unwrap();
    assert!(report.is_success());
    assert_eq!(report.tasks[&task.id].output_tokens, 20);
    outcome.plan.validate().unwrap();
}

#[tokio::test]
async fn the_joined_style_sheet_is_rendered_inside_a_full_staged_plan() {
    let options = PlanOptions {
        split: true,
        staged: true,
        ..PlanOptions::default()
    };
    let mut plan = plan(&manifest("showdown-manifest.json"), &NEXTJS, &options);
    let expected = rendered_css(&plan)["app/globals.css"].clone();
    // The staged plan starts the joined file with the first wave (no deps);
    // it also waits for its first part here, so a rendered task with a
    // model-written dep runs in the same plan.
    let part = plan
        .tasks
        .iter()
        .find(|task| task.title == "app/styles/part1.css")
        .unwrap()
        .id
        .clone();
    let task = plan
        .tasks
        .iter_mut()
        .find(|task| task.title == "app/globals.css")
        .unwrap();
    task.deps.push(part);
    let id = task.id.clone();
    let prompts = Arc::new(Mutex::new(Vec::new()));
    let client: Arc<dyn ModelClient> = Arc::new(Canned {
        prompts: Arc::clone(&prompts),
    });
    let report = execute_plan(&plan, client, SchedulerConfig::default())
        .await
        .unwrap();
    assert!(report.is_success());
    let record = &report.tasks[&id];
    assert_eq!(record.output.as_deref(), Some(expected.as_str()));
    assert_eq!(record.output_tokens, 0);
    let prompts = prompts.lock().unwrap();
    assert!(!prompts.is_empty(), "the other tasks call the model");
    assert!(
        !prompts
            .iter()
            .any(|prompt| prompt.contains("Title: app/globals.css\n")),
        "no request for the joined style sheet"
    );
}
