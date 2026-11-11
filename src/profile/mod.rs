//! Stack profiles: everything the architect knows about one target stack.
//!
//! The core (contracts first, one parallel wave, sharded suites, inline
//! repair) is stack-agnostic. What differs per stack — prompt rules, contract
//! syntax, which files a suite can test, rendered toolchain files, manifest
//! fixups — lives in a `Profile`. A harness built on slimtoken can register its
//! own profile instead of editing the core.

mod nextjs;
mod python;
mod rust;

use std::path::Path;

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};

use crate::architect::Manifest;

pub use nextjs::NEXTJS;
pub use python::PYTHON;
pub use rust::RUST;

/// One target stack. Every field is plain data or a plain function so a
/// profile can be declared as a `static` by any crate.
pub struct Profile {
    /// Stable name (`--profile`).
    pub name: &'static str,
    /// Whole-word objective markers that select this profile in `detect`.
    pub markers: &'static [&'static str],
    /// Line-comment token used for export annotations (`#`, `//`).
    pub comment: &'static str,
    /// Example module path, export signature and test path shown in the
    /// manifest JSON shape.
    pub example_module: &'static str,
    pub example_export: &'static str,
    pub example_test: &'static str,
    /// Test framework name in suite prompts ("Write a {suite} suite ...").
    pub suite: &'static str,
    /// Stack rules at the top of the architect's manifest rules.
    pub planner_rules: &'static str,
    /// Edge-behavior rule appended to export fragments (`ArchitectOptions::edges`).
    pub edge_rule: &'static str,
    /// Extra clause on the export-fragment line limit (e.g. stylesheets).
    pub fragment_note: &'static str,
    /// Source extensions an objective's numbered module list may name for
    /// the skeleton to be taken from it (empty: always ask the model).
    pub listed_module_exts: &'static [&'static str],
    /// Module-worker head; `{all}` is replaced by the program contracts.
    pub module_head: &'static str,
    /// Test-worker head; `{all}`, `{visibility}` and `{silent}` are replaced.
    pub test_head: &'static str,
    /// Extra density clause appended to BOTH heads with `--architect-density`
    /// (A/B ): bans docstrings and non-contract comments. Empty = the
    /// profile's built-in compact-style rule is the only density guidance.
    pub density_rule: &'static str,
    /// Shared-type ownership rule appended to the manifest rules with
    /// `--architect-type-ownership` (A/B ): each interface/type is
    /// defined exactly once and imported by relative path everywhere else.
    /// Empty = the profile has no such class to pin (e.g. Python dict-shaped
    /// contracts).
    pub type_ownership_rule: &'static str,
    /// Paths whose artifacts the run validates in-run (syntax checks).
    pub validates: fn(&str) -> bool,
    /// Files a generated suite can exercise.
    pub is_testable: fn(&str) -> bool,
    /// Files that call other files' exports to present features (entry
    /// points, pages, components); they wait for the in-wave wiring task.
    pub is_consumer: fn(&str) -> bool,
    /// Path of test shard `index` whose first module is `module`.
    pub shard_path: fn(usize, &str) -> String,
    /// Rendered (not generated) file contents, fenced like a worker's output.
    pub scaffold: fn(&str) -> Option<String>,
    /// Fixups on the skeleton before export contracts are written.
    pub normalize_skeleton: fn(&mut Manifest),
    /// Fixups on the full manifest before the plan is built.
    pub normalize_manifest: fn(&mut Manifest),
    /// README "Run" and "Tests" sections for the rendered README.
    pub readme_sections: fn(&Manifest) -> String,
}

/// Built-in profiles; the first is the fallback when no markers match.
pub static PROFILES: &[&Profile] = &[&PYTHON, &NEXTJS, &RUST];

// file-loaded profiles form a process-wide library. Loaded once at
// startup (CLI `--profile-dir`, embedded hosts call `load_dir`), consulted
// by `named` (a file profile may override a built-in name — the host owns
// the library) and by `detect` (file markers win over built-in markers).
static FILE_PROFILES: std::sync::OnceLock<std::sync::Mutex<Vec<&'static Profile>>> =
    std::sync::OnceLock::new();

fn file_profiles() -> &'static std::sync::Mutex<Vec<&'static Profile>> {
    FILE_PROFILES.get_or_init(|| std::sync::Mutex::new(Vec::new()))
}

