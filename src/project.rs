//! Project files: from a run's outputs to files on disk.
//!
//! A worker of a file task answers with one fenced code block, and a
//! single-answer model writes a markdown document with a heading and a
//! fenced block for each file. [`scan`] finds those blocks, [`file_text`]
//! reads the text of one file from a worker's answer, [`safe_path`] keeps a
//! path inside the output directory and [`write_project`] writes the files
//! of a finished run. [`render_doc`] is the inverse of [`scan`]: the
//! document that a check command reads.
//!
//! [`apply_edit_blocks`] and [`EDIT_FORMAT`] are the search/replace edit
//! protocol of a repair that changes a few lines of a long file.
//!
//! Ported from `bench/showdown` (`extract.rs`, `arms::file_text`), where
//! the functions were measured; the behaviour is the same.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result};
use serde::Serialize;

use crate::dag::Plan;
use crate::scheduler::{RunReport, TaskStatus};

/// One file block that [`scan`] found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    pub path: String,
    pub content: String,
    /// False while the closing fence has not arrived.
    pub closed: bool,
}

const SHELL_LANGS: &[&str] = &[
    "bash",
    "sh",
    "shell",
    "console",
    "zsh",
    "powershell",
    "ps1",
    "cmd",
    "bat",
    "terminal",
];
const SHELL_EXTS: &[&str] = &["sh", "bash", "zsh", "ps1", "bat", "cmd"];
const BARE_NAMES: &[&str] = &[
    "Dockerfile",
    "Makefile",
    "Procfile",
    "LICENSE",
    ".gitignore",
    ".dockerignore",
    ".npmrc",
    ".nvmrc",
    ".env.example",
    ".eslintrc",
    ".prettierrc",
];

/// Clean a path and reject one that leaves the output directory: a `..`
/// segment, a drive or scheme (`C:`, anything with `:`), an empty segment,
/// a directory (trailing `/`) or more than 200 bytes. Backslashes become
/// `/`; a leading `./` or `/` is dropped, so a rooted path stays inside the
/// output directory (`/abs/a.ts` is `abs/a.ts`).
pub fn safe_path(raw: &str) -> Option<String> {
    let path = raw.trim().replace('\\', "/");
    let path = path.trim_start_matches("./").trim_start_matches('/');
    if path.is_empty() || path.len() > 200 || path.contains(':') || path.ends_with('/') {
        return None;
    }
    if path.split('/').any(|part| part == ".." || part.is_empty()) {
        return None;
    }
    Some(path.to_string())
}

/// True for a token that names a file: an extension of 1 to 5 characters
/// with a letter, or a known bare name (`Dockerfile`), and only path
/// characters.
pub fn looks_like_path(token: &str) -> bool {
    if token.is_empty() || token.len() > 200 || token.contains("://") {
        return false;
    }
    if !token.chars().all(|ch| {
        ch.is_ascii_alphanumeric()
            || matches!(
                ch,
                '_' | '.' | '/' | '-' | '[' | ']' | '(' | ')' | '@' | '+'
            )
    }) {
        return false;
    }
    let name = token.rsplit('/').next().unwrap_or(token);
    if BARE_NAMES.contains(&name) {
        return true;
    }
    match name.rsplit_once('.') {
        Some((stem, ext)) => {
            !stem.is_empty()
                && (1..=5).contains(&ext.len())
                && ext.chars().all(|ch| ch.is_ascii_alphanumeric())
                && ext.chars().any(|ch| ch.is_ascii_alphabetic())
        }
        None => false,
    }
}

fn strip_wrappers(token: &str) -> &str {
    token.trim_matches(|ch: char| {
        matches!(
            ch,
            '`' | '*'
                | '_'
                | '"'
                | '\''
                | ':'
                | ','
                | ';'
                | '#'
                | '<'
                | '>'
                | '!'
                | '='
                | '{'
                | '}'
        )
    })
}

