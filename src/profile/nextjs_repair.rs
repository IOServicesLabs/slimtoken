//! NEXTJS repair hooks, moved from the `bench/showdown` repair step: how a
//! TypeScript / Next.js project imports its files, which files a joint
//! repair may change, and the Next.js rules of the repair prompts. The
//! TypeScript fixes that need no model call (import paths and forms, the
//! "use client" line) stay in the bench; `prepare` does nothing here.

use std::collections::{BTreeMap, BTreeSet};

use super::repair::{no_prepare, Problem, RepairHooks};

/// The repair hooks of [`NEXTJS`](super::NEXTJS).
pub const NEXTJS_REPAIR: RepairHooks = RepairHooks {
    classify: next_classify,
    is_test: next_is_test,
    is_shared_types: next_is_types_file,
    is_shared: next_is_shared,
    local_specifiers: next_local_specifiers,
    resolve_import: next_resolve_import,
    missing_module_path: next_missing_module_path,
    joint_editable: next_joint_files,
    joint_rules: "If the problem says that a hook must be used within a provider, the page is \
                  not at fault: change the root layout so that the provider is around \
                  `{children}`. If the layout has the provider around `{children}` already, the \
                  provider file is at fault: its context value is empty on the first render (for \
                  example while it loads from storage). Change the provider so that the context \
                  has a complete, usable value on the very first render, on the server too (the \
                  seed data or an empty state), and so that it never renders `children` outside \
                  the context; it can load the stored state after that in an effect. ",
    new_file_rule: ", and make its first line \"use client\" if it uses hooks or event handlers. \
                    A `.ts` file has no JSX: it holds types, data and functions only.",
    prepare: no_prepare,
    guard: super::nextjs_guard::NEXTJS_GUARD,
};

/// A page that does not render, or an import of a missing module.
pub fn next_classify(line: &str) -> Problem {
    if line.contains("when it renders the page") || line.contains("TS9101") {
        Problem::Render
    } else if line.contains("TS2307") || line.contains("Module not found") {
        Problem::MissingModule
    } else {
        Problem::Other
    }
}

/// `.test.`, `.spec.` or a `__tests__/` directory anywhere in the path.
pub fn next_is_test(path: &str) -> bool {
    path.contains(".test.") || path.contains(".spec.") || path.contains("__tests__/")
}

/// A file that owns shared types (`lib/types.ts`, `types/...`).
pub fn next_is_types_file(path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path);
    name == "types.ts" || path.contains("/types/")
}

/// The shared types and the non-test `lib/` modules.
fn next_is_shared(path: &str) -> bool {
    (next_is_types_file(path) || path.starts_with("lib/")) && !next_is_test(path)
}

/// Join an import specifier to the directory of the importing file.
fn join(dir: &str, spec: &str) -> String {
    let mut parts: Vec<&str> = dir.split('/').filter(|part| !part.is_empty()).collect();
    for part in spec.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                if parts.pop().is_none() {
                    // Above the project root: no project file is there.
                    return format!("../{spec}");
                }
            }
            other => parts.push(other),
        }
    }
    parts.join("/")
}

/// The import specifiers in `source` that point into the project (`./`,
/// `../`, `@/`), each once, in order.
pub fn next_local_specifiers(source: &str) -> Vec<String> {
    let mut found = Vec::new();
    for keyword in ["from", "import"] {
        let mut rest = source;
        while let Some(index) = rest.find(keyword) {
            rest = &rest[index + keyword.len()..];
            let after = rest.trim_start_matches([' ', '(']);
            let Some(quote) = after.chars().next().filter(|ch| matches!(ch, '\'' | '"')) else {
                continue;
            };
            let body = &after[1..];
            let Some(end) = body.find(quote) else {
                continue;
            };
            let spec = &body[..end];
            let local = spec.starts_with("./") || spec.starts_with("../") || spec.starts_with("@/");
            if local && !spec.contains('\n') && !found.iter().any(|known| known == spec) {
                found.push(spec.to_string());
            }
        }
    }
    found
}

