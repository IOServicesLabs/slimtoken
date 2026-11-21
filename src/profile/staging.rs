//! Staging data: how a profile orders the wave in stages (shared types
//! first, then core modules, then the files that use them). Plain data so a
//! profile file can override it. Nothing reads it unless
//! `PlanOptions::staged` is set (`architect::stage_tasks`) or a manifest
//! pass option asks for it (`architect::apply_manifest_passes`).

use serde::{Deserialize, Serialize};

use crate::architect::Manifest;

/// The stage a file is written in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Layer {
    /// Shared type declarations; written first, with no deps.
    Types,
    /// Long data files (seed records) that need the types only.
    Data,
    /// Core modules; they see the types.
    Core,
    /// Pages, components and the rest; they see the types and the core.
    Consumer,
}

/// A profile's staging data. All globs use [`glob_match`].
#[derive(Debug, Clone, Copy)]
pub struct Staging {
    /// Globs mapped to layers; the first match wins. A file with no match
    /// is not staged.
    pub layers: &'static [(&'static str, Layer)],
    /// Test files. A test file that would be Core is staged as a Consumer
    /// instead.
    pub test_globs: &'static [&'static str],
    /// Line prefixes that declare a shared type (`export interface `).
    /// Empty: `architect::dedupe_type_owners` does nothing.
    pub type_decl_prefixes: &'static [&'static str],
    /// Files outside the Types layer where a type that two of them declare
    /// is kept by the first and imported by the rest
    /// (`architect::first_type_owner_wins`). Empty: no such pass.
    pub type_owner_globs: &'static [&'static str],
    /// Added to every staged task's prompt.
    pub import_rule: &'static str,
    /// Added to a staged task that waits for earlier files.
    pub staged_rule: &'static str,
    /// Added to a Core task that is written after the types.
    pub types_win_rule: &'static str,
    /// Added to the purpose of Data files (`PlanOptions::tidy`).
    pub data_note: &'static str,
    /// Globs mapped to rules added to the prompt of a staged task; the
    /// first match wins, so list narrower globs (tests) first.
    pub path_rules: &'static [(&'static str, &'static str)],
    /// Staging has been measured for this stack, so a preset may turn it on.
    pub measured: bool,
    /// Long files that the plan writes in parts, each part by its own worker
    /// (`PlanOptions::split`, `architect::split_files`). Empty: no split.
    pub split: &'static [Split],
    /// The rendered text (fenced, as a worker gives it) of a file that only
    /// joins other files of the manifest, such as a file that the split left
    /// to import its parts; `None` for every other path. With
    /// `PlanOptions::split` the plan sets it as the task's `static_output`,
    /// so the file costs no model call. Code, so a profile file inherits it
    /// from its base.
    pub aggregate: fn(&Manifest, &str) -> Option<String>,
    /// Fixed quality rules and prompt texts (`PlanOptions::quality_rules`,
    /// `relative_dates`, `objective_pages`). `NO_QUALITY`: none.
    pub quality: Quality,
}

