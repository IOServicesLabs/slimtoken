//! The NEXTJS guard hooks: how a TypeScript source exports and imports
//! names, which checker lines name a missing export (TS2305, TS2724,
//! TS2614) or a knock-on problem (TS7006), and the text fixes of the
//! "root first" guard (export a declaration, take a name from the file that
//! declares it). Text reading only, like the rest of the NEXTJS hooks: a
//! form it does not know is skipped, never rewritten.

use super::repair::{Export, GuardHooks};

/// The guard hooks of [`NEXTJS_REPAIR`](super::NEXTJS_REPAIR).
pub const NEXTJS_GUARD: GuardHooks = GuardHooks {
    exports: ts_exports,
    imported_names: ts_imported_names,
    missing_export: ts_missing_export,
    knock_on: ts_knock_on,
    declares: ts_declares,
    add_export: ts_add_export,
    retarget: ts_retarget,
    shared_module: ts_shared_module,
};

/// A shared module: under `lib/`, `types/`, `utils/` or `shared/` (also
/// below `src/`), or a file named `types`.
pub fn ts_shared_module(path: &str) -> bool {
    let path = path.strip_prefix("src/").unwrap_or(path);
    let stem = path
        .rsplit('/')
        .next()
        .and_then(|name| name.split('.').next())
        .unwrap_or_default();
    ["lib/", "types/", "utils/", "shared/"]
        .iter()
        .any(|dir| path.starts_with(dir))
        || stem == "types"
}

/// Declaration keywords after `export ` (and after `export default `).
const KEYWORDS: [&str; 9] = [
    "async function ",
    "function ",
    "const ",
    "let ",
    "var ",
    "class ",
    "interface ",
    "type ",
    "enum ",
];

fn is_word(ch: char) -> bool {
    ch.is_alphanumeric() || ch == '_' || ch == '$'
}

/// The identifier at the start of `text`.
fn ident(text: &str) -> &str {
    let end = text.find(|ch: char| !is_word(ch)).unwrap_or(text.len());
    &text[..end]
}

fn collapse(text: &str) -> String {
    let joined = text.split_whitespace().collect::<Vec<_>>().join(" ");
    joined.trim_end_matches(';').trim_end().to_string()
}

/// The declaration that starts at line `start` of `lines`: for a type
/// (`interface`, `type`, `enum`) the whole balanced body, else the header up
/// to the body (`{` of a function or class, `=>` of an arrow, `=` of a
/// value) at bracket depth 0.
fn declaration(lines: &[&str], start: usize, keyword: &str) -> String {
    let whole = matches!(keyword, "interface " | "type " | "enum ");
    let mut text = String::new();
    let mut depth: i32 = 0;
    let mut seen_open = false;
    let value = matches!(keyword, "const " | "let " | "var ");
    for (offset, line) in lines.iter().skip(start).take(80).enumerate() {
        // A blank line or a new top-level statement ends a declaration
        // that has no body of its own (`type A =` and its union lines).
        let continues = line.starts_with(|ch: char| ch.is_whitespace() || "|&})>".contains(ch));
        if offset > 0 && depth <= 0 && (line.trim().is_empty() || !continues) {
            break;
        }
        let mut cut = None;
        let chars: Vec<(usize, char)> = line.char_indices().collect();
        for (pos, (index, ch)) in chars.iter().enumerate() {
            match ch {
                '(' | '[' | '<' => depth += 1,
                ')' | ']' | '>' => {
                    // `=>` is not a closing bracket.
                    let arrow = *ch == '>' && pos > 0 && chars[pos - 1].1 == '=';
                    if !arrow {
                        depth -= 1;
                    } else if value && depth == 0 {
                        cut = Some(*index + 1);
                        break;
                    }
                }
                '{' => {
                    if !whole && depth == 0 {
                        cut = Some(*index);
                        break;
                    }
                    depth += 1;
                    seen_open = true;
                }
                '}' => depth -= 1,
                '=' if !whole && depth == 0 => {
                    let next = chars.get(pos + 1).map(|(_, next)| *next);
                    let arrow = next == Some('>');
                    // `= (a: A): B =>`: the header of an arrow function
                    // goes on to its `=>`.
                    let rest = line[*index + 1..].trim_start();
                    let function = value && (rest.starts_with('(') || rest.starts_with("async"));
                    if !arrow && !function && keyword != "function " && keyword != "async function "
                    {
                        cut = Some(*index);
                        break;
                    }
                }
                ';' if depth == 0 => {
                    cut = Some(*index);
                    break;
                }
                _ => {}
            }
        }
        match cut {
            Some(cut) => {
                text.push_str(&line[..cut]);
                return collapse(&text);
            }
            None => {
                text.push_str(line);
                text.push(' ');
            }
        }
        if whole && seen_open && depth <= 0 {
            return collapse(&text);
        }
    }
    collapse(&text)
}