/// The project file that `spec` (imported from `path`) names, if it exists:
/// `@/` is the root or `src/`, and the code extensions and `index` files
/// are tried.
pub fn next_resolve_import(path: &str, spec: &str, known: &BTreeSet<&str>) -> Option<String> {
    let bases: Vec<String> = match spec.strip_prefix("@/") {
        Some(rest) => vec![rest.to_string(), format!("src/{rest}")],
        None => {
            let dir = path.rsplit_once('/').map_or("", |(dir, _)| dir);
            vec![join(dir, spec)]
        }
    };
    for base in bases {
        for suffix in ["", ".ts", ".tsx", "/index.ts", "/index.tsx", ".js", ".jsx"] {
            let candidate = format!("{base}{suffix}");
            if known.contains(candidate.as_str()) {
                return Some(candidate);
            }
        }
    }
    None
}

/// The path of the file that the missing import `spec` of `path` would be.
/// `None` for an import that is not a relative code module.
fn next_missing_module_path(path: &str, spec: &str) -> Option<String> {
    if !(spec.starts_with("./") || spec.starts_with("../")) {
        return None;
    }
    let name = spec.rsplit('/').next()?;
    if name.contains('.') || name.is_empty() {
        return None;
    }
    let dir = path.rsplit_once('/').map_or("", |(dir, _)| dir);
    let extension = if path.ends_with(".tsx") { "tsx" } else { "ts" };
    Some(format!("{}.{extension}", join(dir, spec)))
}

/// The project files that the imports of `current` (the file `path`) name.
fn imported(path: &str, current: &str, files: &BTreeMap<&str, &str>) -> Vec<String> {
    let known: BTreeSet<&str> = files.keys().copied().collect();
    next_local_specifiers(current)
        .into_iter()
        .filter_map(|spec| next_resolve_import(path, &spec, &known))
        .collect()
}

/// True if `source` has `prefix` and then `name` as a full word.
fn declares(source: &str, prefix: &str, name: &str) -> bool {
    let wanted = format!("{prefix}{name}");
    source.match_indices(&wanted).any(|(index, _)| {
        !source[index + wanted.len()..].starts_with(|ch: char| ch.is_alphanumeric() || ch == '_')
    })
}

