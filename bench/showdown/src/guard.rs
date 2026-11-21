//! The bench part of the `guard` switch (the engine's
//! `slimtoken::guard`): the layout fixes that need no model call, run in
//! the repair hooks' `prepare` before every gate run.
//!
//! - TS9004 (a provider that no file renders): the root layout imports it
//!   and wraps `{children}` in it.
//! - TS9005 (a frame component, `AppShell`/`Nav`/`Sidebar`..., that no file
//!   renders; only with `shellcheck`, as in the gate): the root layout
//!   imports it and renders it around `{children}`, inside the providers.
//!
//! The gate's own checks (`gate::unmounted_providers`,
//! `gate::unrendered_shell`) find the cases, so the fix and the gate line
//! never disagree. A layout whose `{children}` is not there exactly once,
//! or a component whose export form is unclear, is left to the model.

use crate::extract::Found;
use slimtoken::dag::Plan;
use slimtoken::scheduler::{RunReport, TaskStatus};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Layout fixes made by `prepare` since the last [`take_layout_fixes`].
static LAYOUT_FIXES: AtomicUsize = AtomicUsize::new(0);

/// The number of layout fixes since the last call (and reset it).
pub fn take_layout_fixes() -> usize {
    LAYOUT_FIXES.swap(0, Ordering::Relaxed)
}

/// True unless `SHOWDOWN_OFF` holds `guard`.
pub fn on() -> bool {
    crate::arms::step_on("guard")
}

/// One gate line about the layout: (code, component name, component path).
fn layout_case(line: &str) -> Option<(&'static str, String, String)> {
    let code = ["TS9004", "TS9005"]
        .into_iter()
        .find(|code| line.contains(code))?;
    let name_start = line.find("`<")? + 2;
    let name_len = line[name_start..].find(">`")?;
    let name = &line[name_start..name_start + name_len];
    let path_start = line.find("(`")? + 2;
    let path_len = line[path_start..].find("`)")?;
    let path = &line[path_start..path_start + path_len];
    Some((code, name.to_string(), path.to_string()))
}

/// The import line that `layout` needs for the component `name` of the
/// file `path` (text `component`); `None` when its export form is unclear.
fn import_line(layout: &str, name: &str, path: &str, component: &str) -> Option<String> {
    let spec = crate::repair::relative_specifier(layout, path);
    if crate::repair::is_default_export(component, name) {
        Some(format!("import {name} from '{spec}';"))
    } else if crate::repair::has_named_export(component, name) {
        Some(format!("import {{ {name} }} from '{spec}';"))
    } else {
        None
    }
}

/// True if `source` imports the name `name`.
fn imports_name(source: &str, name: &str) -> bool {
    source.lines().any(|line| {
        let line = line.trim_start();
        line.starts_with("import ")
            && line
                .split(|ch: char| !(ch.is_alphanumeric() || ch == '_'))
                .any(|word| word == name)
    })
}

/// `source` with `import` added after its last import line.
fn add_import(source: &str, import: &str) -> String {
    let lines: Vec<&str> = source.lines().collect();
    let last_import = lines.iter().rposition(|line| {
        let line = line.trim_start();
        line.starts_with("import ") || line.starts_with("} from ")
    });
    let at = match last_import {
        Some(index) => index + 1,
        // After a directive line, else at the top.
        None => lines
            .iter()
            .position(|line| {
                let line = line.trim();
                !(line.starts_with("\"use ") || line.starts_with("'use ") || line.is_empty())
            })
            .unwrap_or(lines.len()),
    };
    let mut out: Vec<String> = lines.iter().map(|line| (*line).to_string()).collect();
    out.insert(at, import.to_string());
    let mut text = out.join("\n");
    text.push('\n');
    text
}

/// The layout `source` (the file `layout`) fixed for the gate lines
/// `lines` (TS9004 first, so that a shell goes inside the providers), with
/// the number of fixes; `files` holds the component texts.
pub fn fix_layout_text(
    layout: &str,
    source: &str,
    lines: &[String],
    files: &BTreeMap<String, String>,
) -> (String, usize) {
    let mut cases: Vec<(&str, String, String)> =
        lines.iter().filter_map(|line| layout_case(line)).collect();
    cases.sort_by_key(|(code, _, _)| *code);
    let mut text = source.to_string();
    let mut fixed = 0;
    for (code, name, path) in cases {
        if text.matches("{children}").count() != 1 || text.contains(&format!("<{name}")) {
            continue;
        }
        let Some(component) = files.get(&path) else {
            continue;
        };
        let wrap = code == "TS9004" || component.contains("children");
        let replacement = if wrap {
            format!("<{name}>{{children}}</{name}>")
        } else {
            format!("<{name} />{{children}}")
        };
        let mut next = text.replacen("{children}", &replacement, 1);
        if !imports_name(&next, &name) {
            let Some(import) = import_line(layout, &name, &path, component) else {
                continue;
            };
            next = add_import(&next, &import);
        }
        text = next;
        fixed += 1;
    }
    (text, fixed)
}