/// The names of an `export { a, b as c }` line: the exported names.
fn list_names(inner: &str) -> Vec<String> {
    inner
        .split(',')
        .map(|item| item.trim().trim_start_matches("type ").trim())
        .filter(|item| !item.is_empty())
        .map(|item| {
            let name = item.rsplit(" as ").next().unwrap_or(item).trim();
            name.to_string()
        })
        .collect()
}

/// The exported declarations of a TypeScript source.
pub fn ts_exports(source: &str) -> Vec<Export> {
    let lines: Vec<&str> = source.lines().collect();
    let mut found: Vec<Export> = Vec::new();
    let push = |name: &str, decl: String, found: &mut Vec<Export>| {
        if !name.is_empty() && !found.iter().any(|known| known.name == name) {
            found.push(Export {
                name: name.to_string(),
                decl,
            });
        }
    };
    for (index, line) in lines.iter().enumerate() {
        // Top level only: an export is never indented.
        let Some(rest) = line.strip_prefix("export ") else {
            continue;
        };
        let rest = rest.trim_start();
        if let Some(after) = rest.strip_prefix("default ") {
            let after = after.trim_start();
            let keyword = KEYWORDS
                .iter()
                .find(|keyword| after.starts_with(**keyword))
                .copied()
                .unwrap_or("");
            push("default", declaration(&lines, index, keyword), &mut found);
            continue;
        }
        let rest = rest.strip_prefix("declare ").unwrap_or(rest);
        if let Some(inner) = rest.strip_prefix('{').or_else(|| {
            rest.strip_prefix("type ")
                .and_then(|rest| rest.trim_start().strip_prefix('{'))
        }) {
            let inner = inner.split('}').next().unwrap_or_default();
            for name in list_names(inner) {
                let decl = format!("export {{ {name} }}");
                push(&name, decl, &mut found);
            }
            continue;
        }
        if rest.starts_with('*') {
            continue;
        }
        let Some(keyword) = KEYWORDS.iter().find(|keyword| rest.starts_with(**keyword)) else {
            continue;
        };
        let name = ident(rest[keyword.len()..].trim_start());
        push(name, declaration(&lines, index, keyword), &mut found);
    }
    found
}

/// The import statements of `source` with the specifier `spec`: each is the
/// clause between `import` and `from`.
fn import_clauses(source: &str, spec: &str) -> Vec<String> {
    let mut clauses = Vec::new();
    for quote in ['\'', '"'] {
        let wanted = format!("from {quote}{spec}{quote}");
        let mut searched = 0;
        while let Some(found) = source[searched..].find(&wanted) {
            let at = searched + found;
            searched = at + wanted.len();
            let Some(start) = source[..at].rfind("import ") else {
                continue;
            };
            let clause = source[start + "import ".len()..at].trim();
            // An `import` that ends before this `from` is another statement.
            if clause.contains(';') || clause.contains(" from ") {
                continue;
            }
            clauses.push(clause.to_string());
        }
    }
    clauses
}