/// A profile's fixed quality texts. Each is used only when its plan option
/// is set, so with the default options plans do not change. Empty text (or
/// an empty glob): that option does nothing for the profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Quality {
    /// Added to the end of the module head (`PlanOptions::quality_rules`):
    /// one owner for each cross-file mechanism (theme, state, ids,
    /// settings).
    pub head_rule: &'static str,
    /// `(from, to)` text replacements in every purpose and export line
    /// (`PlanOptions::quality_rules`), after the CSS split, such as the dark
    /// theme selector.
    pub rewrites: &'static [(&'static str, &'static str)],
    /// Added to the purpose of Data-layer files in place of a fixed date
    /// (`PlanOptions::relative_dates`).
    pub date_note: &'static str,
    /// The route files (`PlanOptions::objective_pages`): each gets its bullet
    /// of the objective, the objective's quality bar and the route list.
    pub page_glob: &'static str,
    /// The component files (`PlanOptions::objective_pages`): each gets
    /// `component_rule`.
    pub component_glob: &'static str,
    /// Added to the prompt of each component file.
    pub component_rule: &'static str,
    /// Added to the end of the module head (`PlanOptions::objective_pages`),
    /// whose file list then also has each file's purpose.
    pub purposes_rule: &'static str,
    /// Added to the planner rules (`ArchitectOptions::contract`): the
    /// Types-layer file declares the app contract as code (unions with
    /// labels, catalogs with ids, the storage key, the whole app state, the
    /// settings with their defaults, meanings and readers), and each
    /// collection and UI control has one owner. Empty: no contract, and
    /// `contract::contract_problems` finds nothing for the profile.
    pub contract_rule: &'static str,
    /// The export limit of an export fragment with `ArchitectOptions::contract`,
    /// in place of "At most 4 exports per file" (more for the Types-layer
    /// file and for a module that owns a collection).
    pub contract_cap: &'static str,
    /// Added to the export fragment of a Types-layer file with
    /// `ArchitectOptions::contract`, before the list of every planned file
    /// (path: purpose), so the contract can name catalogs and readers.
    pub contract_types_note: &'static str,
    /// The name of the settings type (`Settings`) for the contract check and
    /// the reader lines (`PlanOptions::contract`). Empty: no settings checks.
    pub settings_type: &'static str,
    /// The name of the type that holds the whole app state (`AppState`) for
    /// the contract check. Empty: no state checks.
    pub state_type: &'static str,
    /// Added to the wiring prompt (`PlanOptions::wiring_rules`).
    pub wiring_rule: &'static str,
    /// Added to the planner rules (`ArchitectOptions::page_local`): a
    /// component that only one page renders goes in that page's file, and a
    /// shared component takes its data as props. Empty: `page_local` does
    /// nothing for the profile.
    pub page_local_rule: &'static str,
    /// The note that `pagelocal::fold_page_components` appends to the purpose
    /// of a page that takes in its private components
    /// (`PlanOptions::page_local`); `{components}` is replaced with the list
    /// of those components. The text before `{components}` marks the note,
    /// so `architect::planner_purpose` can leave it out.
    pub local_note: &'static str,
    /// Added to the prompt of each component file that is left after the
    /// fold and that no layout renders (`PlanOptions::page_local`): it takes
    /// its data as props and never owns the app state.
    pub shared_component_rule: &'static str,
    /// The layout files (`PlanOptions::page_local`): a component that one of
    /// them renders stays a file of its own.
    pub layout_glob: &'static str,
    /// Added to the planner rules (`ArchitectOptions::semantics`): the
    /// Types-layer file states the conventions that every file follows, as
    /// comments and constants (each money field's unit and sign, each date
    /// field's format, each status union's transitions and the one function
    /// that makes them), with one state owner and the display rules. Empty:
    /// no semantics, and `semantics::semantic_problems` finds nothing.
    pub semantics_rule: &'static str,
    /// In place of `page_local_rule` when `ArchitectOptions::semantics` is
    /// set too: the same rule, with the store provider as the only owner of
    /// the stored state. Empty: `page_local_rule` as it is.
    pub semantic_page_local_rule: &'static str,
    /// Added to the export fragment of a Types-layer file with
    /// `ArchitectOptions::semantics` (when `contract_types_note` is not
    /// already there), before the list of every planned file, so the
    /// conventions can name the owner of each status change.
    pub semantics_types_note: &'static str,
    /// The export limit of an export fragment with
    /// `ArchitectOptions::semantics` and without `contract` (with it,
    /// `contract_cap`).
    pub semantics_cap: &'static str,
    /// The fixed rules that end the conventions block of the module head
    /// (`PlanOptions::semantics`), after the conventions read from the plan.
    pub conventions_rule: &'static str,
    /// Added to the prompt of each component that a layout renders
    /// (`PlanOptions::semantics`): it reads and changes the state, the theme
    /// included, only through the store hook.
    pub layout_component_rule: &'static str,
    /// In place of `layout_component_rule` when the plan has no store
    /// provider (`semantics::store_provider`): the component keeps no copy of
    /// the state and loads it again right before it saves, as the conventions
    /// block says for such a plan.
    pub layout_component_bare_rule: &'static str,
    /// The texts of `ArchitectOptions::split_pages` (`crate::splitpage`).
    /// `NO_PAGE_SPLIT`: no page is split.
    pub page_split: PageSplit,
    /// Added to the prompt of each page when the root layout renders a
    /// shell component (`PlanOptions::shell_rule`): the layout already draws
    /// the frame around the page. `{shell}` is the shell's tag (`<AppShell>`)
    /// and `{wrapper}` the page wrapper with the stylesheet's frame classes
    /// (``the page wrapper with class `shell` ``), or `page_shell_no_class`
    /// when the stylesheet names none.
    pub page_shell_rule: &'static str,
    /// The `{wrapper}` of `page_shell_rule` when no class of the stylesheet
    /// names the frame.
    pub page_shell_no_class: &'static str,
    /// Added to the prompt of the shell component itself with
    /// `PlanOptions::shell_rule`, so that the frame `page_shell_rule` tells
    /// the pages about is drawn: `{main}` is the wrapper it renders around
    /// `{children}` (`<main className="shell">{children}</main>`).
    pub shell_frame_rule: &'static str,
}

