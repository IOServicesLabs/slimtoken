//! The gate for SlimToken's check-and-repair step on TypeScript projects.
//!
//! The engine pipes the assembled deliverable to a gate command and repairs
//! the files that the gate names. This gate reads the deliverable on stdin,
//! writes the files to a scratch directory INSIDE a prepared dependency
//! directory (so module resolution finds its `node_modules`), then runs the
//! TypeScript compiler (`tsc --noEmit`) and Vitest at the same time.
//!
//! The output has the shape that the engine's reconcile step reads: one
//! `ERROR <path>(line,col): ...` line for each type error, one
//! `FAILED <test file> > <name>` block for each failed test, and a last line
//! `N failed, M passed (K type errors)`. Exit code 0 means both checks pass.
//!
//! The compiler does not see two rules that `next build` enforces, so the
//! gate checks them itself and reports them in the same `ERROR` format:
//! a component that uses hooks or event handlers needs the `"use client"`
//! directive, and `useSearchParams()` needs a `<Suspense>` boundary.
//! A second declaration of a type that the shared types file (`types.ts`)
//! exports is a `WARN` line: it is not a failure, but two copies of one type
//! drift apart, and the compiler then reports the difference far from the
//! copy.
//!
//! With `SHOWDOWN_ON=qlint` the gate also runs the cross-file quality lints
//! of `qlint.rs` (an unread setting, a literal in the wrong case, a theme
//! selector that nothing sets, a dead link, storage read on the server, a
//! save that drops state, counter ids, fixed seed dates) as `ERROR` lines for
//! the file that should change, and two hints as `WARN` lines.
//!
//! When the type check and the tests pass, the gate does the real thing: it
//! runs `next build` on the files, starts the built app and requests each
//! static page. A build error or a page that does not answer with status 200
//! is an `ERROR` line for the file at fault. The last line then ends with
//! `[deep]`. A deliverable that passes this gate passes the benchmark's
//! `build` and `pages` checks, which run the same commands.

use crate::extract;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

fn strip_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\u{1b}' && chars.peek() == Some(&'[') {
            chars.next();
            for code in chars.by_ref() {
                if code.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            out.push(ch);
        }
    }
    out
}

/// Run a Node script. Returns (exit code was 0, output without colors).
pub(crate) fn node(script: PathBuf, args: &[&str], cwd: &Path) -> (bool, String) {
    node_with_pid(script, args, cwd, |_| {})
}

/// [`node`], and `started` gets the process id once it runs (to stop it).
pub(crate) fn node_with_pid(
    script: PathBuf,
    args: &[&str],
    cwd: &Path,
    started: impl FnOnce(u32),
) -> (bool, String) {
    let output = Command::new("node")
        .arg(script)
        .args(args)
        .current_dir(cwd)
        .env("CI", "1")
        .env("NO_COLOR", "1")
        .env("NEXT_TELEMETRY_DISABLED", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .and_then(|child| {
            started(child.id());
            child.wait_with_output()
        });
    match output {
        Ok(output) => {
            let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
            text.push_str(&String::from_utf8_lossy(&output.stderr));
            (output.status.success(), strip_ansi(&text))
        }
        Err(error) => (false, format!("could not start node: {error}")),
    }
}

/// The number written immediately before `word` in `line`.
fn count_before(line: &str, word: &str) -> u64 {
    let Some(index) = line.find(word) else {
        return 0;
    };
    let digits: String = line[..index]
        .trim_end()
        .chars()
        .rev()
        .take_while(char::is_ascii_digit)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    digits.parse().unwrap_or(0)
}

/// Turn Vitest's verbose output into `FAILED` blocks and the two totals.
pub(crate) fn vitest_report(out: &str) -> (Vec<String>, u64, u64) {
    let mut lines = Vec::new();
    let mut block: Vec<String> = Vec::new();
    for line in out.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('✓') {
            lines.append(&mut block);
        } else if (trimmed.starts_with('×') || trimmed.starts_with("FAIL "))
            && trimmed.contains(" > ")
        {
            lines.append(&mut block);
            let name = trimmed
                .trim_start_matches('×')
                .trim()
                .trim_start_matches("FAIL ")
                .trim();
            block.push(format!("FAILED {name}"));
        } else if !block.is_empty() && !trimmed.is_empty() && block.len() < 30 {
            block.push(format!("    {trimmed}"));
        }
    }
    lines.append(&mut block);
    let totals = out
        .lines()
        .rev()
        .map(str::trim)
        .find(|line| line.starts_with("Tests ") && line.chars().any(|ch| ch.is_ascii_digit()));
    let (failed, passed) = match totals {
        Some(line) => (count_before(line, "failed"), count_before(line, "passed")),
        None => (0, 0),
    };
    (lines, failed, passed)
}

/// True if `source` calls a React hook (`useXxx(`) or sets an event handler.
pub(crate) fn needs_client(source: &str) -> bool {
    if source.contains("createContext") {
        return true;
    }
    // A handler prop (` onClick={`, ` onNext={`): a Server Component cannot
    // pass a function to a component.
    let handler = source.match_indices(" on").any(|(index, _)| {
        let rest = &source[index + 3..];
        let name_len = rest.bytes().take_while(u8::is_ascii_alphanumeric).count();
        rest.starts_with(|ch: char| ch.is_ascii_uppercase()) && rest[name_len..].starts_with("={")
    });
    if handler {
        return true;
    }
    let bytes = source.as_bytes();
    source.match_indices("use").any(|(index, _)| {
        let before_ok = index == 0
            || !(bytes[index - 1].is_ascii_alphanumeric()
                || matches!(bytes[index - 1], b'_' | b'.'));
        let rest = &source[index + 3..];
        let name_len = rest
            .bytes()
            .take_while(|byte| byte.is_ascii_alphanumeric() || *byte == b'_')
            .count();
        before_ok
            && rest.starts_with(|ch: char| ch.is_ascii_uppercase())
            && matches!(rest.as_bytes().get(name_len), Some(b'(' | b'<'))
    })
}

