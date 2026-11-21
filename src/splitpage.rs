//! Split pages across parallel streams (`ArchitectOptions::split_pages`).
//!
//! The wave ends with its longest file: a page that holds its local
//! components (`pagelocal`) is one stream of 3-5k tokens at a fixed decode
//! speed, while every other file of its stage has landed. [`split_pages`]
//! runs once the manifest is final (after its passes, before the plan is
//! built) and gives each local component of such a page a file of its own
//! beside the page (`<page dir>/<dir>/<Name><ext>`, the profile's
//! `quality.page_split`), so the page and its parts are written at the same
//! time.
//!
//! Quality is kept by the same means the plan already uses for files that
//! work together:
//!
//! - **One contract, fixed before the wave.** Each part gets a typed export
//!   line: the signature the plan gave the component (folded by
//!   `pagelocal::fold_page_components`), or, for a component the plan only
//!   names, one cheap call per page (all pages at once) that writes the props
//!   from the planned types and lib functions. A signature whose types the
//!   plan does not declare, or that has no typed props, is not used. The
//!   contracts are in the manifest before the plan is built, so every
//!   writer's module head lists them: the page imports exactly what its parts
//!   export.
//! - **One owner of the state.** A part is presentational (the profile's
//!   `part_note`): data through props, changes through callbacks, no store or
//!   storage of its own; the page keeps the state and the handlers (the
//!   `parts_note` on its purpose), as `pagelocal` asks of local components.
//! - **One page per part.** Only local components (which exactly one page
//!   renders) are split, each into its page's private folder, and the page's
//!   local note loses the entries that became files.
//! - **The same checks.** The parts are ordinary tasks of the page's stage
//!   (same deps: they see the same files) and the gate and the repair loop
//!   treat them as any other file.
//!
//! A page whose plan gives no basis (no local components) is left as it is;
//! so is a component whose contract cannot be settled, which stays local.

use std::collections::{BTreeSet, HashMap};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::architect::{
    apply_manifest_passes, bare_signature, declared_names, identifiers, planner_purpose,
    task_count_and_ceiling, used_type_names, ArchitectOptions, Manifest, ManifestFile, PlanOptions,
    KNOWN_TYPES,
};
use crate::client::{CompletionRequest, ModelClient};
use crate::profile::{glob_match, Layer, Profile, Quality};

/// The knobs of `ArchitectOptions::split_pages`.
#[derive(Debug, Clone)]
pub struct SplitPages {
    /// A page is split when it has at least this many local components.
    pub min_parts: usize,
    /// At most this many parts per page (the first in the page's note).
    pub max_parts: usize,
    /// How long the props call of one page may take; a later answer leaves
    /// that page's unplanned components local. About the saving the split
    /// buys (4-6 s on the saved runs): the calls run between planning and
    /// the wave, so a stalled one must not cost more than the split saves.
    pub props_timeout: Duration,
    /// Where the step's report goes (the bench records it), besides the log.
    pub sink: Option<SplitSink>,
}

impl Default for SplitPages {
    fn default() -> Self {
        Self {
            min_parts: 1,
            max_parts: 4,
            props_timeout: Duration::from_secs(5),
            sink: None,
        }
    }
}

/// The report of the last [`split_pages`] of a run.
pub type SplitSink = Arc<Mutex<Option<SplitReport>>>;

/// One split page and the paths of its parts.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SplitPage {
    pub page: String,
    pub parts: Vec<String>,
}

/// What [`split_pages`] did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SplitReport {
    /// The pages that were split, in manifest order.
    pub pages: Vec<SplitPage>,
    /// Pages with enough local components to split.
    pub candidates: usize,
    /// Part files added.
    pub parts: usize,
    /// Parts whose contract is the plan's own signature.
    pub planned_signatures: usize,
    /// Props calls sent (one per page with unplanned components).
    pub props_calls: usize,
    /// Props calls that failed or timed out.
    pub props_failed: usize,
    /// Components the props calls gave no usable signature (they stay local).
    pub props_rejected: usize,
    /// From the first props call to the last answer (they run at once).
    pub props_ms: u64,
    /// The slowest single props call (a timed-out one counts its timeout).
    pub props_max_ms: u64,
    /// Parts left local because the plan's task ceiling had no room.
    pub parts_capped: usize,
    pub props_input_tokens: u64,
    pub props_output_tokens: u64,
    /// The whole step.
    pub split_ms: u64,
}

/// The marks of the notes that a split adds to purposes (the start of the
/// profile's `parts_note` and `part_note`), for `planner_purpose`; empty for
/// a profile without a page split.
pub fn note_marks(quality: &Quality) -> Vec<&'static str> {
    let split = &quality.page_split;
    if split.dir.is_empty() {
        return Vec::new();
    }
    [(split.parts_note, "{parts}"), (split.part_note, "{page}")]
        .into_iter()
        .filter_map(|(note, slot)| note.split(slot).next())
        .filter(|mark| !mark.trim().is_empty())
        .collect()
}

/// One local component in a page's note.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Entry {
    name: String,
    /// The text in the parentheses after the name ("" without them).
    detail: String,
    /// The entry as written in the note.
    raw: String,
}

/// One local note of a purpose: its byte range and its entries.
#[derive(Debug, Clone)]
struct Note {
    start: usize,
    end: usize,
    entries: Vec<Entry>,
}

/// Split `text` at the `; ` that sit outside brackets.
fn split_entries(text: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let (mut depth, mut from) = (0i32, 0usize);
    let bytes = text.as_bytes();
    for (index, ch) in text.char_indices() {
        match ch {
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => depth -= 1,
            ';' if depth == 0 && bytes.get(index + 1) == Some(&b' ') => {
                parts.push(&text[from..index]);
                from = index + 2;
            }
            _ => {}
        }
    }
    parts.push(&text[from..]);
    parts
}