/// The texts of `ArchitectOptions::split_pages` (`crate::splitpage`): a page
/// whose local components (`local_note`) get files of their own beside it,
/// written at the same time as the page. Empty `dir`: no page is split.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PageSplit {
    /// The folder of a page's parts, beside the page (`_parts`).
    pub dir: &'static str,
    /// The extension of a part file (`.tsx`).
    pub ext: &'static str,
    /// The start of a part's export line (`export default function `); the
    /// part's name follows it.
    pub export_prefix: &'static str,
    /// Appended to the purpose of a part; `{page}` is the page's path. The
    /// text before `{page}` marks the note (`architect::planner_purpose`
    /// leaves it out).
    pub part_note: &'static str,
    /// Appended to the purpose of a split page; `{parts}` is the list of its
    /// parts with their import paths. The text before `{parts}` marks the
    /// note.
    pub parts_note: &'static str,
    /// The rules of the props call for the parts that the plan gives no
    /// typed signature: the form of an answer line.
    pub props_rule: &'static str,
}

/// No page split.
pub const NO_PAGE_SPLIT: PageSplit = PageSplit {
    dir: "",
    ext: "",
    export_prefix: "",
    part_note: "",
    parts_note: "",
    props_rule: "",
};

/// No quality texts: the default for profiles that have none.
pub const NO_QUALITY: Quality = Quality {
    head_rule: "",
    rewrites: &[],
    date_note: "",
    page_glob: "",
    component_glob: "",
    component_rule: "",
    purposes_rule: "",
    contract_rule: "",
    contract_cap: "",
    contract_types_note: "",
    settings_type: "",
    state_type: "",
    wiring_rule: "",
    page_local_rule: "",
    local_note: "",
    shared_component_rule: "",
    layout_glob: "",
    semantics_rule: "",
    semantic_page_local_rule: "",
    semantics_types_note: "",
    semantics_cap: "",
    conventions_rule: "",
    layout_component_rule: "",
    layout_component_bare_rule: "",
    page_split: NO_PAGE_SPLIT,
    page_shell_rule: "",
    page_shell_no_class: "",
    shell_frame_rule: "",
};

/// A long file that the plan writes in parts. The file keeps its path and
/// becomes the aggregator that joins the parts (`Staging::aggregate`
/// renders it); each part gets a run of the file's exports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Split {
    /// The files to split ([`glob_match`]); only the first match in the
    /// manifest is split.
    pub glob: &'static str,
    /// Fewest exports for a split; a file with fewer is written whole.
    pub min_exports: usize,
    /// Exports for each part: the number of parts is the exports over this,
    /// rounded up, and at least 2.
    pub part_exports: usize,
    /// Most parts.
    pub max_parts: usize,
    /// The path of a part, relative to the directory of the file; `{n}` is
    /// the part's number from 1. It must not match `glob`.
    pub part_pattern: &'static str,
    /// The purpose of part 1; `{count}` is the number of parts.
    pub first_purpose: &'static str,
    /// The purpose of each other part; `{n}` is its number, `{count}` the
    /// number of parts.
    pub part_purpose: &'static str,
    /// The new purpose of the split file, which only joins its parts.
    pub joined_purpose: &'static str,
}

/// No rendered aggregator files: `Staging::aggregate` of `NO_STAGING`.
pub fn no_aggregate(_: &Manifest, _: &str) -> Option<String> {
    None
}

/// No staging: the default for profiles that have no staging data.
pub const NO_STAGING: Staging = Staging {
    layers: &[],
    test_globs: &[],
    type_decl_prefixes: &[],
    type_owner_globs: &[],
    import_rule: "",
    staged_rule: "",
    types_win_rule: "",
    data_note: "",
    path_rules: &[],
    measured: false,
    split: &[],
    aggregate: no_aggregate,
    quality: NO_QUALITY,
};

impl Staging {
    /// The layer of `path`: the first matching `layers` glob, except that a
    /// test file is a Consumer, never Core.
    pub fn layer_of(&self, path: &str) -> Option<Layer> {
        let layer = self
            .layers
            .iter()
            .find(|(glob, _)| glob_match(glob, path))
            .map(|&(_, layer)| layer)?;
        if layer == Layer::Core && self.is_test(path) {
            return Some(Layer::Consumer);
        }
        Some(layer)
    }

    /// The rule of the first `path_rules` glob that matches `path`.
    pub fn path_rule(&self, path: &str) -> Option<&'static str> {
        self.path_rules
            .iter()
            .find(|(glob, _)| glob_match(glob, path))
            .map(|&(_, rule)| rule)
    }

    /// Whether `path` matches a `test_globs` entry.
    pub fn is_test(&self, path: &str) -> bool {
        self.test_globs.iter().any(|glob| glob_match(glob, path))
    }
}