/// True if the first statement of `source` is the `"use client"` directive.
pub(crate) fn is_client(source: &str) -> bool {
    source
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty() && !line.starts_with("//"))
        .is_some_and(|line| line.starts_with("\"use client\"") || line.starts_with("'use client'"))
}

/// A context provider that other files import but that no file renders.
/// Each page that uses its hook then fails when it is opened. The layout is
/// the file that must render it, so the `ERROR` line names the layout.
pub(crate) fn unmounted_providers(files: &[extract::Found]) -> Vec<String> {
    let Some(layout) = files
        .iter()
        .find(|file| file.path == "app/layout.tsx" || file.path == "src/app/layout.tsx")
    else {
        return Vec::new();
    };
    let mut lines = Vec::new();
    for provider in files
        .iter()
        .filter(|file| file.path.ends_with("Provider.tsx"))
    {
        let name = provider
            .path
            .rsplit('/')
            .next()
            .and_then(|name| name.strip_suffix(".tsx"))
            .unwrap_or_default();
        let others = || files.iter().filter(|file| file.path != provider.path);
        let imported = others().any(|file| {
            file.content.contains(&format!("/{name}\""))
                || file.content.contains(&format!("/{name}'"))
        });
        let rendered = others().any(|file| file.content.contains(&format!("<{name}")));
        if name.is_empty() || !imported || rendered || !provider.content.contains(name) {
            continue;
        }
        lines.push(format!(
            "ERROR {}(1,1): error TS9004: no file renders `<{name}>` (`{}`), so each page that \
             uses its hook fails when it is opened. In this layout, import `{name}` from `{}` \
             and put `{{children}}` inside `<{name}>`. Keep this layout a Server Component: do \
             not add \"use client\".",
            layout.path, provider.path, provider.path
        ));
    }
    lines
}

/// A component that holds the frame of the app (navigation, side bar,
/// header) and that no file renders. The app then builds and each page
/// opens, but it has no navigation: only a person who opens the app sees it.
pub(crate) fn unrendered_shell(files: &[extract::Found]) -> Vec<String> {
    let Some(layout) = files
        .iter()
        .find(|file| file.path == "app/layout.tsx" || file.path == "src/app/layout.tsx")
    else {
        return Vec::new();
    };
    let mut lines = Vec::new();
    for shell in files.iter().filter(|file| {
        file.path.contains("components/")
            && file.path.ends_with(".tsx")
            && !file.path.contains(".test.")
    }) {
        let name = shell
            .path
            .rsplit('/')
            .next()
            .and_then(|name| name.strip_suffix(".tsx"))
            .unwrap_or_default();
        let lower = name.to_lowercase();
        let is_frame = ["shell", "nav", "sidebar", "header", "topbar"]
            .iter()
            .any(|word| lower.contains(word));
        let rendered = files
            .iter()
            .filter(|file| file.path != shell.path)
            .any(|file| file.content.contains(&format!("<{name}")));
        if name.is_empty() || !is_frame || rendered || !shell.content.contains(name) {
            continue;
        }
        lines.push(format!(
            "ERROR {}(1,1): error TS9005: no file renders `<{name}>` (`{}`), so the app has no \
             navigation. In this layout, import `{name}` from `{}` with the form of its export \
             (default or named) and render it around `{{children}}`, inside the providers. Keep \
             this layout a Server Component: do not add \"use client\".",
            layout.path, shell.path, shell.path
        ));
    }
    lines
}

/// The `next build` rules that the compiler does not check.
fn next_lints(path: &str, source: &str) -> Vec<String> {
    let mut lines = Vec::new();
    if !path.ends_with(".tsx") || path.contains(".test.") {
        return lines;
    }
    if needs_client(source) && !is_client(source) {
        lines.push(format!(
            "ERROR {path}(1,1): error TS9001: this file uses React hooks or event handlers \
             but does not start with the \"use client\" directive, so `next build` fails. \
             Make \"use client\" its first line."
        ));
    }
    if source.contains("useSearchParams(") && !source.contains("Suspense") {
        lines.push(format!(
            "ERROR {path}(1,1): error TS9002: useSearchParams() is not inside a <Suspense> \
             boundary, so `next build` fails. Move the code that calls it into an inner \
             component in this file and render that component inside <Suspense>."
        ));
    }
    lines
}

/// The name that a line declares with `interface` or `type`, if it does.
fn declared_type(line: &str) -> Option<&str> {
    let line = line.trim_start();
    let line = line.strip_prefix("export ").unwrap_or(line);
    let rest = line
        .strip_prefix("interface ")
        .or_else(|| line.strip_prefix("type "))?;
    let end = rest
        .find(|ch: char| !(ch.is_ascii_alphanumeric() || ch == '_'))
        .unwrap_or(rest.len());
    (end > 0).then(|| &rest[..end])
}

/// True for a file that owns shared types.
fn is_types_file(path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path);
    name == "types.ts" || path.contains("/types/")
}

/// Declarations in `source` of a type that a shared types file exports.
fn duplicate_types(path: &str, source: &str, owned: &[(String, String)]) -> Vec<String> {
    if is_types_file(path) {
        return Vec::new();
    }
    source
        .lines()
        .enumerate()
        .filter_map(|(index, line)| {
            let name = declared_type(line)?;
            let (_, owner) = owned.iter().find(|(known, _)| known == name)?;
            Some(format!(
                "WARN {path}({},1): error TS9003: `{name}` is declared here again, but \
                 `{owner}` exports it. Delete this declaration and import `{name}` from \
                 `{owner}`.",
                index + 1
            ))
        })
        .collect()
}