/// An entry of a local note: `` `Name` `` with an optional ` (detail)`.
fn parse_entry(raw: &str) -> Option<Entry> {
    let text = raw.trim();
    let rest = text.strip_prefix('`')?;
    let (name, rest) = rest.split_once('`')?;
    if name.is_empty()
        || !name
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
    {
        return None;
    }
    let rest = rest.trim();
    let detail = if rest.is_empty() {
        String::new()
    } else {
        rest.strip_prefix('(')?.strip_suffix(')')?.to_string()
    };
    Some(Entry {
        name: name.to_string(),
        detail,
        raw: text.to_string(),
    })
}

/// The local notes of `purpose` (`quality.local_note`), or `None` when one
/// does not parse (the page is then not split).
fn local_notes(purpose: &str, quality: &Quality) -> Option<Vec<Note>> {
    let (head, tail) = quality.local_note.split_once("{components}")?;
    if head.is_empty() {
        return None;
    }
    let mut notes = Vec::new();
    let mut from = 0;
    while let Some(at) = purpose[from..].find(head) {
        let start = from + at;
        let body = start + head.len();
        let close = if tail.is_empty() {
            purpose.len()
        } else {
            body + purpose[body..].find(tail)?
        };
        let entries = split_entries(&purpose[body..close])
            .into_iter()
            .map(parse_entry)
            .collect::<Option<Vec<Entry>>>()?;
        let end = close + tail.len();
        notes.push(Note {
            start,
            end,
            entries,
        });
        from = end;
    }
    Some(notes)
}

/// The text before the first `//` of a line.
fn code(line: &str) -> &str {
    line.split("//").next().unwrap_or(line)
}

/// Whether the brackets of `text` close in order.
fn balanced(text: &str) -> bool {
    let mut stack = Vec::new();
    for ch in text.chars() {
        match ch {
            '(' | '[' | '{' => stack.push(ch),
            ')' | ']' | '}' => {
                let open = match ch {
                    ')' => '(',
                    ']' => '[',
                    _ => '{',
                };
                if stack.pop() != Some(open) {
                    return false;
                }
            }
            _ => {}
        }
    }
    stack.is_empty()
}

/// The keyword that declares a part in the profile's export prefix (the last
/// word: `function `).
fn keyword(export_prefix: &str) -> String {
    let word = export_prefix.trim_end().rsplit(' ').next().unwrap_or("");
    format!("{word} ")
}

/// Whether `line` is a usable contract of the part `name`: the profile's
/// export prefix, the name and typed parameters, brackets that close, on one
/// line, and only types that `declared` holds (or the language's own).
fn valid_signature(line: &str, name: &str, prefix: &str, declared: &BTreeSet<String>) -> bool {
    if line.contains('\n') {
        return false;
    }
    let code = code(line).trim_end();
    let Some(rest) = code.strip_prefix(&format!("{prefix}{name}(")) else {
        return false;
    };
    if !balanced(code) {
        return false;
    }
    let params = rest.rsplit_once(')').map_or("", |(inner, _)| inner);
    if !params.contains(':') {
        return false;
    }
    // A props type by name (`TaskCardProps`) pins no shape unless the plan
    // declares it: the page and the part would each write their own.
    let undeclared_props = identifiers(params).iter().any(|(_, word)| {
        word.starts_with(|ch: char| ch.is_ascii_uppercase())
            && word.ends_with("Props")
            && !declared.contains(*word)
            && !KNOWN_TYPES.contains(word)
    });
    if undeclared_props {
        return false;
    }
    used_type_names(&bare_signature(code))
        .iter()
        .all(|used| declared.contains(used))
}

/// The names that the code files of `manifest` declare.
fn declared(manifest: &Manifest) -> BTreeSet<String> {
    manifest
        .files
        .iter()
        .flat_map(|file| file.exports.iter())
        .flat_map(|line| declared_names(&bare_signature(line)))
        .collect()
}

/// One part of a page to split.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Part {
    entry: Entry,
    /// The component's purpose from the plan ("" when the plan only names it).
    purpose: String,
    /// The part's export line, once settled.
    line: Option<String>,
}

/// A page with local components to split.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Candidate {
    page: String,
    parts: Vec<Part>,
}

/// The planned purpose and signature of a local entry: the detail of a
/// folded component is `purpose; signature` (either may be missing); an
/// unplanned one has the fold's placeholder.
fn planned_part(entry: &Entry, prefix: &str, declared: &BTreeSet<String>) -> Part {
    let detail = entry.detail.trim();
    if detail == crate::pagelocal::UNPLANNED_DETAIL {
        return Part {
            entry: entry.clone(),
            purpose: String::new(),
            line: None,
        };
    }
    let start = format!("{}{}(", keyword(prefix), entry.name);
    let Some(at) = detail.find(&start) else {
        return Part {
            entry: entry.clone(),
            purpose: detail.to_string(),
            line: None,
        };
    };
    let purpose = detail[..at].trim().trim_end_matches(';').trim().to_string();
    let signature = &detail[at..];
    // Several signatures are joined with ` | `: the first is the component.
    let signature = split_top(signature, " | ").0.trim();
    let line = format!(
        "{prefix}{}",
        signature
            .strip_prefix(&keyword(prefix))
            .unwrap_or(signature)
    );
    let line = valid_signature(&line, &entry.name, prefix, declared).then_some(line);
    Part {
        entry: entry.clone(),
        purpose,
        line,
    }
}

/// `text` split at the first `separator` outside brackets.
fn split_top<'a>(text: &'a str, separator: &str) -> (&'a str, &'a str) {
    let mut depth = 0i32;
    for (index, ch) in text.char_indices() {
        match ch {
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => depth -= 1,
            _ if depth == 0 && text[index..].starts_with(separator) => {
                return (&text[..index], &text[index + separator.len()..]);
            }
            _ => {}
        }
    }
    (text, "")
}

