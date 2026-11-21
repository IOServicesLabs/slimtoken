//! The conventions of a plan (`ArchitectOptions::semantics`,
//! `PlanOptions::semantics`): the facts that every file must read the same
//! way, stated once in the Types-layer file, and a deterministic check of a
//! manifest against them. From the second blind audit (5 race pairs with
//! every quality switch on): each file was plausible alone, but the files
//! disagreed on what the plan never stated: the sign of an amount, the
//! format of a date, which function moves a status (and sets its
//! timestamps), who owns the stored state, who renders the shell.
//!
//! - [`semantic_problems`]: a money field with no stated unit and sign, a
//!   date field with no stated format, a status union with no transition
//!   owner (or one that does not say which timestamps it sets), a dates
//!   module with no local `today`, a page or component that the plan has
//!   load or save storage beside the store provider, and a page that renders
//!   the shell that the root layout already renders. The architect asks the
//!   fragment that owns each problem once more (with `contract`, in the same
//!   round as the contract problems).
//! - [`conventions`]: the block that ends the module head of every worker,
//!   the same text for all of them (cacheable): the conventions read from
//!   the plan (a field the plan left open gets the profile's default; a
//!   balance stated as positive only becomes signed), the option flags of
//!   the records, the state owner, the shell, then the profile's fixed
//!   `conventions_rule`.
//!
//! Each part needs the profile's `semantics_rule`; PYTHON and RUST have
//! none, so nothing changes for them.

use crate::architect::{declared_names, identifiers, union_members, Manifest, ManifestFile};
use crate::contract::{declared_fields, stem, texts, types_files, ContractProblem};
use crate::profile::{glob_match, Layer, Profile};

/// Words of a field name that make it money (`amount`, `amountCents`,
/// `openingBalance`, `price`).
const MONEY_WORDS: &[&str] = &[
    "amount", "cents", "price", "cost", "balance", "fee", "salary", "payment",
];

/// Words that state a sign (`always > 0`, `positive`, `signed`).
const SIGN_WORDS: &[&str] = &[
    "positive",
    "negative",
    "nonnegative",
    "signed",
    "unsigned",
    "sign",
];

/// Comparisons that state a sign.
const SIGN_MARKS: &[&str] = &["> 0", ">0", ">= 0", ">=0", "< 0", "<0", "<= 0", "<=0"];

/// Texts (lower case) that state a date format.
const FORMAT_MARKS: &[&str] = &[
    "yyyy-mm-dd",
    "timestamp",
    "calendar date",
    "date-only",
    "date only",
];

/// Union members that end a piece of work (they set a completion time).
const DONE_WORDS: &[&str] = &[
    "done",
    "completed",
    "complete",
    "closed",
    "finished",
    "resolved",
];

/// The lower-case camel-case words of `name` (`openingBalance` -> `opening`,
/// `balance`).
fn camel_words(name: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut word = String::new();
    for ch in name.chars() {
        if (ch.is_ascii_uppercase() || ch == '_') && !word.is_empty() {
            words.push(std::mem::take(&mut word));
        }
        if ch.is_ascii_alphanumeric() {
            word.push(ch.to_ascii_lowercase());
        }
    }
    if !word.is_empty() {
        words.push(word);
    }
    words
}

fn is_money(name: &str, ty: &str) -> bool {
    ty.trim().starts_with("number")
        && camel_words(name)
            .iter()
            .any(|word| MONEY_WORDS.contains(&word.as_str()))
}

/// The default convention of a money field the plan left open, by its
/// name: an amount that moves money (`amount`, `amountCents`) is positive
/// with a kind for its direction; a balance (`balance`, `openingCents`) is
/// signed; a limit, target, price or fee is never negative.
fn default_money(name: &str) -> &'static str {
    let words = camel_words(name);
    let has = |word: &str| words.iter().any(|known| known == word);
    if has("amount") {
        "integer cents, always > 0 (the record's kind or category says expense or income)"
    } else if is_balance(name) {
        BALANCE_MONEY
    } else {
        "integer cents, always >= 0"
    }
}

/// The convention of a balance: signed, with 0 and debts valid.
const BALANCE_MONEY: &str = "integer cents, signed (below 0 is owed; 0 and debts are valid)";

/// A balance by its name (`balance`, `openingBalance`, `openingCents`), not
/// an amount that moves money.
fn is_balance(name: &str) -> bool {
    let words = camel_words(name);
    let has = |word: &str| words.iter().any(|known| known == word);
    !has("amount") && (has("balance") || has("opening"))
}

/// Whether a stated sign rules out 0 or below (`always > 0`, `positive`)
/// without allowing a sign (`signed`, `owed`, `< 0`).
fn states_positive_only(text: &str) -> bool {
    let lower = text.to_lowercase();
    (lower.contains("> 0") || lower.contains(">0") || has_word(text, &["positive"]))
        && !has_word(text, &["signed", "owed"])
        && !lower.contains("< 0")
        && !lower.contains("<0")
}

/// A flag of a record that the user sets (`Budget.rollover`, `Note.pinned`):
/// a boolean field, not one that archives or closes it (reversible states)
/// or marks it done (its status or toggle).
fn is_option_flag(field: &Field) -> bool {
    let name = field.name.to_lowercase();
    let bare = name.strip_prefix("is").unwrap_or(&name);
    field.ty.trim().trim_end_matches(';').trim() == "boolean"
        && !is_one_way_flag(&field.name)
        && !DONE_WORDS.contains(&bare)
        && !matches!(bare, "paid" | "read" | "deleted")
}

/// The option flags of the record types with an `id` (see
/// [`is_option_flag`]), in plan order.
fn option_flags(fields: &[Field]) -> Vec<&Field> {
    fields
        .iter()
        .filter(|field| {
            is_option_flag(field)
                && fields
                    .iter()
                    .any(|other| other.owner == field.owner && other.name == "id")
        })
        .collect()
}

/// The option flags of the plan's record types, `(type, field)`, for the
/// page that adds those records (`pagelocal::page_lines`).
pub(crate) fn record_option_flags(manifest: &Manifest, profile: &Profile) -> Vec<(String, String)> {
    let types = types_files(manifest, &profile.staging);
    let fields = record_fields(&types);
    option_flags(&fields)
        .into_iter()
        .map(|field| (field.owner.clone(), field.name.clone()))
        .collect()
}

/// For a money field of a part record (`Split.amountCents`) that the plan
/// left open: the stated money field of the record that holds the parts
/// (`Transaction.amountCents`, with `splits: Split[]`) and its text.
fn parent_money<'a, 'b>(
    pieces: &'a [Piece],
    fields: &'b [Field],
    field: &Field,
) -> Option<(&'b Field, &'a str)> {
    fields
        .iter()
        .filter(|holder| {
            holder.owner != field.owner
                && identifiers(&holder.ty)
                    .iter()
                    .any(|&(_, word)| word == field.owner)
        })
        .find_map(|holder| {
            fields
                .iter()
                .filter(|money| {
                    money.owner == holder.owner
                        && is_money(&money.name, &money.ty)
                        && camel_words(&money.name) == camel_words(&field.name)
                })
                .find_map(|money| {
                    stated(pieces, money, true, states_sign).map(|text| (money, text))
                })
        })
}

/// A moment in time by its name: `createdAt`, `completedAt`.
fn is_timestamp(name: &str) -> bool {
    name.len() > 2
        && name.ends_with("At")
        && name[..name.len() - 2]
            .chars()
            .next_back()
            .is_some_and(|ch| ch.is_ascii_lowercase())
}

fn is_date(name: &str, ty: &str) -> bool {
    ty.trim().starts_with("string")
        && (name == "date" || name.ends_with("Date") || is_timestamp(name))
}

/// The code of an export line: the text before its `//` comment.
fn code(line: &str) -> &str {
    line.split("//").next().unwrap_or(line)
}

/// The comment of an export line, empty when it has none.
fn comment(line: &str) -> &str {
    line.split_once("//").map_or("", |(_, rest)| rest.trim())
}

/// One field of a record type of the Types-layer files.
#[derive(Debug, Clone)]
struct Field {
    /// The type that declares it (`Transaction`).
    owner: String,
    name: String,
    ty: String,
}

impl Field {
    fn label(&self) -> String {
        format!("`{}.{}`", self.owner, self.name)
    }
}

/// The fields of the interfaces and object types that the Types-layer files
/// declare, in their order.
fn record_fields(types: &[&ManifestFile]) -> Vec<Field> {
    let mut fields = Vec::new();
    for line in types.iter().flat_map(|file| file.exports.iter()) {
        let head = code(line);
        let declares_type = head.contains("interface ") || head.contains("type ");
        let Some(owner) = declared_names(head).into_iter().next() else {
            continue;
        };
        if !declares_type || !owner.starts_with(|ch: char| ch.is_ascii_uppercase()) {
            continue;
        }
        let Some((declared, _)) = declared_fields(line) else {
            continue;
        };
        for (name, ty) in declared {
            fields.push(Field {
                owner: owner.clone(),
                name,
                ty,
            });
        }
    }
    fields
}

/// One piece of a Types-layer line: the type that the line declares (for
/// the comment of an interface) and the text between two `;`.
struct Piece {
    owner: Option<String>,
    text: String,
}

/// The pieces of the comments of the Types-layer lines, and of the code of
/// the lines that declare no type (`export const MONEY = '...'`), split at
/// `;`.
fn segments(types: &[&ManifestFile]) -> Vec<Piece> {
    let mut pieces = Vec::new();
    for line in types.iter().flat_map(|file| file.exports.iter()) {
        let head = code(line);
        let declares_type = head.contains("interface ") || head.contains("type ");
        let owner = declared_names(head)
            .into_iter()
            .next()
            .filter(|_| declares_type);
        let mut texts = vec![comment(line)];
        if !declares_type {
            texts.push(head);
        }
        for text in texts {
            for piece in text
                .split(';')
                .map(str::trim)
                .filter(|piece| !piece.is_empty())
            {
                pieces.push(Piece {
                    owner: owner.clone(),
                    text: piece.to_string(),
                });
            }
        }
    }
    pieces
}

