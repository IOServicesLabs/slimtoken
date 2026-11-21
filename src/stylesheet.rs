//! One style sheet after the wave: a planned style sheet that only imports
//! other files of the plan (the file that `architect::split_files` left to
//! join its parts, rendered by `Staging::aggregate`) is replaced with the
//! text of those files, in import order, and the parts leave the plan and
//! the report. The parts are still written at the same time by their own
//! workers; only what is checked, repaired and delivered changes: one style
//! sheet, as an objective that asks for one global style sheet expects. No
//! model call.

use std::collections::{BTreeMap, BTreeSet};

use crate::dag::Plan;
use crate::project::file_text;
use crate::scheduler::{RunReport, TaskStatus};

/// The line of a file task's prompt that names the file (`architect`).
const WRITE: &str = "Now write the COMPLETE contents of `";
/// The exports line of a file task's prompt (`architect`).
const EXPORTS: &str = "\n- Exports (implement exactly these): ";

/// Join each rendered style sheet of `plan` that only imports other files of
/// the plan (`@import "./styles/part1.css";` lines) with those files: when
/// every imported file is a task of the plan that succeeded in `report`,
/// the style sheet's record gets their text in import order (their
/// `@import` and `@charset` lines first, once each) and their token counts,
/// its task becomes a normal file task (no `static_output`, a prompt for the
/// whole file, so a repair can change it), the imported tasks and records
/// are removed, deps on them point to the style sheet, and a rendered
/// README stops listing them. Returns the paths of the joined style sheets.
/// A style sheet with a failed or missing part is left as it is.
pub fn join_imported_stylesheets(plan: &mut Plan, report: &mut RunReport) -> Vec<String> {
    let mut joined = Vec::new();
    let candidates: Vec<String> = plan
        .tasks
        .iter()
        .filter(|task| !task.internal && task.title.ends_with(".css"))
        .filter(|task| task.static_output.is_some())
        .map(|task| task.id.clone())
        .collect();
    for id in candidates {
        if let Some(path) = join_one(plan, report, &id) {
            joined.push(path);
        }
    }
    joined
}

/// [`join_imported_stylesheets`] for the task `id`.
fn join_one(plan: &mut Plan, report: &mut RunReport, id: &str) -> Option<String> {
    let at = plan.tasks.iter().position(|task| task.id == id)?;
    let path = plan.tasks[at].title.clone();
    let rendered = plan.tasks[at].static_output.clone()?;
    let imported = import_only(&path, &file_text(&path, &rendered))?;
    if report.tasks.get(id)?.status != TaskStatus::Succeeded {
        return None;
    }
    // Each imported file: a model-written task of the plan, once, that
    // succeeded with text.
    let mut parts = Vec::new();
    let mut seen = BTreeSet::new();
    for file in &imported {
        let index = plan.tasks.iter().position(|task| {
            task.title == *file && !task.internal && task.static_output.is_none() && task.id != id
        })?;
        let task = &plan.tasks[index];
        if !seen.insert(task.id.clone()) {
            return None;
        }
        let record = report.tasks.get(&task.id)?;
        let output = record
            .output
            .as_deref()
            .filter(|text| !text.trim().is_empty())?;
        if record.status != TaskStatus::Succeeded {
            return None;
        }
        parts.push((index, file_text(&task.title, output)));
    }
    let text = join_texts(parts.iter().map(|(_, text)| text.as_str()));
    let indices: Vec<usize> = parts.iter().map(|(index, _)| *index).collect();
    let part_ids: BTreeSet<String> = indices
        .iter()
        .map(|&index| plan.tasks[index].id.clone())
        .collect();

    // The records: the parts' text and bill go to the style sheet.
    let removed: Vec<_> = part_ids
        .iter()
        .filter_map(|part| report.tasks.remove(part))
        .collect();
    if let Some(record) = report.tasks.get_mut(id) {
        record.output = Some(format!("```css\n{}\n```", text.trim_end()));
        for part in &removed {
            record.attempts += part.attempts;
            record.duration_ms = record.duration_ms.max(part.duration_ms);
            record.input_tokens += part.input_tokens;
            record.output_tokens += part.output_tokens;
            record.wasted_input_tokens += part.wasted_input_tokens;
            record.wasted_output_tokens += part.wasted_output_tokens;
            record.reasoning_tokens = add(record.reasoning_tokens, part.reasoning_tokens);
            record.cached_input_tokens = add(record.cached_input_tokens, part.cached_input_tokens);
            record.usage_is_estimated |= part.usage_is_estimated;
            record.rate_limited |= part.rate_limited;
            if record.provider.is_none() {
                record.provider = part.provider.clone();
            }
            if record.finish_reason.is_none() {
                record.finish_reason = part.finish_reason.clone();
            }
        }
    }
    join_plan(plan, Some(report), at, &indices, &imported);
    Some(path)
}

