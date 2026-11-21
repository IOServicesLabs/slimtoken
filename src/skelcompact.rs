//! Compact skeleton (`ArchitectOptions::skel_compact`).
//!
//! The skeleton call (paths and purposes, no exports) is the first serial
//! step of split planning: every export fragment waits for it, and its answer
//! is pure decode time. Today it is pretty-printed JSON, about 27 output
//! tokens a file of which only the path and the purpose carry information
//! (measured on 12 recorded DeepSeek skeletons: 29-34 files, 789-928 output
//! tokens). With the switch on, the call asks for one line per file,
//! `path: purpose`, and code turns the lines into the same `Manifest`
//! (re-encoding those 12 answers: 0.68x the characters, about 0.6x the
//! tokens). Every file and its purpose reach the export fragments unchanged.
//!
//! The answer is read by [`parse_skeleton`]: the line form, else the JSON the
//! call returns today (a model that answers in JSON anyway loses nothing).
//! Anything else, or a failed call, falls back to today's skeleton request
//! as it is sent with the switch off.

use std::sync::{Arc, Mutex};

use crate::architect::{Manifest, ManifestFile};

/// The knobs of `ArchitectOptions::skel_compact`.
#[derive(Debug, Clone, Default)]
pub struct SkelCompact {
    /// Where the step's report goes (the bench records it), besides the log.
    pub sink: Option<SkelSink>,
}

/// The report of the last compact skeleton of a run.
pub type SkelSink = Arc<Mutex<Option<SkelReport>>>;

/// How the skeleton was obtained.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum SkelFormat {
    /// No compact call ran (switch off, or the skeleton came from the
    /// objective).
    #[default]
    None,
    /// The compact call's answer, in the line form.
    Lines,
    /// The compact call's answer, in today's JSON form.
    Json,
    /// The compact call failed or its answer did not parse: today's
    /// skeleton request ran after it.
    Fallback,
}

impl SkelFormat {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Lines => "lines",
            Self::Json => "json",
            Self::Fallback => "fallback",
        }
    }
}

/// What the compact skeleton call did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SkelReport {
    pub format: SkelFormat,
    /// The compact call, send to answer.
    pub call_ms: u64,
    /// The whole skeleton step (with the fallback's call, if any).
    pub skeleton_ms: u64,
    /// The compact answer's length and output tokens.
    pub chars: usize,
    pub output_tokens: u64,
    /// Files in the skeleton that was used.
    pub files: usize,
    /// Why the compact answer was not used (empty when it was).
    pub reason: String,
}

/// The most words a compact purpose is asked for. The 380 purposes of the
/// 12 recorded skeletons had at most 11 (mean 6.7), so the cap keeps every
/// purpose as the JSON prompt gets it.
pub const PURPOSE_WORDS: usize = 12;

/// The compact skeleton prompt. It keeps the JSON prompt's marker line
/// ("paths and one-sentence purposes ONLY") so tools that find the skeleton
/// call by its prompt still find it.
pub fn compact_prompt(objective: &str, example_module: &str) -> String {
    format!(
        r#"Design the file manifest for this objective.

<objective>
{objective}
</objective>

Return paths and one-sentence purposes ONLY, as plain text lines (this answer
is not JSON): one line per file, nothing before or after, no numbering,
bullets, quotes or code fence:
{example_module}: what this file is responsible for
Each purpose is at most {PURPOSE_WORDS} words. No exports (they come in a later
pass) and no test files.
"#
    )
}

/// The skeleton in `text`: the line form ([`parse_lines`]), else today's JSON
/// form via `json` (the architect's manifest parser). `Err` names why
/// neither applies.
pub fn parse_skeleton(
    text: &str,
    json: impl Fn(&str) -> Option<Manifest>,
) -> Result<(Manifest, SkelFormat), String> {
    if let Some(manifest) = parse_lines(text) {
        return Ok((manifest, SkelFormat::Lines));
    }
    match json(text) {
        Some(manifest) if !manifest.files.is_empty() => Ok((manifest, SkelFormat::Json)),
        Some(_) => Err("JSON skeleton with no files".to_string()),
        None => Err(format!(
            "neither the line form nor JSON: {}",
            text.chars().take(120).collect::<String>()
        )),
    }
}