/// The names that `source` imports from `spec`: (exported, local).
pub fn ts_imported_names(source: &str, spec: &str) -> Vec<(String, String)> {
    let mut names: Vec<(String, String)> = Vec::new();
    for clause in import_clauses(source, spec) {
        let clause = clause.strip_prefix("type ").unwrap_or(&clause).trim();
        let (default, rest) = match clause.find('{') {
            Some(open) => (clause[..open].trim().trim_end_matches(','), &clause[open..]),
            None => (clause, ""),
        };
        let default = default.trim();
        if !default.is_empty() && !default.starts_with('*') {
            let local = ident(default);
            if !local.is_empty() {
                names.push(("default".to_string(), local.to_string()));
            }
        }
        if let Some(inner) = rest.strip_prefix('{') {
            let inner = inner.split('}').next().unwrap_or_default();
            for item in inner.split(',') {
                let item = item.trim().trim_start_matches("type ").trim();
                if item.is_empty() {
                    continue;
                }
                let (exported, local) = match item.split_once(" as ") {
                    Some((exported, local)) => (exported.trim(), local.trim()),
                    None => (item, item),
                };
                if !exported.is_empty() && !local.is_empty() {
                    names.push((exported.to_string(), local.to_string()));
                }
            }
        }
    }
    names.dedup();
    names
}

/// The specifier and the name of a TS2305 / TS2724 / TS2614 line (`Module
/// '"./types"' has no exported member 'AppState'`, `'"./types"' has no
/// exported member named 'Colum'. Did you mean ...`), and of a TS2459 line
/// (`Module '"./types"' declares 'AppState' locally, but it is not
/// exported`).
pub fn ts_missing_export(line: &str) -> Option<(String, String)> {
    let code = ["TS2305", "TS2724", "TS2614", "TS2459"]
        .iter()
        .find(|code| line.contains(**code))?;
    let message = &line[line.find(code)? + code.len()..];
    let spec_start = message.find("'\"")? + 2;
    let spec_len = message[spec_start..].find("\"'")?;
    let spec = &message[spec_start..spec_start + spec_len];
    let after = &message[spec_start + spec_len..];
    let marker = if *code == "TS2459" {
        "declares"
    } else {
        "exported member"
    };
    let after = &after[after.find(marker)?..];
    let name_start = after.find('\'')? + 1;
    let name = ident(&after[name_start..]);
    (!name.is_empty() && !spec.is_empty()).then(|| (spec.to_string(), name.to_string()))
}

/// TS7006: a parameter with an implicit `any`, most often because the type
/// that should give it one is broken in an imported file.
pub fn ts_knock_on(line: &str) -> bool {
    line.contains("TS7006")
}

/// The line index and keyword of the top-level declaration of `name`.
fn declaration_line(source: &str, name: &str) -> Option<(usize, bool)> {
    for (index, line) in source.lines().enumerate() {
        let (exported, rest) = match line.strip_prefix("export ") {
            Some(rest) => (true, rest.trim_start()),
            None => (false, line),
        };
        if rest.starts_with("default ") {
            continue;
        }
        let rest = rest.strip_prefix("declare ").unwrap_or(rest);
        for keyword in KEYWORDS {
            if let Some(after) = rest.strip_prefix(keyword) {
                if ident(after.trim_start()) == name {
                    return Some((index, exported));
                }
            }
        }
    }
    None
}

/// `Some(exported)` if `source` declares `name` at its top level.
pub fn ts_declares(source: &str, name: &str) -> Option<bool> {
    if let Some((_, exported)) = declaration_line(source, name) {
        return Some(exported);
    }
    // `export { name }` of a local declaration counts as exported.
    let listed = source.lines().any(|line| {
        line.strip_prefix("export {")
            .and_then(|rest| rest.split('}').next())
            .is_some_and(|inner| list_names(inner).iter().any(|known| known == name))
    });
    listed.then_some(true)
}

/// `source` with the top-level declaration of `name` exported; `None` when
/// it has none or it is exported already.
pub fn ts_add_export(source: &str, name: &str) -> Option<String> {
    let (index, exported) = declaration_line(source, name)?;
    if exported {
        return None;
    }
    let mut lines: Vec<String> = source.lines().map(str::to_string).collect();
    lines[index] = format!("export {}", lines[index]);
    let mut text = lines.join("\n");
    if source.ends_with('\n') {
        text.push('\n');
    }
    Some(text)
}