impl Profile {
    /// The profile whose markers appear in the objective as whole words,
    /// else the fallback. A bare substring test sent "reaction time" (memory
    /// objective) down the TypeScript profile. File-library profiles are
    /// consulted before built-ins; the fallback stays the first built-in.
    pub fn detect(objective: &str) -> &'static Profile {
        let lower = objective.to_lowercase();
        if let Ok(library) = file_profiles().lock() {
            if let Some(profile) = library.iter().find(|profile| {
                profile
                    .markers
                    .iter()
                    .any(|marker| has_marker(&lower, marker))
            }) {
                return profile;
            }
        }
        PROFILES
            .iter()
            .skip(1)
            .find(|profile| {
                profile
                    .markers
                    .iter()
                    .any(|marker| has_marker(&lower, marker))
            })
            .copied()
            .unwrap_or(PROFILES[0])
    }

    /// A profile by name: the file library first (a host redefining a name
    /// knows what it is doing), then the built-ins.
    pub fn named(name: &str) -> Option<&'static Profile> {
        if let Ok(library) = file_profiles().lock() {
            if let Some(profile) = library.iter().rev().find(|profile| profile.name == name) {
                return Some(profile);
            }
        }
        PROFILES
            .iter()
            .find(|profile| profile.name == name)
            .copied()
    }

    /// Load every `*.json` profile in `dir` into the file library.
    /// Files load in sorted path order, so a library is deterministic; a name
    /// loaded later wins in `named`. Behavior hooks are still inherited from
    /// each file's built-in `base` (see [`Profile::load_file`]). Returns the
    /// number of profiles loaded; a dir with no JSON files loads zero.
    pub fn load_dir(dir: &Path) -> Result<usize> {
        let mut paths: Vec<std::path::PathBuf> = std::fs::read_dir(dir)
            .with_context(|| format!("cannot read profile directory {}", dir.display()))?
            .filter_map(|entry| entry.ok().map(|entry| entry.path()))
            .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
            .collect();
        paths.sort();
        // Load without holding the lock: `load_file` resolves its base via
        // `named`, which re-locks (a std Mutex is not reentrant).
        let mut loaded = Vec::with_capacity(paths.len());
        for path in &paths {
            loaded.push(
                Profile::load_file(path)
                    .with_context(|| format!("failed to load profile {}", path.display()))?,
            );
        }
        file_profiles()
            .lock()
            .map_err(|_| anyhow!("profile library lock poisoned"))?
            .extend(loaded);
        Ok(paths.len())
    }
}

impl std::fmt::Debug for Profile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Profile").field("name", &self.name).finish()
    }
}

/// Data-only profile file. A host stack profile loads from JSON
/// without recompiling: behavior hooks (validators, scaffold, fixups) are
/// inherited from the built-in `base` profile, the string fields below
/// override the base's strings, and omitted fields keep the base value.
#[derive(Debug, Deserialize)]
pub struct ProfileFile {
    /// Built-in profile whose behavior hooks and defaults this file extends.
    pub base: String,
    /// Profile name; defaults to the file stem.
    pub name: Option<String>,
    pub markers: Option<Vec<String>>,
    pub comment: Option<String>,
    pub example_module: Option<String>,
    pub example_export: Option<String>,
    pub example_test: Option<String>,
    pub suite: Option<String>,
    pub planner_rules: Option<String>,
    pub edge_rule: Option<String>,
    pub fragment_note: Option<String>,
    pub listed_module_exts: Option<Vec<String>>,
    pub module_head: Option<String>,
    pub test_head: Option<String>,
    pub density_rule: Option<String>,
    pub type_ownership_rule: Option<String>,
}

