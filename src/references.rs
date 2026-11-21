//! Dangling references of a plan (`ArchitectOptions::references`): the
//! export fragments are planned apart, so one file's contract can call a
//! file or a function that no fragment planned (`via lib/x fn`, `toggle via
//! toggleItem`). `dangling_problems` finds them in the merged manifest
//! without a model call; the architect asks the owning fragments once, in
//! the same repair round as the contract check (`architect::split_manifest`).
//!
//! What it reads: each file's purpose and the comment part of each export
//! (after `//` or ` # `). What it flags:
//! - a path token under a planned top directory (`lib/x`, `@/lib/x.ts`)
//!   that names no planned file or directory: owned by the referring file;
//! - a planned path followed by a name (`lib/x fn`, `lib/x.fn`, `lib/x:fn`)
//!   that file's contract never names: owned by that file (adding the export
//!   makes the reference true), unless the name is a variant of an export
//!   that file already plans (`addItems` for `addItem`): then the referring
//!   file is told to call that export;
//! - a camelCase name (or a snake_case call) right after `via`, `calls`,
//!   `call`, `calling` or `using` that no export declares or describes and
//!   that shares a word with some planned export (the slip case): owned by
//!   the referring file. A name near no export (a platform, dependency or
//!   private-helper call) is left alone.
//!
//! Never flagged: `<Name/>` render mentions (the component check owns them),
//! member calls (`store.reset`, `Date.now`), event props and setters
//! (`onToggle`, `setOpen`, `handleClick`), common platform names, and paths
//! outside the planned directories (`next/navigation`).

use crate::architect::{declared_names, identifiers, Manifest, ManifestFile};
use crate::contract::ContractProblem;
use std::collections::BTreeSet;

/// The words after which a bare name is a call of another file's export.
const VERBS: &[&str] = &["via", "calls", "call", "calling", "using"];

/// Skipped between a verb and its name.
const ARTICLES: &[&str] = &["the", "a", "an"];

/// Platform and framework names a contract may call without planning them.
const PLATFORM: &[&str] = &[
    "useState",
    "useEffect",
    "useMemo",
    "useCallback",
    "useReducer",
    "useRef",
    "useContext",
    "useParams",
    "useRouter",
    "useSearchParams",
    "usePathname",
    "useId",
    "useTransition",
    "useSyncExternalStore",
    "useLayoutEffect",
    "notFound",
    "redirect",
    "localStorage",
    "sessionStorage",
    "structuredClone",
    "parseInt",
    "parseFloat",
    "toFixed",
    "toLocaleString",
    "toLocaleDateString",
    "toLocaleTimeString",
    "toISOString",
    "toLowerCase",
    "toUpperCase",
    "localeCompare",
    "getTime",
    "setTimeout",
    "setInterval",
    "clearTimeout",
    "clearInterval",
    "requestAnimationFrame",
    "addEventListener",
    "removeEventListener",
    "preventDefault",
    "stopPropagation",
    "randomUUID",
    "encodeURIComponent",
    "decodeURIComponent",
    "querySelector",
    "getElementById",
    "forEach",
    "flatMap",
    "findIndex",
    "padStart",
    "startsWith",
    "endsWith",
    "createContext",
    "createElement",
    "createPortal",
    // React 18/19 hooks and helpers.
    "useActionState",
    "useOptimistic",
    "useFormStatus",
    "useFormState",
    "useDeferredValue",
    "useImperativeHandle",
    "useInsertionEffect",
    "useDebugValue",
    "startTransition",
    "forwardRef",
    "createRoot",
    "cloneElement",
    "isValidElement",
    // Next.js server and routing APIs.
    "revalidatePath",
    "revalidateTag",
    "unstable_cache",
    "generateMetadata",
    "generateStaticParams",
    "permanentRedirect",
    "useSelectedLayoutSegment",
    "useSelectedLayoutSegments",
    // DOM, Storage, JSON and built-in methods.
    "getItem",
    "setItem",
    "removeItem",
    "toSorted",
    "toReversed",
    "toSpliced",
    "findLast",
    "findLastIndex",
    "replaceAll",
    "toString",
    "toJSON",
    "toDateString",
    "toTimeString",
    "toUTCString",
    "toPrecision",
    "valueOf",
    "hasOwnProperty",
    "scrollIntoView",
    "scrollTo",
    "getBoundingClientRect",
    "querySelectorAll",
    "getAttribute",
    "setAttribute",
    "classList",
    "dispatchEvent",
    "matchMedia",
    "fromEntries",
    "isArray",
    "isNaN",
    "isFinite",
    "isInteger",
    "lastIndexOf",
    "indexOf",
    "padEnd",
    "trimStart",
    "trimEnd",
    "getFullYear",
    "getMonth",
    "getDate",
    "getDay",
    "getHours",
    "getMinutes",
    "setDate",
    "setHours",
    "setFullYear",
    "setMonth",
    "queueMicrotask",
    "fetch",
];