/// The join of [`join_imported_stylesheets`] for a plan rebuilt from its
/// manifest after the run (a re-repair from the files on disk): each
/// rendered style sheet that only imports model-written tasks of the plan,
/// none of which `exists` on disk (the delivered app holds the joined
/// sheet), becomes the whole-sheet file task, and its parts leave the plan.
/// Returns the paths of the joined style sheets.
pub fn join_planned_stylesheets(plan: &mut Plan, exists: impl Fn(&str) -> bool) -> Vec<String> {
    let candidates: Vec<String> = plan
        .tasks
        .iter()
        .filter(|task| !task.internal && task.title.ends_with(".css"))
        .filter(|task| task.static_output.is_some())
        .map(|task| task.id.clone())
        .collect();
    let mut joined = Vec::new();
    for id in candidates {
        let Some(at) = plan.tasks.iter().position(|task| task.id == id) else {
            continue;
        };
        let path = plan.tasks[at].title.clone();
        let rendered = plan.tasks[at].static_output.clone().unwrap_or_default();
        let Some(imported) = import_only(&path, &file_text(&path, &rendered)) else {
            continue;
        };
        let mut indices = Vec::new();
        for file in &imported {
            let found = plan.tasks.iter().position(|task| {
                task.title == *file
                    && !task.internal
                    && task.static_output.is_none()
                    && task.id != id
            });
            match found {
                Some(index) if !exists(file) && !indices.contains(&index) => indices.push(index),
                _ => {
                    indices.clear();
                    break;
                }
            }
        }
        if indices.is_empty() {
            continue;
        }
        join_plan(plan, None, at, &indices, &imported);
        joined.push(path);
    }
    joined
}

/// The plan side of a join: the style sheet's task at `at` takes the prompt
/// for the whole sheet from its parts (at `parts`, in import order), the
/// part tasks leave the plan, deps on them point to the sheet, and a
/// rendered README (and its record in `report`) stops listing them.
fn join_plan(
    plan: &mut Plan,
    report: Option<&mut RunReport>,
    at: usize,
    parts: &[usize],
    imported: &[String],
) {
    let id = plan.tasks[at].id.clone();
    let id = id.as_str();
    let path = plan.tasks[at].title.clone();
    // The style sheet's task: written by a model from now on (a repair).
    let first = plan.tasks[parts[0]].clone();
    let prompts: Vec<&str> = parts
        .iter()
        .map(|&index| plan.tasks[index].prompt.as_str())
        .collect();
    let (prompt, prefix) = joined_prompt(&path, &first.title, &prompts, first.prefix_split);
    let max_tokens = parts
        .iter()
        .map(|&index| plan.tasks[index].max_tokens)
        .sum::<u32>()
        .max(plan.tasks[at].max_tokens);
    let part_ids: BTreeSet<String> = parts
        .iter()
        .map(|&index| plan.tasks[index].id.clone())
        .collect();
    {
        let task = &mut plan.tasks[at];
        task.static_output = None;
        task.prompt = prompt;
        task.prefix_split = prefix;
        task.max_tokens = max_tokens;
        task.temperature = first.temperature;
        task.reasoning_effort = first.reasoning_effort.clone();
        task.validate_artifact = first.validate_artifact;
        task.require_closed_fences = true;
        task.deps.retain(|dep| !part_ids.contains(dep));
    }

    // The plan: no part tasks; what waited for a part waits for the sheet.
    plan.tasks.retain(|task| !part_ids.contains(&task.id));
    for task in plan.tasks.iter_mut().filter(|task| task.id != id) {
        if task.deps.iter().any(|dep| part_ids.contains(dep)) {
            task.deps.retain(|dep| !part_ids.contains(dep));
            if !task.deps.iter().any(|dep| dep == id) {
                task.deps.push(id.to_string());
            }
        }
    }

    // A rendered README lists the files of the plan.
    let titles: BTreeMap<String, String> = plan
        .tasks
        .iter()
        .filter(|task| task.title.ends_with(".md") && task.static_output.is_some())
        .map(|task| (task.id.clone(), task.title.clone()))
        .collect();
    let mut report = report;
    for (readme, _) in titles {
        let edit = |text: &str| readme_without_parts(text, &path, imported);
        if let Some(task) = plan.tasks.iter_mut().find(|task| task.id == readme) {
            if let Some(text) = task.static_output.as_deref().map(edit) {
                task.static_output = Some(text);
            }
        }
        if let Some(record) = report
            .as_deref_mut()
            .and_then(|report| report.tasks.get_mut(&readme))
        {
            if let Some(text) = record.output.as_deref().map(edit) {
                record.output = Some(text);
            }
        }
    }
}