/// The path a prose line names, if it names one. Only marked-up lines count
/// (heading, bold, backticks, `File: x`, or a line that is only a path), so
/// a sentence that mentions "Next.js" does not become a file.
fn path_in_line(line: &str) -> Option<String> {
    let trimmed = line.trim();
    if trimmed.is_empty() || trimmed.len() > 160 {
        return None;
    }
    // Backticked spans are the strongest signal.
    let mut best: Option<String> = None;
    for (index, span) in trimmed.split('`').enumerate() {
        if index % 2 == 1 && looks_like_path(span.trim()) {
            best = Some(span.trim().to_string());
        }
    }
    if best.is_some() {
        return best.and_then(|path| safe_path(&path));
    }
    let lower = trimmed.to_lowercase();
    let marked = trimmed.starts_with('#')
        || trimmed.starts_with("**")
        || lower.starts_with("file:")
        || lower.starts_with("filename:")
        || lower.starts_with("path:");
    let tokens: Vec<&str> = trimmed.split_whitespace().map(strip_wrappers).collect();
    let lone = tokens.len() == 1;
    if !marked && !lone {
        return None;
    }
    tokens
        .iter()
        .rev()
        .find(|token| looks_like_path(token))
        // A heading such as "## Next.js setup" names a product, not a file:
        // a path in a heading must hold a separator or be the last token.
        .filter(|token| token.contains('/') || tokens.last() == Some(*token) || lone)
        .and_then(|token| safe_path(token))
}

fn path_in_info(info: &str) -> Option<String> {
    for (index, token) in info.split_whitespace().enumerate() {
        let value = token.split_once('=').map(|(_, value)| value).unwrap_or(
            if index == 0 && !token.contains('/') {
                ""
            } else {
                token
            },
        );
        let value = strip_wrappers(value);
        if looks_like_path(value) {
            return safe_path(value);
        }
    }
    None
}

fn path_in_first_line(line: &str) -> Option<String> {
    let trimmed = line.trim();
    let body = ["//", "#", "/*", "<!--", "--", ";"]
        .iter()
        .find_map(|prefix| trimmed.strip_prefix(prefix))?;
    let body = body
        .trim()
        .trim_end_matches("*/")
        .trim_end_matches("-->")
        .trim();
    let body = body
        .strip_prefix("File:")
        .or_else(|| body.strip_prefix("file:"))
        .unwrap_or(body)
        .trim();
    (body.split_whitespace().count() == 1 && looks_like_path(body))
        .then(|| safe_path(body))
        .flatten()
}

/// The width and info string of a fence line (three or more backticks).
pub(crate) fn fence_of(line: &str) -> Option<(usize, &str)> {
    let trimmed = line.trim_start();
    let ticks = trimmed.chars().take_while(|ch| *ch == '`').count();
    (ticks >= 3).then(|| (ticks, trimmed[ticks..].trim()))
}