/// The comment part of an export line (after `//` or ` # `), or "".
fn comment_of(line: &str) -> &str {
    let slashes = line.find("//");
    let hash = line.find(" # ");
    match (slashes, hash) {
        (Some(a), Some(b)) => &line[a.min(b)..],
        (Some(a), None) => &line[a..],
        (None, Some(b)) => &line[b..],
        (None, None) => "",
    }
}

/// The code part of an export line (before its comment).
fn code_of(line: &str) -> &str {
    &line[..line.len() - comment_of(line).len()]
}

/// The texts of `file` that can name other files: its purpose and the
/// comments of its exports.
fn reference_texts(file: &ManifestFile) -> Vec<&str> {
    std::iter::once(file.purpose.as_str())
        .chain(file.exports.iter().map(|line| comment_of(line)))
        .filter(|text| !text.is_empty())
        .collect()
}

/// `path` without the extension of its last segment.
fn stem(path: &str) -> &str {
    let last = path.rfind('/').map_or(0, |slash| slash + 1);
    match path[last..].rfind('.') {
        Some(dot) if dot > 0 => &path[..last + dot],
        _ => path,
    }
}

/// The extension of `path`'s last segment, with its dot ("" when none).
fn extension(path: &str) -> &str {
    &path[stem(path).len()..]
}

enum Resolved<'a> {
    File(&'a ManifestFile),
    Directory,
    Missing,
}

/// The planned file (or directory) that `token` names.
fn resolve<'a>(token: &str, files: &'a [ManifestFile]) -> Resolved<'a> {
    let wanted = stem(token);
    if let Some(file) = files.iter().find(|file| {
        file.path == token
            || stem(&file.path) == wanted
            || stem(&file.path).strip_suffix("/index") == Some(wanted)
    }) {
        return Resolved::File(file);
    }
    let prefix = format!("{}/", token.trim_end_matches('/'));
    if files.iter().any(|file| file.path.starts_with(&prefix)) {
        return Resolved::Directory;
    }
    Resolved::Missing
}

/// Whether `word` reads as a code name: camelCase or PascalCase (an
/// upper-case letter after the first character).
fn code_like(word: &str) -> bool {
    word.chars().skip(1).any(|ch| ch.is_ascii_uppercase())
}

/// Event props, setters and handlers a component owns itself.
fn local_name(word: &str) -> bool {
    ["on", "set", "handle"].iter().any(|prefix| {
        word.strip_prefix(prefix)
            .is_some_and(|rest| rest.starts_with(|ch: char| ch.is_ascii_uppercase()))
    })
}

/// The lower-case words of a name or path (`scaleIngredients` -> scale,
/// ingredients), three characters or more.
fn name_words(name: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut word = String::new();
    for ch in name.chars() {
        if !ch.is_ascii_alphanumeric() || (ch.is_ascii_uppercase() && !word.is_empty()) {
            if word.len() >= 3 {
                words.push(word.clone());
            }
            word.clear();
        }
        if ch.is_ascii_alphanumeric() {
            word.push(ch.to_ascii_lowercase());
        }
    }
    if word.len() >= 3 {
        words.push(word);
    }
    words
}

/// Whether two words share a stem (equal, or the same first four letters).
fn same_word(a: &str, b: &str) -> bool {
    a == b || (a.len() >= 4 && b.len() >= 4 && a[..4] == b[..4])
}