fn has_word(text: &str, words: &[&str]) -> bool {
    identifiers(text)
        .iter()
        .any(|&(_, word)| words.contains(&word.to_lowercase().as_str()))
}

fn states_sign(text: &str) -> bool {
    has_word(text, SIGN_WORDS) || SIGN_MARKS.iter().any(|mark| text.contains(mark))
}

fn states_format(text: &str) -> bool {
    let lower = text.to_lowercase();
    FORMAT_MARKS.iter().any(|mark| lower.contains(mark)) || has_word(text, &["iso"])
}

/// Whether a piece names `field`: in the comment of the field's own type
/// (`own`) the field anywhere (`amountCents signed`), elsewhere the field as
/// a key (`amount:`, `createdAt, completedAt:`) or qualified
/// (`Task.completedAt`); `*At` for a timestamp, or money and amounts as a
/// whole for a money field.
fn mentions(piece: &str, field: &Field, money: bool, own: bool) -> bool {
    let words = identifiers(piece);
    let keyed = words.iter().any(|&(at, word)| {
        if word != field.name {
            return false;
        }
        let after = piece[at + word.len()..].trim_start().chars().next();
        let qualified = piece[..at].ends_with('.');
        own || qualified || matches!(after, Some(':' | ',' | '=' | '/' | ')') | None)
    });
    keyed
        || (is_timestamp(&field.name) && piece.contains("*At"))
        || (money
            && words
                .iter()
                .any(|&(_, word)| matches!(word.to_lowercase().as_str(), "money" | "amounts")))
}

/// The piece that states the convention of `field`, if the plan has one:
/// first in the comment of the field's own type, then anywhere.
fn stated<'a>(
    pieces: &'a [Piece],
    field: &Field,
    money: bool,
    check: fn(&str) -> bool,
) -> Option<&'a str> {
    let own = |piece: &&Piece| piece.owner.as_deref() == Some(field.owner.as_str());
    pieces
        .iter()
        .filter(own)
        .find(|piece| mentions(&piece.text, field, money, true) && check(&piece.text))
        .or_else(|| {
            pieces
                .iter()
                .find(|piece| mentions(&piece.text, field, money, false) && check(&piece.text))
        })
        .map(|piece| piece.text.as_str())
}

/// A status or stage union of the plan.
#[derive(Debug, Clone)]
struct StatusUnion {
    /// The union's name (`Status`), or `Task.status` for an inline union.
    name: String,
    members: Vec<String>,
    /// The line that declares it (for its comment).
    line: String,
    /// The record types with a field of this union.
    entities: Vec<String>,
}

fn status_unions(types: &[&ManifestFile], fields: &[Field]) -> Vec<StatusUnion> {
    let mut unions = Vec::new();
    for line in types.iter().flat_map(|file| file.exports.iter()) {
        let head = code(line);
        let members = union_members(head);
        let Some(name) = declared_names(head).into_iter().next() else {
            continue;
        };
        if members.is_empty() || head.contains('{') || !head.contains('=') {
            continue;
        }
        if !(name.ends_with("Status") || name.ends_with("Stage")) {
            continue;
        }
        let entities = fields
            .iter()
            .filter(|field| field.ty.trim() == name)
            .map(|field| field.owner.clone())
            .collect();
        unions.push(StatusUnion {
            name,
            members,
            line: line.clone(),
            entities,
        });
    }
    // Inline unions of a `status` or `stage` field.
    for field in fields {
        let members = union_members(&field.ty);
        if members.is_empty() || !matches!(field.name.as_str(), "status" | "stage") {
            continue;
        }
        let line = types
            .iter()
            .flat_map(|file| file.exports.iter())
            .find(|line| declared_names(code(line)).first() == Some(&field.owner))
            .cloned()
            .unwrap_or_default();
        unions.push(StatusUnion {
            name: format!("{}.{}", field.owner, field.name),
            members,
            line,
            entities: vec![field.owner.clone()],
        });
    }
    unions
}

/// The names that each file outside the Types layer declares, by file.
fn declared_by_file<'a>(manifest: &'a Manifest, profile: &Profile) -> Vec<(&'a str, Vec<String>)> {
    manifest
        .files
        .iter()
        .filter(|file| profile.staging.layer_of(&file.path) != Some(Layer::Types))
        .map(|file| {
            (
                file.path.as_str(),
                file.exports
                    .iter()
                    .flat_map(|line| declared_names(code(line)))
                    .collect(),
            )
        })
        .collect()
}

/// A lib function name that changes a status (`setTaskStatus`,
/// `moveTask`): a verb with `status` or `stage` in the name, or `move` with
/// one of `entities` (`moveTask` for `Task`; not `moveColumn`).
fn is_status_setter(name: &str, entities: &[String]) -> bool {
    let lower = name.to_lowercase();
    let verb = ["set", "move", "change", "update", "transition"]
        .iter()
        .any(|verb| lower.starts_with(verb));
    (verb && (lower.contains("status") || lower.contains("stage")))
        || lower.strip_prefix("move").is_some_and(|rest| {
            entities
                .iter()
                .any(|entity| !entity.is_empty() && rest.starts_with(&entity.to_lowercase()))
        })
}

/// The plan's status setter for `union` when its comment names none:
/// first a setter that names one of the union's records (`setTaskStatus`,
/// `moveTask` for `Task`), then a `set`/`move` function whose signature
/// takes the union (`moveTask(tasks, id, status: Status)`, for a plan whose
/// records are not declared yet), else, when the plan has this one status
/// union, its one setter with `status` or `stage` in the name. (name, file).
fn status_setter<'a>(
    union: &StatusUnion,
    unions: usize,
    manifest: &'a Manifest,
    profile: &Profile,
) -> Option<(String, &'a str)> {
    let exports: Vec<(String, &'a str, &'a str)> = manifest
        .files
        .iter()
        .filter(|file| profile.staging.layer_of(&file.path) != Some(Layer::Types))
        .flat_map(|file| {
            file.exports.iter().flat_map(move |line| {
                declared_names(code(line))
                    .into_iter()
                    .map(move |name| (name, file.path.as_str(), line.as_str()))
            })
        })
        .collect();
    let named = exports.iter().find(|(name, _, _)| {
        let lower = name.to_lowercase();
        is_status_setter(name, &union.entities)
            && union
                .entities
                .iter()
                .any(|entity| !entity.is_empty() && lower.contains(&entity.to_lowercase()))
    });
    let typed = || {
        let union_type = union.name.split('.').next().unwrap_or(&union.name);
        exports.iter().find(|(name, _, line)| {
            let lower = name.to_lowercase();
            (lower.starts_with("move") || lower.starts_with("set"))
                && !union.name.contains('.')
                && identifiers(code(line))
                    .iter()
                    .skip(1)
                    .any(|&(_, word)| word == union_type)
        })
    };
    let generic = || {
        (unions == 1)
            .then(|| {
                exports
                    .iter()
                    .find(|(name, _, _)| is_status_setter(name, &union.entities))
            })
            .flatten()
    };
    named
        .or_else(typed)
        .or_else(generic)
        .map(|(name, path, _)| (name.clone(), *path))
}

/// The functions outside the types that the union's comment names as its
/// owner: (name, file).
fn transition_owner<'a>(
    union: &StatusUnion,
    declared: &'a [(&'a str, Vec<String>)],
) -> Option<(String, &'a str)> {
    identifiers(comment(&union.line))
        .iter()
        .filter(|&&(_, word)| word.starts_with(|ch: char| ch.is_ascii_lowercase()))
        .find_map(|&(_, word)| {
            declared
                .iter()
                .find(|(_, names)| names.iter().any(|name| name == word))
                .map(|(path, _)| (word.to_string(), *path))
        })
}

/// The timestamp fields of the union's records that a status change sets
/// or clears (`completedAt`, `startedAt`; not `createdAt` or `updatedAt`).
fn transition_stamps(union: &StatusUnion, fields: &[Field]) -> Vec<String> {
    let mut stamps: Vec<String> = Vec::new();
    for field in fields {
        if union.entities.contains(&field.owner)
            && is_timestamp(&field.name)
            && !matches!(field.name.as_str(), "createdAt" | "updatedAt")
            && !stamps.contains(&field.name)
        {
            stamps.push(field.name.clone());
        }
    }
    stamps
}

fn export_names(file: &ManifestFile) -> Vec<String> {
    file.exports
        .iter()
        .flat_map(|line| declared_names(code(line)))
        .collect()
}

/// A context provider file outside the types: its stem or an export ends
/// in `Provider`, or its exports make a context.
fn is_provider(file: &ManifestFile, profile: &Profile) -> bool {
    if profile.staging.layer_of(&file.path) == Some(Layer::Types)
        || profile.staging.is_test(&file.path)
    {
        return false;
    }
    stem(&file.path).ends_with("Provider")
        || export_names(file)
            .iter()
            .any(|name| name.ends_with("Provider"))
        || file
            .exports
            .iter()
            .any(|line| line.contains("createContext"))
}