/// The import specifier from the file `from` to the file `to`, in the style
/// of `like` (`@/` if `like` uses it, else relative).
fn specifier(from: &str, to: &str, like: &str) -> String {
    let stem = [".tsx", ".ts", ".jsx", ".js"]
        .iter()
        .find_map(|extension| to.strip_suffix(extension))
        .unwrap_or(to);
    if like.starts_with("@/") {
        let stem = stem.strip_prefix("src/").unwrap_or(stem);
        return format!("@/{stem}");
    }
    let from_dir: Vec<&str> = from.split('/').collect();
    let from_dir = &from_dir[..from_dir.len().saturating_sub(1)];
    let to_parts: Vec<&str> = stem.split('/').collect();
    let common = from_dir
        .iter()
        .zip(to_parts.iter())
        .take_while(|(left, right)| left == right)
        .count();
    let mut parts: Vec<&str> = vec![".."; from_dir.len() - common];
    parts.extend(&to_parts[common..]);
    let joined = parts.join("/");
    if joined.starts_with("..") {
        joined
    } else {
        format!("./{joined}")
    }
}

/// `source` (the file `importer`) with `name` imported from the file `to`
/// instead of from `spec`. Only a one-line `import { ... } from 'spec'`
/// is rewritten.
pub fn ts_retarget(
    importer: &str,
    source: &str,
    name: &str,
    spec: &str,
    to: &str,
) -> Option<String> {
    let right = specifier(importer, to, spec);
    if right == spec {
        return None;
    }
    let mut lines: Vec<String> = Vec::new();
    let mut done = false;
    for line in source.lines() {
        if done {
            lines.push(line.to_string());
            continue;
        }
        let trimmed = line.trim();
        let from_spec = [format!("'{spec}'"), format!("\"{spec}\"")]
            .iter()
            .any(|quoted| trimmed.contains(&format!("from {quoted}")));
        let Some(clause) = trimmed.strip_prefix("import ") else {
            lines.push(line.to_string());
            continue;
        };
        let Some((clause, _)) = clause.split_once(" from ").filter(|_| from_spec) else {
            lines.push(line.to_string());
            continue;
        };
        let (word, inner) = match clause.trim().strip_prefix("type ") {
            Some(rest) => ("type ", rest.trim()),
            None => ("", clause.trim()),
        };
        let Some(inner) = inner
            .strip_prefix('{')
            .and_then(|rest| rest.strip_suffix('}'))
        else {
            lines.push(line.to_string());
            continue;
        };
        let items: Vec<&str> = inner
            .split(',')
            .map(str::trim)
            .filter(|item| !item.is_empty())
            .collect();
        let item_name = |item: &str| {
            let item = item.trim_start_matches("type ").trim();
            item.split(" as ").next().unwrap_or(item).trim().to_string()
        };
        let Some(moved) = items.iter().find(|item| item_name(item) == name) else {
            lines.push(line.to_string());
            continue;
        };
        let kept: Vec<&str> = items
            .iter()
            .copied()
            .filter(|item| item_name(item) != name)
            .collect();
        let quote = if trimmed.contains(&format!("\"{spec}\"")) {
            '"'
        } else {
            '\''
        };
        if !kept.is_empty() {
            lines.push(format!(
                "import {word}{{ {} }} from {quote}{spec}{quote};",
                kept.join(", ")
            ));
        }
        lines.push(format!(
            "import {word}{{ {moved} }} from {quote}{right}{quote};"
        ));
        done = true;
    }
    if !done {
        return None;
    }
    let mut text = lines.join("\n");
    if source.ends_with('\n') {
        text.push('\n');
    }
    Some(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exports_are_read_with_their_declarations() {
        let source = "import x from './x';\n\
            export interface Task {\n  id: string;\n  done: boolean;\n}\n\
            export type Status = 'todo' | 'done';\n\
            export function moveTask(board: Board,\n  id: string): Board {\n  return board;\n}\n\
            export const useStore = (): Store => {\n  return x;\n};\n\
            export const KEY = 'k';\n\
            export default function BoardPage() {\n  return null;\n}\n\
            export { helper, other as alias };\n\
            function helper() {}\n";
        let found = ts_exports(source);
        let names: Vec<&str> = found.iter().map(|export| export.name.as_str()).collect();
        assert_eq!(
            names,
            ["Task", "Status", "moveTask", "useStore", "KEY", "default", "helper", "alias"]
        );
        assert_eq!(
            found[0].decl,
            "export interface Task { id: string; done: boolean; }"
        );
        assert_eq!(found[1].decl, "export type Status = 'todo' | 'done'");
        assert_eq!(
            found[2].decl,
            "export function moveTask(board: Board, id: string): Board"
        );
        assert_eq!(found[3].decl, "export const useStore = (): Store =>");
        assert_eq!(found[4].decl, "export const KEY");
        assert_eq!(found[5].decl, "export default function BoardPage()");
    }

    #[test]
    fn imported_names_follow_the_clause() {
        let source = "import Board, { moveTask, type Task as T } from '@/lib/board';\n\
            import type { Status } from \"./types\";\nimport * as all from './all';\n\
            import {\n  a,\n  b,\n} from './multi';\n";
        assert_eq!(
            ts_imported_names(source, "@/lib/board"),
            [
                ("default".to_string(), "Board".to_string()),
                ("moveTask".to_string(), "moveTask".to_string()),
                ("Task".to_string(), "T".to_string()),
            ]
        );
        assert_eq!(
            ts_imported_names(source, "./types"),
            [("Status".to_string(), "Status".to_string())]
        );
        assert!(ts_imported_names(source, "./all").is_empty());
        assert_eq!(ts_imported_names(source, "./multi").len(), 2);
    }

    #[test]
    fn missing_export_lines_give_the_specifier_and_the_name() {
        let line = "ERROR lib/seed.ts(1,15): error TS2305: Module '\"./types\"' has no exported \
                    member 'AppState'.";
        assert_eq!(
            ts_missing_export(line),
            Some(("./types".to_string(), "AppState".to_string()))
        );
        let line = "ERROR a.tsx(2,10): error TS2724: '\"@/lib/types\"' has no exported member \
                    named 'Colum'. Did you mean 'Column'?";
        assert_eq!(
            ts_missing_export(line),
            Some(("@/lib/types".to_string(), "Colum".to_string()))
        );
        let line = "ERROR app/page.tsx(3,10): error TS2459: Module '\"../lib/types\"' declares 'AppState' locally, but it is not exported.";
        assert_eq!(
            ts_missing_export(line),
            Some(("../lib/types".to_string(), "AppState".to_string()))
        );
        assert_eq!(ts_missing_export("ERROR a.ts(1,1): error TS2307: x"), None);
        assert!(ts_knock_on(
            "error TS7006: Parameter 't' implicitly has an 'any' type."
        ));
    }

    #[test]
    fn root_fixes_export_and_retarget() {
        let source = "interface AppState {\n  a: number;\n}\nexport const x = 1;\n";
        assert_eq!(ts_declares(source, "AppState"), Some(false));
        assert_eq!(ts_declares(source, "x"), Some(true));
        assert_eq!(ts_declares(source, "y"), None);
        let fixed = ts_add_export(source, "AppState").unwrap();
        assert!(fixed.starts_with("export interface AppState {"));
        assert_eq!(ts_add_export(&fixed, "AppState"), None);

        let importer = "import { AppState, Task } from './types';\nconst a = 1;\n";
        let moved = ts_retarget(
            "lib/seed.ts",
            importer,
            "AppState",
            "./types",
            "lib/storage.ts",
        )
        .unwrap();
        assert_eq!(
            moved,
            "import { Task } from './types';\nimport { AppState } from './storage';\nconst a = 1;\n"
        );
        let importer = "import type { AppState } from \"@/lib/types\";\n";
        let moved = ts_retarget(
            "app/page.tsx",
            importer,
            "AppState",
            "@/lib/types",
            "lib/storage.ts",
        )
        .unwrap();
        assert_eq!(moved, "import type { AppState } from \"@/lib/storage\";\n");
        assert_eq!(
            ts_retarget("app/page.tsx", importer, "Other", "@/lib/types", "lib/x.ts"),
            None
        );
    }
}