impl Profile {
    /// Load a host profile from a JSON [`ProfileFile`]. The returned
    /// profile lives for the rest of the process: owned strings are leased to
    /// 'static — a few KB, and callers (CLI, server) load once at startup.
    pub fn load_file(path: &Path) -> Result<&'static Profile> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("cannot read profile file {}", path.display()))?;
        let file: ProfileFile = serde_json::from_str(&text)
            .with_context(|| format!("invalid profile file {}", path.display()))?;
        let base = Profile::named(&file.base).ok_or_else(|| {
            let names: Vec<_> = PROFILES.iter().map(|profile| profile.name).collect();
            anyhow!(
                "unknown base profile {:?} (choose one of: {})",
                file.base,
                names.join(", ")
            )
        })?;
        /// Lease an owned string to 'static (see `load_file`).
        fn lease(value: String) -> &'static str {
            Box::leak(value.into_boxed_str())
        }
        /// Lease an owned string list to 'static (see `load_file`).
        fn lease_list(values: Vec<String>) -> &'static [&'static str] {
            let leased: Vec<&'static str> = values.into_iter().map(lease).collect();
            Box::leak(leased.into_boxed_slice())
        }
        let name = file.name.unwrap_or_else(|| {
            path.file_stem()
                .map(|stem| stem.to_string_lossy().into_owned())
                .unwrap_or_else(|| "file".to_string())
        });
        let profile = Profile {
            name: lease(name),
            markers: file.markers.map(lease_list).unwrap_or(base.markers),
            comment: file.comment.map(lease).unwrap_or(base.comment),
            example_module: file
                .example_module
                .map(lease)
                .unwrap_or(base.example_module),
            example_export: file
                .example_export
                .map(lease)
                .unwrap_or(base.example_export),
            example_test: file.example_test.map(lease).unwrap_or(base.example_test),
            suite: file.suite.map(lease).unwrap_or(base.suite),
            planner_rules: file.planner_rules.map(lease).unwrap_or(base.planner_rules),
            edge_rule: file.edge_rule.map(lease).unwrap_or(base.edge_rule),
            fragment_note: file.fragment_note.map(lease).unwrap_or(base.fragment_note),
            listed_module_exts: file
                .listed_module_exts
                .map(lease_list)
                .unwrap_or(base.listed_module_exts),
            module_head: file.module_head.map(lease).unwrap_or(base.module_head),
            test_head: file.test_head.map(lease).unwrap_or(base.test_head),
            density_rule: file.density_rule.map(lease).unwrap_or(base.density_rule),
            type_ownership_rule: file
                .type_ownership_rule
                .map(lease)
                .unwrap_or(base.type_ownership_rule),
            // Behavior hooks are code, not data: inherit the base profile's.
            validates: base.validates,
            is_testable: base.is_testable,
            is_consumer: base.is_consumer,
            shard_path: base.shard_path,
            scaffold: base.scaffold,
            normalize_skeleton: base.normalize_skeleton,
            normalize_manifest: base.normalize_manifest,
            readme_sections: base.readme_sections,
        };
        Ok(Box::leak(Box::new(profile)))
    }
}

/// Max_tokens right-sizing profile, data-only JSON. A plan task's
/// `max_tokens` is its output-budget reserve: under TPM pacing the
/// admission bucket reserves it per task, so a plan that over-budgets inflates
/// pacing waits, and the priority queue ranks ready tasks by it. Offline trace
/// analysis (8,909 succeeded tasks): p50 output is 426 tokens, 89.4% fit in
/// 2048, 99.1% in 4096 — most plans carry 2-8x headroom. A profile caps
/// budgets from that evidence; it can only SHRINK a task's budget, never grow
/// it, so a task pinned low by its plan author is never raised.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct MaxTokensProfile {
    /// Budget for tasks that match no entry. Default 1024 (the plan-file
    /// default and the observed p70 fit).
    #[serde(default = "default_max_tokens_cap")]
    pub default_cap: u32,
    /// Per-class caps, first match wins; `title_contains` is a
    /// case-insensitive substring of the task title.
    #[serde(default)]
    pub caps: Vec<MaxTokensCap>,
}

fn default_max_tokens_cap() -> u32 {
    1024
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct MaxTokensCap {
    /// Case-insensitive substring matched against the task title.
    pub title_contains: String,
    pub max_tokens: u32,
}

impl MaxTokensProfile {
    /// Load a max_tokens profile from JSON, e.g.
    /// `{"default_cap": 768, "caps": [{"title_contains": "integration",
    /// "max_tokens": 2048}]}`.
    pub fn load_file(path: &Path) -> Result<MaxTokensProfile> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("cannot read max-tokens profile file {}", path.display()))?;
        serde_json::from_str(&text)
            .with_context(|| format!("invalid max-tokens profile file {}", path.display()))
    }

    /// The budget cap for a task title: the first entry whose substring
    /// matches (case-insensitive), else `default_cap`. Callers apply it as
    /// `task.max_tokens.min(cap)` — a profile never raises a budget.
    pub fn cap_for(&self, title: &str) -> u32 {
        let lower = title.to_lowercase();
        self.caps
            .iter()
            .find(|entry| lower.contains(&entry.title_contains.to_lowercase()))
            .map(|entry| entry.max_tokens)
            .unwrap_or(self.default_cap)
    }
}