/// The URL path of an App Router page file, for example `app/board/page.tsx`
/// gives `/board`. `None` for a file that is not a page.
fn route_of(path: &str) -> Option<String> {
    let rest = path
        .strip_prefix("src/app/")
        .or_else(|| path.strip_prefix("app/"))?;
    let (dir, name) = match rest.rsplit_once('/') {
        Some((dir, name)) => (dir, name),
        None => ("", rest),
    };
    if !matches!(name, "page.tsx" | "page.jsx" | "page.ts" | "page.js") {
        return None;
    }
    // A route group `(name)` is not part of the URL.
    let parts: Vec<&str> = dir
        .split('/')
        .filter(|part| !part.is_empty() && !part.starts_with('('))
        .collect();
    Some(format!("/{}", parts.join("/")))
}

/// `./app/x.tsx:3:1` or `./app/x.tsx (3:1)`: the file, line and column.
fn file_header(line: &str) -> Option<(String, u32, u32)> {
    let rest = line.strip_prefix("./")?;
    let (path, position) = match rest
        .strip_suffix(')')
        .and_then(|rest| rest.rsplit_once(" ("))
    {
        Some((path, position)) => (path, position),
        None => {
            let mut parts = rest.rsplitn(3, ':');
            let column = parts.next()?;
            let row = parts.next()?;
            let path = parts.next()?;
            return Some((project_path(path), row.parse().ok()?, column.parse().ok()?));
        }
    };
    let (row, column) = position.split_once(':')?;
    Some((project_path(path), row.parse().ok()?, column.parse().ok()?))
}

/// The project path of a file that `next build` names in the gate's own
/// directory (`.check/keep-1-2/app/layout.tsx` gives `app/layout.tsx`).
fn project_path(path: &str) -> String {
    let path = path.replace('\\', "/");
    match path.find(".check/") {
        Some(at) => {
            let rest = &path[at + ".check/".len()..];
            rest.split_once('/')
                .map_or(rest, |(_, file)| file)
                .to_string()
        }
        None => path,
    }
}

/// Turn the output of a failed `next build` into `ERROR` lines.
fn build_errors(out: &str, paths: &[String]) -> Vec<String> {
    let lines: Vec<&str> = out.lines().map(str::trim).collect();
    let mut found: Vec<String> = Vec::new();
    let mut recent: Vec<&str> = Vec::new();
    let mut index = 0;
    while index < lines.len() {
        let line = lines[index];
        if let Some((path, row, column)) = file_header(line) {
            let mut message: Vec<&str> = Vec::new();
            let mut next = index + 1;
            while next < lines.len() && message.len() < 6 {
                let text = lines[next];
                if file_header(text).is_some() || (text.is_empty() && !message.is_empty()) {
                    break;
                }
                if !text.is_empty() && !text.starts_with("at ") {
                    message.push(text);
                }
                next += 1;
            }
            found.push(format!(
                "ERROR {path}({row},{column}): error TS9100: `next build` fails: {}",
                message.join(" | ")
            ));
            index = next;
            continue;
        }
        if let Some(route) = line
            .split_once("prerendering page \"")
            .and_then(|(_, rest)| rest.split_once('"'))
            .map(|(route, _)| route)
        {
            let page = paths
                .iter()
                .find(|path| route_of(path).as_deref() == Some(route));
            if let Some(page) = page {
                // The reason is in the lines before or after this line.
                let start = recent.len().saturating_sub(2);
                let mut reason: Vec<&str> = recent[start..].to_vec();
                for text in lines[index + 1..].iter().take(12) {
                    if text.contains("prerendering page") || file_header(text).is_some() {
                        break;
                    }
                    if !text.is_empty() && !text.starts_with("at ") && reason.len() < 8 {
                        reason.push(text);
                    }
                }
                // The project files of the call stack, from the throw site
                // out: the cause is often not in the page.
                let mut stack: Vec<String> = Vec::new();
                for text in lines[index + 1..].iter().take(14) {
                    if text.contains("prerendering page") {
                        break;
                    }
                    let frame = text
                        .strip_prefix("at ")
                        .and_then(|rest| rest.rsplit_once('('))
                        .map(|(_, place)| place.trim_end_matches(')').replace('\\', "/"));
                    let Some(frame) = frame else {
                        continue;
                    };
                    let place = frame.rsplitn(2, ':').last().unwrap_or(&frame).to_string();
                    let known = paths
                        .iter()
                        .any(|path| place.starts_with(&format!("{path}:")));
                    if known && !stack.contains(&place) && stack.len() < 6 {
                        stack.push(place);
                    }
                }
                let stack = if stack.is_empty() {
                    String::new()
                } else {
                    format!(" | call stack (file:line): {}", stack.join(" < "))
                };
                found.push(format!(
                    "ERROR {page}(1,1): error TS9100: `next build` fails when it renders the page {route}: {}{stack}",
                    reason.join(" | ")
                ));
            }
        } else if line.starts_with('⨯')
            || line.starts_with("Error:")
            || line.starts_with("Type error:")
        {
            recent.push(line);
        }
        index += 1;
    }
    found.dedup();
    if found.is_empty() {
        let tail: Vec<&str> = lines
            .iter()
            .rev()
            .filter(|line| !line.is_empty() && !line.starts_with("at "))
            .take(4)
            .copied()
            .collect();
        found.push(format!(
            "ERROR gate: `next build` failed: {}",
            tail.into_iter().rev().collect::<Vec<_>>().join(" | ")
        ));
    }
    found
}

