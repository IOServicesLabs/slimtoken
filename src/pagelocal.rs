//! Pages own their features (`PlanOptions::page_local`,
//! `ArchitectOptions::page_local`). From the blind judge of 8 TaskFlow apps:
//! a plan that splits one page's feature between the page file and the
//! component files that separate workers write gives duplicate UI (the page
//! renders the comments, subtasks or rename form that its component already
//! renders) and state out of sync (a component loads its own copy of the
//! app state at mount and saves it over what the page saved). Nobody owns
//! the whole feature.
//!
//! - [`fold_page_components`] (a manifest pass): a component that exactly
//!   one page renders, by what the plan says (a page or a folded component
//!   names it in its purpose, its exports or its `uses`), leaves the
//!   manifest; its purpose and signatures go into that page's purpose as
//!   local components, so one worker writes the page and its private parts.
//!   A component that no file names goes into the one page that its name,
//!   its id prop or its purpose points to (see [`Usage::guess`]). A
//!   component that a layout renders, that two or more pages render, that a
//!   lib module or a test names, that holds state (a provider, a hook, a
//!   context), or that no file names and no page matches stays a file of
//!   its own.
//! - [`component_lines`]: each component left (apart from a state owner, a
//!   component of a layout, or one whose props are only ids) is
//!   presentational.
//! - [`page_lines`] (with `objective_pages`): each page gets the shared
//!   components it renders, the links to other pages that the routes imply
//!   and the controls for each create or add export it owns.

use crate::architect::{declared_names, identifiers, planner_purpose, Manifest, ManifestFile};
use crate::objective::{href_of, is_detail_bullet, route_of, singular, words, PageBullet};
use crate::profile::{glob_match, Layer, Profile, Staging};
use std::collections::{BTreeSet, HashMap};

/// One page and the components that [`fold_page_components`] put in its
/// file, in manifest order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fold {
    pub page: String,
    pub components: Vec<String>,
}

/// The file stem of `path` (`components/TaskForm.tsx` -> `TaskForm`).
fn stem(path: &str) -> &str {
    let name = path.rsplit('/').next().unwrap_or(path);
    name.split('.').next().unwrap_or(name)
}

/// The code of an export line: the text before its `//` comment.
fn code(line: &str) -> &str {
    line.split("//").next().unwrap_or(line)
}

fn is_script(path: &str) -> bool {
    [".tsx", ".ts", ".jsx", ".js"]
        .iter()
        .any(|ext| path.ends_with(ext))
}

/// The texts of a file that name other files: purpose, exports and uses.
fn texts(file: &ManifestFile) -> impl Iterator<Item = &str> {
    std::iter::once(file.purpose.as_str())
        .chain(file.exports.iter().map(String::as_str))
        .chain(file.uses.iter().map(String::as_str))
}

/// Whether a `uses` entry (`path:name`) points into `path`.
fn uses_path(entry: &str, path: &str) -> bool {
    entry == path
        || entry
            .strip_prefix(path)
            .is_some_and(|rest| rest.starts_with(':'))
}

/// Words of a component name that say what kind of part it is, not what
/// it is about (`CommentList` is about comments).
const PART_WORDS: &[&str] = &[
    "list",
    "form",
    "card",
    "item",
    "view",
    "panel",
    "modal",
    "dialog",
    "table",
    "row",
    "section",
    "widget",
    "button",
    "editor",
    "display",
    "chart",
    "bar",
    "grid",
    "page",
    "component",
    "container",
    "box",
    "header",
    "footer",
    "picker",
    "select",
    "input",
    "field",
    "control",
    "detail",
    "app",
];

/// The entity of a route with a dynamic segment: the segment's parameter
/// when it names one (`[projectId]`, `[project_id]` -> `project`), else the
/// segment before it (`/tasks/[id]` -> `task`).
fn dynamic_entity(route: &str) -> Option<String> {
    let segments: Vec<&str> = route.split('/').filter(|part| !part.is_empty()).collect();
    let first = segments.iter().position(|part| part.starts_with('['))?;
    let param = segments[first]
        .trim_start_matches('[')
        .trim_end_matches(']')
        .trim_start_matches("...");
    let named = param
        .strip_suffix("Id")
        .or_else(|| param.strip_suffix("_id"))
        .filter(|rest| !rest.is_empty())
        .and_then(|rest| camel_words(rest).last().map(|word| singular(word)));
    named.or_else(|| {
        first
            .checked_sub(1)
            .and_then(|before| words(segments[before]).last().map(|word| singular(word)))
    })
}

/// The names of the parameters of an export's signature that are followed
/// by a type (`({ taskId }: { taskId: string })` -> `taskId`), without a
/// `props` parameter.
fn typed_params(line: &str) -> Vec<&str> {
    let code = code(line);
    let Some(open) = code.find('(') else {
        return Vec::new();
    };
    let inner = &code[open + 1..code.rfind(')').unwrap_or(code.len()).max(open + 1)];
    let mut found = Vec::new();
    let bytes = inner.as_bytes();
    for (start, word) in identifiers(inner) {
        let mut end = start + word.len();
        if bytes.get(end) == Some(&b'?') {
            end += 1;
        }
        if bytes.get(end) == Some(&b':') && word != "props" && !found.contains(&word) {
            found.push(word);
        }
    }
    found
}

/// Whether a parameter name is an id (`id`, `taskId`, `task_id`).
fn is_id_param(name: &str) -> bool {
    name == "id" || name.ends_with("Id") || name.ends_with("_id")
}

/// The record types of a plan: the interfaces and type aliases of the
/// Types-layer files and of the lib modules (a plan may declare `Category`
/// in `lib/categories.ts`), each with whether its line names an `id`.
fn record_types(files: &[ManifestFile], staging: &Staging) -> HashMap<String, bool> {
    let mut found: HashMap<String, bool> = HashMap::new();
    for file in files {
        let layer = staging.layer_of(&file.path);
        if layer != Some(Layer::Types) && layer != Some(Layer::Core) {
            continue;
        }
        for line in &file.exports {
            let head = code(line);
            let has_id = words(line).iter().any(|word| word == "id");
            for name in declared_names(head) {
                let typed = head.contains(&format!("interface {name}"))
                    || head.contains(&format!("type {name}"));
                if typed {
                    *found.entry(name).or_insert(false) |= has_id;
                }
            }
        }
    }
    found
}

/// The entity words of record types (`Project` -> `project`).
fn entity_words(types: &HashMap<String, bool>) -> BTreeSet<String> {
    types
        .keys()
        .map(|name| singular(&name.to_lowercase()))
        .collect()
}

/// Who names whom in a manifest.
struct Usage<'a> {
    files: &'a [ManifestFile],
    tests: &'a [ManifestFile],
    profile: &'a Profile,
    /// The entity words of the plan's record types (see [`record_types`]).
    entities: BTreeSet<String>,
    /// The names that each file's exports declare.
    declared: Vec<Vec<String>>,
    /// How many files declare each name.
    declarers: HashMap<String, usize>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Owner {
    Page(usize),
    Keep,
}

impl<'a> Usage<'a> {
    fn new(manifest: &'a Manifest, profile: &'a Profile) -> Self {
        let files = manifest.files.as_slice();
        let declared: Vec<Vec<String>> = files
            .iter()
            .map(|file| {
                file.exports
                    .iter()
                    .flat_map(|line| declared_names(code(line)))
                    .collect()
            })
            .collect();
        let mut declarers = HashMap::new();
        for names in &declared {
            for name in names.iter().collect::<BTreeSet<_>>() {
                *declarers.entry(name.clone()).or_insert(0) += 1;
            }
        }
        Usage {
            files,
            tests: &manifest.test_files,
            profile,
            entities: entity_words(&record_types(files, &profile.staging)),
            declared,
            declarers,
        }
    }

    /// Whether a component holds state for others: a provider (its stem
    /// or an export ends in `Provider`), a hook (`useX`) or a context.
    fn is_state_owner(&self, index: usize) -> bool {
        let file = &self.files[index];
        let name = stem(&file.path);
        name.ends_with("Provider")
            || name.ends_with("Providers")
            || self.declared[index].iter().any(|name| {
                name.ends_with("Provider")
                    || name.ends_with("Context")
                    || name
                        .strip_prefix("use")
                        .is_some_and(|rest| rest.starts_with(|ch: char| ch.is_ascii_uppercase()))
            })
            || file
                .exports
                .iter()
                .any(|line| code(line).contains("createContext"))
    }

    /// Whether a file that is not a page, a layout or a component (a lib
    /// module, a test; not a style sheet or a Types-layer file, whose
    /// settings comment names readers in prose) needs component `index` as
    /// a file: a `uses` entry into it, its path, or a name that only it
    /// declares. Such a component stays a file of its own.
    fn pinned(&self, index: usize) -> bool {
        let path = &self.files[index].path;
        let names: Vec<&str> = self.declared[index]
            .iter()
            .filter(|name| name.len() >= 2 && self.declarers.get(*name) == Some(&1))
            .map(String::as_str)
            .collect();
        let needs = |file: &ManifestFile| {
            file.uses.iter().any(|entry| uses_path(entry, path))
                || texts(file).any(|text| {
                    text.contains(path.as_str())
                        || identifiers(text)
                            .iter()
                            .any(|&(_, word)| names.contains(&word))
                })
        };
        let lib = self.files.iter().enumerate().any(|(user, file)| {
            user != index
                && is_script(&file.path)
                && (!(self.profile.is_consumer)(&file.path)
                    || self.profile.staging.is_test(&file.path))
                && self.profile.staging.layer_of(&file.path) != Some(Layer::Types)
                && !self.is_page(user)
                && !self.is_layout(user)
                && !self.is_component(user)
                && needs(file)
        });
        lib || self.tests.iter().any(needs)
    }

    /// The words that name a page: those of its static route segments and
    /// of its default export, without `page` and without the plan's record
    /// entities (`app/page.tsx` with `DashboardPage` -> `dashboard`;
    /// `/tasks/[id]` with `TaskDetailPage` -> `detail`: many parts are about
    /// tasks, and a part that only takes the task's id is matched by it).
    fn page_words(&self, index: usize) -> BTreeSet<String> {
        let mut found = self.all_page_words(index);
        found.retain(|word| !self.entities.contains(word));
        found
    }

    /// [`Usage::page_words`] with the entities.
    fn all_page_words(&self, index: usize) -> BTreeSet<String> {
        let file = &self.files[index];
        let mut found = BTreeSet::new();
        if let Some(route) = route_of(&file.path) {
            for segment in route.split('/').filter(|part| !part.starts_with('[')) {
                found.extend(word_set(segment));
            }
        }
        for line in &file.exports {
            if !code(line).contains("export default") {
                continue;
            }
            for name in declared_names(code(line)) {
                found.extend(camel_words(&name).iter().map(|word| singular(word)));
            }
        }
        found.remove("page");
        found
    }