#[cfg(test)]
mod file_profile_tests {
    use super::*;

    fn write_temp(name: &str, json: &str) -> std::path::PathBuf {
        static SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "slimtoken-profile-{name}-{}-{seq}.json",
            std::process::id()
        ));
        std::fs::write(&path, json).unwrap();
        path
    }

    #[test]
    fn file_profile_overrides_strings_and_inherits_behavior() {
        let path = write_temp(
            "override",
            r#"{"base":"python","suite":"unittest","comment":"//",
                "planner_rules":"- Custom rule."}"#,
        );
        let profile = Profile::load_file(&path).unwrap();
        assert_eq!(profile.suite, "unittest", "override wins");
        assert_eq!(profile.comment, "//", "override wins");
        assert_eq!(profile.planner_rules, "- Custom rule.");
        let stem = path.file_stem().unwrap().to_string_lossy().into_owned();
        assert_eq!(profile.name, stem, "file stem is the default name");
        assert_eq!(
            profile.example_module, PYTHON.example_module,
            "base default kept"
        );
        // Behavior hooks come from the python base, not from the file.
        assert!((profile.validates)("app/foo.py"));
        assert!(!(profile.validates)("app/foo.ts"));
        assert!((profile.is_testable)("anything"));
        std::fs::remove_file(path).ok();
    }

    #[test]
    fn file_profile_explicit_name_and_markers() {
        let path = write_temp(
            "named",
            r#"{"base":"nextjs","name":"acme","markers":["acme","acmejs"]}"#,
        );
        let profile = Profile::load_file(&path).unwrap();
        assert_eq!(profile.name, "acme");
        assert_eq!(profile.markers, ["acme", "acmejs"].as_slice());
        std::fs::remove_file(path).ok();
    }

    #[test]
    fn file_profile_rejects_unknown_base() {
        let path = write_temp("badbase", r#"{"base":"cobol"}"#);
        assert!(Profile::load_file(&path).is_err());
        std::fs::remove_file(path).ok();
    }
}

fn has_marker(lower: &str, marker: &str) -> bool {
    lower.match_indices(marker).any(|(start, _)| {
        let end = start + marker.len();
        let before = lower[..start].chars().next_back();
        let after = lower[end..].chars().next();
        // Extension markers (".tsx") follow a file stem.
        (marker.starts_with('.') || !before.is_some_and(|ch| ch.is_alphanumeric()))
            && !after.is_some_and(|ch| ch.is_alphanumeric())
    })
}

fn keep(_: &mut Manifest) {}

/// Duration-prediction profile, data-only JSON. The ready queue's
/// tie-break after critical-path depth is "largest first", keyed on the
/// task's max_tokens budget. Measured on 8,398 succeeded tasks: duration
/// correlates with output tokens at spearman 0.877, but budgets are coarse
/// (architect plans use 3 buckets), so same-budget tasks still span ~100x
/// in duration (p50 3.4s, p90 10.7s). A duration profile replaces the
/// budget key with a predicted duration per task class, so the wave's
/// longest tasks are admitted first even when their budgets tie.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DurationProfile {
    /// Prediction for titles matching no entry. Default 3000 ms (the
    /// observed per-task duration median).
    #[serde(default = "default_predicted_ms")]
    pub default_ms: u64,
    /// Per-class predictions, first match wins; `title_contains` is a
    /// case-insensitive substring of the task title.
    #[serde(default)]
    pub entries: Vec<DurationEntry>,
}