/// Find every file block in a markdown document, in document order. A later
/// block with the same path replaces the earlier one in place. A block's
/// path comes from its info string (`ts title="lib/a.ts"`), else from a
/// marked-up prose line just above it (heading, bold, backticks), else from
/// a comment on its first line (`// lib/a.ts`). Shell blocks are run
/// instructions, not files, unless their path is a script; a markdown
/// file keeps the code samples nested in it.
pub fn scan(doc: &str) -> Vec<Found> {
    let mut found: Vec<Found> = Vec::new();
    // Path named by a recent prose line, with the count of non-blank lines
    // seen since (a path goes stale after two lines of other prose).
    let mut candidate: Option<(String, usize)> = None;
    struct Block {
        path: Option<String>,
        width: usize,
        content: String,
        /// Fence widths of the code samples open inside a markdown file.
        nested: Vec<usize>,
        first_seen: bool,
        skip: bool,
    }
    let mut open: Option<Block> = None;

    fn push(found: &mut Vec<Found>, path: String, content: String, closed: bool) {
        let entry = Found {
            path,
            content,
            closed,
        };
        match found.iter_mut().find(|item| item.path == entry.path) {
            Some(existing) => *existing = entry,
            None => found.push(entry),
        }
    }

    for line in doc.lines() {
        if let Some(block) = open.as_mut() {
            let mut closes = false;
            if let Some((ticks, info)) = fence_of(line) {
                let is_markdown = block
                    .path
                    .as_deref()
                    .is_some_and(|path| path.ends_with(".md"));
                // A bare fence at least as wide as the opening one closes
                // the block, unless a code sample of a markdown file is
                // open that is as wide (models nest ``` in ```). A fence
                // wider than every open sample closes it anyway, so a
                // README with a sample that never closes cannot swallow
                // the files after it when its fence is wider.
                if info.is_empty()
                    && ticks >= block.width
                    && block.nested.iter().all(|width| ticks > *width)
                {
                    closes = true;
                } else if is_markdown && !info.is_empty() {
                    // A code sample inside a markdown file.
                    block.nested.push(ticks);
                } else if info.is_empty() {
                    block.nested.pop();
                }
            }
            if closes {
                let block = open.take().expect("open block");
                if let (Some(path), false) = (block.path, block.skip) {
                    push(&mut found, path, block.content, true);
                }
                candidate = None;
                continue;
            }
            if !block.first_seen {
                block.first_seen = true;
                if block.path.is_none() && !block.skip {
                    block.path = path_in_first_line(line);
                }
            }
            block.content.push_str(line);
            block.content.push('\n');
            continue;
        }

        if let Some((ticks, info)) = fence_of(line) {
            let lang = info
                .split_whitespace()
                .next()
                .unwrap_or("")
                .to_ascii_lowercase();
            let path = path_in_info(info).or_else(|| candidate.take().map(|(path, _)| path));
            // A shell block is a run instruction unless its path is a script.
            let skip = SHELL_LANGS.contains(&lang.as_str())
                && !path.as_deref().is_some_and(|path| {
                    SHELL_EXTS.contains(&path.rsplit('.').next().unwrap_or(""))
                });
            open = Some(Block {
                path,
                width: ticks,
                content: String::new(),
                nested: Vec::new(),
                first_seen: false,
                skip,
            });
            continue;
        }

        if line.trim().is_empty() {
            continue;
        }
        match path_in_line(line) {
            Some(path) => candidate = Some((path, 0)),
            None => {
                if let Some((_, age)) = candidate.as_mut() {
                    *age += 1;
                    if *age > 2 {
                        candidate = None;
                    }
                }
            }
        }
    }

    if let Some(block) = open {
        if let (Some(path), false) = (block.path, block.skip) {
            push(&mut found, path, block.content, false);
        }
    }
    found
}

/// A wave worker returns exactly one fenced block. Return its content
/// (with a final newline); text with no opening fence is returned whole.
pub fn strip_fence(output: &str) -> String {
    let trimmed = output.trim();
    let Some((width, _)) = fence_of(trimmed.lines().next().unwrap_or("")) else {
        return format!("{trimmed}\n");
    };
    let mut lines: Vec<&str> = trimmed.lines().skip(1).collect();
    if lines
        .last()
        .and_then(|line| fence_of(line))
        .is_some_and(|(ticks, info)| ticks >= width && info.is_empty())
    {
        lines.pop();
    }
    let mut content = lines.join("\n");
    content.push('\n');
    content
}

/// The text of the file `path` in a worker's answer, read the same way as a
/// check command reads the assembled document ([`render_doc`], [`scan`]): if
/// the answer has more than one code block, the block that `scan` gives
/// for `path` is the file. What goes to disk is thus exactly what the check
/// read. Falls back to [`strip_fence`].
pub fn file_text(path: &str, output: &str) -> String {
    let doc = format!("## {path}\n\n{}\n", output.trim());
    scan(&doc)
        .into_iter()
        .find(|file| file.path == path && file.closed)
        .map(|file| file.content)
        .unwrap_or_else(|| strip_fence(output))
}

/// `content` in a plain fenced block, the form a worker's answer has.
pub fn fenced(content: &str) -> String {
    format!("```\n{}\n```", content.trim_end())
}

/// The markdown document of `files` (path to text) that [`scan`] reads back
/// to the same files: a ``## `path` `` heading and a fenced block for each,
/// in path order. A fence is wider than every backtick run that starts a
/// line of the file, so a file that holds fences (a README, also one with a
/// code sample that never closes) stays one block. A path that
/// [`looks_like_path`] rejects (one with a space) does not read back as
/// itself.
pub fn render_doc(files: &BTreeMap<String, String>) -> String {
    let mut doc = String::new();
    for (path, content) in files {
        let widest = content
            .lines()
            .filter_map(fence_of)
            .map(|(ticks, _)| ticks)
            .max()
            .unwrap_or(0);
        let fence = "`".repeat(widest.max(2) + 1);
        doc.push_str("## `");
        doc.push_str(path);
        doc.push('`');
        doc.push_str("\n\n");
        doc.push_str(&fence);
        doc.push('\n');
        doc.push_str(content);
        if !content.is_empty() && !content.ends_with('\n') {
            doc.push('\n');
        }
        doc.push_str(&fence);
        doc.push_str("\n\n");
    }
    doc
}