    /// The one page that a component no file names belongs to, from what
    /// it is called and what it takes, in this order (the first test that
    /// one page passes wins; two or more pages for a test: none):
    /// 1. a word of its stem that is not a kind of part (`BoardFilters`:
    ///    `board`, `filter`) names the page (`/board`, `DashboardPage`);
    /// 2. an id prop (`taskId`) is the entity of a detail route
    ///    (`/tasks/[id]`), or of a static page named after it and `detail`;
    /// 3. a word of its purpose names the page ("Dashboard summary cards").
    fn guess(&self, index: usize) -> Option<usize> {
        let file = &self.files[index];
        let pages: Vec<(usize, BTreeSet<String>)> = (0..self.files.len())
            .filter(|&page| self.is_page(page) && !self.is_layout(page))
            .map(|page| (page, self.page_words(page)))
            .collect();
        let one = |found: Vec<usize>| -> Result<Option<usize>, ()> {
            match found.as_slice() {
                [] => Ok(None),
                [page] => Ok(Some(*page)),
                _ => Err(()),
            }
        };
        let stem_words: Vec<String> = camel_words(stem(&file.path))
            .iter()
            .map(|word| singular(word))
            .filter(|word| word.len() >= 3 && !PART_WORDS.contains(&word.as_str()))
            .collect();
        let by_stem = pages
            .iter()
            .filter(|(_, names)| stem_words.iter().any(|word| names.contains(word)))
            .map(|(page, _)| *page)
            .collect();
        match one(by_stem) {
            Ok(Some(page)) => return Some(page),
            Err(()) => return None,
            Ok(None) => {}
        }
        let ids: BTreeSet<String> = file
            .exports
            .iter()
            .flat_map(|line| typed_params(line))
            .filter_map(|name| {
                let rest = name
                    .strip_suffix("Id")
                    .or_else(|| name.strip_suffix("_id"))?;
                camel_words(rest).last().map(|word| singular(word))
            })
            .collect();
        if !ids.is_empty() {
            let detail: Vec<usize> = pages
                .iter()
                .filter(|(page, _)| {
                    let route = route_of(&self.files[*page].path).unwrap_or_default();
                    match dynamic_entity(&route) {
                        Some(entity) => ids.contains(&entity),
                        None => {
                            let names = self.all_page_words(*page);
                            names.contains("detail") && ids.iter().any(|id| names.contains(id))
                        }
                    }
                })
                .map(|(page, _)| *page)
                .collect();
            match one(detail) {
                Ok(Some(page)) => return Some(page),
                Err(()) => return None,
                Ok(None) => {}
            }
        }
        let purpose = word_set(&planner_purpose(&file.purpose, &self.profile.staging));
        let by_purpose = pages
            .iter()
            .filter(|(_, names)| names.iter().any(|word| purpose.contains(word)))
            .map(|(page, _)| *page)
            .collect();
        one(by_purpose).ok().flatten()
    }

    fn quality(&self) -> &crate::profile::Quality {
        &self.profile.staging.quality
    }

    fn is_page(&self, index: usize) -> bool {
        glob_match(self.quality().page_glob, &self.files[index].path)
    }

    fn is_component(&self, index: usize) -> bool {
        let path = &self.files[index].path;
        is_script(path) && glob_match(self.quality().component_glob, path)
    }

    fn is_layout(&self, index: usize) -> bool {
        let glob = self.quality().layout_glob;
        !glob.is_empty() && glob_match(glob, &self.files[index].path)
    }

    /// The names that identify a component: its file stem and each name
    /// that its exports declare and no other file declares.
    fn own_names(&self, index: usize) -> Vec<&str> {
        let mut names = vec![stem(&self.files[index].path)];
        for name in &self.declared[index] {
            if name.len() >= 2
                && self.declarers.get(name) == Some(&1)
                && !names.contains(&name.as_str())
            {
                names.push(name);
            }
        }
        names
    }

    /// Whether the file `user` (a page, a layout or a component; not a test,
    /// a lib module or a style sheet) names the file `named`: a `uses`
    /// entry into it, or a name of it that `user` does not declare itself.
    fn names(&self, user: usize, named: usize) -> bool {
        let file = &self.files[user];
        if user == named
            || !is_script(&file.path)
            || !(self.profile.is_consumer)(&file.path)
            || self.profile.staging.is_test(&file.path)
        {
            return false;
        }
        let path = &self.files[named].path;
        if file.uses.iter().any(|entry| uses_path(entry, path)) {
            return true;
        }
        let own: Vec<&str> = self
            .own_names(named)
            .into_iter()
            .filter(|name| !self.declared[user].iter().any(|mine| mine == name))
            .collect();
        texts(file).any(|text| {
            identifiers(text)
                .iter()
                .any(|&(_, word)| own.contains(&word))
        })
    }

    fn users(&self, named: usize) -> Vec<usize> {
        (0..self.files.len())
            .filter(|&user| self.names(user, named))
            .collect()
    }

    /// The one page that renders component `index`, directly or through
    /// components that only it renders; `Keep` for anything else.
    fn owner(&self, index: usize, memo: &mut [Option<Owner>], visiting: &mut [bool]) -> Owner {
        if let Some(owner) = memo[index] {
            return owner;
        }
        if visiting[index] {
            return Owner::Keep;
        }
        if self.is_state_owner(index) || self.pinned(index) {
            memo[index] = Some(Owner::Keep);
            return Owner::Keep;
        }
        let users = self.users(index);
        if users.is_empty() {
            let owner = self.guess(index).map_or(Owner::Keep, Owner::Page);
            memo[index] = Some(owner);
            return owner;
        }
        visiting[index] = true;
        let mut page: Option<usize> = None;
        let mut keep = false;
        for user in users {
            let owner = if self.is_page(user) {
                Owner::Page(user)
            } else if self.is_component(user) {
                self.owner(user, memo, visiting)
            } else {
                Owner::Keep
            };
            match owner {
                Owner::Page(found) if page.is_none_or(|known| known == found) => page = Some(found),
                _ => {
                    keep = true;
                    break;
                }
            }
        }
        visiting[index] = false;
        let owner = match page {
            Some(page) if !keep => Owner::Page(page),
            _ => Owner::Keep,
        };
        memo[index] = Some(owner);
        owner
    }

    /// For each file: the page whose file takes it in (`None` for anything
    /// but a component that exactly one page renders).
    fn owners(&self) -> Vec<Option<usize>> {
        let count = self.files.len();
        let mut memo = vec![None; count];
        let mut visiting = vec![false; count];
        (0..count)
            .map(|index| {
                if !self.is_component(index) {
                    return None;
                }
                match self.owner(index, &mut memo, &mut visiting) {
                    Owner::Page(page) => Some(page),
                    Owner::Keep => None,
                }
            })
            .collect()
    }
}

/// An export line as a local declaration (`export default function X(...)`
/// -> `function X(...)`).
fn local_signature(line: &str) -> &str {
    let line = line.trim();
    line.strip_prefix("export default ")
        .or_else(|| line.strip_prefix("export "))
        .unwrap_or(line)
}

/// One local component in the note of its page: name, planner purpose and
/// signatures.
fn local_entry(file: &ManifestFile, staging: &Staging) -> String {
    let purpose = planner_purpose(&file.purpose, staging);
    let purpose = purpose.trim().trim_end_matches('.');
    let signatures: Vec<&str> = file
        .exports
        .iter()
        .map(|line| local_signature(line))
        .collect();
    let detail = match (purpose.is_empty(), signatures.is_empty()) {
        (true, true) => String::new(),
        (false, true) => format!(" ({purpose})"),
        (true, false) => format!(" ({})", signatures.join(" | ")),
        (false, false) => format!(" ({purpose}; {})", signatures.join(" | ")),
    };
    format!("`{}`{detail}", stem(&file.path))
}

/// A page that takes in local components uses state and handlers, so it is
/// one Client Component (`"use client"` holds for the whole file, and a
/// Client Component is not `async`): a dynamic route reads its parameter
/// with `useParams()` (`profile::next_dyn_route`), a page that reads the
/// query string does so with `useSearchParams()` inside `<Suspense>`
/// (`profile::next_client_pages`), and any other `async` page loses the
/// `async`. A page already planned as a Client Component keeps its line.
fn client_page(file: &mut ManifestFile) {
    let mut one = Manifest {
        files: vec![file.clone()],
        test_files: Vec::new(),
        readme: false,
    };
    crate::profile::next_dyn_route(&mut one);
    crate::profile::next_client_pages(&mut one);
    let Some(done) = one.files.pop() else {
        return;
    };
    file.exports = done.exports;
    for line in &mut file.exports {
        if code(line).contains("export default async function") {
            *line = line.replacen(
                "export default async function",
                "export default function",
                1,
            );
        }
    }
}

/// The manifest pass of `PlanOptions::page_local`: each component that
/// exactly one page renders (see the module docs) leaves the manifest, and
/// that page's purpose gets the profile's `quality.local_note` with the
/// component's purpose and signatures (as local, unexported declarations).
/// The component's `uses` join the page's; every other mention of its path
/// (a settings reader in the types file, a `uses` entry) names the page
/// instead. A page that takes in components becomes a Client Component
/// (see [`client_page`]). Returns the folds in manifest
/// order. Does nothing for a profile without a local note, a page glob or a
/// component glob (PYTHON, RUST). Idempotent: a folded component is gone,
/// and the components left keep their users.
pub fn fold_page_components(manifest: &mut Manifest, profile: &Profile) -> Vec<Fold> {
    let quality = &profile.staging.quality;
    if quality.local_note.is_empty()
        || quality.page_glob.is_empty()
        || quality.component_glob.is_empty()
    {
        return Vec::new();
    }
    let owners = Usage::new(manifest, profile).owners();
    let mut folds: Vec<(usize, Vec<usize>)> = Vec::new();
    for page in 0..manifest.files.len() {
        let parts: Vec<usize> = (0..owners.len())
            .filter(|&index| owners[index] == Some(page))
            .collect();
        if !parts.is_empty() {
            folds.push((page, parts));
        }
    }
    if folds.is_empty() {
        note_unplanned_components(manifest, profile);
        return Vec::new();
    }
    let (start, end) = quality
        .local_note
        .split_once("{components}")
        .unwrap_or((quality.local_note, ""));
    let mut moved: Vec<(String, String)> = Vec::new();
    let result: Vec<Fold> = folds
        .iter()
        .map(|(page, parts)| Fold {
            page: manifest.files[*page].path.clone(),
            components: parts
                .iter()
                .map(|&index| manifest.files[index].path.clone())
                .collect(),
        })
        .collect();
    for (page, parts) in &folds {
        let entries: Vec<String> = parts
            .iter()
            .map(|&index| local_entry(&manifest.files[index], &profile.staging))
            .collect();
        let uses: Vec<String> = parts
            .iter()
            .flat_map(|&index| manifest.files[index].uses.clone())
            .collect();
        let page_path = manifest.files[*page].path.clone();
        for &index in parts {
            moved.push((manifest.files[index].path.clone(), page_path.clone()));
        }
        let file = &mut manifest.files[*page];
        file.purpose
            .push_str(&format!("{start}{}{end}", entries.join("; ")));
        client_page(file);
        for entry in uses {
            if !file.uses.contains(&entry) {
                file.uses.push(entry);
            }
        }
    }
    let gone: BTreeSet<String> = moved.iter().map(|(from, _)| from.clone()).collect();
    manifest.files.retain(|file| !gone.contains(&file.path));
    for file in &mut manifest.files {
        for (from, to) in &moved {
            if file.purpose.contains(from.as_str()) {
                file.purpose = file.purpose.replace(from.as_str(), to);
            }
            for line in &mut file.exports {
                if line.contains(from.as_str()) {
                    *line = line.replace(from.as_str(), to);
                }
            }
            for entry in &mut file.uses {
                if uses_path(entry, from) {
                    *entry = format!("{to}{}", &entry[from.len()..]);
                }
            }
        }
        // A page does not call into itself.
        let own = file.path.clone();
        file.uses.retain(|entry| !uses_path(entry, &own));
        let mut seen = BTreeSet::new();
        file.uses.retain(|entry| seen.insert(entry.clone()));
    }
    note_unplanned_components(manifest, profile);
    result
}

/// Names that a page renders from React or Next.js, never a planned file.
const FRAMEWORK_COMPONENTS: &[&str] = &[
    "Suspense",
    "Fragment",
    "Link",
    "Image",
    "Script",
    "Head",
    "React",
    "StrictMode",
];

