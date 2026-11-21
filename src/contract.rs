//! The app contract of a plan (`ArchitectOptions::contract`,
//! `PlanOptions::contract`): the facts that several files share, declared in
//! the Types-layer file, and a deterministic check of a manifest against
//! them. The check runs in milliseconds on the merged manifest; the
//! architect sends the problems it finds back to the export fragments that
//! own them once (`architect::split_manifest`).
//!
//! What it checks, each only when the profile names what it needs
//! (`Quality::settings_type`, `state_type`, `component_glob`):
//! - every field of the settings type has a reader: a file that the
//!   settings comment names after `->`, or a file outside the types, the
//!   seed, the storage and the settings page that names the field;
//! - a literal compared to (or given for) a field whose type is a string
//!   union of the Types-layer files is a member of that union (`status !==
//!   'Done'` against `'todo' | 'done'`);
//! - the whole-state type is declared, and each of its collections has an
//!   export that creates a record (`createTask`, `addTask`);
//! - every component is named by some other file (a page that renders it).

use crate::architect::{
    declared_names, identifiers, in_value_context, shape_fields, string_literals, union_members,
    Manifest, ManifestFile,
};
use crate::profile::{glob_match, Layer, Profile, Staging};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// One problem of a manifest against its contract.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContractProblem {
    /// The file whose export fragment can fix it; `None` when no single file
    /// can (the problem is only reported).
    pub path: Option<String>,
    /// What is wrong and what the fix is, as the fragment repair reads it.
    pub text: String,
}

/// One field of the settings type with the meaning and the reader files that
/// the settings comment gives (`field: meaning -> path, path`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SettingField {
    pub name: String,
    /// The meaning stated in the comment; empty when none.
    pub meaning: String,
    /// Planned files that the comment names as readers, in its order.
    pub readers: Vec<String>,
}

/// The Types-layer files of `manifest`.
pub(crate) fn types_files<'a>(manifest: &'a Manifest, staging: &Staging) -> Vec<&'a ManifestFile> {
    manifest
        .files
        .iter()
        .filter(|file| staging.layer_of(&file.path) == Some(Layer::Types))
        .collect()
}

/// Split `text` at the `;` and `,` outside brackets.
fn split_top(text: &str, separators: &[char]) -> Vec<String> {
    let mut parts = Vec::new();
    let (mut depth, mut from) = (0usize, 0usize);
    for (index, ch) in text.char_indices() {
        match ch {
            '{' | '[' | '(' | '<' => depth += 1,
            '}' | ']' | ')' => depth = depth.saturating_sub(1),
            '>' if !text[..index].ends_with('=') => depth = depth.saturating_sub(1),
            _ if depth == 0 && separators.contains(&ch) => {
                parts.push(text[from..index].trim().to_string());
                from = index + ch.len_utf8();
            }
            _ => {}
        }
    }
    parts.push(text[from..].trim().to_string());
    parts.retain(|part| !part.is_empty());
    parts
}

/// The fields of the type that `line` declares, `(name, type)`, from its
/// code body (`export interface T { a: A; b: B }`) or else from a shape that
/// starts its comment (`export interface T  // {a: A, b: B}`), with the rest
/// of the comment after them. `None` when neither gives a shape.
pub(crate) fn declared_fields(line: &str) -> Option<(Vec<(String, String)>, String)> {
    let (code, comment) = match line.split_once("//") {
        Some((code, comment)) => (code, comment.trim()),
        None => (line, ""),
    };
    let (shape, rest) = if let Some(open) = code.find('{') {
        (shape_fields(&code[open..])?, comment.to_string())
    } else if comment.starts_with('{') {
        let fields = shape_fields(comment)?;
        // The rest of the comment after the shape's closing brace.
        let mut depth = 0usize;
        let mut end = comment.len();
        for (index, ch) in comment.char_indices() {
            match ch {
                '{' => depth += 1,
                '}' => {
                    depth = depth.saturating_sub(1);
                    if depth == 0 {
                        end = index + 1;
                        break;
                    }
                }
                _ => {}
            }
        }
        let rest = comment[end..]
            .trim_start_matches([';', ',', ' '])
            .to_string();
        (fields, rest)
    } else {
        return None;
    };
    let fields = split_top(&shape, &[';'])
        .into_iter()
        .filter_map(|field| {
            let (key, value) = field.split_once(':')?;
            let key = key.trim().trim_end_matches('?').to_string();
            Some((key, value.trim().to_string()))
        })
        .collect();
    Some((fields, rest))
}

/// The line of a Types-layer file that declares `name`.
pub(crate) fn declaring_line<'a>(types: &[&'a ManifestFile], name: &str) -> Option<&'a str> {
    types
        .iter()
        .flat_map(|file| file.exports.iter())
        .find(|line| {
            declared_names(line.split("//").next().unwrap_or(line))
                .first()
                .is_some_and(|declared| declared == name)
        })
        .map(String::as_str)
}

/// Whether `word` is an identifier of `text`.
pub(crate) fn names(text: &str, word: &str) -> bool {
    identifiers(text).iter().any(|&(_, found)| found == word)
}

/// Every text of a file that the check reads: purpose, exports and uses.
pub(crate) fn texts(file: &ManifestFile) -> impl Iterator<Item = &str> {
    std::iter::once(file.purpose.as_str())
        .chain(file.exports.iter().map(String::as_str))
        .chain(file.uses.iter().map(String::as_str))
}