/// Up to three planned exports (`path: name`) that share words with `name`.
fn nearest(name: &str, files: &[ManifestFile]) -> Vec<String> {
    let wanted = name_words(name);
    let mut scored: Vec<(usize, String)> = Vec::new();
    for file in files {
        for line in &file.exports {
            for declared in declared_names(code_of(line)) {
                let score = name_words(&declared)
                    .iter()
                    .filter(|word| wanted.iter().any(|other| same_word(word, other)))
                    .count();
                let entry = format!("`{}: {declared}`", file.path);
                if score > 0 && !scored.iter().any(|(_, known)| *known == entry) {
                    scored.push((score, entry));
                }
            }
        }
    }
    scored.sort_by_key(|entry| std::cmp::Reverse(entry.0));
    scored.into_iter().take(3).map(|(_, entry)| entry).collect()
}

/// Whether `name` reads as a variant of `declared` (a typo, a plural or a
/// case slip): equal ignoring case, or the same number of words with each
/// pair sharing a stem (`addItems` / `addItem`, not `archiveItem` /
/// `removeItem`).
fn close_name(name: &str, declared: &str) -> bool {
    if name.eq_ignore_ascii_case(declared) {
        return true;
    }
    let (wanted, have) = (name_words(name), name_words(declared));
    !wanted.is_empty()
        && wanted.len() == have.len()
        && wanted.iter().zip(&have).all(|(a, b)| same_word(a, b))
}

/// The fix hint for a reference that names nothing planned.
fn instead(name: &str, files: &[ManifestFile]) -> String {
    let near = nearest(name, files);
    if near.is_empty() {
        "drop it or name a planned export".to_string()
    } else {
        format!("call one of {} instead, or drop it", near.join(", "))
    }
}

/// The names a contract plans: every identifier of every export's code, and
/// every identifier inside `{...}` of an export comment (props and returned
/// members described there).
fn planned_names(files: &[ManifestFile]) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    for line in files.iter().flat_map(|file| file.exports.iter()) {
        for (_, word) in identifiers(code_of(line)) {
            names.insert(word.to_string());
        }
        let comment = comment_of(line);
        let mut depth = 0usize;
        let mut from = 0usize;
        for (index, ch) in comment.char_indices() {
            match ch {
                '{' => {
                    if depth == 0 {
                        from = index;
                    }
                    depth += 1;
                }
                '}' if depth > 0 => {
                    depth -= 1;
                    if depth == 0 {
                        for (_, word) in identifiers(&comment[from..index]) {
                            names.insert(word.to_string());
                        }
                    }
                }
                _ => {}
            }
        }
    }
    names
}

/// The name that starts `rest` after spaces, backticks, quotes and one `:`
/// or `.`, when it reads as a code name (or is called); `None` otherwise.
fn following_name(rest: &str) -> Option<&str> {
    let rest = rest.trim_start_matches([' ', '`', '\'', '"']);
    let rest = rest
        .strip_prefix(':')
        .or_else(|| rest.strip_prefix('.'))
        .unwrap_or(rest)
        .trim_start_matches([' ', '`']);
    let end = rest
        .find(|ch: char| !(ch.is_ascii_alphanumeric() || ch == '_' || ch == '$'))
        .unwrap_or(rest.len());
    let name = &rest[..end];
    let called = rest[end..].starts_with('(');
    (!name.is_empty()
        && name.starts_with(|ch: char| ch.is_ascii_alphabetic())
        && (code_like(name) || called))
        .then_some(name)
}

