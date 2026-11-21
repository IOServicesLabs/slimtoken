//! Parse the judge's answers and check them against the code: every quote
//! must stand in the cited file near the cited line, and a "missing" claim
//! must survive a search for the words the judge says it looked for.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// The first JSON object in a model answer (code fences and prose around
/// it are dropped; trailing commas are repaired).
pub fn parse_json(text: &str) -> Option<serde_json::Value> {
    let start = text.find('{')?;
    let end = text.rfind('}')?;
    if end <= start {
        return None;
    }
    let body = &text[start..=end];
    serde_json::from_str(body).ok().or_else(|| {
        let mut fixed = body.to_string();
        for (from, to) in [(",}", "}"), (",]", "]"), (", }", " }"), (", ]", " ]")] {
            while fixed.contains(from) {
                fixed = fixed.replace(from, to);
            }
        }
        serde_json::from_str(&fixed).ok()
    })
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
pub struct Evidence {
    #[serde(default)]
    pub file: String,
    #[serde(default)]
    pub line: serde_json::Value,
    #[serde(default)]
    pub quote: String,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct ReqClaim {
    pub id: String,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub evidence: Vec<Evidence>,
    #[serde(default)]
    pub searched: Vec<String>,
    #[serde(default)]
    pub note: String,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct Bug {
    #[serde(default)]
    pub severity: String,
    #[serde(default)]
    pub class: serde_json::Value,
    #[serde(default)]
    pub req: String,
    #[serde(default)]
    pub file: String,
    #[serde(default)]
    pub line: serde_json::Value,
    #[serde(default)]
    pub quote: String,
    #[serde(default)]
    pub effect: String,
    /// The other side of a cross-file mismatch (class 8).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub other: Option<Evidence>,
}

/// The class number of a bug (`8`, `"8"`, `"cross-file"`).
pub fn bug_class(value: &serde_json::Value) -> Option<u64> {
    match value {
        serde_json::Value::Number(number) => number.as_u64(),
        serde_json::Value::String(text) if text.to_lowercase().contains("cross") => Some(8),
        serde_json::Value::String(text) => text.trim().parse().ok(),
        _ => None,
    }
}

/// The class of the cross-file mismatches, which cite two quotes.
pub const CROSS_FILE: u64 = 8;

/// A line number given as a number or a string (`"12"`, `"12-14"`).
pub fn line_number(value: &serde_json::Value) -> Option<usize> {
    match value {
        serde_json::Value::Number(number) => number.as_u64().map(|n| n as usize),
        serde_json::Value::String(text) => text
            .split(|ch: char| !ch.is_ascii_digit())
            .find(|part| !part.is_empty())
            .and_then(|part| part.parse().ok()),
        _ => None,
    }
}

/// White space collapsed to single spaces.
fn squash(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The app's files by path, for quote checks.
pub struct Sources<'a> {
    files: BTreeMap<&'a str, Vec<&'a str>>,
}

impl<'a> Sources<'a> {
    pub fn new(files: &'a [(String, String)]) -> Self {
        Sources {
            files: files
                .iter()
                .map(|(path, text)| (path.as_str(), text.lines().collect()))
                .collect(),
        }
    }

    fn lines(&self, file: &str) -> Option<&Vec<&'a str>> {
        let file = file.trim().trim_start_matches("./").replace('\\', "/");
        self.files.get(file.as_str())
    }

    /// True when `quote` stands in `file` within `slack` lines of `line`
    /// (white space ignored; a quote with "..." checks each part of eight
    /// characters or more).
    pub fn verify(&self, file: &str, line: Option<usize>, quote: &str, slack: usize) -> bool {
        let Some(lines) = self.lines(file) else {
            return false;
        };
        let quote = quote.trim().trim_matches('`').trim();
        if quote.is_empty() {
            return false;
        }
        let Some(line) = line.filter(|line| *line >= 1) else {
            return false;
        };
        let from = line.saturating_sub(slack + 1);
        let to = (line + slack).min(lines.len());
        if from >= to {
            return false;
        }
        let window = squash(&lines[from..to].join("\n"));
        let parts: Vec<String> = if quote.contains("...") {
            quote
                .split("...")
                .map(squash)
                .filter(|part| part.len() >= 8)
                .collect()
        } else {
            vec![squash(quote)]
        };
        !parts.is_empty() && parts.iter().all(|part| window.contains(part.as_str()))
    }

    /// True when any of `terms` (four characters or more) occurs in the UI
    /// code (`app/`, `components/`, `src/`), case ignored.
    pub fn any_term_in_ui(&self, terms: &[String]) -> bool {
        let terms: Vec<String> = terms
            .iter()
            .map(|term| term.trim().to_lowercase())
            .filter(|term| term.len() >= 4)
            .collect();
        if terms.is_empty() {
            return false;
        }
        self.files.iter().any(|(path, lines)| {
            let ui = path.starts_with("app/")
                || path.starts_with("components/")
                || path.starts_with("src/");
            ui && !path.contains(".test.")
                && lines.iter().any(|line| {
                    let lower = line.to_lowercase();
                    terms.iter().any(|term| lower.contains(term.as_str()))
                })
        })
    }
}

/// A requirement claim after the checks: status (`full`, `partial`,
/// `missing`, `stub`), and why it changed.
#[derive(Clone, Debug, Serialize)]
pub struct Checked {
    pub id: String,
    pub claimed: String,
    pub status: String,
    pub verified_quotes: usize,
    pub quotes: usize,
    /// A missing/stub claim whose search terms occur in the UI code.
    pub disputed: bool,
}

/// Normalise a status word.
pub fn status_word(text: &str) -> String {
    match text.trim().to_lowercase().as_str() {
        "full" | "done" | "complete" | "met" => "full",
        "partial" | "partly" => "partial",
        "stub" | "placeholder" => "stub",
        _ => "missing",
    }
    .to_string()
}

/// Check one sample's requirement claims: a "full" without one verified
/// quote becomes "partial"; a missing/stub claim whose searched terms occur
/// in the UI is disputed.
pub fn check_claims(claims: &[ReqClaim], sources: &Sources) -> Vec<Checked> {
    claims
        .iter()
        .map(|claim| {
            let claimed = status_word(&claim.status);
            let verified = claim
                .evidence
                .iter()
                .filter(|evidence| {
                    sources.verify(
                        &evidence.file,
                        line_number(&evidence.line),
                        &evidence.quote,
                        5,
                    )
                })
                .count();
            let mut status = claimed.clone();
            if status == "full" && verified == 0 {
                status = "partial".to_string();
            }
            let disputed = (status == "missing" || status == "stub")
                && sources.any_term_in_ui(&claim.searched);
            Checked {
                id: claim.id.clone(),
                claimed,
                status,
                verified_quotes: verified,
                quotes: claim.evidence.len(),
                disputed,
            }
        })
        .collect()
}

/// A path as `Sources` resolves it (`./lib\x.ts` is `lib/x.ts`).
fn normal_path(file: &str) -> String {
    file.trim().trim_start_matches("./").replace('\\', "/")
}

/// Words that do not make a quote specific.
const KEYWORDS: &[&str] = &[
    "return",
    "const",
    "export",
    "default",
    "function",
    "import",
    "await",
    "async",
    "true",
    "false",
    "null",
    "undefined",
    "else",
    "this",
    "from",
    "type",
    "interface",
    "void",
];

/// A quote specific enough to pin a place: at least 12 non-space
/// characters, or an identifier of 4+ characters that is not a keyword
/// (`=`, `}` or `return` match almost any line).
fn substantive(quote: &str) -> bool {
    if quote.chars().filter(|c| !c.is_whitespace()).count() >= 12 {
        return true;
    }
    quote
        .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '$'))
        .any(|word| {
            word.len() >= 4
                && !word.starts_with(|c: char| c.is_ascii_digit())
                && !KEYWORDS.contains(&word)
        })
}