/// The routes that `next build` prerendered with status 200: the keys of
/// `routes` in `.next/prerender-manifest.json` (Next 16: URL paths such as
/// `/` and `/board`, the form [`route_of`] gives) that `notFoundRoutes` does
/// not list, whose entry has no `initialStatus` other than 200 and no
/// redirect `location` in `initialHeaders`, and whose saved response
/// (`.next/server/app/<path>.meta`, `index.meta` for `/`) exists and has the
/// same: no `status` other than 200 and no `location` header. A page that
/// calls `notFound()` or `redirect()` while it renders is prerendered too,
/// with status 404 or 307; Next 16.4 writes that status to the `.meta` file
/// and not always to the manifest, and such a page must still be requested.
/// Empty when the manifest is missing or does not parse, so every page is
/// then requested.
fn prerendered_routes(root: &Path) -> std::collections::HashSet<String> {
    let next = root.join(".next");
    let read_json = |path: std::path::PathBuf| {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
    };
    // No status other than 200 and no redirect, in `status` and `headers`.
    let answers_200 = |entry: &serde_json::Value, status: &str, headers: &str| {
        let status_ok = match entry.get(status) {
            None | Some(serde_json::Value::Null) => true,
            Some(code) => code.as_u64() == Some(200),
        };
        let redirects = entry
            .get(headers)
            .and_then(|headers| headers.as_object())
            .is_some_and(|headers| {
                headers
                    .keys()
                    .any(|name| name.eq_ignore_ascii_case("location"))
            });
        status_ok && !redirects
    };
    let Some(manifest) = read_json(next.join("prerender-manifest.json")) else {
        return Default::default();
    };
    let not_found: Vec<&str> = manifest["notFoundRoutes"]
        .as_array()
        .map(|list| list.iter().filter_map(|route| route.as_str()).collect())
        .unwrap_or_default();
    let Some(routes) = manifest["routes"].as_object() else {
        return Default::default();
    };
    routes
        .iter()
        .filter(|(route, entry)| {
            let name = match route.trim_start_matches('/') {
                "" => "index",
                path => path,
            };
            let saved = read_json(next.join("server").join("app").join(format!("{name}.meta")));
            !not_found.contains(&route.as_str())
                && answers_200(entry, "initialStatus", "initialHeaders")
                && saved.is_some_and(|meta| answers_200(&meta, "status", "headers"))
        })
        .map(|(route, _)| route.clone())
        .collect()
}

/// Start the built app in `root` and request each static page that is not
/// in `skip`. Returns one `ERROR` line for each page that does not answer
/// with status 200. With `prerendered` (the switch) and no page left to
/// request, the app is not started. With `trim_reason` (the switch
/// `gatereason`, its own switch so that an A/B of `prerendered` measures
/// only the skipped pages), the reason of a failed page starts at the
/// server's first `⨯` line when it has one, so that the start-up lines the
/// server prints before it (which the first request reads) do not fill the
/// reason.
fn page_errors(
    root: &Path,
    next: &Path,
    paths: &[String],
    skip: &std::collections::HashSet<String>,
    prerendered: bool,
    trim_reason: bool,
) -> Vec<String> {
    use crate::launch::{free_port, kill_tree, port_open};
    let routes: Vec<(String, &String)> = paths
        .iter()
        .filter_map(|path| route_of(path).map(|route| (route, path)))
        .filter(|(route, _)| !route.contains('['))
        .filter(|(route, _)| !skip.contains(route))
        .collect();
    if prerendered && std::env::var("SHOWDOWN_GATE_TIMING").is_ok() {
        eprintln!(
            "gate: pages to request {} (prerendered, skipped: {})",
            routes.len(),
            skip.len()
        );
    }
    if routes.is_empty() && prerendered {
        return Vec::new();
    }
    let log_path = root.join(".server.log");
    let (Some(port), Ok(log)) = (free_port(), std::fs::File::create(&log_path)) else {
        return vec!["ERROR gate: could not start the built app (no port or no log)".to_string()];
    };
    let child = log.try_clone().and_then(|log_copy| {
        Command::new("node")
            .arg(next)
            .args(["start", "-p", &port.to_string()])
            .current_dir(root)
            .env("NEXT_TELEMETRY_DISABLED", "1")
            .env("NO_COLOR", "1")
            .stdin(Stdio::null())
            .stdout(log_copy)
            .stderr(log)
            .spawn()
    });
    let Ok(mut child) = child else {
        return vec!["ERROR gate: could not start the built app".to_string()];
    };
    let started = std::time::Instant::now();
    let up = loop {
        if port_open(port) {
            break true;
        }
        if matches!(child.try_wait(), Ok(Some(_)))
            || started.elapsed() > std::time::Duration::from_secs(45)
        {
            break false;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    };
    let mut found = Vec::new();
    if up {
        for (route, path) in routes {
            let before = std::fs::metadata(&log_path).map_or(0, |meta| meta.len() as usize);
            let status = crate::pages::status(port, &route);
            if status == Some(200) {
                continue;
            }
            // The server writes the reason to its log.
            std::thread::sleep(std::time::Duration::from_millis(150));
            let log = std::fs::read(&log_path).unwrap_or_default();
            let fresh = strip_ansi(&String::from_utf8_lossy(
                log.get(before..).unwrap_or_default(),
            ));
            let mut lines: Vec<&str> = fresh.lines().map(str::trim).collect();
            if trim_reason {
                if let Some(at) = lines.iter().position(|line| line.starts_with('⨯')) {
                    lines.drain(..at);
                }
            }
            let reason: Vec<&str> = lines
                .into_iter()
                .filter(|line| !line.is_empty() && !line.starts_with("at "))
                .take(5)
                .collect();
            found.push(format!(
                "ERROR {path}(1,1): error TS9101: the build passes, but the page {route} answers \
                 with status {} when it is opened: {}",
                status.map_or("none".to_string(), |code| code.to_string()),
                reason.join(" | ")
            ));
        }
    } else {
        found.push("ERROR gate: the built app did not start".to_string());
    }
    kill_tree(child.id());
    let _ = child.wait();
    found
}

/// Remove the project files of an earlier round that the deliverable no
/// longer has. Caches and build output stay.
fn remove_stale(root: &Path, dir: &Path, paths: &[String]) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with('.') || name == "node_modules" || name == "next-env.d.ts" {
            continue;
        }
        if path.is_dir() {
            remove_stale(root, &path, paths);
            continue;
        }
        let relative = path
            .strip_prefix(root)
            .map(|rest| rest.to_string_lossy().replace('\\', "/"))
            .unwrap_or_default();
        let cache = name.ends_with(".tsbuildinfo");
        if !cache && !paths.contains(&relative) {
            let _ = std::fs::remove_file(&path);
        }
    }
}