/// Components named in a page's line (`renders <AccountForm/>`) that no
/// planned file declares. With the page-local planner rule the planner itself
/// leaves single-page components out of the manifest, but the page line still
/// names them, and without the local note the page worker imports a file that
/// is never written (20261116-051055: 13 TS2307 errors, one per such import).
fn unplanned_components(manifest: &Manifest, page: usize) -> Vec<String> {
    let file = &manifest.files[page];
    let mut text = file.purpose.clone();
    for line in &file.exports {
        text.push('\n');
        text.push_str(line);
    }
    let declared = |name: &str| {
        manifest.files.iter().any(|other| {
            stem(&other.path) == name
                || other.exports.iter().any(|line| {
                    let code = code(line);
                    [
                        "function ",
                        "const ",
                        "class ",
                        "interface ",
                        "type ",
                        "enum ",
                    ]
                    .iter()
                    .any(|kind| {
                        code.match_indices(&format!("{kind}{name}")).any(|(at, _)| {
                            let after = at + kind.len() + name.len();
                            !code[after..]
                                .bytes()
                                .next()
                                .is_some_and(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
                        })
                    })
                })
        })
    };
    let mut found: Vec<String> = Vec::new();
    for (at, _) in text.match_indices('<') {
        // `Array<Task>`, `useState<Settings>`: a type argument, not JSX.
        let generic = text[..at]
            .bytes()
            .next_back()
            .is_some_and(|byte| byte.is_ascii_alphanumeric() || byte == b'_');
        if generic {
            continue;
        }
        let rest = &text[at + 1..];
        let len = rest
            .bytes()
            .take_while(|byte| byte.is_ascii_alphanumeric() || *byte == b'_')
            .count();
        let name = &rest[..len];
        if len < 2 || !name.starts_with(|ch: char| ch.is_ascii_uppercase()) {
            continue;
        }
        let closes = rest[len..]
            .trim_start()
            .starts_with(|ch: char| ch == '/' || ch == '>' || ch.is_ascii_alphabetic());
        if !closes
            || FRAMEWORK_COMPONENTS.contains(&name)
            || found.iter().any(|seen| seen == name)
            || declared(name)
        {
            continue;
        }
        found.push(name.to_string());
    }
    found
}

/// The detail of a local component that no file declares, in the local note
/// of its page (see [`note_unplanned_components`]).
pub(crate) const UNPLANNED_DETAIL: &str = "named in this page's plan; no file of its own";

/// Give each page that names components no file declares the local note for
/// them (see [`unplanned_components`]); such a page is a Client Component like
/// a page with folded components. A name the note already lists is skipped,
/// so the pass stays idempotent.
fn note_unplanned_components(manifest: &mut Manifest, profile: &Profile) {
    let quality = &profile.staging.quality;
    let (start, end) = quality
        .local_note
        .split_once("{components}")
        .unwrap_or((quality.local_note, ""));
    for page in 0..manifest.files.len() {
        if !glob_match(quality.page_glob, &manifest.files[page].path) {
            continue;
        }
        let names: Vec<String> = unplanned_components(manifest, page)
            .into_iter()
            .filter(|name| {
                let listed = format!("`{name}`");
                !(has_local_components(&manifest.files[page].purpose, quality)
                    && manifest.files[page].purpose.contains(&listed))
            })
            .collect();
        if names.is_empty() {
            continue;
        }
        let entries: Vec<String> = names
            .iter()
            .map(|name| format!("`{name}` ({UNPLANNED_DETAIL})"))
            .collect();
        let file = &mut manifest.files[page];
        file.purpose
            .push_str(&format!("{start}{}{end}", entries.join("; ")));
        client_page(file);
    }
}

/// The start of the profile's `quality.local_note` (the text before
/// `{components}`), which marks a page that holds local components; empty
/// for a profile without the note.
pub fn local_mark(quality: &crate::profile::Quality) -> &'static str {
    quality
        .local_note
        .split("{components}")
        .next()
        .unwrap_or("")
}

/// Whether a purpose has the note of [`fold_page_components`].
pub fn has_local_components(purpose: &str, quality: &crate::profile::Quality) -> bool {
    let mark = local_mark(quality);
    !mark.is_empty() && purpose.contains(mark)
}

/// The line that `PlanOptions::page_local` adds to the tail of each file's
/// prompt, in manifest order: the profile's `quality.shared_component_rule`
/// for each component (`quality.component_glob`) that holds no state (not a
/// provider, a hook or a context), that no layout (`quality.layout_glob`)
/// names, and whose props are not only ids (a component planned as
/// `({ taskId })` can only load its records itself: the rule would
/// contradict its contract); empty for the other files, or for a profile
/// without the rule.
pub fn component_lines(manifest: &Manifest, profile: &Profile) -> Vec<String> {
    let rule = profile.staging.quality.shared_component_rule;
    let count = manifest.files.len();
    if rule.is_empty() || profile.staging.quality.component_glob.is_empty() {
        return vec![String::new(); count];
    }
    let usage = Usage::new(manifest, profile);
    (0..count)
        .map(|index| {
            let file = &manifest.files[index];
            let params: Vec<&str> = file
                .exports
                .iter()
                .filter(|line| code(line).contains("export default"))
                .flat_map(|line| typed_params(line))
                .collect();
            let only_ids = !params.is_empty() && params.iter().all(|name| is_id_param(name));
            let shared = usage.is_component(index)
                && !usage.is_state_owner(index)
                && !only_ids
                && !(0..count).any(|user| usage.is_layout(user) && usage.names(user, index));
            if shared {
                format!("\n- {rule}")
            } else {
                String::new()
            }
        })
        .collect()
}

/// The line that `PlanOptions::semantics` adds to the tail of each file's
/// prompt, in manifest order: the profile's `quality.layout_component_rule`
/// for each component (`quality.component_glob`) that a layout
/// (`quality.layout_glob`) names and that holds no state (not the provider,
/// a hook or a context): the other side of [`component_lines`], whose
/// shared components take their data as props. A layout component stays
/// mounted while the pages change, so it uses the store hook; with no store
/// provider in the plan (`semantics::store_provider`), there is no hook, so
/// it gets `quality.layout_component_bare_rule`. Empty for the other files,
/// or for a profile without the rule.
pub fn layout_component_lines(manifest: &Manifest, profile: &Profile) -> Vec<String> {
    let quality = &profile.staging.quality;
    let rule = if crate::semantics::store_provider(manifest, profile).is_some()
        || quality.layout_component_bare_rule.is_empty()
    {
        quality.layout_component_rule
    } else {
        quality.layout_component_bare_rule
    };
    let count = manifest.files.len();
    if rule.is_empty() || quality.component_glob.is_empty() || quality.layout_glob.is_empty() {
        return vec![String::new(); count];
    }
    let usage = Usage::new(manifest, profile);
    (0..count)
        .map(|index| {
            let of_layout = usage.is_component(index)
                && !usage.is_state_owner(index)
                && (0..count).any(|user| usage.is_layout(user) && usage.names(user, index));
            if of_layout {
                format!("\n- {rule}")
            } else {
                String::new()
            }
        })
        .collect()
}

/// The line that `PlanOptions::shell_rule` adds to the tail of each file's
/// prompt, in manifest order: the profile's `quality.page_shell_rule` for
/// each page (`quality.page_glob`, not a layout) when a layout names a
/// component that holds no state and frames the page (it renders it around
/// `{children}`, or the component's texts name its children or the nav;
/// the first such component); `{shell}` is that component's tag and
/// `{wrapper}` the page wrapper with the stylesheet's classes that name a
/// shell (an export `.shell  // page wrapper`), else
/// `quality.page_shell_no_class`. The frame component itself gets
/// `quality.shell_frame_rule`, with `{main}` the wrapper it renders around
/// `{children}`. Empty for the other files, a plan with no such component,
/// or a profile without the rule.
pub fn page_shell_lines(manifest: &Manifest, profile: &Profile) -> Vec<String> {
    let quality = &profile.staging.quality;
    let count = manifest.files.len();
    if quality.page_shell_rule.is_empty()
        || quality.component_glob.is_empty()
        || quality.layout_glob.is_empty()
    {
        return vec![String::new(); count];
    }
    let usage = Usage::new(manifest, profile);
    // The frame: the first stateless component of a layout that the layout
    // renders around `{children}` (`<AppShell>{children}</AppShell>`), or
    // whose own texts name its children or the navigation. A Toaster or a
    // ThemeScript that the layout also names is not the frame.
    let frame = (0..count).find(|&index| {
        if !usage.is_component(index) || usage.is_state_owner(index) {
            return false;
        }
        let layouts: Vec<usize> = (0..count)
            .filter(|&user| usage.is_layout(user) && usage.names(user, index))
            .collect();
        if layouts.is_empty() {
            return false;
        }
        let close = format!("</{}>", stem(&manifest.files[index].path));
        let wraps = layouts
            .iter()
            .any(|&user| texts(&manifest.files[user]).any(|text| text.contains(&close)));
        let names_frame = texts(&manifest.files[index]).any(|text| {
            let lower = text.to_ascii_lowercase();
            lower.contains("children") || lower.contains("nav")
        });
        wraps || names_frame
    });
    let Some(frame) = frame else {
        return vec![String::new(); count];
    };
    let shell = format!("<{}>", stem(&manifest.files[frame].path));
    let classes: BTreeSet<String> = manifest
        .files
        .iter()
        .filter(|file| file.path.ends_with(".css"))
        .flat_map(|file| file.exports.iter())
        .filter_map(|line| {
            let class = code(line).trim().strip_prefix('.')?;
            let class: String = class
                .chars()
                .take_while(|ch| ch.is_ascii_alphanumeric() || *ch == '-' || *ch == '_')
                .collect();
            class.contains("shell").then_some(class)
        })
        .collect();
    let wrapper = if classes.is_empty() {
        quality.page_shell_no_class.to_string()
    } else {
        let names: Vec<String> = classes.iter().map(|class| format!("`{class}`")).collect();
        format!("the page wrapper with class {}", names.join(" or "))
    };
    let rule = quality
        .page_shell_rule
        .replace("{shell}", &shell)
        .replace("{wrapper}", &wrapper);
    // The frame's own line makes the pages' claim true: it renders the
    // wrapper around `{children}`.
    let main = match classes.iter().next() {
        Some(class) => format!("<main className=\"{class}\">{{children}}</main>"),
        None => "<main>{children}</main>".to_string(),
    };
    let frame_rule = quality.shell_frame_rule.replace("{main}", &main);
    (0..count)
        .map(|index| {
            if index == frame && !frame_rule.is_empty() {
                format!("\n- {frame_rule}")
            } else if usage.is_page(index) && !usage.is_layout(index) {
                format!("\n- {rule}")
            } else {
                String::new()
            }
        })
        .collect()
}

/// A page that items of one entity link to: the detail route of the entity
/// (`/tasks/[id]`, or a static page that owns a detail bullet, `/task?id=`),
/// or a static page that the objective says shows one record of it ("a
/// kanban board for one project": `/board?project=${id}`).
struct Target {
    entity: String,
    href: String,
    page: usize,
    one: bool,
}

/// The singular lower-case words of a text, as a set.
fn word_set(text: &str) -> BTreeSet<String> {
    words(text).iter().map(|word| singular(word)).collect()
}

/// The lower-case words of a camel-case name (`RecurringRule` ->
/// `recurring`, `rule`).
fn camel_words(name: &str) -> Vec<String> {
    let mut found = Vec::new();
    let mut word = String::new();
    for ch in name.chars() {
        if ch.is_ascii_uppercase() && !word.is_empty() {
            found.push(std::mem::take(&mut word));
        }
        if ch.is_ascii_alphanumeric() {
            word.push(ch.to_ascii_lowercase());
        }
    }
    if !word.is_empty() {
        found.push(word);
    }
    found
}

