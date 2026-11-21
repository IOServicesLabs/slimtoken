//! Turns a model's markdown answer into files.
//!
//! The single-stream arm returns one document with a heading and a fenced
//! code block for each file. `scan` finds those blocks. It runs on the
//! partial document while the stream is live (to show which file the model
//! writes now) and on the complete document at the end (to write the files).

//! The engine has the same functions in `slimtoken::project` (moved there
//! from here). With the switch `engcheck` on (the default) the bench calls
//! the engine's; `SHOWDOWN_OFF=engcheck` calls these copies. Both give the
//! engine's `Found`.

pub use slimtoken::project::Found;

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

/// Clean a path and reject one that leaves the output directory.
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

fn looks_like_path(token: &str) -> bool {
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

fn fence_of(line: &str) -> Option<(usize, &str)> {
    let trimmed = line.trim_start();
    let ticks = trimmed.chars().take_while(|ch| *ch == '`').count();
    (ticks >= 3).then(|| (ticks, trimmed[ticks..].trim()))
}

/// Find every file block in a markdown document, in document order. A later
/// block with the same path replaces the earlier one in place.
pub fn scan(doc: &str) -> Vec<Found> {
    let mut found: Vec<Found> = Vec::new();
    // Path named by a recent prose line, with the count of non-blank lines
    // seen since (a path goes stale after two lines of other prose).
    let mut candidate: Option<(String, usize)> = None;
    struct Block {
        path: Option<String>,
        width: usize,
        content: String,
        /// Depth of code samples nested inside a markdown file.
        depth: usize,
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
                if is_markdown && !info.is_empty() {
                    // A code sample inside a markdown file.
                    block.depth += 1;
                } else if block.depth > 0 && info.is_empty() {
                    block.depth -= 1;
                } else if ticks >= block.width && info.is_empty() {
                    closes = true;
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
                depth: 0,
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

/// A wave worker returns exactly one fenced block. Return its content.
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

#[cfg(test)]
mod tests {
    use super::*;

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
    fn unsafe_paths_are_rejected() {
        assert_eq!(safe_path("../etc/passwd"), None);
        assert_eq!(safe_path("C:/x.ts"), None);
        assert_eq!(safe_path("./app/page.tsx").as_deref(), Some("app/page.tsx"));
        assert_eq!(safe_path("/abs/file.ts").as_deref(), Some("abs/file.ts"));
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
}
