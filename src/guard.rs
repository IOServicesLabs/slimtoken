//! Guards of the repair loop against problems that a repair round creates
//! in files it did not touch (`repair::RepairPolicy::guard`, off by
//! default). What a guard reads in a source comes from the profile's
//! [`GuardHooks`](crate::profile::GuardHooks); which files use which file
//! comes from a [`CodeIndex`]. [`TextIndex`] answers from the sources
//! themselves (the hooks' imports and exports); a host can give an index of
//! its own (the `bench/showdown` arm with indexio) behind the same guards.
//!
//! - **Export lock.** The repair of a file that other files import carries
//!   a block that lists the exports they use and where ([`lock_block`]).
//!   After the round the exported declarations are compared
//!   ([`Lock::broken`]); each changed one is a lock break, and the files
//!   that use it join the next joint repair of that file.
//! - **Root first.** When two or more files report that a module does not
//!   export the same name (TS2305, TS2724, TS2614, TS2459 with the NEXTJS
//!   hooks; [`root_first`]): a file that declares the name
//!   is fixed with no model call (`export` added, or the import pointed at
//!   that file); else the module gets one repair and the importers wait a
//!   round.
//! - **Knock-on files.** A file whose only problems are knock-on lines
//!   (TS7006) while a file it imports has problems is not repaired.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::profile::RepairHooks;

/// One place where a file uses an export.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Site {
    /// The exported name (`default` for the default export).
    pub name: String,
    /// 1-based line in the using file.
    pub line: usize,
    pub text: String,
    /// The function around the use, when the index knows it.
    pub caller: Option<String>,
}

/// A file that imports another file, and what it uses of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct User {
    pub path: String,
    /// The exported names it imports.
    pub names: Vec<String>,
    pub sites: Vec<Site>,
}

/// Where the guards look up who uses what.
#[async_trait]
pub trait CodeIndex: Send + Sync {
    /// A short name for reports (`text`, `indexio`).
    fn name(&self) -> &str;

    /// The files among `files` (path to text) that import `path`, with the
    /// names they import and their use sites.
    async fn users(
        &self,
        path: &str,
        files: &BTreeMap<String, String>,
        hooks: &RepairHooks,
    ) -> Vec<User>;

    /// After a round changed `path` from `old` to `new` and broke the
    /// exports `names`: the files (among `files`, the new state) that use
    /// one of them. The default reads the imports of `files`.
    async fn affected(
        &self,
        path: &str,
        old: &str,
        new: &str,
        names: &[String],
        files: &BTreeMap<String, String>,
        hooks: &RepairHooks,
    ) -> Vec<String> {
        let _ = (old, new);
        text_affected(path, names, files, hooks)
    }

    /// The files changed (`files` is the whole new state): an index of its
    /// own catches up here. The default does nothing.
    async fn updated(&self, files: &BTreeMap<String, String>) {
        let _ = files;
    }
}

/// The [`CodeIndex`] that reads the sources: the importers from the hooks'
/// specifiers, the names from their import clauses, the sites from the
/// lines that name them.
#[derive(Debug, Default, Clone, Copy)]
pub struct TextIndex;

#[async_trait]
impl CodeIndex for TextIndex {
    fn name(&self) -> &str {
        "text"
    }

    async fn users(
        &self,
        path: &str,
        files: &BTreeMap<String, String>,
        hooks: &RepairHooks,
    ) -> Vec<User> {
        text_users(path, files, hooks)
    }
}

/// The specifiers of `user` (its text `source`) that resolve to `path`.
pub fn specs_to(
    user: &str,
    source: &str,
    path: &str,
    files: &BTreeMap<String, String>,
    hooks: &RepairHooks,
) -> Vec<String> {
    let known: BTreeSet<&str> = files.keys().map(String::as_str).collect();
    (hooks.local_specifiers)(source)
        .into_iter()
        .filter(|spec| (hooks.resolve_import)(user, spec, &known).as_deref() == Some(path))
        .collect()
}