fn default_predicted_ms() -> u64 {
    3000
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DurationEntry {
    /// Case-insensitive substring matched against the task title.
    pub title_contains: String,
    pub predicted_ms: u64,
}

impl DurationProfile {
    /// Load a duration profile from JSON, e.g.
    /// `{"default_ms": 3000, "entries": [{"title_contains": "test shard",
    /// "predicted_ms": 18000}]}`.
    pub fn load_file(path: &Path) -> Result<DurationProfile> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("cannot read duration profile file {}", path.display()))?;
        serde_json::from_str(&text)
            .with_context(|| format!("invalid duration profile file {}", path.display()))
    }

    /// The predicted duration for a task title: the first entry whose
    /// substring matches (case-insensitive), else `default_ms`.
    pub fn predict(&self, title: &str) -> u64 {
        let lower = title.to_lowercase();
        self.entries
            .iter()
            .find(|entry| lower.contains(&entry.title_contains.to_lowercase()))
            .map(|entry| entry.predicted_ms)
            .unwrap_or(self.default_ms)
    }
}

#[cfg(test)]
mod duration_profile_tests {
    use super::*;

    fn write_temp(name: &str, json: &str) -> std::path::PathBuf {
        static SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "slimtoken-duration-{name}-{}-{seq}.json",
            std::process::id()
        ));
        std::fs::write(&path, json).unwrap();
        path
    }

    #[test]
    fn predict_defaults_to_3000ms_and_first_match_wins_case_insensitively() {
        let profile = DurationProfile {
            default_ms: 1000,
            entries: vec![
                DurationEntry {
                    title_contains: "Test Shard".to_string(),
                    predicted_ms: 18_000,
                },
                DurationEntry {
                    title_contains: "integration".to_string(),
                    predicted_ms: 9_000,
                },
            ],
        };
        assert_eq!(profile.predict("Write the TEST SHARD suite"), 18_000);
        assert_eq!(profile.predict("Final Integration assembly"), 9_000);
        assert_eq!(profile.predict("A module"), 1000, "default");
    }

    #[test]
    fn load_file_round_trips_and_missing_fields_use_defaults() {
        let path = write_temp(
            "full",
            r#"{"default_ms": 5000,
                "entries": [{"title_contains": "test", "predicted_ms": 12000}]}"#,
        );
        let profile = DurationProfile::load_file(&path).unwrap();
        assert_eq!(profile.default_ms, 5000);
        assert_eq!(profile.predict("test"), 12_000);
        assert_eq!(profile.predict("other"), 5000);
        std::fs::remove_file(path).ok();

        let path = write_temp("bare", r#"{}"#);
        let profile = DurationProfile::load_file(&path).unwrap();
        assert_eq!(profile.default_ms, 3000, "documented default");
        assert!(profile.entries.is_empty());
        std::fs::remove_file(path).ok();
    }

    #[test]
    fn load_file_rejects_missing_file_and_bad_json() {
        let missing = std::path::Path::new("definitely-not-a-real-file-p4d.json");
        assert!(DurationProfile::load_file(missing).is_err());
        let path = write_temp("badjson", r#"{"default_ms": "ages"}"#);
        assert!(DurationProfile::load_file(&path).is_err());
        std::fs::remove_file(path).ok();
    }
}

#[cfg(test)]
mod max_tokens_profile_tests {
    use super::*;

