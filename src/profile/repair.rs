//! Repair hooks: what the multi-round check-and-repair loop
//! (`repair::check_and_repair`) needs to know about a stack. Plain `fn`
//! pointers and static text, like the rest of a [`Profile`](super::Profile);
//! a profile file inherits them from its base.
//!
//! With [`NO_REPAIR_HOOKS`] the loop repairs one file at a time: no file is
//! a test, no import is followed, no joint repair and no new file is
//! written. A stack that knows its imports (NEXTJS) can do more.

use std::collections::{BTreeMap, BTreeSet};

use crate::dag::Plan;
use crate::scheduler::RunReport;

/// The kind of one line of checker output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Problem {
    /// The app does not render (a page that fails outside itself, as with
    /// a provider that the layout does not set up): its own repair rarely
    /// helps, so the file gets a joint repair at once.
    Render,
    /// An import of a module that is not there. A file whose only problems
    /// are such lines needs no repair while the missing file is written.
    MissingModule,
    /// Any other line.
    Other,
}

/// The stack-specific parts of the repair loop.
#[derive(Debug, Clone, Copy)]
pub struct RepairHooks {
    /// The kind of one line of checker output.
    pub classify: fn(&str) -> Problem,
    /// Test files: they get the test rules and, when stuck, the stuck-test
    /// repair.
    pub is_test: fn(&str) -> bool,
    /// Files that hold the shared types: every repair sees them.
    pub is_shared_types: fn(&str) -> bool,
    /// Files that a new file (a missing module) sees besides the importer's
    /// imports.
    pub is_shared: fn(&str) -> bool,
    /// The import specifiers in a source that point into the project.
    pub local_specifiers: fn(&str) -> Vec<String>,
    /// The project file that a specifier (imported from a path) names, among
    /// the known paths.
    pub resolve_import: fn(&str, &str, &BTreeSet<&str>) -> Option<String>,
    /// The path of the file that a missing import of a path would be; `None`
    /// when no file should be written for it.
    pub missing_module_path: fn(&str, &str) -> Option<String>,
    /// The files that a joint repair of a path (with its problems, given the
    /// project files) may change, the path first. Empty: no joint repair;
    /// the file is repaired alone again.
    pub joint_editable: fn(&str, &str, &BTreeMap<&str, &str>) -> Vec<String>,
    /// Stack rules added to the joint repair prompt (ends with a space, or
    /// empty).
    pub joint_rules: &'static str,
    /// The end of the sentence that asks for a missing module (starts with
    /// `,` or is `.`).
    pub new_file_rule: &'static str,
    /// Fixes that need no model, run on the files before each check.
    /// Returns the number of changes.
    pub prepare: fn(&Plan, &mut RunReport) -> usize,
    /// What the guards of the loop (`RepairPolicy::guard`) read in a source;
    /// [`NO_GUARD_HOOKS`] finds nothing, so the guards do nothing.
    pub guard: GuardHooks,
}

/// One exported declaration of a source: the exported name (`default` for
/// the default export) and its declaration text with the whitespace
/// collapsed (a signature, or a whole `interface` / `type` body).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Export {
    pub name: String,
    pub decl: String,
}

/// The source reading that the repair guards need (see
/// `repair::RepairPolicy::guard`): exports, the names an import takes, the
/// checker lines about a missing export, and the text fixes that need no
/// model. Plain `fn` pointers, like the rest of [`RepairHooks`].
#[derive(Debug, Clone, Copy)]
pub struct GuardHooks {
    /// The exported declarations of a source.
    pub exports: fn(&str) -> Vec<Export>,
    /// The names that a source imports with the specifier given: (exported
    /// name, local name); a default import is (`default`, its local name).
    pub imported_names: fn(&str, &str) -> Vec<(String, String)>,
    /// A checker line that says that a module does not export a name: (the
    /// specifier as the line writes it, the name).
    pub missing_export: fn(&str) -> Option<(String, String)>,
    /// A checker line that is often a knock-on of a problem in an imported
    /// file (TS7006, an implicit `any`).
    pub knock_on: fn(&str) -> bool,
    /// `Some(exported)` if a source declares a name at its top level.
    pub declares: fn(&str, &str) -> Option<bool>,
    /// The source with its top-level declaration of a name exported.
    pub add_export: fn(&str, &str) -> Option<String>,
    /// The source of the importer (a path) with a name imported from the
    /// file given (a path) instead of the specifier given.
    pub retarget: fn(&str, &str, &str, &str, &str) -> Option<String>,
    /// True if a path is a shared module (types, library code) whose
    /// declarations other files may import: the root-first guard points
    /// imports only at such a file, never at a page's or component's own
    /// declaration of the same name.
    pub shared_module: fn(&str) -> bool,
}

fn no_exports(_: &str) -> Vec<Export> {
    Vec::new()
}

fn no_names(_: &str, _: &str) -> Vec<(String, String)> {
    Vec::new()
}

fn no_missing(_: &str) -> Option<(String, String)> {
    None
}

fn no_declaration(_: &str, _: &str) -> Option<bool> {
    None
}

fn no_export_fix(_: &str, _: &str) -> Option<String> {
    None
}

fn no_retarget(_: &str, _: &str, _: &str, _: &str, _: &str) -> Option<String> {
    None
}

/// Guard hooks that read nothing: with them the guards change nothing.
pub const NO_GUARD_HOOKS: GuardHooks = GuardHooks {
    exports: no_exports,
    imported_names: no_names,
    missing_export: no_missing,
    knock_on: never,
    declares: no_declaration,
    add_export: no_export_fix,
    retarget: no_retarget,
    shared_module: never,
};

fn other(_: &str) -> Problem {
    Problem::Other
}

fn never(_: &str) -> bool {
    false
}

fn no_specifiers(_: &str) -> Vec<String> {
    Vec::new()
}

fn no_import(_: &str, _: &str, _: &BTreeSet<&str>) -> Option<String> {
    None
}

fn no_module(_: &str, _: &str) -> Option<String> {
    None
}

fn alone(_: &str, _: &str, _: &BTreeMap<&str, &str>) -> Vec<String> {
    Vec::new()
}

/// `RepairHooks::prepare` that changes nothing.
pub fn no_prepare(_: &Plan, _: &mut RunReport) -> usize {
    0
}

/// Single-file repairs only, and no file is a test (RUST; PYTHON adds its
/// tests in [`PYTHON_REPAIR_HOOKS`]).
pub const NO_REPAIR_HOOKS: RepairHooks = RepairHooks {
    classify: other,
    is_test: never,
    is_shared_types: never,
    is_shared: never,
    local_specifiers: no_specifiers,
    resolve_import: no_import,
    missing_module_path: no_module,
    joint_editable: alone,
    joint_rules: "",
    new_file_rule: ".",
    prepare: no_prepare,
    guard: NO_GUARD_HOOKS,
};

fn python_test(path: &str) -> bool {
    crate::reconcile::default_test_staging().is_test(path)
}

/// [`NO_REPAIR_HOOKS`] with Python test files (`tests/`, `test_*`, the
/// classification of `reconcile::default_test_staging`): they get the test
/// rules and the stuck-test repair (PYTHON).
pub const PYTHON_REPAIR_HOOKS: RepairHooks = RepairHooks {
    is_test: python_test,
    ..NO_REPAIR_HOOKS
};