/// True if `name` is in `line` as a whole word.
fn has_word(line: &str, name: &str) -> bool {
    line.match_indices(name).any(|(index, _)| {
        let before = line[..index].chars().next_back();
        let after = line[index + name.len()..].chars().next();
        let word = |ch: Option<char>| ch.is_some_and(|ch| ch.is_alphanumeric() || ch == '_');
        !word(before) && !word(after)
    })
}

/// Most use sites kept for one name of one file.
const SITES_PER_NAME: usize = 3;

/// The names that `user` imports from `path`, as (exported, local).
pub fn imported_from(
    user: &str,
    source: &str,
    path: &str,
    files: &BTreeMap<String, String>,
    hooks: &RepairHooks,
) -> Vec<(String, String)> {
    let mut names: Vec<(String, String)> = Vec::new();
    for spec in specs_to(user, source, path, files, hooks) {
        for pair in (hooks.guard.imported_names)(source, &spec) {
            if !names.contains(&pair) {
                names.push(pair);
            }
        }
    }
    names
}

/// The use sites of the local names `names` in `source`: lines that name
/// them and are not import lines.
pub fn text_sites(source: &str, names: &[(String, String)]) -> Vec<Site> {
    let mut sites = Vec::new();
    for (exported, local) in names {
        let mut count = 0;
        for (index, line) in source.lines().enumerate() {
            let trimmed = line.trim_start();
            if trimmed.starts_with("import ") || trimmed.starts_with("} from ") {
                continue;
            }
            if has_word(line, local) {
                sites.push(Site {
                    name: exported.clone(),
                    line: index + 1,
                    text: trimmed.trim_end().to_string(),
                    caller: None,
                });
                count += 1;
                if count >= SITES_PER_NAME {
                    break;
                }
            }
        }
    }
    sites
}

/// [`TextIndex::users`].
pub fn text_users(path: &str, files: &BTreeMap<String, String>, hooks: &RepairHooks) -> Vec<User> {
    let mut users = Vec::new();
    for (user, source) in files {
        if user == path {
            continue;
        }
        let names = imported_from(user, source, path, files, hooks);
        if names.is_empty() {
            continue;
        }
        users.push(User {
            path: user.clone(),
            names: names.iter().map(|(exported, _)| exported.clone()).collect(),
            sites: text_sites(source, &names),
        });
    }
    users
}

/// The files among `files` that import one of `names` from `path`.
pub fn text_affected(
    path: &str,
    names: &[String],
    files: &BTreeMap<String, String>,
    hooks: &RepairHooks,
) -> Vec<String> {
    text_users(path, files, hooks)
        .into_iter()
        .filter(|user| user.names.iter().any(|name| names.contains(name)))
        .map(|user| user.path)
        .collect()
}

/// What the guards did in one repair loop.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct GuardReport {
    /// The index that answered the lookups (`text`, `indexio`).
    pub index: String,
    /// Repairs that carried an export lock block.
    pub lock_blocks: usize,
    /// Locked exports whose declaration a round changed or removed.
    pub lock_breaks: usize,
    /// Files added to a joint repair because they use a broken export.
    pub followers_added: usize,
    /// Files fixed with no model call by the root-first guard.
    pub root_fixes: usize,
    /// Modules repaired because files import a name they do not export.
    pub root_repairs: usize,
    /// Importers held for a round while their module is repaired.
    pub held: usize,
    /// Files not repaired because their only problems were knock-on lines.
    pub knock_on_skipped: usize,
    /// Files not repaired in a round because the root-first guard fixed or
    /// repairs the module they import (or fixed or held their own lines):
    /// the next check shows what is left.
    pub root_waits: usize,
    /// Rounds that only applied root-first fixes, with no model request
    /// (counted in `RepairSummary::rounds` like any other round).
    pub root_only_rounds: usize,
}

/// The export lock of one repaired file.
#[derive(Debug, Clone)]
pub struct Lock {
    pub path: String,
    pub users: Vec<User>,
    /// The declaration of each used export before the round.
    pub decls: BTreeMap<String, String>,
}