/// The files that a joint repair of `path` can change: the file, the
/// modules that its problems name (`import("lib/storage").AppState`), the
/// file that declares a type that a problem names, the files of a call
/// stack, the shared types, and for a page that does not render the root
/// layout and the providers. A test file: the test and at most two modules
/// that it imports and that the failing cases name.
pub fn next_joint_files(path: &str, problems: &str, files: &BTreeMap<&str, &str>) -> Vec<String> {
    if next_is_test(path) {
        // The test and the modules that it imports and that the failing
        // cases name (`lib/board > moveTask ...`): at most two.
        let mut editable = vec![path.to_string()];
        let text = files.get(path).copied().unwrap_or_default();
        let lower = problems.to_lowercase();
        for file in imported(path, text, files) {
            let stem = file
                .rsplit('/')
                .next()
                .and_then(|name| name.split('.').next())
                .unwrap_or_default()
                .to_lowercase();
            if stem.len() > 2
                && lower.contains(&stem)
                && editable.len() < 3
                && !next_is_types_file(&file)
            {
                editable.push(file);
            }
        }
        return editable;
    }
    let mut editable = vec![path.to_string()];
    for part in problems.split("import(\"").skip(1) {
        let Some(module) = part.split('"').next() else {
            continue;
        };
        for extension in [".ts", ".tsx"] {
            let candidate = format!("{module}{extension}");
            if files.contains_key(candidate.as_str()) && !editable.contains(&candidate) {
                editable.push(candidate);
            }
        }
    }
    // The file that declares a type that a problem names (`does not exist
    // in type 'Action'`): the fix can be a field of that type.
    for part in problems.split("type '").skip(1) {
        let name: String = part
            .chars()
            .take_while(|ch| ch.is_alphanumeric() || *ch == '_')
            .collect();
        if name.is_empty() || !name.starts_with(|ch: char| ch.is_uppercase()) {
            continue;
        }
        for (file, text) in files {
            let declares_it =
                declares(text, "export interface ", &name) || declares(text, "export type ", &name);
            if declares_it
                && !next_is_test(file)
                && editable.len() < 5
                && !editable.iter().any(|known| known == file)
            {
                editable.push((*file).to_string());
            }
        }
    }
    // The files of a call stack (`lib/dates.ts:13`).
    for file in files.keys() {
        let named =
            problems.contains(&format!(" {file}:")) || problems.contains(&format!("< {file}:"));
        if named && !editable.iter().any(|known| known == file) {
            editable.push((*file).to_string());
        }
    }
    let render = problems.contains("when it renders the page") || problems.contains("TS9101");
    let wanted = |file: &str| {
        next_is_types_file(file)
            || (render && (file.ends_with("app/layout.tsx") || file.contains("Provider")))
    };
    for file in files.keys().filter(|file| wanted(file)) {
        if !editable.iter().any(|known| known == file) {
            editable.push((*file).to_string());
        }
    }
    editable
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn imports_resolve_to_project_files() {
        let known: BTreeSet<&str> = ["lib/types.ts", "components/Board.tsx", "src/lib/a.ts"]
            .into_iter()
            .collect();
        assert_eq!(
            next_resolve_import("app/page.tsx", "../lib/types", &known).as_deref(),
            Some("lib/types.ts")
        );
        assert_eq!(
            next_resolve_import("app/page.tsx", "@/components/Board", &known).as_deref(),
            Some("components/Board.tsx")
        );
        assert_eq!(
            next_resolve_import("app/page.tsx", "@/lib/a", &known).as_deref(),
            Some("src/lib/a.ts")
        );
        // Above the root: no project file.
        assert_eq!(next_resolve_import("a.ts", "../../lib/types", &known), None);
        let source = "import { A } from '../lib/types';\nimport x from \"react\";\nconst m = import('./m');\n";
        assert_eq!(next_local_specifiers(source), ["../lib/types", "./m"]);
        assert_eq!(
            next_missing_module_path("app/page.tsx", "./Widget").as_deref(),
            Some("app/Widget.tsx")
        );
        assert_eq!(next_missing_module_path("app/page.tsx", "react"), None);
    }

    #[test]
    fn joint_files_of_a_test_are_at_most_three() {
        let files: BTreeMap<&str, &str> = [
            (
                "tests/board.test.ts",
                "import { moveTask } from '../lib/board';\nimport { T } from '../lib/types';\n",
            ),
            ("lib/board.ts", "export function moveTask() {}"),
            ("lib/types.ts", "export interface T {}"),
        ]
        .into_iter()
        .collect();
        assert_eq!(
            next_joint_files(
                "tests/board.test.ts",
                "FAILED tests/board.test.ts > board > moveTask",
                &files
            ),
            ["tests/board.test.ts", "lib/board.ts"]
        );
        let render = next_joint_files(
            "app/page.tsx",
            "ERROR app/page.tsx(1,1): when it renders the page",
            &[
                ("app/page.tsx", ""),
                ("app/layout.tsx", ""),
                ("components/StoreProvider.tsx", ""),
                ("lib/types.ts", ""),
            ]
            .into_iter()
            .collect(),
        );
        assert_eq!(
            render,
            [
                "app/page.tsx",
                "app/layout.tsx",
                "components/StoreProvider.tsx",
                "lib/types.ts"
            ]
        );
        assert_eq!(next_classify("x TS2307 y"), Problem::MissingModule);
        assert_eq!(next_classify("x TS9101"), Problem::Render);
        assert_eq!(next_classify("x"), Problem::Other);
    }
}