fn add(a: Option<u64>, b: Option<u64>) -> Option<u64> {
    match (a, b) {
        (None, None) => None,
        (a, b) => Some(a.unwrap_or(0) + b.unwrap_or(0)),
    }
}

/// The files that the style sheet `path` with the text `text` imports,
/// resolved against its directory, when every line of it that is not blank
/// is an `@import` of a relative path (`"./x.css"`, `'x.css'`,
/// `url("./x.css")`, without a media query); `None` otherwise or when it
/// imports nothing.
pub fn import_only(path: &str, text: &str) -> Option<Vec<String>> {
    let dir = path.rsplit_once('/').map_or("", |(dir, _)| dir);
    let mut files = Vec::new();
    for line in text.lines().map(str::trim).filter(|line| !line.is_empty()) {
        let rest = line.strip_prefix("@import")?.trim();
        let rest = rest.strip_suffix(';')?.trim();
        let rest = match rest.strip_prefix("url(") {
            Some(inner) => inner.strip_suffix(')')?.trim(),
            None => rest,
        };
        let quote = rest.chars().next()?;
        if quote != '"' && quote != '\'' {
            return None;
        }
        let spec = rest.strip_prefix(quote)?.strip_suffix(quote)?;
        if spec.contains(quote) || spec.contains(':') || spec.starts_with('/') {
            return None;
        }
        files.push(resolve(dir, spec)?);
    }
    (!files.is_empty()).then_some(files)
}

/// `spec` relative to the directory `dir`; `None` above the root.
fn resolve(dir: &str, spec: &str) -> Option<String> {
    let mut parts: Vec<&str> = dir.split('/').filter(|part| !part.is_empty()).collect();
    for part in spec.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            part => parts.push(part),
        }
    }
    (!parts.is_empty()).then(|| parts.join("/"))
}

/// The texts of the parts as one style sheet: their `@charset` and
/// `@import` lines first (once each; CSS ignores them after a rule), then
/// the rest of each part in order, a blank line between parts.
pub fn join_texts<'a>(texts: impl IntoIterator<Item = &'a str>) -> String {
    let mut charset: Option<String> = None;
    let mut imports: Vec<String> = Vec::new();
    let mut bodies: Vec<String> = Vec::new();
    for text in texts {
        let mut body = Vec::new();
        for line in text.lines() {
            let trimmed = line.trim();
            let statement = trimmed.ends_with(';');
            if statement && trimmed.starts_with("@charset") {
                charset.get_or_insert_with(|| trimmed.to_string());
            } else if statement && trimmed.starts_with("@import") {
                if !imports.iter().any(|seen| seen == trimmed) {
                    imports.push(trimmed.to_string());
                }
            } else {
                body.push(line);
            }
        }
        let body = body.join("\n");
        let body = body.trim_matches('\n').trim_end();
        if !body.is_empty() {
            bodies.push(body.to_string());
        }
    }
    let mut head: Vec<String> = charset.into_iter().collect();
    head.extend(imports);
    let mut text = String::new();
    if !head.is_empty() {
        text.push_str(&head.join("\n"));
        text.push_str("\n\n");
    }
    text.push_str(&bodies.join("\n\n"));
    text.push('\n');
    text
}