/// The store provider of the plan: (file, hook), its hook a `useX` export
/// of that file (`useStore`). Of the context providers, the one that holds
/// the app state: not a theme's (`ThemeProvider`, which only keeps its
/// theme, is never the owner unless it names the state too), first one that
/// names the state type or the storage module's functions, or whose name
/// says store, app, data or state.
pub(crate) fn store_provider<'a>(
    manifest: &'a Manifest,
    profile: &Profile,
) -> Option<(&'a str, String)> {
    let state_type = match profile.staging.quality.state_type {
        "" => "AppState",
        name => name,
    };
    let storage = storage_module(manifest, profile, "")
        .map(|(_, names)| names)
        .unwrap_or_default();
    let mut best: Option<(u8, &ManifestFile)> = None;
    for file in manifest
        .files
        .iter()
        .filter(|file| is_provider(file, profile))
    {
        let words = camel_words(stem(&file.path));
        let providers: Vec<String> = export_names(file)
            .into_iter()
            .filter(|name| name.ends_with("Provider"))
            .collect();
        let theme = words.iter().any(|word| word == "theme")
            || (!providers.is_empty() && providers.iter().all(|name| name.contains("Theme")));
        let reaches = texts(file).any(|text| {
            identifiers(text)
                .iter()
                .any(|&(_, word)| word == state_type || storage.iter().any(|name| name == word))
        });
        let named = words
            .iter()
            .any(|word| matches!(word.as_str(), "store" | "app" | "data" | "state"));
        let score = match (theme, reaches || named) {
            (true, false) => continue,
            (true, true) => 1,
            (false, false) => 2,
            (false, true) => 3,
        };
        if best.is_none_or(|(known, _)| score > known) {
            best = Some((score, file));
        }
    }
    let (_, file) = best?;
    let hook = file
        .exports
        .iter()
        .flat_map(|line| declared_names(code(line)))
        .find(|name| {
            name.strip_prefix("use")
                .is_some_and(|rest| rest.starts_with(|ch: char| ch.is_ascii_uppercase()))
        })
        .unwrap_or_default();
    Some((file.path.as_str(), hook))
}

/// The storage module of the plan (`lib/storage.ts`) and the functions it
/// declares: a lib (Core) file named for storage (`storage`, `persist`,
/// `db`), else such a file in another layer outside the types, else a file
/// whose exports mention `localStorage`, outside the types (whose
/// `STORAGE_KEY` line is not the module), the layouts (a theme script) and
/// the providers.
fn storage_module<'a>(
    manifest: &'a Manifest,
    profile: &Profile,
    provider: &str,
) -> Option<(&'a str, Vec<String>)> {
    let staging = &profile.staging;
    let layout_glob = staging.quality.layout_glob;
    let candidates = || {
        manifest.files.iter().filter(|file| {
            file.path != provider
                && (file.path.ends_with(".ts") || file.path.ends_with(".tsx"))
                && !staging.is_test(&file.path)
                && staging.layer_of(&file.path) != Some(Layer::Types)
                && !is_provider(file, profile)
        })
    };
    let named = |file: &&ManifestFile| {
        let own = stem(&file.path).to_lowercase();
        own.starts_with("storage")
            || own.starts_with("persist")
            || own == "db"
            || own.starts_with("database")
    };
    let file = candidates()
        .find(|file| named(file) && staging.layer_of(&file.path) == Some(Layer::Core))
        .or_else(|| candidates().find(named))
        .or_else(|| {
            candidates().find(|file| {
                !(!layout_glob.is_empty() && glob_match(layout_glob, &file.path))
                    && file
                        .exports
                        .iter()
                        .any(|line| line.contains("localStorage"))
            })
        })?;
    let names = file
        .exports
        .iter()
        .flat_map(|line| declared_names(code(line)))
        .filter(|name| name.starts_with(|ch: char| ch.is_ascii_lowercase()))
        .collect();
    Some((file.path.as_str(), names))
}

/// The components that a root layout renders (`<Nav/>`, `<AppShell>`), by
/// what the plan says, without state owners (`<StoreProvider>`):
/// (layout path, component name).
fn shell_components<'a>(manifest: &'a Manifest, profile: &Profile) -> Vec<(&'a str, String)> {
    let quality = &profile.staging.quality;
    if quality.layout_glob.is_empty() || quality.component_glob.is_empty() {
        return Vec::new();
    }
    let components: Vec<&str> = manifest
        .files
        .iter()
        .filter(|file| glob_match(quality.component_glob, &file.path))
        .map(|file| stem(&file.path))
        .filter(|name| !name.ends_with("Provider") && !name.ends_with("Providers"))
        .collect();
    let mut found = Vec::new();
    for layout in manifest
        .files
        .iter()
        .filter(|file| glob_match(quality.layout_glob, &file.path))
    {
        // The root layout only: a nested layout wraps its own pages.
        if layout.path.matches('/').count() > 1 {
            continue;
        }
        for name in &components {
            // A layout names only what it renders (`Root layout rendering
            // AppShell`).
            let named = texts(layout).any(|text| {
                renders(text, name) || identifiers(text).iter().any(|&(_, word)| word == *name)
            });
            if named
                && !found
                    .iter()
                    .any(|(_, known): &(&str, String)| known == name)
            {
                found.push((layout.path.as_str(), name.to_string()));
            }
        }
    }
    found
}

/// Words of a component name that make it the app's shell.
const SHELL_WORDS: &[&str] = &[
    "shell", "nav", "navbar", "header", "sidebar", "topbar", "frame",
];

/// A shell-like component (`AppShell`, `Nav`) that two or more pages render
/// and the root layout does not: (layout, component, pages).
fn page_shell<'a>(manifest: &'a Manifest, profile: &Profile) -> Option<(&'a str, String, usize)> {
    let quality = &profile.staging.quality;
    if quality.layout_glob.is_empty() || quality.component_glob.is_empty() {
        return None;
    }
    let layout = manifest.files.iter().find(|file| {
        glob_match(quality.layout_glob, &file.path) && file.path.matches('/').count() <= 1
    })?;
    let shells = shell_components(manifest, profile);
    manifest
        .files
        .iter()
        .filter(|file| glob_match(quality.component_glob, &file.path))
        .map(|file| stem(&file.path))
        .filter(|name| {
            camel_words(name)
                .iter()
                .any(|word| SHELL_WORDS.contains(&word.as_str()))
                && !shells.iter().any(|(_, known)| known == name)
        })
        .find_map(|name| {
            let count = manifest
                .files
                .iter()
                .filter(|file| glob_match(quality.page_glob, &file.path))
                .filter(|page| texts(page).any(|text| renders(text, name)))
                .count();
            (count >= 2).then(|| (layout.path.as_str(), name.to_string(), count))
        })
}

/// Whether `text` renders component `name` (`<Name/>`, `<Name>`, `<Name `).
fn renders(text: &str, name: &str) -> bool {
    let tag = format!("<{name}");
    text.match_indices(&tag).any(|(at, _)| {
        text[at + tag.len()..]
            .chars()
            .next()
            .is_none_or(|ch| !(ch.is_ascii_alphanumeric() || ch == '_'))
    })
}

fn join_labels(labels: &[String]) -> String {
    labels.join(", ")
}