/// The `prepare` step of `guard`: fix the root layout for the gate's
/// TS9004 / TS9005 cases. Returns the number of fixes.
pub fn fix_layout(plan: &Plan, report: &mut RunReport) -> usize {
    if !on() {
        return 0;
    }
    let mut files: BTreeMap<String, String> = BTreeMap::new();
    let mut ids: BTreeMap<String, String> = BTreeMap::new();
    for task in &plan.tasks {
        let Some(record) = report.tasks.get(&task.id) else {
            continue;
        };
        if record.status != TaskStatus::Succeeded {
            continue;
        }
        if let Some(output) = &record.output {
            files.insert(
                task.title.clone(),
                crate::arms::file_text(&task.title, output),
            );
            ids.insert(task.title.clone(), task.id.clone());
        }
    }
    let found: Vec<Found> = files
        .iter()
        .map(|(path, content)| Found {
            path: path.clone(),
            content: content.clone(),
            closed: true,
        })
        .collect();
    let mut lines = crate::gate::unmounted_providers(&found);
    if crate::arms::step_on("shellcheck") {
        lines.extend(crate::gate::unrendered_shell(&found));
    }
    let Some(layout) = ["app/layout.tsx", "src/app/layout.tsx"]
        .into_iter()
        .find(|path| files.contains_key(*path))
    else {
        return 0;
    };
    if lines.is_empty() {
        return 0;
    }
    let (text, fixed) = fix_layout_text(layout, &files[layout], &lines, &files);
    if fixed == 0 {
        return 0;
    }
    if let Some(record) = ids.get(layout).and_then(|id| report.tasks.get_mut(id)) {
        record.output = Some(slimtoken::project::fenced(&text));
    }
    LAYOUT_FIXES.fetch_add(fixed, Ordering::Relaxed);
    fixed
}

#[cfg(test)]
mod tests {
    use super::*;

    const LAYOUT: &str = "import './globals.css';\n\nexport const metadata = { title: 'x' };\n\n\
export default function RootLayout({ children }: { children: React.ReactNode }) {\n  return (\n    \
<html lang=\"en\">\n      <body>{children}</body>\n    </html>\n  );\n}\n";

    fn files() -> BTreeMap<String, String> {
        [
            ("app/layout.tsx", LAYOUT),
            (
                "components/StoreProvider.tsx",
                "\"use client\";\nexport function StoreProvider({ children }) { return children; }\n\
                 export function useStore() {}\n",
            ),
            (
                "components/AppShell.tsx",
                "export default function AppShell({ children }) { return <main>{children}</main>; }\n",
            ),
            (
                "app/page.tsx",
                "import { useStore } from '../components/StoreProvider';\n",
            ),
        ]
        .into_iter()
        .map(|(path, text)| (path.to_string(), text.to_string()))
        .collect()
    }

    #[test]
    fn the_layout_gets_the_provider_and_the_shell_inside_it() {
        let files = files();
        let found: Vec<Found> = files
            .iter()
            .map(|(path, content)| Found {
                path: path.clone(),
                content: content.clone(),
                closed: true,
            })
            .collect();
        let mut lines = crate::gate::unmounted_providers(&found);
        lines.extend(crate::gate::unrendered_shell(&found));
        assert_eq!(lines.len(), 2);
        let (text, fixed) = fix_layout_text("app/layout.tsx", LAYOUT, &lines, &files);
        assert_eq!(fixed, 2);
        assert!(text.contains(
            "<body><StoreProvider><AppShell>{children}</AppShell></StoreProvider></body>"
        ));
        assert!(text.contains("import { StoreProvider } from '../components/StoreProvider';"));
        assert!(text.contains("import AppShell from '../components/AppShell';"));
        // The imports go after the last import line, before `metadata`.
        assert!(text.starts_with("import './globals.css';\nimport "));
        // The gate finds nothing after the fix.
        let mut after = files.clone();
        after.insert("app/layout.tsx".to_string(), text);
        let found: Vec<Found> = after
            .iter()
            .map(|(path, content)| Found {
                path: path.clone(),
                content: content.clone(),
                closed: true,
            })
            .collect();
        assert!(crate::gate::unmounted_providers(&found).is_empty());
        assert!(crate::gate::unrendered_shell(&found).is_empty());
    }

    #[test]
    fn an_unclear_layout_is_left_to_the_model() {
        let files = files();
        let line = "ERROR app/layout.tsx(1,1): error TS9004: no file renders `<StoreProvider>` \
                    (`components/StoreProvider.tsx`), so each page ..."
            .to_string();
        let twice = LAYOUT.replace(
            "<body>{children}</body>",
            "<body>{children}{children}</body>",
        );
        let (text, fixed) = fix_layout_text("app/layout.tsx", &twice, &[line], &files);
        assert_eq!(fixed, 0);
        assert_eq!(text, twice);
    }
}