/// The real checks: `next build`, then every static page of the built app.
fn deep_check(root: &Path, modules: &Path, paths: &[String]) -> Vec<String> {
    let next = modules.join("next").join("dist").join("bin").join("next");
    // The fast checks run the compiler on the same files, so the build of
    // the gate does not check the types a second time. (The benchmark's own
    // `npm run build` after the run checks them in full.)
    let config = root.join("next.config.mjs");
    if crate::arms::step_on("notypes") && config.is_file() {
        let moved = root.join("next.config.app.mjs");
        if std::fs::rename(&config, &moved).is_ok() {
            let wrapper = "import app from './next.config.app.mjs';\n\
                const base = typeof app === 'function' ? app : () => app;\n\
                export default async (...args) => ({\n  ...(await base(...args)),\n  \
                typescript: { ignoreBuildErrors: true },\n});\n";
            if std::fs::write(&config, wrapper).is_err() {
                let _ = std::fs::rename(&moved, &config);
            }
        }
    }
    let timing = std::env::var("SHOWDOWN_GATE_TIMING").is_ok();
    let started = std::time::Instant::now();
    let (ok, out) = node(next.clone(), &["build"], root);
    if timing {
        eprintln!("gate: build {:.2}s", started.elapsed().as_secs_f64());
    }
    if !ok {
        // Keep the raw output of the last failed build, for work on this gate.
        if let Some(deps) = modules.parent() {
            let _ = std::fs::write(deps.join("last-failed-build.log"), &out);
        }
        return build_errors(&out, paths);
    }
    let started = std::time::Instant::now();
    // `prerendered`: a page that `next build` prerendered already rendered
    // once without error (a page that fails to prerender fails the build),
    // and the server sends its saved HTML with status 200. So request only
    // the pages it did not prerender, and start no server when none is left.
    // `gatereason`: trim the start-up lines off the reason of a failed page.
    let trim_reason = crate::arms::step_on("gatereason");
    let found = if crate::arms::step_on("prerendered") {
        page_errors(
            root,
            &next,
            paths,
            &prerendered_routes(root),
            true,
            trim_reason,
        )
    } else {
        page_errors(root, &next, paths, &Default::default(), false, trim_reason)
    };
    if timing {
        eprintln!("gate: pages {:.2}s", started.elapsed().as_secs_f64());
    }
    found
}

/// The gate's setting `name` (`MODE` or `DIR`): the engine's
/// `SLIMTOKEN_CHECK_<name>` if it is set, else the bench's
/// `SHOWDOWN_GATE_<name>`. The engine's checker sets both to one value.
fn check_env(name: &str) -> Option<String> {
    std::env::var(format!("SLIMTOKEN_CHECK_{name}"))
        .or_else(|_| std::env::var(format!("SHOWDOWN_GATE_{name}")))
        .ok()
}

/// The files of the gate's input document, read as the gate reads them.
pub(crate) fn scan_doc(text: &str) -> Vec<extract::Found> {
    if crate::repair::engcheck() {
        slimtoken::project::scan(text)
    } else {
        extract::scan(text)
    }
}

/// The path under which the gate writes `path`, or `None` for a path that
/// it does not write.
pub(crate) fn gate_path(path: &str) -> Option<String> {
    if crate::repair::engcheck() {
        slimtoken::project::safe_path(path)
    } else {
        extract::safe_path(path)
    }
}

/// True for a Next.js app (a file under `app/` or `src/app/`).
pub(crate) fn is_next_app(files: &[extract::Found]) -> bool {
    files
        .iter()
        .any(|file| file.path.starts_with("app/") || file.path.starts_with("src/app/"))
}

/// The text that the gate writes for `path`. In the gate's copy of a test,
/// each assertion is a soft one: the test goes on after an assertion that
/// fails, so one gate run reports all wrong expectations of a test, not
/// only the first. A test passes with soft assertions exactly when it passes
/// without.
pub(crate) fn gate_copy(path: &str, content: String) -> String {
    if path.contains(".test.") && crate::arms::step_on("softexpect") {
        content.replace(" expect(", " expect.soft(")
    } else {
        content
    }
}

/// The gate's own checks of `files`, the rules that the compiler does not
/// see: the `ERROR` lines and the `WARN` lines, in the gate's order. Only a
/// file that the gate writes ([`gate_path`]) is checked on its own.
pub(crate) fn lint_files(files: &[extract::Found]) -> (Vec<String>, Vec<String>) {
    let mut lints = Vec::new();
    let mut warnings = Vec::new();
    let is_next = is_next_app(files);
    let owned: Vec<(String, String)> = files
        .iter()
        .filter(|file| is_types_file(&file.path))
        .flat_map(|file| {
            file.content
                .lines()
                .filter(|line| line.trim_start().starts_with("export "))
                .filter_map(declared_type)
                .map(|name| (name.to_string(), file.path.clone()))
                .collect::<Vec<_>>()
        })
        .collect();
    if is_next {
        lints.extend(unmounted_providers(files));
        if crate::arms::step_on("shellcheck") {
            lints.extend(unrendered_shell(files));
        }
    }
    for file in files {
        if gate_path(&file.path).is_none() {
            continue;
        }
        if is_next {
            lints.extend(next_lints(&file.path, &file.content));
        }
        if file.path.ends_with(".ts") || file.path.ends_with(".tsx") {
            warnings.extend(duplicate_types(&file.path, &file.content, &owned));
        }
    }
    // `qlint`: the cross-file quality lints (`qlint.rs`); `qlint2`: the
    // convention lints (TS9020-TS9025), with or without `qlint`.
    let (base, conventions) = (
        crate::arms::step_on("qlint"),
        crate::arms::step_on("qlint2"),
    );
    if is_next && (base || conventions) {
        let app: Vec<(String, String)> = files
            .iter()
            .filter(|file| gate_path(&file.path).is_some())
            .map(|file| (file.path.clone(), file.content.clone()))
            .collect();
        // `seed-dates` is an `ERROR` only when the plan asked for relative
        // dates (`qrules` with `seeddates`).
        let relative_dates = crate::arms::step_on("qrules") && crate::arms::step_on("seeddates");
        let (errors, hints) = crate::qlint::gate_lines_with(
            &app,
            relative_dates,
            base,
            conventions,
            crate::arms::step_on("qsemantic"),
        );
        lints.extend(errors);
        warnings.extend(hints);
    }
    (lints, warnings)
}