/// The files of a run: the text ([`file_text`]) of each succeeded task with
/// output, by task title. Every task of the report counts, so a check sees
/// what the report holds (also a file that a repair added).
pub fn report_files(report: &RunReport) -> BTreeMap<String, String> {
    report_files_where(report, |_| true)
}

/// [`report_files`] for the titles that `keep` accepts.
pub fn report_files_where(
    report: &RunReport,
    keep: impl Fn(&str) -> bool,
) -> BTreeMap<String, String> {
    report
        .tasks
        .values()
        .filter(|record| record.status == TaskStatus::Succeeded && keep(&record.title))
        .filter_map(|record| {
            let output = record.output.as_deref()?;
            Some((record.title.clone(), file_text(&record.title, output)))
        })
        .collect()
}

/// One file that [`write_project`] wrote.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WrittenFile {
    pub path: String,
    pub lines: usize,
    pub bytes: usize,
}

/// Write the files of a finished run to `dir`: for each task of `plan`
/// that is not internal, succeeded with a non-blank output and has a title
/// that is a file path ([`looks_like_path`], [`safe_path`]), the file's text
/// ([`file_text`]). Tasks whose title is not a path (a general plan's
/// "Core types") are skipped. Directories are created as needed.
pub fn write_project(report: &RunReport, plan: &Plan, dir: &Path) -> Result<Vec<WrittenFile>> {
    let mut written = Vec::new();
    for task in plan.tasks.iter().filter(|task| !task.internal) {
        let Some(record) = report.tasks.get(&task.id) else {
            continue;
        };
        if record.status != TaskStatus::Succeeded {
            continue;
        }
        let Some(output) = record
            .output
            .as_deref()
            .filter(|text| !text.trim().is_empty())
        else {
            continue;
        };
        if !looks_like_path(task.title.trim()) {
            continue;
        }
        let Some(path) = safe_path(&task.title) else {
            continue;
        };
        let content = file_text(&task.title, output);
        let target = dir.join(&path);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        std::fs::write(&target, &content)
            .with_context(|| format!("failed to write {}", target.display()))?;
        written.push(WrittenFile {
            path,
            lines: content.lines().count(),
            bytes: content.len(),
        });
    }
    Ok(written)
}

/// The output contract of an edit repair: search/replace blocks that
/// [`apply_edit_blocks`] places. A repair usually changes a few lines, and
/// writing a long file again in full costs some thousand tokens.
pub const EDIT_FORMAT: &str = "Output ONLY search/replace edit blocks, never the whole \
file:\n<<<<<<< SEARCH\n(lines copied exactly from your current version)\n=======\n\
(their replacement)\n>>>>>>> REPLACE\nEach SEARCH must match exactly one place in \
the current version; include enough surrounding lines to make it unique. Use as \
many blocks as the fix needs.";