/// Check one sample's bugs: only bugs whose quote stands at the cited line
/// count; a cross-file bug (class 8, or any bug with an `other` side) counts
/// only when both of its quotes are specific (12+ characters or a real
/// identifier) and stand at their cited lines in two different files;
/// duplicates (same file, lines within 3) count once. Returns (kept bugs,
/// dropped count).
pub fn check_bugs(bugs: &[Bug], sources: &Sources) -> (Vec<Bug>, usize) {
    let mut kept: Vec<Bug> = Vec::new();
    let mut dropped = 0;
    for bug in bugs {
        let line = line_number(&bug.line);
        if !sources.verify(&bug.file, line, &bug.quote, 5) {
            dropped += 1;
            continue;
        }
        let cross = bug_class(&bug.class) == Some(CROSS_FILE) || bug.other.is_some();
        if cross {
            // The second side: another file (paths compared the way
            // `Sources` resolves them), a quote that says something (not
            // `=` or `}`), standing at its cited line.
            let second = bug.other.as_ref().is_some_and(|other| {
                let other_line = line_number(&other.line);
                normal_path(&other.file) != normal_path(&bug.file)
                    && substantive(&bug.quote)
                    && substantive(&other.quote)
                    && sources.verify(&other.file, other_line, &other.quote, 5)
            });
            if !second {
                dropped += 1;
                continue;
            }
        }
        let duplicate = kept.iter().any(|other| {
            other.file == bug.file
                && line_number(&other.line)
                    .zip(line)
                    .is_some_and(|(a, b)| a.abs_diff(b) <= 3)
        });
        if !duplicate {
            let mut bug = bug.clone();
            bug.severity = match bug.severity.trim().to_lowercase().as_str() {
                "broken" | "critical" | "major" => "broken",
                "cosmetic" | "minor" | "style" => "cosmetic",
                _ => "wrong",
            }
            .to_string();
            kept.push(bug);
        }
    }
    (kept, dropped)
}