/// The problems of `manifest` against the conventions of `profile` (see
/// the module docs). Deterministic and idempotent; empty for a profile with
/// no `semantics_rule`.
pub fn semantic_problems(manifest: &Manifest, profile: &Profile) -> Vec<ContractProblem> {
    let staging = &profile.staging;
    let quality = &staging.quality;
    if quality.semantics_rule.is_empty() {
        return Vec::new();
    }
    let types = types_files(manifest, staging);
    let Some(types_path) = types.first().map(|file| file.path.clone()) else {
        return Vec::new();
    };
    let fields = record_fields(&types);
    let pieces = segments(&types);
    let mut problems = Vec::new();

    // Money: a unit and a sign for each money field.
    let open: Vec<String> = fields
        .iter()
        .filter(|field| is_money(&field.name, &field.ty))
        .filter(|field| stated(&pieces, field, true, states_sign).is_none())
        .filter(|field| parent_money(&pieces, &fields, field).is_none())
        .map(Field::label)
        .collect();
    if !open.is_empty() {
        problems.push(ContractProblem {
            path: Some(types_path.clone()),
            text: format!(
                "{} hold money with no stated unit and sign: say it in the comment of each type \
                 (an amount that moves money: `amount: integer cents, always > 0, kind says expense \
                 or income`, with a `kind` field when the record has none; a balance: `balance: \
                 integer cents, signed`; a limit, target or price: `limitCents: integer cents, \
                 always >= 0`); every validator, form and seed follows that one convention.",
                join_labels(&open)
            ),
        });
    }

    // Dates: a format for each date field.
    let dates: Vec<&Field> = fields
        .iter()
        .filter(|field| is_date(&field.name, &field.ty))
        .collect();
    // A date field the plan left open needs no second planner call: the
    // name decides its format (`*At` a timestamp, `date` and `*Date` a
    // calendar date), and [`conventions`] states that default to every file.
    // On Muse that call cost 15 s of a 46 s plan (20261116-131034).
    // The dates module gives the local today.
    if !dates.is_empty() {
        for file in manifest.files.iter().filter(|file| {
            staging.layer_of(&file.path) == Some(Layer::Core)
                && matches!(
                    stem(&file.path).to_lowercase().as_str(),
                    "date" | "dates" | "time" | "datetime"
                )
        }) {
            let today = file.exports.iter().any(|line| {
                declared_names(code(line))
                    .iter()
                    .any(|name| name.to_lowercase().contains("today"))
            });
            if !today {
                problems.push(ContractProblem {
                    path: Some(file.path.clone()),
                    text: "the dates module has no `today`: export `today(): string` (the local \
                           calendar date 'YYYY-MM-DD', never `toISOString().slice(0, 10)`), and \
                           let its parse function read both a 'YYYY-MM-DD' date and a full ISO \
                           timestamp."
                        .to_string(),
                });
            }
        }
    }

    // Status: each union names the one function that changes it, and the
    // timestamps it sets or clears.
    let declared = declared_by_file(manifest, profile);
    let unions = status_unions(&types, &fields);
    for union in &unions {
        let done = union
            .members
            .iter()
            .any(|member| DONE_WORDS.contains(&member.to_lowercase().as_str()));
        let stamps = transition_stamps(union, &fields);
        if !done && stamps.is_empty() {
            continue;
        }
        let setter = status_setter(union, unions.len(), manifest, profile);
        let stamp_list = stamps
            .iter()
            .map(|stamp| format!("`{stamp}`"))
            .collect::<Vec<_>>()
            .join(", ");
        match transition_owner(union, &declared) {
            Some((owner, path)) => {
                let text = comment(&union.line);
                let unsaid: Vec<&String> = stamps
                    .iter()
                    .filter(|stamp| {
                        !identifiers(text)
                            .iter()
                            .any(|&(_, word)| word == stamp.as_str())
                    })
                    .collect();
                if !unsaid.is_empty() {
                    problems.push(ContractProblem {
                        path: Some(types_path.clone()),
                        text: format!(
                            "the comment of `{}` names `{owner}` ({path}) but not the timestamps it \
                             sets or clears: say for {stamp_list} which status sets it and which \
                             clears it (`done sets completedAt, leaving done clears it`).",
                            union.name
                        ),
                    });
                }
            }
            None => {
                let hint = setter
                    .as_ref()
                    .map(|(name, path)| format!(" (the plan has `{name}` in `{path}`)"))
                    .unwrap_or_default();
                let stamps_hint = if stamps.is_empty() {
                    String::new()
                } else {
                    format!(", the timestamps each one sets or clears ({stamp_list})")
                };
                problems.push(ContractProblem {
                    path: Some(types_path.clone()),
                    text: format!(
                        "`{}` has no transition owner: in its comment give its transitions{stamps_hint} \
                         and the ONE lib function that makes them{hint}, e.g. `any -> any via \
                         setTaskStatus (lib/tasks.ts); done sets completedAt, leaving done clears it`.",
                        union.name
                    ),
                });
                if setter.is_none() {
                    // The lib module of the record adds the function.
                    let entity = union.entities.first().cloned().unwrap_or_default();
                    let lower = entity.to_lowercase();
                    let owner = manifest.files.iter().find(|file| {
                        let own = stem(&file.path).to_lowercase();
                        staging.layer_of(&file.path) == Some(Layer::Core)
                            && !entity.is_empty()
                            && (own == lower || own == format!("{lower}s"))
                    });
                    if let Some(owner) = owner {
                        let (field, ty) = match union.name.split_once('.') {
                            Some((_, field)) => (field.to_string(), format!("{entity}['{field}']")),
                            None => ("status".to_string(), union.name.clone()),
                        };
                        problems.push(ContractProblem {
                            path: Some(owner.path.clone()),
                            text: format!(
                                "no export changes a `{entity}`'s status: add `set{entity}Status(items: \
                                 {entity}[], id: string, {field}: {ty}): {entity}[]` with the timestamps \
                                 it sets and clears in its comment; every page that changes a status \
                                 calls it."
                            ),
                        });
                    }
                }
            }
        }
    }

    // One state owner: with a store provider, no page or component loads or
    // saves storage itself.
    if let Some((provider, hook)) = store_provider(manifest, profile) {
        if let Some((storage, functions)) = storage_module(manifest, profile, provider) {
            let via = if hook.is_empty() {
                format!("the store of `{provider}`")
            } else {
                format!("`{hook}` (`{provider}`)")
            };
            for file in manifest.files.iter().filter(|file| {
                file.path != provider
                    && file.path != storage
                    && (profile.is_consumer)(&file.path)
                    && !staging.is_test(&file.path)
            }) {
                let used = functions.iter().find(|name| {
                    texts(file).any(|text| {
                        identifiers(text)
                            .iter()
                            .any(|&(_, word)| word == name.as_str())
                    })
                });
                // A bare `localStorage` in another provider (a theme's) or a
                // layout (its theme script) keeps something of its own.
                let own_store = is_provider(file, profile)
                    || (!quality.layout_glob.is_empty()
                        && glob_match(quality.layout_glob, &file.path));
                let named = used.cloned().or_else(|| {
                    (!own_store && texts(file).any(|text| text.contains("localStorage")))
                        .then(|| "localStorage".to_string())
                });
                if let Some(name) = named {
                    problems.push(ContractProblem {
                        path: Some(file.path.clone()),
                        text: format!(
                            "the plan has this file load or save the stored state (`{name}`) beside \
                             the store provider: only `{provider}` loads and saves it; this file reads \
                             and changes the state through {via}. Drop `{name}` from its contract."
                        ),
                    });
                }
            }
        }
    }

    // The shell: the root layout renders a shell that two or more pages
    // render on their own.
    if let Some((layout, name, count)) = page_shell(manifest, profile) {
        problems.push(ContractProblem {
            path: Some(layout.to_string()),
            text: format!(
                "{count} pages render <{name}/> on their own: the root layout renders it once \
                 around `{{children}}` (name it in this file's export comment, `renders \
                 <{name}>{{children}}</{name}>`), and the pages render only their own content."
            ),
        });
    }
    // The shell: no page renders what the root layout renders.
    for (layout, name) in shell_components(manifest, profile) {
        for page in manifest
            .files
            .iter()
            .filter(|file| glob_match(quality.page_glob, &file.path))
        {
            if texts(page).any(|text| renders(text, &name)) {
                problems.push(ContractProblem {
                    path: Some(page.path.clone()),
                    text: format!(
                        "`{layout}` already renders <{name}/> around every page: this page renders \
                         only its own content; drop <{name}/> from its contract."
                    ),
                });
            }
        }
    }
    problems
}

/// Add `label` to the entry of `groups` with `text` (a new one at the end).
fn group(groups: &mut Vec<(String, Vec<String>)>, text: String, label: String) {
    match groups.iter_mut().find(|(known, _)| *known == text) {
        Some((_, labels)) => {
            if !labels.contains(&label) {
                labels.push(label);
            }
        }
        None => groups.push((text, vec![label])),
    }
}

/// `A.x, B.y: text; C.z: other`.
fn grouped(groups: &[(String, Vec<String>)]) -> String {
    groups
        .iter()
        .map(|(text, labels)| format!("{}: {text}", labels.join(", ")))
        .collect::<Vec<_>>()
        .join("; ")
}

/// The next-date field of a recurring record (`nextDue`, `nextDueDate`,
/// `nextRunAt`): the field that makes a record a schedule.
fn is_next_date(name: &str) -> bool {
    let name = name.to_lowercase();
    ["nextdue", "nextdate", "nextrun", "nextoccurrence"]
        .iter()
        .any(|head| name.starts_with(head))
}

/// The anchor-day field of a recurring record (`dayOfMonth`, `anchorDay`).
fn is_anchor(name: &str) -> bool {
    matches!(name.to_lowercase().as_str(), "dayofmonth" | "anchorday")
}

/// A field of a recurring record besides its next date: its frequency or
/// its anchor day (`frequency`, `dayOfMonth`). Alone (`Habit.frequency`) it
/// makes no schedule.
fn is_cadence(name: &str) -> bool {
    is_anchor(name)
        || matches!(
            name.to_lowercase().as_str(),
            "frequency" | "recurrence" | "cadence"
        )
}

/// A flag, timestamp or status value that puts a record into a state the
/// user may want to undo (`archived`, `isArchived`, `closedAt`, `'closed'`).
fn is_one_way_flag(name: &str) -> bool {
    let name = name.to_lowercase();
    let name = name.strip_prefix("is").unwrap_or(&name);
    let name = name.strip_suffix("at").unwrap_or(name);
    matches!(name, "archived" | "closed" | "hidden")
}