impl Lock {
    /// The lock of `path` (its text `source`) with its `users`; `None` when
    /// no user names an export that `source` declares.
    pub fn new(path: &str, source: &str, users: Vec<User>, hooks: &RepairHooks) -> Option<Self> {
        let exports = (hooks.guard.exports)(source);
        let mut decls = BTreeMap::new();
        for user in &users {
            for name in &user.names {
                if let Some(export) = exports.iter().find(|export| export.name == *name) {
                    decls.insert(name.clone(), export.decl.clone());
                }
            }
        }
        (!decls.is_empty()).then(|| Self {
            path: path.to_string(),
            users,
            decls,
        })
    }

    /// The locked names whose declaration in `source` (the text after the
    /// round) differs from the one before, or is gone.
    pub fn broken(&self, source: &str, hooks: &RepairHooks) -> Vec<String> {
        let exports = (hooks.guard.exports)(source);
        self.decls
            .iter()
            .filter(|(name, decl)| {
                exports
                    .iter()
                    .find(|export| export.name == **name)
                    .is_none_or(|export| export.decl != **decl)
            })
            .map(|(name, _)| name.clone())
            .collect()
    }

    /// The users of the lock that are not in `skip`.
    pub fn without(&self, skip: &[String]) -> Self {
        Self {
            users: self
                .users
                .iter()
                .filter(|user| !skip.contains(&user.path))
                .cloned()
                .collect(),
            ..self.clone()
        }
    }
}

/// Most characters of one lock block.
const LOCK_CHARS: usize = 3_000;

/// The prompt block of `lock`: the exports that other files use, with their
/// use sites. Empty when it has no users.
pub fn lock_block(lock: &Lock) -> String {
    if lock.users.is_empty() {
        return String::new();
    }
    let mut block = format!(
        "\n\n# Used elsewhere\nOther project files use these exports of `{}`. Keep these names \
         and their types (parameters, return types, props, fields) as they are, so that those \
         files still compile; fix the problems inside this file.\n",
        lock.path
    );
    for (name, decl) in &lock.decls {
        let mut line = format!("- `{name}`: `{decl}`. Used in");
        let mut any = false;
        for user in lock.users.iter().filter(|user| user.names.contains(name)) {
            let sites: Vec<String> = user
                .sites
                .iter()
                .filter(|site| site.name == *name)
                .map(|site| match &site.caller {
                    Some(caller) => {
                        format!("{}:{} (in {caller}) `{}`", user.path, site.line, site.text)
                    }
                    None => format!("{}:{} `{}`", user.path, site.line, site.text),
                })
                .collect();
            if sites.is_empty() {
                let _ = write!(line, " `{}`;", user.path);
            } else {
                let _ = write!(line, " {};", sites.join(", "));
            }
            any = true;
        }
        if !any {
            continue;
        }
        line.pop();
        line.push_str(".\n");
        if block.len() + line.len() > LOCK_CHARS {
            block.push_str("- (more uses not shown)\n");
            break;
        }
        block.push_str(&line);
    }
    block
}

/// One importer's line about a missing export: (importer, specifier, line).
type MissingUse = (String, String, String);

/// What the root-first guard does in one round.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RootPlan {
    /// Files changed with no model call: path to the new text.
    pub fixes: BTreeMap<String, String>,
    /// Problem lines that the fixes remove, for each file.
    pub fixed: BTreeMap<String, Vec<String>>,
    /// Modules to repair, with the problem text they get.
    pub repairs: BTreeMap<String, String>,
    /// Importers' problem lines held for one round.
    pub held: BTreeMap<String, Vec<String>>,
    /// The (module, name) pairs handled, so that each is handled once.
    pub handled: Vec<(String, String)>,
}