/// The merge of the samples' statuses for one requirement: agreement wins;
/// full and partial give partial; full against missing/stub (or a disputed
/// claim) needs a tiebreak (`None`).
pub fn merge_status(statuses: &[&Checked]) -> Option<String> {
    if statuses.is_empty() {
        return Some("missing".to_string());
    }
    if statuses.iter().any(|checked| checked.disputed) {
        return None;
    }
    let first = &statuses[0].status;
    if statuses.iter().all(|checked| &checked.status == first) {
        return Some(first.clone());
    }
    let has = |word: &str| statuses.iter().any(|checked| checked.status == word);
    if has("full") && (has("missing") || has("stub")) {
        return None;
    }
    if has("partial") || has("full") {
        return Some("partial".to_string());
    }
    Some("missing".to_string())
}

/// Share of requirements on which samples `a` and `b` agree, and Cohen's
/// kappa over the categories full / partial / not met.
pub fn agreement(a: &[Checked], b: &[Checked]) -> (f64, f64) {
    let category = |status: &str| match status {
        "full" => 0,
        "partial" => 1,
        _ => 2,
    };
    let b_by_id: BTreeMap<&str, &Checked> = b
        .iter()
        .map(|checked| (checked.id.as_str(), checked))
        .collect();
    let pairs: Vec<(usize, usize)> = a
        .iter()
        .filter_map(|checked| {
            b_by_id
                .get(checked.id.as_str())
                .map(|other| (category(&checked.status), category(&other.status)))
        })
        .collect();
    if pairs.is_empty() {
        return (0.0, 0.0);
    }
    let n = pairs.len() as f64;
    let observed = pairs.iter().filter(|(x, y)| x == y).count() as f64 / n;
    let mut expected = 0.0;
    for cat in 0..3 {
        let pa = pairs.iter().filter(|(x, _)| *x == cat).count() as f64 / n;
        let pb = pairs.iter().filter(|(_, y)| *y == cat).count() as f64 / n;
        expected += pa * pb;
    }
    let kappa = if (1.0 - expected).abs() < 1e-9 {
        1.0
    } else {
        (observed - expected) / (1.0 - expected)
    };
    (observed, kappa)
}