/// The skeleton written as `path: purpose` lines. Blank lines and a code
/// fence are skipped, and a bullet, a number or backticks around the path
/// are tolerated; any other line that is not a file (prose, JSON) makes the
/// whole answer `None`, as do a repeated path and an answer with no files.
pub fn parse_lines(text: &str) -> Option<Manifest> {
    let mut files: Vec<ManifestFile> = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with("```") {
            continue;
        }
        let (path, purpose) = parse_line(line)?;
        if files.iter().any(|file| file.path == path) {
            return None;
        }
        files.push(ManifestFile {
            path,
            purpose,
            exports: Vec::new(),
            uses: Vec::new(),
        });
    }
    (!files.is_empty()).then_some(Manifest {
        files,
        test_files: Vec::new(),
        readme: false,
    })
}

/// One `path: purpose` line (also `path - purpose`).
fn parse_line(line: &str) -> Option<(String, String)> {
    let line = strip_marker(line);
    let (token, rest) = line
        .split_once(char::is_whitespace)
        .map_or((line, ""), |(token, rest)| (token, rest.trim_start()));
    let (path, purpose) = match token.strip_suffix(':') {
        Some(path) => (path, rest),
        None => {
            let purpose = rest
                .strip_prefix(':')
                .or_else(|| rest.strip_prefix("- "))
                .or_else(|| rest.strip_prefix('\u{2014}'))
                .or_else(|| rest.strip_prefix('\u{2013}'))?;
            (token, purpose)
        }
    };
    let path = path.trim_matches('`');
    let purpose = purpose.trim();
    (valid_path(path) && !purpose.is_empty()).then(|| (path.to_string(), purpose.to_string()))
}

/// A leading `- `, `* `, `• `, `1. ` or `1) `.
fn strip_marker(line: &str) -> &str {
    for bullet in ["- ", "* ", "\u{2022} "] {
        if let Some(rest) = line.strip_prefix(bullet) {
            return rest.trim_start();
        }
    }
    let digits = line.bytes().take_while(u8::is_ascii_digit).count();
    if digits > 0 {
        let rest = &line[digits..];
        if let Some(rest) = rest.strip_prefix(". ").or_else(|| rest.strip_prefix(") ")) {
            return rest.trim_start();
        }
    }
    line
}

/// File names without an extension that a plan lists at its root (a word
/// of prose such as `Note:` must not read as a file).
const PLAIN_NAMES: &[&str] = &[
    "Makefile",
    "Dockerfile",
    "Containerfile",
    "Procfile",
    "Gemfile",
    "Rakefile",
    "Justfile",
    "Vagrantfile",
    "LICENSE",
    "NOTICE",
    "AUTHORS",
    "CODEOWNERS",
];