/// The gate's checks of `files` that a check during the wave can run on the
/// files that landed so far (`inwavelint`): the per-file rules of a Next.js
/// file ([`next_lints`]) and, with `qlint2`, the app lints whose files have
/// all landed (`qlint::landed_lines`). Returns the `ERROR` lines and the
/// files whose lint waits for a file that has not landed. The lines are
/// the gate's, except that a lint names the files it only reads as context.
pub(crate) fn landed_lints(
    files: &[extract::Found],
) -> (Vec<String>, std::collections::BTreeSet<String>) {
    let mut lints = Vec::new();
    if !is_next_app(files) {
        return (lints, Default::default());
    }
    for file in files {
        if gate_path(&file.path).is_some() {
            lints.extend(next_lints(&file.path, &file.content));
        }
    }
    if !crate::arms::step_on("qlint2") {
        return (lints, Default::default());
    }
    let app: Vec<(String, String)> = files
        .iter()
        .filter(|file| gate_path(&file.path).is_some())
        .map(|file| (file.path.clone(), file.content.clone()))
        .collect();
    let (errors, waiting) = crate::qlint::landed_lines(&app);
    lints.extend(errors);
    (lints, waiting)
}

/// The fast checks' part of the gate output.
pub(crate) struct Fast {
    /// The `ERROR` lines (compiler, then `lints`) and the `FAILED` blocks.
    pub lines: Vec<String>,
    pub type_errors: u64,
    pub failed_tests: u64,
    pub passed: u64,
}

/// The gate's lines for the compiler output `tsc_out` (`tsc_ok`: it exited
/// with 0), the gate's `lints` and the Vitest output, for files written
/// under `root`.
pub(crate) fn fast_lines(
    root: &Path,
    tsc_ok: bool,
    tsc_out: &str,
    lints: Vec<String>,
    vitest_out: Option<&str>,
) -> Fast {
    // Compiler messages name other files by absolute path; make them
    // project-relative so the repair prompt stays short and readable.
    let prefix = format!("{}/", root.display().to_string().replace('\\', "/"));
    let tsc_out = tsc_out.replace(&prefix, "");
    let mut lines: Vec<String> = tsc_out
        .lines()
        .filter(|line| line.contains(": error TS"))
        .map(|line| format!("ERROR {}", line.trim()))
        .collect();
    if !tsc_ok && lines.is_empty() {
        // The compiler failed with no type error to name (it could not
        // start, or the project has no valid tsconfig). Never a pass.
        let first = tsc_out.lines().find(|line| !line.trim().is_empty());
        lines.push(format!(
            "ERROR gate: the type check did not run: {}",
            first.unwrap_or("no output").trim()
        ));
    }
    lines.extend(lints);
    let type_errors = lines.len() as u64;
    let (mut failed_tests, mut passed) = (0, 0);
    if let Some(out) = vitest_out {
        let (blocks, failed, ok) = vitest_report(out);
        // The report shows the test as the worker wrote it.
        lines.extend(
            blocks
                .into_iter()
                .map(|block| block.replace("expect.soft(", "expect(")),
        );
        failed_tests = failed;
        passed = ok;
    }
    Fast {
        lines,
        type_errors,
        failed_tests,
        passed,
    }
}