/// The fields of the profile's settings type, with the meaning and readers
/// that its comment states (`// field: meaning -> path, path; ...`). Only
/// readers that are planned files count. Empty when the profile names no
/// settings type, or the plan declares none with fields.
pub fn settings_fields(manifest: &Manifest, staging: &Staging) -> Vec<SettingField> {
    let settings = staging.quality.settings_type;
    if settings.is_empty() {
        return Vec::new();
    }
    let types = types_files(manifest, staging);
    let Some((fields, comment)) = declaring_line(&types, settings).and_then(declared_fields) else {
        return Vec::new();
    };
    let planned: Vec<&str> = manifest
        .files
        .iter()
        .map(|file| file.path.as_str())
        .collect();
    let mut stated: HashMap<String, (String, Vec<String>)> = HashMap::new();
    for piece in comment.split(';') {
        let Some((head, readers)) = piece.split_once("->") else {
            continue;
        };
        let (name, meaning) = match head.split_once(':') {
            Some((name, meaning)) => (name.trim(), meaning.trim()),
            // `budgetStartDay 1-28 -> ...`: the first word names the field.
            None => match head.trim().split_once(char::is_whitespace) {
                Some((name, meaning)) => (name, meaning.trim()),
                None => (head.trim(), ""),
            },
        };
        let name = name.trim_matches('`');
        let readers = readers
            .split([',', ' ', '+'])
            .map(|path| path.trim().trim_matches(['`', '.', '\'', '"', '(', ')']))
            .filter(|path| planned.contains(path))
            .map(str::to_string)
            .collect();
        stated.insert(name.to_string(), (meaning.to_string(), readers));
    }
    fields
        .into_iter()
        .map(|(name, _)| {
            let (meaning, readers) = stated.remove(&name).unwrap_or_default();
            SettingField {
                name,
                meaning,
                readers,
            }
        })
        .collect()
}

/// Files that only store or edit the settings: a settings reader must be
/// another file. Matched on whole words of the path (its directories and
/// the camel-case words of its stem), so `app/settings/page.tsx`,
/// `components/SettingsForm.tsx`, `lib/storage.ts` and `lib/useAppState.ts`
/// count, but `app/statement/page.tsx`, `components/RestoreButton.tsx` and
/// `components/EmptyState.tsx` do not.
fn stores_settings(path: &str) -> bool {
    const WORDS: &[&str] = &["settings", "storage", "store", "types", "seed"];
    let mut segments: Vec<&str> = path.split('/').collect();
    let file = segments.pop().unwrap_or(path);
    if segments
        .iter()
        .any(|segment| WORDS.contains(&segment.to_lowercase().as_str()))
    {
        return true;
    }
    let mut words: Vec<String> = Vec::new();
    let mut word = String::new();
    for ch in stem(file).chars() {
        if ch.is_ascii_uppercase() && !word.is_empty() {
            words.push(std::mem::take(&mut word));
        }
        if ch.is_ascii_alphanumeric() {
            word.push(ch.to_ascii_lowercase());
        } else if !word.is_empty() {
            words.push(std::mem::take(&mut word));
        }
    }
    if !word.is_empty() {
        words.push(word);
    }
    // `state` only as the app state itself (`state.ts`, `AppState`,
    // `useAppState`), not as a widget state (`EmptyState`).
    let app_state = matches!(
        words
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>()
            .as_slice(),
        ["state"] | ["app", "state"] | ["use", "app", "state"] | ["use", "state"]
    );
    app_state || words.iter().any(|word| WORDS.contains(&word.as_str()))
}

/// The string unions of the Types-layer files: the members of each named
/// union, and for each field of a declared type whose type is a union (named
/// or inline), its members and the union's name. A field name that two types
/// give different unions is left out.
fn union_fields(types: &[&ManifestFile]) -> HashMap<String, (String, Vec<String>)> {
    let mut unions: HashMap<String, Vec<String>> = HashMap::new();
    for line in types.iter().flat_map(|file| file.exports.iter()) {
        let code = line.split("//").next().unwrap_or(line);
        let members = union_members(code);
        if let (Some(name), false) = (declared_names(code).first(), members.is_empty()) {
            if code.contains('=') && !code.contains('{') {
                unions.insert(name.clone(), members);
            }
        }
    }
    let mut fields: HashMap<String, Option<(String, Vec<String>)>> = HashMap::new();
    for line in types.iter().flat_map(|file| file.exports.iter()) {
        let Some((declared, _)) = declared_fields(line) else {
            continue;
        };
        for (field, ty) in declared {
            let inline = union_members(&ty);
            let found = if !inline.is_empty() {
                Some((ty.clone(), inline))
            } else {
                let name = ty.trim();
                unions
                    .get(name)
                    .map(|members| (name.to_string(), members.clone()))
            };
            let Some(found) = found else {
                continue;
            };
            match fields.get(&field) {
                Some(Some(known)) if known.1 != found.1 => {
                    fields.insert(field, None);
                }
                Some(_) => {}
                None => {
                    fields.insert(field, Some(found));
                }
            }
        }
    }
    fields
        .into_iter()
        .filter_map(|(field, found)| found.map(|found| (field, found)))
        .collect()
}