/// Every dangling reference of `manifest` (see the module doc), with the
/// file whose fragment should fix it.
pub fn dangling_problems(manifest: &Manifest) -> Vec<ContractProblem> {
    let files = &manifest.files;
    let roots: BTreeSet<&str> = files
        .iter()
        .filter_map(|file| file.path.split_once('/').map(|(root, _)| root))
        .collect();
    let extensions: BTreeSet<&str> = files.iter().map(|file| extension(&file.path)).collect();
    let planned = planned_names(files);
    let mut problems: Vec<ContractProblem> = Vec::new();
    let mut push = |problem: ContractProblem| {
        if !problems.contains(&problem) {
            problems.push(problem);
        }
    };
    for file in files {
        for text in reference_texts(file) {
            // Path tokens.
            let mut index = 0;
            let bytes = text.as_bytes();
            while index < bytes.len() {
                let path_char = |ch: u8| {
                    ch.is_ascii_alphanumeric()
                        || matches!(ch, b'_' | b'.' | b'/' | b'@' | b'-' | b'[' | b']')
                };
                if !path_char(bytes[index]) {
                    index += 1;
                    continue;
                }
                let start = index;
                while index < bytes.len() && path_char(bytes[index]) {
                    index += 1;
                }
                let raw = &text[start..index];
                let token = raw.trim_end_matches(['.', '-', '/']);
                let token = token
                    .strip_prefix("@/")
                    .or_else(|| token.strip_prefix("./"))
                    .unwrap_or(token);
                if !token.contains('/') || token.starts_with('/') || token.contains("//") {
                    continue;
                }
                let root = token.split('/').next().unwrap_or("");
                // `<Name/>` render mentions are the component check's.
                if !roots.contains(root) || text[..start].ends_with('<') {
                    continue;
                }
                let after = &text[index..];
                // `lib/x.fn`: an extension no planned file has is a member
                // name (`public/x.png`, not a code name, is skipped).
                let ext = extension(token);
                let (path, member) = if ext.is_empty() || extensions.contains(ext) {
                    (token, None)
                } else {
                    match following_name(&ext[1..]) {
                        Some(name) => (stem(token), Some(name)),
                        None => continue,
                    }
                };
                let next = member.or_else(|| following_name(after));
                let (target, name) = match resolve(path, files) {
                    Resolved::File(target) => (target, next),
                    Resolved::Directory => continue,
                    Resolved::Missing => {
                        let hint = format!("{path} {}", next.unwrap_or(""));
                        push(ContractProblem {
                            path: Some(file.path.clone()),
                            text: format!(
                                "`{path}` is not a planned file: {}.",
                                instead(&hint, files)
                            ),
                        });
                        continue;
                    }
                };
                let Some(name) = name else { continue };
                if target.path == file.path || PLATFORM.contains(&name) {
                    continue;
                }
                let named = target
                    .exports
                    .iter()
                    .any(|line| identifiers(line).iter().any(|&(_, word)| word == name));
                if named {
                    continue;
                }
                // A variant of an export the target already plans is the
                // referring file's slip: it is told to call that export, so
                // the target does not grow a near-duplicate.
                let mut near: Vec<String> = Vec::new();
                for line in &target.exports {
                    for declared in declared_names(code_of(line)) {
                        let entry = format!("`{}: {declared}`", target.path);
                        if close_name(name, &declared) && !near.contains(&entry) {
                            near.push(entry);
                        }
                    }
                }
                if near.is_empty() {
                    push(ContractProblem {
                        path: Some(target.path.clone()),
                        text: format!(
                            "`{}` exports no `{name}` but `{}` calls it (`{token} {name}`): add \
                             `{name}` with a signature that fits that use.",
                            target.path, file.path
                        ),
                    });
                } else {
                    push(ContractProblem {
                        path: Some(file.path.clone()),
                        text: format!(
                            "`{}` exports no `{name}` (`{token} {name}`): call {} instead.",
                            target.path,
                            near.join(" or ")
                        ),
                    });
                }
            }
            // Bare names after a calling verb.
            let words = identifiers(text);
            for (at, &(offset, word)) in words.iter().enumerate() {
                if !VERBS.contains(&word.to_ascii_lowercase().as_str()) {
                    continue;
                }
                let mut next = at + 1;
                let mut end = offset + word.len();
                while let Some(&(from, article)) = words.get(next) {
                    let gap = &text[end..from];
                    if !gap.chars().all(|ch| ch.is_whitespace() || ch == '`') {
                        break;
                    }
                    if ARTICLES.contains(&article.to_ascii_lowercase().as_str()) {
                        end = from + article.len();
                        next += 1;
                        continue;
                    }
                    break;
                }
                let Some(&(from, name)) = words.get(next) else {
                    continue;
                };
                if !text[end..from]
                    .chars()
                    .all(|ch| ch.is_whitespace() || ch == '`')
                {
                    continue;
                }
                let after = &text[from + name.len()..];
                // A member (`store.reset`) or a path (`lib/x`) is not a bare name.
                if after.starts_with(['.', '/']) {
                    continue;
                }
                let called = after.starts_with('(');
                let shaped = name.starts_with(|ch: char| ch.is_ascii_lowercase())
                    && (code_like(name) || (called && name.contains('_')));
                if !shaped || local_name(name) || PLATFORM.contains(&name) || planned.contains(name)
                {
                    continue;
                }
                // Only a name that shares a word with a planned export reads
                // as a slip for it; any other name is a platform, dependency
                // or private-helper call the plan need not declare.
                let near = nearest(name, files);
                if near.is_empty() {
                    continue;
                }
                push(ContractProblem {
                    path: Some(file.path.clone()),
                    text: format!(
                        "`{name}` is not an export of any planned file (`{word} {name}`): call one \
                         of {} instead, or drop it.",
                        near.join(", ")
                    ),
                });
            }
        }
    }
    problems
}