/// The prompt of the joined style sheet `path` (for a repair), from the
/// prompts of its parts (`first` is the path of the first): the first
/// part's head and rules, this path, and the exports of every part. With a
/// prompt in another shape, the first part's prompt with this path.
/// Returns the prompt and its `prefix_split`.
fn joined_prompt(
    path: &str,
    first: &str,
    prompts: &[&str],
    prefix: Option<usize>,
) -> (String, Option<usize>) {
    let fallback = || {
        (
            prompts[0].replace(&format!("`{first}`"), &format!("`{path}`")),
            None,
        )
    };
    let Some(at) = prompts[0].find(WRITE) else {
        return fallback();
    };
    let Some(spans) = prompts
        .iter()
        .map(|prompt| exports_span(prompt))
        .collect::<Option<Vec<_>>>()
    else {
        return fallback();
    };
    let exports: Vec<&str> = prompts
        .iter()
        .zip(&spans)
        .map(|(prompt, &(start, end))| prompt[start..end].trim())
        .filter(|exports| !exports.is_empty())
        .collect();
    let tail = &prompts[0][spans[0].1..];
    let prompt = format!(
        "{head}{WRITE}{path}` — ONLY that file.\n- Purpose: The whole style sheet in one file: \
         the text of its {count} parts joined in order (the first part's reset, base styles and \
         variables first).{EXPORTS}{exports}{tail}",
        head = &prompts[0][..at],
        count = prompts.len(),
        exports = exports.join(", "),
    );
    (prompt, prefix.filter(|&split| split <= at))
}

/// The byte span of the exports of a file task's prompt (to the end of its
/// line).
fn exports_span(prompt: &str) -> Option<(usize, usize)> {
    let start = prompt.find(EXPORTS)? + EXPORTS.len();
    let end = prompt[start..]
        .find('\n')
        .map_or(prompt.len(), |at| start + at);
    Some((start, end))
}