/// Share of sample `a`'s bugs that sample `b` also reports (same file,
/// lines within 10).
pub fn bug_overlap(a: &[Bug], b: &[Bug]) -> f64 {
    if a.is_empty() {
        return if b.is_empty() { 1.0 } else { 0.0 };
    }
    let found = a
        .iter()
        .filter(|bug| {
            b.iter().any(|other| {
                other.file == bug.file
                    && line_number(&other.line)
                        .zip(line_number(&bug.line))
                        .is_some_and(|(x, y)| x.abs_diff(y) <= 10)
            })
        })
        .count();
    found as f64 / a.len() as f64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn files() -> Vec<(String, String)> {
        vec![
            (
                "app/board/page.tsx".to_string(),
                "'use client';\nconst a = 1;\nconst project = projects[0];\nexport default function B() {}\n".to_string(),
            ),
            ("lib/team.ts".to_string(), "return t.status === \"Done\";\n".to_string()),
        ]
    }

    #[test]
    fn parse_json_finds_the_object_in_prose_and_fences() {
        let text = "Here:\n```json\n{\"a\": [1, 2,], \"b\": {\"c\": 1,},}\n```";
        let value = parse_json(text).unwrap();
        assert_eq!(value["a"][1], 2);
        assert!(parse_json("no json").is_none());
    }

    #[test]
    fn quotes_must_stand_near_the_cited_line() {
        let files = files();
        let sources = Sources::new(&files);
        assert!(sources.verify(
            "app/board/page.tsx",
            Some(3),
            "const project =   projects[0];",
            5
        ));
        assert!(sources.verify("./app/board/page.tsx", Some(8), "projects[0]", 5));
        assert!(!sources.verify(
            "app/board/page.tsx",
            Some(3),
            "settings.defaultProjectId",
            5
        ));
        assert!(!sources.verify("app/nope.tsx", Some(1), "x", 5));
        assert!(!sources.verify("app/board/page.tsx", None, "projects[0]", 5));
        assert!(sources.verify(
            "app/board/page.tsx",
            Some(2),
            "const a = 1; ... = projects[0];",
            5
        ));
        assert!(sources.any_term_in_ui(&["PROJECT".to_string()]));
        assert!(
            !sources.any_term_in_ui(&["Done".to_string()]),
            "lib/ is not UI"
        );
        assert_eq!(line_number(&serde_json::json!("12-14")), Some(12));
    }

    #[test]
    fn claims_and_bugs_are_checked_and_merged() {
        let files = files();
        let sources = Sources::new(&files);
        let claims: Vec<ReqClaim> = serde_json::from_value(serde_json::json!([
            {"id": "B1", "status": "full", "evidence": [{"file": "app/board/page.tsx", "line": 3, "quote": "projects[0]"}]},
            {"id": "X2", "status": "full", "evidence": [{"file": "app/board/page.tsx", "line": 3, "quote": "defaultProjectId"}]},
            {"id": "TC", "status": "missing", "searched": ["createTask"]},
            {"id": "P1", "status": "missing", "searched": ["project"]},
        ]))
        .unwrap();
        let checked = check_claims(&claims, &sources);
        assert_eq!(checked[0].status, "full");
        assert_eq!(
            checked[1].status, "partial",
            "an unverified full is partial"
        );
        assert!(!checked[2].disputed);
        assert!(checked[3].disputed);
        let bugs: Vec<Bug> = serde_json::from_value(serde_json::json!([
            {"severity": "wrong", "file": "lib/team.ts", "line": 1, "quote": "t.status === \"Done\""},
            {"severity": "Wrong", "file": "lib/team.ts", "line": 2, "quote": "status === \"Done\""},
            {"severity": "broken", "file": "lib/team.ts", "line": 1, "quote": "invented"},
        ]))
        .unwrap();
        let (kept, dropped) = check_bugs(&bugs, &sources);
        assert_eq!((kept.len(), dropped), (1, 1));
        let other = Checked {
            status: "missing".into(),
            ..checked[0].clone()
        };
        assert_eq!(
            merge_status(&[&checked[0], &checked[0]]).as_deref(),
            Some("full")
        );
        assert_eq!(
            merge_status(&[&checked[0], &checked[1]]).as_deref(),
            Some("partial")
        );
        assert_eq!(merge_status(&[&checked[0], &other]), None);
        assert_eq!(merge_status(&[&checked[3]]), None, "disputed");
        let (share, kappa) = agreement(&checked, &checked);
        assert_eq!(share, 1.0);
        assert!((kappa - 1.0).abs() < 1e-9);
        assert_eq!(bug_overlap(&kept, &kept), 1.0);
    }

    #[test]
    fn a_cross_file_bug_needs_both_quotes() {
        let files = files();
        let sources = Sources::new(&files);
        let bugs: Vec<Bug> = serde_json::from_value(serde_json::json!([
            // Both sides stand at their lines: kept.
            {"severity": "broken", "class": 8, "file": "app/board/page.tsx", "line": 3, "quote": "projects[0]",
             "other": {"file": "lib/team.ts", "line": 1, "quote": "t.status === \"Done\""}},
            // The second quote is invented: dropped.
            {"severity": "broken", "class": "8", "file": "app/board/page.tsx", "line": 2, "quote": "const a = 1;",
             "other": {"file": "lib/team.ts", "line": 1, "quote": "status === 'done'"}},
            // Class 8 with no second side: dropped.
            {"severity": "wrong", "class": "cross-file", "file": "lib/team.ts", "line": 1, "quote": "t.status"},
            // The "other side" is the same place: dropped.
            {"severity": "wrong", "class": 8, "file": "lib/team.ts", "line": 1, "quote": "t.status",
             "other": {"file": "./lib/team.ts", "line": 1, "quote": "\"Done\""}},
            // Another class with an `other` side is held to the same rule.
            {"severity": "wrong", "class": 2, "file": "app/board/page.tsx", "line": 4, "quote": "export default function B()",
             "other": {"file": "lib/team.ts", "line": 1, "quote": "nowhere"}},
            // A trivial second quote ("=" is on almost every line): dropped.
            {"severity": "broken", "class": 8, "file": "app/board/page.tsx", "line": 3, "quote": "projects[0]",
             "other": {"file": "lib/team.ts", "line": 1, "quote": "="}},
            // The same file under another spelling: not cross-file.
            {"severity": "broken", "class": 8, "file": "lib/team.ts", "line": 1, "quote": "t.status",
             "other": {"file": "lib\\team.ts", "line": 1, "quote": "t.status === \"Done\""}},
        ]))
        .unwrap();
        let (kept, dropped) = check_bugs(&bugs, &sources);
        assert_eq!((kept.len(), dropped), (1, 6));
        assert!(!substantive("="));
        assert!(!substantive("return"));
        assert!(!substantive("} }"));
        assert!(substantive("parseDate"));
        assert!(substantive("a.b === c.d + 10"));
        assert!(!substantive("a.b === c.d + 1"));
        assert_eq!(kept[0].other.as_ref().unwrap().file, "lib/team.ts");
        assert_eq!(
            bug_class(&serde_json::json!("cross-file")),
            Some(CROSS_FILE)
        );
        assert_eq!(bug_class(&serde_json::json!(" 3 ")), Some(3));
    }
}