#[cfg(test)]
mod tests {
    use super::*;

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

    fn base() -> Vec<ManifestFile> {
        vec![
            file(
                "lib/types.ts",
                "Shared types.",
                &["export interface Item { id: string; name: string; done: boolean }"],
            ),
            file(
                "lib/items.ts",
                "Item operations.",
                &[
                    "export function toggleDone(items: Item[], id: string): Item[]",
                    "export function removeItem(items: Item[], id: string): Item[]",
                ],
            ),
            file(
                "lib/store.ts",
                "State hook.",
                &["export function useStore(): Store  // returns { items, reset, addItem }"],
            ),
            file(
                "components/ItemRow.tsx",
                "One row.",
                &["export default function ItemRow({ item, onToggle })  // calls onToggle on click"],
            ),
        ]
    }

    fn page(comment: &str) -> ManifestFile {
        file(
            "app/page.tsx",
            "Home page.",
            &[&format!("export default function Page()  // {comment}")],
        )
    }

    fn problems(comment: &str) -> Vec<ContractProblem> {
        let mut files = base();
        files.push(page(comment));
        dangling_problems(&manifest(files))
    }

    #[test]
    fn an_unplanned_path_is_owned_by_the_referring_file() {
        let found = problems("scales via lib/scale.ts scaleItems(items, n)");
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].path.as_deref(), Some("app/page.tsx"));
        assert!(found[0]
            .text
            .contains("`lib/scale.ts` is not a planned file"));
        // Without the extension too.
        let found = problems("scales via lib/scale scaleItems");
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].path.as_deref(), Some("app/page.tsx"));
    }

    #[test]
    fn a_planned_export_is_no_problem_and_a_missing_one_goes_to_its_file() {
        assert_eq!(problems("removes via lib/items.ts removeItem"), Vec::new());
        assert_eq!(
            problems("removes via @/lib/items removeItem(items, id)"),
            Vec::new()
        );
        assert_eq!(problems("removes via lib/items.removeItem"), Vec::new());
        let found = problems("archives via lib/items archiveItem");
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].path.as_deref(), Some("lib/items.ts"));
        assert!(found[0].text.contains("exports no `archiveItem`"));
        assert!(found[0].text.contains("`app/page.tsx` calls it"));
    }

    #[test]
    fn a_bare_unplanned_name_after_via_is_flagged_with_near_exports() {
        let found = problems("toggle via toggleItem");
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].path.as_deref(), Some("app/page.tsx"));
        assert!(found[0].text.contains("`toggleItem` is not an export"));
        assert!(found[0].text.contains("`lib/items.ts: toggleDone`"));
    }

    #[test]
    fn props_members_renders_platform_and_outside_paths_are_not_flagged() {
        for comment in [
            "calls onToggle",
            "calls store reset",
            "via store.reset()",
            "calls reset via useStore",
            "calls the addItem action",
            "renders <ItemRow/> per item",
            "uses useRouter from next/navigation",
            "via localStorage",
            "toggle via local helper",
            "reads lib/types Item records",
            "links to app/page.tsx",
        ] {
            assert_eq!(problems(comment), Vec::new(), "{comment}");
        }
    }

    #[test]
    fn a_variant_of_a_planned_export_goes_to_the_referring_file() {
        let found = problems("removes via lib/items removeItems");
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].path.as_deref(), Some("app/page.tsx"));
        assert!(found[0].text.contains("exports no `removeItems`"));
        assert!(found[0]
            .text
            .contains("call `lib/items.ts: removeItem` instead"));
        // A different name that only shares a word still goes to the target.
        let found = problems("archives via lib/items archiveItem");
        assert_eq!(found[0].path.as_deref(), Some("lib/items.ts"));
    }

    #[test]
    fn platform_dependency_and_helper_names_near_no_export_are_not_flagged() {
        for comment in [
            "adds via useOptimistic",
            "saves using getItem",
            "joins classes via twMerge",
            "checks input via safeParse",
            "totals via sumAmounts",
        ] {
            assert_eq!(problems(comment), Vec::new(), "{comment}");
        }
    }
}