/// A rendered README's text without the list lines of the joined files
/// (``- `path` ``), and the style sheet's line with the purpose of the whole
/// (from the first part's line, "Part 1 of 3 of the global style sheet."
/// gives "The global style sheet.").
fn readme_without_parts(text: &str, path: &str, parts: &[String]) -> String {
    let item = |file: &str| format!("- `{file}`");
    let listed = |line: &str, file: &str| {
        let head = item(file);
        line.strip_prefix(&head)
            .is_some_and(|rest| rest.is_empty() || rest.starts_with(' '))
    };
    let whole = text
        .lines()
        .find(|line| listed(line, &parts[0]))
        .and_then(|line| line.split_once(" — "))
        .and_then(|(_, purpose)| {
            let rest = purpose.strip_prefix("Part 1 of ")?;
            let (_, rest) = rest.split_once(" of ")?;
            let mut chars = rest.chars();
            let first = chars.next()?;
            Some(first.to_uppercase().chain(chars).collect::<String>())
        });
    let mut lines = Vec::new();
    for line in text.split('\n') {
        if parts.iter().any(|part| listed(line, part)) {
            continue;
        }
        if listed(line, path) {
            match &whole {
                Some(purpose) => lines.push(format!("{} — {purpose}", item(path))),
                None => lines.push(item(path)),
            }
            continue;
        }
        lines.push(line.to_string());
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dag::Task;
    use crate::scheduler::{PhaseBreakdown, TaskRecord};

    fn task(id: &str, title: &str, prompt: &str) -> Task {
        Task {
            id: id.into(),
            title: title.into(),
            prompt: prompt.into(),
            max_tokens: 4_096,
            ..Task::default()
        }
    }

    fn record(id: &str, title: &str, output: &str, tokens: u64) -> TaskRecord {
        TaskRecord {
            id: id.into(),
            title: title.into(),
            status: TaskStatus::Succeeded,
            output: Some(output.into()),
            error: None,
            attempts: 1,
            duration_ms: 10 * tokens,
            input_tokens: tokens,
            output_tokens: tokens,
            reasoning_tokens: None,
            cached_input_tokens: None,
            provider: None,
            rate_limited: false,
            finish_reason: Some("stop".into()),
            usage_is_estimated: false,
            time_to_first_token_ms: None,
            failure_class: None,
            wasted_input_tokens: 0,
            wasted_output_tokens: 0,
        }
    }

    fn report(records: Vec<TaskRecord>) -> RunReport {
        RunReport {
            objective: "o".into(),
            steering: Vec::new(),
            elapsed_ms: 0,
            phases: PhaseBreakdown::default(),
            task_count: records.len(),
            peak_concurrency: 1,
            total_input_tokens: 0,
            total_output_tokens: 0,
            total_reasoning_tokens: None,
            total_cached_input_tokens: None,
            total_wasted_input_tokens: 0,
            total_wasted_output_tokens: 0,
            mean_time_to_first_token_ms: None,
            aggregate_output_tokens_per_second: 0.0,
            aggregate_total_tokens_per_second: 0.0,
            concurrency_curve: Vec::new(),
            controller_stats: None,
            rate_tune: None,
            usage_is_estimated: false,
            tasks: records
                .into_iter()
                .map(|record| (record.id.clone(), record))
                .collect(),
        }
    }

    const IMPORTS: &str =
        "```css\n@import \"./styles/part1.css\";\n@import \"./styles/part2.css\";\n```";
    const README: &str = "```markdown\n# App\n\n## Files\n\n- `app/page.tsx` — The home page.\n- `app/globals.css` — Imports the parts of the global style sheet.\n- `app/styles/part1.css` — Part 1 of 2 of the global style sheet.\n- `app/styles/part2.css` — Part 2 of 2 of the global style sheet.\n```";

    fn part_prompt(path: &str, exports: &str) -> String {
        format!(
            "HEAD\n{WRITE}{path}` — ONLY that file.\n- Purpose: a part.{EXPORTS}{exports}\n\nRULES"
        )
    }

    /// A split style sheet after the wave: two parts, a page that waited for
    /// them and a rendered README.
    fn wave() -> (Plan, RunReport) {
        let mut sheet = task("css", "app/globals.css", "write it");
        sheet.static_output = Some(IMPORTS.into());
        let mut readme = task("readme", "README.md", "readme");
        readme.static_output = Some(README.into());
        let mut page = task("page", "app/page.tsx", "page");
        page.deps = vec!["p1".into(), "p2".into()];
        let mut p1 = task(
            "p1",
            "app/styles/part1.css",
            &part_prompt("app/styles/part1.css", ".a, .b"),
        );
        p1.prefix_split = Some(5);
        let p2 = task(
            "p2",
            "app/styles/part2.css",
            &part_prompt("app/styles/part2.css", ".c"),
        );
        let plan = Plan {
            objective: "o".into(),
            tasks: vec![page, sheet, p1, p2, readme],
            cache_primes: Vec::new(),
        };
        let mut css = record("css", "app/globals.css", IMPORTS, 0);
        css.attempts = 0;
        let report = report(vec![
            record("page", "app/page.tsx", "```tsx\nexport {};\n```", 5),
            css,
            record(
                "p1",
                "app/styles/part1.css",
                "```css\n@import url(\"https://fonts.example/a.css\");\n:root { --bg: #fff; }\n.a { color: red; }\n```",
                3,
            ),
            record(
                "p2",
                "app/styles/part2.css",
                "```css\n@import url(\"https://fonts.example/a.css\");\n.c { color: blue; }\n```",
                4,
            ),
            record("readme", "README.md", README, 0),
        ]);
        (plan, report)
    }

    #[test]
    fn an_import_only_style_sheet_becomes_one_file() {
        let (mut plan, mut report) = wave();
        assert_eq!(
            join_imported_stylesheets(&mut plan, &mut report),
            ["app/globals.css"]
        );
        let titles: Vec<&str> = plan.tasks.iter().map(|task| task.title.as_str()).collect();
        assert_eq!(titles, ["app/page.tsx", "app/globals.css", "README.md"]);
        assert_eq!(
            report.tasks.keys().collect::<Vec<_>>(),
            ["css", "page", "readme"]
        );
        let files = crate::project::report_files(&report);
        assert_eq!(
            files["app/globals.css"],
            "@import url(\"https://fonts.example/a.css\");\n\n:root { --bg: #fff; }\n.a { color: red; }\n\n.c { color: blue; }\n"
        );
        assert!(!files.keys().any(|path| path.contains("styles/part")));
        // The bill of the parts stays in the report.
        let css = &report.tasks["css"];
        assert_eq!(
            (css.input_tokens, css.output_tokens, css.attempts),
            (7, 7, 2)
        );
        assert_eq!(css.duration_ms, 40);
        // A repair can change the joined file: a normal task with every export.
        let sheet = &plan.tasks[1];
        assert!(sheet.static_output.is_none());
        assert!(sheet.prompt.starts_with("HEAD\n"));
        assert!(sheet.prompt.contains("`app/globals.css` — ONLY that file."));
        assert!(sheet
            .prompt
            .contains(&format!("{EXPORTS}.a, .b, .c\n\nRULES")));
        assert!(!sheet.prompt.contains("part1"));
        assert_eq!(sheet.prefix_split, Some(5));
        assert_eq!(sheet.max_tokens, 8_192);
        // What waited for a part waits for the style sheet.
        assert_eq!(plan.tasks[0].deps, ["css"]);
        plan.validate().unwrap();
        // The README lists one style sheet.
        let readme = &files["README.md"];
        assert!(readme.contains("- `app/globals.css` — The global style sheet.\n"));
        assert!(!readme.contains("part1") && !readme.contains("Imports the parts"));
        assert!(readme.contains("- `app/page.tsx` — The home page."));
        // Idempotent: nothing is left to join.
        assert!(join_imported_stylesheets(&mut plan, &mut report).is_empty());
    }

    #[test]
    fn a_failed_part_or_a_real_style_sheet_is_left_as_it_is() {
        let (mut plan, mut report) = wave();
        report.tasks.get_mut("p2").unwrap().status = TaskStatus::Failed;
        let before = serde_json::to_string(&plan).unwrap();
        assert!(join_imported_stylesheets(&mut plan, &mut report).is_empty());
        assert_eq!(serde_json::to_string(&plan).unwrap(), before);
        assert_eq!(report.tasks.len(), 5);

        // A rendered style sheet with rules of its own is not import-only.
        assert_eq!(
            import_only("app/globals.css", "@import \"./a.css\";\n.x {}\n"),
            None
        );
        // Remote and media-query imports are not parts.
        assert_eq!(
            import_only("app/globals.css", "@import url(\"https://x/a.css\");"),
            None
        );
        assert_eq!(
            import_only("app/globals.css", "@import \"./a.css\" screen;"),
            None
        );
        assert_eq!(import_only("app/globals.css", ""), None);
        assert_eq!(
            import_only(
                "app/globals.css",
                "@import './styles/p1.css';\n\n@import url(\"../x/p2.css\");\n"
            ),
            Some(vec![
                "app/styles/p1.css".to_string(),
                "x/p2.css".to_string()
            ])
        );
        assert_eq!(
            import_only("globals.css", "@import \"./styles/part1.css\";"),
            Some(vec!["styles/part1.css".to_string()])
        );
        assert_eq!(import_only("globals.css", "@import \"../a.css\";"), None);
    }

    /// A re-repair rebuilds the plan from the manifest: the joined sheet on
    /// disk becomes the whole-sheet model task, its missing parts leave.
    #[test]
    fn a_plan_rebuilt_after_the_join_gets_the_whole_sheet_task() {
        let (mut plan, _) = wave();
        // A part still on disk: the sheet keeps its imports.
        let before = serde_json::to_string(&plan).unwrap();
        assert!(
            join_planned_stylesheets(&mut plan, |path| path == "app/styles/part1.css").is_empty()
        );
        assert_eq!(serde_json::to_string(&plan).unwrap(), before);

        assert_eq!(
            join_planned_stylesheets(&mut plan, |path| !path.contains("styles/part")),
            ["app/globals.css"]
        );
        let titles: Vec<&str> = plan.tasks.iter().map(|task| task.title.as_str()).collect();
        assert_eq!(titles, ["app/page.tsx", "app/globals.css", "README.md"]);
        let sheet = &plan.tasks[1];
        assert!(sheet.static_output.is_none());
        assert!(sheet
            .prompt
            .contains("- Purpose: The whole style sheet in one file"));
        assert!(!sheet.prompt.contains("part1.css"));
        assert_eq!(plan.tasks[0].deps, ["css"]);
        let readme = plan.tasks[2].static_output.as_deref().unwrap();
        assert!(!readme.contains("Imports the parts"), "{readme}");
        plan.validate().unwrap();
    }

    #[test]
    fn join_texts_hoists_imports_and_keeps_part_order() {
        let text = join_texts([
            "@charset \"utf-8\";\n.a {}\n\n",
            "\n@charset \"utf-8\";\n@import \"x.css\";\n.b {}\n",
        ]);
        assert_eq!(
            text,
            "@charset \"utf-8\";\n@import \"x.css\";\n\n.a {}\n\n.b {}\n"
        );
    }

    #[test]
    fn a_prompt_in_another_shape_gets_the_first_part_prompt_with_the_path() {
        let (prompt, prefix) = joined_prompt(
            "app/globals.css",
            "app/styles/part1.css",
            &[
                "Write `app/styles/part1.css`.",
                "Write `app/styles/part2.css`.",
            ],
            Some(3),
        );
        assert_eq!(prompt, "Write `app/globals.css`.");
        assert_eq!(prefix, None);
    }
}