/// Apply search/replace edit blocks to `current`. `None` unless there is at
/// least one block and every SEARCH matches exactly once (an ambiguous or
/// missing anchor means the edit cannot be placed safely) and no block
/// holds a mark of another (a block that was not closed). Line ends are
/// normalized to `\n`.
pub fn apply_edit_blocks(current: &str, edits: &str) -> Option<String> {
    let edits = edits.replace("\r\n", "\n");
    let mut text = current.replace("\r\n", "\n");
    let mut applied = 0;
    let mut lines = edits.lines();
    while let Some(line) = lines.next() {
        if line.trim_end() != "<<<<<<< SEARCH" {
            continue;
        }
        let mut search = Vec::new();
        let mut replace = Vec::new();
        let mut in_replace = false;
        let mut closed = false;
        for line in lines.by_ref() {
            match line.trim_end() {
                "=======" if !in_replace => in_replace = true,
                ">>>>>>> REPLACE" if in_replace => {
                    closed = true;
                    break;
                }
                // A block that never closed: the next block's marks would
                // go into the file as text (a page with `<<<<<<< SEARCH` in
                // it, which three more repairs did not mend).
                "<<<<<<< SEARCH" | "=======" | ">>>>>>> REPLACE" => return None,
                _ if in_replace => replace.push(line),
                _ => search.push(line),
            }
        }
        if !closed || search.is_empty() {
            return None;
        }
        let search = search.join("\n");
        if text.matches(&search).count() != 1 {
            return None;
        }
        text = text.replacen(&search, &replace.join("\n"), 1);
        applied += 1;
    }
    (applied > 0).then_some(text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dag::Task;

    #[test]
    fn an_edit_block_that_is_not_closed_places_nothing() {
        let current = "a\nb\nc\n";
        let good = "<<<<<<< SEARCH\na\n=======\nA\n>>>>>>> REPLACE\n<<<<<<< SEARCH\nc\n=======\nC\n>>>>>>> REPLACE\n";
        assert_eq!(
            apply_edit_blocks(current, good).as_deref(),
            Some("A\nb\nC\n")
        );
        // The first block has no end: its replacement would hold the second
        // block's marks.
        let open =
            "<<<<<<< SEARCH\na\n=======\nA\n<<<<<<< SEARCH\nc\n=======\nC\n>>>>>>> REPLACE\n";
        assert_eq!(apply_edit_blocks(current, open), None);
        let twice = "<<<<<<< SEARCH\na\n=======\nA\n=======\nC\n>>>>>>> REPLACE\n";
        assert_eq!(apply_edit_blocks(current, twice), None);
    }

    #[test]
    fn finds_heading_bold_and_backtick_paths() {
        let doc = "Intro text about Next.js here.\n\n### `app/page.tsx`\n\n```tsx\nexport default function Page() {}\n```\n\n**lib/game.ts**\n```ts\nexport const a = 1;\n```\n\nFile: package.json\n```json\n{}\n```\n";
        let found = scan(doc);
        let paths: Vec<&str> = found.iter().map(|item| item.path.as_str()).collect();
        assert_eq!(paths, vec!["app/page.tsx", "lib/game.ts", "package.json"]);
        assert!(found.iter().all(|item| item.closed));
        assert_eq!(found[1].content, "export const a = 1;\n");
    }

    #[test]
    fn open_block_is_reported_unclosed() {
        let found = scan("## lib/a.ts\n```ts\nconst a = 1;\n");
        assert_eq!(found.len(), 1);
        assert!(!found[0].closed);
        assert_eq!(found[0].content, "const a = 1;\n");
    }

    #[test]
    fn shell_blocks_and_product_names_are_not_files() {
        let doc =
            "## Next.js setup\n\n```bash\nnpm install\n```\n\n## Run\n```\nnpm run dev\n```\n";
        assert!(scan(doc).is_empty());
    }

    #[test]
    fn path_from_info_string_and_first_line_comment() {
        let doc = "```ts title=\"lib/a.ts\"\nconst a = 1;\n```\n\n```ts\n// lib/b.ts\nconst b = 2;\n```\n";
        let found = scan(doc);
        let paths: Vec<&str> = found.iter().map(|item| item.path.as_str()).collect();
        assert_eq!(paths, vec!["lib/a.ts", "lib/b.ts"]);
    }

    #[test]
    fn markdown_file_keeps_nested_samples() {
        let doc = "### `README.md`\n```markdown\n# App\n\n```bash\nnpm run dev\n```\n\nDone.\n```\n\n### `a.ts`\n```ts\nconst a = 1;\n```\n";
        let found = scan(doc);
        assert_eq!(found.len(), 2);
        assert!(found[0].content.contains("npm run dev"));
        assert!(found[0].content.contains("Done."));
        assert_eq!(found[1].path, "a.ts");
    }

    #[test]
    fn safe_path_rejects_parent_segments_and_absolute_paths() {
        for raw in [
            "../etc/passwd",
            "a/../../b.ts",
            "a/..",
            "..\\x.ts",
            "C:/x.ts",
            "C:\\Windows\\x.ts",
            "c:x.ts",
            "file:///etc/passwd",
            "a//b.ts",
            "dir/",
            "",
            "   ",
        ] {
            assert_eq!(safe_path(raw), None, "{raw:?}");
        }
        assert_eq!(safe_path(&"a".repeat(201)), None);
        assert_eq!(safe_path("./app/page.tsx").as_deref(), Some("app/page.tsx"));
        assert_eq!(safe_path("lib\\a.ts").as_deref(), Some("lib/a.ts"));
        // A rooted path cannot leave the output directory: it is placed
        // under it, as the bench's writer did.
        assert_eq!(safe_path("/abs/file.ts").as_deref(), Some("abs/file.ts"));
        assert_eq!(
            safe_path("//server/share/a.ts").as_deref(),
            Some("server/share/a.ts")
        );
        assert_eq!(
            safe_path("\\\\server\\a.ts").as_deref(),
            Some("server/a.ts")
        );
    }

    #[test]
    fn later_block_replaces_earlier_same_path() {
        let doc = "### `a.ts`\n```ts\nold\n```\n### `a.ts`\n```ts\nnew\n```\n";
        let found = scan(doc);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].content, "new\n");
    }

    #[test]
    fn strip_fence_returns_inner_content() {
        assert_eq!(strip_fence("```ts\nconst a = 1;\n```"), "const a = 1;\n");
        assert_eq!(strip_fence("plain"), "plain\n");
        assert_eq!(strip_fence("```json\n{}\n"), "{}\n");
    }

    #[test]
    fn file_text_reads_the_block_a_check_reads() {
        assert_eq!(
            file_text("lib/a.ts", "```ts\nconst a = 1;\n```"),
            "const a = 1;\n"
        );
        // Prose before the block: the block is the file.
        assert_eq!(
            file_text("lib/a.ts", "Here it is:\n```ts\nconst a = 1;\n```\n"),
            "const a = 1;\n"
        );
        assert_eq!(file_text("lib/a.ts", "const a = 1;"), "const a = 1;\n");
    }

    #[test]
    fn render_doc_scans_back_to_the_same_files() {
        let mut files = BTreeMap::new();
        files.insert(
            "app/tasks/[id]/page.tsx".to_string(),
            "export default 1;\n".to_string(),
        );
        files.insert(
            "README.md".to_string(),
            "# App\n\n```bash\nnpm run dev\n```\n\n````\nfour\n````\n".to_string(),
        );
        files.insert(
            "lib/a.ts".to_string(),
            "const s = '```';\n```\nx\n".to_string(),
        );
        files.insert("app/(main)/page.tsx".to_string(), "no newline".to_string());
        let doc = render_doc(&files);
        let back: BTreeMap<String, String> = scan(&doc)
            .into_iter()
            .inspect(|file| assert!(file.closed, "{}", file.path))
            .map(|file| (file.path, file.content))
            .collect();
        let mut expected = files.clone();
        expected.insert(
            "app/(main)/page.tsx".to_string(),
            "no newline\n".to_string(),
        );
        assert_eq!(back, expected);
        assert_eq!(render_doc(&BTreeMap::new()), "");
    }

    #[test]
    fn render_doc_survives_a_readme_with_an_open_sample() {
        // A truncated README: its bash sample never closes. It sorts first,
        // and the files after it must still read back.
        let mut files = BTreeMap::new();
        files.insert(
            "README.md".to_string(),
            "# App\n\n```bash\nnpm i\n".to_string(),
        );
        files.insert(
            "__tests__/a.test.ts".to_string(),
            "test('a', () => {});\n".to_string(),
        );
        files.insert("lib/z.ts".to_string(), "export const z = 1;\n".to_string());
        files.insert(
            "docs/guide.md".to_string(),
            "```ts\nconst a = 1;\n```\n\n```bash\nopen\n".to_string(),
        );
        let doc = render_doc(&files);
        let back: BTreeMap<String, String> = scan(&doc)
            .into_iter()
            .inspect(|file| assert!(file.closed, "{}", file.path))
            .map(|file| (file.path, file.content))
            .collect();
        assert_eq!(back, files);
        // A path with a space is not one token: it does not read back.
        let mut spaced = BTreeMap::new();
        spaced.insert("my file.ts".to_string(), "x\n".to_string());
        let paths: Vec<String> = scan(&render_doc(&spaced))
            .into_iter()
            .map(|file| file.path)
            .collect();
        assert_ne!(paths, vec!["my file.ts".to_string()]);
    }

    #[test]
    fn a_wider_fence_closes_a_markdown_file_with_an_open_sample() {
        let doc = "## README.md\n\n````markdown\n# App\n\n```bash\nnpm i\n````\n\n## a.ts\n\n```ts\nconst a = 1;\n```\n";
        let found = scan(doc);
        assert_eq!(found.len(), 2);
        assert!(found[0].closed && found[0].content.ends_with("npm i\n"));
        assert_eq!(found[1].path, "a.ts");
        // Same width as the open sample: the sample closes, not the file.
        let doc = "## README.md\n\n```markdown\n```bash\nnpm i\n```\nDone.\n```\n";
        let found = scan(doc);
        assert!(found[0].closed && found[0].content.contains("Done."));
    }

    #[tokio::test]
    async fn write_project_writes_path_tasks_and_skips_the_rest() {
        let task = |id: &str, title: &str, internal: bool, output: &str| Task {
            id: id.into(),
            title: title.into(),
            prompt: format!("Write {title}."),
            internal,
            static_output: Some(output.into()),
            ..Task::default()
        };
        let plan = Plan {
            objective: "o".into(),
            tasks: vec![
                task("a", "lib/a.ts", false, "```ts\nexport const a = 1;\n```"),
                task("b", "Core types", false, "```ts\nx\n```"),
                task("c", "../escape.ts", false, "```ts\nx\n```"),
                task("d", "wiring.md", true, "```\nnotes\n```"),
                task("f", "lib/blank.ts", false, "  \n"),
            ],
            cache_primes: Vec::new(),
        };
        let client: std::sync::Arc<dyn crate::ModelClient> =
            std::sync::Arc::new(crate::client::MockClient::new(std::time::Duration::ZERO));
        let mut report = crate::scheduler::execute_plan(
            &plan,
            client,
            crate::scheduler::SchedulerConfig::default(),
        )
        .await
        .unwrap();
        // A task that did not succeed is not written.
        let mut with_failed = plan.clone();
        with_failed
            .tasks
            .push(task("e", "app/page.tsx", false, "```tsx\nx\n```"));
        let mut failed = report.tasks["a"].clone();
        failed.id = "e".into();
        failed.title = "app/page.tsx".into();
        failed.status = TaskStatus::Failed;
        report.tasks.insert("e".into(), failed);

        let dir = std::env::temp_dir().join(format!(
            "slimtoken-project-{}-{}",
            std::process::id(),
            line!()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let written = write_project(&report, &with_failed, &dir).unwrap();
        assert_eq!(
            written,
            vec![WrittenFile {
                path: "lib/a.ts".into(),
                lines: 1,
                bytes: 20
            }]
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("lib/a.ts")).unwrap(),
            "export const a = 1;\n"
        );
        assert!(!dir.join("Core types").exists());
        assert!(!dir.join("wiring.md").exists());
        assert!(!dir.join("app").exists());
        std::fs::remove_dir_all(&dir).ok();

        let files = report_files(&report);
        assert_eq!(files["lib/a.ts"], "export const a = 1;\n");
        assert!(!files.contains_key("app/page.tsx"));
        assert!(files.contains_key("wiring.md"));
    }

    #[test]
    fn edit_blocks_apply_only_with_unique_anchors() {
        let current = "```ts\nit(\"a\", () => {\n  expect(f()).toBe(1);\n});\n```";
        let edits = "<<<<<<< SEARCH\n  expect(f()).toBe(1);\n=======\n  expect(f()).toBe(2);\n>>>>>>> REPLACE\n";
        assert_eq!(
            apply_edit_blocks(current, edits).as_deref(),
            Some("```ts\nit(\"a\", () => {\n  expect(f()).toBe(2);\n});\n```")
        );
        let missing = "<<<<<<< SEARCH\nnot there\n=======\nx\n>>>>>>> REPLACE";
        assert_eq!(apply_edit_blocks(current, missing), None);
        let twice = "```\nx\nx\n```";
        let ambiguous = "<<<<<<< SEARCH\nx\n=======\ny\n>>>>>>> REPLACE";
        assert_eq!(apply_edit_blocks(twice, ambiguous), None);
        assert_eq!(apply_edit_blocks(current, "the whole file again"), None);
        // An unclosed block cannot be placed.
        assert_eq!(
            apply_edit_blocks("a\n", "<<<<<<< SEARCH\na\n=======\nb\n"),
            None
        );
        // CRLF edits apply to an LF file.
        assert_eq!(
            apply_edit_blocks(
                "a\nb\n",
                "<<<<<<< SEARCH\r\na\r\n=======\r\nA\r\n>>>>>>> REPLACE\r\n"
            )
            .as_deref(),
            Some("A\nb\n")
        );
    }
}