/// The identifier that a literal at `start..end` of `text` is compared to
/// or given for: `x.status !== 'Done'`, `'Done' === status`, `status:
/// 'Done'`.
fn compared_field(text: &str, start: usize, end: usize) -> Option<String> {
    let before = text[..start - 1].trim_end();
    let after = text.get(end + 1..).unwrap_or("").trim_start();
    // A member of a union that the line declares itself (`kind: 'expense' |
    // 'income'`) is not compared to a field of the shared types.
    let union = |side: &str, at_end: bool| {
        if at_end {
            side.ends_with('|') && !side.ends_with("||")
        } else {
            side.starts_with('|') && !side.starts_with("||")
        }
    };
    if union(before, true) || union(after, false) {
        return None;
    }
    let trailing = |side: &str| -> Option<String> {
        let word: String = side
            .chars()
            .rev()
            .take_while(|ch| ch.is_ascii_alphanumeric() || *ch == '_')
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        (!word.is_empty()).then_some(word)
    };
    for op in ["===", "!==", "==", "!="] {
        if let Some(side) = before.strip_suffix(op) {
            return trailing(side.trim_end());
        }
        if let Some(side) = after.strip_prefix(op) {
            let word: String = side
                .trim_start()
                .chars()
                .take_while(|ch| ch.is_ascii_alphanumeric() || *ch == '_' || *ch == '.')
                .collect();
            return word.rsplit('.').next().map(str::to_string);
        }
    }
    let side = before.strip_suffix(':')?;
    // `status: 'Done'` names a field; a quoted key (`'done': 'Done'`) is a
    // labels map entry.
    if side.ends_with(['\'', '"']) {
        return None;
    }
    trailing(side.trim_end())
}

/// Words of component names that say what kind of widget it is, not what it
/// shows: they never pick the page that renders it.
const GENERIC_WORDS: &[&str] = &[
    "list",
    "form",
    "card",
    "view",
    "page",
    "panel",
    "table",
    "chart",
    "item",
    "row",
    "grid",
    "bar",
    "app",
    "and",
    "the",
    "with",
    "component",
    "tsx",
];

/// Lower-case words of a camel-case or path name, singular (a trailing `s`
/// dropped), of three letters or more, without [`GENERIC_WORDS`].
fn name_words(name: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut word = String::new();
    for ch in name.chars() {
        if ch.is_ascii_uppercase() && !word.is_empty() {
            words.push(std::mem::take(&mut word));
        }
        if ch.is_ascii_alphanumeric() {
            word.push(ch.to_ascii_lowercase());
        } else if !word.is_empty() {
            words.push(std::mem::take(&mut word));
        }
    }
    if !word.is_empty() {
        words.push(word);
    }
    words
        .into_iter()
        .map(|word| word.strip_suffix('s').map(str::to_string).unwrap_or(word))
        .filter(|word| word.len() >= 3 && !GENERIC_WORDS.contains(&word.as_str()))
        .collect()
}

/// The file stem of `path` (`components/TaskForm.tsx` -> `TaskForm`).
pub(crate) fn stem(path: &str) -> &str {
    let name = path.rsplit('/').next().unwrap_or(path);
    name.split('.').next().unwrap_or(name)
}

/// Give each setting that no planned file reads a reader without a model
/// call: the Core or Consumer file (not types, data, storage, settings or a
/// test) whose path and purpose share the most words with the setting's name
/// and meaning gets a note in its purpose that it reads the setting. The
/// contract check then counts it as read. A setting no file matches keeps
/// its problem. Returns the settings given a reader. On Muse the planner
/// call that this replaces cost 12 s of a 38 s plan (20261116-133941:
/// `Settings.monthStartDay`).
pub fn assign_setting_readers(manifest: &mut Manifest, profile: &Profile) -> usize {
    let staging = &profile.staging;
    let settings = staging.quality.settings_type;
    if settings.is_empty() || staging.quality.contract_rule.is_empty() {
        return 0;
    }
    let candidate = |file: &ManifestFile| {
        matches!(
            staging.layer_of(&file.path),
            Some(Layer::Core) | Some(Layer::Consumer)
        ) && !stores_settings(&file.path)
            && !staging.is_test(&file.path)
            && !glob_match(staging.quality.component_glob, &file.path)
    };
    let mut given = 0;
    for field in settings_fields(manifest, staging) {
        if !field.readers.is_empty() {
            continue;
        }
        let read = manifest.files.iter().any(|file| {
            staging.layer_of(&file.path) != Some(Layer::Types)
                && staging.layer_of(&file.path) != Some(Layer::Data)
                && !stores_settings(&file.path)
                && texts(file).any(|text| names(text, &field.name))
        });
        if read {
            continue;
        }
        let mut words = name_words(&field.name);
        words.extend(name_words(&field.meaning));
        if words.is_empty() {
            continue;
        }
        let best = manifest
            .files
            .iter()
            .enumerate()
            .filter(|(_, file)| candidate(file))
            .map(|(index, file)| {
                let mut file_words = name_words(&file.path);
                file_words.extend(name_words(&file.purpose));
                let score = words
                    .iter()
                    .filter(|word| file_words.contains(word))
                    .count();
                // A lib module first on a tie: the behaviour lives there.
                let core = staging.layer_of(&file.path) == Some(Layer::Core);
                (score, core, std::cmp::Reverse(index))
            })
            .filter(|(score, _, _)| *score > 0)
            .max();
        let Some((_, _, std::cmp::Reverse(index))) = best else {
            continue;
        };
        let meaning = if field.meaning.is_empty() {
            String::new()
        } else {
            format!(" ({})", field.meaning.trim())
        };
        manifest.files[index].purpose.push_str(&format!(
            " Reads `{settings}.{}`{meaning} and changes its behaviour by it.",
            field.name
        ));
        given += 1;
    }
    given
}

/// The verbs of an export that creates a record: `{verb}{Entity}`, or the
/// bare verb in the file that owns the collection. An upsert (`saveGoal`,
/// `setBudget`) and an append (`logActivity`) create too.
const CREATE_VERBS: &[&str] = &[
    "create", "add", "new", "insert", "upsert", "save", "put", "set", "log", "append",
];

/// Collection names of a log: appended to, never edited or removed.
const LOG_NAMES: &[&str] = &["activity", "log", "history", "audit"];