/// The directory of a path (`app/x/page.tsx` -> `app/x`).
fn dir_of(path: &str) -> &str {
    path.rsplit_once('/').map_or("", |(dir, _)| dir)
}

/// The path of the part `name` of `page`.
fn part_path(page: &str, name: &str, quality: &Quality) -> String {
    let split = &quality.page_split;
    match dir_of(page) {
        "" => format!("{}/{name}{}", split.dir, split.ext),
        dir => format!("{dir}/{}/{name}{}", split.dir, split.ext),
    }
}

/// The pages of `manifest` to split, with their planned parts.
fn candidates(manifest: &Manifest, profile: &Profile, options: &SplitPages) -> Vec<Candidate> {
    let quality = &profile.staging.quality;
    let split = &quality.page_split;
    if split.dir.is_empty() || quality.page_glob.is_empty() || quality.local_note.is_empty() {
        return Vec::new();
    }
    let declared = declared(manifest);
    let paths: BTreeSet<&str> = manifest
        .files
        .iter()
        .chain(&manifest.test_files)
        .map(|file| file.path.as_str())
        .collect();
    let mut found = Vec::new();
    for file in &manifest.files {
        if !glob_match(quality.page_glob, &file.path) {
            continue;
        }
        let Some(notes) = local_notes(&file.purpose, quality) else {
            continue;
        };
        let mut names = BTreeSet::new();
        let parts: Vec<Part> = notes
            .iter()
            .flat_map(|note| note.entries.iter())
            .filter(|entry| names.insert(entry.name.clone()))
            .filter(|entry| !paths.contains(part_path(&file.path, &entry.name, quality).as_str()))
            .take(options.max_parts.max(1))
            .map(|entry| planned_part(entry, split.export_prefix, &declared))
            .collect();
        if parts.is_empty() || parts.len() < options.min_parts.max(1) {
            continue;
        }
        found.push(Candidate {
            page: file.path.clone(),
            parts,
        });
    }
    found
}