/// A relative file path: path characters only (route segments such as
/// `[id]` or `(group)` included), a file name with an extension or a
/// leading dot (or, without one, a name of [`PLAIN_NAMES`]; any other name
/// without a dot reads as a folder), no `..` part and no absolute root.
fn valid_path(path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or("");
    let named = if name.contains('.') {
        !name.ends_with('.')
    } else {
        PLAIN_NAMES.contains(&name)
    };
    !path.is_empty()
        && !path.starts_with('/')
        && !path.split('/').any(|part| part.is_empty() || part == "..")
        && named
        && path.chars().all(|ch| {
            ch.is_ascii_alphanumeric()
                || matches!(
                    ch,
                    '_' | '/' | '.' | '-' | '[' | ']' | '(' | ')' | '@' | '+'
                )
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn json(text: &str) -> Option<Manifest> {
        serde_json::from_str(text.trim()).ok()
    }

    fn paths(manifest: &Manifest) -> Vec<(&str, &str)> {
        manifest
            .files
            .iter()
            .map(|file| (file.path.as_str(), file.purpose.as_str()))
            .collect()
    }

    #[test]
    fn line_form_parses_every_file_and_purpose() {
        let text = "package.json: Project manifest with scripts.\n\
                    lib/types.ts: Shared unions and records.\n\
                    app/recipes/[id]/page.tsx: Detail page with scaler, steps, notes.\n";
        let (manifest, format) = parse_skeleton(text, json).unwrap();
        assert_eq!(format, SkelFormat::Lines);
        assert_eq!(
            paths(&manifest),
            vec![
                ("package.json", "Project manifest with scripts."),
                ("lib/types.ts", "Shared unions and records."),
                (
                    "app/recipes/[id]/page.tsx",
                    "Detail page with scaler, steps, notes."
                ),
            ]
        );
        assert!(manifest.test_files.is_empty());
        assert!(!manifest.readme);
        assert!(manifest.files.iter().all(|file| file.exports.is_empty()));
    }

    #[test]
    fn line_form_tolerates_fences_bullets_numbers_and_dashes() {
        let text = "```\n- `lib/a.ts`: First module\n\n2. lib/b.ts - Second module\n\
                    3) app/(shop)/page.tsx \u{2014} Third\r\n* .env.example: Env sample\n```";
        let manifest = parse_lines(text).unwrap();
        assert_eq!(
            paths(&manifest),
            vec![
                ("lib/a.ts", "First module"),
                ("lib/b.ts", "Second module"),
                ("app/(shop)/page.tsx", "Third"),
                (".env.example", "Env sample"),
            ]
        );
    }

    #[test]
    fn line_form_takes_files_without_an_extension() {
        let text = "Makefile: Build targets.\nDockerfile: Image.\nLICENSE: MIT text.\n\
                    src/main.rs: Entry point.\n";
        let manifest = parse_lines(text).unwrap();
        assert_eq!(
            paths(&manifest),
            vec![
                ("Makefile", "Build targets."),
                ("Dockerfile", "Image."),
                ("LICENSE", "MIT text."),
                ("src/main.rs", "Entry point."),
            ]
        );
        // A word of prose at the root is no file.
        assert!(parse_lines("Note: these are the files.\nsrc/main.rs: Entry.\n").is_none());
    }

    #[test]
    fn json_form_still_parses() {
        let text = r#"{"files":[{"path":"lib/a.ts","purpose":"A"},{"path":"app/page.tsx","purpose":"Home"}],"test_files":[],"readme":false}"#;
        assert!(parse_lines(text).is_none(), "JSON is not the line form");
        let (manifest, format) = parse_skeleton(text, json).unwrap();
        assert_eq!(format, SkelFormat::Json);
        assert_eq!(
            paths(&manifest),
            vec![("lib/a.ts", "A"), ("app/page.tsx", "Home")]
        );
    }

    #[test]
    fn pretty_json_is_not_mistaken_for_lines() {
        let text = "{\n  \"files\": [\n    {\"path\": \"lib/a.ts\", \"purpose\": \"A\"}\n  ],\n  \"test_files\": [],\n  \"readme\": false\n}";
        assert!(parse_lines(text).is_none());
        assert_eq!(parse_skeleton(text, json).unwrap().1, SkelFormat::Json);
    }

    #[test]
    fn garbage_is_an_error_so_the_caller_falls_back() {
        for text in [
            "",
            "   \n\n",
            "Here is the manifest you asked for:\nlib/a.ts: A",
            "lib/a.ts: A\nThat is all.",
            "lib/a.ts:\napp/page.tsx: Home",
            "lib/a.ts: A\nlib/a.ts: again",
            "/etc/passwd: absolute",
            "../up.ts: outside",
            "lib/folder: no extension",
            "{\"files\": []}",
            "{\"files\": [",
        ] {
            assert!(
                parse_skeleton(text, json).is_err(),
                "{text:?} must fall back"
            );
        }
    }

    #[test]
    fn the_prompt_keeps_the_skeleton_marker_and_the_cap() {
        let prompt = compact_prompt("Build X", "lib/example.ts");
        assert!(prompt.contains("paths and one-sentence purposes ONLY"));
        assert!(prompt.contains("lib/example.ts: what this file is responsible for"));
        assert!(prompt.contains("at most 12 words"));
        assert!(prompt.contains("<objective>\nBuild X\n</objective>"));
    }

    #[test]
    fn the_line_form_of_a_recorded_skeleton_is_smaller_and_lossless() {
        // The first entries of a recorded DeepSeek skeleton (20261116-201424).
        let recorded = r#"{
  "files": [
    {"path": "package.json", "purpose": "Project manifest with Next, React, TypeScript, Vitest scripts."},
    {"path": "lib/types.ts", "purpose": "Shared unions, catalogs, AppState, Settings, storage key."},
    {"path": "lib/shopping.ts", "purpose": "Generate shopping list from plan minus pantry, check off, custom items."},
    {"path": "app/recipes/[id]/page.tsx", "purpose": "Recipe detail with scaler, steps, notes."}
  ],
  "test_files": [],
  "readme": false
}"#;
        let today = json(recorded).unwrap();
        let lines: String = today
            .files
            .iter()
            .map(|file| format!("{}: {}\n", file.path, file.purpose))
            .collect();
        assert!(
            lines.len() * 10 < recorded.len() * 8,
            "at least 20% fewer chars"
        );
        let compact = parse_lines(&lines).unwrap();
        assert_eq!(paths(&compact), paths(&today));
    }
}