/// The singular of a lower-case collection name: `services` -> `service`,
/// `classes` -> `class`, `replies` -> `reply`; a name with no plural ending
/// (`fuel`, `attendance`) stays.
fn singular(key: &str) -> String {
    if let Some(stem) = key.strip_suffix("ies") {
        return format!("{stem}y");
    }
    for ending in ["sses", "xes", "ches", "shes"] {
        if key.ends_with(ending) {
            return key[..key.len() - 2].to_string();
        }
    }
    match key.strip_suffix('s') {
        Some(stem) if !stem.ends_with('s') && !stem.is_empty() => stem.to_string(),
        _ => key.to_string(),
    }
}

/// The problems of `manifest` against the app contract of `profile` (see
/// the module docs). Deterministic and idempotent; empty for a profile with
/// no `contract_rule`.
pub fn contract_problems(manifest: &Manifest, profile: &Profile) -> Vec<ContractProblem> {
    let staging = &profile.staging;
    let quality = &staging.quality;
    if quality.contract_rule.is_empty() {
        return Vec::new();
    }
    let types = types_files(manifest, staging);
    let types_path = types.first().map(|file| file.path.clone());
    let is_data = |path: &str| staging.layer_of(path) == Some(Layer::Data);
    let is_types = |path: &str| staging.layer_of(path) == Some(Layer::Types);
    let mut problems = Vec::new();

    // Settings: declared with fields, and every field read.
    let settings = quality.settings_type;
    if !settings.is_empty() {
        let named = manifest
            .files
            .iter()
            .any(|file| texts(file).any(|text| names(text, settings)));
        let fields = settings_fields(manifest, staging);
        if named && fields.is_empty() {
            problems.push(ContractProblem {
                path: types_path.clone(),
                text: format!(
                    "`{settings}` has no fields in the plan: declare `export interface {settings} {{ ... }}` \
                     with every setting, `DEFAULT_SETTINGS`, and in its comment each field's meaning and \
                     readers (`field: meaning -> path`)."
                ),
            });
        }
        for field in &fields {
            if !field.readers.is_empty() {
                continue;
            }
            let read = manifest.files.iter().any(|file| {
                !is_types(&file.path)
                    && !is_data(&file.path)
                    && !stores_settings(&file.path)
                    && texts(file).any(|text| names(text, &field.name))
            });
            if !read {
                problems.push(ContractProblem {
                    path: types_path.clone(),
                    text: format!(
                        "`{settings}.{}` has no reader: end its entry in the {settings} comment with \
                         `-> <the files whose behaviour it changes>`.",
                        field.name
                    ),
                });
            }
        }
    }

    // Literals compared to a union field are members of the union.
    let unions = union_fields(&types);
    if !unions.is_empty() {
        let mut seen = std::collections::HashSet::new();
        for file in manifest.files.iter().filter(|file| !is_types(&file.path)) {
            for text in texts(file) {
                for (start, end) in string_literals(text) {
                    if !in_value_context(text, start, end)
                        && !text[..start - 1].trim_end().ends_with(':')
                    {
                        continue;
                    }
                    let Some(field) = compared_field(text, start, end) else {
                        continue;
                    };
                    let Some((union, members)) = unions.get(&field) else {
                        continue;
                    };
                    let literal = &text[start..end];
                    if members.iter().any(|member| member == literal)
                        || !seen.insert((file.path.clone(), field.clone(), literal.to_string()))
                    {
                        continue;
                    }
                    let values = members
                        .iter()
                        .map(|member| format!("'{member}'"))
                        .collect::<Vec<_>>()
                        .join(" | ");
                    problems.push(ContractProblem {
                        path: Some(file.path.clone()),
                        text: format!(
                            "`'{literal}'` is not a value of `{field}` ({union}: {values}): use one of \
                             those values."
                        ),
                    });
                }
            }
        }
    }

    // The whole app state is declared, and each collection can be created.
    let state = quality.state_type;
    if !state.is_empty() && !types.is_empty() {
        match declaring_line(&types, state).and_then(declared_fields) {
            None => problems.push(ContractProblem {
                path: types_path.clone(),
                text: format!(
                    "the plan declares no `{state}` with fields: declare `export interface {state} {{ ... }}` \
                     with every collection of the app and `settings`."
                ),
            }),
            Some((fields, _)) => {
                for (key, ty) in fields {
                    let Some(entity) = ty.trim().strip_suffix("[]") else {
                        continue;
                    };
                    if !entity.starts_with(|c: char| c.is_ascii_uppercase())
                        || !entity.chars().all(|c| c.is_ascii_alphanumeric())
                    {
                        continue;
                    }
                    // A fixed catalog (`export const MEMBERS: Member[]` in
                    // the types) is not created at run time: asking for its
                    // add/update/remove would add features nobody asked for.
                    let catalog = types.iter().flat_map(|file| file.exports.iter()).any(|line| {
                        let code = line.split("//").next().unwrap_or(line);
                        code.contains("const ")
                            && (code.contains(&format!(": {entity}[]"))
                                || code.contains(&format!(": readonly {entity}[]"))
                                || code.contains(&format!("Array<{entity}>")))
                    });
                    if catalog {
                        continue;
                    }
                    let lower = entity.to_lowercase();
                    // The collection's own word also names its entity:
                    // `addService` creates the `ServiceRecord` of `services`
                    // (the check asked for `addServiceRecord`, and the
                    // repair renamed exports that the other contracts call).
                    let single = singular(&key.to_lowercase());
                    let creates = manifest
                        .files
                        .iter()
                        .filter(|file| !is_types(&file.path) && !is_data(&file.path))
                        .any(|file| {
                            let own = stem(&file.path).to_lowercase();
                            let owns = own == lower || own == format!("{lower}s") || own == key;
                            // The code of the line only: a comment word
                            // (`new expense record` of `payOccurrence`)
                            // creates nothing (audit 9: recurring items had
                            // no create, edit or delete). An upsert or an
                            // append in the code (`saveGoal`, `setBudget`,
                            // `logActivity`) creates too.
                            file.exports.iter().any(|line| {
                                let code = line.split("//").next().unwrap_or(line);
                                identifiers(code).iter().any(|&(_, word)| {
                                    let word = word.to_lowercase();
                                    CREATE_VERBS.iter().any(|verb| {
                                        word == format!("{verb}{lower}")
                                            || word == format!("{verb}{single}")
                                            // Bare `set` is also the `Set` type.
                                            || (owns && word == *verb && *verb != "set")
                                    })
                                })
                            })
                        });
                    if creates {
                        continue;
                    }
                    let owner = manifest
                        .files
                        .iter()
                        .find(|file| {
                            let own = stem(&file.path).to_lowercase();
                            !is_types(&file.path)
                                && !is_data(&file.path)
                                && (own == lower || own == format!("{lower}s") || own == key)
                        })
                        .or_else(|| {
                            manifest.files.iter().find(|file| {
                                let own = stem(&file.path).to_lowercase();
                                file.path.ends_with(".ts")
                                    && (own.contains("store") || own == "state")
                            })
                        });
                    // A log (an activity feed) is appended to, never edited.
                    let log = LOG_NAMES
                        .iter()
                        .any(|word| lower.contains(word) || key.to_lowercase().contains(word));
                    let text = if log {
                        format!(
                            "no export creates a `{entity}` (`{state}.{key}`): add one export \
                             `log{entity}` that appends an entry to the file that owns `{key}`."
                        )
                    } else {
                        format!(
                            "no export creates a `{entity}` (`{state}.{key}`): add `add{entity}` or \
                             `create{entity}` with its update and remove functions to the file that \
                             owns `{key}`."
                        )
                    };
                    problems.push(ContractProblem {
                        path: owner.map(|file| file.path.clone()),
                        text,
                    });
                }
            }
        }
    }

    // Every component is rendered by some other file.
    if !quality.component_glob.is_empty() {
        for file in manifest
            .files
            .iter()
            .filter(|file| glob_match(quality.component_glob, &file.path))
        {
            let mut own: Vec<String> = file
                .exports
                .iter()
                .flat_map(|line| declared_names(line.split("//").next().unwrap_or(line)))
                .collect();
            own.push(stem(&file.path).to_string());
            let used = manifest.files.iter().any(|other| {
                other.path != file.path
                    && (other.uses.iter().any(|entry| entry.starts_with(&file.path))
                        || texts(other).any(|text| own.iter().any(|name| names(text, name))))
            });
            if used {
                continue;
            }
            let words = name_words(stem(&file.path));
            // The page that most likely shows it, by shared words (the
            // first of the best, in manifest order); else the layout or
            // another consumer outside the components.
            let best = |candidate: &dyn Fn(&ManifestFile) -> bool| {
                manifest
                    .files
                    .iter()
                    .filter(|other| other.path != file.path && candidate(other))
                    .map(|other| {
                        let mut page_words = name_words(&other.path);
                        page_words.extend(texts(other).flat_map(name_words));
                        let score = words
                            .iter()
                            .filter(|word| page_words.contains(word))
                            .count();
                        (score, other)
                    })
                    .filter(|(score, _)| *score > 0)
                    .fold(
                        None,
                        |best: Option<(usize, &ManifestFile)>, (score, other)| match best {
                            Some((top, _)) if top >= score => best,
                            _ => Some((score, other)),
                        },
                    )
                    .map(|(_, other)| other.path.clone())
            };
            let page = best(&|other| glob_match(quality.page_glob, &other.path)).or_else(|| {
                best(&|other| {
                    (profile.is_consumer)(&other.path)
                        && staging.layer_of(&other.path) == Some(Layer::Consumer)
                        && !glob_match(quality.component_glob, &other.path)
                        && !glob_match(quality.page_glob, &other.path)
                        && !staging.is_test(&other.path)
                })
            });
            let name = stem(&file.path);
            problems.push(ContractProblem {
                path: page,
                text: format!(
                    "no page renders `{name}` (`{}`): the page that shows it names it in its export \
                     comment (`renders <{name}/>`).",
                    file.path
                ),
            });
        }
    }
    problems
}