/// Run the gate. Returns the process exit code.
pub fn run(deps: &Path) -> i32 {
    // The checks run in another directory, so the paths must be absolute.
    let deps = &match deps.canonicalize() {
        Ok(path) => match path.to_str().and_then(|text| text.strip_prefix(r"\\?\")) {
            Some(rest) if !rest.starts_with("UNC") => PathBuf::from(rest),
            _ => path,
        },
        Err(_) => deps.to_path_buf(),
    };
    if !deps.join("node_modules").join("typescript").is_dir() {
        // A missing toolchain must fail loudly: with no compiler output there
        // are no errors to count, and that would read as a pass.
        println!(
            "ERROR gate: no node_modules/typescript under {}",
            deps.display()
        );
        println!("1 failed, 0 passed (toolchain missing)");
        return 2;
    }
    let mut text = String::new();
    if std::io::stdin().read_to_string(&mut text).is_err() {
        println!("ERROR gate: the deliverable on stdin is not UTF-8");
        println!("1 failed, 0 passed (no input)");
        return 2;
    }
    // `SLIMTOKEN_CHECK_DIR` (the engine's checker; `SHOWDOWN_GATE_DIR` is
    // the bench's name for it) names a directory that stays between gate
    // runs of one benchmark run, so that the compiler and the build work
    // from their caches (`tsconfig.tsbuildinfo`, `.next/cache`) in later
    // rounds.
    let kept_name = check_env("DIR").filter(|name| {
        !name.is_empty()
            && name
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || ch == '-')
    });
    let root = deps.join(".check").join(match &kept_name {
        Some(name) => format!("keep-{name}"),
        None => format!("run-{}", std::process::id()),
    });
    // `SHOWDOWN_GATE_MODE`: `fast` runs only the fast checks, `deep` runs
    // only the real build and the pages. The repair step starts one of each
    // at the same time. With no mode the gate runs the fast checks and then,
    // if they pass, the real build.
    let mode = check_env("MODE").unwrap_or_default();
    let (only_fast, only_deep) = (mode == "fast", mode == "deep");
    // Remove what an earlier gate run could not remove (a locked file). A
    // new directory can be the directory of a gate that runs now.
    if let Ok(entries) = std::fs::read_dir(deps.join(".check")) {
        for entry in entries.flatten() {
            let old = entry
                .metadata()
                .and_then(|data| data.modified())
                .ok()
                .and_then(|time| time.elapsed().ok())
                .is_some_and(|age| age.as_secs() > 600);
            if old {
                let _ = std::fs::remove_dir_all(entry.path());
            }
        }
    }
    if mode == "clean" {
        // The repair step is done with a kept directory.
        if kept_name.is_some() {
            let _ = std::fs::remove_dir_all(&root);
        }
        return 0;
    }
    let mut has_tests = false;
    let mut paths: Vec<String> = Vec::new();
    let files = scan_doc(&text);
    let is_next = is_next_app(&files);
    let (mut lints, mut warnings) = lint_files(&files);
    for file in files {
        let Some(path) = gate_path(&file.path) else {
            continue;
        };
        has_tests |= file.path.contains(".test.");
        paths.push(file.path.clone());
        let target = root.join(&path);
        if let Some(parent) = target.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let content = gate_copy(&file.path, file.content);
        // An unchanged file keeps its time stamp, for the caches.
        let same = kept_name.is_some()
            && std::fs::read(&target).is_ok_and(|old| old == content.as_bytes());
        if !same {
            let _ = std::fs::write(target, content);
        }
    }
    if kept_name.is_some() {
        remove_stale(&root, &root, &paths);
        // The wrapper of an earlier round (see `deep_check`).
        let _ = std::fs::remove_file(root.join("next.config.app.mjs"));
    }

    let modules = deps.join("node_modules");
    let tsc_script = modules.join("typescript").join("bin").join("tsc");
    let vitest_script = modules.join("vitest").join("vitest.mjs");
    // The two checks are independent: run them at the same time.
    if only_deep {
        lints.clear();
        warnings.clear();
    }
    let ((tsc_ok, tsc_out), vitest_out) = std::thread::scope(|scope| {
        if only_deep {
            return ((true, String::new()), None);
        }
        let tsc = scope.spawn(|| {
            let started = std::time::Instant::now();
            let out = node(tsc_script, &["--noEmit", "-p", "."], &root);
            if std::env::var("SHOWDOWN_GATE_TIMING").is_ok() {
                eprintln!("gate: tsc {:.2}s", started.elapsed().as_secs_f64());
            }
            out
        });
        let vitest = has_tests.then(|| {
            scope.spawn(|| {
                node(
                    vitest_script,
                    &["run", "--root", ".", "--reporter=verbose"],
                    &root,
                )
            })
        });
        (
            tsc.join().unwrap_or_default(),
            vitest.map(|job| job.join().unwrap_or_default().1),
        )
    });

    let Fast {
        mut lines,
        mut type_errors,
        failed_tests,
        passed,
    } = fast_lines(&root, tsc_ok, &tsc_out, lints, vitest_out.as_deref());
    let mut failed = type_errors + failed_tests;
    // The real build and the real pages, when the fast checks pass.
    let deep = failed == 0 && !only_fast && is_next && modules.join("next").is_dir();
    if deep {
        let found = deep_check(&root, &modules, &paths);
        type_errors += found.len() as u64;
        failed += found.len() as u64;
        lines.extend(found);
    }
    lines.extend(warnings);
    lines.push(format!(
        "{failed} failed, {passed} passed ({type_errors} type errors){}",
        if deep { " [deep]" } else { "" }
    ));
    for _ in 0..10 {
        if kept_name.is_some() || std::fs::remove_dir_all(&root).is_ok() || !root.exists() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    println!("{}", lines.join("\n"));
    i32::from(failed != 0)
}

#[cfg(test)]
mod tests {
    #[test]
    fn prerendered_routes_are_the_keys_of_the_manifest() {
        let root = std::env::temp_dir().join(format!(
            "showdown-prerendered-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        assert!(super::prerendered_routes(&root).is_empty());
        std::fs::create_dir_all(root.join(".next")).unwrap();
        // The shape of Next 16.4's file (fields cut down).
        std::fs::write(
            root.join(".next").join("prerender-manifest.json"),
            r#"{"version":4,"routes":{"/":{"srcRoute":"/","dataRoute":"/index.rsc"},
            "/board":{"initialStatus":200,"srcRoute":"/board"},"/_not-found":{"initialStatus":404},
            "/team":{"initialStatus":404,"initialHeaders":{"x-nextjs-stale-time":"300"}},
            "/old":{"initialStatus":307,"initialHeaders":{"location":"/board"}},
            "/moved":{"initialHeaders":{"Location":"/board"}},
            "/gone":{"initialStatus":200},"/calendar":{},"/archive":{},"/later":{}},"dynamicRoutes":{},
            "notFoundRoutes":["/gone"],"preview":{}}"#,
        )
        .unwrap();
        // The saved responses: `/team` called `notFound()` (Next 16.4 then
        // writes 404 here only), `/later` has no `.meta` file.
        let app = root.join(".next").join("server").join("app");
        std::fs::create_dir_all(&app).unwrap();
        for (name, meta) in [
            ("index", r#"{"headers":{"x-nextjs-prerender":"1"}}"#),
            ("board", r#"{"status":200,"headers":{}}"#),
            ("calendar", r#"{"status":404,"headers":{}}"#),
            ("archive", r#"{"headers":{"location":"/board"}}"#),
            ("_not-found", r#"{"status":404}"#),
            ("team", r#"{"status":404}"#),
            ("old", "{}"),
            ("moved", "{}"),
            ("gone", "{}"),
        ] {
            std::fs::write(app.join(format!("{name}.meta")), meta).unwrap();
        }
        let routes = super::prerendered_routes(&root);
        std::fs::remove_dir_all(&root).ok();
        assert!(routes.contains("/"));
        assert!(routes.contains("/board"));
        // Prerendered with another status, or as a redirect, or with no saved
        // response: still requested.
        for requested in [
            "/_not-found",
            "/team",
            "/old",
            "/moved",
            "/gone",
            "/calendar",
            "/archive",
            "/later",
        ] {
            assert!(!routes.contains(requested), "{requested}");
        }
        assert_eq!(super::route_of("app/page.tsx").as_deref(), Some("/"));
        assert_eq!(
            super::route_of("app/(main)/board/page.tsx").as_deref(),
            Some("/board")
        );
        assert!(!routes.contains("/search"));
    }

    use super::*;

    #[test]
    fn vitest_output_becomes_failed_blocks_and_totals() {
        let out = " ✓ tests/a.test.ts > adds 1ms\n × tests/a.test.ts > removes 3ms\n   AssertionError: expected 1 to be 2\n ✓ tests/b.test.ts > lists 1ms\n\n Test Files  1 failed | 1 passed (2)\n      Tests  1 failed | 2 passed (3)\n";
        let (lines, failed, passed) = vitest_report(out);
        assert_eq!(
            lines,
            vec![
                "FAILED tests/a.test.ts > removes 3ms".to_string(),
                "    AssertionError: expected 1 to be 2".to_string(),
            ]
        );
        assert_eq!((failed, passed), (1, 2));
        assert_eq!(declared_type("export interface Task {"), Some("Task"));
        assert_eq!(declared_type("  type Status = 'a' | 'b';"), Some("Status"));
        assert_eq!(declared_type("const type = 1;"), None);
        let owned = vec![("Task".to_string(), "lib/types.ts".to_string())];
        let copy = "import x from 'y';\nexport interface Task { id: string }\ninterface Other {}\n";
        let found = duplicate_types("lib/tasks.ts", copy, &owned);
        assert_eq!(found.len(), 1);
        assert!(found[0].starts_with("WARN lib/tasks.ts(2,1): error TS9003"));
        assert!(duplicate_types("lib/types.ts", copy, &owned).is_empty());
        let file = |path: &str, content: &str| extract::Found {
            path: path.to_string(),
            content: content.to_string(),
            closed: true,
        };
        let mut files = vec![
            file(
                "app/layout.tsx",
                "export default function Layout({ children }) { return children; }",
            ),
            file(
                "components/StoreProvider.tsx",
                "export function StoreProvider() {}",
            ),
            file(
                "app/page.tsx",
                "import { useStore } from '../components/StoreProvider';",
            ),
        ];
        let found = unmounted_providers(&files);
        assert_eq!(found.len(), 1);
        assert!(found[0].starts_with("ERROR app/layout.tsx(1,1): error TS9004"));
        files[0].content = "<StoreProvider>{children}</StoreProvider>".to_string();
        assert!(unmounted_providers(&files).is_empty());
        files.push(file(
            "components/AppShell.tsx",
            "export default function AppShell() {}",
        ));
        assert_eq!(
            project_path(".check/keep-70716-1-d/app/layout.tsx"),
            "app/layout.tsx"
        );
        assert_eq!(project_path("app/layout.tsx"), "app/layout.tsx");
        assert_eq!(
            file_header("./.check/keep-1-d/app/layout.tsx:13:14"),
            Some(("app/layout.tsx".to_string(), 13, 14))
        );
        let found = unrendered_shell(&files);
        assert_eq!(found.len(), 1);
        assert!(found[0].starts_with("ERROR app/layout.tsx(1,1): error TS9005"));
        files[0].content = "<AppShell>{children}</AppShell>".to_string();
        assert!(unrendered_shell(&files).is_empty());
        files.pop();
        assert_eq!(route_of("app/page.tsx").as_deref(), Some("/"));
        assert_eq!(
            route_of("app/(main)/board/page.tsx").as_deref(),
            Some("/board")
        );
        assert_eq!(route_of("app/board/Board.tsx"), None);
        assert_eq!(
            file_header("./app/tasks/[id]/page.tsx:3:1"),
            Some(("app/tasks/[id]/page.tsx".to_string(), 3, 1))
        );
        let paths = vec!["app/board/page.tsx".to_string()];
        let out = "./app/x.tsx:33:24\nType error: Property 'a' does not exist.\n\n  31 | code\n";
        let found = build_errors(out, &paths);
        assert_eq!(found.len(), 1);
        assert!(found[0].starts_with("ERROR app/x.tsx(33,24): error TS9100"));
        let out = "⨯ useSearchParams() should be wrapped in a suspense boundary\nError occurred prerendering page \"/board\". Read more\n";
        let found = build_errors(out, &paths);
        assert!(found[0].starts_with("ERROR app/board/page.tsx(1,1): error TS9100"));
        assert!(found[0].contains("suspense boundary"));
        assert!(build_errors("something else", &paths)[0].starts_with("ERROR gate:"));
        assert!(needs_client("const [a, b] = useState(0);"));
        assert!(needs_client("<button onClick={go}>"));
        assert!(needs_client("<CalendarGrid days={days} onNext={next} />"));
        assert!(!needs_client("const once = 1; // based on={x}"));
        assert!(!needs_client(
            "export default function Page() { return <p>because(1)</p>; }"
        ));
        assert!(is_client("// note\n'use client';\nimport x from 'y';"));
        assert_eq!(
            next_lints("app/board/page.tsx", "const q = useSearchParams();").len(),
            2
        );
        assert!(next_lints("lib/x.ts", "useState(1)").is_empty());
        let clean = " ✓ tests/a.test.ts > adds 1ms\n      Tests  5 passed (5)\n";
        assert_eq!(vitest_report(clean), (Vec::new(), 0, 5));
    }
}