/// The root-first guard for the problems `problems` (path to checker
/// text) and the files `files`; `done` holds the (module, name) pairs that
/// an earlier round handled, and `repairable` the files that a repair may
/// change (a fixed output or an internal file is never a module to repair
/// or a file to add `export` to).
pub fn root_first(
    problems: &BTreeMap<String, String>,
    files: &BTreeMap<String, String>,
    hooks: &RepairHooks,
    done: &BTreeSet<(String, String)>,
    repairable: &BTreeSet<String>,
) -> RootPlan {
    let known: BTreeSet<&str> = files.keys().map(String::as_str).collect();
    // (module, name) -> [(importer, specifier, line)].
    let mut groups: BTreeMap<(String, String), Vec<MissingUse>> = BTreeMap::new();
    for (file, text) in problems {
        for line in text.lines() {
            let Some((spec, name)) = (hooks.guard.missing_export)(line) else {
                continue;
            };
            let Some(module) = (hooks.resolve_import)(file, &spec, &known) else {
                continue;
            };
            groups
                .entry((module, name))
                .or_default()
                .push((file.clone(), spec, line.to_string()));
        }
    }
    let mut plan = RootPlan::default();
    let mut texts: BTreeMap<String, String> = BTreeMap::new();
    let text_of = |path: &str, texts: &BTreeMap<String, String>| {
        texts
            .get(path)
            .or_else(|| files.get(path))
            .cloned()
            .unwrap_or_default()
    };
    for ((module, name), importers) in groups {
        let distinct: BTreeSet<&str> = importers.iter().map(|(file, _, _)| file.as_str()).collect();
        if distinct.len() < 2 || done.contains(&(module.clone(), name.clone())) {
            continue;
        }
        plan.handled.push((module.clone(), name.clone()));
        let declarers: Vec<(&String, bool)> = files
            .iter()
            .filter(|(path, _)| !(hooks.is_test)(path) && !distinct.contains(path.as_str()))
            .filter_map(|(path, _)| {
                (hooks.guard.declares)(&text_of(path, &texts), &name)
                    .map(|exported| (path, exported))
            })
            .collect();
        // The module itself, else the one shared module that declares the
        // name (a page's or component's own type of that name may be
        // another type). A declaration that needs `export` must be in a
        // file that may change.
        let owner = declarers
            .iter()
            .find(|(path, _)| **path == module)
            .or_else(|| {
                (declarers.len() == 1 && (hooks.guard.shared_module)(declarers[0].0))
                    .then(|| &declarers[0])
            })
            .filter(|(path, exported)| *exported || repairable.contains(*path));
        match owner {
            Some((owner, exported)) => {
                let owner = (*owner).clone();
                // The module exports the name already (another form of
                // import is wrong): not this guard's case.
                if owner == module && *exported {
                    continue;
                }
                if !exported {
                    let source = text_of(&owner, &texts);
                    match (hooks.guard.add_export)(&source, &name) {
                        Some(fixed) => {
                            texts.insert(owner.clone(), fixed);
                        }
                        None => continue,
                    }
                }
                for (importer, spec, line) in &importers {
                    let moved = owner == module || {
                        let source = text_of(importer, &texts);
                        match (hooks.guard.retarget)(importer, &source, &name, spec, &owner) {
                            Some(fixed) => {
                                texts.insert(importer.clone(), fixed);
                                true
                            }
                            None => false,
                        }
                    };
                    if moved {
                        plan.fixed
                            .entry(importer.clone())
                            .or_default()
                            .push(line.clone());
                    }
                }
            }
            None if repairable.contains(&module) => {
                let names: Vec<&str> = distinct.iter().copied().collect();
                let mut text = format!(
                    "ERROR {module}(1,1): error TS2305: `{name}` is imported from this file by \
                     `{}`, but this file does not export it. Export `{name}` from this file \
                     (declare it if it is missing), with the form and the type that those files \
                     use:\n",
                    names.join("`, `")
                );
                for (_, _, line) in &importers {
                    text.push_str(line);
                    text.push('\n');
                }
                plan.repairs
                    .entry(module.clone())
                    .or_default()
                    .push_str(&text);
                for (importer, _, line) in &importers {
                    plan.held
                        .entry(importer.clone())
                        .or_default()
                        .push(line.clone());
                }
            }
            None => {}
        }
    }
    plan.fixes = texts;
    plan
}