/// The export lines of the files of `layer` that `keep` keeps, under their
/// paths.
fn layer_block(
    manifest: &Manifest,
    profile: &Profile,
    layer: Layer,
    keep: impl Fn(&str) -> bool,
) -> String {
    let mut out = String::new();
    for file in &manifest.files {
        if profile.staging.layer_of(&file.path) != Some(layer)
            || file.exports.is_empty()
            || !keep(&file.path)
        {
            continue;
        }
        out.push_str(&format!("## {}\n", file.path));
        for line in &file.exports {
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

/// Keep at most `room` parts over all `candidates`, in order; a candidate
/// left with fewer than `min_parts` is not split. Returns the parts dropped.
fn cap_parts(candidates: &mut Vec<Candidate>, room: usize, options: &SplitPages) -> usize {
    let mut left = room;
    let mut dropped = 0;
    for candidate in candidates.iter_mut() {
        let keep = candidate.parts.len().min(left);
        let keep = if keep < options.min_parts.max(1) {
            0
        } else {
            keep
        };
        dropped += candidate.parts.len() - keep;
        candidate.parts.truncate(keep);
        left -= keep;
    }
    candidates.retain(|candidate| !candidate.parts.is_empty());
    dropped
}

/// The page that the part `purpose` belongs to (the path in its
/// `page_split.part_note`), or `None` for a file that is not a part.
pub(crate) fn page_of_part<'a>(purpose: &'a str, quality: &Quality) -> Option<&'a str> {
    let split = &quality.page_split;
    if split.dir.is_empty() {
        return None;
    }
    let (head, tail) = split.part_note.split_once("{page}")?;
    if head.trim().is_empty() {
        return None;
    }
    let start = purpose.find(head)? + head.len();
    let rest = &purpose[start..];
    let end = if tail.is_empty() {
        rest.len()
    } else {
        rest.find(tail)?
    };
    Some(&rest[..end]).filter(|page| !page.is_empty())
}

/// The props call of `candidate`: the planned types and lib functions, the
/// page and the parts the plan gives no contract.
fn props_prompt(
    objective: &str,
    manifest: &Manifest,
    profile: &Profile,
    candidate: &Candidate,
) -> Option<String> {
    let wanted: Vec<&Part> = candidate
        .parts
        .iter()
        .filter(|part| part.line.is_none())
        .collect();
    if wanted.is_empty() {
        return None;
    }
    let page = manifest
        .files
        .iter()
        .find(|file| file.path == candidate.page)?;
    let purpose = planner_purpose(&page.purpose, &profile.staging);
    let parts: Vec<String> = wanted
        .iter()
        .map(|part| match part.purpose.trim() {
            "" => format!("- `{}`", part.entry.name),
            purpose => format!("- `{}`: {purpose}", part.entry.name),
        })
        .collect();
    // The lib files that the page's plan names (`lib/x.ts`, `lib/x`), or
    // every one when it names none.
    let text = std::iter::once(page.purpose.as_str())
        .chain(page.exports.iter().map(String::as_str))
        .chain(page.uses.iter().map(String::as_str))
        .collect::<Vec<_>>()
        .join(
            "
",
        );
    let named = |path: &str| {
        let stem = path.rsplit_once('.').map_or(path, |(stem, _)| stem);
        text.contains(stem)
    };
    let any_named = manifest
        .files
        .iter()
        .any(|file| profile.staging.layer_of(&file.path) == Some(Layer::Core) && named(&file.path));
    let split = &profile.staging.quality.page_split;
    Some(format!(
        "Plan the props of the parts of one page of this app. One worker writes the page and \
another writes each part, at the same time, each from these contracts only, so the contracts \
must fit together.\n\n<objective>\n{objective}\n</objective>\n\nThe page `{}`: {}\nIts export:\n{}\n\n\
The planned types:\n{}\nThe planned lib functions (the page calls them and gives the parts the \
results and callbacks):\n{}\nThe parts (each one becomes a file of its own that only this page \
renders):\n{}\n\nRules:\n- The page holds all the state and calls the lib functions; a part shows \
only what its props give it and reports each change through a callback prop.\n{}\n\nAnswer with \
exactly one line per part, in the order above, and nothing else.",
        page.path,
        purpose.trim(),
        page.exports.join("\n"),
        layer_block(manifest, profile, Layer::Types, |_| true),
        layer_block(manifest, profile, Layer::Core, |path| !any_named || named(path)),
        parts.join("\n"),
        split.props_rule,
    ))
}

/// The export lines of an answer of the props call, by part name: each line
/// that holds the profile's export prefix and one of `names`, when it is a
/// valid contract (see [`valid_signature`]).
fn parse_props(
    text: &str,
    names: &[&str],
    prefix: &str,
    declared: &BTreeSet<String>,
) -> HashMap<String, String> {
    let mut found = HashMap::new();
    for line in text.lines() {
        let Some(at) = line.find(prefix) else {
            continue;
        };
        let export = line[at..].trim().trim_end_matches('`').trim_end();
        let Some(name) = export[prefix.len()..]
            .split(|ch: char| !(ch.is_ascii_alphanumeric() || ch == '_'))
            .next()
        else {
            continue;
        };
        if !names.contains(&name) || found.contains_key(name) {
            continue;
        }
        if valid_signature(export, name, prefix, declared) {
            found.insert(name.to_string(), export.to_string());
        }
    }
    found
}

/// Ask for the props of every candidate's unplanned parts at once; fills
/// `line` where an answer gives a valid one.
#[allow(clippy::too_many_arguments)]
async fn define_props(
    client: &dyn ModelClient,
    objective: &str,
    manifest: &Manifest,
    profile: &Profile,
    candidates: &mut [Candidate],
    options: &SplitPages,
    architect: &ArchitectOptions,
    report: &mut SplitReport,
) {
    let split = &profile.staging.quality.page_split;
    let declared = declared(manifest);
    let calls: Vec<(usize, String, usize)> = candidates
        .iter()
        .enumerate()
        .filter_map(|(index, candidate)| {
            let wanted = candidate
                .parts
                .iter()
                .filter(|part| part.line.is_none())
                .count();
            props_prompt(objective, manifest, profile, candidate).map(|user| (index, user, wanted))
        })
        .collect();
    if calls.is_empty() {
        return;
    }
    report.props_calls = calls.len();
    let started = Instant::now();
    let answers = futures::future::join_all(calls.iter().map(|(index, user, wanted)| {
        let request = CompletionRequest {
            system: "You are the architect for a greenfield software build. You decide the exact \
interface contracts. Answer only with the lines asked for."
                .to_string(),
            shared_context: None,
            user: user.clone(),
            max_tokens: (200 + 160 * *wanted as u32).min(1_600),
            temperature: 0.1,
            reasoning_effort: Some(architect.fragment_effort.clone()),
            prompt_cache_key: architect.cache_key.clone(),
            allow_empty: false,
            response_format: None,
        };
        let page = candidates[*index].page.clone();
        async move {
            let sent = started.elapsed().as_millis() as u64;
            let outcome = tokio::time::timeout(options.props_timeout, client.complete(request)).await;
            let done_ms = started.elapsed().as_millis() as u64;
            match &outcome {
                Ok(Ok(done)) => tracing::info!(
                    page = %page,
                    sent_ms = sent,
                    done_ms,
                    output_tokens = done.output_tokens,
                    "split page: props call answered"
                ),
                Ok(Err(error)) => {
                    tracing::warn!(page = %page, done_ms, "split page: props call failed: {error:#}")
                }
                Err(_) => tracing::warn!(page = %page, done_ms, "split page: props call timed out"),
            }
            (*index, outcome.ok().and_then(Result::ok), done_ms.saturating_sub(sent))
        }
    }))
    .await;
    report.props_ms = started.elapsed().as_millis() as u64;
    report.props_max_ms = answers.iter().map(|(_, _, ms)| *ms).max().unwrap_or(0);
    for (index, answer, _) in answers {
        let Some(done) = answer else {
            report.props_failed += 1;
            continue;
        };
        report.props_input_tokens += done.input_tokens;
        report.props_output_tokens += done.output_tokens;
        let candidate = &mut candidates[index];
        let names: Vec<&str> = candidate
            .parts
            .iter()
            .filter(|part| part.line.is_none())
            .map(|part| part.entry.name.as_str())
            .collect();
        let lines = parse_props(&done.text, &names, split.export_prefix, &declared);
        for part in candidate
            .parts
            .iter_mut()
            .filter(|part| part.line.is_none())
        {
            part.line = lines.get(&part.entry.name).cloned();
        }
    }
    report.props_rejected = candidates
        .iter()
        .flat_map(|candidate| candidate.parts.iter())
        .filter(|part| part.line.is_none())
        .count();
}

/// Rewrite `manifest` for the candidates' settled parts: each part becomes a
/// file right after its page, the page's local note keeps only the entries
/// left, and the page gets the parts note and a `uses` entry per part.
/// Returns the split pages.
fn apply_split(
    manifest: &mut Manifest,
    profile: &Profile,
    candidates: &[Candidate],
) -> Vec<SplitPage> {
    let quality = &profile.staging.quality;
    let split = &quality.page_split;
    let mut done = Vec::new();
    for candidate in candidates {
        let parts: Vec<&Part> = candidate
            .parts
            .iter()
            .filter(|part| part.line.is_some())
            .collect();
        if parts.is_empty() {
            continue;
        }
        let Some(index) = manifest
            .files
            .iter()
            .position(|file| file.path == candidate.page)
        else {
            continue;
        };
        let Some(notes) = local_notes(&manifest.files[index].purpose, quality) else {
            continue;
        };
        let moved: BTreeSet<&str> = parts.iter().map(|part| part.entry.name.as_str()).collect();
        let (head, tail) = quality
            .local_note
            .split_once("{components}")
            .unwrap_or((quality.local_note, ""));
        // The notes, rebuilt from the back so the ranges stay valid.
        let mut purpose = manifest.files[index].purpose.clone();
        for note in notes.iter().rev() {
            let left: Vec<&str> = note
                .entries
                .iter()
                .filter(|entry| !moved.contains(entry.name.as_str()))
                .map(|entry| entry.raw.as_str())
                .collect();
            let text = if left.is_empty() {
                String::new()
            } else {
                format!("{head}{}{tail}", left.join("; "))
            };
            purpose.replace_range(note.start..note.end, &text);
        }
        let list: Vec<String> = parts
            .iter()
            .map(|part| {
                format!(
                    "`{name}` (`./{}/{name}`)",
                    split.dir,
                    name = part.entry.name
                )
            })
            .collect();
        purpose.push_str(&split.parts_note.replace("{parts}", &list.join("; ")));
        let page_path = candidate.page.clone();
        let mut files = Vec::new();
        for part in &parts {
            let path = part_path(&page_path, &part.entry.name, quality);
            let line = part.line.clone().unwrap_or_default();
            let comment = line
                .split_once("//")
                .map(|(_, rest)| rest.trim())
                .unwrap_or("");
            let base = match (part.purpose.trim(), comment) {
                ("", "") => part.entry.name.clone(),
                ("", comment) => comment.to_string(),
                (purpose, _) => purpose.to_string(),
            };
            let base = base.trim_end_matches('.');
            files.push(ManifestFile {
                path: path.clone(),
                purpose: format!("{base}.{}", split.part_note.replace("{page}", &page_path)),
                exports: vec![line],
                uses: Vec::new(),
            });
            let entry = format!("{path}:{}", part.entry.name);
            let page = &mut manifest.files[index];
            if !page.uses.contains(&entry) {
                page.uses.push(entry);
            }
        }
        manifest.files[index].purpose = purpose;
        let paths: Vec<String> = files.iter().map(|file| file.path.clone()).collect();
        for (offset, file) in files.into_iter().enumerate() {
            manifest.files.insert(index + 1 + offset, file);
        }
        done.push(SplitPage {
            page: page_path,
            parts: paths,
        });
    }
    done
}

/// `ArchitectOptions::split_pages` (see the module docs): on a copy of
/// `manifest` with the plan's passes (`options`), pick the pages with local
/// components, settle a contract for each part (the plan's signature, or the
/// props call of its page, all pages at once) and split them. When a page is
/// split, `manifest` becomes that copy (the passes are idempotent, so the
/// plan runs them again on the same text); otherwise it is left as it is.
/// The report is logged and put in `split.sink`.
#[allow(clippy::too_many_arguments)]
pub async fn split_pages(
    client: &dyn ModelClient,
    objective: &str,
    manifest: &mut Manifest,
    max_tasks: usize,
    profile: &Profile,
    options: &PlanOptions,
    split: &SplitPages,
    architect: &ArchitectOptions,
) -> SplitReport {
    let started = Instant::now();
    let mut report = SplitReport::default();
    let mut passed = manifest.clone();
    apply_manifest_passes(&mut passed, profile, options);
    let mut found = candidates(&passed, profile, split);
    report.candidates = found.len();
    // The plan must stay under its task ceiling (`plan_from_manifest` stops
    // a plan over it): the parts take only the room left, in manifest order.
    let mut counted = passed.clone();
    (profile.normalize_manifest)(&mut counted);
    let (tasks, ceiling) = task_count_and_ceiling(&counted, max_tasks, options);
    report.parts_capped = cap_parts(&mut found, ceiling.saturating_sub(tasks), split);
    if report.parts_capped > 0 {
        tracing::info!(
            tasks,
            ceiling,
            parts_capped = report.parts_capped,
            "split pages: parts left local, the plan's task ceiling has no room for them"
        );
    }
    report.planned_signatures = found
        .iter()
        .flat_map(|candidate| candidate.parts.iter())
        .filter(|part| part.line.is_some())
        .count();
    if !found.is_empty() {
        define_props(
            client,
            objective,
            &passed,
            profile,
            &mut found,
            split,
            architect,
            &mut report,
        )
        .await;
        report.pages = apply_split(&mut passed, profile, &found);
        report.parts = report.pages.iter().map(|page| page.parts.len()).sum();
        if !report.pages.is_empty() {
            *manifest = passed;
        }
    }
    report.split_ms = started.elapsed().as_millis() as u64;
    tracing::info!(
        split_pages = report.pages.len(),
        parts = report.parts,
        candidates = report.candidates,
        planned_signatures = report.planned_signatures,
        props_calls = report.props_calls,
        props_failed = report.props_failed,
        props_rejected = report.props_rejected,
        props_ms = report.props_ms,
        props_max_ms = report.props_max_ms,
        parts_capped = report.parts_capped,
        props_output_tokens = report.props_output_tokens,
        split_ms = report.split_ms,
        pages = %report
            .pages
            .iter()
            .map(|page| page.page.as_str())
            .collect::<Vec<_>>()
            .join(" "),
        "split pages"
    );
    if let Some(sink) = &split.sink {
        if let Ok(mut slot) = sink.lock() {
            *slot = Some(report.clone());
        }
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::architect::{plan_from_manifest, plan_project_with, ManifestSource};
    use crate::client::Completion;
    use crate::profile::NEXTJS;
    use anyhow::Result;

    fn file(path: &str, purpose: &str, exports: &[&str]) -> ManifestFile {
        ManifestFile {
            path: path.to_string(),
            purpose: purpose.to_string(),
            exports: exports.iter().map(|line| line.to_string()).collect(),
            uses: Vec::new(),
        }
    }

    const OBJECTIVE: &str = "Build a project tracker web app with Next.js and TypeScript.";

    /// A plan whose projects page renders a component of its own (planned
    /// with typed props) and whose board page names two that no file plans;
    /// the calendar page renders none.
    fn manifest() -> Manifest {
        Manifest {
            files: vec![
                file(
                    "lib/types.ts",
                    "Shared types.",
                    &[
                        "export interface Task { id: string; title: string; status: Status }",
                        "export type Status = 'todo' | 'done'",
                        "export interface Project { id: string; name: string }",
                    ],
                ),
                file(
                    "lib/tasks.ts",
                    "Task operations.",
                    &["export function moveTask(tasks: Task[], id: string, status: Status): Task[]"],
                ),
                file(
                    "app/projects/page.tsx",
                    "Projects list.",
                    &["export default function ProjectsPage()  // renders <ProjectForm/>; lists projects"],
                ),
                file(
                    "components/ProjectForm.tsx",
                    "Create project form.",
                    &["export default function ProjectForm(props: { onSubmit: (name: string) => void })  // trims name"],
                ),
                file(
                    "app/board/page.tsx",
                    "Board of tasks.",
                    &["export default function BoardPage()  // renders <Column/> + <TaskEditor/>; moves via lib/tasks"],
                ),
                file(
                    "app/calendar/page.tsx",
                    "Month calendar.",
                    &["export default function CalendarPage()  // tasks on due dates"],
                ),
            ],
            test_files: Vec::new(),
            readme: false,
        }
    }

    fn plan_options() -> PlanOptions {
        PlanOptions {
            staged: true,
            page_local: true,
            ..PlanOptions::default()
        }
    }

    /// Answers the props call of the board page; counts the calls.
    struct Props {
        calls: Mutex<Vec<String>>,
    }

    #[async_trait::async_trait]
    impl ModelClient for Props {
        async fn complete(&self, request: CompletionRequest) -> Result<Completion> {
            self.calls.lock().unwrap().push(request.user.clone());
            let text = "Column: export default function Column({ tasks, onMove }: { tasks: Task[]; onMove: (id: string, status: Status) => void })  // one column of cards; onMove reports a move\n\
`TaskEditor: export default function TaskEditor({ task, onSave }: { task: Task | null; onSave: (title: string) => void })  // edits the selected task`";
            Ok(Completion {
                text: text.to_string(),
                provider: None,
                input_tokens: 900,
                output_tokens: 80,
                reasoning_tokens: None,
                cached_input_tokens: None,
                finish_reason: Some("stop".into()),
                usage_is_estimated: false,
                elapsed: Duration::from_millis(1),
                time_to_first_token: None,
            })
        }
    }

    fn props() -> Props {
        Props {
            calls: Mutex::new(Vec::new()),
        }
    }

    async fn split(manifest: &mut Manifest, client: &Props) -> SplitReport {
        split_pages(
            client,
            OBJECTIVE,
            manifest,
            40,
            &NEXTJS,
            &plan_options(),
            &SplitPages::default(),
            &ArchitectOptions::default(),
        )
        .await
    }

    fn find<'a>(manifest: &'a Manifest, path: &str) -> &'a ManifestFile {
        manifest
            .files
            .iter()
            .find(|file| file.path == path)
            .unwrap_or_else(|| panic!("{path} is planned"))
    }

    #[tokio::test]
    async fn a_page_with_local_components_becomes_the_page_and_its_part_files() {
        let mut manifest = manifest();
        let client = props();
        let report = split(&mut manifest, &client).await;
        assert_eq!(
            report.pages,
            vec![
                SplitPage {
                    page: "app/projects/page.tsx".into(),
                    parts: vec!["app/projects/_parts/ProjectForm.tsx".into()],
                },
                SplitPage {
                    page: "app/board/page.tsx".into(),
                    parts: vec![
                        "app/board/_parts/Column.tsx".into(),
                        "app/board/_parts/TaskEditor.tsx".into(),
                    ],
                },
            ]
        );
        assert_eq!(report.parts, 3);
        assert_eq!(report.planned_signatures, 1);
        // One call, for the board page only (the projects page's component
        // has its planned signature), with the planned types and lib.
        let calls = client.calls.lock().unwrap().clone();
        assert_eq!(calls.len(), 1);
        assert!(calls[0].contains("The page `app/board/page.tsx`"));
        assert!(calls[0].contains("export interface Task"));
        assert!(calls[0].contains("export function moveTask"));
        assert!(calls[0].contains("- `Column`"));
        assert!(!calls[0].contains("ProjectForm"));

        // The planned signature becomes the part's contract.
        let form = find(&manifest, "app/projects/_parts/ProjectForm.tsx");
        assert_eq!(
            form.exports,
            vec!["export default function ProjectForm(props: { onSubmit: (name: string) => void })  // trims name"]
        );
        assert!(form.purpose.starts_with("Create project form."));
        assert!(form
            .purpose
            .contains("Part of the page `app/projects/page.tsx`"));
        // The answer's lines are the other parts' contracts.
        let column = find(&manifest, "app/board/_parts/Column.tsx");
        assert!(column.exports[0]
            .starts_with("export default function Column({ tasks, onMove }: { tasks: Task[];"));
        assert!(column.purpose.starts_with("one column of cards"));
        let editor = find(&manifest, "app/board/_parts/TaskEditor.tsx");
        assert!(!editor.exports[0].ends_with('`'));

        // The page composes its parts: no local note left, the parts note
        // with each import path, a `uses` entry per part.
        let board = find(&manifest, "app/board/page.tsx");
        let quality = &NEXTJS.staging.quality;
        assert!(!crate::pagelocal::has_local_components(
            &board.purpose,
            quality
        ));
        assert!(board
            .purpose
            .contains("`Column` (`./_parts/Column`); `TaskEditor` (`./_parts/TaskEditor`)"));
        assert_eq!(
            board.uses,
            vec![
                "app/board/_parts/Column.tsx:Column",
                "app/board/_parts/TaskEditor.tsx:TaskEditor"
            ]
        );
        // The parts come right after their page.
        let paths: Vec<&str> = manifest
            .files
            .iter()
            .map(|file| file.path.as_str())
            .collect();
        let at = paths
            .iter()
            .position(|path| *path == "app/board/page.tsx")
            .unwrap();
        assert_eq!(paths[at + 1], "app/board/_parts/Column.tsx");

        // The plan: the parts are tasks with the page's deps, and every
        // writer's head lists their contracts.
        let plan =
            plan_from_manifest(OBJECTIVE, &manifest, 40, Some(&NEXTJS), &plan_options()).unwrap();
        let task = |title: &str| {
            plan.tasks
                .iter()
                .find(|task| task.title == title)
                .unwrap_or_else(|| panic!("{title} is a task"))
        };
        let mut page_deps = task("app/board/page.tsx").deps.clone();
        let mut part_deps = task("app/board/_parts/Column.tsx").deps.clone();
        page_deps.sort();
        part_deps.sort();
        assert_eq!(page_deps, part_deps);
        assert!(!page_deps.contains(&task("app/board/_parts/Column.tsx").id));
        assert!(task("lib/types.ts")
            .prompt
            .contains("export default function Column({ tasks, onMove }"));
        // The passes are idempotent on the split manifest: planning it again
        // adds no note and folds nothing back.
        let mut again = manifest.clone();
        apply_manifest_passes(&mut again, &NEXTJS, &plan_options());
        assert_eq!(
            serde_json::to_string(&again).unwrap(),
            serde_json::to_string(&manifest).unwrap()
        );
    }

    #[tokio::test]
    async fn a_page_without_local_components_is_untouched() {
        let mut manifest = manifest();
        let client = props();
        split(&mut manifest, &client).await;
        let calendar = find(&manifest, "app/calendar/page.tsx");
        assert_eq!(calendar.purpose, "Month calendar.");
        assert!(calendar.uses.is_empty());
        assert!(!manifest
            .files
            .iter()
            .any(|file| file.path.starts_with("app/calendar/_parts/")));

        // A plan with no local components at all: no call, no change.
        let mut plain = Manifest {
            files: vec![
                manifest.files[0].clone(),
                file(
                    "app/calendar/page.tsx",
                    "Month calendar.",
                    &["export default function CalendarPage()  // tasks on due dates"],
                ),
            ],
            test_files: Vec::new(),
            readme: false,
        };
        let before = serde_json::to_string(&plain).unwrap();
        let client = props();
        let report = split(&mut plain, &client).await;
        assert_eq!(report.candidates, 0);
        assert!(client.calls.lock().unwrap().is_empty());
        assert_eq!(serde_json::to_string(&plain).unwrap(), before);
    }

    #[tokio::test]
    async fn a_part_without_a_usable_contract_stays_local() {
        // An answer that names an undeclared type: the board page keeps its
        // note and gets no parts.
        struct Bad;
        #[async_trait::async_trait]
        impl ModelClient for Bad {
            async fn complete(&self, _: CompletionRequest) -> Result<Completion> {
                Ok(Completion {
                    text: "Column: export default function Column({ c }: { c: Lane })\nTaskEditor: export default function TaskEditor()".into(),
                    provider: None,
                    input_tokens: 1,
                    output_tokens: 1,
                    reasoning_tokens: None,
                    cached_input_tokens: None,
                    finish_reason: None,
                    usage_is_estimated: false,
                    elapsed: Duration::from_millis(1),
                    time_to_first_token: None,
                })
            }
        }
        let mut manifest = manifest();
        let report = split_pages(
            &Bad,
            OBJECTIVE,
            &mut manifest,
            40,
            &NEXTJS,
            &plan_options(),
            &SplitPages::default(),
            &ArchitectOptions::default(),
        )
        .await;
        assert_eq!(report.props_rejected, 2);
        assert_eq!(report.pages.len(), 1, "the projects page still splits");
        let board = find(&manifest, "app/board/page.tsx");
        assert!(crate::pagelocal::has_local_components(
            &board.purpose,
            &NEXTJS.staging.quality
        ));
        assert!(!manifest
            .files
            .iter()
            .any(|file| file.path.starts_with("app/board/_parts/")));
    }

    #[tokio::test]
    async fn with_the_switch_off_the_manifest_is_unchanged() {
        let saved = manifest();
        let client = props();
        let (planned, plan) = plan_project_with(
            &client,
            OBJECTIVE,
            40,
            &[],
            &ArchitectOptions::default(),
            &plan_options(),
            Some(&NEXTJS),
            ManifestSource {
                saved: Some(saved.clone()),
                prepare: None,
            },
        )
        .await
        .unwrap();
        assert!(client.calls.lock().unwrap().is_empty());
        assert_eq!(
            serde_json::to_string(&planned).unwrap(),
            serde_json::to_string(&saved).unwrap()
        );
        let direct =
            plan_from_manifest(OBJECTIVE, &saved, 40, Some(&NEXTJS), &plan_options()).unwrap();
        let prompts = |plan: &crate::dag::Plan| -> Vec<String> {
            plan.tasks.iter().map(|task| task.prompt.clone()).collect()
        };
        assert_eq!(prompts(&plan), prompts(&direct));

        // On, the same call splits (and the report reaches the sink).
        let sink: SplitSink = Arc::default();
        let architect = ArchitectOptions {
            split_pages: Some(SplitPages {
                sink: Some(sink.clone()),
                ..SplitPages::default()
            }),
            ..ArchitectOptions::default()
        };
        let (planned, plan) = plan_project_with(
            &client,
            OBJECTIVE,
            40,
            &[],
            &architect,
            &plan_options(),
            Some(&NEXTJS),
            ManifestSource {
                saved: Some(saved),
                prepare: None,
            },
        )
        .await
        .unwrap();
        assert!(planned
            .files
            .iter()
            .any(|file| file.path == "app/board/_parts/Column.tsx"));
        assert!(plan
            .tasks
            .iter()
            .any(|task| task.title == "app/board/_parts/TaskEditor.tsx"));
        assert_eq!(
            sink.lock().unwrap().as_ref().map(|report| report.parts),
            Some(3)
        );
    }

    #[test]
    fn notes_parse_and_marks_follow_the_profile() {
        let quality = &NEXTJS.staging.quality;
        let (head, tail) = quality.local_note.split_once("{components}").unwrap();
        let purpose = format!(
            "Page.{head}`A` (Does a; function A(props: {{ x: (a: number; b: string) }}) | function B()); `B` ({}){tail}",
            crate::pagelocal::UNPLANNED_DETAIL
        );
        let notes = local_notes(&purpose, quality).unwrap();
        assert_eq!(notes.len(), 1);
        let names: Vec<&str> = notes[0].entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["A", "B"]);
        assert_eq!(notes[0].end, purpose.len());
        assert_eq!(note_marks(quality).len(), 2);
        assert!(note_marks(&crate::profile::NO_QUALITY).is_empty());
        let stripped = planner_purpose(
            &format!(
                "Grid.{}",
                quality
                    .page_split
                    .part_note
                    .replace("{page}", "app/x/page.tsx")
            ),
            &NEXTJS.staging,
        );
        assert_eq!(stripped, "Grid.");
        assert_eq!(
            page_of_part(
                &format!(
                    "Grid.{}",
                    quality
                        .page_split
                        .part_note
                        .replace("{page}", "app/x/page.tsx")
                ),
                quality
            ),
            Some("app/x/page.tsx")
        );
        assert_eq!(page_of_part("Grid.", quality), None);
    }

    fn task_of<'a>(plan: &'a crate::dag::Plan, title: &str) -> &'a crate::dag::Task {
        plan.tasks
            .iter()
            .find(|task| task.title == title)
            .unwrap_or_else(|| panic!("{title} is a task"))
    }

    #[tokio::test]
    async fn a_split_page_keeps_its_wiring_and_its_parts_take_none() {
        let mut manifest = manifest();
        split(&mut manifest, &props()).await;
        let options = PlanOptions {
            wave_wiring: true,
            wiring_rules: true,
            ..plan_options()
        };
        let plan = plan_from_manifest(OBJECTIVE, &manifest, 40, Some(&NEXTJS), &options).unwrap();
        let wiring = crate::architect::WIRING_TASK.to_string();
        let calls = "prerequisite output `wiring` maps consumer paths";
        let board = task_of(&plan, "app/board/page.tsx");
        assert!(board.deps.contains(&wiring), "{:?}", board.deps);
        assert!(board.prompt.contains(calls));
        for part in [
            "app/board/_parts/Column.tsx",
            "app/board/_parts/TaskEditor.tsx",
            "app/projects/_parts/ProjectForm.tsx",
        ] {
            let part = task_of(&plan, part);
            assert!(!part.deps.contains(&wiring), "{part:?}");
            assert!(!part.prompt.contains(calls));
        }
    }

    #[tokio::test]
    async fn the_parts_stay_under_the_plans_task_ceiling() {
        let mut passed = manifest();
        apply_manifest_passes(&mut passed, &NEXTJS, &plan_options());
        (NEXTJS.normalize_manifest)(&mut passed);
        let (tasks, _) = task_count_and_ceiling(&passed, 1, &plan_options());
        // The smallest budget whose ceiling holds the plan as it is.
        let max_tasks = (1..=tasks)
            .find(|max| task_count_and_ceiling(&passed, *max, &plan_options()).1 >= tasks)
            .unwrap();
        let room = task_count_and_ceiling(&passed, max_tasks, &plan_options()).1 - tasks;
        assert!(room < 3, "the test needs less room than the three parts");
        let mut manifest = manifest();
        let report = split_pages(
            &props(),
            OBJECTIVE,
            &mut manifest,
            max_tasks,
            &NEXTJS,
            &plan_options(),
            &SplitPages::default(),
            &ArchitectOptions::default(),
        )
        .await;
        assert!(report.parts <= room, "{report:?}");
        assert_eq!(report.parts_capped, 3 - room);
        plan_from_manifest(
            OBJECTIVE,
            &manifest,
            max_tasks,
            Some(&NEXTJS),
            &plan_options(),
        )
        .expect("the split plan stays under the ceiling");
    }

    #[test]
    fn a_props_type_the_plan_does_not_declare_is_no_contract() {
        let prefix = NEXTJS.staging.quality.page_split.export_prefix;
        let mut declared: BTreeSet<String> = ["Task".to_string()].into_iter().collect();
        for line in [
            "export default function TaskCard({ task }: TaskCardProps)",
            "export default function TaskCard(props: TaskCardProps)",
        ] {
            assert!(
                !valid_signature(line, "TaskCard", prefix, &declared),
                "{line}"
            );
        }
        let inline = "export default function TaskCard({ task }: { task: Task })";
        assert!(valid_signature(inline, "TaskCard", prefix, &declared));
        declared.insert("TaskCardProps".to_string());
        assert!(valid_signature(
            "export default function TaskCard({ task }: TaskCardProps)",
            "TaskCard",
            prefix,
            &declared
        ));
    }

    #[tokio::test]
    async fn a_part_reads_its_pages_routes() {
        let mut manifest = manifest();
        split(&mut manifest, &props()).await;
        let options = PlanOptions {
            objective_pages: true,
            ..plan_options()
        };
        let plan = plan_from_manifest(OBJECTIVE, &manifest, 40, Some(&NEXTJS), &options).unwrap();
        let routes = "Routes of the app (link only to these)";
        assert!(task_of(&plan, "app/board/page.tsx").prompt.contains(routes));
        assert!(task_of(&plan, "app/board/_parts/Column.tsx")
            .prompt
            .contains(routes));
    }
}