/// Words of a page bullet that say the page changes records.
const CHANGE_WORDS: &[&str] = &[
    "create", "add", "new", "edit", "move", "rename", "archive", "delete", "remove", "mark",
    "update", "split", "close", "manage",
];

/// The prefixes of a lib export that adds a record (`createTask`,
/// `addComment`, `upsertTransaction`, `saveGoal`, `newBudget`).
const CREATE_PREFIXES: &[&str] = &["create", "add", "upsert", "save", "new"];

/// The prefixes of a lib export that edits, deletes or moves a record of an
/// entity that has a create export (`updateGoal`, `removeGoal`,
/// `unarchiveProject`); in this order on the page line.
const CHANGE_PREFIXES: &[&str] = &[
    "update",
    "edit",
    "rename",
    "remove",
    "delete",
    "archive",
    "unarchive",
    "restore",
    "close",
    "reopen",
];

/// The controls that the changer exports of a page (`` `updateGoal` (lib/goals.ts) ``)
/// back: an edit control only with an update, edit or rename export, a
/// delete control only with a remove or delete export, and a state control,
/// with its reverse only when the module exports the pair.
fn change_controls(items: &[String]) -> String {
    let has = |prefixes: &[&str]| {
        items.iter().any(|item| {
            prefixes.iter().any(|prefix| {
                item.strip_prefix('`')
                    .and_then(|name| name.strip_prefix(prefix))
                    .is_some_and(|rest| rest.starts_with(|ch: char| ch.is_ascii_uppercase()))
            })
        })
    };
    let mut controls: Vec<&str> = Vec::new();
    if has(&["update", "edit", "rename"]) {
        controls.push("an edit control");
    }
    if has(&["remove", "delete"]) {
        controls.push("a delete control");
    }
    let forward = has(&["archive", "close"]);
    let back = has(&["unarchive", "restore", "reopen"]);
    match (forward, back) {
        (true, true) => controls.push("a control for each state change and its reverse"),
        (false, false) => {}
        _ => controls.push("a control for each state change"),
    }
    match controls.as_slice() {
        [] => "a control for each change".to_string(),
        [one] => one.to_string(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    }
}

/// Entities that the app records as a side effect (an activity feed), not
/// through a form: their create or add export gets no control.
const LOG_WORDS: &[&str] = &[
    "activity",
    "event",
    "log",
    "history",
    "notification",
    "audit",
];

/// The pages that the records of each entity link to, from the routes (see
/// [`Target`]). `entities` are the declared record types in lower case
/// (`project`): a page that shows "one X" links only for such an X ("for
/// one month" names no record).
fn link_targets<'b>(
    routes: &[Option<String>],
    bullet_of: &dyn Fn(usize) -> Option<&'b PageBullet>,
    entities: &BTreeSet<String>,
) -> Vec<Target> {
    let mut targets: Vec<Target> = Vec::new();
    for (index, route) in routes.iter().enumerate() {
        let Some(route) = route else { continue };
        if route.contains('[') {
            if let (Some(entity), Some(href)) = (dynamic_entity(route), href_of(route)) {
                targets.push(Target {
                    entity,
                    href,
                    page: index,
                    one: false,
                });
            }
            continue;
        }
        if let Some(bullet) = bullet_of(index).filter(|bullet| is_detail_bullet(bullet)) {
            if let Some(first) = words(&bullet.name).first() {
                targets.push(Target {
                    entity: singular(first),
                    href: format!("{route}?id=${{id}}"),
                    page: index,
                    one: false,
                });
            }
        }
    }
    for (index, route) in routes.iter().enumerate() {
        let Some(route) = route else { continue };
        if route == "/" || route.contains('[') || targets.iter().any(|target| target.page == index)
        {
            continue;
        }
        let Some(bullet) = bullet_of(index) else {
            continue;
        };
        let lower = bullet.text.to_lowercase();
        let entity = ["for one ", "for a single ", "of one "]
            .iter()
            .find_map(|marker| {
                let at = lower.find(marker)?;
                words(&lower[at + marker.len()..])
                    .first()
                    .map(|word| singular(word))
            });
        let Some(entity) = entity.filter(|entity| entities.contains(entity)) else {
            continue;
        };
        if targets.iter().any(|target| target.entity == entity) {
            continue;
        }
        targets.push(Target {
            href: format!("{route}?{entity}=${{id}}"),
            entity,
            page: index,
            one: true,
        });
    }
    targets
}