/// `text` without the lines in `drop`.
pub fn without_lines(text: &str, drop: &[String]) -> String {
    let mut out = String::new();
    for line in text.lines() {
        if !drop.iter().any(|dropped| dropped == line) {
            out.push_str(line);
            out.push('\n');
        }
    }
    if out.trim().is_empty() {
        String::new()
    } else {
        out
    }
}

/// True if every line of `problems` that is not blank is a knock-on line.
pub fn only_knock_on(problems: &str, hooks: &RepairHooks) -> bool {
    let mut lines = problems
        .lines()
        .filter(|line| !line.trim().is_empty())
        .peekable();
    lines.peek().is_some() && lines.all(|line| (hooks.guard.knock_on)(line))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profile::NEXTJS_REPAIR;

    fn files(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(path, text)| (path.to_string(), text.to_string()))
            .collect()
    }

    #[test]
    fn users_and_sites_come_from_the_imports() {
        let files = files(&[
            (
                "lib/board.ts",
                "export function moveTask(id: string): void {}\nexport const KEY = 'k';\n",
            ),
            (
                "app/page.tsx",
                "import { moveTask } from '../lib/board';\nexport default function P() {\n  moveTask('a');\n}\n",
            ),
            ("app/other.tsx", "import { x } from '../lib/x';\n"),
        ]);
        let users = text_users("lib/board.ts", &files, &NEXTJS_REPAIR);
        assert_eq!(users.len(), 1);
        assert_eq!(users[0].path, "app/page.tsx");
        assert_eq!(users[0].names, ["moveTask"]);
        assert_eq!(users[0].sites[0].line, 3);
        let lock = Lock::new(
            "lib/board.ts",
            &files["lib/board.ts"],
            users,
            &NEXTJS_REPAIR,
        )
        .unwrap();
        let block = lock_block(&lock);
        assert!(block.contains("`moveTask`: `export function moveTask(id: string): void`"));
        assert!(block.contains("app/page.tsx:3 `moveTask('a');`"));
        assert!(lock
            .broken(&files["lib/board.ts"], &NEXTJS_REPAIR)
            .is_empty());
        let changed = "export function moveTask(id: number): void {}\n";
        assert_eq!(lock.broken(changed, &NEXTJS_REPAIR), ["moveTask"]);
        assert_eq!(
            text_affected(
                "lib/board.ts",
                &["moveTask".to_string()],
                &files,
                &NEXTJS_REPAIR
            ),
            ["app/page.tsx"]
        );
    }

    fn all(files: &BTreeMap<String, String>) -> BTreeSet<String> {
        files.keys().cloned().collect()
    }

    fn missing(file: &str, spec: &str, name: &str) -> String {
        format!(
            "ERROR {file}(1,10): error TS2305: Module '\"{spec}\"' has no exported member \
             '{name}'.\n"
        )
    }

    #[test]
    fn root_first_exports_a_declared_name_and_holds_for_a_missing_one() {
        let files = files(&[
            ("lib/types.ts", "interface AppState {\n  a: number;\n}\n"),
            ("lib/store.ts", "export const x = 1;\n"),
            ("app/a.tsx", "import { AppState } from '../lib/types';\nimport { useStore } from '../lib/store';\n"),
            ("app/b.tsx", "import { AppState } from '../lib/types';\nimport { useStore } from '../lib/store';\n"),
        ]);
        let problems: BTreeMap<String, String> = ["app/a.tsx", "app/b.tsx"]
            .iter()
            .map(|file| {
                (
                    file.to_string(),
                    missing(file, "../lib/types", "AppState")
                        + &missing(file, "../lib/store", "useStore"),
                )
            })
            .collect();
        let plan = root_first(
            &problems,
            &files,
            &NEXTJS_REPAIR,
            &BTreeSet::new(),
            &all(&files),
        );
        assert!(plan.fixes["lib/types.ts"].starts_with("export interface AppState"));
        assert_eq!(plan.fixed["app/a.tsx"].len(), 1);
        assert!(plan.repairs["lib/store.ts"].contains("`useStore` is imported from this file"));
        assert_eq!(plan.held["app/b.tsx"].len(), 1);
        assert_eq!(plan.handled.len(), 2);
        let left = without_lines(&problems["app/a.tsx"], &plan.fixed["app/a.tsx"]);
        assert_eq!(left.lines().count(), 1);
        let done: BTreeSet<(String, String)> = plan.handled.iter().cloned().collect();
        assert_eq!(
            root_first(&problems, &files, &NEXTJS_REPAIR, &done, &all(&files)),
            RootPlan::default()
        );
        // One importer only: no guard.
        let one: BTreeMap<String, String> = problems.into_iter().take(1).collect();
        assert!(
            root_first(&one, &files, &NEXTJS_REPAIR, &BTreeSet::new(), &all(&files))
                .handled
                .is_empty()
        );
    }

    #[test]
    fn root_first_points_the_import_at_the_declaring_file() {
        let files = files(&[
            ("lib/types.ts", "export type Task = { id: string };\n"),
            (
                "lib/storage.ts",
                "export interface AppState {\n  a: number;\n}\n",
            ),
            (
                "app/a.tsx",
                "import { AppState, Task } from '../lib/types';\n",
            ),
            ("app/b.tsx", "import { AppState } from '@/lib/types';\n"),
        ]);
        let problems: BTreeMap<String, String> =
            [("app/a.tsx", "../lib/types"), ("app/b.tsx", "@/lib/types")]
                .iter()
                .map(|(file, spec)| (file.to_string(), missing(file, spec, "AppState")))
                .collect();
        let plan = root_first(
            &problems,
            &files,
            &NEXTJS_REPAIR,
            &BTreeSet::new(),
            &all(&files),
        );
        assert_eq!(
            plan.fixes["app/a.tsx"],
            "import { Task } from '../lib/types';\nimport { AppState } from '../lib/storage';\n"
        );
        assert_eq!(
            plan.fixes["app/b.tsx"],
            "import { AppState } from '@/lib/storage';\n"
        );
        assert!(plan.repairs.is_empty());
        assert!(only_knock_on(
            "ERROR a.tsx(1,1): error TS7006: Parameter 't' implicitly has an 'any' type.\n",
            &NEXTJS_REPAIR
        ));
        assert!(!only_knock_on("", &NEXTJS_REPAIR));
    }

    #[test]
    fn root_first_never_points_imports_at_a_components_own_type() {
        let files = files(&[
            (
                "lib/types.ts",
                "export type Task = { id: string };
",
            ),
            (
                "components/Card.tsx",
                "interface Props {
  id: string;
}
export function Card(p: Props) {}
",
            ),
            (
                "app/a.tsx",
                "import { Props } from '../lib/types';
",
            ),
            (
                "app/b.tsx",
                "import { Props } from '../lib/types';
",
            ),
        ]);
        let problems: BTreeMap<String, String> = ["app/a.tsx", "app/b.tsx"]
            .iter()
            .map(|file| (file.to_string(), missing(file, "../lib/types", "Props")))
            .collect();
        let plan = root_first(
            &problems,
            &files,
            &NEXTJS_REPAIR,
            &BTreeSet::new(),
            &all(&files),
        );
        assert!(plan.fixes.is_empty());
        assert!(plan.repairs["lib/types.ts"].contains("`Props` is imported from this file"));
        assert_eq!(plan.held.len(), 2);
        // A module that no repair may change: the guard leaves it alone,
        // and the importers are repaired as usual.
        let fixed: BTreeSet<String> = ["app/a.tsx", "app/b.tsx", "components/Card.tsx"]
            .iter()
            .map(|path| path.to_string())
            .collect();
        let plan = root_first(&problems, &files, &NEXTJS_REPAIR, &BTreeSet::new(), &fixed);
        assert!(plan.fixes.is_empty() && plan.repairs.is_empty() && plan.held.is_empty());
    }
}