/// Match a `/`-separated path against a glob. `*` matches any text inside
/// one segment, `**` matches zero or more whole segments, and `{a,b}`
/// matches either choice. A glob with no `/` matches the file name only,
/// so `*.test.ts` matches a suffix in any directory.
pub fn glob_match(glob: &str, path: &str) -> bool {
    expand_braces(glob).iter().any(|glob| {
        if glob.contains('/') {
            let glob: Vec<&str> = glob.split('/').collect();
            let path: Vec<&str> = path.split('/').collect();
            match_segments(&glob, &path)
        } else {
            let name = path.rsplit('/').next().unwrap_or(path);
            match_segment(glob.as_bytes(), name.as_bytes())
        }
    })
}

/// Every glob that `{a,b}` choices spell out; nested braces expand too.
fn expand_braces(glob: &str) -> Vec<String> {
    let Some(open) = glob.find('{') else {
        return vec![glob.to_string()];
    };
    let mut depth = 0;
    let mut close = None;
    let mut commas = Vec::new();
    for (index, ch) in glob[open..].char_indices() {
        match ch {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    close = Some(open + index);
                    break;
                }
            }
            ',' if depth == 1 => commas.push(open + index),
            _ => {}
        }
    }
    // An unclosed brace is literal text.
    let Some(close) = close else {
        return vec![glob.to_string()];
    };
    let mut bounds = vec![open];
    bounds.extend(commas);
    bounds.push(close);
    let suffix = &glob[close + 1..];
    bounds
        .windows(2)
        .flat_map(|pair| {
            let choice = &glob[pair[0] + 1..pair[1]];
            expand_braces(&format!("{}{choice}{suffix}", &glob[..open]))
        })
        .collect()
}

fn match_segments(glob: &[&str], path: &[&str]) -> bool {
    match glob.split_first() {
        None => path.is_empty(),
        Some((&"**", rest)) => (0..=path.len()).any(|skip| match_segments(rest, &path[skip..])),
        Some((segment, rest)) => path.split_first().is_some_and(|(name, path_rest)| {
            match_segment(segment.as_bytes(), name.as_bytes()) && match_segments(rest, path_rest)
        }),
    }
}

/// One segment; `*` matches any run of bytes.
fn match_segment(glob: &[u8], name: &[u8]) -> bool {
    let (mut g, mut n) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    while n < name.len() {
        if g < glob.len() && glob[g] == b'*' {
            star = Some((g, n));
            g += 1;
        } else if g < glob.len() && glob[g] == name[n] {
            g += 1;
            n += 1;
        } else if let Some((star_g, star_n)) = star {
            g = star_g + 1;
            n = star_n + 1;
            star = Some((star_g, star_n + 1));
        } else {
            return false;
        }
    }
    glob[g..].iter().all(|&byte| byte == b'*')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stars_stay_inside_one_segment() {
        assert!(glob_match("lib/*.ts", "lib/tasks.ts"));
        assert!(!glob_match("lib/*.ts", "lib/sub/tasks.ts"));
        assert!(glob_match("lib/**/*.ts", "lib/tasks.ts"));
        assert!(glob_match("lib/**/*.ts", "lib/a/b/tasks.ts"));
        assert!(!glob_match("lib/**/*.ts", "src/lib/tasks.ts"));
        assert!(glob_match("**", "any/path.txt"));
        assert!(glob_match("*/**/types/**", "src/types/a.ts"));
        assert!(!glob_match("*/**/types/**", "types/a.ts"));
    }

    #[test]
    fn braces_and_name_suffixes() {
        assert!(glob_match("{lib,src/lib}/**/*.ts", "src/lib/x.ts"));
        assert!(glob_match("**/*.{ts,tsx}", "app/page.tsx"));
        assert!(!glob_match("**/*.{ts,tsx}", "app/globals.css"));
        assert!(glob_match("{a,{b,c}}.ts", "c.ts"));
        assert!(glob_match("*.test.ts", "tests/deep/x.test.ts"));
        assert!(!glob_match("*.test.ts", "tests/x.spec.ts"));
        assert!(glob_match("{a.ts", "{a.ts"), "an unclosed brace is literal");
    }

    #[test]
    fn layers_first_match_and_tests_are_never_core() {
        let staging = Staging {
            layers: &[("**/types.ts", Layer::Types), ("lib/**", Layer::Core)],
            test_globs: &["**/*.test.*"],
            ..NO_STAGING
        };
        assert_eq!(staging.layer_of("lib/types.ts"), Some(Layer::Types));
        assert_eq!(staging.layer_of("lib/tasks.ts"), Some(Layer::Core));
        assert_eq!(staging.layer_of("lib/tasks.test.ts"), Some(Layer::Consumer));
        assert_eq!(staging.layer_of("app/page.tsx"), None);
        assert_eq!(NO_STAGING.layer_of("lib/tasks.ts"), None);
    }
}