/// The lines that `PlanOptions::page_local` adds to each route file's tail
/// with `objective_pages`, in manifest order (empty for other files).
/// `bullets` is the objective's page list and `owners` the bullet of each
/// file (`objective::match_pages`). A page gets:
/// - the shared components it names (still files of their own), each with
///   its purpose: it imports them and does not render again what they do;
/// - the links that the routes imply: each record of an entity that has a
///   detail page (see [`Target`]) links to it, on each other page whose
///   bullet, purpose or exports name the entity;
/// - on a page that shows one record, where it reads its id and that the
///   user can pick another;
/// - the create or add exports of the `lib` modules (a Core-layer function
///   `createX`/`addX` for a type `X` that the Types-layer files or a lib
///   module declare, or for a collection of the Types-layer files
///   (`categories`); `upsertX`/`saveX`/`newX` for such a type with an `id`;
///   not a log such as an activity feed) whose records this page owns: the static page
///   named after the entity (`/projects`), else among the pages (not the
///   entity's own detail page) that name it, the first whose bullet names it
///   and changes records ("move tasks"), else whose bullet names it, else
///   whose bullet changes records, else the first;
/// - the option flags of the records it adds (`Budget.rollover`, see
///   `semantics::record_option_flags`), or of the record it is named after
///   when no create export owns it, for its form.
///
/// A settings page (`/settings`, `/preferences`) gets no links: it names
/// records only to pick a default.
pub fn page_lines(
    manifest: &Manifest,
    profile: &Profile,
    bullets: &[PageBullet],
    owners: &[Option<usize>],
) -> Vec<String> {
    let files = &manifest.files;
    let staging = &profile.staging;
    let usage = Usage::new(manifest, profile);
    let count = files.len();
    let routes: Vec<Option<String>> = (0..count)
        .map(|index| {
            if usage.is_page(index) {
                route_of(&files[index].path)
            } else {
                None
            }
        })
        .collect();
    let bullet_of = |index: usize| {
        owners
            .get(index)
            .copied()
            .flatten()
            .and_then(|at| bullets.get(at))
    };
    let mentions: Vec<BTreeSet<String>> = (0..count)
        .map(|index| {
            let mut text = bullet_of(index).map_or(String::new(), |bullet| bullet.text.clone());
            for part in texts(&files[index]) {
                text.push(' ');
                text.push_str(part);
            }
            word_set(&text)
        })
        .collect();
    // The record types (with whether they have an id) and the collections
    // of the Types-layer files (`categories: ...` -> `category`; the whole
    // line: plans often give the fields in the comment).
    let record_types = record_types(files, staging);
    let collections: BTreeSet<String> = files
        .iter()
        .filter(|file| staging.layer_of(&file.path) == Some(Layer::Types))
        .flat_map(|file| file.exports.iter())
        .flat_map(|line| words(line))
        .filter_map(|word| {
            let one = singular(&word);
            (one != word).then_some(one)
        })
        .collect();
    let targets = link_targets(&routes, &bullet_of, &usage.entities);
    let pages: Vec<usize> = (0..count)
        .filter(|&index| routes[index].is_some())
        .collect();
    let changes = |page: usize| {
        bullet_of(page).is_some_and(|bullet| {
            words(&bullet.text)
                .iter()
                .any(|word| CHANGE_WORDS.contains(&word.trim_end_matches('s')))
        })
    };
    let mut creators: Vec<Vec<String>> = vec![Vec::new(); count];
    let mut changers: Vec<Vec<String>> = vec![Vec::new(); count];
    // The record types that each page adds (`Budget`), for their flags.
    let mut created: Vec<Vec<String>> = vec![Vec::new(); count];
    let flags = crate::semantics::record_option_flags(manifest, profile);
    for (index, file) in files.iter().enumerate() {
        if staging.layer_of(&file.path) != Some(Layer::Core) {
            continue;
        }
        for name in &usage.declared[index] {
            let Some((prefix, entity)) = CREATE_PREFIXES.iter().find_map(|prefix| {
                name.strip_prefix(prefix)
                    .filter(|rest| rest.starts_with(|ch: char| ch.is_ascii_uppercase()))
                    .map(|rest| (*prefix, rest))
            }) else {
                continue;
            };
            let typed = record_types.get(entity).copied();
            let record = match prefix {
                // `createCategory` with no `Category` type: a collection of
                // the app state still names it.
                "create" | "add" => typed.is_some() || collections.contains(&entity.to_lowercase()),
                // `upsertTransaction`, `saveGoal`: a record with an id (not
                // `saveState`, `saveSettings`).
                _ => typed == Some(true),
            };
            if !record || entity == "Id" || entity == "Ids" {
                continue;
            }
            let parts = camel_words(entity);
            let mut names: Vec<String> = vec![parts.concat()];
            for part in [parts.first(), parts.last()].into_iter().flatten() {
                if !names.contains(part) {
                    names.push(part.clone());
                }
            }
            if names.iter().any(|word| LOG_WORDS.contains(&word.as_str())) {
                continue;
            }
            let own_detail = |page: usize| {
                targets.iter().any(|target| {
                    target.page == page && !target.one && names.contains(&target.entity)
                })
            };
            let list = pages.iter().copied().find(|&page| {
                let route = routes[page].as_deref().unwrap_or("");
                !route.contains('[')
                    && !own_detail(page)
                    && route
                        .rsplit('/')
                        .next()
                        .and_then(|last| words(last).last().map(|word| singular(word)))
                        .is_some_and(|last| names.contains(&last))
            });
            let in_bullet = |page: usize| {
                bullet_of(page).is_some_and(|bullet| {
                    let words = word_set(&bullet.text);
                    names.iter().any(|word| words.contains(word))
                })
            };
            let owner = list.or_else(|| {
                let named: Vec<usize> = pages
                    .iter()
                    .copied()
                    .filter(|&page| {
                        !own_detail(page) && names.iter().any(|word| mentions[page].contains(word))
                    })
                    .collect();
                let first =
                    |test: &dyn Fn(usize) -> bool| named.iter().copied().find(|&page| test(page));
                first(&|page| in_bullet(page) && changes(page))
                    .or_else(|| first(&|page| in_bullet(page)))
                    .or_else(|| first(&|page| changes(page)))
                    .or_else(|| named.first().copied())
            });
            if let Some(owner) = owner {
                let item = format!("`{name}` ({})", file.path);
                if !creators[owner].contains(&item) {
                    creators[owner].push(item);
                }
                if !created[owner].iter().any(|known| known == entity) {
                    created[owner].push(entity.to_string());
                }
                // The same module's update, remove and state exports of
                // the entity: the page that adds records also edits and
                // deletes them (audits 6-8: goals, comments and recurring
                // rules could be added but not edited or deleted).
                for prefix in CHANGE_PREFIXES {
                    let changer = format!("{prefix}{entity}");
                    let item = format!("`{changer}` ({})", file.path);
                    if usage.declared[index].contains(&changer) && !changers[owner].contains(&item)
                    {
                        changers[owner].push(item);
                    }
                }
            }
        }
    }
    // The flags of each page: of the records it adds, else (a record kept
    // by `setBudget`, with no create export) of the static page named after
    // the record (`/budgets`).
    let mut flag_lines: Vec<Vec<String>> = vec![Vec::new(); count];
    for (owner, field) in &flags {
        let lower = camel_words(owner).concat();
        let page = pages
            .iter()
            .copied()
            .find(|&page| created[page].contains(owner))
            .or_else(|| {
                pages.iter().copied().find(|&page| {
                    let route = routes[page].as_deref().unwrap_or("");
                    !route.contains('[')
                        && route
                            .rsplit('/')
                            .next()
                            .map(|last| singular(&last.replace('-', "")))
                            .is_some_and(|last| last == lower)
                })
            });
        if let Some(page) = page {
            flag_lines[page].push(format!("`{owner}.{field}`"));
        }
    }

    (0..count)
        .map(|index| {
            if routes[index].is_none() {
                return String::new();
            }
            let mut out = String::new();
            let shared: Vec<String> = (0..count)
                .filter(|&other| usage.is_component(other) && usage.names(index, other))
                .map(|other| {
                    let file = &files[other];
                    let purpose = planner_purpose(&file.purpose, staging);
                    let purpose = purpose.trim().trim_end_matches('.');
                    if purpose.is_empty() {
                        format!("`{}` ({})", stem(&file.path), file.path)
                    } else {
                        format!("`{}` ({}: {purpose})", stem(&file.path), file.path)
                    }
                })
                .collect();
            if !shared.is_empty() {
                out.push_str(&format!(
                    "\n- Shared components this page renders (import each one from its file and \
                     give it the data and callbacks that its contract takes; do not render again \
                     what its purpose says it renders): {}",
                    shared.join("; ")
                ));
            }
            if has_local_components(&files[index].purpose, &staging.quality) {
                // The local note alone still let pages import a local
                // component as a file (20261116-115401: 7 TS2307 errors,
                // one per page).
                let existing: Vec<String> = (0..count)
                    .filter(|&other| glob_match(staging.quality.component_glob, &files[other].path))
                    .map(|other| format!("`{}`", files[other].path))
                    .collect();
                let list = if existing.is_empty() {
                    "none".to_string()
                } else {
                    existing.join(", ")
                };
                out.push_str(&format!(
                    "\n- Component files: the only component files that exist are {list}; this \
                     page imports no other component file, and writes every other component it \
                     renders in this file."
                ));
            }
            let settings = routes[index]
                .as_deref()
                .and_then(|route| route.rsplit('/').next())
                .is_some_and(|last| last == "settings" || last == "preferences");
            let mut linked: Vec<&str> = Vec::new();
            let mut links: Vec<String> = Vec::new();
            for target in &targets {
                if settings
                    || target.page == index
                    || linked.contains(&target.entity.as_str())
                    || !mentions[index].contains(&target.entity)
                {
                    continue;
                }
                linked.push(&target.entity);
                links.push(format!(
                    "each {} that this page shows links to `{}`",
                    target.entity, target.href
                ));
            }
            if !links.is_empty() {
                out.push_str(&format!(
                    "\n- Links between pages (from the routes): {} (the card, row, list item, \
                     calendar entry or search result is the link, or holds one).",
                    links.join("; ")
                ));
            }
            for target in targets
                .iter()
                .filter(|target| target.page == index && target.one)
            {
                let entity = &target.entity;
                out.push_str(&format!(
                    "\n- This page shows one {entity}: it reads the {entity}'s id from the query \
                     string (`{}`), opens the default {entity} when the settings name one and \
                     none is given, else the first, and has a control to pick another {entity}.",
                    target.href
                ));
            }
            if !creators[index].is_empty() {
                out.push_str(&format!(
                    "\n- Controls to add records: this page has a form or a button for each of \
                     these, wired to it, and shows the new record at once: {}",
                    creators[index].join(", ")
                ));
            }
            // Audit 9: a per-record option (`Budget.rollover`) that no form
            // offered stayed `false` for every new record.
            let own_flags = &flag_lines[index];
            if !own_flags.is_empty() {
                out.push_str(&format!(
                    "\n- Record flags: the form that adds or edits these records offers {} (a \
                     checkbox each), saved through the create or update export (in its record or \
                     input object, or as its optional last argument when the export takes one).",
                    own_flags.join(", ")
                ));
            }
            if !changers[index].is_empty() {
                out.push_str(&format!(
                    "\n- Controls to change records: where this page lists records, each one has \
                     {}, wired to these: {}",
                    change_controls(&changers[index]),
                    changers[index].join(", ")
                ));
            }
            out
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::architect::{apply_manifest_passes, plan_from_manifest, PlanOptions};
    use crate::profile::{NEXTJS, PYTHON};

    fn file(path: &str, purpose: &str, exports: &[&str]) -> ManifestFile {
        ManifestFile {
            path: path.to_string(),
            purpose: purpose.to_string(),
            exports: exports.iter().map(|line| line.to_string()).collect(),
            uses: Vec::new(),
        }
    }

    const OBJECTIVE: &str = "Build \"TaskFlow\", a project management web app with Next.js and \
TypeScript.\n\nPages:\n- Dashboard: summary cards and recent tasks.\n- Projects: list of \
projects; create, rename, archive.\n- Board: a kanban board for one project; move tasks between \
columns.\n- Task detail: edit title; subtasks; comments.\n- Calendar: tasks on their due \
dates.\n- Settings: theme and default project.\n\nQuality bar: every page is complete.\n";

    /// A TaskFlow-like plan: a layout with its shell, pages that name the
    /// components they render (`renders <X/>`), one component on two pages,
    /// one component inside another and one that no file names.
    fn manifest() -> Manifest {
        let mut uses_comment = file(
            "components/CommentList.tsx",
            "Comment thread with add form.",
            &["export default function CommentList(props: { comments: Comment[]; onAdd: (body: string) => void })  // empty -> 'No comments'"],
        );
        uses_comment.uses = vec!["lib/comments.ts:addComment".to_string()];
        Manifest {
            files: vec![
                file(
                    "lib/types.ts",
                    "Shared types.",
                    &[
                        "export interface Task { id: string; title: string }",
                        "export interface Project { id: string; name: string }",
                        "export interface Comment { id: string; body: string }",
                        "export interface Settings { theme: string; defaultProjectId: string }  // theme: light or dark -> components/SettingsForm.tsx; defaultProjectId: board opens -> app/board/page.tsx",
                        "export interface Activity { id: string; text: string }",
                    ],
                ),
                file(
                    "lib/tasks.ts",
                    "Task operations.",
                    &["export function createTask(tasks: Task[], title: string): Task[]  // appends"],
                ),
                file(
                    "lib/projects.ts",
                    "Project operations.",
                    &["export function createProject(projects: Project[], name: string): Project[]"],
                ),
                file(
                    "lib/comments.ts",
                    "Comments.",
                    &["export function addComment(comments: Comment[], body: string): Comment[]"],
                ),
                file(
                    "lib/activity.ts",
                    "Activity log.",
                    &["export function addActivity(items: Activity[], text: string): Activity[]"],
                ),
                file(
                    "app/layout.tsx",
                    "Root layout.",
                    &["export default function RootLayout({ children }: { children: React.ReactNode })  // renders <AppShell/>"],
                ),
                file(
                    "components/AppShell.tsx",
                    "Navigation shell.",
                    &["export default function AppShell({ children }: { children: React.ReactNode })  // nav links"],
                ),
                file(
                    "app/page.tsx",
                    "Dashboard with summary cards.",
                    &["export default function DashboardPage()  // renders <SummaryCards/> and <TaskCard/>; recent tasks"],
                ),
                file(
                    "components/SummaryCards.tsx",
                    "Dashboard summary metric cards.",
                    &["export default function SummaryCards(props: { open: number })  // three cards"],
                ),
                file(
                    "components/TaskCard.tsx",
                    "Compact task card.",
                    &["export default function TaskCard(props: { task: Task; onOpen: (id: string) => void })"],
                ),
                file(
                    "app/projects/page.tsx",
                    "Projects list.",
                    &["export default async function ProjectsPage()  // renders <ProjectForm/>; lists projects"],
                ),
                file(
                    "components/ProjectForm.tsx",
                    "Create and rename project form.",
                    &["export default function ProjectForm(props: { onSubmit: (name: string) => void })  // trims name"],
                ),
                file(
                    "app/board/page.tsx",
                    "Kanban board for one project.",
                    &["export default function BoardPage()  // renders <BoardColumn/>; moves call lib/tasks"],
                ),
                file(
                    "components/BoardColumn.tsx",
                    "Kanban column with cards.",
                    &["export default function BoardColumn(props: { tasks: Task[] })  // renders one <TaskCard/> per task"],
                ),
                file(
                    "app/tasks/[id]/page.tsx",
                    "Task detail with comments.",
                    &["export default function TaskDetailPage()  // renders <CommentList/>"],
                ),
                uses_comment,
                file(
                    "app/calendar/page.tsx",
                    "Month calendar.",
                    &["export default function CalendarPage()  // tasks on due dates"],
                ),
                file(
                    "app/settings/page.tsx",
                    "Settings.",
                    &["export default function SettingsPage()  // renders <SettingsForm/>"],
                ),
                file(
                    "components/SettingsForm.tsx",
                    "Theme and default project controls.",
                    &["export default function SettingsForm(props: { settings: Settings; onSave: (s: Settings) => void })"],
                ),
                file(
                    "components/Unused.tsx",
                    "A widget no page names.",
                    &["export default function Unused()"],
                ),
            ],
            test_files: vec![file("tests/all.test.ts", "", &[])],
            readme: false,
        }
    }

    #[test]
    fn a_page_that_names_a_component_with_no_file_gets_the_local_note() {
        // The planned accounts page of race 20261116-051055: the planner left
        // the two components out (the page-local rule), the page imported them.
        let mut manifest = Manifest {
            files: vec![
                file("lib/types.ts", "Types.", &["export interface Account { id: string }"]),
                file(
                    "app/accounts/page.tsx",
                    "Accounts page with create, rename and close controls.",
                    &["export default function AccountsPage()  // renders <AccountForm/> and <AccountList/> and <Money/>; uses useState<Account[]>; wraps <Suspense> around <Link href>"],
                ),
                file("components/Money.tsx", "Money.", &["export default function Money(props: { cents: number })"]),
                file("app/page.tsx", "Overview.", &["export default function Home()  // renders <Money/>"]),
            ],
            test_files: Vec::new(),
            readme: false,
        };
        fold_page_components(&mut manifest, &NEXTJS);
        let quality = &NEXTJS.staging.quality;
        let accounts = &manifest.files[1];
        assert!(
            has_local_components(&accounts.purpose, quality),
            "{}",
            accounts.purpose
        );
        assert!(
            accounts.purpose.contains("`AccountForm`")
                && accounts.purpose.contains("`AccountList`")
        );
        // Generic type arguments and framework components are not local components.
        assert!(
            !accounts.purpose.contains("`Account`")
                && !accounts.purpose.contains("`Suspense`")
                && !accounts.purpose.contains("`Link`")
        );
        // A planned shared component stays a file and its page gets no note.
        let home = manifest
            .files
            .iter()
            .find(|file| file.path == "app/page.tsx")
            .unwrap();
        assert!(!home.purpose.contains("`Money`"));
        assert!(!manifest.files[1].purpose.contains("`Money`"));
        assert!(manifest
            .files
            .iter()
            .any(|file| file.path == "components/Money.tsx"));
        // Idempotent.
        let before = manifest.clone();
        fold_page_components(&mut manifest, &NEXTJS);
        assert_eq!(
            manifest
                .files
                .iter()
                .map(|f| &f.purpose)
                .collect::<Vec<_>>(),
            before.files.iter().map(|f| &f.purpose).collect::<Vec<_>>()
        );
    }

    fn paths(manifest: &Manifest) -> Vec<&str> {
        manifest
            .files
            .iter()
            .map(|file| file.path.as_str())
            .collect()
    }

    fn purpose<'a>(manifest: &'a Manifest, path: &str) -> &'a str {
        &manifest
            .files
            .iter()
            .find(|file| file.path == path)
            .unwrap()
            .purpose
    }

    #[test]
    fn a_component_of_one_page_goes_into_that_page() {
        let mut folded = manifest();
        let folds = fold_page_components(&mut folded, &NEXTJS);
        let pairs: Vec<(&str, Vec<&str>)> = folds
            .iter()
            .map(|fold| {
                (
                    fold.page.as_str(),
                    fold.components.iter().map(String::as_str).collect(),
                )
            })
            .collect();
        assert_eq!(
            pairs,
            [
                ("app/page.tsx", vec!["components/SummaryCards.tsx"]),
                ("app/projects/page.tsx", vec!["components/ProjectForm.tsx"]),
                ("app/board/page.tsx", vec!["components/BoardColumn.tsx"]),
                (
                    "app/tasks/[id]/page.tsx",
                    vec!["components/CommentList.tsx"]
                ),
                ("app/settings/page.tsx", vec!["components/SettingsForm.tsx"]),
            ]
        );
        let left = paths(&folded);
        for gone in [
            "components/SummaryCards.tsx",
            "components/ProjectForm.tsx",
            "components/BoardColumn.tsx",
            "components/CommentList.tsx",
            "components/SettingsForm.tsx",
        ] {
            assert!(!left.contains(&gone), "{gone}");
        }
        // The page's purpose has the component's purpose and signature as a
        // local declaration.
        let projects = purpose(&folded, "app/projects/page.tsx");
        assert!(projects.starts_with("Projects list. Local components of this file"));
        assert!(projects.contains(
            "`ProjectForm` (Create and rename project form; function ProjectForm(props: { onSubmit: \
             (name: string) => void })  // trims name)"
        ));
        assert!(projects.ends_with("gives each one its data and callbacks as props."));
        // A page with local components is not an async Server Component.
        let page = folded
            .files
            .iter()
            .find(|file| file.path == "app/projects/page.tsx")
            .unwrap();
        assert!(page.exports[0].starts_with("export default function ProjectsPage()"));
        // A reader in the settings comment names the page now.
        assert!(folded.files[0].exports[3].contains("-> app/settings/page.tsx;"));
        assert!(!folded.files[0].exports[3].contains("components/SettingsForm.tsx"));
        // The component's calls are the page's.
        let detail = folded
            .files
            .iter()
            .find(|file| file.path == "app/tasks/[id]/page.tsx")
            .unwrap();
        assert_eq!(detail.uses, ["lib/comments.ts:addComment"]);
        // Idempotent.
        let mut again = folded.clone();
        assert!(fold_page_components(&mut again, &NEXTJS).is_empty());
        assert_eq!(
            serde_json::to_string(&again).unwrap(),
            serde_json::to_string(&folded).unwrap()
        );
    }

    #[test]
    fn shared_layout_and_unnamed_components_stay_files() {
        let mut folded = manifest();
        fold_page_components(&mut folded, &NEXTJS);
        let left = paths(&folded);
        // The layout's shell, a card on the dashboard and inside a board
        // component (two pages), and a component that no file names.
        for kept in [
            "components/AppShell.tsx",
            "components/TaskCard.tsx",
            "components/Unused.tsx",
        ] {
            assert!(left.contains(&kept), "{kept}");
        }
        // The lib modules and tests are untouched.
        assert_eq!(folded.files[1].purpose, "Task operations.");
        assert_eq!(folded.test_files.len(), 1);
    }

    #[test]
    fn a_dynamic_route_page_and_a_nested_component_fold_too() {
        // A component that only a folded component renders goes into the
        // same page.
        let mut manifest = manifest();
        manifest.files.push(file(
            "components/CommentItem.tsx",
            "One comment.",
            &["export function CommentItem(props: { comment: Comment })"],
        ));
        manifest.files[15].exports[0].push_str("; renders <CommentItem/> per comment");
        let folds = fold_page_components(&mut manifest, &NEXTJS);
        let detail = folds
            .iter()
            .find(|fold| fold.page == "app/tasks/[id]/page.tsx")
            .unwrap();
        assert_eq!(
            detail.components,
            ["components/CommentList.tsx", "components/CommentItem.tsx"]
        );
        let text = purpose(&manifest, "app/tasks/[id]/page.tsx");
        assert!(text.contains("`CommentList` (Comment thread with add form;"));
        assert!(text.contains(
            "`CommentItem` (One comment; function CommentItem(props: { comment: Comment }))"
        ));
    }

    #[test]
    fn nothing_changes_with_the_switch_off_or_for_other_profiles() {
        let manifest = manifest();
        let json = |value: &Manifest| serde_json::to_string(value).unwrap();
        // Off: the passes and the plan are those of the default options.
        let mut passed = manifest.clone();
        apply_manifest_passes(&mut passed, &NEXTJS, &PlanOptions::default());
        assert_eq!(json(&passed), json(&manifest));
        let off = PlanOptions {
            objective_pages: true,
            quality_rules: true,
            ..PlanOptions::default()
        };
        let plan = plan_from_manifest(OBJECTIVE, &manifest, 64, Some(&NEXTJS), &off).unwrap();
        for task in &plan.tasks {
            assert!(
                !task.prompt.contains("Local components of this file"),
                "{}",
                task.id
            );
            assert!(!task
                .prompt
                .contains(NEXTJS.staging.quality.shared_component_rule));
            assert!(!task.prompt.contains("Links between pages"));
        }
        // PYTHON has no local note: the pass does nothing.
        let mut python = manifest.clone();
        assert!(fold_page_components(&mut python, &PYTHON).is_empty());
        assert_eq!(json(&python), json(&manifest));
        assert!(component_lines(&manifest, &PYTHON)
            .iter()
            .all(String::is_empty));
    }

    #[test]
    fn shared_components_are_presentational() {
        let mut folded = manifest();
        fold_page_components(&mut folded, &NEXTJS);
        let rule = NEXTJS.staging.quality.shared_component_rule;
        assert!(rule.contains("takes its data through props"));
        assert!(rule.contains("callback prop"));
        assert!(rule.contains("never loads its own copy of the app state"));
        let lines = component_lines(&folded, &NEXTJS);
        let line_of = |path: &str| {
            let index = folded
                .files
                .iter()
                .position(|file| file.path == path)
                .unwrap();
            lines[index].as_str()
        };
        assert_eq!(line_of("components/TaskCard.tsx"), format!("\n- {rule}"));
        // The layout's shell and a provider keep their own job.
        assert_eq!(line_of("components/AppShell.tsx"), "");
        assert_eq!(line_of("app/page.tsx"), "");
        // In the plan: the shared component's prompt ends with the rule.
        let on = PlanOptions {
            page_local: true,
            ..PlanOptions::default()
        };
        let plan = plan_from_manifest(OBJECTIVE, &manifest(), 64, Some(&NEXTJS), &on).unwrap();
        let card = plan
            .tasks
            .iter()
            .find(|task| task.title == "components/TaskCard.tsx")
            .unwrap();
        assert!(card.prompt.ends_with(rule), "{}", card.prompt);
        assert!(!plan
            .tasks
            .iter()
            .any(|task| task.title == "components/BoardColumn.tsx"));
        // The module head lists the page without the note (README too).
        let board = plan
            .tasks
            .iter()
            .find(|task| task.title == "app/board/page.tsx")
            .unwrap();
        assert!(board
            .prompt
            .contains("- Purpose: Kanban board for one project. Local components"));
        assert_eq!(
            board
                .prompt
                .matches("Local components of this file")
                .count(),
            1
        );
        // A page with local components writes their code too; with the
        // switch, so may any page (its parts planned inline).
        assert_eq!(board.max_tokens, 8_192);
        assert_eq!(card.max_tokens, 4_096);
        let calendar = plan
            .tasks
            .iter()
            .find(|task| task.title == "app/calendar/page.tsx")
            .unwrap();
        assert_eq!(calendar.max_tokens, 8_192);
    }

    #[test]
    fn pages_get_room_for_their_parts_without_a_fold_and_on_a_replay() {
        let tokens = |plan: &crate::dag::Plan, path: &str| {
            plan.tasks
                .iter()
                .find(|task| task.title == path)
                .unwrap()
                .max_tokens
        };
        // A plan that followed the planner rule: no component files, the
        // parts inline in the pages, nothing to fold. A page that still names
        // a part gets the local note for it (race 20261116-051055 imported
        // them instead).
        let mut inline = manifest();
        inline
            .files
            .retain(|file| !file.path.starts_with("components/"));
        let on = PlanOptions {
            page_local: true,
            ..PlanOptions::default()
        };
        let mut passed = inline.clone();
        apply_manifest_passes(&mut passed, &NEXTJS, &on);
        assert!(passed
            .files
            .iter()
            .any(|file| has_local_components(&file.purpose, &NEXTJS.staging.quality)));
        let plan = plan_from_manifest(OBJECTIVE, &inline, 64, Some(&NEXTJS), &on).unwrap();
        assert_eq!(tokens(&plan, "app/tasks/[id]/page.tsx"), 8_192);
        assert_eq!(tokens(&plan, "lib/tasks.ts"), 4_096);
        // Off: as before.
        let off = plan_from_manifest(
            OBJECTIVE,
            &inline,
            64,
            Some(&NEXTJS),
            &PlanOptions::default(),
        )
        .unwrap();
        assert_eq!(tokens(&off, "app/tasks/[id]/page.tsx"), 4_096);
        // A folded manifest replayed with the switch off (`rerepair`): the
        // pages that hold parts keep their room, the others do not.
        let mut folded = manifest();
        fold_page_components(&mut folded, &NEXTJS);
        let replay = plan_from_manifest(
            OBJECTIVE,
            &folded,
            64,
            Some(&NEXTJS),
            &PlanOptions::default(),
        )
        .unwrap();
        assert_eq!(tokens(&replay, "app/board/page.tsx"), 8_192);
        assert_eq!(tokens(&replay, "app/calendar/page.tsx"), 4_096);
    }

    /// The task page of saved run q123b-2: it names only its subtasks; its
    /// form and comments, the board's filters and the search results are
    /// named by no file.
    fn unnamed_manifest() -> Manifest {
        Manifest {
            files: vec![
                file(
                    "lib/types.ts",
                    "Shared types.",
                    &[
                        "export interface Task { id: string; title: string }",
                        "export interface Project { id: string; name: string }",
                    ],
                ),
                file(
                    "lib/comments.ts",
                    "Comments.",
                    &["export function addComment(taskId: string, body: string): void"],
                ),
                file(
                    "components/AppProvider.tsx",
                    "Client context providing AppState and dispatch.",
                    &[
                        "export function AppProvider(props: {children: React.ReactNode}): React.ReactElement",
                        "export function useApp(): {state: AppState}",
                    ],
                ),
                file(
                    "components/AppState.tsx",
                    "Client state wiring lib to UI.",
                    &["export function useAppState(): AppState"],
                ),
                file(
                    "app/layout.tsx",
                    "Root layout with provider and nav.",
                    &["export default function RootLayout(props: { children: React.ReactNode })"],
                ),
                file(
                    "app/page.tsx",
                    "Dashboard page.",
                    &["export default function DashboardPage()  // recent tasks"],
                ),
                file(
                    "components/SummaryCards.tsx",
                    "Dashboard summary cards.",
                    &["export default function SummaryCards({ open }: { open: number })"],
                ),
                file(
                    "app/board/page.tsx",
                    "Kanban board page for one project.",
                    &["export default function BoardPage()  // renders <BoardColumns/>"],
                ),
                file(
                    "components/BoardColumns.tsx",
                    "Kanban columns.",
                    &["export default function BoardColumns(props: { tasks: Task[] })"],
                ),
                file(
                    "components/BoardFilters.tsx",
                    "Assignee and priority filter controls.",
                    &["export default function BoardFilters(props: { value: string; onChange: (next: string) => void })"],
                ),
                file(
                    "app/tasks/[id]/page.tsx",
                    "Task detail page.",
                    &["export default async function TaskDetailPage({ params }: { params: Promise<{ id: string }> })  // renders <SubtaskList/>"],
                ),
                file(
                    "components/SubtaskList.tsx",
                    "Subtask checklist UI.",
                    &["export default function SubtaskList({ taskId }: { taskId: string })"],
                ),
                file(
                    "components/TaskForm.tsx",
                    "Task detail edit form.",
                    &["export default function TaskForm(props: { task: Task; onSave: (task: Task) => void })"],
                ),
                file(
                    "components/CommentList.tsx",
                    "Comment list and add form.",
                    &["export default function CommentList({ taskId }: { taskId: string })  // add via lib/comments"],
                ),
                file(
                    "app/search/page.tsx",
                    "Global search page.",
                    &["export default async function SearchPage({ searchParams }: { searchParams: Promise<{ q?: string }> })  // renders <SearchPanel/>"],
                ),
                file(
                    "components/SearchResults.tsx",
                    "Search results with highlighted matches.",
                    &["export default function SearchResults(props: { results: Task[] })"],
                ),
                file(
                    "components/StatusBadge.tsx",
                    "Colored status pill.",
                    &["export default function StatusBadge(props: { status: string })"],
                ),
                file(
                    "components/IdOnly.tsx",
                    "A widget of one record.",
                    &["export default function IdOnly({ recordId }: { recordId: string })"],
                ),
            ],
            test_files: Vec::new(),
            readme: false,
        }
    }

    #[test]
    fn a_component_no_file_names_goes_to_the_page_it_belongs_to() {
        let mut folded = unnamed_manifest();
        let folds = fold_page_components(&mut folded, &NEXTJS);
        let of = |page: &str| -> Vec<&str> {
            folds
                .iter()
                .find(|fold| fold.page == page)
                .map(|fold| fold.components.iter().map(String::as_str).collect())
                .unwrap_or_default()
        };
        // By its stem (`board`, `search`), by its id prop (`taskId`: the
        // task route) or its stem (`task`), by its purpose ("Dashboard").
        assert_eq!(of("app/page.tsx"), ["components/SummaryCards.tsx"]);
        assert_eq!(
            of("app/board/page.tsx"),
            ["components/BoardColumns.tsx", "components/BoardFilters.tsx"]
        );
        assert_eq!(
            of("app/tasks/[id]/page.tsx"),
            [
                "components/SubtaskList.tsx",
                "components/TaskForm.tsx",
                "components/CommentList.tsx"
            ]
        );
        assert_eq!(of("app/search/page.tsx"), ["components/SearchResults.tsx"]);
        let left = paths(&folded);
        // State owners (a provider, a hook), a part that matches no page and
        // one whose id names no route stay files.
        for kept in [
            "components/AppProvider.tsx",
            "components/AppState.tsx",
            "components/StatusBadge.tsx",
            "components/IdOnly.tsx",
        ] {
            assert!(left.contains(&kept), "{kept}");
        }
        // The components left: state owners and an id-only one get no
        // presentational rule; a plain one does.
        let lines = component_lines(&folded, &NEXTJS);
        let line_of = |path: &str| {
            let at = folded
                .files
                .iter()
                .position(|file| file.path == path)
                .unwrap();
            lines[at].as_str()
        };
        assert_eq!(line_of("components/AppProvider.tsx"), "");
        assert_eq!(line_of("components/AppState.tsx"), "");
        assert_eq!(line_of("components/IdOnly.tsx"), "");
        assert!(line_of("components/StatusBadge.tsx").contains("takes its data through props"));
        // The async pages that took in parts are Client Components now.
        let export = |path: &str| {
            folded
                .files
                .iter()
                .find(|file| file.path == path)
                .unwrap()
                .exports[0]
                .clone()
        };
        let detail = export("app/tasks/[id]/page.tsx");
        assert!(
            detail.starts_with("export default function TaskDetailPage()  // Client Component"),
            "{detail}"
        );
        assert!(detail.contains("useParams()"));
        let search = export("app/search/page.tsx");
        assert!(
            search.starts_with("export default function SearchPage()  // Client Component"),
            "{search}"
        );
        assert!(search.contains("useSearchParams()"));
        assert!(!search.contains("Promise"));
        // Idempotent.
        let mut again = folded.clone();
        assert!(fold_page_components(&mut again, &NEXTJS).is_empty());
        assert_eq!(
            serde_json::to_string(&again).unwrap(),
            serde_json::to_string(&folded).unwrap()
        );
    }

    #[test]
    fn a_component_that_a_lib_module_or_a_test_needs_stays_a_file() {
        let mut manifest = manifest();
        // `lib/board.ts` calls a helper of the board's column.
        let mut board = file(
            "lib/board.ts",
            "Board moves.",
            &["export function moveTask(tasks: Task[]): Task[]"],
        );
        board.uses = vec!["components/BoardColumn.tsx:groupTasks".to_string()];
        manifest.files.push(board);
        // A test names the project form's helper.
        manifest.files[11]
            .exports
            .push("export function cleanName(name: string): string".to_string());
        manifest.test_files[0].uses = vec!["components/ProjectForm.tsx:cleanName".to_string()];
        let folds = fold_page_components(&mut manifest, &NEXTJS);
        let left = paths(&manifest);
        assert!(left.contains(&"components/BoardColumn.tsx"));
        assert!(left.contains(&"components/ProjectForm.tsx"));
        assert!(!folds
            .iter()
            .any(|fold| fold.page == "app/projects/page.tsx" || fold.page == "app/board/page.tsx"));
        // The lib's entry still names the component, not a page.
        let lib = manifest
            .files
            .iter()
            .find(|file| file.path == "lib/board.ts")
            .unwrap();
        assert_eq!(lib.uses, ["components/BoardColumn.tsx:groupTasks"]);
    }

    #[test]
    fn a_route_parameter_that_names_its_entity_is_the_entity() {
        assert_eq!(
            dynamic_entity("/board/[projectId]").as_deref(),
            Some("project")
        );
        assert_eq!(
            dynamic_entity("/board/[project_id]").as_deref(),
            Some("project")
        );
        assert_eq!(dynamic_entity("/tasks/[id]").as_deref(), Some("task"));
        assert_eq!(dynamic_entity("/tasks/[slug]").as_deref(), Some("task"));
        assert_eq!(dynamic_entity("/tasks"), None);
        // In a plan: the projects page links to the board of a project.
        let mut manifest = manifest();
        manifest.files[12].path = "app/board/[projectId]/page.tsx".to_string();
        fold_page_components(&mut manifest, &NEXTJS);
        let bullets = crate::objective::page_bullets(OBJECTIVE);
        let paths: Vec<&str> = manifest
            .files
            .iter()
            .map(|file| file.path.as_str())
            .collect();
        let owners = crate::objective::match_pages(&paths, &bullets);
        let lines = page_lines(&manifest, &NEXTJS, &bullets, &owners);
        let at = manifest
            .files
            .iter()
            .position(|file| file.path == "app/projects/page.tsx")
            .unwrap();
        assert!(
            lines[at].contains("each project that this page shows links to `/board/${projectId}`"),
            "{}",
            lines[at]
        );
        assert!(!lines.iter().any(|line| line.contains("each board")));
    }

    const LEDGERLY: &str = "Build \"Ledgerly\", a personal finance web app with Next.js and \
TypeScript.\n\nPages:\n- Overview: net worth and cash flow.\n- Accounts: list of accounts; \
create, rename, close.\n- Transactions: a table of transactions; add, edit, delete, split.\n- \
Transaction detail: edit one transaction and its splits.\n- Budgets: spending for one month per \
category, with rollover.\n- Settings: currency and theme.\n\nQuality bar: every page is complete.\n";

    /// A Ledgerly plan like saved run 20261115-141049: `upsertTransaction`,
    /// `createCategory` with `Category` in a lib module, `saveState`.
    fn ledgerly() -> Manifest {
        Manifest {
            files: vec![
                file(
                    "lib/types.ts",
                    "Shared types.",
                    &[
                        "export interface Account  // {id:string,name:string}",
                        "export interface Transaction  // {id:string,accountId:string,category:string}",
                        "export interface AppState  // {accounts:Account[],transactions:Transaction[],categories:string[]}",
                        "export interface Settings  // {currency:string,theme:string}",
                    ],
                ),
                file(
                    "lib/categories.ts",
                    "Categories.",
                    &[
                        "export interface Category {id:string;name:string}",
                        "export function createCategory(n: string): Category",
                    ],
                ),
                file(
                    "lib/accounts.ts",
                    "Accounts.",
                    &["export function createAccount(n: string): Account"],
                ),
                file(
                    "lib/transactions.ts",
                    "Transactions.",
                    &["export function upsertTransaction(list: Transaction[], t: Transaction): Transaction[]"],
                ),
                file(
                    "lib/storage.ts",
                    "Storage.",
                    &[
                        "export function saveState(s: AppState): void",
                        "export function saveSettings(s: Settings): void",
                    ],
                ),
                file("lib/ids.ts", "Ids.", &["export function createId(): string"]),
                file(
                    "app/page.tsx",
                    "Overview.",
                    &["export default function OverviewPage()"],
                ),
                file(
                    "app/accounts/page.tsx",
                    "Accounts list.",
                    &["export default function AccountsPage()"],
                ),
                file(
                    "app/transactions/page.tsx",
                    "Transactions table.",
                    &["export default function TransactionsPage()"],
                ),
                file(
                    "app/transactions/[id]/page.tsx",
                    "Transaction detail.",
                    &["export default function TransactionDetailPage()"],
                ),
                file(
                    "app/budgets/page.tsx",
                    "Monthly budgets by category.",
                    &["export default function BudgetsPage()"],
                ),
                file(
                    "app/settings/page.tsx",
                    "Settings.",
                    &["export default function SettingsPage()"],
                ),
            ],
            test_files: Vec::new(),
            readme: false,
        }
    }

    #[test]
    fn ledgerly_pages_get_links_and_controls_and_no_month_target() {
        let manifest = ledgerly();
        let bullets = crate::objective::page_bullets(LEDGERLY);
        let paths: Vec<&str> = manifest
            .files
            .iter()
            .map(|file| file.path.as_str())
            .collect();
        let owners = crate::objective::match_pages(&paths, &bullets);
        let lines = page_lines(&manifest, &NEXTJS, &bullets, &owners);
        let line_of = |path: &str| {
            let at = manifest
                .files
                .iter()
                .position(|file| file.path == path)
                .unwrap();
            lines[at].as_str()
        };
        let transactions = line_of("app/transactions/page.tsx");
        assert!(transactions
            .contains("each transaction that this page shows links to `/transactions/${id}`"));
        assert!(transactions.contains("`upsertTransaction` (lib/transactions.ts)"));
        assert!(line_of("app/accounts/page.tsx").contains("`createAccount` (lib/accounts.ts)"));
        // `createCategory`: `Category` is declared in a lib module.
        assert!(lines
            .iter()
            .any(|line| line.contains("`createCategory` (lib/categories.ts)")));
        // "for one month" names no record: no `?month=` target.
        assert!(!lines.iter().any(|line| line.contains("month=")));
        assert!(!lines
            .iter()
            .any(|line| line.contains("This page shows one")));
        // State, settings and ids are not records.
        for name in ["saveState", "saveSettings", "createId"] {
            assert!(!lines.iter().any(|line| line.contains(name)), "{name}");
        }
        // No record has an option flag: no flag line.
        assert!(!lines.iter().any(|line| line.contains("Record flags")));
    }

    /// Audit 9 (Ledgerly h2): `Budget.rollover` was read by the lib but no
    /// form offered it. The page that adds a record names its option flags;
    /// a record kept by `setBudget` (no create export) gets them on the page
    /// named after it.
    #[test]
    fn the_form_page_offers_the_option_flags_of_its_records() {
        let mut manifest = ledgerly();
        let types = &mut manifest.files[0].exports;
        types[0] =
            "export interface Account  // {id:string,name:string,pinned:boolean,closed:boolean}"
                .to_string();
        types.push(
            "export interface Budget { id: string; categoryId: string; limit: number; rollover: boolean }"
                .to_string(),
        );
        manifest.files.push(file(
            "lib/budgets.ts",
            "Budgets.",
            &["export function setBudget(budgets: Budget[], categoryId: string, limit: number): Budget[]"],
        ));
        let bullets = crate::objective::page_bullets(LEDGERLY);
        let paths: Vec<&str> = manifest
            .files
            .iter()
            .map(|file| file.path.as_str())
            .collect();
        let owners = crate::objective::match_pages(&paths, &bullets);
        let lines = page_lines(&manifest, &NEXTJS, &bullets, &owners);
        let line_of = |path: &str| {
            let at = manifest
                .files
                .iter()
                .position(|file| file.path == path)
                .unwrap();
            lines[at].as_str()
        };
        assert!(line_of("app/budgets/page.tsx").contains(
            "- Record flags: the form that adds or edits these records offers `Budget.rollover` (a \
             checkbox each), saved through the create or update export (in its record or input \
             object, or as its optional last argument when the export takes one)."
        ));
        let accounts = line_of("app/accounts/page.tsx");
        assert!(
            accounts.contains("offers `Account.pinned` (a checkbox each)"),
            "{accounts}"
        );
        // Closing is a reversible state, not an option of the form.
        assert!(!accounts.contains("Account.closed"), "{accounts}");
        assert!(!line_of("app/transactions/page.tsx").contains("Record flags"));
    }

    /// Audits 6-8: records could be added but not edited, deleted or
    /// unarchived. The page that adds them also gets their update, remove
    /// and state exports; a log stays without controls.
    #[test]
    fn the_page_that_adds_records_also_edits_deletes_and_unarchives_them() {
        let mut plan = manifest();
        let module = |plan: &Manifest, path: &str| {
            plan.files
                .iter()
                .position(|file| file.path == path)
                .unwrap()
        };
        let projects = module(&plan, "lib/projects.ts");
        for line in [
            "export function updateProject(projects: Project[], id: string, name: string): Project[]",
            "export function removeProject(projects: Project[], id: string): Project[]",
            "export function archiveProject(projects: Project[], id: string): Project[]",
            "export function unarchiveProject(projects: Project[], id: string): Project[]",
            "export function removeProjects(projects: Project[]): Project[]",
        ] {
            plan.files[projects].exports.push(line.to_string());
        }
        let comments = module(&plan, "lib/comments.ts");
        plan.files[comments].exports.push(
            "export function deleteComment(comments: Comment[], id: string): Comment[]".into(),
        );
        let activity = module(&plan, "lib/activity.ts");
        plan.files[activity].exports.push(
            "export function removeActivity(items: Activity[], id: string): Activity[]".into(),
        );
        fold_page_components(&mut plan, &NEXTJS);
        let bullets = crate::objective::page_bullets(OBJECTIVE);
        let paths: Vec<&str> = plan.files.iter().map(|file| file.path.as_str()).collect();
        let owners = crate::objective::match_pages(&paths, &bullets);
        let lines = page_lines(&plan, &NEXTJS, &bullets, &owners);
        let line_of = |path: &str| {
            let index = plan
                .files
                .iter()
                .position(|file| file.path == path)
                .unwrap();
            lines[index].as_str()
        };
        let page = line_of("app/projects/page.tsx");
        assert!(
            page.contains(
                "- Controls to change records: where this page lists records, each one has an \
                 edit control, a delete control and a control for each state change and its \
                 reverse, wired to these: `updateProject` (lib/projects.ts), `removeProject` \
                 (lib/projects.ts), `archiveProject` (lib/projects.ts), `unarchiveProject` \
                 (lib/projects.ts)"
            ),
            "{page}"
        );
        assert!(!page.contains("removeProjects"), "{page}");
        let detail = line_of("app/tasks/[id]/page.tsx");
        assert!(
            detail.contains("`deleteComment` (lib/comments.ts)"),
            "{detail}"
        );
        // Only a delete export: no edit control is asked for.
        assert!(
            detail.contains("each one has a delete control, wired to these"),
            "{detail}"
        );
        assert!(!lines.iter().any(|line| line.contains("removeActivity")));
        // A module with only a create export adds no change line.
        assert!(!line_of("app/board/page.tsx").contains("Controls to change records"));
    }

    #[test]
    fn change_controls_follow_the_exports_found() {
        let items = |names: &[&str]| -> Vec<String> {
            names
                .iter()
                .map(|name| format!("`{name}` (lib/x.ts)"))
                .collect()
        };
        assert_eq!(change_controls(&items(&["updateGoal"])), "an edit control");
        assert_eq!(
            change_controls(&items(&["archiveGoal"])),
            "a control for each state change"
        );
        assert_eq!(
            change_controls(&items(&["archiveGoal", "unarchiveGoal"])),
            "a control for each state change and its reverse"
        );
        assert_eq!(
            change_controls(&items(&["renameGoal", "deleteGoal"])),
            "an edit control and a delete control"
        );
    }

    #[test]
    fn pages_get_their_shared_components_links_and_create_controls() {
        let mut folded = manifest();
        fold_page_components(&mut folded, &NEXTJS);
        let bullets = crate::objective::page_bullets(OBJECTIVE);
        let paths: Vec<&str> = folded.files.iter().map(|file| file.path.as_str()).collect();
        let owners = crate::objective::match_pages(&paths, &bullets);
        let lines = page_lines(&folded, &NEXTJS, &bullets, &owners);
        let line_of = |path: &str| {
            let index = folded
                .files
                .iter()
                .position(|file| file.path == path)
                .unwrap();
            lines[index].as_str()
        };
        // The dashboard renders a shared card: it imports it and does not
        // render its part again.
        let dashboard = line_of("app/page.tsx");
        assert!(dashboard.contains(
            "- Shared components this page renders (import each one from its file and give it \
             the data and callbacks that its contract takes; do not render again what its \
             purpose says it renders): `TaskCard` (components/TaskCard.tsx: Compact task card)"
        ));
        // Tasks link to their detail route, projects to the board of one
        // project.
        assert!(dashboard.contains("each task that this page shows links to `/tasks/${id}`"));
        let calendar = line_of("app/calendar/page.tsx");
        assert!(calendar.contains("each task that this page shows links to `/tasks/${id}`"));
        let projects = line_of("app/projects/page.tsx");
        assert!(
            projects.contains("each project that this page shows links to `/board?project=${id}`")
        );
        assert!(projects.contains("Controls to add records"));
        assert!(projects.contains("`createProject` (lib/projects.ts)"));
        // A page with local components hears which component files exist.
        assert!(projects
            .contains("- Component files: the only component files that exist are `components/"));
        assert!(projects.contains("this page imports no other component file"));
        assert!(!projects.contains("createTask"));
        // The board shows one project and owns the tasks ("move tasks").
        let board = line_of("app/board/page.tsx");
        assert!(board.contains(
            "- This page shows one project: it reads the project's id from the query string \
             (`/board?project=${id}`)"
        ));
        assert!(board.contains("`createTask` (lib/tasks.ts)"));
        assert!(!board.contains("links to `/board"));
        // The detail page owns the comments; it does not link to itself.
        let detail = line_of("app/tasks/[id]/page.tsx");
        assert!(detail.contains("`addComment` (lib/comments.ts)"));
        assert!(!detail.contains("links to `/tasks/${id}`"));
        // A log gets no control, a settings page no links, other files nothing.
        assert!(!lines.iter().any(|line| line.contains("addActivity")));
        assert!(!line_of("app/settings/page.tsx").contains("Links between pages"));
        assert_eq!(line_of("lib/tasks.ts"), "");
        assert_eq!(line_of("components/TaskCard.tsx"), "");

        // A static detail page (`/task?id=`) is the target of the task links,
        // and the list rule does not give it `createTask`.
        let mut query = manifest();
        query.files[14].path = "app/task/page.tsx".to_string();
        fold_page_components(&mut query, &NEXTJS);
        let paths: Vec<&str> = query.files.iter().map(|file| file.path.as_str()).collect();
        let owners = crate::objective::match_pages(&paths, &bullets);
        let lines = page_lines(&query, &NEXTJS, &bullets, &owners);
        let at = |path: &str| {
            query
                .files
                .iter()
                .position(|file| file.path == path)
                .unwrap()
        };
        assert!(lines[at("app/calendar/page.tsx")].contains("links to `/task?id=${id}`"));
        assert!(!lines[at("app/task/page.tsx")].contains("createTask"));
        assert!(lines[at("app/board/page.tsx")].contains("createTask"));
    }

    #[test]
    fn with_objective_pages_the_page_prompt_owns_its_local_components() {
        let on = PlanOptions {
            page_local: true,
            objective_pages: true,
            ..PlanOptions::default()
        };
        let plan = plan_from_manifest(OBJECTIVE, &manifest(), 64, Some(&NEXTJS), &on).unwrap();
        let board = plan
            .tasks
            .iter()
            .find(|task| task.title == "app/board/page.tsx")
            .unwrap();
        assert!(board.prompt.contains(
            "You own every feature in this bullet, the local components of this file included: \
             Board:"
        ));
        assert!(board.prompt.contains("Links between pages"));
        // The head lists purposes without the notes.
        let head = &board.prompt[..board.prompt.find("Now write the COMPLETE").unwrap()];
        assert!(!head.contains("Local components"));
        assert!(head.contains("- `app/board/page.tsx` (Kanban board for one project):"));
        // A page without local components keeps the old words.
        let calendar = plan
            .tasks
            .iter()
            .find(|task| task.title == "app/calendar/page.tsx")
            .unwrap();
        assert!(calendar
            .prompt
            .contains("You own every feature in this bullet: Calendar:"));
    }

    #[test]
    fn the_contract_check_does_not_ask_for_folded_components() {
        let mut folded = manifest();
        fold_page_components(&mut folded, &NEXTJS);
        let problems = crate::contract::contract_problems(&folded, &NEXTJS);
        let unrendered: Vec<&str> = problems
            .iter()
            .filter(|problem| problem.text.starts_with("no page renders"))
            .map(|problem| problem.text.as_str())
            .collect();
        // Only the component that no file names, as before the fold.
        assert_eq!(unrendered.len(), 1, "{unrendered:?}");
        assert!(unrendered[0].contains("`Unused`"));
    }

    #[test]
    fn the_planner_rule_comes_with_the_architect_option() {
        let rule = NEXTJS.staging.quality.page_local_rule;
        assert!(rule.contains("a component that only one page renders is not a\n  file of its own"));
        assert!(rule.contains("renders <TaskCard/>"));
        assert!(PYTHON.staging.quality.page_local_rule.is_empty());
        assert!(!crate::architect::ArchitectOptions::default().page_local);
        assert!(!PlanOptions::default().page_local);
    }
}