    fn write_temp(name: &str, json: &str) -> std::path::PathBuf {
        static SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "slimtoken-maxtok-{name}-{}-{seq}.json",
            std::process::id()
        ));
        std::fs::write(&path, json).unwrap();
        path
    }

    #[test]
    fn cap_defaults_to_1024_and_matches_first_entry_case_insensitively() {
        let profile = MaxTokensProfile {
            default_cap: 768,
            caps: vec![
                MaxTokensCap {
                    title_contains: "Integration".to_string(),
                    max_tokens: 2048,
                },
                MaxTokensCap {
                    title_contains: "test".to_string(),
                    max_tokens: 512,
                },
            ],
        };
        assert_eq!(profile.cap_for("Final INTEGRATION assembly"), 2048);
        assert_eq!(profile.cap_for("Write the Unit Test shard"), 512);
        assert_eq!(profile.cap_for("Reverse words"), 768, "default cap");
    }

    #[test]
    fn load_file_parses_caps_and_missing_fields_use_defaults() {
        let path = write_temp(
            "full",
            r#"{"default_cap": 640,
                "caps": [{"title_contains": "integration", "max_tokens": 2048}]}"#,
        );
        let profile = MaxTokensProfile::load_file(&path).unwrap();
        assert_eq!(profile.default_cap, 640);
        assert_eq!(profile.cap_for("integration"), 2048);
        std::fs::remove_file(path).ok();

        let path = write_temp("bare", r#"{}"#);
        let profile = MaxTokensProfile::load_file(&path).unwrap();
        assert_eq!(profile.default_cap, 1024, "documented default");
        assert!(profile.caps.is_empty());
        assert_eq!(profile.cap_for("anything"), 1024);
        std::fs::remove_file(path).ok();
    }

    #[test]
    fn load_file_rejects_missing_file_and_bad_json() {
        let missing = std::path::Path::new("definitely-not-a-real-file-p4.json");
        assert!(MaxTokensProfile::load_file(missing).is_err());
        let path = write_temp("badjson", r#"{"default_cap": "lots"}"#);
        assert!(MaxTokensProfile::load_file(&path).is_err());
        std::fs::remove_file(path).ok();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markers_match_whole_words_only() {
        let name = |objective| Profile::detect(objective).name;
        assert_eq!(
            name("track accuracy and reaction time in a Python CLI"),
            "python"
        );
        assert_eq!(name("a React dashboard"), "nextjs");
        assert_eq!(name("Next.js (App Router, TypeScript)"), "nextjs");
        assert_eq!(name("pages in app/page.tsx"), "nextjs");
        assert_eq!(name("Build a CLI with tests."), "python");
    }

    #[test]
    fn profiles_resolve_by_name() {
        assert_eq!(Profile::named("nextjs").map(|p| p.name), Some("nextjs"));
        assert!(Profile::named("cobol").is_none());
    }

    #[test]
    fn rust_profile_detects_and_shards() {
        let name = |objective| Profile::detect(objective).name;
        assert_eq!(name("Build a Rust CLI that summarizes logs"), "rust");
        assert_eq!(name("a cargo workspace tool"), "rust");
        // Whole-word: "trust"/"crust" must not select rust.
        assert_eq!(name("a trusted Python ledger"), "python");
        assert_eq!((RUST.shard_path)(1, "src/parse.rs"), "tests/01_parse.rs");
        assert_eq!((RUST.shard_path)(2, "src/report.rs"), "tests/02_report.rs");
        assert_eq!(RUST.name, "rust");
    }

    #[test]
    fn rust_scaffold_renders_cargo_toml_only() {
        let toml = (RUST.scaffold)("Cargo.toml").expect("Cargo.toml rendered");
        assert!(toml.contains("name = \"app\""), "crate name pinned");
        assert!(toml.contains("edition = \"2021\""));
        assert!((RUST.scaffold)("src/main.rs").is_none());
        assert!((RUST.scaffold)("package.json").is_none());
        assert!(!(RUST.validates)("src/main.rs"), "no in-run rust validator");
        assert!((RUST.is_consumer)("src/lib.rs"), "lib.rs waits for wiring");
        assert!((RUST.is_consumer)("src/main.rs"));
        assert!(!(RUST.is_consumer)("src/parse.rs"));
    }

    // the file-loaded library. Names and markers here are unique on
    // purpose: the registry is process-wide, so a test profile must never
    // shadow a built-in name or marker another test relies on.
    #[test]
    fn load_dir_registers_profiles_for_named_and_detect() {
        let dir =
            std::env::temp_dir().join(format!("slimtoken-profile-dir-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("fastapi-ish.json"),
            r#"{"base": "python", "name": "filelibfastapi",
                "markers": ["filelibfastapi", "filelibuvicorn"]}"#,
        )
        .unwrap();
        std::fs::write(dir.join("nested-ignored.txt"), "not a profile").unwrap();
        let loaded = Profile::load_dir(&dir).unwrap();
        assert_eq!(loaded, 1, "only *.json files load");
        let profile = Profile::named("filelibfastapi").expect("library profile by name");
        assert_eq!(profile.comment, "#", "strings inherited from base python");
        assert_eq!(profile.suite, "pytest");
        // File markers win over built-ins in detect.
        assert_eq!(
            Profile::detect("build a filelibfastapi service").name,
            "filelibfastapi"
        );
        // Inherited hooks come from the base profile.
        assert!((profile.validates)("app/main.py"));
        // Missing dir and empty dir.
        assert!(Profile::load_dir(&dir.join("missing")).is_err());
        let empty = dir.join("empty");
        std::fs::create_dir_all(&empty).unwrap();
        assert_eq!(Profile::load_dir(&empty).unwrap(), 0);
        std::fs::remove_dir_all(&dir).ok();
    }
}