/// The tail line for `path` with `PlanOptions::contract`: the settings
/// fields that the contract names this file as a reader of. Empty when none.
/// With `parameter` (a lib module: the caller passes the settings, since
/// only the storage module touches storage) the line asks for the value as
/// a parameter instead of reading it.
pub fn reader_line(path: &str, fields: &[SettingField], settings: &str, parameter: bool) -> String {
    let read: Vec<String> = fields
        .iter()
        .filter(|field| field.readers.iter().any(|reader| reader == path))
        .map(|field| {
            if field.meaning.is_empty() {
                format!("`{}`", field.name)
            } else {
                format!("`{}` ({})", field.name, field.meaning)
            }
        })
        .collect();
    if read.is_empty() {
        return String::new();
    }
    if parameter {
        return format!(
            "\n- Settings this file applies (fields of the app's `{settings}`): {}. Take each \
             one as a parameter from the caller (never read storage here) and apply it.",
            read.join(", ")
        );
    }
    format!(
        "\n- Settings this file reads (from the app's `{settings}`; apply each one, do not only \
         save it): {}",
        read.join(", ")
    )
}

/// The lines that `PlanOptions::wiring_rules` adds to the wiring prompt:
/// the profile's `wiring_rule` and, when the plan names settings readers,
/// each settings field with its readers.
pub fn wiring_lines(fields: &[SettingField], rule: &str) -> String {
    let mut text = rule.to_string();
    let listed: Vec<String> = fields
        .iter()
        .filter(|field| !field.readers.is_empty())
        .map(|field| format!("- `{}` -> {}", field.name, field.readers.join(", ")))
        .collect();
    if !listed.is_empty() {
        text.push_str(
            " Each settings field is read by the files listed for it below: list the export \
             that gives each of them the settings.",
        );
        text.push_str("\nSettings fields and the files that read them:\n");
        text.push_str(&listed.join("\n"));
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profile::NEXTJS;

    fn file(path: &str, purpose: &str, exports: &[&str]) -> ManifestFile {
        ManifestFile {
            path: path.to_string(),
            purpose: purpose.to_string(),
            exports: exports.iter().map(|line| line.to_string()).collect(),
            uses: Vec::new(),
        }
    }

    fn manifest(files: Vec<ManifestFile>) -> Manifest {
        Manifest {
            files,
            test_files: Vec::new(),
            readme: false,
        }
    }

    /// The audit's cases: 'Done' against a lower-case union, a setting with
    /// no reader, no AppState, a component no page renders.
    #[test]
    fn the_check_finds_the_audit_cases() {
        let plan = manifest(vec![
            file(
                "lib/types.ts",
                "Shared types.",
                &[
                    "export type Status = 'backlog' | 'done'  // exactly these",
                    "export interface Task { id: string; status: Status }",
                    "export interface Settings { theme: 'light' | 'dark'; defaultProjectId: string | null }  // theme: colour scheme -> app/layout.tsx",
                ],
            ),
            file(
                "lib/team.ts",
                "Team workload.",
                &["export function openCount(tasks: {status: string}[]): number  // counts status!=='Done'"],
            ),
            file("app/layout.tsx", "Root layout.", &["export default function RootLayout()"]),
            file("app/board/page.tsx", "Kanban board.", &["export default function BoardPage()  // renders <Column/>"]),
            file("app/settings/page.tsx", "Edits Settings.", &["export default function SettingsPage()  // defaultProjectId select"]),
            file("components/Column.tsx", "One column.", &["export default function Column()"]),
            file("components/BoardChart.tsx", "Board chart.", &["export default function BoardChart()"]),
        ]);
        let problems = contract_problems(&plan, &NEXTJS);
        let texts: Vec<&str> = problems
            .iter()
            .map(|problem| problem.text.as_str())
            .collect();
        assert!(
            problems
                .iter()
                .any(|problem| problem.path.as_deref() == Some("lib/team.ts")
                    && problem.text.contains("`'Done'` is not a value of `status`")),
            "{texts:?}"
        );
        assert!(
            problems
                .iter()
                .any(|problem| problem.path.as_deref() == Some("lib/types.ts")
                    && problem
                        .text
                        .contains("`Settings.defaultProjectId` has no reader")),
            "{texts:?}"
        );
        assert!(
            !texts.iter().any(|text| text.contains("Settings.theme")),
            "{texts:?}"
        );
        assert!(
            texts
                .iter()
                .any(|text| text.contains("declares no `AppState`")),
            "{texts:?}"
        );
        assert!(
            problems.iter().any(
                |problem| problem.path.as_deref() == Some("app/board/page.tsx")
                    && problem.text.contains("no page renders `BoardChart`")
            ),
            "{texts:?}"
        );
        assert!(
            !texts.iter().any(|text| text.contains("`Column`")),
            "{texts:?}"
        );
        // Deterministic.
        assert_eq!(problems, contract_problems(&plan, &NEXTJS));
    }

    #[test]
    fn settings_readers_parse_without_a_colon_and_joined_by_plus() {
        let plan = manifest(vec![
            file(
                "lib/types.ts",
                "Shared types.",
                &["export interface Settings { budgetStartDay: number; theme: string }  // budgetStartDay 1-28 -> lib/budgets.ts app/budgets/page.tsx; theme->app/layout.tsx+components/AppShell.tsx"],
            ),
            file("lib/budgets.ts", "Budgets.", &["export function progress(): number"]),
            file("app/budgets/page.tsx", "Budgets page.", &["export default function BudgetsPage()"]),
            file("app/layout.tsx", "Layout.", &["export default function RootLayout()"]),
            file("components/AppShell.tsx", "Shell.", &["export default function AppShell()"]),
        ]);
        let fields = settings_fields(&plan, &NEXTJS.staging);
        assert_eq!(fields[0].name, "budgetStartDay");
        assert_eq!(fields[0].meaning, "1-28");
        assert_eq!(
            fields[0].readers,
            ["lib/budgets.ts", "app/budgets/page.tsx"]
        );
        assert_eq!(
            fields[1].readers,
            ["app/layout.tsx", "components/AppShell.tsx"]
        );
    }

    #[test]
    fn an_unread_setting_gets_its_likeliest_reader_without_a_model_call() {
        let mut plan = manifest(vec![
            file(
                "lib/types.ts",
                "Shared types.",
                &[
                    "export interface Budget { id: string; limitCents: number }",
                    "export interface Settings { monthStartDay: number; zzzQuux: boolean }  // monthStartDay: first day of the budget month; zzzQuux: nothing",
                    "export interface AppState { budgets: Budget[]; settings: Settings }",
                ],
            ),
            file("lib/storage.ts", "Load and save the budget state.", &["export function load(): AppState"]),
            file("lib/budgets.ts", "Budget progress per month.", &["export function progress(state: AppState): number"]),
            file("app/budgets/page.tsx", "Budgets page.", &["export default function BudgetsPage()"]),
            file("app/settings/page.tsx", "Settings form.", &["export default function SettingsPage()"]),
        ]);
        let before = contract_problems(&plan, &NEXTJS);
        assert!(
            before
                .iter()
                .any(|p| p.text.contains("`Settings.monthStartDay` has no reader")),
            "{before:?}"
        );
        assert_eq!(assign_setting_readers(&mut plan, &NEXTJS), 1);
        // The lib module wins the tie with its page; storage never reads.
        assert!(
            plan.files[2]
                .purpose
                .contains("Reads `Settings.monthStartDay`"),
            "{:?}",
            plan.files[2]
        );
        assert!(!plan.files[1].purpose.contains("Reads"));
        let after = contract_problems(&plan, &NEXTJS);
        assert!(
            !after.iter().any(|p| p.text.contains("monthStartDay")),
            "{after:?}"
        );
        // A setting no file matches keeps its problem.
        assert!(
            after
                .iter()
                .any(|p| p.text.contains("`Settings.zzzQuux` has no reader")),
            "{after:?}"
        );
        // Idempotent.
        assert_eq!(assign_setting_readers(&mut plan, &NEXTJS), 0);
    }

    #[test]
    fn a_full_contract_has_no_problems_and_names_the_readers() {
        let plan = manifest(vec![
            file(
                "lib/types.ts",
                "Shared types.",
                &[
                    "export type Status = 'todo' | 'done'",
                    "export interface Task { id: string; status: Status }",
                    "export interface Settings { defaultProjectId: string | null; currency: string }  // defaultProjectId: project the board opens -> app/board/page.tsx; currency: money format -> lib/money.ts, components/Nope.tsx",
                    "export interface AppState { tasks: Task[]; settings: Settings }",
                ],
            ),
            file("lib/tasks.ts", "Tasks.", &["export function addTask(state: AppState, task: Task): AppState  // status==='todo' first"]),
            file("lib/money.ts", "Money.", &["export function formatCents(cents: number, currency: string): string"]),
            file("app/board/page.tsx", "Board.", &["export default function BoardPage()"]),
        ]);
        assert_eq!(contract_problems(&plan, &NEXTJS), Vec::new());
        let fields = settings_fields(&plan, &NEXTJS.staging);
        assert_eq!(fields.len(), 2);
        assert_eq!(fields[0].readers, ["app/board/page.tsx"]);
        assert_eq!(fields[0].meaning, "project the board opens");
        // An unplanned reader is dropped.
        assert_eq!(fields[1].readers, ["lib/money.ts"]);
        let line = reader_line("app/board/page.tsx", &fields, "Settings", false);
        assert!(
            line.contains("`defaultProjectId` (project the board opens)"),
            "{line}"
        );
        assert_eq!(reader_line("lib/tasks.ts", &fields, "Settings", false), "");
        // A lib reader takes the value as a parameter.
        let lib = reader_line("lib/money.ts", &fields, "Settings", true);
        assert!(lib.contains("as a parameter from the caller"), "{lib}");
        assert!(!lib.contains("do not only save"), "{lib}");
        let wiring = wiring_lines(&fields, "RULE");
        assert!(
            wiring.starts_with("RULE Each settings field is read by the files listed"),
            "{wiring}"
        );
        assert!(
            wiring.contains("\nSettings fields and the files"),
            "{wiring}"
        );
        assert!(wiring.contains("- `currency` -> lib/money.ts"), "{wiring}");
        // No readers stated: no pointer to a list that is not there.
        assert_eq!(wiring_lines(&[], "RULE"), "RULE");
    }

    /// A fixed catalog in the state needs no create export; settings
    /// readers match whole path words.
    #[test]
    fn catalogs_need_no_create_and_reader_paths_match_whole_words() {
        let plan = manifest(vec![
            file(
                "lib/types.ts",
                "Shared types.",
                &[
                    "export interface Member { id: string; name: string }",
                    "export interface Task { id: string }",
                    "export const MEMBERS: Member[]  // [{id:'ana', name:'Ana'}]",
                    "export interface Settings { pageSize: number }  // pageSize: rows per page",
                    "export interface AppState { tasks: Task[]; members: Member[]; settings: Settings }",
                ],
            ),
            file("lib/tasks.ts", "Tasks.", &["export function addTask(state: AppState, task: Task): AppState"]),
            file("app/statement/page.tsx", "Statement.", &["export default function StatementPage()  // pageSize rows"]),
        ]);
        let problems = contract_problems(&plan, &NEXTJS);
        let texts: Vec<&str> = problems
            .iter()
            .map(|problem| problem.text.as_str())
            .collect();
        assert!(
            !texts.iter().any(|text| text.contains("`Member`")),
            "{texts:?}"
        );
        assert!(
            !texts.iter().any(|text| text.contains("no reader")),
            "{texts:?}"
        );
        for path in [
            "app/settings/page.tsx",
            "components/SettingsForm.tsx",
            "lib/storage.ts",
            "lib/store.ts",
            "lib/useAppState.ts",
            "lib/state.ts",
            "lib/seed.ts",
        ] {
            assert!(stores_settings(path), "{path}");
        }
        for path in [
            "app/statement/page.tsx",
            "components/RestoreButton.tsx",
            "components/EmptyState.tsx",
        ] {
            assert!(!stores_settings(path), "{path}");
        }
    }

    /// Audit 9 (Ledgerly h2): `payOccurrence`'s comment said "new expense
    /// record", which counted as a create of `Recurring`; the recurring page
    /// got no add, edit or delete.
    #[test]
    fn a_comment_word_creates_no_record() {
        let types = file(
            "lib/types.ts",
            "Shared types.",
            &[
                "export interface Recurring { id: string; name: string }",
                "export interface AppState { recurring: Recurring[] }",
            ],
        );
        let pay = "export function payOccurrence(items: Recurring[], id: string): Recurring[]  // transaction: new expense record";
        let plan = manifest(vec![
            types.clone(),
            file("lib/recurring.ts", "Recurring.", &[pay]),
        ]);
        let problems = contract_problems(&plan, &NEXTJS);
        let missing = problems
            .iter()
            .find(|problem| problem.text.starts_with("no export creates a `Recurring`"))
            .expect("asks for a create");
        assert_eq!(missing.path.as_deref(), Some("lib/recurring.ts"));
        // A create in the code of a line still counts.
        let plan = manifest(vec![
            types,
            file(
                "lib/recurring.ts",
                "Recurring.",
                &[pay, "export function addRecurring(items: Recurring[], input: Recurring): Recurring[]"],
            ),
        ]);
        assert!(!contract_problems(&plan, &NEXTJS)
            .iter()
            .any(|problem| problem.text.contains("no export creates")));
    }

    /// `addService` creates the `ServiceRecord` of `AppState.services`: the
    /// collection's singular names the entity too. A file with only reads
    /// of it is still asked for a create.
    #[test]
    fn a_create_named_after_the_collection_counts() {
        let types = file(
            "lib/types.ts",
            "Shared types.",
            &[
                "export interface ServiceRecord { id: string }",
                "export interface CannedReply { id: string }",
                "export interface AppState { services: ServiceRecord[]; replies: CannedReply[] }",
            ],
        );
        let plan = manifest(vec![
            types.clone(),
            file(
                "lib/services.ts",
                "Services.",
                &["export function addService(state: AppState, input: { date: string }): AppState"],
            ),
            file(
                "lib/canned.ts",
                "Replies.",
                &["export function createReply(state: AppState, text: string): AppState"],
            ),
        ]);
        assert!(!contract_problems(&plan, &NEXTJS)
            .iter()
            .any(|problem| problem.text.contains("no export creates")));
        let plan = manifest(vec![
            types,
            file(
                "lib/services.ts",
                "Services.",
                &["export function servicesForVehicle(state: AppState, id: string): ServiceRecord[]"],
            ),
        ]);
        assert!(contract_problems(&plan, &NEXTJS)
            .iter()
            .any(|problem| problem
                .text
                .starts_with("no export creates a `ServiceRecord`")));
        assert_eq!(singular("classes"), "class");
        assert_eq!(singular("fuel"), "fuel");
        assert_eq!(singular("class"), "class");
    }

    /// An upsert or an append in the code creates (`saveGoal` "adds or
    /// updates", `setBudget`, `logActivity`); a log with no create is asked
    /// for an append only, never for update and remove.
    #[test]
    fn upserts_and_appends_create_and_a_log_needs_no_edit() {
        let types = file(
            "lib/types.ts",
            "Shared types.",
            &[
                "export interface Goal { id: string }",
                "export interface Budget { id: string }",
                "export interface Activity { id: string }",
                "export interface AppState { goals: Goal[]; budgets: Budget[]; activity: Activity[] }",
            ],
        );
        let creates = manifest(vec![
            types.clone(),
            file(
                "lib/goals.ts",
                "Goals.",
                &["export function saveGoal(goals: Goal[], goal: Goal): Goal[] // adds or updates"],
            ),
            file(
                "lib/budgets.ts",
                "Budgets.",
                &["export function setBudget(budgets: Budget[], id: string, amount: number): Budget[]"],
            ),
            file(
                "lib/activity.ts",
                "Activity.",
                &["export function logActivity(items: Activity[], text: string): Activity[] // appends a new entry"],
            ),
        ]);
        assert!(!contract_problems(&creates, &NEXTJS)
            .iter()
            .any(|problem| problem.text.contains("no export creates")));
        let no_log = manifest(vec![
            types,
            file("lib/goals.ts", "Goals.", &["export function saveGoal(goals: Goal[], goal: Goal): Goal[]"]),
            file("lib/budgets.ts", "Budgets.", &["export function setBudget(budgets: Budget[]): Budget[]"]),
            file("lib/activity.ts", "Activity.", &["export function recentActivity(items: Activity[]): Activity[] // new entries first"]),
        ]);
        let problems = contract_problems(&no_log, &NEXTJS);
        let log = problems
            .iter()
            .find(|problem| problem.text.starts_with("no export creates a `Activity`"))
            .expect("asks for an append");
        assert!(
            log.text.contains("`logActivity` that appends"),
            "{}",
            log.text
        );
        assert!(!log.text.contains("update and remove"), "{}", log.text);
    }

    #[test]
    fn a_comment_shape_gives_the_fields_and_profiles_without_a_contract_check_nothing() {
        let plan = manifest(vec![file(
            "lib/types.ts",
            "Types.",
            &["export interface Settings  // {currency:string,monthStartDay:number}; monthStartDay 1-28"],
        )]);
        let fields = settings_fields(&plan, &NEXTJS.staging);
        let names: Vec<&str> = fields.iter().map(|field| field.name.as_str()).collect();
        assert_eq!(names, ["currency", "monthStartDay"]);
        assert_eq!(
            contract_problems(&plan, &crate::profile::PYTHON),
            Vec::new()
        );
    }
}