/// The conventions block that `PlanOptions::semantics` adds to the end of
/// the module head (the same text for every worker). Empty for a profile
/// with no `semantics_rule`.
pub fn conventions(manifest: &Manifest, profile: &Profile) -> String {
    let staging = &profile.staging;
    let quality = &staging.quality;
    if quality.semantics_rule.is_empty() {
        return String::new();
    }
    let types = types_files(manifest, staging);
    let fields = record_fields(&types);
    let pieces = segments(&types);
    let mut lines: Vec<String> = Vec::new();

    // Money, as stated, else the default convention; fields with the same
    // text share one entry.
    let mut money: Vec<(String, Vec<String>)> = Vec::new();
    for field in fields
        .iter()
        .filter(|field| is_money(&field.name, &field.ty))
    {
        let text = match stated(&pieces, field, true, states_sign) {
            // Audit 9: a plan that said `openingBalance: always > 0` made
            // the validator and the form reject empty accounts and debts.
            Some(text) if is_balance(&field.name) && states_positive_only(text) => {
                // The sign alone gives the direction (a debt stored below
                // 0 and negated again by its kind double-negates), and the
                // types' comment says signed, so no lint reads `> 0` back.
                format!(
                    "{BALANCE_MONEY}: the value alone gives the direction (a kind only labels it, \
                     never flips the sign) and the field's comment in the types says signed, in \
                     place of the plan's `{text}`"
                )
            }
            Some(text) => text.to_string(),
            None => match parent_money(&pieces, &fields, field) {
                Some((parent, text)) => {
                    format!(
                        "the unit and sign of {} ({text}), the parts summing to it",
                        parent.label()
                    )
                }
                None => default_money(&field.name).to_string(),
            },
        };
        group(&mut money, text, field.label());
    }
    if !money.is_empty() {
        lines.push(format!("- Money: {}.", grouped(&money)));
        // Audits 6-8: a fixed 'USD' in tables and CSV, JPY shown 100x or
        // with 2 decimals.
        lines.push(
            "- Money display: an amount in integer cents stays major unit x 100 whatever the \
             currency; a form takes major units and stores `Math.round(value * 100)`, and the one \
             format helper divides by 100 and formats with `Intl.NumberFormat` and the currency \
             setting, which rounds to that currency's own fraction digits (0 for JPY and KRW); \
             tables, charts and CSV exports use it too. No format call or export passes a literal \
             currency code: the currency comes from the setting (a currency picker may list codes, \
             the default settings hold the default)."
                .to_string(),
        );
    }

    // Dates, as stated, else by the name: `*At` a timestamp, the rest dates.
    let mut dates: Vec<(String, Vec<String>)> = Vec::new();
    for field in fields
        .iter()
        .filter(|field| is_date(&field.name, &field.ty))
    {
        let text = match stated(&pieces, field, false, states_format) {
            Some(text) => text.to_string(),
            None if is_timestamp(&field.name) => "timestamp, full ISO".to_string(),
            None => "calendar date 'YYYY-MM-DD'".to_string(),
        };
        group(&mut dates, text, field.label());
    }
    if !dates.is_empty() {
        let module = manifest
            .files
            .iter()
            .find(|file| {
                staging.layer_of(&file.path) == Some(Layer::Core)
                    && matches!(stem(&file.path).to_lowercase().as_str(), "date" | "dates")
            })
            .map(|file| {
                format!(
                    " Parse, compare and format dates only with `{}`.",
                    file.path
                )
            })
            .unwrap_or_default();
        lines.push(format!("- Dates: {}.{module}", grouped(&dates)));
    }

    // Status changes.
    let declared = declared_by_file(manifest, profile);
    let unions = status_unions(&types, &fields);
    for union in &unions {
        let text = comment(&union.line);
        // The owner the comment names, else the plan's setter of this union.
        let owner = transition_owner(union, &declared)
            .or_else(|| status_setter(union, unions.len(), manifest, profile));
        let stamps = transition_stamps(union, &fields);
        let members = union
            .members
            .iter()
            .map(|member| format!("'{member}'"))
            .collect::<Vec<_>>()
            .join(" | ");
        let rule = match (&owner, text.is_empty()) {
            (Some((name, path)), true) => {
                format!("changes only through `{name}` (`{path}`), on every page that changes it")
            }
            (Some((name, path)), false) => format!(
                "changes only through `{name}` (`{path}`), on every page that changes it: {text}"
            ),
            (None, false) => text.to_string(),
            (None, true) => String::new(),
        };
        let stamps_rule = if stamps.is_empty() || owner.is_none() {
            String::new()
        } else {
            format!(
                "; that function also sets and clears {}",
                stamps
                    .iter()
                    .map(|stamp| format!("`{stamp}`"))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        };
        if !rule.is_empty() {
            lines.push(format!(
                "- Status `{}` ({members}) {rule}{stamps_rule}.",
                union.name
            ));
        }
    }

    // Schedules, only for a record type with schedule fields (audits 6-8:
    // next due rolled from today or the paid date, overdue never due, no or
    // two transactions per payment, an unclamped month step).
    // Only a record with a next-date field recurs; its frequency and anchor
    // fields join it (a bare `Habit.frequency` makes no schedule).
    let next: Vec<&Field> = fields
        .iter()
        .filter(|field| is_next_date(&field.name))
        .collect();
    if !next.is_empty() {
        let schedule: Vec<&Field> = fields
            .iter()
            .filter(|field| {
                is_next_date(&field.name)
                    || (is_cadence(&field.name)
                        && next.iter().any(|next| next.owner == field.owner))
            })
            .collect();
        let anchored = schedule.iter().any(|field| is_anchor(&field.name));
        let next_labels = next
            .iter()
            .map(|field| field.label())
            .collect::<Vec<_>>()
            .join(", ");
        // A transaction only in an app with money fields.
        let entry = if money.is_empty() {
            "one completion entry, when the types have one"
        } else {
            "one transaction, or one completion entry for a record without money"
        };
        let (step, seed) = if anchored {
            (
                " (or recomputes it from its anchor day)",
                " The seed sets the next date consistent with its anchor day.",
            )
        } else {
            ("", "")
        };
        lines.push(format!(
            "- Schedules ({}): the stored next date ({next_labels}) is the one source of the next \
             occurrence; it is due when that date <= today, so an overdue one stays due until it \
             is done and is never rolled forward on read. Paying or completing an occurrence \
             records it exactly once ({entry}), dated that occurrence, and moves the stored next \
             date one period on from its stored value{step}, never from today or the date it was \
             done, with the dates module, clamped to the month end (Jan 31 + 1 month = Feb 28).{seed}",
            schedule
                .iter()
                .map(|field| field.label())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }

    // Reversible states, only when a record can be archived or closed
    // (audits 6-8: no unarchive or reopen, archived records gone from view).
    // A flag or timestamp gets a reverse export; a status member is reversed
    // by the union's one status function (the Status line owns `status`).
    let flags: Vec<String> = fields
        .iter()
        .filter(|field| is_one_way_flag(&field.name))
        .map(Field::label)
        .collect();
    let members: Vec<String> = unions
        .iter()
        .filter_map(|union| {
            let member = union
                .members
                .iter()
                .find(|member| is_one_way_flag(member))?;
            let back = union.members.iter().find(|other| *other != member)?;
            Some(format!("`{}` '{member}' -> '{back}'", union.name))
        })
        .collect();
    let mut reversible: Vec<String> = Vec::new();
    if !flags.is_empty() {
        reversible.push(format!(
            "{}: the lib module that archives or closes a record also exports the reverse \
             (unarchive, reopen)",
            flags.join(", ")
        ));
    }
    if !members.is_empty() {
        reversible.push(format!(
            "{}: the status function allows the reverse transition",
            members.join(", ")
        ));
    }
    if !reversible.is_empty() {
        lines.push(format!(
            "- Reversible states ({}); the page that sets the state offers both directions; \
             archived or closed records stay reachable through a filter or section (`Show \
             archived`) and are left out only of pickers for new records.",
            reversible.join("; ")
        ));
    }

    // Option flags, only when a record has one (audit 9: `Budget.rollover`
    // was read by the lib, but every new budget got `false` and no form
    // offered it).
    let options: Vec<String> = option_flags(&fields)
        .into_iter()
        .map(Field::label)
        .collect();
    if !options.is_empty() {
        lines.push(format!(
            "- Record flags ({}): the form that creates or edits the record offers each one (a \
             checkbox, or a toggle on its row), and the lib function that creates or updates the \
             record takes it and stores it, never a fixed `false`: as a field of its record or \
             input object, or as an optional last parameter (`rollover = false`), so calls with \
             the planned arguments still compile.",
            options.join(", ")
        ));
    }

    // The state owner.
    match store_provider(manifest, profile) {
        Some((provider, hook)) => {
            let storage = storage_module(manifest, profile, provider)
                .map(|(path, _)| format!(" (through `{path}`)"))
                .unwrap_or_default();
            let hook = if hook.is_empty() {
                "its hook".to_string()
            } else {
                format!("`{hook}`")
            };
            lines.push(format!(
                "- State: `{provider}` is the only file that loads and saves the stored state{storage}; \
                 every other file (the layout's components and the pages included) reads and changes \
                 the state, the theme too, through {hook}; to reset the data a page uses {hook} as well \
                 (its reset, or the seed state through its setter) and never clears the storage \
                 itself. The provider loads the stored state in a \
                 useEffect, so its first render holds the seed data: {hook} also returns `ready` \
                 (false until the stored state was read), and the provider renders a loading state \
                 instead of its children until `ready`, so no page, component or effect sees the \
                 seed data in place of the stored data. When nothing is stored, that effect saves \
                 the seed before it sets `ready`, so record ids stay the same across reloads."
            ));
        }
        None => lines.push(
            "- State: the plan has no store provider: each page loads the state in a useEffect and \
             saves the whole state (the seed at once when nothing is stored), and keeps a `ready` \
             flag (true once it loaded); a component \
             that the layout renders keeps no copy of the state (it loads it again right before it \
             saves)."
                .to_string(),
        ),
    }

    // The shell.
    let shells = shell_components(manifest, profile);
    if !shells.is_empty() {
        let names = shells
            .iter()
            .map(|(_, name)| format!("<{name}/>"))
            .collect::<Vec<_>>()
            .join(", ");
        let layout = shells[0].0;
        lines.push(format!(
            "- Shell: `{layout}` renders {names} around every page; a page renders only its own \
             content, never {names} again."
        ));
    }

    format!(
        "Conventions of this app (stated once for every file; follow them exactly):\n{}\n{}",
        lines.join("\n"),
        quality.conventions_rule
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profile::{NEXTJS, PYTHON, RUST};

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

    /// The audit-2 shapes: money and dates with no convention, a status
    /// union with no owner, pages that load storage beside the provider and
    /// render the layout's nav again.
    fn open_plan() -> Manifest {
        manifest(vec![
            file(
                "lib/types.ts",
                "Shared types.",
                &[
                    "export type Status = 'backlog' | 'in_progress' | 'done'",
                    "export interface Task { id: string; status: Status; dueDate: string; createdAt: string; completedAt: string | null }",
                    "export interface Transaction { id: string; accountId: string; amount: number; date: string }",
                    "export interface AppState { tasks: Task[]; transactions: Transaction[] }",
                ],
            ),
            file("lib/dates.ts", "Date helpers.", &["export function parseDate(iso: string): Date  // 'YYYY-MM-DD' only"]),
            file("lib/board.ts", "Board moves.", &["export function moveTask(tasks: Task[], id: string, status: Status): Task[]"]),
            file("lib/storage.ts", "Storage.", &["export function loadState(): AppState", "export function saveState(state: AppState): void"]),
            file("components/StoreProvider.tsx", "Store.", &["export function useStore(): {state: AppState}", "export default function StoreProvider({children})"]),
            file("components/Nav.tsx", "Top nav.", &["export default function Nav()"]),
            file("app/layout.tsx", "Root layout.", &["export default function RootLayout({ children })  // renders <StoreProvider> + <Nav/>"]),
            file("app/accounts/page.tsx", "Accounts.", &["export default function AccountsPage()  // loads state via lib/storage loadState then saveState"]),
            file("app/budgets/page.tsx", "Budgets.", &["export default function BudgetsPage()  // renders <Nav/> and the budget table"]),
            file("app/board/page.tsx", "Board.", &["export default function BoardPage()  // renders columns; uses useStore"]),
        ])
    }

    #[test]
    fn the_check_finds_the_open_conventions() {
        let problems = semantic_problems(&open_plan(), &NEXTJS);
        let find = |path: &str, text: &str| {
            problems
                .iter()
                .any(|problem| problem.path.as_deref() == Some(path) && problem.text.contains(text))
        };
        let all: Vec<&str> = problems
            .iter()
            .map(|problem| problem.text.as_str())
            .collect();
        assert!(
            find("lib/types.ts", "`Transaction.amount` hold money"),
            "{all:?}"
        );
        // Open date formats are not a problem: the conventions give the
        // default by the name.
        assert!(
            !all.iter()
                .any(|text| text.contains("have no stated format")),
            "{all:?}"
        );
        assert!(
            find("lib/types.ts", "`Status` has no transition owner"),
            "{all:?}"
        );
        assert!(
            find("lib/types.ts", "the plan has `moveTask` in `lib/board.ts`"),
            "{all:?}"
        );
        assert!(find("lib/dates.ts", "has no `today`"), "{all:?}");
        assert!(
            find(
                "app/accounts/page.tsx",
                "(`loadState`) beside the store provider"
            ),
            "{all:?}"
        );
        assert!(
            find("app/budgets/page.tsx", "already renders <Nav/>"),
            "{all:?}"
        );
        // The board page uses the hook and renders no nav; the layout's own
        // `<StoreProvider>` is not a shell; the provider itself is the owner.
        assert!(
            !problems.iter().any(|problem| matches!(
                problem.path.as_deref(),
                Some("app/board/page.tsx" | "app/layout.tsx" | "components/StoreProvider.tsx")
            )),
            "{all:?}"
        );
        assert_eq!(problems, semantic_problems(&open_plan(), &NEXTJS));
    }

    fn stated_plan() -> Manifest {
        manifest(vec![
            file(
                "lib/types.ts",
                "Shared types.",
                &[
                    "export type Status = 'backlog' | 'in_progress' | 'done'  // any -> any via moveTask (lib/board.ts); done sets completedAt, leaving done clears it",
                    "export interface Task { id: string; status: Status; dueDate: string; createdAt: string; completedAt: string | null }  // dueDate: calendar date 'YYYY-MM-DD'; createdAt, completedAt: timestamp, full ISO",
                    "export interface Transaction { id: string; amount: number; kind: 'expense' | 'income'; date: string }  // amount: integer cents, always > 0, kind says expense or income; date: calendar date 'YYYY-MM-DD'",
                ],
            ),
            file("lib/dates.ts", "Date helpers.", &["export function toDay(value: string): string", "export function today(): string  // local date"]),
            file("lib/board.ts", "Board moves.", &["export function moveTask(tasks: Task[], id: string, status: Status): Task[]"]),
            file("lib/storage.ts", "Storage.", &["export function loadState(): AppState", "export function saveState(state: AppState): void"]),
            file("components/StoreProvider.tsx", "Store.", &["export function useStore(): {state: AppState}", "export default function StoreProvider({children})  // loadState on mount, saveState on change"]),
            file("components/AppShell.tsx", "Shell with nav.", &["export default function AppShell({children})"]),
            file("app/layout.tsx", "Root layout.", &["export default function RootLayout({ children })  // renders <StoreProvider> + <AppShell/>"]),
            file("app/board/page.tsx", "Board.", &["export default function BoardPage()  // moves via moveTask"]),
        ])
    }

    #[test]
    fn a_stated_plan_has_no_problems_and_its_block_quotes_the_plan() {
        let plan = stated_plan();
        assert_eq!(semantic_problems(&plan, &NEXTJS), Vec::new());
        let block = conventions(&plan, &NEXTJS);
        assert!(block.starts_with("Conventions of this app"), "{block}");
        assert!(block.contains("- Money: `Transaction.amount`: amount: integer cents, always > 0, kind says expense or income."), "{block}");
        assert!(
            block.contains(
                "`Task.createdAt`, `Task.completedAt`: createdAt, completedAt: timestamp, full ISO"
            ),
            "{block}"
        );
        assert!(
            block.contains("Parse, compare and format dates only with `lib/dates.ts`."),
            "{block}"
        );
        assert!(block.contains("- Status `Status` ('backlog' | 'in_progress' | 'done') changes only through `moveTask` (`lib/board.ts`)"), "{block}");
        assert!(
            block.contains("that function also sets and clears `completedAt`."),
            "{block}"
        );
        assert!(block.contains("- State: `components/StoreProvider.tsx` is the only file that loads and saves the stored state (through `lib/storage.ts`)"), "{block}");
        assert!(
            block.contains("through `useStore`; to reset the data a page uses `useStore` as well"),
            "{block}"
        );
        assert!(
            block.contains("- Shell: `app/layout.tsx` renders <AppShell/> around every page"),
            "{block}"
        );
        assert!(
            block.ends_with(NEXTJS.staging.quality.conventions_rule),
            "{block}"
        );
    }

    #[test]
    fn the_block_states_the_ready_flag_validation_search_and_catalogs() {
        // Audit 3: the store's first render is the seed, so a detail page
        // called notFound() on a user's record and a form kept seed values;
        // a lib function threw inside a state updater; a comment hit linked
        // to /tasks/<commentId>; two label catalogs had different ids.
        let block = conventions(&stated_plan(), &NEXTJS);
        assert!(
            block.contains(
                "`useStore` also returns `ready` (false until the stored state was read)"
            ),
            "{block}"
        );
        for needle in [
            "never `notFound()` or a redirect before `ready`",
            "after a delete it navigates away first",
            "call the lib function that may throw first, outside the state setter",
            "a comment hit links to its task",
            "declared once, in `lib/types.ts`",
            "returns the loading state after all of its hooks",
            "a list the user edits is a collection of the state",
        ] {
            assert!(block.contains(needle), "{needle}: {block}");
        }
        // Audit 5: the provider gates its children; a form is keyed by the
        // record id and never re-sets its drafts in an effect on the record;
        // an activity feed reads a log; empty optional values; every series.
        for needle in [
            "and the provider renders a loading state instead of its children until `ready`, so \
             no page, component or effect sees the seed data in place of the stored data",
            "With a store provider, it renders a loading state instead of its children until \
             `ready`",
            "rendered with a key of the record id (`key={task.id}`; without a store provider \
             that gates its children, `key={task.id + String(ready)}`)",
            "it never re-sets its drafts in a useEffect on the record",
            "never discards unsaved drafts",
            "the state holds an activity list, every mutation appends an entry to it, and the \
             feed reads that list",
            "an optional field may be empty ('' or undefined)",
            "never throw in render",
            "a chart draws every series that its lib function returns for display",
        ] {
            assert!(block.contains(needle), "{needle}: {block}");
        }
        // The old advice is gone: no resync on the record, no `ready` in
        // the key once the provider gates.
        for gone in [
            "re-initialises in a useEffect",
            "depends on the record itself",
            "not only on its id",
        ] {
            assert!(!block.contains(gone), "{gone}: {block}");
        }
        // The qrules head still says the stored state loads in an effect.
        assert!(NEXTJS
            .staging
            .quality
            .head_rule
            .contains("Stored data loads in a useEffect"));
        let mut bare = stated_plan();
        bare.files
            .retain(|file| file.path != "components/StoreProvider.tsx");
        let block = conventions(&bare, &NEXTJS);
        assert!(
            block.contains("keeps a `ready` flag (true once it loaded)"),
            "{block}"
        );
        assert!(NEXTJS
            .staging
            .quality
            .semantics_rule
            .contains("The hook also returns `ready`"));
        assert!(NEXTJS
            .staging
            .quality
            .semantics_rule
            .contains("renders a loading state instead of its children until `ready`"));
        // The bare plan keeps its own `ready` per page and says no gate.
        let bare_block = conventions(&bare, &NEXTJS);
        assert!(!bare_block.contains("and the provider renders a loading state"));
        // Its Loading bullet states the gate only as the case with a provider
        // and keeps the per-page wait for the plan without one.
        assert!(!bare_block.contains("so the store provider renders"));
        assert!(bare_block.contains(
            "Without one, a page that waits for its own `ready` returns the loading state after all \
             of its hooks"
        ));
    }

    /// Audits 6-8: schedule, reversible-state and money-display lines come
    /// only with the fields that need them.
    #[test]
    fn schedules_reversible_states_and_money_display_only_when_the_types_have_them() {
        let plain = conventions(&stated_plan(), &NEXTJS);
        assert!(!plain.contains("- Schedules"), "{plain}");
        assert!(!plain.contains("- Reversible states"), "{plain}");
        assert!(
            plain.contains("- Money display: an amount in integer cents stays major unit x 100")
        );
        assert!(plain.contains("0 for JPY and KRW"));
        // One money model: forms and the helper both scale by 100, and only
        // format calls are kept from naming a currency.
        assert!(plain.contains("a form takes major units and stores `Math.round(value * 100)`"));
        assert!(plain.contains("the one format helper divides by 100"));
        assert!(plain.contains("No format call or export passes a literal currency code"));
        assert!(!plain.contains("10 **"));
        let mut plan = stated_plan();
        plan.files[0].exports.push(
            "export interface Recurring { id: string; amount: number; frequency: 'monthly' | 'weekly'; dayOfMonth: number; nextDue: string }"
                .to_string(),
        );
        plan.files[0].exports.push(
            "export interface Project { id: string; name: string; archived: boolean }".to_string(),
        );
        plan.files[0]
            .exports
            .push("export type AccountStatus = 'open' | 'closed'".to_string());
        let block = conventions(&plan, &NEXTJS);
        assert!(
            block.contains(
                "- Schedules (`Recurring.frequency`, `Recurring.dayOfMonth`, `Recurring.nextDue`): \
                 the stored next date (`Recurring.nextDue`) is the one source"
            ),
            "{block}"
        );
        for needle in [
            "it is due when that date <= today",
            "records it exactly once (one transaction, or one completion entry for a record without money), dated that occurrence",
            "(or recomputes it from its anchor day), never from today or the date it was done",
            "clamped to the month end",
            "The seed sets the next date consistent with its anchor day.",
        ] {
            assert!(block.contains(needle), "{needle}: {block}");
        }
        assert!(
            block.contains("- Reversible states (`Project.archived`: the lib module"),
            "{block}"
        );
        assert!(block.contains("exports the reverse (unarchive, reopen)"));
        // A status member is reversed by its status function, not a new export.
        assert!(
            block.contains(
                "`AccountStatus` 'closed' -> 'open': the status function allows the reverse transition"
            ),
            "{block}"
        );
        assert!(
            !block.contains("`AccountStatus`: the lib module"),
            "{block}"
        );
        // No money field: no money display line.
        let mut bare = stated_plan();
        bare.files[0].exports.remove(2);
        assert!(!conventions(&bare, &NEXTJS).contains("Money display"));
        assert!(is_one_way_flag("isArchived") && is_one_way_flag("closedAt"));
        assert!(!is_one_way_flag("status") && !is_one_way_flag("createdAt"));
        assert!(is_next_date("nextDueDate") && !is_next_date("dueDate"));
        assert!(is_cadence("frequency") && !is_next_date("frequency"));
    }

    /// A frequency without a next date (a habit, a digest setting) is no
    /// schedule; a schedule names its own next-date field and, without money
    /// fields, no transaction.
    #[test]
    fn a_schedule_needs_a_next_date_field_and_names_it() {
        let mut habits = stated_plan();
        habits.files[0].exports.push(
            "export interface Habit { id: string; name: string; frequency: 'daily' | 'weekly' }"
                .to_string(),
        );
        assert!(!conventions(&habits, &NEXTJS).contains("- Schedules"));

        let mut chores = stated_plan();
        chores.files[0].exports.remove(2);
        chores.files[0].exports.push(
            "export interface Chore { id: string; frequency: 'daily' | 'weekly'; nextRunAt: string }"
                .to_string(),
        );
        let block = conventions(&chores, &NEXTJS);
        assert!(
            block.contains(
                "- Schedules (`Chore.frequency`, `Chore.nextRunAt`): the stored next date (`Chore.nextRunAt`)"
            ),
            "{block}"
        );
        assert!(!block.contains("`nextDue`"), "{block}");
        assert!(!block.contains("transaction"), "{block}");
        assert!(!block.contains("anchor day"), "{block}");
        assert!(!block.contains("calendar date)"), "{block}");
    }

    #[test]
    fn an_open_plan_gets_the_default_conventions() {
        let block = conventions(&open_plan(), &NEXTJS);
        assert!(block.contains("- Money: `Transaction.amount`: integer cents, always > 0 (the record's kind or category says expense or income)."), "{block}");
        assert!(block.contains("- Dates: `Task.dueDate`, `Transaction.date`: calendar date 'YYYY-MM-DD'; `Task.createdAt`, `Task.completedAt`: timestamp, full ISO."), "{block}");
        // No owner in the comment: the plan's one setter.
        assert!(block.contains("- Status `Status` ('backlog' | 'in_progress' | 'done') changes only through `moveTask` (`lib/board.ts`), on every page that changes it; that function also sets and clears `completedAt`."), "{block}");
        assert!(
            block.contains("- Shell: `app/layout.tsx` renders <Nav/>"),
            "{block}"
        );
    }

    #[test]
    fn a_status_with_an_owner_but_no_stamps_and_no_setter_at_all() {
        let mut plan = stated_plan();
        plan.files[0].exports[0] =
            "export type Status = 'todo' | 'done'  // via moveTask".to_string();
        let problems = semantic_problems(&plan, &NEXTJS);
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(problems[0]
            .text
            .contains("names `moveTask` (lib/board.ts) but not the timestamps"));
        // No owner named and no setter planned: the tasks module adds it.
        let plan = manifest(vec![
            file("lib/types.ts", "Types.", &[
                "export type TaskStatus = 'todo' | 'done'",
                "export interface Task { id: string; status: TaskStatus; completedAt: string | null }  // completedAt: timestamp, full ISO",
            ]),
            file("lib/tasks.ts", "Tasks.", &["export function addTask(tasks: Task[], title: string): Task[]"]),
        ]);
        let problems = semantic_problems(&plan, &NEXTJS);
        assert!(problems.iter().any(|problem| problem.path.as_deref() == Some("lib/tasks.ts")
            && problem.text.contains("add `setTaskStatus(items: Task[], id: string, status: TaskStatus): Task[]`")), "{problems:?}");
    }

    #[test]
    fn no_store_means_no_owner_problem_and_other_profiles_get_nothing() {
        let mut plan = open_plan();
        plan.files
            .retain(|file| file.path != "components/StoreProvider.tsx");
        let problems = semantic_problems(&plan, &NEXTJS);
        assert!(!problems
            .iter()
            .any(|problem| problem.text.contains("beside the store provider")));
        assert!(conventions(&plan, &NEXTJS).contains("the plan has no store provider"));
        for profile in [&PYTHON, &RUST] {
            assert_eq!(semantic_problems(&open_plan(), profile), Vec::new());
            assert_eq!(conventions(&open_plan(), profile), "");
        }
    }

    /// The audit-2 run 052814 shapes: the layout names `AppShell` in prose
    /// and a page renders it again; another plan where only the pages render
    /// the shell asks the layout for it; a sign stated in the type's own
    /// comment without a key (`amountCents signed`).
    #[test]
    fn shells_and_signs_stated_in_prose() {
        let plan = manifest(vec![
            file("lib/types.ts", "Types.", &["export interface Transaction { id: string; amountCents: number }  // amountCents signed; splits [] when unsplit"]),
            file("components/AppShell.tsx", "Shell.", &["export default function AppShell({ children })"]),
            file("app/layout.tsx", "Root layout rendering AppShell and global styles.", &["export default function RootLayout({ children })"]),
            file("app/page.tsx", "Overview.", &["export default function OverviewPage()  // renders <AppShell> with net worth"]),
        ]);
        let problems = semantic_problems(&plan, &NEXTJS);
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert_eq!(problems[0].path.as_deref(), Some("app/page.tsx"));
        assert!(problems[0].text.contains("already renders <AppShell/>"));
        let plan = manifest(vec![
            file(
                "lib/types.ts",
                "Types.",
                &["export interface Note { id: string }"],
            ),
            file(
                "components/AppShell.tsx",
                "Shell.",
                &["export default function AppShell({ children })"],
            ),
            file(
                "app/layout.tsx",
                "Root layout.",
                &["export default function RootLayout({ children })  // html and body"],
            ),
            file(
                "app/page.tsx",
                "Home.",
                &["export default function Home()  // renders <AppShell>"],
            ),
            file(
                "app/notes/page.tsx",
                "Notes.",
                &["export default function Notes()  // renders <AppShell>"],
            ),
        ]);
        let problems = semantic_problems(&plan, &NEXTJS);
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert_eq!(problems[0].path.as_deref(), Some("app/layout.tsx"));
        assert!(problems[0]
            .text
            .starts_with("2 pages render <AppShell/> on their own"));
    }

    #[test]
    fn words_and_marks() {
        assert!(is_money("openingBalance", "number"));
        assert!(is_money("amountCents", "number"));
        assert!(!is_money("amount", "string"));
        assert!(!is_money("count", "number"));
        assert!(is_timestamp("completedAt") && !is_timestamp("At") && !is_timestamp("chat"));
        assert!(is_date("dueDate", "string | null") && is_date("date", "string"));
        assert!(!is_date("lastPaid", "string"));
        // `assignee` holds no `sign` word.
        assert!(!states_sign("assigneeId: member id"));
        assert!(states_sign("amount: cents, always > 0"));
        assert!(states_format("createdAt: ISO string"));
        assert!(!states_format("createdAt: when it was made"));
        let task = vec!["Task".to_string()];
        assert!(is_status_setter("setTaskStatus", &task) && is_status_setter("moveTask", &task));
        assert!(!is_status_setter("updateTask", &task));
        // `move` alone is not a status change (`moveColumn`, `moveProjectUp`).
        assert!(!is_status_setter("moveColumn", &task));
        assert!(!is_status_setter("moveProjectUp", &task));
        assert!(renders("renders <Nav/> and more", "Nav"));
        assert!(!renders("renders <NavLink/>", "Nav"));
    }

    /// Run 20261114-183809 shape: a theme provider before the store provider
    /// in the manifest, and a `STORAGE_KEY` line in the types (065909: a
    /// layout's theme script that mentions `localStorage`).
    #[test]
    fn the_store_owner_is_not_the_theme_provider_or_the_types() {
        let plan = manifest(vec![
            file("app/layout.tsx", "Root layout.", &["export default function RootLayout({ children })  // renders <ThemeProvider><DataProvider>; inline theme script reads localStorage"]),
            file("components/ThemeProvider.tsx", "Theme context.", &["export function ThemeProvider({ children })  // keeps the theme in localStorage", "export function useTheme(): { theme: Theme; toggle(): void }"]),
            file("lib/types.ts", "Types.", &["export const STORAGE_KEY = 'app.v1'  // the localStorage key", "export interface AppState { notes: Note[] }"]),
            file("components/DataProvider.tsx", "App data.", &["export function DataProvider({ children })  // loadState on mount, saveState on change", "export function useData(): { state: AppState }"]),
            file("lib/storage.ts", "Storage.", &["export function loadState(): AppState", "export function saveState(state: AppState): void"]),
            file("app/page.tsx", "Home.", &["export default function Home()  // uses useData"]),
        ]);
        assert_eq!(
            store_provider(&plan, &NEXTJS),
            Some(("components/DataProvider.tsx", "useData".to_string()))
        );
        assert_eq!(
            storage_module(&plan, &NEXTJS, "components/DataProvider.tsx").map(|(path, _)| path),
            Some("lib/storage.ts")
        );
        let problems = semantic_problems(&plan, &NEXTJS);
        assert!(
            !problems
                .iter()
                .any(|problem| problem.text.contains("beside the store provider")),
            "{problems:?}"
        );
        let block = conventions(&plan, &NEXTJS);
        assert!(block.contains("- State: `components/DataProvider.tsx` is the only file that loads and saves the stored state (through `lib/storage.ts`)"), "{block}");
        assert!(
            block.contains("through `useData`; to reset the data a page uses `useData` as well"),
            "{block}"
        );
        // A theme provider alone is no store.
        let mut alone = plan.clone();
        alone
            .files
            .retain(|file| file.path != "components/DataProvider.tsx");
        assert_eq!(store_provider(&alone, &NEXTJS), None);
    }

    /// Two status unions: each gets the setter of its own record, and a
    /// `moveColumn` helper is nobody's.
    #[test]
    fn each_status_union_gets_its_own_setter() {
        let plan = manifest(vec![
            file("lib/types.ts", "Types.", &[
                "export type TaskStatus = 'todo' | 'done'",
                "export type ProjectStatus = 'active' | 'completed'",
                "export interface Task { id: string; status: TaskStatus; completedAt: string | null }  // completedAt: timestamp, full ISO",
                "export interface Project { id: string; status: ProjectStatus }",
            ]),
            file("lib/board.ts", "Board.", &["export function moveColumn(columns: string[], from: number, to: number): string[]"]),
            file("lib/projects.ts", "Projects.", &["export function setProjectStatus(projects: Project[], id: string, status: ProjectStatus): Project[]"]),
            file("lib/tasks.ts", "Tasks.", &["export function moveTask(tasks: Task[], id: string, status: TaskStatus): Task[]"]),
        ]);
        let block = conventions(&plan, &NEXTJS);
        assert!(block.contains("- Status `TaskStatus` ('todo' | 'done') changes only through `moveTask` (`lib/tasks.ts`)"), "{block}");
        assert!(block.contains("- Status `ProjectStatus` ('active' | 'completed') changes only through `setProjectStatus` (`lib/projects.ts`)"), "{block}");
        assert!(!block.contains("moveColumn"), "{block}");
    }

    /// Audit-2 050156 shape: the records are only named in the types (their
    /// fields come later), so the union has no record yet; the setter that
    /// takes the union in its signature is its owner.
    #[test]
    fn a_union_without_records_gets_the_setter_that_takes_it() {
        let plan = manifest(vec![
            file("lib/types.ts", "Types.", &[
                "export type Status = 'backlog' | 'review' | 'done'  // column order matches STATUSES",
                "export type Task  // declared here for the signatures that name it",
            ]),
            file("lib/tasks.ts", "Tasks.", &["export function createTask(tasks: Task[], input: { title: string; status: Status }): Task[]"]),
            file("lib/board.ts", "Board.", &["export function moveTask(tasks: Task[], id: string, status: Status): Task[]", "export function groupByStatus(tasks: Task[]): Record<Status, Task[]>"]),
        ]);
        let problems = semantic_problems(&plan, &NEXTJS);
        assert!(
            problems.iter().any(|problem| problem
                .text
                .contains("(the plan has `moveTask` in `lib/board.ts`)")),
            "{problems:?}"
        );
    }

    /// Audit-2 053134 shape: `Transaction.amountCents` stated signed, its
    /// splits open; a balance, a limit and a target open.
    #[test]
    fn money_defaults_follow_the_kind_of_field_and_the_parent_record() {
        let plan = manifest(vec![file("lib/types.ts", "Types.", &[
            "export interface Split { categoryId: string; amountCents: number }",
            "export interface Transaction { id: string; amountCents: number; splits: Split[] }  // amountCents signed: expenses negative",
            "export interface Account { id: string; openingCents: number }",
            "export interface Budget { id: string; limitCents: number }",
            "export interface Goal { id: string; targetCents: number }",
        ])]);
        let block = conventions(&plan, &NEXTJS);
        assert!(block.contains("`Split.amountCents`: the unit and sign of `Transaction.amountCents` (amountCents signed: expenses negative), the parts summing to it"), "{block}");
        assert!(
            block.contains(
                "`Account.openingCents`: integer cents, signed (below 0 is owed; 0 and debts are valid)"
            ),
            "{block}"
        );
        assert!(
            block.contains("`Budget.limitCents`, `Goal.targetCents`: integer cents, always >= 0"),
            "{block}"
        );
        assert!(!block.contains("kind or category"), "{block}");
        let problems = semantic_problems(&plan, &NEXTJS);
        let money = problems
            .iter()
            .find(|problem| problem.text.contains("hold money"))
            .expect("open money fields");
        // The split follows its stated parent: not asked again.
        assert!(!money.text.contains("Split.amountCents"), "{}", money.text);
        assert!(
            money.text.starts_with(
                "`Account.openingCents`, `Budget.limitCents`, `Goal.targetCents` hold money"
            ),
            "{}",
            money.text
        );
    }

    /// Audit 9 (Ledgerly h2): the plan said `openingBalance: always > 0`, so
    /// the validator and the form rejected empty accounts and card debt; a
    /// `Budget.rollover` that no form offered stayed `false`.
    #[test]
    fn a_positive_balance_becomes_signed_and_record_flags_get_a_form() {
        let plan = manifest(vec![file("lib/types.ts", "Types.", &[
            "export interface Account { id: string; name: string; openingBalance: number; closed: boolean }  // openingBalance: integer cents, always > 0, kind says asset or debt",
            "export interface Transaction { id: string; amount: number; kind: 'expense' | 'income' }  // amount: integer cents, always > 0, kind says expense or income",
            "export interface Budget { id: string; limit: number; rollover: boolean }  // limit: integer cents, always > 0",
            "export interface Task { id: string; done: boolean; isArchived: boolean }",
            "export interface Settings { compact: boolean }",
        ])]);
        let block = conventions(&plan, &NEXTJS);
        assert!(block.contains(
            "`Account.openingBalance`: integer cents, signed (below 0 is owed; 0 and debts are valid): \
             the value alone gives the direction (a kind only labels it, never flips the sign) and \
             the field's comment in the types says signed, in place of the plan's `openingBalance: integer cents, always > 0, kind says asset or debt`"
        ), "{block}");
        // An amount that moves money and a limit keep their stated sign.
        assert!(
            block.contains("`Transaction.amount`: amount: integer cents, always > 0"),
            "{block}"
        );
        assert!(
            block.contains("- Record flags (`Budget.rollover`): the form that creates or edits")
                && block
                    .contains("or as an optional last parameter (`rollover = false`), so calls"),
            "{block}"
        );
        // Closed, done, archived and settings flags are not options here.
        for absent in [
            "`Account.closed`",
            "`Task.done`",
            "`Task.isArchived`",
            "`Settings.compact`",
        ] {
            assert!(
                !block
                    .lines()
                    .any(|line| line.starts_with("- Record flags") && line.contains(absent)),
                "{absent}"
            );
        }
        assert_eq!(
            record_option_flags(&plan, &NEXTJS),
            vec![("Budget".to_string(), "rollover".to_string())]
        );
        // A signed balance stays as the plan says it; no flags, no line.
        let plain = manifest(vec![file("lib/types.ts", "Types.", &[
            "export interface Account { id: string; balance: number }  // balance: integer cents, signed, below 0 is owed",
        ])]);
        let block = conventions(&plain, &NEXTJS);
        assert!(
            block.contains("`Account.balance`: balance: integer cents, signed, below 0 is owed."),
            "{block}"
        );
        assert!(!block.contains("in place of the plan's"), "{block}");
        assert!(!block.contains("Record flags"), "{block}");
        assert!(states_positive_only("always > 0"));
        assert!(!states_positive_only("signed, > 0 is credit"));
        assert!(!states_positive_only("always >= 0"));
    }

    /// Audit 9: the seed was kept in memory until the first edit, so its
    /// generated ids changed on every reload and a refreshed detail page
    /// showed 'not found'.
    #[test]
    fn the_first_load_saves_the_seed() {
        let block = conventions(&stated_plan(), &NEXTJS);
        assert!(
            block.contains(
                "When nothing is stored, that effect saves the seed before it sets `ready`"
            ),
            "{block}"
        );
        let bare = manifest(vec![file(
            "lib/types.ts",
            "Types.",
            &["export interface Note { id: string }"],
        )]);
        let block = conventions(&bare, &NEXTJS);
        assert!(
            block.contains("(the seed at once when nothing is stored)"),
            "{block}"
        );
    }

    #[test]
    fn problem_texts_have_no_runs_of_spaces() {
        let mut plan = open_plan();
        plan.files.push(file(
            "app/notes/page.tsx",
            "Notes.",
            &["export default function Notes()  // renders <AppShell>"],
        ));
        plan.files.push(file(
            "app/page.tsx",
            "Home.",
            &["export default function Home()  // renders <AppShell>"],
        ));
        plan.files.push(file(
            "components/AppShell.tsx",
            "Shell.",
            &["export default function AppShell({ children })"],
        ));
        let problems = semantic_problems(&plan, &NEXTJS);
        assert!(
            problems.iter().any(|problem| problem
                .text
                .contains("pages render <AppShell/> on their own")),
            "{problems:?}"
        );
        for problem in &problems {
            assert!(!problem.text.contains("  "), "{}", problem.text);
        }
        assert!(!conventions(&plan, &NEXTJS).contains("  "));
    }
}
