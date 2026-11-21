//! Cross-file quality lints (`qlint`): defects that build, type-check and
//! pass the tests, but that a person who opens the app sees. Each lint looks
//! at the whole app (pages, components, `lib/`, the CSS and the README), not
//! at one file, and is tuned for precision: an `ERROR` line costs a repair
//! round, so a lint fires only on a pattern that the blind audit of
//! 2026-11-15 judged a defect and that the single-stream apps (which the
//! audit judged good) do not show.
//!
//! | code   | lint                  | level | the line names |
//! |--------|-----------------------|-------|----------------|
//! | TS9010 | `settings-unread`     | ERROR | the file that should read the setting |
//! | TS9011 | `literal-case`        | ERROR | the file with the comparison |
//! | TS9012 | `theme-selector`      | ERROR | the file that sets the theme |
//! | TS9013 | `dead-route`          | ERROR | the file with the link |
//! | TS9014 | `server-storage`      | ERROR | the Server Component page |
//! | TS9015 | `partial-save`        | ERROR | the file with the save |
//! | TS9016 | `id-collision`        | ERROR | the id module |
//! | TS9017 | `seed-dates`          | ERROR | the seed file |
//! | TS9018 | `readme-spec`         | WARN  | `README.md` (the plan renders it; a repair cannot change it) |
//! | TS9019 | `css-undefined-class` | WARN  | the file with the class |
//!
//! With `qlint2` (default off) the convention lints run too, from the second
//! blind audit (5 race pairs): two files that each look right but disagree;
//! TS9026-TS9029 from the third (TaskFlow with one store whose first render
//! is the seed), TS9030-TS9032 from the fifth (the default stack against the
//! single stream), TS9033 from the seventh.
//!
//! | code   | lint                    | level | the line names |
//! |--------|-------------------------|-------|----------------|
//! | TS9020 | `storage-outside-store` | ERROR | the page or component that reads or writes storage beside the store provider that the root layout mounts (a provider that only a page renders: the layout; with no mounted provider: a layout component that saves a copy it loaded once) |
//! | TS9021 | `duplicate-shell`       | ERROR | the page that renders the layout's shell again |
//! | TS9022 | `sign-mismatch`         | ERROR | the lib validator that rejects the negative amounts the callers and the seed send (the callers and the seed instead when the types file states a positive amount) |
//! | TS9023 | `date-format-mismatch`  | ERROR | the date helper that accepts only 'YYYY-MM-DD' but gets stored ISO timestamps |
//! | TS9024 | `transition-timestamps` | ERROR | the lib file whose status change never sets the completion timestamp |
//! | TS9025 | `hardcoded-currency`    | ERROR | the page or component that formats money with a fixed currency code |
//! | TS9026 | `render-before-ready`   | ERROR | the page that calls notFound()/redirect() on a record before the store's `ready` flag (the store too when it has no flag and `ungated-store` is silent), or the form that copies a record into useState once (no key with `ready`, no effect on `ready`); none when the provider renders its children only once `ready` |
//! | TS9027 | `throw-in-updater`      | ERROR | the page or component that calls a throwing lib function inside a state updater, in a try block |
//! | TS9028 | `search-link`           | ERROR | the file that links a comment search hit by the comment's own id |
//! | TS9029 | `duplicate-catalog`     | ERROR | the file that declares its own copy of a catalog of the types file with other ids |
//! | TS9030 | `draft-resync`          | ERROR | the component that re-sets its form drafts in a useEffect on the record object (any change to the record discards unsaved edits; key the form by the record id) |
//! | TS9031 | `ungated-store`         | ERROR | the store provider that starts from the seed and renders its children before the stored state was read, when a page looks a record up by a route or query parameter or an effect copies the state into local state (only for an app planned with `qsemantic`, whose conventions ask for that gate) |
//! | TS9032 | `empty-format`          | ERROR | the lib formatter that throws on an empty value and that a page or component calls in render, unguarded, with a field that may be empty |
//! | TS9033 | `activity-unwritten`    | WARN  | the types file whose state holds an activity, history or log collection that no file but the seed ever writes (the feed shows only the seed's entries) |
//!
//! Each line explains the defect and the change in its own text, so the
//! repair prompt (which quotes the line) carries the explanation.

use std::collections::{BTreeMap, BTreeSet};

/// `ERROR` (a repair round fixes it) or `WARN` (a hint, never a failure).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Error,
    Warn,
}

/// One finding.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Lint {
    pub level: Level,
    pub code: &'static str,
    pub name: &'static str,
    /// The file that should change.
    pub path: String,
    /// 1-based line of the finding in `path` (1 when it has none).
    pub line: usize,
    pub message: String,
}

impl Lint {
    /// The gate line: `ERROR app/x.tsx(3,1): error TS9011: literal-case: ...`.
    pub fn render(&self) -> String {
        let level = match self.level {
            Level::Error => "ERROR",
            Level::Warn => "WARN",
        };
        format!(
            "{level} {}({},1): error {}: {}: {}",
            self.path, self.line, self.code, self.name, self.message
        )
    }
}

/// The lint names, in the order of the table above.
pub const NAMES: &[&str] = &[
    "settings-unread",
    "literal-case",
    "theme-selector",
    "dead-route",
    "server-storage",
    "partial-save",
    "id-collision",
    "seed-dates",
    "readme-spec",
    "css-undefined-class",
];

/// The `qlint2` lint names, in the order of the second table above.
pub const NAMES2: &[&str] = &[
    "storage-outside-store",
    "duplicate-shell",
    "sign-mismatch",
    "date-format-mismatch",
    "transition-timestamps",
    "hardcoded-currency",
    "render-before-ready",
    "throw-in-updater",
    "search-link",
    "duplicate-catalog",
    "draft-resync",
    "ungated-store",
    "empty-format",
    "activity-unwritten",
];

/// One file of the app: its project path and its text with comments blanked
/// (strings kept, so line numbers and offsets stay those of the file).
struct File<'a> {
    path: &'a str,
    raw: &'a str,
    code: String,
}

impl File<'_> {
    fn is_test(&self) -> bool {
        is_test(self.path)
    }
    fn is_script(&self) -> bool {
        (self.path.ends_with(".ts") || self.path.ends_with(".tsx")) && !self.path.ends_with(".d.ts")
    }
    fn line_of(&self, at: usize) -> usize {
        line_of(&self.code, at)
    }
}

fn is_test(path: &str) -> bool {
    path.contains(".test.") || path.contains(".spec.") || path.contains("__tests__/")
}

fn line_of(text: &str, at: usize) -> usize {
    text[..at.min(text.len())].matches('\n').count() + 1
}

fn is_ident(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'$'
}

/// `source` with `//` and `/* */` comments replaced by spaces (newlines
/// kept). Strings and template literals are left as they are.
fn blank_comments(source: &str) -> String {
    let bytes = source.as_bytes();
    let mut out = bytes.to_vec();
    let mut index = 0;
    let mut quote: Option<u8> = None;
    while index < bytes.len() {
        let byte = bytes[index];
        if let Some(open) = quote {
            if byte == b'\\' {
                index += 2;
                continue;
            }
            if byte == open || (byte == b'\n' && open != b'`') {
                quote = None;
            }
            index += 1;
            continue;
        }
        match byte {
            b'"' | b'\'' | b'`' => quote = Some(byte),
            b'/' if bytes.get(index + 1) == Some(&b'/') => {
                // `://` in a URL inside JSX text is rare; a comment is not.
                while index < bytes.len() && bytes[index] != b'\n' {
                    out[index] = b' ';
                    index += 1;
                }
                continue;
            }
            b'/' if bytes.get(index + 1) == Some(&b'*') => {
                while index < bytes.len()
                    && !(bytes[index] == b'*' && bytes.get(index + 1) == Some(&b'/'))
                {
                    if bytes[index] != b'\n' {
                        out[index] = b' ';
                    }
                    index += 1;
                }
                for _ in 0..2 {
                    if index < bytes.len() {
                        out[index] = b' ';
                        index += 1;
                    }
                }
                continue;
            }
            _ => {}
        }
        index += 1;
    }
    // Only ASCII bytes were replaced by ASCII spaces, but a multi-byte
    // character inside a comment was blanked byte by byte: still valid UTF-8.
    String::from_utf8(out).unwrap_or_else(|_| source.to_string())
}

/// `code` with the text of every string and template literal replaced by
/// spaces (quotes and newlines kept), so a search sees only code.
fn blank_strings(code: &str) -> String {
    let bytes = code.as_bytes();
    let mut out = bytes.to_vec();
    let mut index = 0;
    let mut quote: Option<u8> = None;
    while index < bytes.len() {
        let byte = bytes[index];
        match quote {
            Some(open) => {
                if byte == b'\\' {
                    out[index] = b' ';
                    if index + 1 < bytes.len() && bytes[index + 1] != b'\n' {
                        out[index + 1] = b' ';
                    }
                    index += 2;
                    continue;
                }
                if byte == open || (byte == b'\n' && open != b'`') {
                    quote = None;
                } else if byte != b'\n' {
                    out[index] = b' ';
                }
            }
            None => {
                if matches!(byte, b'"' | b'\'' | b'`') {
                    quote = Some(byte);
                }
            }
        }
        index += 1;
    }
    String::from_utf8(out).unwrap_or_else(|_| code.to_string())
}

/// Byte offsets of `word` in `text` as a whole identifier.
fn word_positions(text: &str, word: &str) -> Vec<usize> {
    let bytes = text.as_bytes();
    text.match_indices(word)
        .map(|(at, _)| at)
        .filter(|&at| {
            let before = at == 0 || !is_ident(bytes[at - 1]);
            let after = bytes
                .get(at + word.len())
                .is_none_or(|byte| !is_ident(*byte));
            before && after
        })
        .collect()
}

/// The next byte after `at` that is not white space.
fn next_char(text: &str, at: usize) -> Option<u8> {
    text.as_bytes()[at.min(text.len())..]
        .iter()
        .copied()
        .find(|byte| !byte.is_ascii_whitespace())
}

/// The last byte before `at` that is not white space.
fn prev_char(text: &str, at: usize) -> Option<u8> {
    text.as_bytes()[..at.min(text.len())]
        .iter()
        .rev()
        .copied()
        .find(|byte| !byte.is_ascii_whitespace())
}

/// The index of the bracket that closes the one at `open` (`{`, `(` or
/// `[`), skipping strings. `None` when it does not close.
fn matching(text: &str, open: usize) -> Option<usize> {
    let bytes = text.as_bytes();
    let mut depth = 0i32;
    let mut index = open;
    let mut quote: Option<u8> = None;
    while index < bytes.len() {
        let byte = bytes[index];
        if let Some(q) = quote {
            if byte == b'\\' {
                index += 2;
                continue;
            }
            if byte == q {
                quote = None;
            }
            index += 1;
            continue;
        }
        match byte {
            b'"' | b'\'' | b'`' => quote = Some(byte),
            b'{' | b'(' | b'[' => depth += 1,
            b'}' | b')' | b']' => {
                depth -= 1;
                if depth == 0 {
                    return Some(index);
                }
            }
            _ => {}
        }
        index += 1;
    }
    None
}

/// The top-level keys of the object literal or type body that opens at
/// `open` (a `{`), and whether it spreads another object (`...x`).
fn object_keys(text: &str, open: usize) -> Option<(Vec<String>, bool)> {
    let close = matching(text, open)?;
    let body = &text[open + 1..close];
    let bytes = body.as_bytes();
    let mut keys = Vec::new();
    let mut spread = false;
    let mut depth = 0i32;
    let mut quote: Option<u8> = None;
    // True at the start of an entry (after `{`, `,` or `;`).
    let mut start = true;
    let mut index = 0;
    while index < bytes.len() {
        let byte = bytes[index];
        if let Some(q) = quote {
            if byte == b'\\' {
                index += 2;
                continue;
            }
            if byte == q {
                quote = None;
            }
            index += 1;
            continue;
        }
        match byte {
            b'"' | b'\'' | b'`' => {
                quote = Some(byte);
                start = false;
            }
            b'{' | b'(' | b'[' => {
                depth += 1;
                start = false;
            }
            b'}' | b')' | b']' => depth -= 1,
            b',' | b';' if depth == 0 => start = true,
            b'\n' if depth == 0 => {
                // A type body separates members by new lines too.
                if prev_char(body, index).is_some_and(|ch| !b"|&:=+-*/?([{".contains(&ch)) {
                    start = true;
                }
            }
            b'.' if depth == 0 && start && body[index..].starts_with("...") => {
                spread = true;
                start = false;
            }
            _ if depth == 0
                && start
                && (byte.is_ascii_alphabetic() || byte == b'_' || byte == b'$') =>
            {
                let end = index
                    + bytes[index..]
                        .iter()
                        .take_while(|byte| is_ident(**byte))
                        .count();
                let name = &body[index..end];
                let after = next_char(body, end);
                let is_key = matches!(after, Some(b':' | b'?' | b',' | b'}' | b';' | b'('))
                    || after.is_none()
                    || body[end..].trim_start().starts_with('\n');
                if name != "readonly" && is_key {
                    keys.push(name.to_string());
                }
                start = false;
                index = end;
                continue;
            }
            _ if !byte.is_ascii_whitespace() => start = false,
            _ => {}
        }
        index += 1;
    }
    Some((keys, spread))
}

/// The files of the app, prepared.
struct App<'a> {
    files: Vec<File<'a>>,
}

impl<'a> App<'a> {
    fn new(files: &'a [(String, String)]) -> Self {
        App {
            files: files
                .iter()
                .map(|(path, content)| File {
                    path,
                    raw: content,
                    code: if path.ends_with(".ts")
                        || path.ends_with(".tsx")
                        || path.ends_with(".css")
                    {
                        blank_comments(content)
                    } else {
                        content.clone()
                    },
                })
                .collect(),
        }
    }

    fn scripts(&self) -> impl Iterator<Item = &File<'a>> {
        self.files
            .iter()
            .filter(|file| file.is_script() && !file.is_test())
    }

    fn get(&self, path: &str) -> Option<&File<'a>> {
        self.files.iter().find(|file| file.path == path)
    }

    /// The pages: (route segments, file).
    fn pages(&self) -> Vec<(Vec<String>, &File<'a>)> {
        self.files
            .iter()
            .filter_map(|file| route_segments(file.path).map(|route| (route, file)))
            .collect()
    }

    /// The file that holds the app's state for every page: a provider or
    /// store module, else the root layout.
    fn state_owner(&self) -> Option<&File<'a>> {
        let lower = |file: &File| file.path.to_lowercase();
        self.scripts()
            .find(|file| {
                let path = lower(file);
                (path.ends_with("provider.tsx") || path.ends_with("providers.tsx"))
                    && file.code.contains("createContext")
            })
            .or_else(|| {
                self.scripts().find(|file| {
                    let path = lower(file);
                    path.starts_with("lib/store") && file.code.contains("createContext")
                })
            })
            .or_else(|| {
                self.files
                    .iter()
                    .find(|file| file.path == "app/layout.tsx" || file.path == "src/app/layout.tsx")
            })
    }
}

/// The URL segments of an App Router page (`app/tasks/[id]/page.tsx` gives
/// `["tasks", "[id]"]`), or `None` for a file that is not a page.
fn route_segments(path: &str) -> Option<Vec<String>> {
    let rest = path
        .strip_prefix("src/app/")
        .or_else(|| path.strip_prefix("app/"))?;
    let (dir, name) = rest.rsplit_once('/').unwrap_or(("", rest));
    if !matches!(name, "page.tsx" | "page.jsx" | "page.ts" | "page.js") {
        return None;
    }
    Some(
        dir.split('/')
            .filter(|part| !part.is_empty() && !part.starts_with('(') && !part.starts_with('@'))
            .map(str::to_string)
            .collect(),
    )
}

/// Run every lint on the app's files (`(project path, text)`): the `qlint`
/// set, without the `qlint2` convention lints (see [`lint_app_with`]).
#[cfg(test)]
pub fn lint_app(files: &[(String, String)]) -> Vec<Lint> {
    lint_app_with(files, true, false, false)
}

/// Run the `qlint` set (`base`) and the `qlint2` convention lints
/// (`conventions`, after the base set) on the app's files. `semantics`: the
/// app was planned with `qsemantic`, so a money field that its types file
/// leaves open follows the default convention of the prompts (always > 0).
pub fn lint_app_with(
    files: &[(String, String)],
    base: bool,
    conventions: bool,
    semantics: bool,
) -> Vec<Lint> {
    let app = App::new(files);
    let mut lints = if base { base_lints(&app) } else { Vec::new() };
    if conventions {
        lints.extend(storage_outside_store(&app));
        lints.extend(duplicate_shell(&app));
        lints.extend(sign_mismatch(&app, semantics));
        lints.extend(date_format_mismatch(&app));
        lints.extend(transition_timestamps(&app));
        lints.extend(hardcoded_currency(&app));
        // `ungated-store` names the store file with the fix of every page
        // under it: TS9026's line on that file says less.
        let ungated = ungated_store(&app, semantics);
        lints.extend(
            render_before_ready(&app)
                .into_iter()
                .filter(|lint| ungated.iter().all(|store| store.path != lint.path)),
        );
        lints.extend(throw_in_updater(&app));
        lints.extend(search_link(&app));
        lints.extend(duplicate_catalog(&app));
        lints.extend(draft_resync(&app));
        lints.extend(ungated);
        lints.extend(empty_format(&app));
        lints.extend(activity_unwritten(&app));
    }
    lints
}

/// The `qlint` set.
fn base_lints(app: &App) -> Vec<Lint> {
    let mut lints = Vec::new();
    let theme = theme_selector(app);
    // A theme that no file applies is the theme lint's finding, once.
    lints.extend(settings_unread(app, !theme.is_empty()));
    lints.extend(literal_case(app));
    lints.extend(theme);
    lints.extend(dead_route(app));
    lints.extend(server_storage(app));
    lints.extend(partial_save(app));
    lints.extend(id_collision(app));
    lints.extend(seed_dates(app));
    lints.extend(readme_spec(app));
    lints.extend(css_undefined_class(app));
    lints
}

/// The gate's lines for the app: (`ERROR` lines, `WARN` lines).
/// `relative_dates`: the plan asked the seed for dates relative to the day
/// the data loads (`qrules`). Without it the plan asks for fixed dates
/// around the day of the run (`seeddates`), so `seed-dates` is only a `WARN`
/// there: an `ERROR` would make the repair rewrite the seed against its own
/// spec (and against tests that pin those dates). `base` runs the `qlint`
/// set, `conventions` the `qlint2` convention lints.
#[cfg(test)]
pub fn gate_lines(files: &[(String, String)], relative_dates: bool) -> (Vec<String>, Vec<String>) {
    gate_lines_with(files, relative_dates, true, false, false)
}

/// [`gate_lines`] with the lint sets chosen (`semantics` as in
/// [`lint_app_with`]).
pub fn gate_lines_with(
    files: &[(String, String)],
    relative_dates: bool,
    base: bool,
    conventions: bool,
    semantics: bool,
) -> (Vec<String>, Vec<String>) {
    let mut errors = Vec::new();
    let mut warnings = Vec::new();
    for mut lint in lint_app_with(files, base, conventions, semantics) {
        if lint.name == "seed-dates" && !relative_dates {
            lint.level = Level::Warn;
        }
        match lint.level {
            Level::Error => errors.push(lint.render()),
            Level::Warn => warnings.push(lint.render()),
        }
    }
    (errors, warnings)
}

// ---- settings-unread -------------------------------------------------------

/// Words that name a settings-like type.
fn is_settings_type(name: &str) -> bool {
    ["Settings", "Prefs", "Preferences"]
        .iter()
        .any(|suffix| name.ends_with(suffix))
}

/// The fields of the app's settings: field -> the file that declares it.
fn settings_fields(app: &App) -> BTreeMap<String, String> {
    let mut fields: BTreeMap<String, (String, bool)> = BTreeMap::new();
    for file in app.scripts() {
        let code = &file.code;
        let mut opens: Vec<usize> = Vec::new();
        for keyword in ["interface ", "type "] {
            for at in word_positions(code, keyword.trim_end()) {
                let rest = &code[at + keyword.len()..];
                let name_len = rest.bytes().take_while(|byte| is_ident(*byte)).count();
                if name_len == 0 || !is_settings_type(&rest[..name_len]) {
                    continue;
                }
                let after = &rest[name_len..];
                let Some(brace) = after.find('{') else {
                    continue;
                };
                // `interface X {`, `interface X extends Y {`, `type X = {`.
                let between = after[..brace].trim();
                if between.is_empty() || between == "=" || between.starts_with("extends") {
                    opens.push(at + keyword.len() + name_len + brace);
                }
            }
        }
        let named_count = opens.len();
        // An inline settings shape in a lib module: `settings: { ... }`.
        if file.path.starts_with("lib/")
            || file.path.starts_with("src/lib/")
            || file.path.contains("types")
        {
            for at in word_positions(code, "settings") {
                let rest = &code[at + "settings".len()..];
                let trimmed = rest.trim_start().trim_start_matches('?').trim_start();
                if let Some(after_colon) = trimmed.strip_prefix(':') {
                    if after_colon.trim_start().starts_with('{') {
                        let offset = code.len() - after_colon.trim_start().len();
                        opens.push(offset);
                    }
                }
            }
        }
        for (index, open) in opens.into_iter().enumerate() {
            if let Some((keys, _)) = object_keys(code, open) {
                for key in keys {
                    if key.len() >= 3 {
                        // A named type (found first in each file) names its
                        // file; an inline shape only fills in.
                        let named = index < named_count;
                        match fields.get(&key) {
                            Some((_, true)) => {}
                            Some((_, false)) if !named => {}
                            _ => {
                                fields.insert(key, (file.path.to_string(), named));
                            }
                        }
                    }
                }
            }
        }
    }
    fields
        .into_iter()
        .map(|(key, (path, _))| (key, path))
        .collect()
}

fn is_settings_path(path: &str) -> bool {
    path.to_lowercase().contains("setting")
}

/// Files whose mention of a setting is not a use of it: settings UI, types,
/// storage, seed, validation and tests.
fn is_settings_plumbing(path: &str) -> bool {
    let lower = path.to_lowercase();
    let name = lower.rsplit('/').next().unwrap_or(&lower);
    // The settings UI; a `lib/settings.ts` that reads a field is a reader.
    let settings_ui =
        is_settings_path(path) && !(lower.starts_with("lib/") || lower.starts_with("src/lib/"));
    settings_ui
        || is_test(path)
        || name.starts_with("types")
        || lower.contains("/types/")
        || name.starts_with("storage")
        || name.starts_with("seed")
        || name.starts_with("validat")
        || name.starts_with("schema")
}

/// True when the identifier at `at` is the key of an object literal or of a
/// type member (`field: value`, `field?: T`), not a read of the field.
fn is_key_position(code: &str, at: usize, len: usize) -> bool {
    let rest = code[at + len..].trim_start();
    let key = rest.starts_with(':') || rest.starts_with("?:");
    // `a ? field : b` reads it.
    key && prev_char(code, at) != Some(b'?')
}

/// Split `defaultProjectId` into lower-case words.
fn camel_words(name: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut current = String::new();
    for ch in name.chars() {
        if (ch.is_ascii_uppercase() || ch == '_') && !current.is_empty() {
            words.push(current.to_lowercase());
            current.clear();
        }
        if ch != '_' {
            current.push(ch);
        }
    }
    if !current.is_empty() {
        words.push(current.to_lowercase());
    }
    words
}

fn settings_unread(app: &App, theme_flagged: bool) -> Vec<Lint> {
    let fields = settings_fields(app);
    let mut lints = Vec::new();
    for (field, declared) in &fields {
        let words = camel_words(field);
        let has = |word: &str| words.iter().any(|w| w == word);
        let is_theme = has("theme") || has("dark") || has("mode");
        if is_theme && theme_flagged {
            continue;
        }
        // Written in the settings UI.
        let written = app.scripts().any(|file| {
            is_settings_path(file.path) && !word_positions(&file.code, field).is_empty()
        });
        if !written {
            continue;
        }
        let read = app.scripts().any(|file| {
            !is_settings_plumbing(file.path)
                && word_positions(&file.code, field)
                    .into_iter()
                    .any(|at| !is_key_position(&file.code, at, field.len()))
        });
        if read {
            continue;
        }
        let pages: Vec<&File> = app
            .pages()
            .into_iter()
            .map(|(_, file)| file)
            .filter(|file| !is_settings_path(file.path))
            .collect();
        let pages_with = |part: &str| -> Vec<&File> {
            pages
                .iter()
                .copied()
                .filter(|file| file.path.to_lowercase().contains(part))
                .collect()
        };
        let (targets, hint): (Vec<&File>, &str) = if is_theme {
            (
                app.state_owner().into_iter().collect(),
                "apply it to `document.documentElement` from the state that every page loads, so \
                 the theme holds on every page and after a reload, not only on the Settings page",
            )
        } else if has("currency") || has("locale") {
            let money: Vec<&File> = pages
                .iter()
                .copied()
                .filter(|file| {
                    let code = &file.code;
                    [
                        "formatMoney",
                        "formatCents",
                        "formatCurrency",
                        "formatAmount",
                        "money(",
                    ]
                    .iter()
                    .any(|name| code.contains(name))
                })
                .collect();
            (
                money,
                "pass it to every money formatting call in this file in place of a fixed currency",
            )
        } else if has("project") {
            // A dynamic board (`app/projects/[id]/board`) always has a
            // project in its URL: the page that links to it (the project
            // list, else the home page) should pick the default.
            let dynamic = |file: &&File| file.path.contains('[');
            let board = pages_with("board");
            let fixed: Vec<&File> = board.iter().copied().filter(|f| !dynamic(f)).collect();
            if !fixed.is_empty() || board.is_empty() {
                let found = if board.is_empty() {
                    pages_with("project")
                } else {
                    fixed
                };
                (
                    found,
                    "open this page on that project when the URL or the user chose none (an \
                         explicit choice still wins)",
                )
            } else {
                let mut linking: Vec<&File> = pages_with("project")
                    .into_iter()
                    .filter(|file| !dynamic(file))
                    .collect();
                if linking.is_empty() {
                    linking = pages
                        .iter()
                        .copied()
                        .filter(|file| matches!(file.path, "app/page.tsx" | "src/app/page.tsx"))
                        .collect();
                }
                (
                    linking,
                    "make the board link (or the button that opens the board) go to the board \
                         of that project when the user has not picked another one, and show it \
                         first",
                )
            }
        } else if has("month") || has("week") || has("day") || has("period") {
            let mut found = pages_with("budget");
            if found.is_empty() {
                found = pages_with("calendar");
            }
            (
                found,
                "use it where this page computes its period (the first day of the month or week \
                 that the setting names), through the lib function that computes the period",
            )
        } else {
            let found: Vec<&File> = pages
                .iter()
                .copied()
                .filter(|file| {
                    words
                        .iter()
                        .filter(|word| word.len() >= 4 && *word != "default")
                        .any(|word| file.path.to_lowercase().contains(word.as_str()))
                })
                .collect();
            (
                found,
                "use it where this page shows what the setting controls",
            )
        };
        let message = |target: &str| {
            format!(
                "the setting `{field}` (declared in `{declared}`) is changed on the Settings page, \
                 but no file outside settings, storage, seed and types reads it, so changing it \
                 does nothing. In `{target}`, read `{field}` from the same stored settings that the \
                 Settings page saves and {hint}."
            )
        };
        if targets.is_empty() {
            let settings_page = app
                .scripts()
                .find(|file| {
                    is_settings_path(file.path) && !word_positions(&file.code, field).is_empty()
                })
                .map(|file| file.path.to_string())
                .unwrap_or_else(|| declared.clone());
            lints.push(Lint {
                level: Level::Warn,
                code: "TS9010",
                name: "settings-unread",
                line: 1,
                message: message("the page that the setting controls"),
                path: settings_page,
            });
            continue;
        }
        for target in targets.into_iter().take(6) {
            lints.push(Lint {
                level: Level::Error,
                code: "TS9010",
                name: "settings-unread",
                path: target.path.to_string(),
                line: 1,
                message: message(target.path),
            });
        }
    }
    lints
}

// ---- literal-case ----------------------------------------------------------

/// A quoted string at `at` (`'x'` or `"x"`): its text and end offset.
fn quoted_at(text: &str, at: usize) -> Option<(&str, usize)> {
    let quote = *text.as_bytes().get(at)?;
    if quote != b'\'' && quote != b'"' {
        return None;
    }
    let end = text[at + 1..].find(quote as char)? + at + 1;
    let inner = &text[at + 1..end];
    (!inner.contains('\n')).then_some((inner, end + 1))
}

/// Parse `'a' | 'b' | 'c'` at the start of `text`: the literals.
fn union_at(text: &str) -> Option<Vec<String>> {
    let mut rest = text.trim_start();
    let mut literals = Vec::new();
    loop {
        rest = rest.trim_start().trim_start_matches('|').trim_start();
        let (inner, end) = quoted_at(rest, 0)?;
        literals.push(inner.to_string());
        rest = &rest[end..];
        if !rest.trim_start().starts_with('|') {
            break;
        }
    }
    (literals.len() >= 2).then_some(literals)
}

/// The string-literal unions of the app: field name -> each declaration's
/// (literals, file), a shared types file's first.
fn field_unions(app: &App) -> BTreeMap<String, Vec<(Vec<String>, String)>> {
    // Every declaration of each union type name: (file, literals). Two
    // modules may declare different `Status` types (display labels in one,
    // keys in the other), so a name resolves through the file's own
    // declaration or its import, and only falls back to every declaration.
    let mut named: BTreeMap<String, Vec<(String, Vec<String>)>> = BTreeMap::new();
    for file in app.scripts() {
        for at in word_positions(&file.code, "type") {
            let rest = &file.code[at + 4..];
            let name_len = rest
                .trim_start()
                .bytes()
                .take_while(|byte| is_ident(*byte))
                .count();
            let trimmed = rest.trim_start();
            let name = &trimmed[..name_len];
            let after = trimmed[name_len..].trim_start();
            if let Some(value) = after.strip_prefix('=') {
                if let Some(literals) = union_at(value) {
                    named
                        .entry(name.to_string())
                        .or_default()
                        .push((file.path.to_string(), literals));
                }
            }
        }
    }
    // The declarations a type name in `file` can mean.
    let resolve_type = |file: &File, name: &str| -> Vec<Vec<String>> {
        let Some(declarations) = named.get(name) else {
            return Vec::new();
        };
        if let Some((_, literals)) = declarations.iter().find(|(path, _)| path == file.path) {
            return vec![literals.clone()];
        }
        for (names, spec) in imports(&file.code) {
            if !names.iter().any(|imported| imported == name) {
                continue;
            }
            if let Some(module) = resolve(app, file.path, &spec) {
                if let Some((_, literals)) =
                    declarations.iter().find(|(path, _)| path == module.path)
                {
                    return vec![literals.clone()];
                }
            }
        }
        declarations
            .iter()
            .map(|(_, literals)| literals.clone())
            .collect()
    };
    let mut fields: BTreeMap<String, Vec<(Vec<String>, String)>> = BTreeMap::new();
    for file in app.scripts() {
        let code = &file.code;
        let bytes = code.as_bytes();
        for (at, _) in code.match_indices(':') {
            // `field: 'a' | 'b'` or `field?: Status`.
            let mut start = at;
            if start > 0 && bytes[start - 1] == b'?' {
                start -= 1;
            }
            let name_end = start;
            let name_start = code[..name_end]
                .bytes()
                .rev()
                .take_while(|byte| is_ident(*byte))
                .count();
            if name_start == 0 {
                continue;
            }
            let name = &code[name_end - name_start..name_end];
            if name.chars().next().is_some_and(|ch| ch.is_ascii_digit()) {
                continue;
            }
            let value = &code[at + 1..];
            let all = match union_at(value) {
                Some(literals) => vec![literals],
                None => {
                    let trimmed = value.trim_start();
                    let len = trimmed.bytes().take_while(|byte| is_ident(*byte)).count();
                    let after = trimmed[len..].trim_start();
                    // Only the type itself, not `Status[]` or a generic.
                    if len > 0
                        && (after.starts_with(';')
                            || after.starts_with(',')
                            || after.starts_with('}')
                            || after.starts_with('\n')
                            || after.starts_with(')')
                            || after.is_empty())
                    {
                        resolve_type(file, &trimmed[..len])
                    } else {
                        Vec::new()
                    }
                }
            };
            for literals in all {
                // The shared types file's declaration comes first.
                let types_file = file
                    .path
                    .rsplit('/')
                    .next()
                    .is_some_and(|name| name.starts_with("types"));
                let list = fields.entry(name.to_string()).or_default();
                if types_file {
                    list.insert(0, (literals, file.path.to_string()));
                } else {
                    list.push((literals, file.path.to_string()));
                }
            }
        }
    }
    fields
}

/// Lower case, letters and digits only: "In Progress" and "in-progress"
/// give the same text.
fn norm(text: &str) -> String {
    text.chars()
        .filter(char::is_ascii_alphanumeric)
        .map(|ch| ch.to_ascii_lowercase())
        .collect()
}

fn literal_case(app: &App) -> Vec<Lint> {
    let unions = field_unions(app);
    if unions.is_empty() {
        return Vec::new();
    }
    let mut lints = Vec::new();
    let mut seen: BTreeSet<(String, String)> = BTreeSet::new();
    for file in app.scripts() {
        let code = &file.code;
        let mut found: Vec<(usize, String, String)> = Vec::new();
        for op in ["===", "!==", "==", "!="] {
            for (at, _) in code.match_indices(op) {
                // `==` inside `===`.
                if op.len() == 2
                    && (code[at..].starts_with("===")
                        || code[at..].starts_with("!==")
                        || (at > 0 && matches!(code.as_bytes()[at - 1], b'=' | b'!')))
                {
                    continue;
                }
                let right = code[at + op.len()..].trim_start();
                let right_at = code.len() - right.len();
                // `x.field === 'L'`
                if let Some((literal, _)) = quoted_at(code, right_at) {
                    let left = code[..at].trim_end();
                    let len = left
                        .bytes()
                        .rev()
                        .take_while(|byte| is_ident(*byte))
                        .count();
                    // A field access only (`task.status`): a bare
                    // parameter is often a label-to-key mapper's input.
                    let head = &left[..left.len() - len];
                    if len > 0 && head.ends_with('.') {
                        found.push((
                            at,
                            left[left.len() - len..].to_string(),
                            literal.to_string(),
                        ));
                    }
                    continue;
                }
                // `'L' === x.field`
                let left = code[..at].trim_end();
                if left.ends_with('\'') || left.ends_with('"') {
                    let quote = left.as_bytes()[left.len() - 1] as char;
                    if let Some(open) = left[..left.len() - 1].rfind(quote) {
                        let literal = &left[open + 1..left.len() - 1];
                        let expr_len = right
                            .bytes()
                            .take_while(|byte| is_ident(*byte) || *byte == b'.' || *byte == b'?')
                            .count();
                        let expr = &right[..expr_len];
                        let field = expr
                            .contains('.')
                            .then(|| expr.rsplit('.').next())
                            .flatten();
                        if let Some(field) = field.filter(|f| !f.is_empty()) {
                            if !literal.contains('\n') {
                                found.push((
                                    at,
                                    field.trim_end_matches('?').to_string(),
                                    literal.to_string(),
                                ));
                            }
                        }
                    }
                }
            }
        }
        // `case 'L':` in `switch (x.field)`.
        for at in word_positions(code, "case") {
            let rest = code[at + 4..].trim_start();
            let literal_at = code.len() - rest.len();
            let Some((literal, _)) = quoted_at(code, literal_at) else {
                continue;
            };
            let Some(switch) = code[..at].rfind("switch") else {
                continue;
            };
            let Some(open) = code[switch..].find('(').map(|o| switch + o) else {
                continue;
            };
            let Some(close) = matching(code, open) else {
                continue;
            };
            let expr = code[open + 1..close].trim();
            if !expr.contains('.') {
                continue;
            }
            if let Some(field) = expr.rsplit('.').next() {
                found.push((at, field.to_string(), literal.to_string()));
            }
        }
        // The modules this file imports: a field declared in a module it
        // does not import (a board's label-valued `status`) does not type
        // its records.
        let imported: BTreeSet<&str> = imports(&file.code)
            .into_iter()
            .filter_map(|(_, spec)| resolve(app, file.path, &spec).map(|module| module.path))
            .collect();
        // (line, field, literal) of every comparison: a defensive
        // `t.status === 'done' || t.status === 'Done'` already holds the
        // type's value and is no defect.
        let compared: BTreeSet<(usize, String, String)> = found
            .iter()
            .map(|(at, field, literal)| (file.line_of(*at), field.clone(), literal.clone()))
            .collect();
        for (at, field, literal) in found {
            let Some(all) = unions.get(&field) else {
                continue;
            };
            let in_scope: Vec<(Vec<String>, String)> = all
                .iter()
                .filter(|(_, declared)| {
                    declared == file.path
                        || imported.contains(declared.as_str())
                        || declared
                            .rsplit('/')
                            .next()
                            .is_some_and(|name| name.starts_with("types"))
                })
                .cloned()
                .collect();
            let declarations = if in_scope.is_empty() { all } else { &in_scope };
            // A value of any declaration of this field in scope is fine (a
            // column type that holds the labels, say).
            if declarations
                .iter()
                .any(|(literals, _)| literals.contains(&literal))
            {
                continue;
            }
            let Some((literals, declared, meant)) =
                declarations.iter().find_map(|(literals, declared)| {
                    literals
                        .iter()
                        .find(|known| norm(known) == norm(&literal))
                        .map(|meant| (literals, declared, meant))
                })
            else {
                continue;
            };
            if compared.contains(&(file.line_of(at), field.clone(), meant.clone())) {
                continue;
            }
            if !seen.insert((file.path.to_string(), literal.clone())) {
                continue;
            }
            let union = literals
                .iter()
                .map(|known| format!("\"{known}\""))
                .collect::<Vec<_>>()
                .join(" | ");
            lints.push(Lint {
                level: Level::Error,
                code: "TS9011",
                name: "literal-case",
                path: file.path.to_string(),
                line: file.line_of(at),
                message: format!(
                    "this compares `{field}` with \"{literal}\", but `{field}` holds {union} (declared \
                     in `{declared}`), so the test never matches and the result is wrong. Compare \
                     with \"{meant}\", the value in the type; keep \"{literal}\" only as text shown \
                     to the user."
                ),
            });
        }
    }
    lints
}

// ---- theme-selector --------------------------------------------------------

const DARK_CLASSES: &[&str] = &["theme-dark", "dark-theme", "dark-mode", "dark"];

/// The dark-theme class names that `css` defines (`.theme-dark`, `.dark`).
fn css_dark_classes(css: &str) -> BTreeSet<&'static str> {
    DARK_CLASSES
        .iter()
        .copied()
        .filter(|class| {
            css.match_indices(&format!(".{class}")).any(|(at, _)| {
                let end = at + 1 + class.len();
                css.as_bytes()
                    .get(end)
                    .is_none_or(|byte| !(is_ident(*byte) || *byte == b'-'))
            })
        })
        .collect()
}

/// The byte ranges of `code` where a class name is set: `className={...}`,
/// `className="..."`, `classList.x(...)`, and `className = ...;`.
fn class_spans(code: &str) -> Vec<(usize, usize)> {
    let mut spans = Vec::new();
    for (at, _) in code.match_indices("className") {
        let rest = &code[at + "className".len()..];
        let trimmed = rest.trim_start();
        let offset = code.len() - trimmed.len();
        if let Some(value) = trimmed.strip_prefix('=') {
            let value_trim = value.trim_start();
            let start = code.len() - value_trim.len();
            if value_trim.starts_with('{') {
                if let Some(close) = matching(code, start) {
                    spans.push((start, close));
                }
            } else if value_trim.starts_with('"') || value_trim.starts_with('\'') {
                if let Some((_, end)) = quoted_at(code, start) {
                    spans.push((start, end));
                }
            } else {
                // `el.className = dark ? "dark" : "";`
                let end = code[start..]
                    .find([';', '\n'])
                    .map_or(code.len(), |end| start + end);
                spans.push((offset, end));
            }
        }
    }
    for (at, _) in code.match_indices("classList.") {
        if let Some(open) = code[at..].find('(').map(|open| at + open) {
            if let Some(close) = matching(code, open) {
                spans.push((open, close));
            }
        }
    }
    spans
}

/// How a file sets the theme: the data attribute, and the dark classes it
/// sets (`theme-dark` from `` `theme-${theme}` `` too). A quoted class
/// counts only where it is set, not compared (`theme === "dark"`); the
/// generic `dark` only where a class is set (`className`, `classList`).
fn theme_setters(file: &File) -> (Option<usize>, BTreeMap<&'static str, usize>) {
    let code = &file.code;
    let attribute = [
        "dataset.theme",
        "\"data-theme\"",
        "'data-theme'",
        "data-theme=",
    ]
    .iter()
    .filter_map(|needle| code.find(needle))
    .min();
    let mut classes = BTreeMap::new();
    if !(code.contains("className") || code.contains("classList")) {
        return (attribute, classes);
    }
    let spans = class_spans(code);
    let bytes = code.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        let Some((inner, end)) = quoted_at(code, index) else {
            index += 1;
            continue;
        };
        let before = code[..index].trim_end();
        let compared = ["===", "!==", "==", "!=", "case"]
            .iter()
            .any(|op| before.ends_with(op));
        let in_class = spans.iter().any(|(from, to)| *from <= index && index < *to);
        if !compared && inner.len() < 60 {
            for class in DARK_CLASSES {
                if inner.split_whitespace().any(|token| token == *class)
                    && (*class != "dark" || in_class)
                {
                    classes.entry(*class).or_insert(index);
                }
            }
        }
        index = end;
    }
    if let Some(at) = code.find("theme-${") {
        classes.entry("theme-dark").or_insert(at);
    }
    (attribute, classes)
}

fn theme_selector(app: &App) -> Vec<Lint> {
    let css: String = app
        .files
        .iter()
        .filter(|file| file.path.ends_with(".css"))
        .map(|file| file.code.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    let css_attr = css.contains("[data-theme");
    let css_classes = css_dark_classes(&css);
    if !css_attr && css_classes.is_empty() {
        return Vec::new();
    }
    let mut attr_files = Vec::new();
    let mut class_files = Vec::new();
    for file in app.scripts() {
        let (attribute, classes) = theme_setters(file);
        if let Some(at) = attribute {
            attr_files.push((file, at));
        }
        if !classes.is_empty() {
            class_files.push((file, classes));
        }
    }
    let class_match = class_files
        .iter()
        .any(|(_, classes)| classes.keys().any(|class| css_classes.contains(class)));
    let css_text = if css_attr {
        "`[data-theme=\"dark\"]`".to_string()
    } else {
        css_classes
            .iter()
            .map(|class| format!("`.{class}`"))
            .collect::<Vec<_>>()
            .join(" and ")
    };
    let mut lints = Vec::new();
    let push = |lints: &mut Vec<Lint>, file: &File, at: usize, what: &str, change: &str| {
        lints.push(Lint {
            level: Level::Error,
            code: "TS9012",
            name: "theme-selector",
            path: file.path.to_string(),
            line: file.line_of(at),
            message: format!(
                "the style sheet gives the dark theme only through {css_text}, but this file {what}, \
                 which no style rule reads, so choosing the dark theme changes nothing. {change}"
            ),
        });
    };
    // Each file that sets the theme the wrong way: (file, first offset, what it sets).
    let mut wrong: BTreeMap<&str, (&File, usize, Vec<String>)> = BTreeMap::new();
    if css_attr {
        if attr_files.is_empty() && !class_match {
            for (file, classes) in &class_files {
                let entry = wrong
                    .entry(file.path)
                    .or_insert((*file, usize::MAX, Vec::new()));
                for (class, at) in classes {
                    entry.1 = entry.1.min(*at);
                    entry.2.push(format!("the class `{class}`"));
                }
            }
        }
    } else if !class_match {
        for (file, at) in &attr_files {
            let entry = wrong
                .entry(file.path)
                .or_insert((*file, usize::MAX, Vec::new()));
            entry.1 = entry.1.min(*at);
            entry.2.push("`data-theme`".to_string());
        }
        for (file, classes) in &class_files {
            let entry = wrong
                .entry(file.path)
                .or_insert((*file, usize::MAX, Vec::new()));
            for (class, at) in classes {
                entry.1 = entry.1.min(*at);
                entry.2.push(format!("the class `{class}`"));
            }
        }
    }
    let change = if css_attr {
        "Set the theme as `document.documentElement.dataset.theme = theme` (\"light\" or \"dark\") \
         instead."
            .to_string()
    } else {
        let wanted = css_classes.iter().next().copied().unwrap_or("theme-dark");
        format!(
            "Set the class the style sheet reads instead: \
             `document.documentElement.classList.toggle(\"{wanted}\", theme === \"dark\")`."
        )
    };
    for (file, at, what) in wrong.values() {
        push(
            &mut lints,
            file,
            *at,
            &format!("sets {}", what.join(" and ")),
            &change,
        );
    }
    // A dark style that nothing applies, while the app has a theme setting.
    if lints.is_empty() && attr_files.is_empty() && class_files.is_empty() {
        let has_setting = app
            .scripts()
            .any(|file| is_settings_path(file.path) && file.code.contains("theme"));
        if let (true, Some(owner)) = (has_setting, app.state_owner()) {
            let change = if css_attr {
                "Set `document.documentElement.dataset.theme` from the stored theme in an effect here."
                    .to_string()
            } else {
                let wanted = css_classes.iter().next().copied().unwrap_or("theme-dark");
                format!(
                    "Toggle `{wanted}` on `document.documentElement` from the stored theme in an \
                     effect here."
                )
            };
            lints.push(Lint {
                level: Level::Error,
                code: "TS9012",
                name: "theme-selector",
                path: owner.path.to_string(),
                line: 1,
                message: format!(
                    "the style sheet gives the dark theme through {css_text}, but no file applies the \
                     theme setting to the page, so choosing the dark theme changes nothing. This file \
                     holds the state every page loads. {change}"
                ),
            });
        }
    }
    lints
}

// ---- dead-route ------------------------------------------------------------

/// The internal link targets in `code`: (offset, target as written, up to
/// the query string; a template part is `*`).
fn link_targets(code: &str) -> Vec<(usize, String)> {
    let mut targets = Vec::new();
    let starts = ["href=", "href:", "push(", "replace(", "redirect("];
    for start in starts {
        for (at, _) in code.match_indices(start) {
            if start.ends_with('(')
                && !(start == "redirect("
                    || code[..at].ends_with("router.")
                    || code[..at].ends_with("router?."))
            {
                continue;
            }
            let rest = code[at + start.len()..].trim_start();
            let rest = rest.strip_prefix('{').map_or(rest, str::trim_start);
            let Some(quote) = rest
                .chars()
                .next()
                .filter(|ch| matches!(ch, '"' | '\'' | '`'))
            else {
                continue;
            };
            let body = &rest[1..];
            let Some(end) = body.find(quote) else {
                continue;
            };
            let mut target = body[..end].to_string();
            if !target.starts_with('/') || target.starts_with("//") {
                continue;
            }
            // `"/tasks/" + id`: the last part is dynamic.
            let concatenated = body[end + 1..].trim_start().starts_with('+');
            if let Some(cut) = target.find(['?', '#']) {
                target.truncate(cut);
            } else if concatenated && target.ends_with('/') {
                target.push('*');
            }
            let mut out = String::new();
            let mut chars = target.chars().peekable();
            while let Some(ch) = chars.next() {
                if ch == '$' && chars.peek() == Some(&'{') {
                    let mut depth = 0;
                    for inner in chars.by_ref() {
                        if inner == '{' {
                            depth += 1;
                        } else if inner == '}' {
                            depth -= 1;
                            if depth == 0 {
                                break;
                            }
                        }
                    }
                    out.push('*');
                } else {
                    out.push(ch);
                }
            }
            targets.push((at, out));
        }
    }
    targets
}

/// Literal segments that a dynamic `[id]` page reads as an id although a
/// link meant a page of their own.
const ACTION_SEGMENTS: &[&str] = &["new", "create", "add", "edit"];

fn dead_route(app: &App) -> Vec<Lint> {
    let pages = app.pages();
    if pages.is_empty() {
        return Vec::new();
    }
    let routes: Vec<String> = pages
        .iter()
        .map(|(segments, _)| format!("/{}", segments.join("/")))
        .collect();
    let mut lints = Vec::new();
    let mut seen = BTreeSet::new();
    for file in app.scripts() {
        if !(file.path.starts_with("app/")
            || file.path.starts_with("src/")
            || file.path.starts_with("components/"))
        {
            continue;
        }
        for (at, target) in link_targets(&file.code) {
            if target.starts_with("/api")
                || target
                    .rsplit('/')
                    .next()
                    .is_some_and(|last| last.contains('.'))
            {
                continue;
            }
            let parts: Vec<&str> = target.split('/').filter(|part| !part.is_empty()).collect();
            if parts.iter().any(|part| part.contains('*') && *part != "*") {
                // `task-${id}`: a dynamic segment with a literal part.
                continue;
            }
            let mut through: Option<&File> = None;
            let served = pages.iter().any(|(segments, page)| {
                let catch_all = segments
                    .last()
                    .is_some_and(|last| last.starts_with("[..") || last.starts_with("[[.."));
                if !(segments.len() == parts.len()
                    || (catch_all && parts.len() >= segments.len() - 1))
                {
                    return false;
                }
                let mut via_dynamic = None;
                for (index, part) in parts.iter().enumerate() {
                    let Some(segment) = segments.get(index) else {
                        return catch_all;
                    };
                    if segment.starts_with('[') {
                        if ACTION_SEGMENTS.contains(part) {
                            via_dynamic = Some(*part);
                        }
                        if segment.starts_with("[..") || segment.starts_with("[[..") {
                            return true;
                        }
                        continue;
                    }
                    if *part != "*" && part != segment {
                        return false;
                    }
                }
                if let Some(word) = via_dynamic {
                    // The `[id]` page handles `new` itself?
                    let handles = page.code.contains(&format!("\"{word}\""))
                        || page.code.contains(&format!("'{word}'"));
                    if !handles {
                        through = Some(*page);
                        return false;
                    }
                }
                true
            });
            if served || !seen.insert((file.path.to_string(), target.clone())) {
                continue;
            }
            let why = match through {
                Some(page) => format!(
                    " (`{}` takes \"{}\" as an id and finds no record)",
                    page.path,
                    parts.last().unwrap_or(&"")
                ),
                None => String::new(),
            };
            lints.push(Lint {
                level: Level::Error,
                code: "TS9013",
                name: "dead-route",
                path: file.path.to_string(),
                line: file.line_of(at),
                message: format!(
                    "this links to `{target}`, but no page serves it{why}, so the user lands on a \
                     page that does not work. The pages are: {}. Link to one of them (for a form \
                     that adds a record, open the form on the list page itself).",
                    routes.join(", ")
                ),
            });
        }
    }
    lints
}

// ---- server-storage --------------------------------------------------------

/// The relative imports of `code`: (imported names, specifier).
fn imports(code: &str) -> Vec<(Vec<String>, String)> {
    let mut found = Vec::new();
    for at in word_positions(code, "import") {
        let rest = &code[at..];
        let Some(end) = rest.find(['\n', ';']).map(|end| {
            // Multi-line `import {\n a,\n b\n} from "x"`.
            if rest[..end].contains('{') && !rest[..end].contains('}') {
                rest.find('}')
                    .and_then(|close| rest[close..].find(['\n', ';']).map(|e| close + e))
                    .unwrap_or(end)
            } else {
                end
            }
        }) else {
            continue;
        };
        let statement = &rest[..end];
        let Some(from) = statement.rfind("from") else {
            continue;
        };
        let spec = statement[from + 4..]
            .trim()
            .trim_matches(|ch| ch == '"' || ch == '\'' || ch == ';');
        let head = &statement[6..from];
        let names: Vec<String> = head
            .replace(['{', '}'], ",")
            .split(',')
            .filter_map(|part| {
                let part = part.trim().trim_start_matches("type ").trim();
                let name = part.rsplit(" as ").next()?.trim();
                (!name.is_empty() && name != "*").then(|| name.to_string())
            })
            .collect();
        found.push((names, spec.to_string()));
    }
    found
}

/// The project path that `spec`, imported from `from`, names, if the app
/// has it.
fn resolve<'a, 'b>(app: &'b App<'a>, from: &str, spec: &str) -> Option<&'b File<'a>> {
    let base: Vec<&str> = if let Some(rest) = spec.strip_prefix("@/") {
        rest.split('/').collect()
    } else if spec.starts_with('.') {
        let mut parts: Vec<&str> = from.split('/').collect();
        parts.pop();
        for part in spec.split('/') {
            match part {
                "." | "" => {}
                ".." => {
                    parts.pop();
                }
                other => parts.push(other),
            }
        }
        parts
    } else {
        return None;
    };
    let joined = base.join("/");
    ["", ".ts", ".tsx", "/index.ts", "/index.tsx"]
        .iter()
        .find_map(|ext| app.get(&format!("{joined}{ext}")))
        .or_else(|| {
            // `@/` may also map to `src/`.
            spec.strip_prefix("@/").and_then(|_| {
                ["", ".ts", ".tsx"]
                    .iter()
                    .find_map(|ext| app.get(&format!("src/{joined}{ext}")))
            })
        })
}

fn server_storage(app: &App) -> Vec<Lint> {
    let mut lints = Vec::new();
    for file in app.scripts() {
        let name = file.path.rsplit('/').next().unwrap_or_default();
        let is_route_file = (file.path.starts_with("app/") || file.path.starts_with("src/app/"))
            && matches!(name, "page.tsx" | "page.ts" | "layout.tsx" | "template.tsx");
        if !is_route_file || crate::gate::is_client(file.raw) || crate::gate::needs_client(file.raw)
        {
            continue;
        }
        // Code only: a theme script in a string runs in the browser.
        let code = blank_strings(&file.code);
        let direct = ["localStorage", "sessionStorage", "window."]
            .iter()
            .filter_map(|needle| code.find(needle).map(|at| (at, needle)))
            .filter(|(at, _)| !code[..*at].ends_with("typeof "))
            .min();
        let mut via: Option<(usize, String, String)> = None;
        if direct.is_none() {
            for (names, spec) in imports(&file.code) {
                let Some(module) = resolve(app, file.path, &spec) else {
                    continue;
                };
                // Code only, as for the page: a theme script returned as a
                // string runs in the browser.
                if !blank_strings(&module.code).contains("localStorage") {
                    continue;
                }
                for name in names {
                    let called = word_positions(&code, &name)
                        .into_iter()
                        .find(|at| code[at + name.len()..].trim_start().starts_with('('));
                    if let Some(at) = called {
                        via = Some((at, name, module.path.to_string()));
                        break;
                    }
                }
                if via.is_some() {
                    break;
                }
            }
        }
        let (at, what) = match (direct, via) {
            (Some((at, needle)), _) => (at, format!("uses `{}`", needle.trim_end_matches('.'))),
            (None, Some((at, name, module))) => (
                at,
                format!("calls `{name}` from `{module}`, which reads `localStorage`"),
            ),
            _ => continue,
        };
        // A layout (or any route file exporting `metadata`) must stay a
        // Server Component: `metadata` in a "use client" file fails the build.
        let keep_server = name.starts_with("layout.")
            || name.starts_with("template.")
            || file.code.contains("export const metadata")
            || file.code.contains("export async function generateMetadata")
            || file.code.contains("export function generateMetadata");
        let change = if keep_server {
            "Keep this file a Server Component (it exports `metadata` or wraps every page; \
             \"use client\" here fails the build): move the storage read into a client provider \
             or child component (\"use client\" as its first line) that reads the stored state \
             in `useEffect`, and render that component here."
        } else {
            "Make it a Client Component (\"use client\" as its first line, no `async` default \
             export, query values from `useSearchParams` inside `<Suspense>`) and read the \
             stored state in `useEffect` or through the app's state hook."
        };
        let kind = if name.starts_with("page.") {
            "page"
        } else {
            "route file"
        };
        lints.push(Lint {
            level: Level::Error,
            code: "TS9014",
            name: "server-storage",
            path: file.path.to_string(),
            line: file.line_of(at),
            message: format!(
                "this {kind} is a Server Component (no \"use client\") and {what}. On the server \
                 there is no storage, so it renders the seed or nothing and never the user's data. \
                 {change}"
            ),
        });
    }
    lints
}

// ---- partial-save ----------------------------------------------------------

/// The fields of the interface or object type `name`, if the app declares it.
fn type_fields(app: &App, name: &str) -> Option<(Vec<String>, String)> {
    for file in app.scripts() {
        for keyword in ["interface", "type"] {
            for at in word_positions(&file.code, keyword) {
                let rest = file.code[at + keyword.len()..].trim_start();
                if !rest.starts_with(name)
                    || rest
                        .as_bytes()
                        .get(name.len())
                        .is_some_and(|b| is_ident(*b))
                {
                    continue;
                }
                let after = &rest[name.len()..];
                let Some(brace) = after.find('{') else {
                    continue;
                };
                let between = after[..brace].trim();
                if !(between.is_empty() || between == "=" || between.starts_with("extends")) {
                    continue;
                }
                let open = file.code.len() - after.len() + brace;
                if let Some((keys, _)) = object_keys(&file.code, open) {
                    if !keys.is_empty() {
                        return Some((keys, file.path.to_string()));
                    }
                }
            }
        }
    }
    None
}

/// The declared type of the first parameter of function `name`, if the app
/// declares one.
fn first_param_type(app: &App, name: &str) -> Option<String> {
    for file in app.scripts() {
        for at in word_positions(&file.code, name) {
            let before = file.code[..at].trim_end();
            let declares = before.ends_with("function")
                || before.ends_with("const")
                || before.ends_with("let");
            if !declares {
                continue;
            }
            let rest = &file.code[at + name.len()..];
            let Some(open) = rest.find('(') else {
                continue;
            };
            if rest[..open].contains(['\n', ';']) {
                continue;
            }
            let params = &rest[open + 1..];
            let Some(close) = params.find([')', ',']) else {
                continue;
            };
            let Some((_, ty)) = params[..close].split_once(':') else {
                continue;
            };
            let ty = ty.trim();
            let len = ty.bytes().take_while(|byte| is_ident(*byte)).count();
            return (len > 0).then(|| ty[..len].to_string());
        }
    }
    None
}

/// True when function `name`'s own body merges its argument into the
/// stored state (`{...state, ...patch}`).
fn merges(app: &App, name: &str) -> bool {
    app.scripts().any(|file| {
        word_positions(&file.code, name).into_iter().any(|at| {
            let before = file.code[..at].trim_end();
            if !(before.ends_with("function") || before.ends_with("const")) {
                return false;
            }
            let rest = &file.code[at..];
            let Some(open) = rest.find('{') else {
                return false;
            };
            let end = matching(&file.code, at + open).unwrap_or(file.code.len());
            file.code[at + open..end].contains("...")
        })
    })
}

/// Key sets of the object literals in the seed files with at least three
/// keys: (keys, file).
fn seed_shapes<'a>(app: &'a App) -> Vec<(Vec<String>, &'a str)> {
    let mut shapes = Vec::new();
    for file in app.scripts() {
        let name = file.path.rsplit('/').next().unwrap_or_default();
        if !name.starts_with("seed") {
            continue;
        }
        for (at, _) in file.code.match_indices('{') {
            if let Some((keys, false)) = object_keys(&file.code, at) {
                if keys.len() >= 3 {
                    shapes.push((keys, file.path));
                }
            }
        }
    }
    shapes
}

fn partial_save(app: &App) -> Vec<Lint> {
    let seeds = seed_shapes(app);
    let mut lints = Vec::new();
    for file in app.scripts() {
        let name = file.path.rsplit('/').next().unwrap_or_default();
        if name.starts_with("seed") || name.starts_with("storage") {
            continue;
        }
        let code = &file.code;
        let mut calls: Vec<(usize, String)> = Vec::new();
        for (at, _) in code.match_indices("localStorage.setItem(") {
            calls.push((at, "localStorage.setItem".to_string()));
        }
        for prefix in ["save", "persist"] {
            for (at, _) in code.match_indices(prefix) {
                if (at > 0 && is_ident(code.as_bytes()[at - 1])) || code[..at].ends_with('.') {
                    continue;
                }
                let len = code[at..]
                    .bytes()
                    .take_while(|byte| is_ident(*byte))
                    .count();
                let callee = &code[at..at + len];
                if code[at + len..].starts_with('(') && !code[..at].trim_end().ends_with("function")
                {
                    calls.push((at, callee.to_string()));
                }
            }
        }
        for (at, callee) in calls {
            let Some(open) = code[at..].find('(').map(|open| at + open) else {
                continue;
            };
            let Some(close) = matching(code, open) else {
                continue;
            };
            let args = &code[open + 1..close];
            // The object literal argument, maybe inside `JSON.stringify(`.
            let Some(brace) = args.find('{') else {
                continue;
            };
            let before = args[..brace].trim_end();
            let literal_arg =
                before.is_empty() || before.ends_with(',') || before.ends_with("JSON.stringify(");
            if !literal_arg || args[..brace].contains("=>") {
                continue;
            }
            let Some((keys, spread)) = object_keys(code, open + 1 + brace) else {
                continue;
            };
            if spread || keys.len() < 2 {
                continue;
            }
            let expected = if callee == "localStorage.setItem" {
                None
            } else {
                if merges(app, &callee) {
                    continue;
                }
                let ty = first_param_type(app, &callee);
                // A create/patch input type is not the whole stored state.
                let input = ty.as_deref().is_some_and(|ty| {
                    ty.starts_with("Partial")
                        || ty.starts_with("New")
                        || ty.starts_with("Create")
                        || [
                            "Input", "Patch", "Draft", "Update", "Params", "Options", "Fields",
                        ]
                        .iter()
                        .any(|suffix| ty.ends_with(suffix))
                });
                if input {
                    continue;
                }
                ty.filter(|ty| ty != "string" && ty != "unknown" && ty != "any")
                    .and_then(|ty| {
                        type_fields(app, &ty)
                            .map(|(fields, path)| (fields, format!("`{ty}` in `{path}`")))
                    })
            };
            let expected = expected.or_else(|| {
                seeds
                    .iter()
                    .filter(|(shape, _)| {
                        keys.iter().all(|key| shape.contains(key)) && shape.len() > keys.len()
                    })
                    .max_by_key(|(shape, _)| shape.len())
                    .map(|(shape, path)| (shape.clone(), format!("the seed in `{path}`")))
            });
            let Some((fields, source)) = expected else {
                continue;
            };
            if !keys.iter().all(|key| fields.contains(key)) {
                continue;
            }
            let missing: Vec<&String> = fields
                .iter()
                .filter(|field| !keys.contains(field))
                .collect();
            if missing.is_empty() {
                continue;
            }
            let missing_text = missing
                .iter()
                .map(|field| format!("`{field}`"))
                .collect::<Vec<_>>()
                .join(", ");
            lints.push(Lint {
                level: Level::Error,
                code: "TS9015",
                name: "partial-save",
                path: file.path.to_string(),
                line: file.line_of(at),
                message: format!(
                    "this saves `{{{}}}` as the whole stored state, but the stored state also holds \
                     {missing_text} ({source}), so this save deletes them and other pages lose their \
                     data. Save the whole state with the changed parts: `{{ ...state, {} }}`.",
                    keys.join(", "),
                    keys.join(", ")
                ),
            });
        }
    }
    lints
}

// ---- id-collision ----------------------------------------------------------

fn id_collision(app: &App) -> Vec<Lint> {
    let mut lints = Vec::new();
    for file in app.scripts() {
        if !(file.path.starts_with("lib/")
            || file.path.starts_with("src/lib/")
            || file.path.starts_with("utils/"))
        {
            continue;
        }
        let code = &file.code;
        let unique = [
            "Date.now",
            "Math.random",
            "randomUUID",
            "crypto",
            "performance.now",
            "new Date",
        ]
        .iter()
        .any(|needle| code.contains(needle));
        if unique {
            continue;
        }
        let mut offset = 0;
        for line in code.split_inclusive('\n') {
            let at = offset;
            offset += line.len();
            // A module-level counter: `let counter = 0;` at column 0.
            let rest = line.strip_prefix("export ").unwrap_or(line);
            let Some(rest) = rest.strip_prefix("let ") else {
                continue;
            };
            let len = rest.bytes().take_while(|byte| is_ident(*byte)).count();
            let name = &rest[..len];
            let value = rest[len..]
                .trim_start()
                .trim_start_matches(": number")
                .trim_start();
            let numeric = value
                .strip_prefix('=')
                .map(|value| value.trim().trim_end_matches(';').trim())
                .is_some_and(|value| value.parse::<i64>().is_ok());
            if len == 0 || !numeric {
                continue;
            }
            let bumped = [
                format!("++{name}"),
                format!("{name}++"),
                format!("{name} += 1"),
                format!("{name} = {name} + 1"),
            ]
            .iter()
            .any(|bump| code.contains(bump.as_str()));
            let stringified = code.contains('`')
                || code.contains("+ \"")
                || code.contains("+ '")
                || code.contains("String(");
            if !(bumped && stringified) {
                continue;
            }
            // Already fixed: the module raises the counter from stored ids
            // and the app calls that function (pl-2: the repair added
            // `syncIds` and its call, and the lint fired again for 4 rounds).
            if counter_synced(app, file.path, code, name) {
                break;
            }
            // Keep the id format: the tests often pin it (`"task-1"` after
            // a reset), so the fix moves the counter, not the format.
            // The provider or store that loads the state calls it (not a
            // root layout, which is a Server Component).
            let loader = app
                .state_owner()
                .filter(|owner| !owner.path.ends_with("layout.tsx") && owner.path != file.path);
            let owner = loader
                .map(|owner| format!("`{}`", owner.path))
                .unwrap_or_else(|| "the module that loads the stored state".to_string());
            if let Some(loader) = loader {
                let module = file.path.trim_end_matches(".tsx").trim_end_matches(".ts");
                lints.push(Lint {
                    level: Level::Error,
                    code: "TS9016",
                    name: "id-collision",
                    path: loader.path.to_string(),
                    line: 1,
                    message: format!(
                        "this file loads the stored state, but the id counter in `{}` does not \
                         know the stored ids, so after a reload new records reuse them. Import \
                         `syncIds` from `{module}` (in the import style this file already uses) \
                         and call it with every id of the loaded state \
                         (all records of every collection) right after the state loads; change \
                         nothing else.",
                        file.path
                    ),
                });
            }
            lints.push(Lint {
                level: Level::Error,
                code: "TS9016",
                name: "id-collision",
                path: file.path.to_string(),
                line: file.line_of(at),
                message: format!(
                    "new ids come from the counter `{name}`, which starts again at its first value \
                     on every page load, so a record created after a reload gets the id of a seed \
                     or stored record and edits the wrong one. Keep the id format and the existing \
                     functions as they are (tests expect them); add and export \
                     `syncIds(ids: string[])` here, which raises `{name}` to the highest number \
                     at the end of any of those ids, and call it with every stored id where the \
                     state loads ({owner})."
                ),
            });
            break;
        }
    }
    lints
}

/// The id module exports a function that sets the counter `name` from data
/// (`counter = n`, `counter = Math.max(...)`, not a reset to a literal or a
/// `+ 1` bump), and a non-test script outside the module calls it.
fn counter_synced(app: &App, module_path: &str, code: &str, name: &str) -> bool {
    let assigns = |body: &str| {
        let needle = format!("{name} = ");
        body.match_indices(needle.as_str()).any(|(at, _)| {
            let before = body[..at].chars().next_back();
            if before.is_some_and(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '.') {
                return false;
            }
            let rhs = body[at + needle.len()..].trim_start();
            let literal = rhs
                .split([';', '\n'])
                .next()
                .unwrap_or("")
                .trim()
                .parse::<i64>()
                .is_ok();
            !literal && !rhs.starts_with(&format!("{name} + 1"))
        })
    };
    let mut syncers = Vec::new();
    for (at, _) in code.match_indices("export function ") {
        let rest = &code[at + "export function ".len()..];
        let len = rest.bytes().take_while(|byte| is_ident(*byte)).count();
        if len == 0 {
            continue;
        }
        let body_end = rest[len..]
            .find("\nexport ")
            .map_or(rest.len(), |end| end + len);
        if assigns(&rest[len..body_end]) {
            syncers.push(rest[..len].to_string());
        }
    }
    if syncers.is_empty() {
        return false;
    }
    app.scripts().any(|other| {
        other.path != module_path
            && !other.path.contains(".test.")
            && syncers
                .iter()
                .any(|syncer| other.code.contains(&format!("{syncer}(")))
    })
}

// ---- seed-dates ------------------------------------------------------------

/// Offsets of quoted absolute ISO dates (`"2026-11-02`).
fn iso_dates(code: &str) -> Vec<usize> {
    let bytes = code.as_bytes();
    let mut found = Vec::new();
    for (at, _) in code.match_indices("20") {
        if at == 0 || !matches!(bytes[at - 1], b'"' | b'\'' | b'`') {
            continue;
        }
        let date = bytes.get(at..at + 10);
        let is_date = date.is_some_and(|date| {
            date.iter().enumerate().all(|(index, byte)| match index {
                4 | 7 => *byte == b'-',
                _ => byte.is_ascii_digit(),
            })
        });
        if is_date {
            found.push(at);
        }
    }
    found
}

fn seed_dates(app: &App) -> Vec<Lint> {
    let mut lints = Vec::new();
    for file in app.scripts() {
        let name = file.path.rsplit('/').next().unwrap_or_default();
        if !name.starts_with("seed") {
            continue;
        }
        let code = &file.code;
        let dates = iso_dates(code);
        let arithmetic = [
            "Date.now(",
            "new Date()",
            "setDate(",
            "setMonth(",
            "getTime() -",
            "getTime() +",
            "daysAgo",
            "daysFromNow",
            "addDays",
            "addMonths",
            "86400000",
            "86_400_000",
            "864e5",
        ]
        .iter()
        .any(|needle| code.contains(needle));
        if dates.len() <= 3 || arithmetic {
            continue;
        }
        let first = dates[0];
        lints.push(Lint {
            level: Level::Error,
            code: "TS9017",
            name: "seed-dates",
            path: file.path.to_string(),
            line: file.line_of(first),
            message: format!(
                "the seed has {} fixed dates (\"{}\" ...) and computes none from the current date, so \
                 the demo data goes stale: in a few weeks the figures of this week and this month, \
                 the calendar and the charts are empty or all overdue. Compute each date when the data \
                 loads, relative to today, through a small helper (`daysFromToday(n)` returning a \
                 `YYYY-MM-DD` string), keeping the same spread of past and future dates.",
                dates.len(),
                &code[first..first + 10]
            ),
        });
    }
    lints
}

// ---- readme-spec -----------------------------------------------------------

/// Phrases of the plan's file spec that do not belong in a README.
const SPEC_PHRASES: &[&str] = &[
    "this file does not declare",
    "Write each record as one compact",
    "Today is 20",
    "Compute every date when the data loads",
    "character for character",
    "Imports the parts of the global style sheet",
];

fn readme_spec(app: &App) -> Vec<Lint> {
    let Some(readme) = app
        .files
        .iter()
        .find(|file| file.path.eq_ignore_ascii_case("README.md"))
    else {
        return Vec::new();
    };
    let mut problems = Vec::new();
    let mut line = 1;
    for phrase in SPEC_PHRASES {
        if let Some(at) = readme.raw.find(phrase) {
            if problems.is_empty() {
                line = line_of(readme.raw, at);
            }
            problems.push(format!("\"{phrase}\""));
        }
    }
    let lines: Vec<&str> = readme.raw.lines().collect();
    let mut empty = Vec::new();
    for (index, text) in lines.iter().enumerate() {
        if !text.starts_with("## ") {
            continue;
        }
        let body_empty = lines[index + 1..]
            .iter()
            .take_while(|next| !next.starts_with('#'))
            .all(|next| {
                next.trim().is_empty()
                    || next.trim().starts_with("```")
                    || next.trim().starts_with("~~~")
            });
        if body_empty {
            if problems.is_empty() && empty.is_empty() {
                line = index + 1;
            }
            empty.push(format!("`{}`", text.trim()));
        }
    }
    if problems.is_empty() && empty.is_empty() {
        return Vec::new();
    }
    let mut what = Vec::new();
    if !problems.is_empty() {
        what.push(format!(
            "the generator's file notes ({})",
            problems.join(", ")
        ));
    }
    if !empty.is_empty() {
        what.push(format!("empty sections ({})", empty.join(", ")));
    }
    vec![Lint {
        level: Level::Warn,
        code: "TS9018",
        name: "readme-spec",
        path: readme.path.to_string(),
        line,
        message: format!(
            "the README holds {}: a reader gets the build plan, not a README. It should say what \
             the app is, its features, and how to install, run and test it.",
            what.join(" and ")
        ),
    }]
}

// ---- css-undefined-class ---------------------------------------------------

/// The class names that the style sheets define.
fn css_classes(app: &App) -> BTreeSet<String> {
    let mut classes = BTreeSet::new();
    for file in app.files.iter().filter(|file| file.path.ends_with(".css")) {
        let code = &file.code;
        let bytes = code.as_bytes();
        for (at, _) in code.match_indices('.') {
            let starts_name = bytes
                .get(at + 1)
                .is_some_and(|byte| byte.is_ascii_alphabetic() || *byte == b'_' || *byte == b'-');
            let after_number = at > 0 && bytes[at - 1].is_ascii_digit();
            if !starts_name || after_number {
                continue;
            }
            let len = bytes[at + 1..]
                .iter()
                .take_while(|byte| is_ident(**byte) || **byte == b'-')
                .count();
            classes.insert(code[at + 1..at + 1 + len].to_string());
        }
    }
    classes
}

/// A template's text with each `${...}` part replaced by a space.
fn without_placeholders(template: &str) -> String {
    let mut out = String::new();
    let mut depth = 0;
    let mut chars = template.chars().peekable();
    while let Some(ch) = chars.next() {
        if depth == 0 && ch == '$' && chars.peek() == Some(&'{') {
            chars.next();
            depth = 1;
            // Kept as a marker: a token around it is not a class name.
            out.push_str("${}");
            continue;
        }
        if depth > 0 {
            match ch {
                '{' => depth += 1,
                '}' => depth -= 1,
                _ => {}
            }
            continue;
        }
        out.push(ch);
    }
    out
}

fn css_undefined_class(app: &App) -> Vec<Lint> {
    let defined = css_classes(app);
    if defined.is_empty() {
        return Vec::new();
    }
    let mut lints = Vec::new();
    for file in app.scripts().filter(|file| file.path.ends_with(".tsx")) {
        let code = &file.code;
        let mut missing: BTreeMap<String, usize> = BTreeMap::new();
        for (at, _) in code.match_indices("className=") {
            let rest = &code[at + "className=".len()..];
            let values: Vec<String> = if rest.starts_with('"') || rest.starts_with('\'') {
                quoted_at(rest, 0)
                    .map(|(inner, _)| vec![inner.to_string()])
                    .unwrap_or_default()
            } else if rest.starts_with('{') {
                let open = at + "className=".len();
                let close = matching(code, open).unwrap_or(open);
                let expr = &code[open..close];
                // The quoted parts of the expression that are not compared,
                // and the literal text of a template (without `${...}`).
                let mut parts = Vec::new();
                let mut index = 0;
                while index < expr.len() {
                    match quoted_at(expr, index) {
                        Some((inner, end)) => {
                            let before = expr[..index].trim_end();
                            let compared = ["===", "!==", "==", "!="]
                                .iter()
                                .any(|op| before.ends_with(op));
                            if !compared {
                                parts.push(inner.to_string());
                            }
                            index = end;
                        }
                        None => index += 1,
                    }
                }
                if let Some(start) = expr.find('`') {
                    if let Some(end) = expr[start + 1..].find('`') {
                        parts.push(without_placeholders(&expr[start + 1..start + 1 + end]));
                    }
                }
                parts
            } else {
                Vec::new()
            };
            for value in values {
                for token in value.split_whitespace() {
                    let clean = token.contains("${") || token.contains('}') || token.contains('{');
                    let plain = token.bytes().all(|byte| is_ident(byte) || byte == b'-')
                        && token
                            .bytes()
                            .next()
                            .is_some_and(|byte| byte.is_ascii_alphabetic());
                    if clean || !plain || defined.contains(token) {
                        continue;
                    }
                    missing.entry(token.to_string()).or_insert(at);
                }
            }
        }
        if missing.is_empty() {
            continue;
        }
        let first = missing.values().copied().min().unwrap_or(0);
        let names: Vec<String> = missing
            .keys()
            .take(8)
            .map(|name| format!("`{name}`"))
            .collect();
        lints.push(Lint {
            level: Level::Warn,
            code: "TS9019",
            name: "css-undefined-class",
            path: file.path.to_string(),
            line: file.line_of(first),
            message: format!(
                "no style sheet defines the class {}{}: the element gets no style of its own. Use \
                 a class that the style sheet defines when one fits.",
                names.join(", "),
                if missing.len() > 8 { " (and more)" } else { "" }
            ),
        });
    }
    lints
}

// ---- qlint2: the conventions between files (TS9020-TS9025) ----------------
//
// From the second blind audit (5 race pairs, every quality switch on): each
// SlimToken file was plausible alone, but the files disagreed on a
// convention that no prompt stated once. Each lint below finds one such
// disagreement between two named places in the code, and names the side to
// change.

/// One function of a file (comments blanked): its name, where its name is,
/// its parameter list and its body (byte ranges of the code, the brackets
/// excluded), and whether it is exported.
struct Func {
    name: String,
    at: usize,
    params: (usize, usize),
    body: (usize, usize),
    exported: bool,
}

impl Func {
    fn body<'a>(&self, code: &'a str) -> &'a str {
        &code[self.body.0..self.body.1]
    }
    fn params<'a>(&self, code: &'a str) -> &'a str {
        &code[self.params.0..self.params.1]
    }
    /// The names of the parameters, in order.
    fn param_names(&self, code: &str) -> Vec<String> {
        split_top(self.params(code))
            .iter()
            .map(|param| {
                let param = param.trim().trim_start_matches("...");
                param
                    .bytes()
                    .take_while(|byte| is_ident(*byte))
                    .map(char::from)
                    .collect()
            })
            .collect()
    }
}

/// `text` split at the commas outside brackets and strings, trimmed.
fn split_top(text: &str) -> Vec<&str> {
    let bytes = text.as_bytes();
    let mut parts = Vec::new();
    let (mut depth, mut from, mut index) = (0i32, 0usize, 0usize);
    let mut quote: Option<u8> = None;
    while index < bytes.len() {
        let byte = bytes[index];
        if let Some(open) = quote {
            if byte == b'\\' {
                index += 2;
                continue;
            }
            if byte == open {
                quote = None;
            }
            index += 1;
            continue;
        }
        match byte {
            b'"' | b'\'' | b'`' => quote = Some(byte),
            b'(' | b'[' | b'{' | b'<' => depth += 1,
            b')' | b']' | b'}' => depth -= 1,
            b'>' if index > 0 && bytes[index - 1] != b'=' => depth -= 1,
            b',' if depth == 0 => {
                parts.push(text[from..index].trim());
                from = index + 1;
            }
            _ => {}
        }
        index += 1;
    }
    let last = text[from..].trim();
    if !last.is_empty() {
        parts.push(last);
    }
    parts
}

/// Skip a `<...>` type parameter list that opens at `at`.
fn skip_angles(code: &str, at: usize) -> usize {
    let bytes = code.as_bytes();
    let mut depth = 0i32;
    let mut index = at;
    while index < bytes.len() {
        match bytes[index] {
            b'<' => depth += 1,
            b'>' if bytes[index - 1] != b'=' => {
                depth -= 1;
                if depth == 0 {
                    return index + 1;
                }
            }
            b'\n' | b';' | b'{' => return at,
            _ => {}
        }
        index += 1;
    }
    at
}

/// The functions of `code` (comments blanked): `function f(...) {...}`,
/// `const f = (...) => ...` and `const f = function (...) {...}`, with an
/// optional `async` and return type.
fn functions(code: &str) -> Vec<Func> {
    let bytes = code.as_bytes();
    let mut found = Vec::new();
    for keyword in ["function", "const", "let"] {
        for at in word_positions(code, keyword) {
            let rest = &code[at + keyword.len()..];
            let name_at = code.len() - rest.trim_start().len();
            let name_len = bytes[name_at..]
                .iter()
                .take_while(|byte| is_ident(**byte))
                .count();
            if name_len == 0 {
                continue;
            }
            let name = &code[name_at..name_at + name_len];
            let mut index = name_at + name_len;
            let mut arrow = false;
            if keyword != "function" {
                let after = code[index..].trim_start();
                let Some(value) = after.strip_prefix('=') else {
                    continue;
                };
                if value.starts_with(['=', '>']) {
                    continue;
                }
                let mut value = value.trim_start();
                if let Some(rest) = value.strip_prefix("async") {
                    value = rest.trim_start();
                }
                if let Some(rest) = value.strip_prefix("function") {
                    value = rest.trim_start();
                } else {
                    arrow = true;
                }
                index = code.len() - value.len();
            } else {
                index = code.len() - code[index..].trim_start().len();
            }
            if bytes.get(index) == Some(&b'<') {
                index = skip_angles(code, index);
                index = code.len() - code[index..].trim_start().len();
            }
            if bytes.get(index) != Some(&b'(') {
                continue;
            }
            let Some(close) = matching(code, index) else {
                continue;
            };
            // The return type (a `{ ... }` type literal is skipped), then the
            // arrow and/or the body.
            let mut pos = close + 1;
            let mut body = None;
            let mut in_type = false;
            let mut saw_arrow = false;
            let mut depth = 0i32;
            while pos < bytes.len() {
                let byte = bytes[pos];
                match byte {
                    b':' if depth == 0 && !saw_arrow => in_type = true,
                    b'(' | b'[' | b'<' => depth += 1,
                    b')' | b']' => depth -= 1,
                    b'>' if bytes[pos - 1] == b'=' && depth == 0 => {
                        saw_arrow = true;
                        in_type = false;
                        let value = code[pos + 1..].trim_start();
                        let start = code.len() - value.len();
                        if !value.starts_with('{') {
                            // An expression body: to the end of the statement.
                            let end = if value.starts_with('(') {
                                matching(code, start).map_or(code.len(), |end| end + 1)
                            } else {
                                code[start..]
                                    .find([';', '\n'])
                                    .map_or(code.len(), |end| start + end)
                            };
                            body = Some((start, end));
                            break;
                        }
                    }
                    b'>' => depth -= 1,
                    b'{' if depth == 0 => {
                        let typed = in_type
                            && prev_char(code, pos).is_some_and(|ch| b":|&<,(".contains(&ch));
                        if typed {
                            pos = matching(code, pos).unwrap_or(code.len());
                        } else {
                            if arrow && !saw_arrow {
                                break;
                            }
                            let end = matching(code, pos).unwrap_or(code.len());
                            body = Some((pos + 1, end));
                            break;
                        }
                    }
                    b';' if depth == 0 => break,
                    _ => {}
                }
                pos += 1;
            }
            let Some(body) = body else {
                continue;
            };
            let before = code[..at].trim_end();
            let exported = before.ends_with("export")
                || before.ends_with("default")
                || (before.ends_with("async")
                    && code[..before.len() - 5].trim_end().ends_with("export"));
            found.push(Func {
                name: name.to_string(),
                at: name_at,
                params: (index + 1, close),
                body,
                exported,
            });
        }
    }
    found.sort_by_key(|func| func.at);
    found
}

/// The innermost function whose body holds `at`.
fn enclosing(funcs: &[Func], at: usize) -> Option<&Func> {
    funcs
        .iter()
        .filter(|func| func.body.0 <= at && at <= func.body.1)
        .min_by_key(|func| func.body.1 - func.body.0)
}

fn is_lib_path(path: &str) -> bool {
    path.starts_with("lib/") || path.starts_with("src/lib/")
}

fn is_ui_path(path: &str) -> bool {
    ["app/", "components/", "src/app/", "src/components/"]
        .iter()
        .any(|prefix| path.starts_with(prefix))
}

fn file_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

fn is_seed_path(path: &str) -> bool {
    file_name(path).starts_with("seed")
}

/// The root layout of the app.
fn root_layout<'b, 'a>(app: &'b App<'a>) -> Option<&'b File<'a>> {
    app.get("app/layout.tsx")
        .or_else(|| app.get("src/app/layout.tsx"))
        .or_else(|| app.get("app/layout.jsx"))
}

/// The offset of the `{` of the object literal that holds `at`, if any.
fn enclosing_object(code: &str, at: usize) -> Option<usize> {
    let bytes = code.as_bytes();
    let mut depth = 0i32;
    let mut index = at;
    while index > 0 {
        index -= 1;
        match bytes[index] {
            b'}' | b')' | b']' => depth += 1,
            b'(' | b'[' => {
                if depth == 0 {
                    return None;
                }
                depth -= 1;
            }
            b'{' => {
                if depth == 0 {
                    return Some(index);
                }
                depth -= 1;
            }
            _ => {}
        }
    }
    None
}

/// The callee of the call whose argument list holds `at` (`formatMoney`,
/// `Intl.NumberFormat`), looking through object literals.
fn enclosing_call(code: &str, at: usize) -> Option<&str> {
    let bytes = code.as_bytes();
    let mut depth = 0i32;
    let mut index = at;
    while index > 0 {
        index -= 1;
        match bytes[index] {
            b'}' | b')' | b']' => depth += 1,
            b'{' | b'[' => {
                if depth > 0 {
                    depth -= 1;
                }
            }
            b'(' => {
                if depth == 0 {
                    let before = code[..index].trim_end();
                    let start = before
                        .bytes()
                        .rev()
                        .take_while(|byte| is_ident(*byte) || *byte == b'.')
                        .count();
                    return Some(&before[before.len() - start..]);
                }
                depth -= 1;
            }
            b';' if depth == 0 => return None,
            _ => {}
        }
    }
    None
}

/// The value after a key at `at` (`key: value`) up to the `,` or `}` that
/// ends it, when the identifier at `at` is an object key.
fn key_value(code: &str, at: usize, len: usize) -> Option<&str> {
    if !is_key_position(code, at, len) {
        return None;
    }
    let rest = &code[at + len..];
    let colon = rest.find(':')?;
    let value = &rest[colon + 1..];
    let bytes = value.as_bytes();
    let mut depth = 0i32;
    for (index, byte) in bytes.iter().enumerate() {
        match byte {
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' if depth == 0 => return Some(value[..index].trim()),
            b')' | b']' | b'}' => depth -= 1,
            b',' | b';' | b'\n' if depth == 0 => return Some(value[..index].trim()),
            _ => {}
        }
    }
    Some(value.trim())
}

/// Whether an expression negates a value: `-x`, `-Math.abs(x)`, `c ? a :
/// -b`, `x * -1` (not a subtraction `a - b`).
fn negates(expr: &str) -> bool {
    if expr.contains("* -1") || expr.contains("*-1") {
        return true;
    }
    let bytes = expr.as_bytes();
    bytes.iter().enumerate().any(|(index, byte)| {
        if *byte != b'-' {
            return false;
        }
        let next = bytes.get(index + 1).copied();
        let operand = next.is_some_and(|ch| ch.is_ascii_alphabetic() || ch == b'(' || ch == b'_');
        let unary = match prev_char(expr, index) {
            None => true,
            Some(ch) => b"?:=(,".contains(&ch),
        };
        operand && unary
    })
}

/// The expression assigned to `name` in a `const`/`let` declaration of
/// `code`, up to the end of its statement.
fn assigned<'a>(code: &'a str, name: &str) -> Vec<(usize, &'a str)> {
    word_positions(code, name)
        .into_iter()
        .filter_map(|at| {
            let before = code[..at].trim_end();
            if !(before.ends_with("const") || before.ends_with("let")) {
                return None;
            }
            let rest = code[at + name.len()..].trim_start();
            let rest = match rest.strip_prefix(':') {
                // `const x: number = ...`
                Some(typed) => &typed[typed.find('=')?..],
                None => rest,
            };
            let value = rest.strip_prefix('=')?;
            if value.starts_with('=') {
                return None;
            }
            let end = value.find([';', '\n']).unwrap_or(value.len());
            Some((at, &value[..end]))
        })
        .collect()
}

/// Money by its name: `amount`, `amountCents`, `cents`, `price`.
fn is_money_name(name: &str) -> bool {
    camel_words(name).iter().any(|word| {
        matches!(
            word.as_str(),
            "amount" | "cents" | "price" | "cost" | "balance" | "fee" | "salary" | "payment"
        )
    })
}

/// The identifier chain that ends right before `at` (`input.amount`).
fn chain_before(text: &str, at: usize) -> &str {
    let before = text[..at].trim_end();
    let len = before
        .bytes()
        .rev()
        .take_while(|byte| is_ident(*byte) || *byte == b'.')
        .count();
    &before[before.len() - len..]
}

/// The identifier chain that starts at `at` (`input.amount`).
fn chain_at(text: &str, at: usize) -> &str {
    let rest = &text[at..];
    let len = rest
        .bytes()
        .take_while(|byte| is_ident(*byte) || *byte == b'.')
        .count();
    &rest[..len]
}

/// The values that a throw guard `cond` rejects as negative: each `x < 0`,
/// `x <= 0` and, under a `!( ... )`, `x > 0` / `x >= 0`.
fn rejected_negatives(cond: &str) -> Vec<String> {
    let mut found = Vec::new();
    let bytes = cond.as_bytes();
    let is_zero = |rest: &str| {
        let rest = rest.trim_start();
        rest.starts_with('0') && !rest[1..].starts_with(|ch: char| ch.is_ascii_digit() || ch == '.')
    };
    for (index, byte) in bytes.iter().enumerate() {
        if *byte != b'<' || bytes.get(index + 1) == Some(&b'<') {
            continue;
        }
        let after = if bytes.get(index + 1) == Some(&b'=') {
            index + 2
        } else {
            index + 1
        };
        if is_zero(&cond[after..]) {
            let chain = chain_before(cond, index);
            if !chain.is_empty() {
                found.push(chain.to_string());
            }
        }
    }
    for (at, _) in cond.match_indices("!(") {
        let Some(close) = matching(cond, at + 1) else {
            continue;
        };
        let inner = &cond[at + 2..close];
        let inner_bytes = inner.as_bytes();
        for (index, byte) in inner_bytes.iter().enumerate() {
            if *byte != b'>' || (index > 0 && inner_bytes[index - 1] == b'=') {
                continue;
            }
            let after = if inner_bytes.get(index + 1) == Some(&b'=') {
                index + 2
            } else {
                index + 1
            };
            if is_zero(&inner[after..]) {
                let chain = chain_before(inner, index);
                if !chain.is_empty() {
                    found.push(chain.to_string());
                }
            }
        }
    }
    found
}

/// A lib check that throws on a negative money value.
struct SignGuard<'a> {
    file: &'a str,
    line: usize,
    /// The function with the check.
    guard: String,
    /// The field (or parameter) it checks.
    field: String,
    /// The exported functions that run the check.
    entries: Vec<String>,
    /// The fields of the records those functions take.
    keys: BTreeSet<String>,
}

/// Type names in `text` (`TransactionInput`, `Partial<Recurring>`), with
/// the helper types left out.
fn type_names(text: &str) -> Vec<String> {
    let mut names = Vec::new();
    let bytes = text.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index].is_ascii_uppercase() && (index == 0 || !is_ident(bytes[index - 1])) {
            let len = bytes[index..]
                .iter()
                .take_while(|byte| is_ident(**byte))
                .count();
            let name = &text[index..index + len];
            if !matches!(
                name,
                "Partial" | "Omit" | "Pick" | "Array" | "Record" | "Readonly" | "Required"
            ) {
                names.push(name.to_string());
            }
            index += len;
            continue;
        }
        index += 1;
    }
    names
}

/// The fields of the records that function `func` takes: inline object
/// parameter types and the declared types its parameters name (an `Input`,
/// `Patch` or `Draft` type also gives its base type's fields).
fn record_keys(app: &App, code: &str, func: &Func) -> BTreeSet<String> {
    let mut keys = BTreeSet::new();
    let params = func.params(code);
    for (at, _) in params.match_indices('{') {
        if enclosing_object(params, at).is_some() {
            continue;
        }
        if let Some((found, _)) = object_keys(params, at) {
            keys.extend(found);
        }
    }
    for name in type_names(params) {
        let mut candidates = vec![name.clone()];
        for suffix in [
            "Input", "Patch", "Draft", "Data", "Fields", "Values", "Form",
        ] {
            if let Some(base) = name.strip_suffix(suffix) {
                candidates.push(base.to_string());
            }
        }
        for candidate in candidates {
            if let Some((fields, _)) = type_fields(app, &candidate) {
                keys.extend(fields);
            }
        }
    }
    keys
}

fn sign_guards<'a>(app: &'a App) -> Vec<SignGuard<'a>> {
    let mut guards = Vec::new();
    for file in app.scripts() {
        if !is_lib_path(file.path) || is_seed_path(file.path) {
            continue;
        }
        let code = &file.code;
        let funcs = functions(code);
        for at in word_positions(code, "if") {
            let rest = code[at + 2..].trim_start();
            if !rest.starts_with('(') {
                continue;
            }
            let open = code.len() - rest.len();
            let Some(close) = matching(code, open) else {
                continue;
            };
            let after = code[close + 1..].trim_start();
            let start = code.len() - after.len();
            let then = if after.starts_with('{') {
                &code[start..matching(code, start).unwrap_or(code.len())]
            } else {
                &code[start..start + after.find(';').unwrap_or(after.len())]
            };
            if word_positions(then, "throw").is_empty() {
                continue;
            }
            let Some(func) = enclosing(&funcs, at) else {
                continue;
            };
            for value in rejected_negatives(&code[open + 1..close]) {
                let last = value.rsplit('.').next().unwrap_or(&value).to_string();
                if !is_money_name(&last) {
                    continue;
                }
                // (field, exported functions that run the check)
                let mut checks: Vec<(String, Vec<&Func>)> = Vec::new();
                let params = func.param_names(code);
                if value.contains('.') || func.exported {
                    let entries: Vec<&Func> = if func.exported {
                        vec![func]
                    } else {
                        funcs
                            .iter()
                            .filter(|other| {
                                other.exported
                                    && word_positions(other.body(code), &func.name).iter().any(
                                        |&call| {
                                            other.body(code)[call + func.name.len()..]
                                                .trim_start()
                                                .starts_with('(')
                                        },
                                    )
                            })
                            .collect()
                    };
                    if value.contains('.') || params.contains(&value) {
                        checks.push((last.clone(), entries));
                    }
                } else if params.contains(&value) {
                    // A helper (`assertAmount(amount)`): each call names the
                    // field it checks (`assertAmount(input.amount)`).
                    for call in word_positions(code, &func.name) {
                        let rest = code[call + func.name.len()..].trim_start();
                        if !rest.starts_with('(') || call == func.at {
                            continue;
                        }
                        let arg_at = code.len() - rest.len() + 1;
                        let arg = chain_at(
                            code,
                            arg_at + (code[arg_at..].len() - code[arg_at..].trim_start().len()),
                        );
                        let field = arg.rsplit('.').next().unwrap_or(arg).to_string();
                        if field.is_empty() || !is_money_name(&field) {
                            continue;
                        }
                        let Some(caller) = enclosing(&funcs, call) else {
                            continue;
                        };
                        let entries: Vec<&Func> = if caller.exported {
                            vec![caller]
                        } else {
                            Vec::new()
                        };
                        match checks.iter_mut().find(|(known, _)| *known == field) {
                            Some((_, known)) => known.extend(entries),
                            None => checks.push((field, entries)),
                        }
                    }
                }
                for (field, entries) in checks {
                    if entries.is_empty()
                        || guards.iter().any(|guard: &SignGuard| {
                            guard.file == file.path && guard.field == field
                        })
                    {
                        continue;
                    }
                    let mut keys = BTreeSet::new();
                    for entry in &entries {
                        keys.extend(record_keys(app, code, entry));
                    }
                    let mut names: Vec<String> =
                        entries.iter().map(|entry| entry.name.clone()).collect();
                    names.dedup();
                    guards.push(SignGuard {
                        file: file.path,
                        line: file.line_of(at),
                        guard: func.name.clone(),
                        field,
                        entries: names,
                        keys,
                    });
                }
            }
        }
    }
    guards
}

/// Where `code` writes `field` negated in an object literal whose keys are
/// (mostly) those of the guarded records: `field: -x`, `field: signed` with
/// `const signed = c ? x : -x`, or the shorthand `field` with `const field =
/// ... -x`. The offset of the first one.
fn negated_field(code: &str, field: &str, keys: &BTreeSet<String>) -> Option<usize> {
    let negated_local = |name: &str| assigned(code, name).iter().any(|(_, value)| negates(value));
    word_positions(code, field).into_iter().find(|&at| {
        let negated = match key_value(code, at, field.len()) {
            Some(value) => {
                negates(value)
                    || (value.bytes().all(is_ident) && !value.is_empty() && negated_local(value))
            }
            None => {
                // `{ ..., amount, ... }`
                let shorthand = matches!(prev_char(code, at), Some(b',' | b'{'))
                    && matches!(next_char(code, at + field.len()), Some(b',' | b'}'));
                shorthand && negated_local(field)
            }
        };
        negated
            && enclosing_object(code, at)
                .and_then(|open| object_keys(code, open))
                .is_some_and(|(found, _)| record_like(&found, keys))
    })
}

/// Whether the keys of an object (or the parameters of a helper) are the
/// guarded record's: at least three of them, and most of them (a
/// transaction is not a budget because both have `id` and `amount`).
fn record_like(found: &[String], keys: &BTreeSet<String>) -> bool {
    let shared = found.iter().filter(|key| keys.contains(*key)).count();
    shared >= 3.min(keys.len().max(1)) && shared * 10 >= found.len() * 7
}

/// Negative literals of `field` in a seed file: object literals with the
/// guarded records' keys (`amount: -1650`), and calls of a positional
/// helper whose parameters are those fields (`tx('t1', ..., -1650)`).
fn seed_negatives(code: &str, field: &str, keys: &BTreeSet<String>) -> (usize, Option<usize>) {
    let negative_literal = |value: &str| {
        let value = value.trim();
        value.starts_with('-')
            && value[1..]
                .trim_start()
                .starts_with(|ch: char| ch.is_ascii_digit())
    };
    let mut count = 0;
    let mut first = None;
    for at in word_positions(code, field) {
        let Some(value) = key_value(code, at, field.len()) else {
            continue;
        };
        if !negative_literal(value) {
            continue;
        }
        let fits = enclosing_object(code, at)
            .and_then(|open| object_keys(code, open))
            .is_some_and(|(found, _)| record_like(&found, keys));
        if fits {
            count += 1;
            first.get_or_insert(at);
        }
    }
    for helper in functions(code) {
        let names = helper.param_names(code);
        let Some(index) = names.iter().position(|name| name == field) else {
            continue;
        };
        if !record_like(&names, keys) {
            continue;
        }
        for call in word_positions(code, &helper.name) {
            let rest = code[call + helper.name.len()..].trim_start();
            if call == helper.at || !rest.starts_with('(') {
                continue;
            }
            let open = code.len() - rest.len();
            let Some(close) = matching(code, open) else {
                continue;
            };
            let args = split_top(&code[open + 1..close]);
            if args.get(index).is_some_and(|arg| negative_literal(arg)) {
                count += 1;
                first.get_or_insert(call);
            }
        }
    }
    (count, first)
}

/// The sign that a note states: `Some(true)` positive (`always > 0`, `never
/// negative`), `Some(false)` signed (`expenses negative`), `None` when it
/// states none or both.
fn note_sign(note: &str) -> Option<bool> {
    let mut note = note.to_lowercase();
    for phrase in [
        "never negative",
        "never negated",
        "not negative",
        "no negative",
        "non-negative",
        "nonnegative",
        "unsigned",
    ] {
        note = note.replace(phrase, " positive ");
    }
    let positive = note.contains("positive") || note.contains("> 0") || note.contains(">0");
    let signed = note.contains("negative") || note.contains("signed") || note.contains("< 0");
    // `expenses negative, income positive` states a signed amount.
    let by_direction = [
        "expenses negative",
        "expense negative",
        "expenses are negative",
        "negative for expense",
        "outflows negative",
        "debits negative",
    ]
    .iter()
    .any(|phrase| note.contains(phrase));
    match (positive, signed) {
        (true, false) => Some(true),
        (false, true) => Some(false),
        (true, true) if by_direction => Some(false),
        _ => None,
    }
}

/// The comment text of a line (`// x`, `/** x */`, ` * x`), if it is one.
fn comment_text(line: &str) -> Option<&str> {
    let line = line.trim();
    if line.starts_with("*/") {
        return Some("");
    }
    ["///", "//", "/**", "/*", "*"]
        .iter()
        .find_map(|mark| line.strip_prefix(mark))
        .map(|rest| rest.trim_end_matches("*/").trim())
}

/// The comment lines right above line `index` of `lines`, joined.
fn comment_above(lines: &[&str], index: usize) -> String {
    let mut notes = Vec::new();
    for line in lines[..index].iter().rev() {
        match comment_text(line) {
            Some(text) => notes.push(text),
            None => break,
        }
    }
    notes.reverse();
    notes.join(" ")
}

/// The sign that the types file states for `field`: `Some(true)` positive
/// (`always > 0`), `Some(false)` signed (`expenses negative`), with where
/// it says so (`lib/types.ts:12`). Read from the field's own comment (on
/// its line or the JSDoc above it), else from the clause that names the
/// field in the comment of its type. `None` when it states none or both.
fn stated_sign(app: &App, field: &str) -> Option<(bool, String)> {
    for file in app.scripts() {
        let name = file_name(file.path);
        if !(name.starts_with("types") || file.path.contains("/types/")) {
            continue;
        }
        let lines: Vec<&str> = file.raw.lines().collect();
        for (index, line) in lines.iter().enumerate() {
            let code = line.split("//").next().unwrap_or(line);
            if word_positions(code, field).is_empty() || comment_text(line).is_some() {
                continue;
            }
            let is_head = |row: &str| {
                row.contains("interface ") || (row.contains("type ") && row.contains('='))
            };
            // The field's own comment (a one-line type's comment is its type's).
            if !is_head(line) {
                let own = format!(
                    "{} {}",
                    comment_above(&lines, index),
                    line.split_once("//").map_or("", |(_, note)| note)
                );
                if let Some(sign) = note_sign(&own) {
                    return Some((sign, format!("{}:{}", file.path, index + 1)));
                }
            }
            // The comment of its type: the clause that names the field.
            let Some(head) = (0..=index).rev().find(|&row| is_head(lines[row])) else {
                continue;
            };
            let type_note = format!(
                "{} {}",
                comment_above(&lines, head),
                lines[head].split_once("//").map_or("", |(_, note)| note)
            );
            let clauses: Vec<&str> = type_note
                .split([';', '.'])
                .filter(|clause| !word_positions(clause, field).is_empty())
                .collect();
            if let Some(sign) = note_sign(&clauses.join(" ")) {
                return Some((sign, format!("{}:{}", file.path, head + 1)));
            }
        }
    }
    None
}

/// `positive_default`: `qsemantic` ran, so a money field that the types
/// file leaves open follows the default positive convention.
fn sign_mismatch(app: &App, positive_default: bool) -> Vec<Lint> {
    let mut lints = Vec::new();
    for guard in sign_guards(app) {
        // The files that call a guarded function, and the components they
        // render (a form that hands its record to the page's callback).
        let mut producers: Vec<&File> = Vec::new();
        for file in app.scripts() {
            if file.path == guard.file || is_seed_path(file.path) {
                continue;
            }
            let calls = imports(&file.code).iter().any(|(names, spec)| {
                names.iter().any(|name| guard.entries.contains(name))
                    && resolve(app, file.path, spec).is_some_and(|module| module.path == guard.file)
            });
            if !calls {
                continue;
            }
            if !producers.iter().any(|known| known.path == file.path) {
                producers.push(file);
            }
            for (_, spec) in imports(&file.code) {
                if let Some(module) = resolve(app, file.path, &spec) {
                    if module.path.ends_with(".tsx")
                        && !module.is_test()
                        && !producers.iter().any(|known| known.path == module.path)
                    {
                        producers.push(module);
                    }
                }
            }
        }
        let negated: Vec<(&File, usize)> = producers
            .iter()
            .filter_map(|file| {
                let code = blank_strings(&file.code);
                negated_field(&code, &guard.field, &guard.keys).map(|at| (*file, at))
            })
            .collect();
        let mut seeds: Vec<(&File, usize, usize)> = Vec::new();
        for file in app.scripts().filter(|file| is_seed_path(file.path)) {
            let (count, first) = seed_negatives(&file.code, &guard.field, &guard.keys);
            if let Some(first) = first {
                seeds.push((file, count, first));
            }
        }
        if negated.is_empty() && seeds.is_empty() {
            continue;
        }
        let field = &guard.field;
        let entries = guard
            .entries
            .iter()
            .map(|name| format!("`{name}`"))
            .collect::<Vec<_>>()
            .join(", ");
        let mut evidence: Vec<String> = negated
            .iter()
            .map(|(file, at)| {
                format!(
                    "{}:{} sends `{field}` negated (an expense) to {entries}",
                    file.path,
                    file.line_of(*at)
                )
            })
            .collect();
        evidence.extend(seeds.iter().map(|(file, count, at)| {
            format!(
                "{}:{} holds {count} negative `{field}` value{}",
                file.path,
                file.line_of(*at),
                if *count == 1 { "" } else { "s" }
            )
        }));
        let evidence = evidence.join(", and ");
        // With `qsemantic`, a field the plan left open got the default
        // convention in every prompt: always > 0, a kind gives the direction.
        let stated = stated_sign(app, field).or_else(|| {
            positive_default.then(|| {
                (
                    true,
                    "the default of the conventions block in every prompt".to_string(),
                )
            })
        });
        match stated {
            Some((true, source)) => {
                // The types file says positive: the producers change.
                for (file, at) in &negated {
                    lints.push(Lint {
                        level: Level::Error,
                        code: "TS9022",
                        name: "sign-mismatch",
                        path: file.path.to_string(),
                        line: file.line_of(*at),
                        message: format!(
                            "this file sends `{field}` negated to {entries} ({}), but the app's \
                             convention is a positive amount ({source}) and `{}` ({}:{}) \
                             throws on a negative one, so saving an expense fails. Send the positive \
                             amount (`Math.abs`) and the expense/income kind instead of a negated \
                             amount.",
                            guard.file, guard.guard, guard.file, guard.line
                        ),
                    });
                }
                for (file, count, at) in &seeds {
                    lints.push(Lint {
                        level: Level::Error,
                        code: "TS9022",
                        name: "sign-mismatch",
                        path: file.path.to_string(),
                        line: file.line_of(*at),
                        message: format!(
                            "this seed holds {count} negative `{field}` values, but the app's \
                             convention is a positive amount ({source}) and `{}` ({}:{}) \
                             throws on a negative one, so editing a seeded expense fails. Store \
                             each amount positive, with its expense/income kind.",
                            guard.guard, guard.file, guard.line
                        ),
                    });
                }
            }
            stated => {
                let note = match stated {
                    Some((_, source)) => format!(" (the convention of {source})"),
                    None => String::new(),
                };
                lints.push(Lint {
                    level: Level::Error,
                    code: "TS9022",
                    name: "sign-mismatch",
                    path: guard.file.to_string(),
                    line: guard.line,
                    message: format!(
                        "`{}` throws on a negative `{field}`, but the rest of the app writes expenses \
                         as negative amounts{note}: {evidence}. Adding or editing an expense throws. \
                         Follow the sign the callers and the seed use (expenses negative, income \
                         positive): reject only a zero or non-integer `{field}` here \
                         (`!Number.isInteger(x) || x === 0`), and compare any parts (splits) with \
                         the signed total.",
                        guard.guard
                    ),
                });
            }
        }
    }
    lints
}

// ---- date-format-mismatch --------------------------------------------------

/// The offsets of regex literals in `code` that accept only a calendar
/// date: anchored (`/^...$/`) and ending with `\d{4}-\d{2}-\d{2}` (groups
/// aside) right before the closing `$`.
fn date_only_patterns(code: &str) -> Vec<usize> {
    let mut found = Vec::new();
    for (at, _) in code.match_indices("/^") {
        let rest = &code[at + 2..];
        let line_end = rest.find('\n').unwrap_or(rest.len());
        let Some(end) = rest[..line_end].find("$/") else {
            continue;
        };
        let inner: String = rest[..end]
            .chars()
            .filter(|ch| !matches!(ch, '(' | ')'))
            .collect();
        if inner.ends_with("\\d{4}-\\d{2}-\\d{2}") || inner.ends_with("[0-9]{4}-[0-9]{2}-[0-9]{2}")
        {
            found.push(at);
        }
    }
    found
}

/// Whether `body` rejects a value that its date pattern does not match: an
/// `if` whose condition negates the match (`!m` with `const m =
/// RE.exec(s)`, `!RE.test(s)`, `m === null`) and whose branch throws or
/// returns nothing. A helper that falls back to `new Date(s)` instead does
/// not reject.
fn rejects_mismatch(body: &str, needles: &[String]) -> bool {
    let holds_pattern = |text: &str| {
        text.contains("/^")
            || needles
                .iter()
                .any(|needle| !needle.is_empty() && !word_positions(text, needle).is_empty())
    };
    let mut names: Vec<String> = needles
        .iter()
        .filter(|name| !name.is_empty())
        .cloned()
        .collect();
    for at in word_positions(body, "const")
        .into_iter()
        .chain(word_positions(body, "let"))
    {
        let rest = body[at..]
            .trim_start_matches("const")
            .trim_start_matches("let")
            .trim_start();
        let name: String = rest
            .bytes()
            .take_while(|byte| is_ident(*byte))
            .map(char::from)
            .collect();
        if !name.is_empty()
            && assigned(body, &name)
                .iter()
                .any(|(_, value)| holds_pattern(value))
        {
            names.push(name);
        }
    }
    word_positions(body, "if").into_iter().any(|at| {
        let rest = body[at + 2..].trim_start();
        if !rest.starts_with('(') {
            return false;
        }
        let open = body.len() - rest.len();
        let Some(close) = matching(body, open) else {
            return false;
        };
        let cond = &body[open + 1..close];
        let after = body[close + 1..].trim_start();
        let start = body.len() - after.len();
        let then = if after.starts_with('{') {
            &body[start..matching(body, start).unwrap_or(body.len())]
        } else {
            &body[start..start + after.find(';').unwrap_or(after.len())]
        };
        let gives_up = !word_positions(then, "throw").is_empty()
            || then.contains("return null")
            || then.contains("return undefined")
            || then.contains("NaN");
        let negated = names.iter().any(|name| {
            cond.contains(&format!("!{name}"))
                || cond.contains(&format!("{name} === null"))
                || cond.contains(&format!("{name} == null"))
        }) || (cond.contains('!') && cond.contains("/^"));
        gives_up && negated
    })
}

/// Whether a date helper's body handles a full timestamp before its
/// pattern: it cuts a value at its `T` (`.slice(0, 10)`, `.split('T')`,
/// `.substring(0, 10)`) or tests for the `T` and converts it (`includes('T')`
/// or `indexOf('T')` with `new Date(`). This is the fix the lint asks for, so
/// such a helper is not strict.
fn cuts_timestamp(body: &str) -> bool {
    let compact: String = body.chars().filter(|ch| !ch.is_whitespace()).collect();
    let cut = [
        ".slice(0,10)",
        ".substring(0,10)",
        ".substr(0,10)",
        ".split('T')",
        ".split(\"T\")",
    ]
    .iter()
    .any(|needle| compact.contains(needle));
    let tests_t = [
        "includes('T')",
        "includes(\"T\")",
        "indexOf('T')",
        "indexOf(\"T\")",
    ]
    .iter()
    .any(|needle| compact.contains(needle));
    cut || (tests_t && compact.contains("newDate("))
}

/// The fields stored as full ISO timestamps: `field: new
/// Date().toISOString()`, `field: now` with `const now = new
/// Date().toISOString()`, or `field: daysAgo(3)` with a helper that returns
/// `.toISOString()` (none of them cut to the date). Field -> (file, offset).
fn timestamp_fields<'a>(app: &'a App) -> BTreeMap<String, (&'a str, usize)> {
    let full = |expr: &str| {
        expr.contains(".toISOString()")
            && !expr.contains(".toISOString().slice")
            && !expr.contains(".toISOString().substring")
            && !expr.contains(".toISOString().split")
            && !expr.contains(".toISOString().substr")
    };
    let mut fields = BTreeMap::new();
    for file in app.scripts() {
        let code = &file.code;
        let funcs = functions(code);
        // Helpers that return a full timestamp.
        let stamping: Vec<&str> = funcs
            .iter()
            .filter(|func| {
                let body = func.body(code);
                full(body) && !body.contains(".slice(0, 10)") && !body.contains(".slice(0,10)")
            })
            .map(|func| func.name.as_str())
            .collect();
        for (at, _) in code.match_indices("At") {
            let start = code[..at]
                .bytes()
                .rev()
                .take_while(|byte| is_ident(*byte))
                .count();
            let name_at = at - start;
            if start == 0
                || code
                    .as_bytes()
                    .get(at + 2)
                    .is_some_and(|byte| is_ident(*byte))
            {
                continue;
            }
            let name = &code[name_at..at + 2];
            let Some(value) = key_value(code, name_at, name.len()) else {
                continue;
            };
            let callee = value.bytes().take_while(|byte| is_ident(*byte)).count();
            let stamped = full(value)
                || (value.bytes().all(is_ident)
                    && assigned(code, value).iter().any(|(_, rhs)| full(rhs)))
                || (callee > 0
                    && value[callee..].trim_start().starts_with('(')
                    && stamping.contains(&&value[..callee]));
            if stamped {
                fields
                    .entry(name.to_string())
                    .or_insert((file.path, name_at));
            }
        }
    }
    fields
}

/// A strict date helper, or a wrapper of one: `root` is the index of the
/// strict helper it reaches.
struct Strict<'a> {
    name: String,
    path: &'a str,
    at: usize,
    exported: bool,
    root: usize,
}

/// Whether a call of `entry.name` in file `from` (its functions `own`, its
/// imports `links`) reaches `entry`: the file's own function of that name,
/// else the export of `entry`'s file that it imports. Two files that each
/// keep a private `parseIso` do not reach each other's.
fn reaches(
    app: &App,
    from: &str,
    own: &[Func],
    links: &[(Vec<String>, String)],
    entry: &Strict,
) -> bool {
    if own.iter().any(|func| func.name == entry.name) {
        return from == entry.path;
    }
    entry.exported
        && links.iter().any(|(names, spec)| {
            names.contains(&entry.name)
                && resolve(app, from, spec).is_some_and(|module| module.path == entry.path)
        })
}

fn date_format_mismatch(app: &App) -> Vec<Lint> {
    let stored = timestamp_fields(app);
    if stored.is_empty() {
        return Vec::new();
    }
    let mut lints = Vec::new();
    for file in app.scripts() {
        if !is_lib_path(file.path) {
            continue;
        }
        let patterns = date_only_patterns(&file.code);
        if patterns.is_empty() {
            continue;
        }
        let funcs = functions(&file.code);
        // The regex itself, or the name of the const that holds it.
        let mut needles: Vec<String> = Vec::new();
        for &at in &patterns {
            let line_start = file.code[..at].rfind('\n').map_or(0, |start| start + 1);
            let line = &file.code[line_start..at];
            match line.split_once('=').map(|(left, _)| left.trim()) {
                Some(left) if left.starts_with("const ") || left.contains(" const ") => {
                    let name = left
                        .rsplit(' ')
                        .next()
                        .unwrap_or(left)
                        .trim_end_matches(|ch: char| !ch.is_ascii_alphanumeric() && ch != '_');
                    needles.push(name.to_string());
                }
                _ => needles.push(String::new()),
            }
        }
        // The strict helpers: their bodies use the pattern and reject.
        let mut strict: Vec<Strict> = funcs
            .iter()
            .filter(|func| {
                let body = func.body(&file.code);
                rejects_mismatch(body, &needles)
                    && !cuts_timestamp(body)
                    && (patterns
                        .iter()
                        .any(|&at| func.body.0 <= at && at <= func.body.1)
                        || needles
                            .iter()
                            .any(|name| !name.is_empty() && !word_positions(body, name).is_empty()))
            })
            .map(|func| Strict {
                name: func.name.clone(),
                path: file.path,
                at: func.at,
                exported: func.exported,
                root: 0,
            })
            .collect();
        if strict.is_empty() {
            continue;
        }
        for (index, entry) in strict.iter_mut().enumerate() {
            entry.root = index;
        }
        let roots: Vec<(String, usize)> = strict
            .iter()
            .map(|entry| (entry.name.clone(), entry.at))
            .collect();
        // Wrappers anywhere that pass their first parameter on to a strict
        // helper that their call reaches (`dayStart(iso) { parseDate(iso) }`).
        for _ in 0..3 {
            let mut added = false;
            for other in app.scripts().filter(|other| !other.is_test()) {
                let own = functions(&other.code);
                let links = imports(&other.code);
                for func in &own {
                    let Some(param) = func.param_names(&other.code).into_iter().next() else {
                        continue;
                    };
                    if param.is_empty()
                        || strict
                            .iter()
                            .any(|entry| entry.name == func.name && entry.path == other.path)
                    {
                        continue;
                    }
                    let body = func.body(&other.code);
                    let wrapped = strict.iter().find(|entry| {
                        reaches(app, other.path, &own, &links, entry)
                            && word_positions(body, &entry.name).iter().any(|&call| {
                                let rest = body[call + entry.name.len()..].trim_start();
                                rest.strip_prefix('(')
                                    .is_some_and(|args| chain_at(args.trim_start(), 0) == param)
                            })
                    });
                    if let Some(root) = wrapped.map(|entry| entry.root) {
                        strict.push(Strict {
                            name: func.name.clone(),
                            path: other.path,
                            at: func.at,
                            exported: func.exported,
                            root,
                        });
                        added = true;
                    }
                }
            }
            if !added {
                break;
            }
        }
        // A call of a strict helper on a stored timestamp, where the call
        // reaches that helper (the same file, or an import of its export).
        let mut hit: Option<(&str, usize, String, usize, String)> = None;
        'files: for other in app.scripts().filter(|other| !other.is_test()) {
            let code = &other.code;
            let own = functions(code);
            let links = imports(code);
            for entry in &strict {
                if !reaches(app, other.path, &own, &links, entry) {
                    continue;
                }
                let name = &entry.name;
                for call in word_positions(code, name) {
                    let rest = code[call + name.len()..].trim_start();
                    if !rest.starts_with('(') {
                        continue;
                    }
                    let open = code.len() - rest.len();
                    let Some(close) = matching(code, open) else {
                        continue;
                    };
                    let arg = split_top(&code[open + 1..close])
                        .first()
                        .copied()
                        .unwrap_or("");
                    let mut texts = vec![arg.to_string()];
                    if arg.bytes().all(is_ident) && !arg.is_empty() {
                        texts.extend(
                            assigned(code, arg)
                                .iter()
                                .map(|(_, value)| value.to_string()),
                        );
                    }
                    // A value cut to its date first (`completedAt.slice(0, 10)`)
                    // matches the pattern.
                    let cut = |text: &str| {
                        let text = text.replace(' ', "");
                        text.contains(".slice(0,10)")
                            || text.contains(".substring(0,10)")
                            || text.contains(".substr(0,10)")
                            || text.contains(".split('T')")
                            || text.contains(".split(\"T\")")
                    };
                    let field = stored.keys().find(|field| {
                        texts
                            .iter()
                            .any(|text| !word_positions(text, field).is_empty() && !cut(text))
                    });
                    if let Some(field) = field {
                        hit = Some((
                            other.path,
                            other.line_of(call),
                            field.clone(),
                            entry.root,
                            entry.name.clone(),
                        ));
                        break 'files;
                    }
                }
            }
        }
        let Some((path, line, field, root, called)) = hit else {
            continue;
        };
        let (helper, helper_at) = roots[root].clone();
        let via = if called == helper {
            String::new()
        } else {
            format!(" (through `{called}`, which calls it)")
        };
        let (store_path, store_at) = stored[&field];
        let store_line = app
            .get(store_path)
            .map_or(1, |store| store.line_of(store_at));
        lints.push(Lint {
            level: Level::Error,
            code: "TS9023",
            name: "date-format-mismatch",
            path: file.path.to_string(),
            line: file.line_of(helper_at),
            message: format!(
                "`{helper}` accepts only a 'YYYY-MM-DD' date (its pattern ends right after the day), \
                 but {path}:{line} passes it `{field}`{via}, which {store_path}:{store_line} stores as a full \
                 ISO timestamp (`toISOString()`): it throws or gives nothing for every record, so each \
                 count or chart built on it is 0. Make `{helper}` read a timestamp too: cut a value \
                 with a `T` to its date part (`value.slice(0, 10)`, or `new Date(value)` and its local \
                 date) before the pattern."
            ),
        });
    }
    lints
}

// ---- transition-timestamps -------------------------------------------------

/// The completion timestamp fields that a status change sets or clears.
const DONE_STAMPS: &[&str] = &[
    "completedAt",
    "doneAt",
    "finishedAt",
    "closedAt",
    "resolvedAt",
    "completedDate",
    "completedOn",
];

fn is_done_word(word: &str) -> bool {
    matches!(
        word.to_lowercase().as_str(),
        "done" | "completed" | "complete" | "closed" | "finished" | "resolved"
    )
}

/// The value that clears `stamp` of type `record` in `code`, by its
/// declaration: `null` for `string | null`, `undefined` for an optional
/// `stamp?: string`, else `null` once its type allows it.
fn stamp_clear(code: &str, record: &str, stamp: &str) -> &'static str {
    let start = word_positions(code, record).into_iter().find(|&at| {
        let before = code[..at].trim_end();
        before.ends_with("interface") || before.ends_with("type")
    });
    let Some(start) = start else {
        return "null";
    };
    let Some(decl) = word_positions(&code[start..], stamp).into_iter().next() else {
        return "null";
    };
    let rest = &code[start + decl + stamp.len()..];
    let end = rest.find([';', ',', '\n', '}']).unwrap_or(rest.len());
    let ty = &rest[..end];
    if ty.contains("null") {
        "null"
    } else if ty.trim_start().starts_with('?') || ty.contains("undefined") {
        "undefined (the key is optional, not nullable)"
    } else {
        "null (make its type `string | null` first)"
    }
}

fn transition_timestamps(app: &App) -> Vec<Lint> {
    let unions = field_unions(app);
    let done = |field: &str| {
        unions.get(field).is_some_and(|declared| {
            declared
                .iter()
                .any(|(literals, _)| literals.iter().any(|literal| is_done_word(literal)))
        })
    };
    // The record types with a status and a completion stamp.
    let mut records: Vec<(String, String, String, bool, &str)> = Vec::new();
    let mut seen = BTreeSet::new();
    for file in app.scripts() {
        for keyword in ["interface", "type"] {
            for at in word_positions(&file.code, keyword) {
                let rest = file.code[at + keyword.len()..].trim_start();
                let len = rest.bytes().take_while(|byte| is_ident(*byte)).count();
                let name = &rest[..len];
                if len == 0 || !seen.insert(name.to_string()) {
                    continue;
                }
                let Some((fields, declared_in)) = type_fields(app, name) else {
                    continue;
                };
                let Some(status) = ["status", "stage"]
                    .iter()
                    .find(|field| fields.iter().any(|key| key == *field))
                else {
                    continue;
                };
                let Some(stamp) = DONE_STAMPS
                    .iter()
                    .find(|stamp| fields.iter().any(|key| key == *stamp))
                else {
                    continue;
                };
                if done(status) {
                    let started = fields.iter().any(|key| key == "startedAt");
                    // How the stamp is cleared, by its declared type.
                    let clear = app
                        .get(&declared_in)
                        .map_or("null", |decl| stamp_clear(&decl.code, name, stamp));
                    records.push((
                        name.to_string(),
                        status.to_string(),
                        stamp.to_string(),
                        started,
                        clear,
                    ));
                }
            }
        }
    }
    let mut lints = Vec::new();
    let all_funcs: Vec<(&File, Vec<Func>)> = app
        .scripts()
        .map(|file| (file, functions(&file.code)))
        .collect();
    let touches = |name: &str, stamp: &str| {
        all_funcs.iter().any(|(file, funcs)| {
            funcs.iter().any(|func| {
                func.name == name && !word_positions(func.body(&file.code), stamp).is_empty()
            })
        })
    };
    for (file, funcs) in &all_funcs {
        if !is_lib_path(file.path) || is_seed_path(file.path) {
            continue;
        }
        let code = &file.code;
        for func in funcs {
            let lower = func.name.to_lowercase();
            if ["create", "add", "new", "make", "seed"]
                .iter()
                .any(|verb| lower.starts_with(verb))
            {
                continue;
            }
            let params = func.params(code);
            let body = func.body(code);
            for (record, status, stamp, started, clear) in &records {
                if word_positions(params, record).is_empty()
                    || !word_positions(body, stamp).is_empty()
                {
                    continue;
                }
                // A spread record with a new status: `{ ...t, status }`, or an
                // assignment `t.status = s`.
                let sets = word_positions(body, status).into_iter().find(|&at| {
                    let absolute = func.body.0 + at;
                    let in_spread = enclosing_object(code, absolute)
                        .and_then(|open| object_keys(code, open))
                        .is_some_and(|(keys, spread)| {
                            spread && keys.iter().any(|key| key == status)
                        });
                    let shorthand_or_key = is_key_position(body, at, status.len())
                        || (matches!(prev_char(body, at), Some(b',' | b'{'))
                            && matches!(next_char(body, at + status.len()), Some(b',' | b'}')));
                    let assigned = body[..at].ends_with('.')
                        && body[at + status.len()..].trim_start().starts_with('=')
                        && !body[at + status.len()..].trim_start().starts_with("==");
                    if assigned {
                        return true;
                    }
                    if !(in_spread && shorthand_or_key) {
                        return false;
                    }
                    // A fixed status that is not a done one (`status: 'archived'`).
                    match key_value(body, at, status.len()) {
                        Some(value) if value.starts_with(['\'', '"']) => {
                            is_done_word(value.trim_matches(['\'', '"']))
                        }
                        _ => true,
                    }
                });
                let Some(at) = sets else {
                    continue;
                };
                // A helper it calls may set the stamp.
                let delegates = identifiers_of(body).iter().any(|name| {
                    name != &func.name && body.contains(&format!("{name}(")) && touches(name, stamp)
                });
                if delegates {
                    continue;
                }
                // Only a status change that a page or component makes (a lib
                // helper that no screen calls shows nothing wrong).
                let called = app.scripts().any(|other| {
                    is_ui_path(other.path)
                        && imports(&other.code).iter().any(|(names, spec)| {
                            names.contains(&func.name)
                                && resolve(app, other.path, spec)
                                    .is_some_and(|module| module.path == file.path)
                        })
                });
                if !called {
                    continue;
                }
                let start = if *started {
                    ", and `startedAt` when it first leaves the initial status"
                } else {
                    ""
                };
                lints.push(Lint {
                    level: Level::Error,
                    code: "TS9024",
                    name: "transition-timestamps",
                    path: file.path.to_string(),
                    line: file.line_of(func.body.0 + at),
                    message: format!(
                        "`{}` sets a {record}'s `{status}` but never `{stamp}` ({record} has it, and a \
                         done status exists): a {record} moved to done here has no completion time, so \
                         every count and chart that reads `{stamp}` misses it, and one moved out of \
                         done keeps a stale one. In `{}`, set `{stamp}` to the current timestamp \
                         (`new Date().toISOString()`) when the status becomes done (keep it when it \
                         stays done) and to {clear} when it leaves done{start}.",
                        func.name, func.name
                    ),
                });
                break;
            }
        }
    }
    lints
}

/// The identifiers of `code`, each once.
fn identifiers_of(code: &str) -> BTreeSet<String> {
    let mut found = BTreeSet::new();
    let bytes = code.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if (bytes[index].is_ascii_alphabetic() || bytes[index] == b'_')
            && (index == 0 || !is_ident(bytes[index - 1]))
        {
            let len = bytes[index..]
                .iter()
                .take_while(|byte| is_ident(**byte))
                .count();
            found.insert(code[index..index + len].to_string());
            index += len;
            continue;
        }
        index += 1;
    }
    found
}

// ---- storage-outside-store -------------------------------------------------

/// The text of a plain string literal (`'a'`, `"a"`, a template with no `${`).
fn unquote(text: &str) -> Option<String> {
    let text = text.trim();
    let first = text.chars().next()?;
    (matches!(first, '\'' | '"' | '`')
        && text.len() >= 2
        && text.ends_with(first)
        && !text.contains("${"))
    .then(|| text[1..text.len() - 1].to_string())
}

/// The string constants of the app (`const STORAGE_KEY = 'taskflow'`), by
/// name; the first one of a name wins.
fn string_consts(app: &App) -> BTreeMap<String, String> {
    let mut consts = BTreeMap::new();
    for file in app.scripts() {
        let code = &file.code;
        for (at, _) in code.match_indices("const ") {
            if at > 0 && is_ident(code.as_bytes()[at - 1]) {
                continue;
            }
            let rest = &code[at + "const ".len()..];
            let len = rest.bytes().take_while(|byte| is_ident(*byte)).count();
            if len == 0 {
                continue;
            }
            let line = rest[len..].split('\n').next().unwrap_or("");
            let Some((_, value)) = line.split_once('=') else {
                continue;
            };
            let value = value.trim().trim_end_matches(';').trim();
            let value = value.strip_suffix("as const").unwrap_or(value);
            if let Some(text) = unquote(value) {
                consts.entry(rest[..len].to_string()).or_insert(text);
            }
        }
    }
    consts
}

/// The keys that `code` passes to `localStorage.getItem`, `setItem` and
/// `removeItem`: a literal's text, a constant's value, else the argument as
/// it is.
fn storage_keys(code: &str, consts: &BTreeMap<String, String>) -> BTreeSet<String> {
    let mut keys = BTreeSet::new();
    for method in ["getItem(", "setItem(", "removeItem("] {
        for (at, _) in code.match_indices(method) {
            if !code[..at].ends_with("localStorage.") {
                continue;
            }
            let rest = &code[at + method.len()..];
            let end = rest.find([',', ')']).unwrap_or(rest.len());
            let arg = rest[..end].trim();
            let key = unquote(arg)
                .or_else(|| consts.get(arg).cloned())
                .unwrap_or_else(|| arg.to_string());
            keys.insert(key);
        }
    }
    keys
}

fn storage_outside_store<'a>(app: &'a App) -> Vec<Lint> {
    let uses_storage = |file: &File| {
        let code = blank_strings(&file.code);
        code.match_indices("localStorage")
            .map(|(at, _)| at)
            .find(|&at| !code[..at].trim_end().ends_with("typeof"))
    };
    // The store providers: a context with a provider component.
    let providers: Vec<&File> = app
        .scripts()
        .filter(|file| {
            file.code.contains("createContext")
                && functions(&file.code)
                    .iter()
                    .any(|func| func.exported && func.name.ends_with("Provider"))
        })
        .collect();
    // Every context provider file (a theme provider too): each one owns its
    // context and is never told to use another one's hook.
    let context_files: Vec<&str> = providers.iter().map(|file| file.path).collect();
    // The storage modules: lib files that touch `localStorage`.
    let storage: Vec<&File> = app
        .scripts()
        .filter(|file| {
            is_lib_path(file.path)
                && uses_storage(file).is_some()
                && !providers.iter().any(|provider| provider.path == file.path)
        })
        .collect();
    // A provider's own modules (the lib store it builds on).
    let closure = |provider: &'a str| -> Vec<&'a str> {
        let mut chain: Vec<&str> = vec![provider];
        let mut index = 0;
        while index < chain.len() {
            if let Some(file) = app.get(chain[index]) {
                for (_, spec) in imports(&file.code) {
                    if let Some(module) = resolve(app, file.path, &spec) {
                        if is_lib_path(module.path) && !chain.contains(&module.path) {
                            chain.push(module.path);
                        }
                    }
                }
            }
            index += 1;
        }
        chain
    };
    // A store provider loads the app state from a storage module (a theme
    // provider that keeps only its theme in `localStorage` is not one).
    let mut chain: Vec<&str> = Vec::new();
    let providers: Vec<&File> = providers
        .into_iter()
        .filter(|provider| {
            let own = closure(provider.path);
            let stores = own
                .iter()
                .any(|path| storage.iter().any(|file| file.path == *path));
            if stores {
                chain.extend(own);
            }
            stores
        })
        .collect();
    let storage_fns: Vec<(&str, Vec<String>)> = storage
        .iter()
        .map(|file| {
            (
                file.path,
                functions(&file.code)
                    .into_iter()
                    .filter(|func| func.exported)
                    .map(|func| func.name)
                    .collect(),
            )
        })
        .collect();
    // How a file reaches storage: the first call of a storage function it
    // imports, else a direct `localStorage`.
    let reach = |file: &File| -> Option<(usize, String)> {
        let code = blank_strings(&file.code);
        for (names, spec) in imports(&file.code) {
            let Some(module) = resolve(app, file.path, &spec) else {
                continue;
            };
            let Some((_, fns)) = storage_fns.iter().find(|(path, _)| *path == module.path) else {
                continue;
            };
            for name in names.iter().filter(|name| fns.contains(name)) {
                let call = word_positions(&code, name)
                    .into_iter()
                    .find(|at| code[at + name.len()..].trim_start().starts_with('('));
                if let Some(at) = call {
                    return Some((at, format!("calls `{name}` from `{}`", module.path)));
                }
            }
        }
        uses_storage(file).map(|at| (at, "uses `localStorage`".to_string()))
    };
    let layout = root_layout(app);
    let of_layout = |file: &File| {
        layout.is_some_and(|layout| {
            layout.path == file.path
                || imports(&layout.code).iter().any(|(_, spec)| {
                    resolve(app, layout.path, spec).is_some_and(|module| module.path == file.path)
                })
        })
    };
    // The keys of the storage modules: a file that keeps something else (a
    // theme) under a key of its own overwrites no state.
    let consts = string_consts(app);
    let state_keys: BTreeSet<String> = storage
        .iter()
        .flat_map(|file| storage_keys(&file.code, &consts))
        .collect();
    let own_key = |file: &File| {
        let keys = storage_keys(&file.code, &consts);
        !state_keys.is_empty() && !keys.is_empty() && keys.is_disjoint(&state_keys)
    };
    let hook_of = |provider: &File| {
        functions(&provider.code).into_iter().find(|func| {
            func.exported
                && func
                    .name
                    .strip_prefix("use")
                    .is_some_and(|rest| rest.starts_with(|ch: char| ch.is_ascii_uppercase()))
        })
    };
    // A provider is mounted when the root layout, or a component in the
    // layout's import tree (a `Providers` wrapper), renders it.
    let mounted = |provider: &File| {
        let Some(layout) = layout else {
            return false;
        };
        let names: Vec<String> = functions(&provider.code)
            .into_iter()
            .filter(|func| func.exported && func.name.ends_with("Provider"))
            .map(|func| func.name)
            .collect();
        let mut tree: Vec<&str> = vec![layout.path];
        let mut index = 0;
        while index < tree.len() {
            if let Some(file) = app.get(tree[index]) {
                if names
                    .iter()
                    .any(|name| renders_tag(&file.code, name).is_some())
                {
                    return true;
                }
                for (_, spec) in imports(&file.code) {
                    if let Some(module) = resolve(app, file.path, &spec) {
                        if is_ui_path(module.path) && !tree.contains(&module.path) {
                            tree.push(module.path);
                        }
                    }
                }
            }
            index += 1;
        }
        false
    };
    let mut lints = Vec::new();
    if let Some(provider) = providers.iter().find(|provider| mounted(provider)) {
        let hook = hook_of(provider)
            .map(|func| format!("`{}()`", func.name))
            .unwrap_or_else(|| "its hook".to_string());
        for file in app.scripts() {
            if !is_ui_path(file.path)
                || chain.contains(&file.path)
                || context_files.contains(&file.path)
            {
                continue;
            }
            let Some((at, what)) = reach(file) else {
                continue;
            };
            if !what.starts_with("calls") && own_key(file) {
                continue;
            }
            let kind = if of_layout(file) {
                "layout component (it stays mounted while the pages change)"
            } else if route_segments(file.path).is_some() {
                "page"
            } else {
                "component"
            };
            lints.push(Lint {
                level: Level::Error,
                code: "TS9020",
                name: "storage-outside-store",
                path: file.path.to_string(),
                line: file.line_of(at),
                message: format!(
                    "this {kind} {what} itself, beside the store provider `{}`: its own copy of the state \
                     goes stale and, when saved, overwrites what the other pages changed (or it reads \
                     storage before the provider has loaded it). Take the state, the theme included, \
                     from the store hook {hook} and save every change through it; remove the storage \
                     calls from this file.",
                    provider.path
                ),
            });
        }
        return lints;
    }
    // A store provider that the layout does not mount: a file that calls its
    // hook outside it throws ("must be used within"), so the layout wraps the
    // pages in it. A provider that no file renders at all is the gate's own
    // TS9004 (`gate::unmounted_providers`), not repeated here; one that only
    // a page renders is this lint's. The rest is read as an app with no
    // provider.
    if let (Some(provider), Some(layout)) = (providers.first(), layout) {
        let name = functions(&provider.code)
            .into_iter()
            .find(|func| func.exported && func.name.ends_with("Provider"))
            .map(|func| func.name)
            .unwrap_or_default();
        let rendered: Vec<&str> = app
            .scripts()
            .filter(|file| file.path != provider.path && renders_tag(&file.code, &name).is_some())
            .map(|file| file.path)
            .collect();
        if let Some(hook) = hook_of(provider).filter(|_| !rendered.is_empty()) {
            let callers: Vec<&str> = app
                .scripts()
                .filter(|file| is_ui_path(file.path) && !chain.contains(&file.path))
                .filter(|file| {
                    word_positions(&file.code, &hook.name).iter().any(|at| {
                        file.code[at + hook.name.len()..]
                            .trim_start()
                            .starts_with('(')
                    })
                })
                .map(|file| file.path)
                .collect();
            if !callers.is_empty() {
                let at = layout.code.find("{children}").unwrap_or(0);
                let shown = callers
                    .iter()
                    .take(3)
                    .map(|path| format!("`{path}`"))
                    .collect::<Vec<_>>()
                    .join(", ");
                let more = if callers.len() > 3 { ", ..." } else { "" };
                lints.push(Lint {
                    level: Level::Error,
                    code: "TS9020",
                    name: "storage-outside-store",
                    path: layout.path.to_string(),
                    line: layout.line_of(at),
                    message: format!(
                        "the root layout does not render the store provider <{name}> (`{}`; only \
                         `{}` does), yet {} file(s) call `{}()` ({shown}{more}): outside its \
                         provider the hook has no state (or throws). Import {name} here and wrap \
                         `{{children}}` in <{name}>...</{name}> inside <body>, around the shell too.",
                        provider.path,
                        rendered.join("`, `"),
                        callers.len(),
                        hook.name
                    ),
                });
            }
        }
    }
    // No provider: each page loads its own state, which is the app's design;
    // only a component that stays mounted (the layout's) and saves a copy it
    // loaded once loses the pages' changes.
    let saves = |file: &File| {
        let code = blank_strings(&file.code);
        code.contains("localStorage.setItem")
            || storage_fns.iter().any(|(_, fns)| {
                fns.iter().any(|name| {
                    let lower = name.to_lowercase();
                    (lower.starts_with("save")
                        || lower.starts_with("write")
                        || lower.starts_with("persist"))
                        && word_positions(&code, name)
                            .iter()
                            .any(|at| code[at + name.len()..].trim_start().starts_with('('))
                })
            })
    };
    let loads = |file: &File| {
        let code = blank_strings(&file.code);
        code.contains("localStorage.getItem")
            || storage_fns.iter().any(|(_, fns)| {
                fns.iter().any(|name| {
                    let lower = name.to_lowercase();
                    (lower.starts_with("load")
                        || lower.starts_with("read")
                        || lower.starts_with("get"))
                        && word_positions(&code, name)
                            .iter()
                            .any(|at| code[at + name.len()..].trim_start().starts_with('('))
                })
            })
    };
    let pages_save = app
        .scripts()
        .any(|file| route_segments(file.path).is_some() && saves(file));
    if !pages_save {
        return lints;
    }
    for file in app.scripts() {
        if !is_ui_path(file.path) || route_segments(file.path).is_some() || !of_layout(file) {
            continue;
        }
        if !(loads(file) && saves(file)) {
            continue;
        }
        // Through the app's storage module (a theme kept under its own
        // `localStorage` key overwrites nothing).
        let Some((at, what)) = reach(file).filter(|(_, what)| what.starts_with("calls")) else {
            continue;
        };
        lints.push(Lint {
            level: Level::Error,
            code: "TS9020",
            name: "storage-outside-store",
            path: file.path.to_string(),
            line: file.line_of(at),
            message: format!(
                "this layout component {what}: the root layout keeps it mounted while the pages \
                 change, so the state it loaded once goes stale, and saving it (the theme toggle) \
                 overwrites every change the pages saved since. Keep no copy of the app state here: \
                 load the stored state again right before each save and change only what this \
                 component owns (`saveState({{ ...loadState(), settings: {{ ...loadState().settings, \
                 theme }} }})`)."
            ),
        });
    }
    lints
}

// ---- duplicate-shell -------------------------------------------------------

/// Whether `code` renders JSX element `name` (`<Name>`, `<Name/>`, `<Name `).
fn renders_tag(code: &str, name: &str) -> Option<usize> {
    let tag = format!("<{name}");
    code.match_indices(&tag).map(|(at, _)| at).find(|&at| {
        code.as_bytes()
            .get(at + tag.len())
            .is_none_or(|byte| !is_ident(*byte))
    })
}

/// The class tokens of the elements of `code` whose tag is `nav`, `header`
/// or `aside`, and every class token with `shell` in it.
fn shell_classes(code: &str) -> BTreeSet<String> {
    let mut found = BTreeSet::new();
    for (at, _) in code.match_indices("className=") {
        let rest = &code[at + "className=".len()..];
        let rest = rest.trim_start_matches('{').trim_start();
        let Some(quote) = rest
            .chars()
            .next()
            .filter(|ch| matches!(ch, '"' | '\'' | '`'))
        else {
            continue;
        };
        let Some(end) = rest[1..].find(quote) else {
            continue;
        };
        let classes = &rest[1..1 + end];
        let tag_at = code[..at].rfind('<').unwrap_or(0);
        let tag: String = code[tag_at + 1..]
            .chars()
            .take_while(|ch| ch.is_ascii_alphanumeric())
            .collect();
        for token in classes.split_whitespace() {
            if token.contains("${") {
                continue;
            }
            if matches!(tag.as_str(), "nav" | "header" | "aside") || token.contains("shell") {
                found.insert(token.to_string());
            }
        }
    }
    found
}

/// What the root layout draws around every page.
struct Frame<'a> {
    layout: &'a str,
    /// The project components the layout renders (not providers): (name,
    /// path).
    shells: Vec<(String, String)>,
    /// The class names of the layout's own frame (and its shells').
    classes: BTreeSet<String>,
    /// Every project script that the layout imports is in the app (false
    /// while one has not landed: the frame may be larger than seen).
    complete: bool,
}

/// True for a specifier of a project script (`./x`, `../x.tsx`, `@/x`), not
/// a style sheet or another asset.
fn is_script_spec(spec: &str) -> bool {
    if !(spec.starts_with('.') || spec.starts_with("@/")) {
        return false;
    }
    let last = spec.rsplit('/').next().unwrap_or(spec);
    match last.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => {
            matches!(ext, "ts" | "tsx" | "js" | "jsx")
        }
        _ => true,
    }
}

/// The frame of the app's root layout, if it has one.
fn layout_frame<'a>(app: &App<'a>) -> Option<Frame<'a>> {
    let layout = root_layout(app)?;
    // The project components the layout renders (not providers).
    let mut shells: Vec<(String, String)> = Vec::new();
    let mut complete = true;
    for (names, spec) in imports(&layout.code) {
        let Some(module) = resolve(app, layout.path, &spec) else {
            complete &= !is_script_spec(&spec);
            continue;
        };
        if module.code.contains("createContext")
            || (is_lib_path(module.path) && !module.path.ends_with(".tsx"))
        {
            continue;
        }
        for name in names {
            if name.ends_with("Provider") || name.ends_with("Providers") {
                continue;
            }
            if renders_tag(&layout.code, &name).is_some() {
                shells.push((name, module.path.to_string()));
            }
        }
    }
    // The class names of the layout's own frame (and its shells').
    let mut classes = shell_classes(&blank_comments(layout.raw));
    for (_, path) in &shells {
        if let Some(module) = app.get(path) {
            classes.extend(shell_classes(&module.code));
        }
    }
    Some(Frame {
        layout: layout.path,
        shells,
        classes,
        complete,
    })
}

/// The names under which `page` imports a shell component of `frame`.
fn imported_shells(app: &App, frame: &Frame, page: &File) -> Vec<String> {
    let mut found = Vec::new();
    for (names, spec) in imports(&page.code) {
        let Some(module) = resolve(app, page.path, &spec) else {
            continue;
        };
        if frame.shells.iter().any(|(_, path)| *path == module.path) {
            found.extend(names);
        }
    }
    found
}

/// Where and how `page` draws the frame of the layout again, if it does.
fn page_duplicate(app: &App, frame: &Frame, page: &File) -> Option<(usize, String)> {
    for name in imported_shells(app, frame, page) {
        if let Some(at) = renders_tag(&page.code, &name) {
            return Some((
                at,
                format!("renders <{name}> again, so the navigation shows twice"),
            ));
        }
    }
    let own = shell_classes(&page.code);
    let token = own.iter().find(|token| frame.classes.contains(*token))?;
    let at = page
        .code
        .match_indices("className=")
        .map(|(at, _)| at)
        .find(|&at| {
            let attr = page.code[at..].split(['\n', '>']).next().unwrap_or("");
            attr.split(['"', '\'', '`', ' ', '{', '}'])
                .any(|word| word == token)
        })
        .unwrap_or(0);
    Some((
        at,
        format!(
            "draws the layout's frame again (class `{token}`), so the frame is nested \
             in itself (its padding, width and grid apply twice)"
        ),
    ))
}

fn duplicate_shell(app: &App) -> Vec<Lint> {
    match layout_frame(app) {
        Some(frame) => duplicate_shell_in(app, &frame, false),
        None => Vec::new(),
    }
}

/// The `duplicate-shell` lints of the pages of `app` against `frame`.
/// `in_wave`: the line names the layout as context (``from `app/layout.tsx` ``),
/// so a repair during the wave changes the page alone.
fn duplicate_shell_in(app: &App, frame: &Frame, in_wave: bool) -> Vec<Lint> {
    let mut lints = Vec::new();
    for (_, page) in app.pages() {
        let Some((at, what)) = page_duplicate(app, frame, page) else {
            continue;
        };
        let shown = frame
            .shells
            .first()
            .map(|(name, _)| format!("<{name}>"))
            .unwrap_or_else(|| "its navigation frame".to_string());
        let lead = if in_wave {
            format!("The root layout, from `{}`,", frame.layout)
        } else {
            format!("`{}`", frame.layout)
        };
        lints.push(Lint {
            level: Level::Error,
            code: "TS9021",
            name: "duplicate-shell",
            path: page.path.to_string(),
            line: page.line_of(at),
            message: format!(
                "{lead} already renders {shown} around every page, and this page {what}. Remove it \
                 (and its import or wrapper) from this page and return only the page's own content."
            ),
        });
    }
    lints
}

/// The app lints that a check during the wave can run on `files` (the
/// files that landed so far): only those whose inputs have all landed, so
/// that each line is the one the gate after the wave gives. Today
/// `duplicate-shell` (TS9021), which reads a page, the root layout and the
/// components that the layout imports; its line names the layout as context
/// only. Returns the `ERROR` lines and the pages whose lint waits for a file
/// that has not landed (the root layout or a script it imports).
pub fn landed_lines(files: &[(String, String)]) -> (Vec<String>, BTreeSet<String>) {
    let app = App::new(files);
    let frame = layout_frame(&app).filter(|frame| frame.complete);
    let Some(frame) = frame else {
        let waiting = app
            .pages()
            .into_iter()
            .map(|(_, page)| page.path.to_string())
            .collect();
        return (Vec::new(), waiting);
    };
    let lines = duplicate_shell_in(&app, &frame, true)
        .iter()
        .map(Lint::render)
        .collect();
    (lines, BTreeSet::new())
}

// ---- duplicate-shell fix (`shellfix`) --------------------------------------

/// The pages of `files` that `duplicate-shell` names, fixed with no model
/// call: each `return` whose JSX root is the layout's frame (a shell
/// component of the layout, or an element whose class is the frame's)
/// returns its children instead (in a fragment `<>...</>`; an element that
/// has other classes keeps them and loses only the frame's), and an import
/// of the shell that nothing uses any more goes. A page is fixed only when
/// the change is clear (no other attribute is lost, every tag closes, the
/// import has a one-line form) and the lint passes on the fixed page;
/// otherwise it is left for the repair. Returns (path, fixed text) for each
/// fixed page, and the paths of the pages left alone.
pub fn fix_duplicate_shell(files: &[(String, String)]) -> (Vec<(String, String)>, Vec<String>) {
    let app = App::new(files);
    let Some(frame) = layout_frame(&app) else {
        return (Vec::new(), Vec::new());
    };
    let (mut fixed, mut left) = (Vec::new(), Vec::new());
    for (_, page) in app.pages() {
        if page_duplicate(&app, &frame, page).is_none() {
            continue;
        }
        let text = unwrap_frame(&app, &frame, page).filter(|text| {
            let code = blank_comments(text);
            let after = File {
                path: page.path,
                raw: text,
                code,
            };
            page_duplicate(&app, &frame, &after).is_none()
        });
        match text {
            Some(text) => fixed.push((page.path.to_string(), text)),
            None => left.push(page.path.to_string()),
        }
    }
    (fixed, left)
}

/// The end of the JSX opening tag that starts at `at` (`<`): the offset of
/// its `>` and whether it closes itself (`/>`). Skips strings and `{...}`
/// expressions (an arrow's `=>` inside them).
fn tag_end(code: &str, at: usize) -> Option<(usize, bool)> {
    let bytes = code.as_bytes();
    let mut index = at + 1;
    let mut depth = 0usize;
    let mut quote: Option<u8> = None;
    while index < bytes.len() {
        let byte = bytes[index];
        if let Some(open) = quote {
            if byte == b'\\' {
                index += 2;
                continue;
            }
            if byte == open {
                quote = None;
            }
            index += 1;
            continue;
        }
        match byte {
            b'"' | b'\'' | b'`' => quote = Some(byte),
            b'{' => depth += 1,
            b'}' => depth = depth.checked_sub(1)?,
            b'>' if depth == 0 => {
                return Some((index, prev_char(code, index) == Some(b'/')));
            }
            _ => {}
        }
        index += 1;
    }
    None
}

/// The closing tag `</tag>` of the element whose opening tag ends at
/// `open_end` (its `>`): (start of `</`, end after its `>`). Counts the
/// nested elements of the same tag, and skips a string or template literal
/// that opens a `{...}` expression (`{"</div>"}`), whose `<` is no tag.
fn closing_tag(code: &str, open_end: usize, tag: &str) -> Option<(usize, usize)> {
    let bytes = code.as_bytes();
    let mut depth = 1usize;
    let mut index = open_end + 1;
    let ends_name = |at: usize| {
        bytes
            .get(at)
            .is_some_and(|byte| !is_ident(*byte) && *byte != b'.')
    };
    while let Some(offset) = code[index..].find(['<', '{']) {
        let at = index + offset;
        if bytes[at] == b'{' {
            index = skip_leading_literal(bytes, at + 1);
            continue;
        }
        if code[at + 1..].starts_with('/') {
            let name_at = at + 2;
            if code[name_at..].starts_with(tag) && ends_name(name_at + tag.len()) {
                let close = name_at + code[name_at..].find('>')?;
                depth -= 1;
                if depth == 0 {
                    return Some((at, close + 1));
                }
                index = close + 1;
                continue;
            }
        } else if code[at + 1..].starts_with(tag) && ends_name(at + 1 + tag.len()) {
            let (end, closes) = tag_end(code, at)?;
            if !closes {
                depth += 1;
            }
            index = end + 1;
            continue;
        }
        index = at + 1;
    }
    None
}

/// The offset after the string or template literal that starts at `at`
/// (after white space), or `at` when none starts there or it never closes.
fn skip_leading_literal(bytes: &[u8], at: usize) -> usize {
    let mut index = at;
    while bytes.get(index).is_some_and(u8::is_ascii_whitespace) {
        index += 1;
    }
    let Some(&quote) = bytes
        .get(index)
        .filter(|b| matches!(b, b'"' | b'\'' | b'`'))
    else {
        return at;
    };
    index += 1;
    while let Some(&byte) = bytes.get(index) {
        if byte == b'\\' {
            index += 2;
            continue;
        }
        if byte == quote {
            return index + 1;
        }
        if byte == b'\n' && quote != b'`' {
            return at;
        }
        index += 1;
    }
    at
}

/// Whether the children of an element unwrapped by `shellfix` (the code
/// between its opening and its closing tag) are whole: their `{` and `}`
/// pair up, and every `<tag` that does not close itself has its `</tag>`.
/// A closing tag found inside a literal (`{ok ? "a" : "</div>"}`) leaves the
/// children cut, and the page is left for the repair.
fn whole_children(children: &str, tag: &str) -> bool {
    // The literals that head an expression are text, as `closing_tag` reads
    // them: blank them.
    let mut blank = children.as_bytes().to_vec();
    let mut index = 0;
    while index < blank.len() {
        if blank[index] == b'{' {
            let end = skip_leading_literal(children.as_bytes(), index + 1);
            for byte in &mut blank[index + 1..end] {
                if !matches!(*byte, b'"' | b'\'' | b'`' | b'\n') {
                    *byte = b' ';
                }
            }
            index = end;
        } else {
            index += 1;
        }
    }
    let Ok(children) = String::from_utf8(blank) else {
        return false;
    };
    let children = children.as_str();
    let mut depth = 0isize;
    for byte in children.bytes() {
        match byte {
            b'{' => depth += 1,
            b'}' => depth -= 1,
            _ => {}
        }
        if depth < 0 {
            return false;
        }
    }
    if depth != 0 {
        return false;
    }
    let bytes = children.as_bytes();
    let ends_name = |at: usize| {
        bytes
            .get(at)
            .is_none_or(|byte| !is_ident(*byte) && *byte != b'.')
    };
    let open = format!("<{tag}");
    let opens = children
        .match_indices(&open)
        .filter(|(at, _)| ends_name(at + open.len()))
        .filter(|(at, _)| tag_end(children, *at).is_some_and(|(_, closes)| !closes))
        .count();
    let close = format!("</{tag}");
    let closes = children
        .match_indices(&close)
        .filter(|(at, _)| ends_name(at + close.len()))
        .count();
    opens == closes
}

/// The value of the `className` attribute in the attribute text `attrs`
/// when it is a plain string (`className="a b"`, `className={"a b"}`):
/// (start, end of the whole attribute in `attrs`, the value).
fn plain_class(attrs: &str) -> Option<(usize, usize, &str)> {
    // An attribute of this tag, not one inside an expression (`fallback={...}`).
    let blank = blank_strings(attrs);
    let at = word_positions(&blank, "className")
        .into_iter()
        .find(|&at| blank[..at].matches('{').count() == blank[..at].matches('}').count())?;
    let rest = &attrs[at + "className".len()..];
    let after_eq = rest.strip_prefix('=')?;
    let (braced, value) = match after_eq.strip_prefix('{') {
        Some(inner) => (true, inner.trim_start()),
        None => (false, after_eq),
    };
    let quote = value.chars().next().filter(|ch| matches!(ch, '"' | '\''))?;
    let close = value[1..].find(quote)? + 1;
    let class = &value[1..close];
    let mut end = attrs.len() - value.len() + close + 1;
    if braced {
        let tail = &attrs[end..];
        let trimmed = tail.trim_start();
        if !trimmed.starts_with('}') {
            return None;
        }
        end += tail.len() - trimmed.len() + 1;
    }
    Some((at, end, class))
}

/// `page` with the layout's frame unwrapped from the JSX root of each of its
/// `return`s (see [`fix_duplicate_shell`]); `None` when nothing changed or
/// the change is not clear.
fn unwrap_frame(app: &App, frame: &Frame, page: &File) -> Option<String> {
    let code = page.code.as_str();
    let shells = imported_shells(app, frame, page);
    // (start, end, replacement) in `code`'s offsets, which are the raw text's.
    let mut edits: Vec<(usize, usize, String)> = Vec::new();
    for at in word_positions(code, "return") {
        let mut root = at + "return".len();
        while code
            .as_bytes()
            .get(root)
            .is_some_and(u8::is_ascii_whitespace)
        {
            root += 1;
        }
        if code.as_bytes().get(root) == Some(&b'(') {
            root += 1;
            while code
                .as_bytes()
                .get(root)
                .is_some_and(u8::is_ascii_whitespace)
            {
                root += 1;
            }
        }
        if code.as_bytes().get(root) != Some(&b'<') {
            continue;
        }
        let tag: String = code[root + 1..]
            .bytes()
            .take_while(|byte| is_ident(*byte) || *byte == b'.')
            .map(char::from)
            .collect();
        if tag.is_empty() {
            continue;
        }
        let (open_end, closes) = tag_end(code, root)?;
        let attrs = &code[root + 1 + tag.len()..open_end];
        if !shells.contains(&tag) {
            let Some((start, end, class)) = plain_class(attrs) else {
                continue;
            };
            let frame_tag = matches!(tag.as_str(), "nav" | "header" | "aside");
            let (ours, others): (Vec<&str>, Vec<&str>) =
                class.split_whitespace().partition(|token| {
                    frame.classes.contains(*token) && (frame_tag || token.contains("shell"))
                });
            if ours.is_empty() {
                continue;
            }
            if !others.is_empty() {
                // Keep the element and its other classes.
                let attr_at = root + 1 + tag.len();
                edits.push((
                    attr_at + start,
                    attr_at + end,
                    format!("className=\"{}\"", others.join(" ")),
                ));
                continue;
            }
            let rest = format!("{}{}", &attrs[..start], &attrs[end..]);
            if !rest.trim().is_empty() {
                return None;
            }
        } else if !attrs.trim().is_empty() {
            return None;
        }
        if closes {
            return None;
        }
        let (close_start, close_end) = closing_tag(code, open_end, &tag)?;
        if !whole_children(&code[open_end + 1..close_start], &tag) {
            return None;
        }
        edits.push((root, open_end + 1, "<>".to_string()));
        edits.push((close_start, close_end, "</>".to_string()));
    }
    if edits.is_empty() {
        return None;
    }
    edits.sort_by_key(|edit| std::cmp::Reverse(edit.0));
    let mut text = page.raw.to_string();
    for (start, end, replacement) in edits {
        text.replace_range(start..end, &replacement);
    }
    // A shell that only its import names any more (its path is a string).
    for name in &shells {
        let code = blank_strings(&blank_comments(&text));
        if word_positions(&code, name).len() == 1 {
            text = drop_import(&text, name)?;
        }
    }
    Some(text)
}

/// `source` without the import of `name` (its one-line `import` statement,
/// or `name` out of its list); `None` when the statement has another form.
fn drop_import(source: &str, name: &str) -> Option<String> {
    let mut offset = 0;
    for line in source.split_inclusive('\n') {
        let trimmed = line.trim();
        let is_import = trimmed.starts_with("import ")
            && trimmed.contains(" from ")
            && word_positions(&blank_strings(trimmed), name).len() == 1;
        if !is_import {
            offset += line.len();
            continue;
        }
        let statement = trimmed.trim_end_matches(';');
        let from = statement.rfind(" from ")?;
        let head = statement["import ".len()..from].trim();
        let tail = &statement[from..];
        let (default, named) = match head.split_once('{') {
            Some((before, list)) => (
                before.trim().trim_end_matches(',').trim(),
                Some(list.strip_suffix('}')?.trim()),
            ),
            None => (head, None),
        };
        let names: Vec<&str> = named
            .map(|list| {
                list.split(',')
                    .map(str::trim)
                    .filter(|item| !item.is_empty())
                    .collect()
            })
            .unwrap_or_default();
        let default = (!default.is_empty()).then_some(default);
        let (default, names) = if default == Some(name) {
            (None, names)
        } else if names.contains(&name) {
            (
                default,
                names.into_iter().filter(|item| *item != name).collect(),
            )
        } else {
            return None;
        };
        let semi = if trimmed.ends_with(';') { ";" } else { "" };
        let ending = &line[line.trim_end().len()..];
        let indent = &line[..line.len() - line.trim_start().len()];
        let replacement = match (default, names.is_empty()) {
            (None, true) => String::new(),
            (Some(default), true) => format!("{indent}import {default}{tail}{semi}{ending}"),
            (None, false) => format!(
                "{indent}import {{ {} }}{tail}{semi}{ending}",
                names.join(", ")
            ),
            (Some(default), false) => format!(
                "{indent}import {default}, {{ {} }}{tail}{semi}{ending}",
                names.join(", ")
            ),
        };
        let mut out = source.to_string();
        out.replace_range(offset..offset + line.len(), &replacement);
        return Some(out);
    }
    None
}

// ---- activity-unwritten ----------------------------------------------------

/// The fields of `interface AppState` in the types file that hold an
/// activity log (`activity`, `activities`, `history`, `log`, `events`) as an
/// array: (field, offset in the types file).
fn log_fields(code: &str) -> Vec<(String, usize)> {
    let Some(at) = code.find("interface AppState") else {
        return Vec::new();
    };
    let Some(open) = code[at..].find('{').map(|open| at + open) else {
        return Vec::new();
    };
    let Some(close) = matching(code, open) else {
        return Vec::new();
    };
    let mut found = Vec::new();
    let mut offset = open + 1;
    for line in code[open + 1..close].split('\n') {
        let trimmed = line.trim_start();
        let name: String = trimmed
            .bytes()
            .take_while(|byte| is_ident(*byte))
            .map(char::from)
            .collect();
        let rest = trimmed[name.len()..].trim_start();
        let log = matches!(
            name.to_ascii_lowercase().as_str(),
            "activity" | "activities" | "activitylog" | "history" | "log" | "events"
        );
        if log && rest.starts_with(':') && rest.contains("[]") {
            found.push((name, offset + (line.len() - trimmed.len())));
        }
        offset += line.len() + 1;
    }
    found
}

/// Whether the word at `pos` is a shorthand property of an object literal
/// that spreads another (`{ ...state, tasks, activity }`): a write of the
/// field. Destructuring (`const { activity } = state`, `({ activity }) =>`)
/// is not.
fn shorthand_write(code: &str, pos: usize, len: usize) -> bool {
    let before = code[..pos].trim_end();
    let after = code[pos + len..].trim_start();
    if !(before.ends_with(',') || before.ends_with('{'))
        || !(after.starts_with('}') || after.starts_with(','))
    {
        return false;
    }
    let Some(open) = code[..pos].rfind('{') else {
        return false;
    };
    let Some(close) = matching(code, open) else {
        return false;
    };
    if close < pos || !code[open..close].contains("...") {
        return false;
    }
    let rest = code[close + 1..].trim_start();
    !(rest.starts_with('=') && !rest.starts_with("=="))
        && !rest.starts_with(':')
        && !rest.starts_with(") =>")
        && !rest.starts_with(")=>")
}

/// TS9033: the state type holds an activity log, but only the seed fills it
/// (audit 7, f1: every feed entry was a seed entry; moves, edits and
/// comments never showed). A writer is a file other than the types, seed,
/// storage and test files whose code builds the field (`activity: [`,
/// `activity: [entry, ...s.activity]`, `.activity.push(`); a writer in a lib
/// module counts only when another file calls one of its exports.
fn activity_unwritten(app: &App) -> Vec<Lint> {
    let Some(types) = app
        .scripts()
        .find(|file| file.path.ends_with("lib/types.ts"))
    else {
        return Vec::new();
    };
    let mut lints = Vec::new();
    for (field, at) in log_fields(&types.code) {
        let passive = |path: &str| {
            let stem = path.rsplit('/').next().unwrap_or(path);
            is_test(path)
                || stem.starts_with("types.")
                || stem.starts_with("seed")
                || stem.starts_with("storage")
        };
        let writes = |code: &str| {
            word_positions(code, &field).into_iter().any(|pos| {
                let after = code[pos + field.len()..].trim_start();
                let before = code[..pos].trim_end();
                (after.starts_with(':')
                    && !after[1..]
                        .trim_start()
                        .starts_with(|ch: char| ch.is_ascii_uppercase()))
                    || (before.ends_with('.')
                        && (after.starts_with(".push(") || after.starts_with(".unshift(")))
                    || shorthand_write(code, pos, field.len())
            })
        };
        let written = app
            .scripts()
            .filter(|file| !passive(file.path))
            .any(|file| {
                if !writes(&file.code) {
                    return false;
                }
                if !is_lib_path(file.path) {
                    return true;
                }
                // A lib writer counts once the function that writes is
                // called: from another file, or inside its own file by
                // another function (a private `logActivity` that the
                // exported mutations call).
                functions(&file.code)
                    .iter()
                    .filter(|func| writes(func.body(&file.code)))
                    .any(|func| {
                        word_positions(&file.code, &func.name).len() > 1
                            || app.scripts().any(|other| {
                                other.path != file.path
                                    && !passive(other.path)
                                    && !word_positions(&other.code, &func.name).is_empty()
                            })
                    })
            });
        if written {
            continue;
        }
        // A WARN: the fix is in the lib modules that change the state, not
        // in the file the line names, so a repair of that file alone cannot
        // clear it (20261116-125718-r7-1: three rejected rounds, 94 s).
        lints.push(Lint {
            level: Level::Warn,
            code: "TS9033",
            name: "activity-unwritten",
            path: types.path.to_string(),
            line: types.line_of(at),
            message: format!(
                "`AppState.{field}` is the activity log, but no file except the seed ever adds \
                 an entry to it: the feed shows only the seed's entries, and creating, moving, \
                 editing or commenting never appears. Give the lib module of the log one \
                 function that appends an entry (`{{ id, text, at: new Date().toISOString() }}`) \
                 and call it from every lib function or store action whose change the feed \
                 shows (create, move, edit, comment, archive)."
            ),
        });
    }
    lints
}

// ---- hardcoded-currency ----------------------------------------------------

const CURRENCY_CODES: &[&str] = &[
    "USD", "EUR", "GBP", "JPY", "CAD", "AUD", "CHF", "CNY", "INR", "SEK", "NZD", "MXN", "BRL",
];

fn hardcoded_currency(app: &App) -> Vec<Lint> {
    let settings = settings_fields(app);
    let Some(declared) = settings
        .iter()
        .find(|(field, _)| field.to_lowercase() == "currency")
        .map(|(_, path)| path.clone())
    else {
        return Vec::new();
    };
    // The store hook: an exported `useX` of a context provider that is not
    // only a theme's.
    let hook = app
        .scripts()
        .filter(|file| {
            file.code.contains("createContext")
                && !file_name(file.path).to_lowercase().starts_with("theme")
        })
        .find_map(|file| {
            let funcs = functions(&file.code);
            funcs
                .iter()
                .any(|func| func.exported && func.name.ends_with("Provider"))
                .then(|| {
                    funcs.into_iter().find(|func| {
                        func.exported
                            && func.name.strip_prefix("use").is_some_and(|rest| {
                                rest.starts_with(|ch: char| ch.is_ascii_uppercase())
                                    && !rest.starts_with("Theme")
                            })
                    })
                })
                .flatten()
                .map(|func| func.name)
        });
    let mut lints = Vec::new();
    for file in app.scripts() {
        if !is_ui_path(file.path) {
            continue;
        }
        let code = &file.code;
        let mut hit = None;
        for currency in CURRENCY_CODES {
            for quote in ['\'', '"'] {
                let literal = format!("{quote}{currency}{quote}");
                for (at, _) in code.match_indices(&literal) {
                    let before = code[..at].trim_end();
                    // A fallback (`settings.currency ?? 'USD'`) or a default.
                    if before.ends_with("??") || before.ends_with("||") || before.ends_with('=') {
                        continue;
                    }
                    let Some(callee) = enclosing_call(code, at) else {
                        continue;
                    };
                    let lower = callee.to_lowercase();
                    let formats = lower.contains("format")
                        || lower.contains("money")
                        || lower.contains("currency")
                        || lower.ends_with("tolocalestring");
                    if formats {
                        hit = Some((at, currency, callee.to_string()));
                        break;
                    }
                }
                if hit.is_some() {
                    break;
                }
            }
            if hit.is_some() {
                break;
            }
        }
        let Some((at, currency, callee)) = hit else {
            continue;
        };
        // Where the file gets the currency: a page from the store hook (or
        // the settings it loads), a component that reads the store from its
        // hook, any other component from a new prop that its renderers pass.
        let fix = if route_segments(file.path).is_some() {
            match &hook {
                Some(hook) => format!("read it from the store hook (`{hook}().settings.currency`)"),
                None => {
                    "read it from the settings this page loads (`settings.currency`)".to_string()
                }
            }
        } else if let Some(hook) = hook.as_ref().filter(|hook| {
            word_positions(code, hook)
                .iter()
                .any(|at| code[at + hook.len()..].trim_start().starts_with('('))
        }) {
            format!(
                "read it from `{hook}()` (`settings.currency`), which this component already uses"
            )
        } else {
            let renderers = renderers_of(app, file);
            let verb = if renderers.len() > 1 {
                "pass"
            } else {
                "passes"
            };
            let named = if renderers.is_empty() {
                "each file that renders it".to_string()
            } else {
                renderers
                    .iter()
                    .map(|path| format!("`{path}`"))
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            format!(
                "take it as an optional prop (`currency?: string`, used as `currency ?? \
                 '{currency}'` only while a renderer passes none); {named} then {verb} \
                 `currency={{settings.currency}}`"
            )
        };
        lints.push(Lint {
            level: Level::Error,
            code: "TS9025",
            name: "hardcoded-currency",
            path: file.path.to_string(),
            line: file.line_of(at),
            message: format!(
                "this file formats money with a fixed '{currency}' (`{callee}`) while the settings \
                 have a `currency` field ({declared}): a user who picks another currency still sees \
                 '{currency}' here. Pass the settings' currency to the money helper instead of \
                 '{currency}': {fix}."
            ),
        });
    }
    // The other half of the prop fix: a component that takes a `currency`
    // prop and falls back to a fixed code, rendered without one.
    for component in app.scripts().filter(|file| {
        is_ui_path(file.path) && route_segments(file.path).is_none() && !is_test(file.path)
    }) {
        let takes = functions(&component.code).iter().any(|func| {
            func.exported && !word_positions(func.params(&component.code), "currency").is_empty()
        });
        let falls_back = CURRENCY_CODES.iter().any(|currency| {
            ['\'', '"'].iter().any(|quote| {
                component
                    .code
                    .match_indices(&format!("{quote}{currency}{quote}"))
                    .any(|(at, _)| {
                        let before = component.code[..at].trim_end();
                        (before.ends_with("??") || before.ends_with('='))
                            && before
                                .trim_end_matches(['?', '='])
                                .trim_end()
                                .ends_with("currency")
                    })
            })
        });
        if !(takes && falls_back) {
            continue;
        }
        for (path, at, name) in renders_without(app, component, "currency") {
            let Some(renderer) = app.get(path) else {
                continue;
            };
            lints.push(Lint {
                level: Level::Error,
                code: "TS9025",
                name: "hardcoded-currency",
                path: path.to_string(),
                line: renderer.line_of(at),
                message: format!(
                    "this file renders <{name}> without its `currency` prop, so `{}` shows its \
                     fixed fallback currency while the settings have a `currency` field \
                     ({declared}). Pass `currency={{settings.currency}}` (from the settings this \
                     file reads, or the store hook).",
                    component.path
                ),
            });
        }
    }
    lints
}

/// The exported component names of `file` (capitalised exported functions).
fn component_names(file: &File) -> Vec<String> {
    functions(&file.code)
        .into_iter()
        .filter(|func| func.exported && func.name.starts_with(|ch: char| ch.is_ascii_uppercase()))
        .map(|func| func.name)
        .collect()
}

/// The UI files that import `file` and render one of its components.
fn renderers_of<'a>(app: &'a App, file: &File) -> Vec<&'a str> {
    let names = component_names(file);
    app.scripts()
        .filter(|other| {
            other.path != file.path
                && is_ui_path(other.path)
                && imports(&other.code).iter().any(|(_, spec)| {
                    resolve(app, other.path, spec).is_some_and(|module| module.path == file.path)
                })
                && names
                    .iter()
                    .any(|name| renders_tag(&other.code, name).is_some())
        })
        .map(|other| other.path)
        .collect()
}

/// The text of the JSX opening tag at `at` (`<Name a={() => b}>`), up to
/// its `>` outside braces.
fn tag_text(code: &str, at: usize) -> &str {
    let mut depth = 0i32;
    for (offset, byte) in code.as_bytes()[at..].iter().enumerate() {
        match byte {
            b'{' => depth += 1,
            b'}' => depth -= 1,
            b'>' if depth <= 0 => return &code[at..at + offset + 1],
            _ => {}
        }
    }
    &code[at..]
}

/// The tags of `component`'s components that its renderers write without
/// prop `prop`: (renderer, offset, component name). A tag that spreads its
/// props (`{...props}`) may pass it.
fn renders_without<'a>(
    app: &'a App,
    component: &File,
    prop: &str,
) -> Vec<(&'a str, usize, String)> {
    let names = component_names(component);
    let mut found = Vec::new();
    for path in renderers_of(app, component) {
        let Some(renderer) = app.get(path) else {
            continue;
        };
        for name in &names {
            let tag = format!("<{name}");
            for (at, _) in renderer.code.match_indices(&tag) {
                if renderer
                    .code
                    .as_bytes()
                    .get(at + tag.len())
                    .is_some_and(|byte| is_ident(*byte))
                {
                    continue;
                }
                let text = tag_text(&renderer.code, at);
                if word_positions(text, prop).is_empty() && !text.contains("{...") {
                    found.push((path, at, name.clone()));
                    break;
                }
            }
        }
    }
    found
}

// ---- qlint2, audit 3: the store's first render, updaters, search, catalogs -
//
// From the third blind audit (TaskFlow, the base stack against qsemantic and
// qlint2): with one store whose first render is the seed (storage loads in an
// effect), a detail page called notFound() on a record the user had created
// and a form kept the seed values, then saved them over the user's edits; a
// lib function that throws ran inside a state updater, where the try around
// it never sees the error; a comment hit linked to /tasks/<commentId>; two
// label catalogs had different ids.

/// The names of a flag that says the stored state was read.
const READY_WORDS: &[&str] = &[
    "ready",
    "hydrated",
    "loaded",
    "isReady",
    "isHydrated",
    "isLoaded",
    "hasHydrated",
    "hasLoaded",
    "mounted",
    "isMounted",
    // Inverse flags: true until the stored state was read.
    "loading",
    "isLoading",
    "hydrating",
    "isHydrating",
];

/// Whether `word` is an inverse flag (`loading`: true until the load).
fn is_inverse_flag(word: &str) -> bool {
    word.to_lowercase().contains("loading") || word.to_lowercase().contains("hydrating")
}

/// "until `ready` is true" / "while `loading` is true".
fn until_flag(flag: &str) -> String {
    if is_inverse_flag(flag) {
        format!("while `{flag}` is true")
    } else {
        format!("until `{flag}` is true")
    }
}

/// The first hydration flag that `code` names.
fn ready_word(code: &str) -> Option<&'static str> {
    READY_WORDS
        .iter()
        .copied()
        .find(|word| !word_positions(code, word).is_empty())
}

/// `end` moved back to a char boundary of `text`.
fn boundary(text: &str, end: usize) -> usize {
    let mut end = end.min(text.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    end
}

/// Whether `name` is called in `code` (`name(`).
fn calls(code: &str, name: &str) -> bool {
    word_positions(code, name)
        .iter()
        .any(|at| code[at + name.len()..].starts_with('('))
}

fn is_hook_name(name: &str) -> bool {
    name.strip_prefix("use").is_some_and(|rest| {
        rest.starts_with(|ch: char| ch.is_ascii_uppercase()) && !rest.starts_with("Theme")
    })
}

/// The end of the statement that starts at `from`: the `;` or newline at
/// depth 0 (brackets and strings skipped), or the bracket that closes an
/// enclosing one.
fn statement_end(code: &str, from: usize) -> usize {
    let bytes = code.as_bytes();
    let mut depth = 0i32;
    let mut index = from;
    let mut quote: Option<u8> = None;
    while index < bytes.len() {
        let byte = bytes[index];
        if let Some(open) = quote {
            if byte == b'\\' {
                index += 2;
                continue;
            }
            if byte == open {
                quote = None;
            }
            index += 1;
            continue;
        }
        match byte {
            b'"' | b'\'' | b'`' => quote = Some(byte),
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' => {
                if depth == 0 {
                    return index;
                }
                depth -= 1;
            }
            b';' if depth == 0 => return index,
            b'\n' if depth == 0 => {
                // A chain or an operator on the next line continues it.
                let next = code[index..].trim_start();
                if !next.starts_with(['.', '?', ':', '|', '&', '+']) {
                    return index;
                }
            }
            _ => {}
        }
        index += 1;
    }
    code.len()
}

/// One `const`/`let` declaration: the names it binds, where it is, and its
/// value (to the end of the statement).
struct Decl<'a> {
    names: Vec<String>,
    at: usize,
    value: &'a str,
    /// The offset of the value's first byte.
    value_at: usize,
}

/// The name a destructuring element binds (`a`, `b: c` gives `c`, `d = 1`
/// gives `d`, `...rest` gives `rest`).
fn bound_name(part: &str) -> String {
    let part = part.trim().trim_start_matches("...");
    let part = part.rsplit(':').next().unwrap_or(part);
    let part = part.split('=').next().unwrap_or(part).trim();
    part.bytes()
        .take_while(|byte| is_ident(*byte))
        .map(char::from)
        .collect()
}

fn declarations(code: &str) -> Vec<Decl<'_>> {
    let bytes = code.as_bytes();
    let mut found = Vec::new();
    for keyword in ["const", "let"] {
        for at in word_positions(code, keyword) {
            let start = boundary(code, at + keyword.len());
            let lhs_at = code.len() - code[start..].trim_start().len();
            let (names, mut index) = match bytes.get(lhs_at) {
                Some(b'{' | b'[') => {
                    let Some(close) = matching(code, lhs_at) else {
                        continue;
                    };
                    let names = split_top(&code[lhs_at + 1..close])
                        .iter()
                        .map(|part| bound_name(part))
                        .filter(|name| !name.is_empty())
                        .collect();
                    (names, close + 1)
                }
                _ => {
                    let len = bytes[lhs_at..]
                        .iter()
                        .take_while(|byte| is_ident(**byte))
                        .count();
                    if len == 0 {
                        continue;
                    }
                    (vec![code[lhs_at..lhs_at + len].to_string()], lhs_at + len)
                }
            };
            // An optional type, then `=`.
            let mut found_eq = None;
            let mut depth = 0i32;
            while index < bytes.len() {
                match bytes[index] {
                    b'(' | b'[' | b'{' | b'<' => depth += 1,
                    b')' | b']' | b'}' => {
                        depth -= 1;
                        // `for (const x of xs) {`: no value.
                        if depth < 0 {
                            break;
                        }
                    }
                    b'>' if index > 0 && bytes[index - 1] != b'=' => depth -= 1,
                    b'=' if depth == 0 => {
                        let next = bytes.get(index + 1).copied();
                        if next != Some(b'=') && next != Some(b'>') {
                            found_eq = Some(index);
                            break;
                        }
                    }
                    b';' | b'\n' if depth == 0 => break,
                    _ => {}
                }
                index += 1;
            }
            let Some(eq) = found_eq else {
                continue;
            };
            let end = statement_end(code, eq + 1);
            found.push(Decl {
                names,
                at: lhs_at,
                value: code[eq + 1..end].trim(),
                value_at: code.len() - code[eq + 1..].trim_start().len(),
            });
        }
    }
    found.sort_by_key(|decl| decl.at);
    found
}

/// Whether an expression is a function (`(a) => ...`, `a => ...`,
/// `function ...`, `async ...`).
fn is_function_expr(expr: &str) -> bool {
    let expr = expr.trim();
    let expr = expr.strip_prefix("async").map_or(expr, str::trim_start);
    if expr.starts_with("function") {
        return true;
    }
    if expr.starts_with('(') {
        return matching(expr, 0).is_some_and(|close| {
            let rest = expr[close + 1..].trim_start();
            rest.starts_with("=>") || (rest.starts_with(':') && rest.contains("=>"))
        });
    }
    let len = expr.bytes().take_while(|byte| is_ident(*byte)).count();
    len > 0 && expr[len..].trim_start().starts_with("=>")
}

/// The argument lists of the `useEffect(...)` calls of `code` (byte ranges,
/// the parentheses excluded).
fn effects(code: &str) -> Vec<(usize, usize)> {
    word_positions(code, "useEffect")
        .into_iter()
        .filter_map(|at| {
            let open = at + "useEffect".len();
            if code.as_bytes().get(open) != Some(&b'(') {
                return None;
            }
            matching(code, open).map(|close| (open + 1, close))
        })
        .collect()
}

/// Whether one of the effects of `code` loads the stored state (calls a
/// `load*` or `read*` function, or `localStorage.getItem`).
fn effect_loads(code: &str) -> bool {
    effects(code).iter().any(|&(from, to)| {
        let text = &code[from..to];
        text.contains("getItem(")
            || identifiers_of(text).iter().any(|name| {
                let lower = name.to_lowercase();
                (lower.starts_with("load") || lower.starts_with("read")) && calls(text, name)
            })
    })
}

/// The setters of `useState` in `code` (`const [x, setX] = useState(...)`).
fn state_setters(code: &str) -> Vec<String> {
    declarations(code)
        .into_iter()
        .filter(|decl| decl.names.len() >= 2 && decl.value.contains("useState"))
        .map(|decl| decl.names[1].clone())
        .collect()
}

/// Whether `text` passes a function to `setter` (`setter((prev) => ...)`).
fn passes_function(text: &str, setter: &str) -> bool {
    word_positions(text, setter).into_iter().any(|at| {
        let open = at + setter.len();
        text.as_bytes().get(open) == Some(&b'(')
            && matching(text, open).is_some_and(|close| {
                split_top(&text[open + 1..close])
                    .first()
                    .is_some_and(|arg| is_function_expr(arg))
            })
    })
}

/// The app's store: the context provider (not a theme's) that loads the
/// stored state in an effect, with its hook.
struct Store<'b, 'a> {
    file: &'b File<'a>,
    provider: String,
    hook: String,
    /// The hydration flag of its value, if it has one.
    flag: Option<&'static str>,
    /// The provider renders its children only once the flag is set.
    gates: bool,
    /// Its first render is the seed data (not null or empty collections).
    seeded: bool,
    /// Its change functions that run their callback inside a React updater.
    updaters: Vec<String>,
}

fn store<'b, 'a>(app: &'b App<'a>) -> Option<Store<'b, 'a>> {
    app.scripts()
        .filter(|file| {
            file.code.contains("createContext")
                && !file_name(file.path).to_lowercase().starts_with("theme")
        })
        .find_map(|file| {
            let code = &file.code;
            let funcs = functions(code);
            let provider = funcs
                .iter()
                .find(|func| func.exported && func.name.ends_with("Provider"))?;
            let hook = funcs
                .iter()
                .find(|func| func.exported && is_hook_name(&func.name))?;
            if !effect_loads(provider.body(code)) {
                return None;
            }
            // The value the provider passes, with the declaration of a
            // value variable (`value={value}`).
            let mut value = String::new();
            for (at, _) in code.match_indices(".Provider") {
                let tag = tag_text(code, at);
                let Some(open) = tag.find("value={").map(|index| index + "value=".len()) else {
                    continue;
                };
                let Some(close) = matching(tag, open) else {
                    continue;
                };
                let inner = tag[open + 1..close].trim();
                value.push_str(inner);
                value.push('\n');
                if inner.bytes().all(is_ident) {
                    for decl in declarations(code) {
                        if decl.names.len() == 1 && decl.names[0] == inner {
                            value.push_str(decl.value);
                            value.push('\n');
                        }
                    }
                }
            }
            let flag = READY_WORDS.iter().copied().find(|word| {
                !word_positions(&value, word).is_empty()
                    || !word_positions(hook.body(code), word).is_empty()
            });
            let gates = gates_children(provider.body(code));
            let body = provider.body(code);
            // `useReducer(reducer, init)` or `useReducer(reducer, arg, init)`.
            let reducer_seeds = word_positions(body, "useReducer").into_iter().any(|at| {
                let mut open = at + "useReducer".len();
                if body.as_bytes().get(open) == Some(&b'<') {
                    open = skip_angles(body, open);
                }
                body.as_bytes().get(open) == Some(&b'(')
                    && matching(body, open).is_some_and(|close| {
                        let args = split_top(&body[open + 1..close]);
                        args.len() >= 3 || args.get(1).is_some_and(|init| seeds(init))
                    })
            });
            let seeded = reducer_seeds || state_inits(body).iter().any(|(_, init)| seeds(init));
            let setters = state_setters(code);
            let decls = declarations(code);
            let updaters = identifiers_of(&blank_strings(&value))
                .into_iter()
                .filter(|name| {
                    if setters.contains(name) {
                        return true;
                    }
                    let text = decls
                        .iter()
                        .find(|decl| decl.names.len() == 1 && &decl.names[0] == name)
                        .map(|decl| decl.value)
                        .or_else(|| {
                            funcs
                                .iter()
                                .find(|func| &func.name == name)
                                .map(|func| func.body(code))
                        });
                    text.is_some_and(|text| {
                        setters.iter().any(|setter| passes_function(text, setter))
                    })
                })
                .collect();
            Some(Store {
                file,
                provider: provider.name.clone(),
                hook: hook.name.clone(),
                flag,
                gates,
                seeded,
                updaters,
            })
        })
}

/// Whether a useState initial value may hold records at the first render:
/// anything but `null`, `undefined`, a plain value (`false`, `0`, `''`) or a
/// literal with only empty collections (`{ tasks: [] }`). A seed
/// (`seedState`, `() => freshState()`, `[{ ... }]`) or a call that may read
/// one (`loadState()`, the seed on the server) counts.
fn seeds(init: &str) -> bool {
    let text = blank_strings(init);
    let text = text.trim();
    let text = text
        .strip_prefix("()")
        .and_then(|rest| rest.trim_start().strip_prefix("=>"))
        .map_or(text, str::trim);
    if text.is_empty()
        || ["null", "undefined", "true", "false"].contains(&text)
        || text.starts_with(['"', '\'', '`'])
        || text.parse::<f64>().is_ok()
    {
        return false;
    }
    if text.starts_with(['[', '{', '(']) {
        let flat = text.replace(char::is_whitespace, "");
        let flat = flat.trim_start_matches('(');
        return flat.contains("[{") || flat.contains('(');
    }
    true
}

/// Whether `text` names the variable `name` (not a field `x.name`).
fn names_var(text: &str, name: &str) -> bool {
    word_positions(text, name)
        .into_iter()
        .any(|at| !text[..at].ends_with('.'))
}

/// Whether `at` of `body` is at its top level (outside every bracket).
fn top_level(body: &str, at: usize) -> bool {
    let bytes = body.as_bytes();
    let mut depth = 0i32;
    let mut index = 0;
    let mut quote: Option<u8> = None;
    while index < at.min(bytes.len()) {
        let byte = bytes[index];
        if let Some(open) = quote {
            if byte == b'\\' {
                index += 2;
                continue;
            }
            if byte == open {
                quote = None;
            }
        } else {
            match byte {
                b'"' | b'\'' | b'`' => quote = Some(byte),
                b'(' | b'[' | b'{' => depth += 1,
                b')' | b']' | b'}' => depth -= 1,
                _ => {}
            }
        }
        index += 1;
    }
    depth == 0
}

/// Whether a provider's body renders its children only once the stored state
/// is there: a top-level `if (...) return <Loading/>` (without the children),
/// or children (`children`, `props.children`) inside a branch on the flag:
/// `{ready ? children : null}`, `{!ready ? <Loading/> : props.children}`,
/// `{data && children}`, `return ready ? <Ctx.Provider>{children}...</Ctx.Provider>
/// : <Loading/>`.
fn gates_children(body: &str) -> bool {
    let early = word_positions(body, "if").into_iter().any(|at| {
        if !top_level(body, at) {
            return false;
        }
        let open = body.len() - body[at + 2..].trim_start().len();
        if body.as_bytes().get(open) != Some(&b'(') {
            return false;
        }
        let Some(close) = matching(body, open) else {
            return false;
        };
        let then = body.len() - body[close + 1..].trim_start().len();
        let end = if body.as_bytes().get(then) == Some(&b'{') {
            matching(body, then).unwrap_or(body.len())
        } else {
            statement_end(body, then)
        };
        let consequent = &body[then..end];
        !word_positions(consequent, "return").is_empty()
            && word_positions(consequent, "children").is_empty()
    });
    let branch = word_positions(body, "children").into_iter().any(|at| {
        let before = body[..at].trim_end().trim_end_matches('(').trim_end();
        (before.ends_with('?') && !before.ends_with("??"))
            || before.ends_with("&&")
            || (renders_children(body, at) && flag_gates(body, at))
    });
    early || branch
}

/// Whether the `children` at `at` of `code` is the rendered children (bare or
/// `props.children`), not a field of another object.
fn renders_children(code: &str, at: usize) -> bool {
    let before = &code[..at];
    !before.ends_with('.') || before.ends_with("props.")
}

/// The names of `code` that hold a route parameter (`params`, `const { id } =
/// useParams()`, `const id = Array.isArray(raw) ? raw[0] : raw`).
fn route_ids(code: &str) -> BTreeSet<String> {
    let mut names: BTreeSet<String> = ["params", "searchParams", "useParams", "useSearchParams"]
        .iter()
        .map(|name| name.to_string())
        .collect();
    let decls = declarations(code);
    for _ in 0..3 {
        for decl in &decls {
            // A record looked up by the id is not an id.
            if is_function_expr(decl.value) || decl.value.contains(".find(") {
                continue;
            }
            if names.iter().any(|name| names_var(decl.value, name)) {
                names.extend(decl.names.iter().cloned());
            }
        }
    }
    names
}

/// The records that `code` looks up by a route parameter: (name, offset of
/// the declaration).
fn route_records(code: &str) -> Vec<(String, usize)> {
    let ids = route_ids(code);
    declarations(code)
        .into_iter()
        .filter(|decl| {
            decl.names.len() == 1
                && !is_function_expr(decl.value)
                && !decl.value.contains("useState")
                && !decl.value.contains("useCallback")
                && decl.value.contains(".find(")
                && !ids.contains(&decl.names[0])
                && ids.iter().any(|id| names_var(decl.value, id))
        })
        .map(|decl| (decl.names[0].clone(), decl.at))
        .collect()
}

/// The condition of the innermost `if` whose consequent holds `at`.
fn guard_of(code: &str, at: usize) -> Option<&str> {
    let bytes = code.as_bytes();
    word_positions(&code[..at], "if")
        .into_iter()
        .rev()
        .find_map(|start| {
            let open = code.len() - code[start + 2..].trim_start().len();
            if bytes.get(open) != Some(&b'(') {
                return None;
            }
            let close = matching(code, open)?;
            let then = code.len() - code[close + 1..].trim_start().len();
            let end = if bytes.get(then) == Some(&b'{') {
                matching(code, then)?
            } else {
                statement_end(code, then)
            };
            (close < at && at <= end).then(|| &code[open + 1..close])
        })
}

/// Whether `cond` tests that `name` is missing (`!task`, `task == null`,
/// `task === undefined`).
fn tests_missing(cond: &str, name: &str) -> bool {
    word_positions(cond, name).into_iter().any(|at| {
        let after = cond[at + name.len()..].trim_start();
        let negated = cond[..at].trim_end().ends_with('!')
            && !cond[..at].trim_end().ends_with("!!")
            && !after.starts_with(['.', '[', '?']);
        let compared = ["== null", "=== null", "== undefined", "=== undefined"]
            .iter()
            .any(|test| after.starts_with(test));
        negated || compared
    })
}

/// The calls of `code` that leave the page for a missing record
/// (`notFound()`, `redirect(...)`, `router.replace(...)`): (offset, call).
fn leaves_page(code: &str) -> Vec<(usize, &'static str)> {
    let mut found = Vec::new();
    for name in ["notFound", "redirect"] {
        for at in word_positions(code, name) {
            if code[at + name.len()..].starts_with('(') && !code[..at].ends_with('.') {
                found.push((
                    at,
                    if name == "notFound" {
                        "notFound()"
                    } else {
                        "redirect()"
                    },
                ));
            }
        }
    }
    for at in word_positions(code, "replace") {
        if code[at + "replace".len()..].starts_with('(')
            && code[..at].trim_end().ends_with("router.")
        {
            found.push((at, "router.replace()"));
        }
    }
    found.sort();
    found
}

/// Whether the JSX tag at `tag_at` of `code` is rendered only once `flag`
/// is set: an earlier `if (!flag) return`, a `flag ? ... : ...` or
/// `flag && ...` before it, or a key with the flag.
fn renders_after_ready(code: &str, tag_at: usize, loaded: &[String]) -> bool {
    if waits_for_state(code, tag_at, loaded) {
        return true;
    }
    let tag = tag_text(code, tag_at);
    if tag
        .find("key=")
        .is_some_and(|key| ready_word(&tag[key..]).is_some())
    {
        return true;
    }
    flag_gates(code, tag_at)
}

/// The end of the expression that starts at `from`: the `;` at depth 0 or
/// the bracket that closes an enclosing one (newlines do not end it).
fn expression_end(code: &str, from: usize) -> usize {
    let bytes = code.as_bytes();
    let mut depth = 0i32;
    let mut index = from;
    let mut quote: Option<u8> = None;
    while index < bytes.len() {
        let byte = bytes[index];
        if let Some(open) = quote {
            if byte == b'\\' {
                index += 2;
                continue;
            }
            if byte == open {
                quote = None;
            }
            index += 1;
            continue;
        }
        match byte {
            b'"' | b'\'' | b'`' => quote = Some(byte),
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' => {
                if depth == 0 {
                    return index;
                }
                depth -= 1;
            }
            b';' if depth == 0 => return index,
            _ => {}
        }
        index += 1;
    }
    code.len()
}

/// The starts (just after the `{`) of the `{` blocks that hold `at`,
/// innermost first, and 0 for the module.
fn enclosing_blocks(code: &str, at: usize) -> Vec<usize> {
    let text = blank_strings(code);
    let bytes = text.as_bytes();
    let mut found = Vec::new();
    let mut depth = 0i32;
    let mut index = at.min(bytes.len());
    while index > 0 {
        index -= 1;
        match bytes[index] {
            b'}' | b')' | b']' => depth += 1,
            b'(' | b'[' => depth = (depth - 1).max(0),
            b'{' => {
                if depth == 0 {
                    found.push(index + 1);
                } else {
                    depth -= 1;
                }
            }
            _ => {}
        }
    }
    found.push(0);
    found
}

/// Whether `at` of `code` runs only once a hydration flag says the stored
/// state was read: inside an `if` whose condition names the flag
/// (`if (ready && !task) notFound()`), after an `if (!ready) return ...` (or
/// `if (loading) return ...`), or inside a `ready ? ... : ...` / `ready && ...`
/// (`loading ? <Spinner/> : ...`) branch.
fn flag_gates(code: &str, at: usize) -> bool {
    if guard_of(code, at).is_some_and(|cond| ready_word(cond).is_some()) {
        return true;
    }
    // An early return counts only at the top level of a block that holds `at`
    // (the component's body, an effect's body), not inside another one.
    let blocks = enclosing_blocks(code, at);
    let early = word_positions(&code[..at], "if").into_iter().any(|start| {
        if !blocks
            .iter()
            .any(|&from| start >= from && top_level(&code[from..], start - from))
        {
            return false;
        }
        let open = code.len() - code[start + 2..].trim_start().len();
        if code.as_bytes().get(open) != Some(&b'(') {
            return false;
        }
        let Some(close) = matching(code, open) else {
            return false;
        };
        let cond = &code[open + 1..close];
        let then = code[close + 1..].trim_start();
        let then = then.strip_prefix('{').map_or(then, str::trim_start);
        close < at
            && then.starts_with("return")
            && READY_WORDS.iter().any(|word| {
                word_positions(cond, word).into_iter().any(|pos| {
                    let negated = cond[..pos].trim_end().ends_with('!');
                    negated != is_inverse_flag(word)
                })
            })
    });
    early
        || READY_WORDS.iter().any(|word| {
            word_positions(&code[..at], word).into_iter().any(|pos| {
                let after = code[pos + word.len()..].trim_start();
                let negated = code[..pos].trim_end().ends_with('!');
                let ternary = after.starts_with('?') && !after.starts_with("?.");
                let and = after.starts_with("&&") && negated == is_inverse_flag(word);
                (ternary || and) && expression_end(code, pos) >= at
            })
        })
}

/// Whether `code` returns early, before `at`, while a state that loads later
/// is still empty (`if (state === null) return <Loading/>`, `if (!data)
/// return ...`): the useState values of the state, set in an effect, and
/// `loaded`, the names read from a store whose first render is empty.
fn waits_for_state(code: &str, at: usize, loaded: &[String]) -> bool {
    let mut states: Vec<String> = declarations(code)
        .into_iter()
        .filter(|decl| decl.names.len() >= 2 && decl.value.contains("useState"))
        .map(|decl| decl.names[0].clone())
        .collect();
    states.extend(loaded.iter().cloned());
    word_positions(&code[..at], "if").into_iter().any(|start| {
        let open = code.len() - code[start + 2..].trim_start().len();
        if code.as_bytes().get(open) != Some(&b'(') {
            return false;
        }
        let Some(close) = matching(code, open) else {
            return false;
        };
        let cond = &code[open + 1..close];
        let then = code[close + 1..].trim_start();
        let then = then.strip_prefix('{').map_or(then, str::trim_start);
        then.starts_with("return")
            && close < at
            && states.iter().any(|state| tests_missing(cond, state))
    })
}

/// The `useState(...)` initial values of `body`: (offset in `body`, text).
fn state_inits(body: &str) -> Vec<(usize, &str)> {
    word_positions(body, "useState")
        .into_iter()
        .filter_map(|at| {
            let mut open = at + "useState".len();
            if body.as_bytes().get(open) == Some(&b'<') {
                open = skip_angles(body, open);
            }
            if body.as_bytes().get(open) != Some(&b'(') {
                return None;
            }
            let close = matching(body, open)?;
            Some((at, &body[open + 1..close]))
        })
        .collect()
}

/// Whether `text` reads a field of `name` (`task.title`, `task?.title`).
fn reads_field(text: &str, name: &str) -> bool {
    word_positions(text, name).into_iter().any(|at| {
        let after = &text[at + name.len()..];
        after.starts_with('.') || after.starts_with("?.")
    })
}

/// Whether one effect of `body` re-runs when a hydration flag changes (its
/// dependency list names `ready`): it copies the stored record once it was
/// read. An effect on the record itself is no fix (it discards the drafts on
/// every change of the record: `draft-resync`), nor one on its id (a seed
/// record keeps its id when the stored state is read).
fn resyncs_on_flag(body: &str) -> bool {
    effects(body).iter().any(|&(from, to)| {
        split_top(&body[from..to])
            .last()
            .is_some_and(|deps| deps.starts_with('[') && ready_word(deps).is_some())
    })
}

/// The parameter names of a component (`{ task, onSave }`).
fn prop_names(params: &str) -> Vec<String> {
    let params = params.trim();
    let Some(close) = params
        .starts_with('{')
        .then(|| matching(params, 0))
        .flatten()
    else {
        return Vec::new();
    };
    split_top(&params[1..close])
        .iter()
        .map(|part| {
            part.split([':', '='])
                .next()
                .unwrap_or("")
                .trim()
                .trim_start_matches("...")
                .to_string()
        })
        .filter(|name| !name.is_empty())
        .collect()
}

/// Whether the pages under `store` see only the stored state: the provider,
/// or a layout component (`{ready ? children : <Loading/>}`), renders them
/// only once it was read.
fn store_waits(app: &App, store: &Store) -> bool {
    store.gates
        || app.scripts().any(|file| {
            is_ui_path(file.path)
                && file.path != store.file.path
                && route_segments(file.path).is_none()
                && calls(&file.code, &store.hook)
                // `{ready ? children : <Loading/>}`, or `if (!ready) return
                // <Loading/>` before the children.
                && word_positions(&file.code, "children")
                    .into_iter()
                    .filter(|&at| renders_children(&file.code, at))
                    .any(|at| flag_gates(&file.code, at))
        })
}

/// "Make `lib/store.tsx` render a loading state instead of its children
/// until `ready`": the one fix of every page under a seeded store.
fn gate_fix(store: &Store) -> String {
    match store.flag {
        Some(flag) => format!(
            "make `{}` render a loading state instead of its children {} (every page and \
             effect under it then starts from the stored state)",
            store.file.path,
            until_flag(flag)
        ),
        None => format!(
            "give `{}` a `ready` flag (false until its effect read the stored state) and make it \
             render a loading state instead of its children until `ready` is true (every page and \
             effect under it then starts from the stored state)",
            store.file.path
        ),
    }
}

fn render_before_ready(app: &App) -> Vec<Lint> {
    let store = store(app);
    let waits = store.as_ref().is_some_and(|store| store_waits(app, store));
    // The names a page reads from a store whose first render is empty (`const
    // { state } = useStore()` with `useState(null)`): a page that returns
    // early while they are missing waits for the load.
    let loaded_names = |code: &str| -> Vec<String> {
        match &store {
            Some(store) if !store.seeded => declarations(code)
                .into_iter()
                .filter(|decl| calls(decl.value, &store.hook))
                .flat_map(|decl| decl.names)
                .collect(),
            _ => Vec::new(),
        }
    };
    if waits {
        return Vec::new();
    }
    let mut lints = Vec::new();
    // The files that need the store's missing flag.
    let mut leaving: BTreeSet<String> = BTreeSet::new();
    let mut seen_forms: BTreeSet<(String, String)> = BTreeSet::new();
    for file in app.scripts().filter(|file| is_ui_path(file.path)) {
        let code = &file.code;
        let reads = match &store {
            Some(store) => file.path != store.file.path && calls(code, &store.hook),
            None => effect_loads(code),
        };
        if !reads {
            continue;
        }
        let records = route_records(code);
        if records.is_empty() {
            continue;
        }
        let (source, flag) = match &store {
            Some(store) => (
                if store.seeded {
                    format!("`{}()` holds the seed data", store.hook)
                } else {
                    format!("`{}()` holds an empty state", store.hook)
                },
                store.flag.unwrap_or("ready"),
            ),
            None => (
                "this page's state holds the seed (or empty) data".to_string(),
                "ready",
            ),
        };
        let loaded = loaded_names(code);
        let wait = until_flag(flag);
        // A missing record that leaves the page before the flag is read.
        {
            let hit = leaves_page(code).into_iter().find_map(|(at, call)| {
                if waits_for_state(code, at, &loaded) || flag_gates(code, at) {
                    return None;
                }
                let cond = guard_of(code, at).map(str::to_string).or_else(|| {
                    // `find(...) ?? notFound()`.
                    let before = code[..at].trim_end();
                    (before.ends_with("??") || before.ends_with("||"))
                        .then(|| {
                            declarations(code)
                                .into_iter()
                                .find(|decl| {
                                    decl.value_at <= at && at < decl.value_at + decl.value.len()
                                })
                                .map(|decl| format!("!{}", decl.names[0]))
                        })
                        .flatten()
                })?;
                records
                    .iter()
                    .find(|(name, _)| tests_missing(&cond, name))
                    .map(|(name, _)| (at, call, name.clone()))
            });
            if let Some((at, call, record)) = hit {
                let fix = match &store {
                    Some(store) if store.flag.is_some() => format!(
                        "Best, {}; else read `{flag}` from `{}()` here and return a loading state \
                         {wait} (after all of the component's hooks); call {call} only after it \
                         (after a delete, navigate away first)",
                        gate_fix(store),
                        store.hook
                    ),
                    Some(store) => format!(
                        "`{}` has no such flag yet. Best, {}; else read it from `{}()` here and \
                         return a loading state until it is true (after all of the component's \
                         hooks); call {call} only after it (after a delete, navigate away first)",
                        store.file.path,
                        gate_fix(store),
                        store.hook
                    ),
                    None => format!(
                        "Keep a `ready` flag (a useState set to true right after the load in the \
                         effect) and return a loading state until it is true (after all of the \
                         component's hooks); call {call} only after it"
                    ),
                };
                lints.push(Lint {
                    level: Level::Error,
                    code: "TS9026",
                    name: "render-before-ready",
                    path: file.path.to_string(),
                    line: file.line_of(at),
                    message: format!(
                        "this file calls {call} when `{record}` is missing, but {source} until the \
                         stored state is read in a useEffect: a record the user created gives a 404 \
                         on a reload or a deep link. {fix}."
                    ),
                });
                leaving.insert(file.path.to_string());
            }
        }
        // A form that copies a record into its state once. With an empty (or
        // null) first render, the form mounts only once a record is there.
        if store.as_ref().is_some_and(|store| !store.seeded) {
            continue;
        }
        let funcs = functions(code);
        let mut forms: Vec<(&File, String, usize, String, String)> = Vec::new();
        let imported = imports(code);
        for (record, declared) in &records {
            // In the component that looks the record up, from the record itself.
            if let Some(func) = enclosing(&funcs, *declared)
                .filter(|func| func.name.starts_with(|ch: char| ch.is_ascii_uppercase()))
            {
                let body = func.body(code);
                if let Some((at, _)) = state_inits(body)
                    .into_iter()
                    .find(|(_, init)| reads_field(init, record) && !resyncs_on_flag(body))
                {
                    let keyed = format!(
                        "move the form into a component of its own, rendered with \
                         `key={{{record}.id + String({flag})}}`"
                    );
                    forms.push((
                        file,
                        func.name.clone(),
                        func.body.0 + at,
                        record.clone(),
                        match &store {
                            Some(store) => format!(
                                "{} (then move the form into a component of its own, rendered \
                                 with `key={{{record}.id}}`), or {keyed}",
                                gate_fix(store)
                            ),
                            None => keyed,
                        },
                    ));
                }
            }
            // In a component that this file renders with the record as a prop.
            for (tag_at, _) in code.match_indices('<') {
                let name_len = code.as_bytes()[tag_at + 1..]
                    .iter()
                    .take_while(|byte| is_ident(**byte))
                    .count();
                let name = &code[tag_at + 1..tag_at + 1 + name_len];
                if !name.starts_with(|ch: char| ch.is_ascii_uppercase()) {
                    continue;
                }
                let tag = tag_text(code, tag_at);
                let Some(prop) = split_props(tag).into_iter().find_map(|(prop, value)| {
                    let value = value.trim().trim_end_matches('!');
                    (value == record.as_str()).then_some(prop)
                }) else {
                    continue;
                };
                if renders_after_ready(code, tag_at, &loaded) {
                    continue;
                }
                // The component: local, else imported.
                let owner: Option<&File> = if funcs.iter().any(|func| func.name == name) {
                    Some(file)
                } else {
                    imported
                        .iter()
                        .find(|(names, _)| names.iter().any(|imported| imported == name))
                        .and_then(|(_, spec)| resolve(app, file.path, spec))
                };
                let Some(owner) = owner else {
                    continue;
                };
                let owner_funcs = functions(&owner.code);
                let Some(component) = owner_funcs.iter().find(|func| func.name == name) else {
                    continue;
                };
                if !prop_names(component.params(&owner.code)).contains(&prop) {
                    continue;
                }
                let body = component.body(&owner.code);
                if resyncs_on_flag(body) {
                    continue;
                }
                let Some((at, _)) = state_inits(body)
                    .into_iter()
                    .find(|(_, init)| reads_field(init, &prop))
                else {
                    continue;
                };
                let site = if owner.path == file.path {
                    "below".to_string()
                } else {
                    format!("in `{}`", file.path)
                };
                let keyed = format!(
                    "render it with `key={{{record}.id + String({flag})}}` ({site}: <{name} \
                     {prop}={{{record}}}>)"
                );
                forms.push((
                    owner,
                    name.to_string(),
                    component.body.0 + at,
                    prop.clone(),
                    match &store {
                        Some(store) => format!(
                            "{} (then `key={{{record}.id}}` is enough), or {keyed}; never \
                             re-set its fields in a useEffect on `{prop}`",
                            gate_fix(store)
                        ),
                        None => {
                            format!("{keyed}; never re-set its fields in a useEffect on `{prop}`")
                        }
                    },
                ));
            }
        }
        for (owner, name, at, field_of, fix) in forms {
            if !seen_forms.insert((owner.path.to_string(), name.clone())) {
                continue;
            }
            leaving.insert(owner.path.to_string());
            lints.push(Lint {
                level: Level::Error,
                code: "TS9026",
                name: "render-before-ready",
                path: owner.path.to_string(),
                line: owner.line_of(at),
                message: format!(
                    "<{name}> copies the fields of `{field_of}` into useState once, at its first \
                     render, but {source} until the stored state is read in a useEffect: after a \
                     reload the form shows the seed values, and Save writes them back over the \
                     user's edits. Fix: {fix}."
                ),
            });
        }
    }
    if let Some(store) = store
        .as_ref()
        .filter(|store| store.flag.is_none() && !leaving.is_empty())
    {
        let at = word_positions(&store.file.code, &store.provider)
            .first()
            .copied()
            .unwrap_or(0);
        let pages = leaving
            .iter()
            .map(|path| format!("`{path}`"))
            .collect::<Vec<_>>()
            .join(", ");
        lints.push(Lint {
            level: Level::Error,
            code: "TS9026",
            name: "render-before-ready",
            path: store.file.path.to_string(),
            line: store.file.line_of(at),
            message: format!(
                "the store's first render is {} (it reads the stored state in a useEffect) and \
                 its value has no flag that says the stored state was read, so {pages} cannot \
                 wait for it (a 404, or a form that keeps the seed values, for a record the user \
                 created or edited). Add `ready` (`const [ready, setReady] = useState(false)`, \
                 `setReady(true)` in that effect right after the load) to the provider's value \
                 and return it from `{}()`.",
                if store.seeded {
                    "the seed data"
                } else {
                    "an empty state"
                },
                store.hook
            ),
        });
    }
    lints
}

/// The props of a JSX opening tag (`<A b={c} d="e">`): (name, value text).
fn split_props(tag: &str) -> Vec<(String, String)> {
    let bytes = tag.as_bytes();
    let mut props = Vec::new();
    let mut index = 1;
    while index < bytes.len() && is_ident(bytes[index]) {
        index += 1;
    }
    while index < bytes.len() {
        if !is_ident(bytes[index]) {
            index += 1;
            continue;
        }
        let start = index;
        while index < bytes.len() && (is_ident(bytes[index]) || bytes[index] == b'-') {
            index += 1;
        }
        let name = &tag[start..index];
        if bytes.get(index) != Some(&b'=') {
            continue;
        }
        index += 1;
        match bytes.get(index) {
            Some(b'{') => {
                let Some(close) = matching(tag, index) else {
                    break;
                };
                props.push((name.to_string(), tag[index + 1..close].to_string()));
                index = close + 1;
            }
            Some(&quote @ (b'"' | b'\'')) => {
                let end = tag[index + 1..]
                    .find(char::from(quote))
                    .map_or(tag.len(), |end| index + 1 + end);
                props.push((name.to_string(), tag[index..end.min(tag.len())].to_string()));
                index = end + 1;
            }
            _ => {}
        }
    }
    props
}

// ---- qlint2, audit 5: ungated store, draft resync, empty format -----------
//
// From the fifth blind audit (5 race pairs, the default stack against the
// single stream): a store whose first render is the seed rendered its pages
// at once (seed numbers, then a flash to the stored data; a child effect
// picked a seed id and kept it); a form re-set its drafts in an effect on the
// record, so a status change on the same page discarded the unsaved edits; a
// date formatter threw on a due date the user had cleared.

/// Whether the dependency list `deps` holds `name` itself (`[task]`, `[task,
/// ready]`), not only a field or an expression of it (`[task.id]`, `[task ?
/// task.id : '']`).
fn names_bare(deps: &str, name: &str) -> bool {
    let inner = deps
        .trim()
        .strip_prefix('[')
        .and_then(|rest| rest.strip_suffix(']'))
        .unwrap_or("");
    split_top(inner)
        .iter()
        .any(|part| part.trim_end_matches('!').trim() == name)
}

/// The `useState` values of `body` with their setters.
fn state_pairs(body: &str) -> Vec<(String, String)> {
    declarations(body)
        .into_iter()
        .filter(|decl| decl.names.len() >= 2 && decl.value.contains("useState"))
        .map(|decl| (decl.names[0].clone(), decl.names[1].clone()))
        .collect()
}

/// Whether `body` binds `var` to a form control (`value={var}`,
/// `checked={var}`, `value={var ?? ''}`).
fn binds_control(body: &str, var: &str) -> bool {
    ["value=", "checked="].iter().any(|prop| {
        body.match_indices(prop).any(|(at, _)| {
            let open = at + prop.len();
            body.as_bytes().get(open) == Some(&b'{')
                && matching(body, open).is_some_and(|close| {
                    let inner = body[open + 1..close].trim();
                    inner.strip_prefix(var).is_some_and(|rest| {
                        !rest.starts_with(|ch: char| ch.is_ascii_alphanumeric() || ch == '_')
                            && !rest.trim_start().starts_with('.')
                    })
                })
        })
    })
}

/// The setters of `pairs` that `text` calls with a field of `record`
/// (`setTitle(task.title)`), as the names of their values.
fn copied_fields(text: &str, record: &str, pairs: &[(String, String)]) -> Vec<String> {
    pairs
        .iter()
        .filter(|(_, setter)| {
            word_positions(text, setter).into_iter().any(|at| {
                let open = at + setter.len();
                text.as_bytes().get(open) == Some(&b'(')
                    && matching(text, open)
                        .is_some_and(|close| reads_field(&text[open + 1..close], record))
            })
        })
        .map(|(var, _)| var.clone())
        .collect()
}

/// Whether an effect's callback copies only once: an `if` of it tests a
/// state or ref that the callback itself sets (`if (record && !init) { ...;
/// setInit(true) }`, `if (done.current) return; done.current = true`).
fn copies_once(callback: &str, pairs: &[(String, String)]) -> bool {
    let conds: Vec<&str> = word_positions(callback, "if")
        .into_iter()
        .filter_map(|at| {
            let open = callback.len() - callback[at + 2..].trim_start().len();
            (callback.as_bytes().get(open) == Some(&b'('))
                .then(|| matching(callback, open).map(|close| &callback[open + 1..close]))
                .flatten()
        })
        .collect();
    let by_state = pairs.iter().any(|(var, setter)| {
        calls(callback, setter) && conds.iter().any(|cond| names_var(cond, var))
    });
    let by_ref = callback.match_indices(".current").any(|(at, _)| {
        let len = callback[..at]
            .bytes()
            .rev()
            .take_while(|byte| is_ident(*byte))
            .count();
        let name = &callback[at - len..at];
        let after = callback[at + ".current".len()..].trim_start();
        len > 0
            && after.starts_with('=')
            && !after.starts_with("==")
            && conds
                .iter()
                .any(|cond| cond.contains(&format!("{name}.current")))
    });
    by_state || by_ref
}

/// The props of a component: its destructured parameters (`{ task, onSave }`)
/// or the names its body destructures from one props parameter (`props:
/// Props` with `const { task, onSave } = props`).
fn component_props(params: &str, body: &str) -> Vec<String> {
    let mut names = prop_names(params);
    let single = params.split(':').next().unwrap_or("").trim();
    if names.is_empty() && !single.is_empty() && single.bytes().all(is_ident) {
        for decl in declarations(body) {
            if decl.value.trim() == single {
                names.extend(decl.names.iter().cloned());
            }
        }
    }
    names
}

/// Whether `record` has an id the form could be keyed by: `code` reads
/// `record.id`, or `body` looks it up in a collection (`.find(`).
fn has_id(code: &str, body: &str, record: &str) -> bool {
    let reads_id = word_positions(code, record).into_iter().any(|at| {
        let after = &code[at + record.len()..];
        let rest = after
            .strip_prefix("?.id")
            .or_else(|| after.strip_prefix(".id"));
        rest.is_some_and(|rest| {
            !rest.starts_with(|ch: char| ch.is_ascii_alphanumeric() || ch == '_')
        })
    });
    reads_id
        || declarations(body).iter().any(|decl| {
            decl.names.iter().any(|name| name == record) && decl.value.contains(".find(")
        })
}

/// Whether a render site of `<component>` passes it, as `prop`, a record
/// with an id (`task={task}` where that file reads `task.id` or finds it in
/// a collection).
fn prop_has_id(app: &App, component: &str, prop: &str) -> bool {
    let tag = format!("<{component}");
    let attr = format!("{prop}={{");
    app.scripts()
        .filter(|file| is_ui_path(file.path))
        .any(|file| {
            let code = &file.code;
            code.match_indices(&tag).any(|(at, _)| {
                if code[at + tag.len()..].starts_with(|ch: char| ch.is_ascii_alphanumeric()) {
                    return false;
                }
                let text = tag_text(code, at);
                word_positions(text, prop).into_iter().any(|pos| {
                    if !text[pos..].starts_with(&attr) {
                        return false;
                    }
                    let open = pos + prop.len() + 1;
                    matching(text, open).is_some_and(|close| {
                        let value = text[open + 1..close].trim();
                        value.contains(".find(")
                            || (value.bytes().all(is_ident) && has_id(code, code, value))
                    })
                })
            })
        })
}

/// Whether `body` treats `record` as possibly missing (`record ? ... : ...`,
/// `record?.x`, `record && ...`, `if (!record)`).
fn nullable(body: &str, record: &str) -> bool {
    // Not a word of a string ("Delete this task?").
    let body = blank_strings(body);
    let body = body.as_str();
    tests_missing(body, record)
        || word_positions(body, record).into_iter().any(|at| {
            let after = body[at + record.len()..].trim_start();
            after.starts_with("?.")
                || after.starts_with("&&")
                || (after.starts_with('?') && !after.starts_with("??"))
        })
}

fn draft_resync(app: &App) -> Vec<Lint> {
    let store = store(app);
    let ungated = store
        .as_ref()
        .filter(|store| store.seeded && !store_waits(app, store));
    let mut lints = Vec::new();
    for file in app
        .scripts()
        .filter(|file| is_ui_path(file.path) && !file.is_test())
    {
        let code = &file.code;
        let funcs = functions(code);
        let mut seen: BTreeSet<usize> = BTreeSet::new();
        for func in funcs
            .iter()
            .filter(|func| func.name.starts_with(|ch: char| ch.is_ascii_uppercase()))
        {
            let body = func.body(code);
            let pairs = state_pairs(body);
            let drafts: Vec<(String, String)> = pairs
                .iter()
                .filter(|(var, _)| binds_control(body, var))
                .cloned()
                .collect();
            if drafts.is_empty() {
                continue;
            }
            for (from, to) in effects(body) {
                let args = split_top(&body[from..to]);
                if args.len() < 2 || !args[args.len() - 1].starts_with('[') {
                    continue;
                }
                let deps = args[args.len() - 1];
                let callback = args[0];
                if copies_once(callback, &pairs) {
                    continue;
                }
                let hit = identifiers_of(deps).into_iter().find_map(|record| {
                    // Only a record with an id (one the form could be keyed
                    // by): not a settings object or the whole state.
                    if !names_bare(deps, &record)
                        || READY_WORDS.contains(&record.as_str())
                        || pairs.iter().any(|(var, _)| *var == record)
                        || !(has_id(code, body, &record) || prop_has_id(app, &func.name, &record))
                    {
                        return None;
                    }
                    let copied = copied_fields(callback, &record, &drafts);
                    (!copied.is_empty()).then_some((record, copied))
                });
                let Some((record, copied)) = hit else {
                    continue;
                };
                if !seen.insert(func.body.0 + from) {
                    continue;
                }
                // Where to key it: the render site of a component that takes
                // the record as a prop, else a component of its own.
                let is_prop = component_props(func.params(code), body).contains(&record);
                // `editing: Transaction | null`: no id to key by while empty
                // (a record of the component itself is keyed after its
                // not-found check).
                let id = if is_prop && nullable(body, &record) {
                    format!("{record}?.id ?? 'new'")
                } else {
                    format!("{record}.id")
                };
                let site = is_prop
                    .then(|| {
                        // This file first, then the files that import it
                        // from here (another file may declare its own
                        // component of the same name).
                        let own = std::iter::once(file);
                        let importers = app.scripts().filter(|other| {
                            is_ui_path(other.path)
                                && other.path != file.path
                                && imports(&other.code).into_iter().any(|(names, spec)| {
                                    names.contains(&func.name)
                                        && resolve(app, other.path, &spec)
                                            .is_some_and(|target| target.path == file.path)
                                })
                        });
                        own.chain(importers)
                            .find(|other| {
                                other.code.match_indices(&format!("<{}", func.name)).any(
                                    |(at, _)| {
                                        !other.code[at + 1 + func.name.len()..]
                                            .starts_with(|ch: char| ch.is_ascii_alphanumeric())
                                    },
                                )
                            })
                            .map(|other| other.path)
                    })
                    .flatten();
                let key = match (is_prop, site) {
                    (true, Some(path)) if path == file.path => format!(
                        "render <{}> with `key={{{id}}}` (below), so its useState initial values \
                         take the fields of `{record}`",
                        func.name
                    ),
                    (true, Some(path)) => format!(
                        "render <{}> with `key={{{id}}}` (in `{path}`), so its useState initial \
                         values take the fields of `{record}`",
                        func.name
                    ),
                    (true, None) => format!(
                        "render <{}> with `key={{{id}}}`, so its useState initial values take the \
                         fields of `{record}`",
                        func.name
                    ),
                    (false, _) => format!(
                        "move the form into a component of its own that takes `{record}` as a \
                         prop, initialises its useState from it, and is rendered here with \
                         `key={{{id}}}`"
                    ),
                };
                // An effect on the id alone keeps a seed record's values when
                // the stored state replaces the seed (the id stays the same).
                let gate = match ungated {
                    Some(store) => format!(
                        " An effect on `{record}.id` alone is no fix while the store renders its \
                         seed first (a seed record keeps its id when the stored state replaces \
                         it): also {}.",
                        gate_fix(store)
                    ),
                    None => format!(" An effect on `{record}.id` alone is fine."),
                };
                let names = copied
                    .iter()
                    .map(|name| format!("`{name}`"))
                    .collect::<Vec<_>>()
                    .join(", ");
                lints.push(Lint {
                    level: Level::Error,
                    code: "TS9030",
                    name: "draft-resync",
                    path: file.path.to_string(),
                    line: file.line_of(func.body.0 + from),
                    message: format!(
                        "<{}> keeps the fields of `{record}` in form drafts ({names}) and re-sets \
                         them in a useEffect that depends on `{record}` itself: any change to the \
                         record (a status select, a label toggle, a save elsewhere on the page) \
                         gives a new `{record}` object, and the effect discards the user's unsaved \
                         edits. Remove that effect and key the form by the record id instead: \
                         {key}.{gate}",
                        func.name
                    ),
                });
            }
        }
    }
    lints
}

/// The names that `code` binds from `hook()` and the values derived from
/// them (`const { state } = useStore()`, `const projects = state.projects`).
fn hook_names(code: &str, hook: &str) -> BTreeSet<String> {
    let decls = declarations(code);
    let mut names: BTreeSet<String> = decls
        .iter()
        .filter(|decl| calls(decl.value, hook))
        .flat_map(|decl| decl.names.iter().cloned())
        .filter(|name| !name.starts_with("set"))
        .collect();
    for _ in 0..2 {
        for decl in &decls {
            if !is_function_expr(decl.value)
                && !decl.value.contains("useState")
                && names.iter().any(|name| names_var(decl.value, name))
            {
                names.extend(decl.names.iter().cloned());
            }
        }
    }
    names.retain(|name| !READY_WORDS.contains(&name.as_str()));
    names
}

/// The first effect of `code` that copies the store's data into a local
/// useState before the stored state was read: it reads a name bound from
/// `hook()`, calls a local setter, and neither it nor its dependencies name
/// a hydration flag. Its offset.
fn seed_effect(code: &str, hook: &str) -> Option<usize> {
    let names = hook_names(code, hook);
    if names.is_empty() {
        return None;
    }
    let pairs = state_pairs(code);
    effects(code).into_iter().find_map(|(from, to)| {
        let text = &code[from..to];
        let args = split_top(text);
        let callback = args.first()?;
        // It keeps what it copied: it copies once, or it does not re-run
        // when the stored state replaces the seed.
        let deps = args
            .last()
            .filter(|deps| args.len() >= 2 && deps.starts_with('['));
        let keeps = copies_once(callback, &pairs)
            || deps.is_some_and(|deps| !names.iter().any(|name| names_var(deps, name)));
        (ready_word(text).is_none()
            && keeps
            && names.iter().any(|name| names_var(callback, name))
            && pairs.iter().any(|(_, setter)| calls(callback, setter))
            && !fills_empty_only(callback, &pairs))
        .then_some(from)
    })
}

/// Whether every local setter call of an effect's callback only fills an
/// empty value (`if (author === '' && members.length > 0)
/// setAuthor(members[0].id)`): a default pick, not a copy of the state.
fn fills_empty_only(callback: &str, pairs: &[(String, String)]) -> bool {
    let mut any = false;
    for (var, setter) in pairs {
        for at in word_positions(callback, setter) {
            if !callback[at + setter.len()..].starts_with('(') {
                continue;
            }
            any = true;
            let empty = guard_of(callback, at).is_some_and(|cond| {
                tests_missing(cond, var)
                    || ["=== ''", "=== \"\"", "== ''", "== \"\"", ".length === 0"]
                        .iter()
                        .any(|test| {
                            cond.contains(&format!("{var} {test}"))
                                || cond.contains(&format!("{var}{test}"))
                        })
            });
            if !empty {
                return false;
            }
        }
    }
    any
}

/// `semantics`: the app was planned with `qsemantic`, whose conventions say
/// that the provider renders its children only once `ready`. Without them a
/// seeded provider that renders at once is a common design (the pages wait
/// for `ready` themselves), so the lint is silent.
fn ungated_store(app: &App, semantics: bool) -> Vec<Lint> {
    if !semantics {
        return Vec::new();
    }
    let Some(store) = store(app) else {
        return Vec::new();
    };
    if !store.seeded || store_waits(app, &store) {
        return Vec::new();
    }
    // What the seed first render breaks: a page that looks a record up by the
    // route id, an effect that copies the state into local state.
    let mut found: Vec<String> = Vec::new();
    for file in app
        .scripts()
        .filter(|file| is_ui_path(file.path) && file.path != store.file.path && !file.is_test())
    {
        let code = &file.code;
        if !calls(code, &store.hook) {
            continue;
        }
        if let Some(at) = seed_effect(code, &store.hook) {
            found.push(format!(
                "the effect at `{}:{}` copies the seed into local state and keeps it",
                file.path,
                file.line_of(at)
            ));
        } else if !route_records(code).is_empty() {
            found.push(format!(
                "`{}` looks a record up by a route or query parameter in the seed",
                file.path
            ));
        }
    }
    if found.is_empty() {
        return Vec::new();
    }
    let shown = found.len().min(3);
    let more = if found.len() > shown {
        format!(" (and {} more)", found.len() - shown)
    } else {
        String::new()
    };
    let at = word_positions(&store.file.code, &store.provider)
        .first()
        .copied()
        .unwrap_or(0);
    let flag = match store.flag {
        Some(flag) => format!("`{flag}`, which `{}()` already returns", store.hook),
        None => format!(
            "a `ready` flag (`const [ready, setReady] = useState(false)`, `setReady(true)` in that \
             effect right after the load; return it from `{}()` too)",
            store.hook
        ),
    };
    vec![Lint {
        level: Level::Error,
        code: "TS9031",
        name: "ungated-store",
        path: store.file.path.to_string(),
        line: store.file.line_of(at),
        message: format!(
            "<{}> starts from the seed data (it reads the stored state in a useEffect) and renders \
             its children at once, so every page first shows the seed and then flashes to the \
             stored data, and code under it acts on the seed: {}{more}. Render a loading state \
             instead of the children until the stored state was read, using {flag}: `if (!ready) \
             return <p>Loading...</p>;` after all of the provider's hooks (or `{{ready ? children : \
             <p>Loading...</p>}}`). Pages and effects under it then see only the stored data.",
            store.provider,
            found[..shown].join("; ")
        ),
    }]
}

/// Whether `body`, the body of a function whose first parameter is `param`,
/// returns early for an empty value (`if (!value) return ''`, `if (value ===
/// '') return`), or catches what it throws.
fn handles_empty(body: &str, param: &str) -> bool {
    if !word_positions(body, "catch").is_empty() {
        return true;
    }
    word_positions(body, "if").into_iter().any(|at| {
        let open = body.len() - body[at + 2..].trim_start().len();
        if body.as_bytes().get(open) != Some(&b'(') {
            return false;
        }
        let Some(close) = matching(body, open) else {
            return false;
        };
        let cond = &body[open + 1..close];
        let then = body[close + 1..].trim_start();
        let then = then.strip_prefix('{').map_or(then, str::trim_start);
        let empty = tests_missing(cond, param)
            || negates_param(cond, param)
            || [
                "=== ''", "=== \"\"", "== ''", "== \"\"", ".trim()", ".length",
            ]
            .iter()
            .any(|test| cond.contains(&format!("{param}{test}")));
        empty && then.starts_with("return")
    })
}

/// Whether `cond` negates an expression of `param`: `!value?.trim()`,
/// `!DATE_RE.test(value)`, `!isValid(value)` (an empty value takes that
/// early return too).
fn negates_param(cond: &str, param: &str) -> bool {
    word_positions(cond, param).into_iter().any(|at| {
        let chain = cond[..at]
            .trim_end_matches(|ch: char| ch.is_ascii_alphanumeric() || "_.?$(".contains(ch))
            .trim_end();
        chain.ends_with('!') && !chain.ends_with("!!")
    })
}

/// Whether `func` of `module` throws on an empty value: its body throws, or
/// it calls a function of the module that throws without first testing
/// its parameter (`value ? parseDate(value) : null` does not).
fn throws_on_empty(module: &File, func: &Func, funcs: &[Func], param: &str) -> bool {
    let body = func.body(&module.code);
    !word_positions(body, "throw").is_empty()
        || funcs.iter().any(|other| {
            other.name != func.name
                && !word_positions(other.body(&module.code), "throw").is_empty()
                && word_positions(body, &other.name).into_iter().any(|at| {
                    body[at + other.name.len()..].starts_with('(')
                        && !body[..at].ends_with('.')
                        && !guarded_call(body, at, param, false)
                })
        })
}

/// The fields of the app's records that may be empty for `formatter`:
/// optional in a type (`dueDate?: string`), or guarded where another call of
/// the formatter shows them (`task.dueDate ? formatDate(task.dueDate) :
/// '-'`).
fn optional_fields(app: &App, formatter: &str) -> BTreeMap<String, String> {
    let mut found: BTreeMap<String, String> = BTreeMap::new();
    for file in app.scripts().filter(|file| !file.is_test()) {
        let code = &file.code;
        if is_ui_path(file.path) {
            for at in word_positions(code, formatter) {
                let open = at + formatter.len();
                if code.as_bytes().get(open) != Some(&b'(') {
                    continue;
                }
                let Some(close) = matching(code, open) else {
                    continue;
                };
                let Some(field) = split_top(&code[open + 1..close])
                    .first()
                    .and_then(|arg| arg.rsplit_once('.'))
                    .map(|(_, field)| field.to_string())
                else {
                    continue;
                };
                if !field.is_empty()
                    && field.bytes().all(is_ident)
                    && guarded_call(code, at, &field, true)
                {
                    found.entry(field.clone()).or_insert_with(|| {
                        format!(
                            "`{}:{}` shows it only when it is set",
                            file.path,
                            file.line_of(at)
                        )
                    });
                }
            }
            continue;
        }
        // The record types: `lib/types.ts`, `types/task.ts`.
        if !file_name(file.path).starts_with("types") && !file.path.contains("types/") {
            continue;
        }
        for (at, _) in code.match_indices("?:") {
            let len = code[..at]
                .bytes()
                .rev()
                .take_while(|byte| is_ident(*byte))
                .count();
            if len == 0 || at < len {
                continue;
            }
            let name = &code[at - len..at];
            let before = code[..at - len].trim_end();
            // A type member (`{ a?: x; b?: y }`), not a ternary (`a ? b : c`).
            if before.ends_with([';', '{', ',']) || before.ends_with('\n') || before.is_empty() {
                found
                    .entry(name.to_string())
                    .or_insert_with(|| format!("`{name}?:` in `{}`", file.path));
            }
        }
    }
    found
}

/// Whether the call at `at` of `code`, on `field`, runs only when the field
/// is set: inside `x.field ? ... : ...`, `x.field && ...` or an `if` that
/// tests it, before it in the same expression.
/// `strict`: only a test of the field itself (`x.field ?`, `x.field &&`,
/// `if (x.field)`, `if (!x.field) return`) counts, not a comparison or a
/// filter.
fn guarded_call(code: &str, at: usize, field: &str, strict: bool) -> bool {
    // `x.field` tested for itself: not compared (`total !== x.field`).
    let truthy = |text: &str, pos: usize| {
        let after = text[pos + field.len()..].trim_start();
        let chain = text[..pos]
            .trim_end_matches(|ch: char| ch.is_ascii_alphanumeric() || "_.?$".contains(ch))
            .trim_end();
        let before_ok = chain.is_empty()
            || chain.ends_with(['!', '(', '{'])
            || chain.ends_with("&&")
            || chain.ends_with("||");
        let after_ok = after.is_empty()
            || after.starts_with("&&")
            || after.starts_with("||")
            || after.starts_with(')')
            || (after.starts_with('?') && !after.starts_with("?.") && !after.starts_with("??"));
        before_ok && after_ok
    };
    if guard_of(code, at).is_some_and(|cond| {
        word_positions(cond, field)
            .into_iter()
            .any(|pos| !strict || truthy(cond, pos))
    }) {
        return true;
    }
    // `x.field ? fmt(x.field) : '-'`, `x.field && fmt(x.field)`: a test whose
    // expression holds the call.
    word_positions(&code[..at], field).into_iter().any(|pos| {
        let after = code[pos + field.len()..].trim_start();
        let test = (after.starts_with('?') && !after.starts_with("?.") && !after.starts_with("??"))
            || after.starts_with("&&");
        test && (!strict || truthy(code, pos)) && expression_end(code, pos) >= at
    })
}

fn empty_format(app: &App) -> Vec<Lint> {
    let mut lints = Vec::new();
    for module in app
        .scripts()
        .filter(|file| is_lib_path(file.path) && !file.is_test())
    {
        let funcs = functions(&module.code);
        for func in funcs
            .iter()
            .filter(|func| func.exported && func.name.to_lowercase().starts_with("format"))
        {
            let Some(param) = func.param_names(&module.code).into_iter().next() else {
                continue;
            };
            if param.is_empty()
                || !throws_on_empty(module, func, &funcs, &param)
                || handles_empty(func.body(&module.code), &param)
            {
                continue;
            }
            let optional = optional_fields(app, &func.name);
            if optional.is_empty() {
                continue;
            }
            // Its unguarded calls in render on a field that may be empty.
            let mut calls_found: Vec<(String, usize, String)> = Vec::new();
            for file in app
                .scripts()
                .filter(|file| is_ui_path(file.path) && !file.is_test())
            {
                let code = &file.code;
                let imported = imports(code).into_iter().any(|(names, spec)| {
                    names.contains(&func.name)
                        && resolve(app, file.path, &spec)
                            .is_some_and(|target| target.path == module.path)
                });
                if !imported {
                    continue;
                }
                for at in word_positions(code, &func.name) {
                    let open = at + func.name.len();
                    if code.as_bytes().get(open) != Some(&b'(') || code[..at].ends_with('.') {
                        continue;
                    }
                    let Some(close) = matching(code, open) else {
                        continue;
                    };
                    let Some(arg) = split_top(&code[open + 1..close]).first().copied() else {
                        continue;
                    };
                    // `task.dueDate`, `t?.dueDate`.
                    let Some(dot) = arg.rfind('.') else {
                        continue;
                    };
                    let field = &arg[dot + 1..];
                    if field.is_empty()
                        || !field.bytes().all(is_ident)
                        || !arg[..dot]
                            .trim_end_matches('?')
                            .bytes()
                            .all(|byte| is_ident(byte) || byte == b'.' || byte == b'?')
                        || !optional.contains_key(field)
                        || guarded_call(code, at, field, false)
                    {
                        continue;
                    }
                    calls_found.push((file.path.to_string(), file.line_of(at), arg.to_string()));
                }
            }
            let Some((path, line, arg)) = calls_found.first().cloned() else {
                continue;
            };
            let field = arg.rsplit('.').next().unwrap_or("").to_string();
            let more = if calls_found.len() > 1 {
                format!(" ({} more such calls)", calls_found.len() - 1)
            } else {
                String::new()
            };
            lints.push(Lint {
                level: Level::Error,
                code: "TS9032",
                name: "empty-format",
                path: module.path.to_string(),
                line: module.line_of(func.at),
                message: format!(
                    "`{}` throws on an empty value, but `{path}:{line}` calls it while rendering \
                     with `{arg}`{more}, a field that may be empty ({}): the page crashes once the \
                     user clears it. Make `{}` return '' for an empty value (for example `if \
                     (!{param}) return '';` as its first line) and throw only on a malformed \
                     non-empty one.",
                    func.name, optional[&field], func.name
                ),
            });
        }
    }
    lints
}

// ---- throw-in-updater ------------------------------------------------------

/// Whether `func` of `module` throws: its body, or the body of a function of
/// the same module that it calls, has a `throw`.
fn throws(module: &File, func: &Func, funcs: &[Func]) -> bool {
    let body = func.body(&module.code);
    !word_positions(body, "throw").is_empty()
        || funcs.iter().any(|other| {
            other.name != func.name
                && calls(body, &other.name)
                && !word_positions(other.body(&module.code), "throw").is_empty()
        })
}

fn throw_in_updater(app: &App) -> Vec<Lint> {
    let store = store(app);
    let mut lints = Vec::new();
    for file in app.scripts().filter(|file| is_ui_path(file.path)) {
        let code = &file.code;
        let mut setters = state_setters(code);
        if let Some(store) = store.as_ref().filter(|store| calls(code, &store.hook)) {
            setters.extend(
                store
                    .updaters
                    .iter()
                    .filter(|name| !word_positions(code, name).is_empty())
                    .cloned(),
            );
        }
        if setters.is_empty() {
            continue;
        }
        // The lib functions this file imports that throw.
        let mut throwing: Vec<(String, String)> = Vec::new();
        for (names, spec) in imports(code) {
            let Some(module) = resolve(app, file.path, &spec) else {
                continue;
            };
            if !is_lib_path(module.path) {
                continue;
            }
            let funcs = functions(&module.code);
            for name in names {
                if funcs
                    .iter()
                    .any(|func| func.name == name && func.exported && throws(module, func, &funcs))
                {
                    throwing.push((name, module.path.to_string()));
                }
            }
        }
        if throwing.is_empty() {
            continue;
        }
        let mut hit = None;
        'tries: for try_at in word_positions(code, "try") {
            let open = code.len() - code[try_at + 3..].trim_start().len();
            if code.as_bytes().get(open) != Some(&b'{') {
                continue;
            }
            let Some(close) = matching(code, open) else {
                continue;
            };
            let block = &code[open..close];
            for setter in &setters {
                for at in word_positions(block, setter) {
                    let call = at + setter.len();
                    if block.as_bytes().get(call) != Some(&b'(') {
                        continue;
                    }
                    let Some(end) = matching(block, call) else {
                        continue;
                    };
                    let Some(arg) = split_top(&block[call + 1..end]).first().copied() else {
                        continue;
                    };
                    if !is_function_expr(arg) {
                        continue;
                    }
                    if let Some((name, path)) = throwing.iter().find(|(name, _)| calls(arg, name)) {
                        hit = Some((open + at, setter.clone(), name.clone(), path.clone()));
                        break 'tries;
                    }
                }
            }
        }
        let Some((at, setter, name, path)) = hit else {
            continue;
        };
        lints.push(Lint {
            level: Level::Error,
            code: "TS9027",
            name: "throw-in-updater",
            path: file.path.to_string(),
            line: file.line_of(at),
            message: format!(
                "`{name}` (`{path}`) throws on invalid input, but this file calls it inside the \
                 updater passed to `{setter}`, in a try block: React runs the updater later, while \
                 it renders, so the catch never sees the error and the page crashes. Validate \
                 before setState: call `{name}` first, outside `{setter}` (in the try: `const next \
                 = {name}(...)` from the current state), catch its error there, then call \
                 `{setter}` with its result."
            ),
        });
    }
    lints
}

// ---- search-link -----------------------------------------------------------

fn search_link(app: &App) -> Vec<Lint> {
    let mut lints = Vec::new();
    for module in app.scripts() {
        let code = &module.code;
        // An object literal for a comment hit whose id is the comment's.
        let mut hit = None;
        for key in ["kind", "type"] {
            for at in word_positions(code, key) {
                let Some(value) = key_value(code, at, key.len()) else {
                    continue;
                };
                if unquote(value).as_deref() != Some("comment") {
                    continue;
                }
                let Some(open) = enclosing_object(code, at) else {
                    continue;
                };
                let Some(close) = matching(code, open) else {
                    continue;
                };
                let object = &code[open..=close];
                let Some(id_at) = word_positions(object, "id")
                    .into_iter()
                    .find(|&id_at| is_key_position(object, id_at, 2))
                else {
                    continue;
                };
                let id = key_value(object, id_at, 2).unwrap_or("");
                let Some(owner) = id.strip_suffix(".id") else {
                    continue;
                };
                let carries_task = !word_positions(object, "taskId").is_empty();
                hit = Some((owner.to_string(), carries_task, module.line_of(open)));
                break;
            }
            if hit.is_some() {
                break;
            }
        }
        let Some((owner, carries_task, line)) = hit else {
            continue;
        };
        // The UI files that show these hits: this one, or those that import it.
        for ui in app.scripts().filter(|ui| {
            is_ui_path(ui.path)
                && (ui.path == module.path
                    || imports(&ui.code).iter().any(|(_, spec)| {
                        resolve(app, ui.path, spec).is_some_and(|file| file.path == module.path)
                    }))
        }) {
            let ui_code = &ui.code;
            // A link by a task id (`${hit.taskId}`, `${comment.taskId}`).
            let by_task = ui_code.match_indices("taskId}").any(|(at, _)| {
                let before = &ui_code[boundary(ui_code, at.saturating_sub(40))..at];
                before
                    .rfind("${")
                    .is_some_and(|slot| !before[slot..].contains('}'))
            });
            if by_task {
                continue;
            }
            // Links `/<route>/${x.id}` that a comment hit reaches.
            let mut links = Vec::new();
            for (at, _) in ui_code.match_indices("`/") {
                let end = ui_code[at + 1..]
                    .find('`')
                    .map_or(ui_code.len(), |end| at + 1 + end);
                let text = &ui_code[at + 1..end];
                if text.contains("comment") {
                    continue;
                }
                let Some(slot) = text.find("${") else {
                    continue;
                };
                let inner = &text[slot + 2..];
                let Some(close) = inner.find('}') else {
                    continue;
                };
                let expr = inner[..close].trim();
                if expr.ends_with(".id")
                    && expr[..expr.len() - 3].bytes().all(is_ident)
                    && link_is_for_comments(ui_code, at)
                {
                    links.push((at, text[..slot].to_string()));
                }
            }
            let Some((at, route)) = links.first() else {
                continue;
            };
            let fix = if carries_task {
                format!(
                    "link a comment hit to `{route}${{hit.taskId}}` (the hit carries its task id)"
                )
            } else {
                format!(
                    "give the comment hit its task id in `{}` (line {line}: `taskId: {owner}.taskId`) \
                     and link it to `{route}${{hit.taskId}}`",
                    module.path
                )
            };
            lints.push(Lint {
                level: Level::Error,
                code: "TS9028",
                name: "search-link",
                path: ui.path.to_string(),
                line: ui.line_of(*at),
                message: format!(
                    "a comment search hit has the comment's id (`id: {owner}.id`, `{}`), but this \
                     file links hits to `{route}${{...id}}`: a comment result opens a page for a \
                     record that does not exist. Search results link to the record that owns them: \
                     {fix}.",
                    module.path
                ),
            });
        }
    }
    lints
}

/// The kind tests of `text` (`hit.kind === "task"`, `case 'task':`): (offset
/// of the end of the test, the kind).
fn kind_tests(text: &str) -> Vec<(usize, String)> {
    let mut found = Vec::new();
    for key in ["kind", "type"] {
        for at in word_positions(text, key) {
            if !text[..at].ends_with('.') {
                continue;
            }
            let rest = &text[at + key.len()..];
            let trimmed = rest.trim_start();
            let Some(value) = ["===", "=="]
                .iter()
                .find_map(|op| trimmed.strip_prefix(op))
                .map(str::trim_start)
            else {
                continue;
            };
            let Some(quote) = value.chars().next().filter(|ch| matches!(ch, '\'' | '"')) else {
                continue;
            };
            let Some(end) = value[1..].find(quote) else {
                continue;
            };
            let consumed = text.len() - value.len() + end + 2;
            found.push((consumed, value[1..end + 1].to_string()));
        }
    }
    for at in word_positions(text, "case") {
        let value = text[at + 4..].trim_start();
        let Some(quote) = value.chars().next().filter(|ch| matches!(ch, '\'' | '"')) else {
            continue;
        };
        if let Some(end) = value[1..].find(quote) {
            found.push((
                text.len() - value.len() + end + 2,
                value[1..end + 1].to_string(),
            ));
        }
    }
    found.sort();
    found
}

/// Whether the link at `at` of `code` is the one a comment hit gets: in a
/// tag keyed for comments, in a `.map` over comment hits, under a test for
/// the kind 'comment', or in the branch after the tests of the other kinds
/// that leaves 'comment' untested.
fn link_is_for_comments(code: &str, at: usize) -> bool {
    // The tag: `<Link key={`comment-${hit.id}`} href={`/tasks/${hit.id}`}>`.
    if let Some(open) = code[..at].rfind('<') {
        if code[open..at].to_lowercase().contains("comment") && !code[open..at].contains('>') {
            return true;
        }
    }
    // The list: `commentHits.map((hit) => ...)`.
    if let Some(map) = code[..at].rfind(".map(") {
        if matching(code, map + 4).is_some_and(|close| close > at) {
            let before = &code[..map];
            let len = before
                .bytes()
                .rev()
                .take_while(|byte| is_ident(*byte) || *byte == b'.')
                .count();
            let receiver = before[before.len() - len..].to_lowercase();
            if receiver.contains("comment") {
                return true;
            }
            if receiver.contains("task") || receiver.contains("project") {
                return false;
            }
        }
    }
    if let Some(cond) = guard_of(code, at) {
        if let Some((_, kind)) = kind_tests(cond).last() {
            return kind == "comment";
        }
    }
    let from = boundary(code, at.saturating_sub(500));
    let window = &code[from..at];
    let tests = kind_tests(window);
    let Some((end, kind)) = tests.last() else {
        return false;
    };
    if tests.iter().any(|(_, kind)| kind == "comment") && kind != "comment" {
        return false;
    }
    let between = window[*end..].trim_start();
    if let Some(rest) = between.strip_prefix('?') {
        // In the consequent of the last test, or after a `:` at its depth.
        let mut depth = 0i32;
        let mut alternate = false;
        for byte in blank_strings(rest).bytes() {
            match byte {
                b'(' | b'[' | b'{' => depth += 1,
                b')' | b']' | b'}' => depth -= 1,
                b':' if depth == 0 => alternate = true,
                _ => {}
            }
        }
        return if alternate {
            kind != "comment"
        } else {
            kind == "comment"
        };
    }
    // After `if (hit.kind === 'x') return ...;`: the rest of the kinds.
    between.starts_with(')') && kind != "comment"
}

// ---- duplicate-catalog -----------------------------------------------------

/// The exported arrays of `{ id: '...' }` objects of `file`: (name, offset,
/// ids).
fn id_catalogs(file: &File) -> Vec<(String, usize, BTreeSet<String>)> {
    let code = &file.code;
    let mut found = Vec::new();
    for decl in declarations(code) {
        if decl.names.len() != 1 || !code[..decl.at].trim_end().ends_with("const") {
            continue;
        }
        let keyword = code[..decl.at].trim_end();
        if !keyword[..keyword.len() - 5].trim_end().ends_with("export") {
            continue;
        }
        let value = decl.value.trim();
        if !value.starts_with('[') {
            continue;
        }
        let Some(close) = matching(value, 0) else {
            continue;
        };
        let mut ids = BTreeSet::new();
        for element in split_top(&value[1..close]) {
            if !element.starts_with('{') {
                continue;
            }
            if let Some(id) = word_positions(element, "id")
                .into_iter()
                .find(|&at| {
                    is_key_position(element, at, 2) && enclosing_object(element, at) == Some(0)
                })
                .and_then(|at| key_value(element, at, 2))
                .and_then(unquote)
            {
                ids.insert(id);
            }
        }
        if ids.len() >= 2 {
            found.push((decl.names[0].clone(), decl.at, ids));
        }
    }
    found
}

fn duplicate_catalog(app: &App) -> Vec<Lint> {
    let is_types = |path: &str| {
        let name = file_name(path);
        is_lib_path(path) && (name.starts_with("types.") || name == "types")
    };
    let catalogs: Vec<(&File, String, usize, BTreeSet<String>)> = app
        .scripts()
        .flat_map(|file| {
            id_catalogs(file)
                .into_iter()
                .map(move |(name, at, ids)| (file, name, at, ids))
        })
        .collect();
    let mut lints = Vec::new();
    for (types, name, _, ids) in catalogs.iter().filter(|(file, ..)| is_types(file.path)) {
        for (other, other_name, at, other_ids) in &catalogs {
            if is_types(other.path)
                || other_name.to_lowercase() != name.to_lowercase()
                || other_ids == ids
            {
                continue;
            }
            let list = |ids: &BTreeSet<String>| {
                ids.iter()
                    .take(4)
                    .map(|id| format!("'{id}'"))
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            // Which catalog's own ids the other files (the seed, the pages) use.
            let used = |own: &BTreeSet<String>, beside: &BTreeSet<String>| {
                own.difference(beside)
                    .filter(|id| {
                        app.scripts().any(|file| {
                            file.path != types.path
                                && file.path != other.path
                                && ['\'', '"', '`']
                                    .iter()
                                    .any(|quote| file.code.contains(&format!("{quote}{id}{quote}")))
                        })
                    })
                    .count()
            };
            let fix = if used(other_ids, ids) > used(ids, other_ids) {
                format!(
                    "The other files use this copy's ids: give `{name}` of `{}` the ids the \
                     records need (or change the seed to its ids), then delete this copy and \
                     import `{name}` from `{}`.",
                    types.path, types.path
                )
            } else {
                format!(
                    "Delete this copy and import `{name}` from `{}` (the seed and the pages then \
                     share its ids).",
                    types.path
                )
            };
            lints.push(Lint {
                level: Level::Error,
                code: "TS9029",
                name: "duplicate-catalog",
                path: other.path.to_string(),
                line: other.line_of(*at),
                message: format!(
                    "this file declares its own `{other_name}` (ids {}) beside `{name}` of `{}` (ids \
                     {}): records that hold one catalog's ids show nothing, or the wrong entry, in \
                     every file that reads the other. One catalog per fixed list. {fix}",
                    list(other_ids),
                    types.path,
                    list(ids),
                ),
            });
        }
    }
    lints
}

#[cfg(test)]
mod tests {
    fn activity_lints(extra: &[(&str, &str)]) -> Vec<Lint> {
        let mut files: Vec<(String, String)> = vec![
            (
                "lib/types.ts".into(),
                "export interface Activity { id: string; text: string }
export interface AppState {
  tasks: string[];
  activity: Activity[];
}
".into(),
            ),
            (
                "lib/seed.ts".into(),
                "export function seedState() { return { tasks: [], activity: [{ id: 'a1', text: 'x' }] }; }
".into(),
            ),
        ];
        for (path, code) in extra {
            files.push((path.to_string(), code.to_string()));
        }
        lint_app_with(&files, false, true, false)
            .into_iter()
            .filter(|lint| lint.name == "activity-unwritten")
            .collect()
    }

    #[test]
    fn activity_unwritten_flags_a_log_that_only_the_seed_fills() {
        let page = "const { activity } = state;
export default function Page() { return activity.map(a => a.text); }
";
        let lints = activity_lints(&[("app/page.tsx", page)]);
        assert_eq!(lints.len(), 1, "{lints:?}");
        assert_eq!(lints[0].path, "lib/types.ts");
        // A lib writer that a page calls is a writer.
        let lib = "export function addTask(state: AppState, t: string): AppState {
  const activity = [...state.activity, { id: t, text: t }];
  return { ...state, tasks: [...state.tasks, t], activity };
}
";
        let called = "import { addTask } from '../lib/tasks';
export default function Board() { addTask(s, 'x'); }
";
        assert!(
            activity_lints(&[("lib/tasks.ts", lib), ("app/board/page.tsx", called)]).is_empty()
        );
        // A private helper that an exported mutation of its file calls.
        let private = "function logActivity(state: AppState): AppState {
  return { ...state, activity: [...state.activity, { id: 'a', text: 'a' }] };
}
export function addTask(state: AppState): AppState { return logActivity(state); }
";
        assert!(activity_lints(&[("lib/tasks.ts", private)]).is_empty());
        // The same writer that no file calls does not count.
        assert_eq!(activity_lints(&[("lib/tasks.ts", lib)]).len(), 1);
        // A page that builds the field itself is a writer.
        let writes = "setState(s => ({ ...s, activity: [entry, ...s.activity] }));
";
        assert!(activity_lints(&[("app/board/page.tsx", writes)]).is_empty());
    }

    #[test]
    fn date_helper_that_converts_a_timestamp_first_is_not_strict() {
        let body = "const calendar = value.includes('T') ? toCalendar(new Date(value)) : value;
  const m = DATE_RE.exec(calendar);
  if (!m) throw new RangeError('x');";
        assert!(cuts_timestamp(body));
        assert!(cuts_timestamp(
            "const m = RE.exec(value.slice(0, 10)); if (!m) throw 1;"
        ));
        assert!(cuts_timestamp("const [day] = value.split('T');"));
        assert!(!cuts_timestamp(
            "const m = DATE_RE.exec(value);
  if (!m) throw new RangeError('x');"
        ));
        assert!(!cuts_timestamp(
            "if (value.includes('T')) throw new Error('no');"
        ));
    }

    use super::*;

    fn app(files: &[(&str, &str)]) -> Vec<(String, String)> {
        files
            .iter()
            .map(|(path, content)| (path.to_string(), content.to_string()))
            .collect()
    }

    fn named(lints: &[Lint], name: &str) -> Vec<Lint> {
        lints
            .iter()
            .filter(|lint| lint.name == name)
            .cloned()
            .collect()
    }

    #[test]
    fn helpers_parse_keys_comments_and_words() {
        let code = blank_comments("a // b \"c\"\n/* d\n e */ f 'g // h'");
        assert_eq!(code.lines().count(), 3);
        assert!(!code.contains('b') && !code.contains('d'));
        assert!(code.contains("'g // h'"));
        let text = "{ projects: next, tasks, ...rest, nested: { a: 1 }, 'q': 2 }";
        let (keys, spread) = object_keys(text, 0).unwrap();
        assert_eq!(keys, vec!["projects", "tasks", "nested"]);
        assert!(spread);
        let body = "{\n  theme: \"light\" | \"dark\";\n  defaultProjectId?: string | null\n  count: number\n}";
        assert_eq!(
            object_keys(body, 0).unwrap().0,
            vec!["theme", "defaultProjectId", "count"]
        );
        assert_eq!(word_positions("a ab a_b a", "a"), vec![0, 9]);
        assert_eq!(
            camel_words("defaultProjectId"),
            vec!["default", "project", "id"]
        );
        assert_eq!(union_at(" 'a' | \"b\";").unwrap(), vec!["a", "b"]);
        assert_eq!(
            route_segments("app/tasks/[id]/page.tsx").unwrap(),
            vec!["tasks", "[id]"]
        );
        assert_eq!(
            route_segments("app/page.tsx").unwrap(),
            Vec::<String>::new()
        );
        assert_eq!(norm("In Progress"), norm("in-progress"));
    }

    const TYPES: &str = "export type Status = 'backlog' | 'in-progress' | 'done';\n\
        export interface Task { id: string; status: Status }\n\
        export interface Settings {\n  theme: 'light' | 'dark';\n  defaultProjectId: string | null;\n}\n";

    #[test]
    fn settings_unread_names_the_page_that_should_read_it() {
        let settings = "'use client';\nexport default function S() { const s = load(); \
            save({ ...s, settings: { ...s.settings, defaultProjectId: id } }); \
            return <select value={s.settings.defaultProjectId ?? ''} /> }";
        let files = app(&[
            ("lib/types.ts", TYPES),
            ("lib/storage.ts", "const D = { theme: 'light', defaultProjectId: null };"),
            ("app/settings/page.tsx", settings),
            ("app/board/page.tsx", "'use client';\nexport default function B() { const p = projects[0]; }"),
            ("app/layout.tsx", "export default function L({children}) { document.documentElement.dataset.theme = s.settings.theme; }"),
        ]);
        let found = named(&lint_app(&files), "settings-unread");
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].path, "app/board/page.tsx");
        assert_eq!(found[0].level, Level::Error);
        assert!(found[0]
            .render()
            .starts_with("ERROR app/board/page.tsx(1,1): error TS9010: settings-unread:"));
        // The board reads it: no finding.
        let mut fixed = files.clone();
        fixed[3].1 = "export default function B() { const p = state.settings.defaultProjectId ?? projects[0]; }".into();
        assert!(named(&lint_app(&fixed), "settings-unread").is_empty());
        // A read only in the storage validator does not count.
        let mut storage_only = files.clone();
        storage_only[1].1 = "if (typeof s.defaultProjectId !== 'string') return null;".into();
        assert_eq!(named(&lint_app(&storage_only), "settings-unread").len(), 1);
        // A dynamic board always has a project in its URL: the project list
        // that links to it should pick the default.
        let mut dynamic = files.clone();
        dynamic[3].0 = "app/projects/[id]/board/page.tsx".into();
        dynamic.push((
            "app/projects/page.tsx".into(),
            "export default function P() { return <a href={`/projects/${p.id}/board`} />; }".into(),
        ));
        let found = named(&lint_app(&dynamic), "settings-unread");
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].path, "app/projects/page.tsx");
        assert!(found[0].message.contains("board link"));
    }

    #[test]
    fn literal_case_flags_a_label_compared_with_a_union_field() {
        let team = "export function open(tasks: {status: string}[]) { return tasks.filter(t => t.status !== \"Done\").length; }";
        let files = app(&[("lib/types.ts", TYPES), ("lib/team.ts", team)]);
        let found = named(&lint_app(&files), "literal-case");
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].path, "lib/team.ts");
        assert!(found[0].message.contains("\"done\""));
        // The type's own value, and an unrelated string: no finding.
        let good = "const a = t.status === 'done'; const b = t.title === 'Done';";
        let files = app(&[("lib/types.ts", TYPES), ("lib/team.ts", good)]);
        assert!(named(&lint_app(&files), "literal-case").is_empty());
        // `case` in a switch on the field.
        let label = "switch (task.status) { case 'In Progress': return 1; case 'done': return 2; }";
        let files = app(&[("lib/types.ts", TYPES), ("lib/label.ts", label)]);
        assert_eq!(named(&lint_app(&files), "literal-case").len(), 1);
        // A label-to-key mapper on a second, label-valued `Status` type (run
        // 20261114-160206), and a normaliser on a bare string parameter:
        // no finding.
        let board_types = "export type Status = 'Backlog' | 'In Progress' | 'Done';\n\
            export interface Column { status: Status; title: string }";
        let mapper = "import type { Status } from '@/lib/board';\n\
            function statusKey(status: Status) { if (status === 'Backlog') return 'backlog'; \
            if (status === 'In Progress') return 'in-progress'; return 'done'; }\n\
            function norm(s: string) { return s === 'inProgress' ? 'in-progress' : s; }";
        let files = app(&[
            ("lib/types.ts", TYPES),
            ("lib/board.ts", board_types),
            ("app/board/page.tsx", mapper),
        ]);
        assert!(
            named(&lint_app(&files), "literal-case").is_empty(),
            "{:?}",
            named(&lint_app(&files), "literal-case")
        );
        // A field typed through the imported label type holds the labels.
        let column = "import type { Status } from '../lib/board';\n\
            interface Card { status: Status }\nconst done = (c: Card) => c.status === 'Done';";
        let files = app(&[
            ("lib/types.ts", TYPES),
            ("lib/board.ts", board_types),
            ("app/board/card.tsx", column),
        ]);
        assert!(named(&lint_app(&files), "literal-case").is_empty());
        // ... but a module that does not import the board's labels compares
        // records of the shared types: still a finding.
        let files = app(&[
            ("lib/types.ts", TYPES),
            ("lib/board.ts", board_types),
            ("lib/team.ts", team),
        ]);
        assert_eq!(named(&lint_app(&files), "literal-case").len(), 1);
        // A defensive check of both spellings on one line: no finding.
        let both =
            "const n = tasks.filter(t => t.status === 'done' || t.status === 'Done').length;";
        let files = app(&[("lib/types.ts", TYPES), ("lib/projects.ts", both)]);
        assert!(named(&lint_app(&files), "literal-case").is_empty());
    }

    #[test]
    fn theme_selector_matches_setters_and_style_rules() {
        let css_class = ".theme-dark { --bg: #000; }";
        let css_attr = ":root[data-theme=\"dark\"] { --bg: #000; }";
        let attr =
            "'use client';\nuseEffect(() => { document.documentElement.dataset.theme = t; });";
        let class = "'use client';\nreturn <html className={`theme-${theme}`}>";
        // Attribute setter, class rule: the setter must change.
        let files = app(&[("app/globals.css", css_class), ("lib/store.ts", attr)]);
        let found = named(&lint_app(&files), "theme-selector");
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].path, "lib/store.ts");
        assert!(found[0].message.contains("classList.toggle(\"theme-dark\""));
        // Class setter, attribute rule.
        let files = app(&[("app/globals.css", css_attr), ("app/layout.tsx", class)]);
        assert_eq!(named(&lint_app(&files), "theme-selector").len(), 1);
        // Matching pairs: nothing.
        for (css, code) in [(css_class, class), (css_attr, attr)] {
            let files = app(&[("app/globals.css", css), ("lib/store.ts", code)]);
            assert!(named(&lint_app(&files), "theme-selector").is_empty());
        }
        let body = "document.body.className = dark ? 'dark' : '';";
        let files = app(&[
            ("app/globals.css", "body.dark { color: white }"),
            ("lib/store.tsx", body),
        ]);
        assert!(named(&lint_app(&files), "theme-selector").is_empty());
    }

    #[test]
    fn dead_route_flags_links_without_a_page() {
        let pages = [
            ("app/page.tsx", "export default function H() { return <Link href=\"/transactions/new\">Add</Link>; }"),
            ("app/transactions/page.tsx", "export default function T() { return <a href={`/transactions/${t.id}`}>x</a>; }"),
            ("app/transactions/[id]/page.tsx", "export default function D() { const t = find(id); }"),
            ("components/Nav.tsx", "const links = [{ href: \"/\" }, { href: \"/transactions\" }, { href: '/reports' }];"),
        ];
        let found = named(&lint_app(&app(&pages)), "dead-route");
        assert_eq!(found.len(), 2, "{found:?}");
        assert!(found
            .iter()
            .any(|lint| lint.path == "app/page.tsx"
                && lint.message.contains("takes \"new\" as an id")));
        assert!(found
            .iter()
            .any(|lint| lint.path == "components/Nav.tsx" && lint.message.contains("`/reports`")));
        // A query string, a concatenated id and a page that handles "new".
        let mut ok = pages.to_vec();
        ok[2].1 = "if (id === 'new') { return <Form />; }";
        ok[3].1 =
            "router.push('/transactions?account=' + id); <a href={\"/transactions/\" + id} />";
        assert!(named(&lint_app(&app(&ok)), "dead-route").is_empty());
    }

    #[test]
    fn server_storage_flags_a_server_page_that_reads_storage() {
        let store = "export function loadStore() { if (typeof window === 'undefined') return seed(); return JSON.parse(localStorage.getItem('k')!); }";
        let page = "import { loadStore } from '../../lib/store';\nexport default async function Search({ searchParams }) { const s = loadStore(); return <p>{s.tasks.length}</p>; }";
        let files = app(&[("lib/store.ts", store), ("app/search/page.tsx", page)]);
        let found = named(&lint_app(&files), "server-storage");
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].line, 2);
        let client = format!("\"use client\";\n{page}");
        let files = app(&[("lib/store.ts", store), ("app/search/page.tsx", &client)]);
        assert!(named(&lint_app(&files), "server-storage").is_empty());
        // A theme script in a string runs in the browser.
        let layout = "const script = `try{var t=localStorage.getItem('t')}catch(e){}`;
export default function L({children}) { return <html><head><script dangerouslySetInnerHTML={{ __html: script }} /></head><body>{children}</body></html>; }";
        let files = app(&[("app/layout.tsx", layout)]);
        assert!(named(&lint_app(&files), "server-storage").is_empty());
        // The same script returned by a helper module: still no finding.
        let helper =
            "export function themeScript() { return `try{localStorage.getItem('t')}catch(e){}`; }";
        let uses = "import { themeScript } from '../lib/theme';\nexport default function L({children}) { return <script dangerouslySetInnerHTML={{ __html: themeScript() }} />; }";
        let files = app(&[("lib/theme.ts", helper), ("app/layout.tsx", uses)]);
        assert!(named(&lint_app(&files), "server-storage").is_empty());
        // A layout that reads storage stays a Server Component (`metadata`).
        let reads = "import { loadStore } from '../lib/store';\nexport const metadata = { title: 'x' };\nexport default function L({children}) { const s = loadStore(); return <html>{children}</html>; }";
        let files = app(&[("lib/store.ts", store), ("app/layout.tsx", reads)]);
        let found = named(&lint_app(&files), "server-storage");
        assert_eq!(found.len(), 1);
        assert!(found[0]
            .message
            .contains("Keep this file a Server Component"));
        assert!(!found[0].message.contains("Make it a Client Component"));
        assert_eq!(
            blank_strings(
                "a('xy', `z
${w}`) + \"q\""
            ),
            "a('  ', ` 
    `) + \" \""
        );
        // A server page that does not touch storage.
        let files = app(&[
            ("lib/store.ts", store),
            (
                "app/about/page.tsx",
                "export default function A() { return <p>About</p>; }",
            ),
        ]);
        assert!(named(&lint_app(&files), "server-storage").is_empty());
    }

    #[test]
    fn partial_save_flags_a_save_that_drops_state() {
        let seed = "export function buildSeed() { return JSON.stringify({ version: 1, projects, tasks, members, comments }); }";
        let page = "'use client';\nfunction persist(p, t) {\n  saveState(KEY, JSON.stringify({ projects: p, tasks: t }));\n}";
        let files = app(&[
            ("lib/seed.ts", seed),
            (
                "lib/storage.ts",
                "export function saveState(key: string, value: string) {}",
            ),
            ("app/projects/page.tsx", page),
        ]);
        let found = named(&lint_app(&files), "partial-save");
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].line, 3);
        assert!(found[0].message.contains("`version`"));
        // The whole state, spread.
        let spread = "saveState(KEY, JSON.stringify({ ...state, projects: p, tasks: t }));";
        let files = app(&[("lib/seed.ts", seed), ("app/projects/page.tsx", spread)]);
        assert!(named(&lint_app(&files), "partial-save").is_empty());
        // A typed save: the type gives the fields.
        let typed = "export interface AppState { projects: P[]; tasks: T[]; settings: S }\nexport function saveState(state: AppState) {}";
        let call = "saveState({ projects, tasks });";
        let files = app(&[("lib/storage.ts", typed), ("app/x/page.tsx", call)]);
        let found = named(&lint_app(&files), "partial-save");
        assert_eq!(found.len(), 1);
        assert!(found[0].message.contains("`settings`"));
        // A create-input type with optional fields is not the stored state.
        let input = "export interface TaskInput { title: string; status: S; due?: string; notes?: string }\nexport function saveTask(input: TaskInput) {}";
        let files = app(&[
            (
                "lib/seed.ts",
                "const t = { title: 'a', status: 'b', due: 'c', notes: 'd' };",
            ),
            ("lib/tasks.ts", input),
            ("app/x/page.tsx", "saveTask({ title, status });"),
        ]);
        assert!(named(&lint_app(&files), "partial-save").is_empty());
        let full = "saveState({ projects, tasks, settings });";
        let files = app(&[("lib/storage.ts", typed), ("app/x/page.tsx", full)]);
        assert!(named(&lint_app(&files), "partial-save").is_empty());
    }

    #[test]
    fn id_collision_flags_a_bare_counter() {
        let ids = "let counter = 0;\nexport function newId(prefix: string) {\n  counter++;\n  return `${prefix}-${counter}`;\n}";
        let found = named(&lint_app(&app(&[("lib/ids.ts", ids)])), "id-collision");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].line, 1);
        assert!(found[0].message.contains("Keep the id format"));
        assert!(!found[0].message.contains("Date.now"));
        // With a state provider, it gets the `syncIds` call.
        let provider = "'use client';\nconst Ctx = createContext(null);";
        let files = app(&[
            ("lib/ids.ts", ids),
            ("components/StoreProvider.tsx", provider),
        ]);
        let found = named(&lint_app(&files), "id-collision");
        assert_eq!(found.len(), 2, "{found:?}");
        assert!(found
            .iter()
            .any(|lint| lint.path == "components/StoreProvider.tsx"
                && lint.message.contains("syncIds")));
        let unique = format!("{ids}\nconst t = Date.now();");
        assert!(named(&lint_app(&app(&[("lib/ids.ts", &unique)])), "id-collision").is_empty());
        // A local loop counter is not an id generator.
        let local = "export function f() { let i = 0; i++; return `${i}`; }";
        assert!(named(&lint_app(&app(&[("lib/x.ts", local)])), "id-collision").is_empty());
    }

    #[test]
    fn id_collision_accepts_a_counter_synced_from_stored_ids() {
        // The repaired files of bench run 20261116-000420-pl-2.
        let ids = "let counter = 0;\n\nexport function makeId(prefix: string): string {\n  counter += 1;\n  return `${prefix}-${counter}`;\n}\n\nexport function resetIds(): void {\n  counter = 0;\n}\n\nexport function syncIds(ids: string[]): void {\n  for (const id of ids) {\n    const n = Number(id.slice(id.lastIndexOf(\"-\") + 1));\n    if (Number.isFinite(n) && n > counter) counter = n;\n  }\n}\n";
        let store = "\"use client\";\nimport { syncIds } from \"./ids\";\nconst Ctx = createContext(null);\nexport function StoreProvider() { useEffect(() => { const s = loadState(); syncIds(collectIds(s)); }, []); }";
        let files = app(&[("lib/ids.ts", ids), ("lib/store.tsx", store)]);
        assert!(named(&lint_app(&files), "id-collision").is_empty());
        // The sync exists but nothing calls it: still an error.
        let uncalled = app(&[
            ("lib/ids.ts", ids),
            (
                "lib/store.tsx",
                "\"use client\";\nconst Ctx = createContext(null);",
            ),
        ]);
        assert!(!named(&lint_app(&uncalled), "id-collision").is_empty());
        // A reset alone (`counter = 0`) is not a sync.
        let reset_only = "let counter = 0;\nexport function makeId(p: string) {\n  counter += 1;\n  return `${p}-${counter}`;\n}\nexport function resetIds() {\n  counter = 0;\n}\n";
        let called = app(&[
            ("lib/ids.ts", reset_only),
            (
                "lib/store.tsx",
                "\"use client\";\nconst Ctx = createContext(null);\nresetIds();",
            ),
        ]);
        assert!(!named(&lint_app(&called), "id-collision").is_empty());
    }

    #[test]
    fn seed_dates_flags_fixed_dates_only() {
        let fixed = "export const tasks = [\n{ due: '2026-11-02' }, { due: '2026-11-05' }, { due: '2026-11-09' }, { due: '2026-11-20' }];";
        let found = named(&lint_app(&app(&[("lib/seed.ts", fixed)])), "seed-dates");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].line, 2);
        let relative = format!("{fixed}\nconst now = Date.now();");
        assert!(named(&lint_app(&app(&[("lib/seed.ts", &relative)])), "seed-dates").is_empty());
        // Not a seed file, and three dates or fewer.
        assert!(named(
            &lint_app(&app(&[("lib/dates.test.ts", fixed)])),
            "seed-dates"
        )
        .is_empty());
        let few = "const a = ['2026-01-01', '2026-01-02'];";
        assert!(named(&lint_app(&app(&[("lib/seed.ts", few)])), "seed-dates").is_empty());
    }

    #[test]
    fn readme_spec_is_a_warning_on_plan_text_and_empty_sections() {
        let spec = "# Project\n\n## Files\n- `lib/tasks.ts` Imports Task from `lib/types.ts`: this file does not declare it.\n\n## Run\n";
        let found = named(&lint_app(&app(&[("README.md", spec)])), "readme-spec");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].level, Level::Warn);
        assert!(found[0].message.contains("`## Run`"));
        let good = "# TaskFlow\n\nA project manager.\n\n## Run\n\n```bash\nnpm install\nnpm run dev\n```\n";
        assert!(named(&lint_app(&app(&[("README.md", good)])), "readme-spec").is_empty());
    }

    #[test]
    fn css_undefined_class_is_a_warning() {
        let css = ".card { padding: 4px } .grid-7 { display: grid }";
        let page = "export default function P() { return <div className=\"card calendar-grid\"><p className={ok ? 'grid-7' : 'missing'} /></div>; }";
        let found = named(
            &lint_app(&app(&[("app/globals.css", css), ("app/page.tsx", page)])),
            "css-undefined-class",
        );
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].level, Level::Warn);
        assert!(
            found[0].message.contains("`calendar-grid`") && found[0].message.contains("`missing`")
        );
        let fine = "export default function P() { return <div className=\"card\" />; }";
        assert!(named(
            &lint_app(&app(&[("app/globals.css", css), ("app/page.tsx", fine)])),
            "css-undefined-class"
        )
        .is_empty());
    }

    #[test]
    fn gate_lines_split_errors_and_warnings() {
        let files = app(&[
            ("README.md", "# P\n\n## Run\n"),
            (
                "lib/ids.ts",
                "let n = 0;\nexport const id = () => `t${++n}`;",
            ),
        ]);
        let (errors, warnings) = gate_lines(&files, true);
        assert_eq!(errors.len(), 1);
        assert!(errors[0].starts_with("ERROR lib/ids.ts(1,1): error TS9016: id-collision:"));
        assert!(errors[0].contains("Keep the id format") && errors[0].contains("syncIds"));
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].starts_with("WARN README.md("));
        // `seed-dates` is a hint unless the plan asked for relative dates.
        let seed = "export const t = [{ d: '2026-11-02' }, { d: '2026-11-05' }, \
                    { d: '2026-11-09' }, { d: '2026-11-20' }];";
        let files = app(&[("lib/seed.ts", seed)]);
        let (errors, warnings) = gate_lines(&files, true);
        assert_eq!((errors.len(), warnings.len()), (1, 0));
        let (errors, warnings) = gate_lines(&files, false);
        assert_eq!((errors.len(), warnings.len()), (0, 1));
        assert!(warnings[0].starts_with("WARN lib/seed.ts("));
    }

    // ---- qlint2 ------------------------------------------------------------

    fn conventions(files: &[(&str, &str)], name: &str) -> Vec<Lint> {
        let files = app(files);
        let all = lint_app_with(&files, false, true, false);
        assert!(all.iter().all(|lint| NAMES2.contains(&lint.name)));
        assert!(
            lint_app(&files)
                .iter()
                .all(|lint| !NAMES2.contains(&lint.name)),
            "the qlint set has no convention lint"
        );
        named(&all, name)
    }

    const STORAGE: &str = "export function loadState() { return JSON.parse(localStorage.getItem(\"k\") ?? \"{}\"); }\nexport function saveState(s) { localStorage.setItem(\"k\", JSON.stringify(s)); }\n";
    const PROVIDER: &str = "\"use client\";\nimport { createContext, useContext } from \"react\";\nimport { loadState } from \"../lib/storage\";\nconst Ctx = createContext(null);\nexport function StoreProvider({ children }) {\n  const state = loadState();\n  return <Ctx.Provider value={state}>{children}</Ctx.Provider>;\n}\nexport function useStore() { return useContext(Ctx); }\n";
    const LAYOUT: &str = "import { StoreProvider } from \"../components/StoreProvider\";\nexport default function RootLayout({ children }) {\n  return <html><body><StoreProvider>{children}</StoreProvider></body></html>;\n}\n";

    #[test]
    fn storage_outside_store_names_the_page_beside_the_provider() {
        let page = "import { loadState, saveState } from \"../../lib/storage\";\nexport default function Goals() {\n  const state = loadState();\n  saveState(state);\n  return <div />;\n}\n";
        let good = "import { useStore } from \"../components/StoreProvider\";\nexport default function Home() {\n  const state = useStore();\n  return <div>{String(state)}</div>;\n}\n";
        let files = [
            ("lib/storage.ts", STORAGE),
            ("components/StoreProvider.tsx", PROVIDER),
            ("app/layout.tsx", LAYOUT),
            ("app/goals/page.tsx", page),
            ("app/page.tsx", good),
        ];
        let lints = conventions(&files, "storage-outside-store");
        assert_eq!(lints.len(), 1, "{lints:?}");
        assert_eq!(
            (lints[0].path.as_str(), lints[0].line),
            ("app/goals/page.tsx", 3)
        );
        assert!(lints[0].message.contains("`useStore()`"));
        // A theme provider that keeps only its theme is not a store.
        let theme = "import { createContext } from \"react\";\nconst Ctx = createContext(null);\nexport function ThemeProvider({ children }) {\n  const theme = localStorage.getItem(\"theme\");\n  return <Ctx.Provider value={theme}>{children}</Ctx.Provider>;\n}\n";
        let layout = LAYOUT.replace("StoreProvider", "ThemeProvider");
        let files = [
            ("lib/storage.ts", STORAGE),
            ("components/StoreProvider.tsx", theme),
            ("app/layout.tsx", layout.as_str()),
            ("app/goals/page.tsx", page),
        ];
        assert!(conventions(&files, "storage-outside-store").is_empty());
    }

    #[test]
    fn duplicate_shell_names_the_page_that_renders_the_layout_shell() {
        let shell = "export function AppShell({ children }) {\n  return <div className=\"frame\"><nav />{children}</div>;\n}\n";
        let layout = "import { AppShell } from \"../components/AppShell\";\nexport default function RootLayout({ children }) {\n  return <html><body><AppShell>{children}</AppShell></body></html>;\n}\n";
        let twice = "import { AppShell } from \"../components/AppShell\";\nexport default function Home() {\n  return (\n    <AppShell>\n      <p>Home</p>\n    </AppShell>\n  );\n}\n";
        let once = "export default function About() {\n  return <p>About</p>;\n}\n";
        let files = [
            ("components/AppShell.tsx", shell),
            ("app/layout.tsx", layout),
            ("app/page.tsx", twice),
            ("app/about/page.tsx", once),
        ];
        let lints = conventions(&files, "duplicate-shell");
        assert_eq!(lints.len(), 1, "{lints:?}");
        assert_eq!((lints[0].path.as_str(), lints[0].line), ("app/page.tsx", 4));
        assert!(lints[0].message.contains("shows twice"));
        // The layout's own frame class drawn again by a page.
        let framed = "export default function About() {\n  return <div className=\"app-shell\"><p>About</p></div>;\n}\n";
        let layout = "export default function RootLayout({ children }) {\n  return <html><body><div className=\"app-shell\"><nav>x</nav>{children}</div></body></html>;\n}\n";
        let files = [
            ("app/layout.tsx", layout),
            ("app/about/page.tsx", framed),
            ("app/page.tsx", once),
        ];
        let lints = conventions(&files, "duplicate-shell");
        assert_eq!(lints.len(), 1, "{lints:?}");
        assert_eq!(lints[0].path, "app/about/page.tsx");
        assert!(lints[0].message.contains("nested"));
    }

    /// The frame of the recorded runs: the layout renders `<AppShell>`, whose
    /// `<main className="shell">` holds the page.
    const FRAME_SHELL: &str = "\"use client\";\nimport Link from \"next/link\";\n\nexport default function AppShell({ children }: { children: React.ReactNode }) {\n  return (\n    <>\n      <nav className=\"nav\">\n        <Link href=\"/\" className=\"nav-link\">Overview</Link>\n      </nav>\n      {children ? <main className=\"shell\">{children}</main> : null}\n    </>\n  );\n}\n";
    const FRAME_LAYOUT: &str = "import type { Metadata } from 'next';\nimport './globals.css';\nimport { StoreProvider } from '../lib/store';\nimport AppShell from '../components/AppShell';\n\nexport default function RootLayout({ children }: { children: React.ReactNode }) {\n  return (\n    <html lang=\"en\">\n      <body>\n        <StoreProvider>\n          <AppShell>{children}</AppShell>\n        </StoreProvider>\n      </body>\n    </html>\n  );\n}\n";
    const FRAME_STORE: &str = "\"use client\";\nimport { createContext, useContext } from \"react\";\nconst Ctx = createContext(null);\nexport function StoreProvider({ children }) { return <Ctx.Provider value={null}>{children}</Ctx.Provider>; }\nexport function useStore() { return useContext(Ctx); }\n";

    /// The app of the frame with `pages`, fixed: (fixed pages, pages left).
    fn shell_fix(pages: &[(&str, &str)]) -> (BTreeMap<String, String>, Vec<String>) {
        let mut files = vec![
            ("components/AppShell.tsx", FRAME_SHELL),
            ("app/layout.tsx", FRAME_LAYOUT),
            ("lib/store.tsx", FRAME_STORE),
        ];
        files.extend_from_slice(pages);
        let files = app(&files);
        let (fixed, left) = fix_duplicate_shell(&files);
        // The lint passes on every fixed page.
        let after: Vec<(String, String)> = files
            .iter()
            .map(|(path, text)| {
                let text = fixed
                    .iter()
                    .find(|(fixed, _)| fixed == path)
                    .map_or(text, |(_, text)| text);
                (path.clone(), text.clone())
            })
            .collect();
        let lints = lint_app_with(&after, false, true, false);
        for (path, _) in &fixed {
            assert!(
                named(&lints, "duplicate-shell")
                    .iter()
                    .all(|lint| &lint.path != path),
                "{path} still draws the frame"
            );
        }
        (fixed.into_iter().collect(), left)
    }

    #[test]
    fn shellfix_unwraps_the_frame_class_from_every_return_of_a_page() {
        let page = "\"use client\";\n\nimport Link from \"next/link\";\nimport { useStore } from \"../../lib/store\";\n\nexport default function LogPage() {\n  const { state } = useStore();\n  if (state.vehicles.length === 0) {\n    return (\n      <div className=\"shell\">\n        <h1>Log service</h1>\n        <div className=\"card\">\n          <Link className=\"button\" href=\"/vehicles\">Go to vehicles</Link>\n        </div>\n      </div>\n    );\n  }\n  return (\n    <div className=\"shell\">\n      <h1>Log service</h1>\n      <div className=\"card\">\n        <div className=\"row\">\n          <button onClick={() => (state.x > 1 ? 1 : 2)}>Save</button>\n        </div>\n        {state.items.map((item) => <div key={item.id}>{item.name}</div>)}\n      </div>\n    </div>\n  );\n}\n";
        let (fixed, left) = shell_fix(&[("app/log/page.tsx", page)]);
        assert!(left.is_empty(), "{left:?}");
        let text = &fixed["app/log/page.tsx"];
        assert!(!text.contains("shell"), "{text}");
        assert_eq!(text.matches("<>").count(), 2, "{text}");
        assert_eq!(text.matches("</>").count(), 2, "{text}");
        // Only the wrapper changed: the inner elements close where they did.
        let expected = page
            .replace("<div className=\"shell\">", "<>")
            .replace("      </div>\n    );\n  }", "      </>\n    );\n  }")
            .replace("    </div>\n  );\n}\n", "    </>\n  );\n}\n");
        assert_eq!(text, &expected);
    }

    #[test]
    fn shellfix_unwraps_the_shell_component_and_drops_its_import() {
        let page = "import AppShell from \"../components/AppShell\";\nimport { formatMoney } from \"../lib/money\";\n\nexport default function Home() {\n  return (\n    <AppShell>\n      <h1>Home</h1>\n      <p>{formatMoney(1)}</p>\n    </AppShell>\n  );\n}\n";
        let money = "export function formatMoney(n: number): string { return String(n); }\n";
        let (fixed, left) = shell_fix(&[("app/page.tsx", page), ("lib/money.ts", money)]);
        assert!(left.is_empty(), "{left:?}");
        assert_eq!(
            fixed["app/page.tsx"],
            "import { formatMoney } from \"../lib/money\";\n\nexport default function Home() {\n  return (\n    <>\n      <h1>Home</h1>\n      <p>{formatMoney(1)}</p>\n    </>\n  );\n}\n"
        );
        // A named import list keeps its other names.
        let listed = "import { AppShell, SHELL_TITLE } from \"../components/AppShell\";\nexport default function Home() {\n  return <AppShell><h1>{SHELL_TITLE}</h1></AppShell>;\n}\n";
        let shell =
            format!("{FRAME_SHELL}export const SHELL_TITLE = \"x\";\nexport {{ AppShell }};\n");
        let mut files = app(&[
            ("components/AppShell.tsx", shell.as_str()),
            ("app/layout.tsx", FRAME_LAYOUT),
            ("app/page.tsx", listed),
        ]);
        files[1].1 = files[1]
            .1
            .replace("import AppShell from", "import { AppShell } from");
        let (fixed, left) = fix_duplicate_shell(&files);
        assert!(left.is_empty(), "{left:?}");
        assert_eq!(
            fixed[0].1,
            "import { SHELL_TITLE } from \"../components/AppShell\";\nexport default function Home() {\n  return <><h1>{SHELL_TITLE}</h1></>;\n}\n"
        );
    }

    #[test]
    fn shellfix_keeps_other_classes_and_pages_without_the_frame() {
        let mixed = "export default function Fuel() {\n  return (\n    <div className=\"shell fuel-page\">\n      <h1>Fuel</h1>\n    </div>\n  );\n}\n";
        let plain = "export default function About() {\n  return (\n    <section className=\"card\">\n      <h1>About</h1>\n    </section>\n  );\n}\n";
        let (fixed, left) =
            shell_fix(&[("app/fuel/page.tsx", mixed), ("app/about/page.tsx", plain)]);
        assert!(left.is_empty(), "{left:?}");
        assert_eq!(fixed.len(), 1, "{fixed:?}");
        assert_eq!(
            fixed["app/fuel/page.tsx"],
            mixed.replace("\"shell fuel-page\"", "\"fuel-page\"")
        );
    }

    #[test]
    fn shellfix_skips_a_closing_tag_inside_a_string() {
        let page = "export default function Docs() {\n  return (\n    <div className=\"shell\">\n      <h1>Tags</h1>\n      <code>{\"</div>\"}</code>\n      <code>{`<div ${1}`}</code>\n    </div>\n  );\n}\n";
        let (fixed, left) = shell_fix(&[("app/docs/page.tsx", page)]);
        assert!(left.is_empty(), "{left:?}");
        let expected = page
            .replace("<div className=\"shell\">", "<>")
            .replace("    </div>\n  );", "    </>\n  );");
        assert_eq!(fixed["app/docs/page.tsx"], expected);
    }

    #[test]
    fn shellfix_leaves_an_unclear_page_for_the_repair() {
        // The frame class also on a fallback that is not a returned root.
        let suspense = "import { Suspense } from \"react\";\nexport default function Search() {\n  return (\n    <Suspense fallback={<div className=\"shell\"><p>Loading</p></div>}>\n      <div className=\"shell\"><p>Results</p></div>\n    </Suspense>\n  );\n}\n";
        // A wrapper with a style of its own: unwrapping would lose it.
        let styled = "export default function Reports() {\n  return (\n    <div className=\"shell\" style={{ gap: 16 }}>\n      <h1>Reports</h1>\n    </div>\n  );\n}\n";
        // A shell that closes itself (no children to keep).
        let bare = "import AppShell from \"../../components/AppShell\";\nexport default function Team() {\n  return <AppShell />;\n}\n";
        let (fixed, mut left) = shell_fix(&[
            ("app/search/page.tsx", suspense),
            ("app/reports/page.tsx", styled),
            ("app/team/page.tsx", bare),
        ]);
        assert!(fixed.is_empty(), "{fixed:?}");
        left.sort();
        assert_eq!(
            left,
            [
                "app/reports/page.tsx",
                "app/search/page.tsx",
                "app/team/page.tsx"
            ]
        );
        // A closing tag inside a literal that is not the expression's head:
        // the found close would cut the children, so the page is left.
        let literal = "export default function Docs() {\n  return (\n    <div className=\"shell\">\n      <p>{ok ? \"a\" : \"</div>\"}</p>\n    </div>\n  );\n}\n";
        let (fixed, left) = shell_fix(&[("app/docs/page.tsx", literal)]);
        assert!(fixed.is_empty(), "{fixed:?}");
        assert_eq!(left, ["app/docs/page.tsx"]);
        // No layout: nothing to fix.
        let files = app(&[("app/reports/page.tsx", styled)]);
        assert_eq!(fix_duplicate_shell(&files), (Vec::new(), Vec::new()));
    }

    #[test]
    fn landed_lines_wait_for_the_layout_and_its_shell() {
        let page = "export default function Settings() {\n  return (\n    <div className=\"shell\">\n      <h1>Settings</h1>\n    </div>\n  );\n}\n";
        // The shell has not landed: the page waits, no line.
        let files = app(&[
            ("app/layout.tsx", FRAME_LAYOUT),
            ("lib/store.tsx", FRAME_STORE),
            ("app/settings/page.tsx", page),
        ]);
        let (lines, waiting) = landed_lines(&files);
        assert!(lines.is_empty(), "{lines:?}");
        assert_eq!(
            waiting.into_iter().collect::<Vec<_>>(),
            ["app/settings/page.tsx"]
        );
        // No layout yet: the same.
        let (lines, waiting) = landed_lines(&app(&[("app/settings/page.tsx", page)]));
        assert!(lines.is_empty() && waiting.len() == 1);
        // Everything landed: the gate's line, with the layout as context.
        let files = app(&[
            ("components/AppShell.tsx", FRAME_SHELL),
            ("app/layout.tsx", FRAME_LAYOUT),
            ("lib/store.tsx", FRAME_STORE),
            ("app/settings/page.tsx", page),
        ]);
        let (lines, waiting) = landed_lines(&files);
        assert!(waiting.is_empty());
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(lines[0].starts_with("ERROR app/settings/page.tsx(3,1): error TS9021"));
        assert!(
            lines[0].contains("The root layout, from `app/layout.tsx`, already renders <AppShell>")
        );
        // The gate's own line is unchanged.
        let gate = named(
            &lint_app_with(&files, false, true, false),
            "duplicate-shell",
        )
        .iter()
        .map(Lint::render)
        .collect::<Vec<_>>();
        assert_eq!(gate.len(), 1);
        assert!(gate[0].contains(
            "error TS9021: duplicate-shell: `app/layout.tsx` already renders <AppShell>"
        ));
        // The in-wave line is a repair of the page alone; the gate's names
        // the layout as a second file to change (a joint repair).
        let texts: BTreeMap<&str, &str> = files
            .iter()
            .map(|(path, text)| (path.as_str(), text.as_str()))
            .collect();
        let named_with = |line: &str| {
            slimtoken::repair::named_files("app/settings/page.tsx", line, &texts, is_test)
        };
        assert!(named_with(&lines[0]).is_empty());
        assert_eq!(named_with(&gate[0]), ["app/layout.tsx"]);
    }

    const TX_TYPES: &str = "export interface Transaction {\n  id: string;\n  amount: number;\n  date: string;\n  note: string;\n}\n";
    const TX_LIB: &str = "import { Transaction } from \"./types\";\nfunction assertAmount(amount: number) {\n  if (amount <= 0) throw new Error(\"amount must be positive\");\n}\nexport function addTransaction(list: Transaction[], tx: Transaction): Transaction[] {\n  assertAmount(tx.amount);\n  return [...list, tx];\n}\n";
    const TX_FORM: &str = "import { addTransaction } from \"../lib/transactions\";\nexport function TransactionForm({ list, kind, value }) {\n  const amount = kind === \"income\" ? value : -value;\n  return addTransaction(list, { id: \"t\", amount, date: \"2026-01-01\", note: \"\" });\n}\n";

    #[test]
    fn sign_mismatch_names_the_validator_or_the_producer() {
        let files = [
            ("lib/types.ts", TX_TYPES),
            ("lib/transactions.ts", TX_LIB),
            ("components/TransactionForm.tsx", TX_FORM),
        ];
        let lints = conventions(&files, "sign-mismatch");
        assert_eq!(lints.len(), 1, "{lints:?}");
        assert_eq!(
            (lints[0].path.as_str(), lints[0].line),
            ("lib/transactions.ts", 3)
        );
        assert!(lints[0]
            .message
            .contains("components/TransactionForm.tsx:4"));
        // A positive convention in the types file: the producer changes.
        let stated = TX_TYPES.replace(
            "amount: number;",
            "amount: number; // integer cents, always > 0",
        );
        let files = [
            ("lib/types.ts", stated.as_str()),
            ("lib/transactions.ts", TX_LIB),
            ("components/TransactionForm.tsx", TX_FORM),
        ];
        let lints = conventions(&files, "sign-mismatch");
        assert_eq!(lints.len(), 1, "{lints:?}");
        assert_eq!(lints[0].path, "components/TransactionForm.tsx");
        // Every amount positive: nothing to say.
        let positive = TX_FORM.replace("-value", "value");
        let files = [
            ("lib/types.ts", TX_TYPES),
            ("lib/transactions.ts", TX_LIB),
            ("components/TransactionForm.tsx", positive.as_str()),
        ];
        assert!(conventions(&files, "sign-mismatch").is_empty());
    }

    #[test]
    fn date_format_mismatch_names_the_strict_helper() {
        let dates = "const ISO_DATE = /^(\\d{4})-(\\d{2})-(\\d{2})$/;\nexport function parseDate(value: string): Date {\n  const m = ISO_DATE.exec(value);\n  if (!m) throw new RangeError(\"bad date\");\n  return new Date(Number(m[1]), Number(m[2]) - 1, Number(m[3]));\n}\n";
        let seed = "export const tasks = [\n  { id: \"t1\", title: \"a\", createdAt: new Date().toISOString() },\n];\n";
        let metrics = "import { parseDate } from \"./dates\";\nexport function age(t: { createdAt: string }) {\n  return parseDate(t.createdAt).getTime();\n}\n";
        let files = [
            ("lib/dates.ts", dates),
            ("lib/seed.ts", seed),
            ("lib/metrics.ts", metrics),
        ];
        let lints = conventions(&files, "date-format-mismatch");
        assert_eq!(lints.len(), 1, "{lints:?}");
        assert_eq!((lints[0].path.as_str(), lints[0].line), ("lib/dates.ts", 2));
        assert!(
            lints[0].message.contains("lib/metrics.ts:3")
                && lints[0].message.contains("lib/seed.ts:2")
        );
        // Cut to the date first: it matches.
        let cut = metrics.replace("t.createdAt)", "t.createdAt.slice(0, 10))");
        let files = [
            ("lib/dates.ts", dates),
            ("lib/seed.ts", seed),
            ("lib/metrics.ts", cut.as_str()),
        ];
        assert!(conventions(&files, "date-format-mismatch").is_empty());
        // A helper that falls back to `new Date(value)` rejects nothing.
        let lenient = dates.replace(
            "if (!m) throw new RangeError(\"bad date\");",
            "if (!m) return new Date(value);",
        );
        let files = [
            ("lib/dates.ts", lenient.as_str()),
            ("lib/seed.ts", seed),
            ("lib/metrics.ts", metrics),
        ];
        assert!(conventions(&files, "date-format-mismatch").is_empty());
    }

    #[test]
    fn transition_timestamps_names_the_status_change_without_the_stamp() {
        let types = "export interface Task {\n  id: string;\n  status: \"todo\" | \"done\";\n  completedAt: string | null;\n}\n";
        let board = "import { Task } from \"./types\";\nexport function moveTask(tasks: Task[], id: string, status: Task[\"status\"]): Task[] {\n  return tasks.map((t) => (t.id === id ? { ...t, status } : t));\n}\n";
        let page = "import { moveTask } from \"../../lib/board\";\nexport default function Board() {\n  moveTask([], \"a\", \"done\");\n  return <div />;\n}\n";
        let files = [
            ("lib/types.ts", types),
            ("lib/board.ts", board),
            ("app/board/page.tsx", page),
        ];
        let lints = conventions(&files, "transition-timestamps");
        assert_eq!(lints.len(), 1, "{lints:?}");
        assert_eq!((lints[0].path.as_str(), lints[0].line), ("lib/board.ts", 3));
        assert!(
            lints[0].message.contains("`moveTask`") && lints[0].message.contains("`completedAt`")
        );
        let stamped = board.replace(
            "{ ...t, status }",
            "{ ...t, status, completedAt: status === \"done\" ? new Date().toISOString() : null }",
        );
        let files = [
            ("lib/types.ts", types),
            ("lib/board.ts", stamped.as_str()),
            ("app/board/page.tsx", page),
        ];
        assert!(conventions(&files, "transition-timestamps").is_empty());
        // A helper that no screen calls.
        let files = [("lib/types.ts", types), ("lib/board.ts", board)];
        assert!(conventions(&files, "transition-timestamps").is_empty());
    }

    #[test]
    fn hardcoded_currency_names_the_formatter_call() {
        let types = "export interface Settings {\n  currency: string;\n  theme: string;\n}\n";
        let table = "import { formatMoney } from \"../lib/money\";\nexport function Table({ cents }) {\n  return <td>{formatMoney(cents, \"USD\")}</td>;\n}\n";
        let files = [("lib/types.ts", types), ("components/Table.tsx", table)];
        let lints = conventions(&files, "hardcoded-currency");
        assert_eq!(lints.len(), 1, "{lints:?}");
        assert_eq!(
            (lints[0].path.as_str(), lints[0].line),
            ("components/Table.tsx", 3)
        );
        let fallback = table.replace("\"USD\")", "settings.currency ?? \"USD\")");
        let files = [
            ("lib/types.ts", types),
            ("components/Table.tsx", fallback.as_str()),
        ];
        assert!(conventions(&files, "hardcoded-currency").is_empty());
        // No currency setting: a fixed currency is the app's.
        let files = [
            (
                "lib/types.ts",
                "export interface Settings {\n  theme: string;\n}\n",
            ),
            ("components/Table.tsx", table),
        ];
        assert!(conventions(&files, "hardcoded-currency").is_empty());
    }

    /// Runs 20261114-065909 / -190030: a store provider that the layout does
    /// not mount; 183809: a theme provider around the store that keeps its
    /// theme under its own key; a nav with its own theme key.
    #[test]
    fn storage_outside_store_needs_a_mounted_provider_and_the_state_key() {
        let page = "import { loadState, saveState } from \"../../lib/storage\";\nexport default function Goals() {\n  const state = loadState();\n  saveState(state);\n  return <div />;\n}\n";
        let hooked = "import { useStore } from \"../../components/StoreProvider\";\nexport default function Notes() {\n  const state = useStore();\n  return <div>{String(state)}</div>;\n}\n";
        let bare = "export default function RootLayout({ children }) {\n  return <html><body>{children}</body></html>;\n}\n";
        // Mounted nowhere: the gate's TS9004 says so; pages are not sent to
        // a hook that would throw.
        let files = [
            ("lib/storage.ts", STORAGE),
            ("components/StoreProvider.tsx", PROVIDER),
            ("app/layout.tsx", bare),
            ("app/goals/page.tsx", page),
            ("app/notes/page.tsx", hooked),
        ];
        assert!(conventions(&files, "storage-outside-store").is_empty());
        // Mounted by one page only: the layout wraps the pages in it.
        let wraps = "import { StoreProvider } from \"../../components/StoreProvider\";\nexport default function Board() {\n  return <StoreProvider><p>Board</p></StoreProvider>;\n}\n";
        let files = [
            ("lib/storage.ts", STORAGE),
            ("components/StoreProvider.tsx", PROVIDER),
            ("app/layout.tsx", bare),
            ("app/goals/page.tsx", page),
            ("app/notes/page.tsx", hooked),
            ("app/board/page.tsx", wraps),
        ];
        let lints = conventions(&files, "storage-outside-store");
        assert_eq!(lints.len(), 1, "{lints:?}");
        assert_eq!(
            (lints[0].path.as_str(), lints[0].line),
            ("app/layout.tsx", 2)
        );
        assert!(lints[0]
            .message
            .contains("wrap `{children}` in <StoreProvider>"));
        // A theme provider around the store, and a nav, each with a theme key
        // of their own: not state; the page beside the store still is.
        let theme = "\"use client\";\nimport { createContext } from \"react\";\nconst Ctx = createContext(\"light\");\nexport function ThemeProvider({ children }) {\n  const theme = localStorage.getItem(\"app.theme\");\n  return <Ctx.Provider value={theme}>{children}</Ctx.Provider>;\n}\n";
        let nav = "const THEME_KEY = \"app.theme\";\nexport function Nav() {\n  const toggle = () => localStorage.setItem(THEME_KEY, \"dark\");\n  return <nav onClick={toggle} />;\n}\n";
        let layout = "import { ThemeProvider } from \"../components/ThemeProvider\";\nimport { StoreProvider } from \"../components/StoreProvider\";\nimport { Nav } from \"../components/Nav\";\nexport default function RootLayout({ children }) {\n  return <html><body><ThemeProvider><StoreProvider><Nav />{children}</StoreProvider></ThemeProvider></body></html>;\n}\n";
        let files = [
            ("lib/storage.ts", STORAGE),
            ("components/StoreProvider.tsx", PROVIDER),
            ("components/ThemeProvider.tsx", theme),
            ("components/Nav.tsx", nav),
            ("app/layout.tsx", layout),
            ("app/goals/page.tsx", page),
        ];
        let lints = conventions(&files, "storage-outside-store");
        assert_eq!(lints.len(), 1, "{lints:?}");
        assert_eq!(lints[0].path, "app/goals/page.tsx");
        // The same nav on the state's key is a copy of the state.
        let stale = nav.replace("\"app.theme\"", "\"k\"");
        let files = [
            ("lib/storage.ts", STORAGE),
            ("components/StoreProvider.tsx", PROVIDER),
            ("components/Nav.tsx", stale.as_str()),
            ("app/layout.tsx", layout),
        ];
        let lints = conventions(&files, "storage-outside-store");
        assert_eq!(lints.len(), 1, "{lints:?}");
        assert_eq!(lints[0].path, "components/Nav.tsx");
    }

    #[test]
    fn sign_mismatch_reads_the_convention_in_jsdoc_and_type_comments() {
        let files_with = |types: &str| -> Vec<Lint> {
            let files = [
                ("lib/types.ts", types),
                ("lib/transactions.ts", TX_LIB),
                ("components/TransactionForm.tsx", TX_FORM),
            ];
            conventions(&files, "sign-mismatch")
        };
        // JSDoc above the field, with both "always > 0" and "never negative".
        let jsdoc = TX_TYPES.replace(
            "  amount: number;",
            "  /** Integer cents, always > 0, never negative; kind says expense or income. */\n  amount: number;",
        );
        let lints = files_with(&jsdoc);
        assert_eq!(lints.len(), 1, "{lints:?}");
        assert_eq!(lints[0].path, "components/TransactionForm.tsx");
        assert!(
            lints[0].message.contains("lib/types.ts:4"),
            "{}",
            lints[0].message
        );
        // The clause that names the field in the comment of its type.
        let typed = TX_TYPES.replace(
            "export interface Transaction {",
            "// amount: integer cents, always > 0; date: calendar date\nexport interface Transaction {",
        );
        let lints = files_with(&typed);
        assert_eq!(lints.len(), 1, "{lints:?}");
        assert_eq!(lints[0].path, "components/TransactionForm.tsx");
        // Nothing stated: the validator, unless qsemantic gave the default.
        let lints = files_with(TX_TYPES);
        assert_eq!(lints[0].path, "lib/transactions.ts");
        let files = [
            ("lib/types.ts", TX_TYPES),
            ("lib/transactions.ts", TX_LIB),
            ("components/TransactionForm.tsx", TX_FORM),
        ];
        let files = app(&files);
        let lints: Vec<Lint> = lint_app_with(&files, false, true, true)
            .into_iter()
            .filter(|lint| lint.name == "sign-mismatch")
            .collect();
        assert_eq!(lints.len(), 1, "{lints:?}");
        assert_eq!(lints[0].path, "components/TransactionForm.tsx");
        assert!(lints[0].message.contains("conventions block"));
        assert_eq!(note_sign("expenses negative, income positive"), Some(false));
        assert_eq!(note_sign("always > 0, or negative"), None);
        assert_eq!(note_sign("signed: expenses negative"), Some(false));
        assert_eq!(note_sign("non-negative cents"), Some(true));
    }

    /// Run 20261114-073835: two files that each keep a private `parseIso`.
    #[test]
    fn date_format_mismatch_resolves_each_call_to_its_helper() {
        let strict = "const ISO_DATE = /^(\\d{4})-(\\d{2})-(\\d{2})$/;\nfunction parseIso(value: string): Date {\n  const m = ISO_DATE.exec(value);\n  if (!m) throw new RangeError(\"bad date\");\n  return new Date(Number(m[1]), Number(m[2]) - 1, Number(m[3]));\n}\n";
        let dates = format!("{strict}export function formatDate(value: string): string {{\n  return parseIso(value.slice(0, 10)).toDateString();\n}}\n");
        let velocity = format!("{strict}export function age(t: {{ createdAt: string }}) {{\n  return parseIso(t.createdAt).getTime();\n}}\n");
        let seed = "export const tasks = [\n  { id: \"t1\", title: \"a\", createdAt: new Date().toISOString() },\n];\n";
        let detail = "import { formatDate } from \"../lib/dates\";\nfunction parseIso(v: string) { return new Date(v); }\nexport function Detail({ t }) {\n  return <p>{formatDate(t.dueDate)} {parseIso(t.createdAt).getTime()}</p>;\n}\n";
        let files = [
            ("lib/dates.ts", dates.as_str()),
            ("lib/velocity.ts", velocity.as_str()),
            ("lib/seed.ts", seed),
            ("components/Detail.tsx", detail),
        ];
        let lints = conventions(&files, "date-format-mismatch");
        assert_eq!(lints.len(), 1, "{lints:?}");
        assert_eq!(lints[0].path, "lib/velocity.ts");
        assert!(
            lints[0].message.contains("lib/velocity.ts:8"),
            "{}",
            lints[0].message
        );
    }

    #[test]
    fn transition_timestamps_clears_by_the_declared_type() {
        let types = "export interface Task {\n  id: string;\n  status: \"todo\" | \"done\";\n  completedAt?: string;\n}\n";
        let board = "import { Task } from \"./types\";\nexport function moveTask(tasks: Task[], id: string, status: Task[\"status\"]): Task[] {\n  return tasks.map((t) => (t.id === id ? { ...t, status } : t));\n}\n";
        let page = "import { moveTask } from \"../../lib/board\";\nexport default function Board() {\n  moveTask([], \"a\", \"done\");\n  return <div />;\n}\n";
        let files = [
            ("lib/types.ts", types),
            ("lib/board.ts", board),
            ("app/board/page.tsx", page),
        ];
        let lints = conventions(&files, "transition-timestamps");
        assert_eq!(lints.len(), 1, "{lints:?}");
        assert!(
            lints[0].message.contains("to undefined"),
            "{}",
            lints[0].message
        );
        assert!(!lints[0].message.contains("to null"));
    }

    /// Run 20261115-083658: a props-only table with a fixed 'USD'.
    #[test]
    fn hardcoded_currency_words_the_fix_by_file_kind() {
        let types = "export interface Settings {\n  currency: string;\n}\n";
        let table = "import { formatMoney } from \"../lib/money\";\nexport function Table({ cents }) {\n  return <td>{formatMoney(cents, \"USD\")}</td>;\n}\n";
        let page = "import { Table } from \"../../components/Table\";\nexport default function Spend() {\n  return <Table cents={1} />;\n}\n";
        let files = [
            ("lib/types.ts", types),
            ("components/Table.tsx", table),
            ("app/spend/page.tsx", page),
        ];
        let lints = conventions(&files, "hardcoded-currency");
        assert_eq!(lints.len(), 1, "{lints:?}");
        assert!(
            lints[0].message.contains("take it as an optional prop"),
            "{}",
            lints[0].message
        );
        assert!(lints[0]
            .message
            .contains("`app/spend/page.tsx` then passes"));
        assert!(!lints[0].message.contains("store"));
        // After the component fix: the page that passes no currency.
        let fixed = "import { formatMoney } from \"../lib/money\";\nexport function Table({ cents, currency }) {\n  return <td>{formatMoney(cents, currency ?? \"USD\")}</td>;\n}\n";
        let files = [
            ("lib/types.ts", types),
            ("components/Table.tsx", fixed),
            ("app/spend/page.tsx", page),
        ];
        let lints = conventions(&files, "hardcoded-currency");
        assert_eq!(lints.len(), 1, "{lints:?}");
        assert_eq!(
            (lints[0].path.as_str(), lints[0].line),
            ("app/spend/page.tsx", 3)
        );
        let passes = page.replace("cents={1}", "cents={1} currency={settings.currency}");
        let files = [
            ("lib/types.ts", types),
            ("components/Table.tsx", fixed),
            ("app/spend/page.tsx", passes.as_str()),
        ];
        assert!(conventions(&files, "hardcoded-currency").is_empty());
        // A page with a store hook reads it there.
        let direct = "import { useStore } from \"../../components/StoreProvider\";\nimport { formatMoney } from \"../../lib/money\";\nexport default function Spend() {\n  const { state } = useStore();\n  return <p>{formatMoney(1, \"USD\")}</p>;\n}\n";
        let files = [
            ("lib/types.ts", types),
            ("components/StoreProvider.tsx", PROVIDER),
            ("app/spend/page.tsx", direct),
        ];
        let lints = conventions(&files, "hardcoded-currency");
        assert_eq!(lints.len(), 1, "{lints:?}");
        assert!(
            lints[0].message.contains("`useStore().settings.currency`"),
            "{}",
            lints[0].message
        );
    }

    #[test]
    fn qlint2_names_follow_the_lints() {
        assert_eq!(NAMES.len(), 10, "the qlint set is unchanged");
        assert_eq!(NAMES2.len(), 14);
        assert!(NAMES2.iter().all(|name| !NAMES.contains(name)));
    }

    // ---- qlint2, audit 3 ---------------------------------------------------

    /// A store like audit 3's: the seed first, storage read in an effect.
    const SEED_STORE: &str = "\"use client\";\nimport { createContext, useCallback, useContext, useEffect, useState } from \"react\";\nimport { loadState } from \"./storage\";\nimport { seedState } from \"./seed\";\nconst Ctx = createContext(null);\nexport function StoreProvider({ children }) {\n  const [state, setStateValue] = useState(seedState);\n  useEffect(() => {\n    const stored = loadState();\n    if (stored) setStateValue(stored);\n  }, []);\n  const setState = useCallback((updater) => {\n    setStateValue((prev) => updater(prev));\n  }, []);\n  return <Ctx.Provider value={{ state, setState }}>{children}</Ctx.Provider>;\n}\nexport function useStore() {\n  return useContext(Ctx);\n}\n";

    /// [`SEED_STORE`] with a `ready` flag.
    fn ready_store() -> String {
        SEED_STORE
            .replace(
                "  useEffect(() => {\n    const stored = loadState();\n    if (stored) setStateValue(stored);\n",
                "  const [ready, setReady] = useState(false);\n  useEffect(() => {\n    const stored = loadState();\n    if (stored) setStateValue(stored);\n    setReady(true);\n",
            )
            .replace("value={{ state, setState }}", "value={{ state, setState, ready }}")
    }

    const DETAIL: &str = "\"use client\";\nimport { useState } from \"react\";\nimport { useParams, notFound } from \"next/navigation\";\nimport { useStore } from \"../../../lib/store\";\nfunction TaskForm({ task, onSave }) {\n  const [title, setTitle] = useState(task.title);\n  return <form onSubmit={() => onSave(title)}><input value={title} onChange={(e) => setTitle(e.target.value)} /></form>;\n}\nexport default function TaskPage() {\n  const params = useParams();\n  const id = Array.isArray(params.id) ? params.id[0] : params.id;\n  const { state, setState } = useStore();\n  const task = state.tasks.find((t) => t.id === id);\n  if (!task) {\n    notFound();\n  }\n  const project = state.projects.find((p) => p.id === task.projectId);\n  return <TaskForm task={task} onSave={(title) => setState((s) => s)} />;\n}\n";

    fn ready_files(store: &str, page: &str) -> Vec<Lint> {
        let files = [
            ("lib/storage.ts", STORAGE),
            ("lib/store.tsx", store),
            ("app/tasks/[id]/page.tsx", page),
        ];
        conventions(&files, "render-before-ready")
    }

    /// Audit 3 (H1): a 404 for a task the user created, a form with the
    /// seed values.
    #[test]
    fn render_before_ready_names_the_page_the_form_and_the_store() {
        let lints = ready_files(SEED_STORE, DETAIL);
        let at: Vec<(&str, usize)> = lints
            .iter()
            .map(|lint| (lint.path.as_str(), lint.line))
            .collect();
        assert_eq!(
            at,
            vec![
                ("app/tasks/[id]/page.tsx", 15),
                ("app/tasks/[id]/page.tsx", 6),
                ("lib/store.tsx", 6)
            ],
            "{lints:?}"
        );
        assert!(lints[0]
            .message
            .contains("calls notFound() when `task` is missing"));
        assert!(lints[0]
            .message
            .contains("`lib/store.tsx` has no such flag yet"));
        assert!(lints[1]
            .message
            .contains("<TaskForm> copies the fields of `task`"));
        assert!(lints[1].message.contains("`key={task.id + String(ready)}`"));
        assert!(lints[2].message.contains("Add `ready`"));
        // A store with the flag: the store is fine, the page still waits for
        // nothing.
        let lints = ready_files(&ready_store(), DETAIL);
        assert_eq!(lints.len(), 2, "{lints:?}");
        assert!(lints
            .iter()
            .all(|lint| lint.path == "app/tasks/[id]/page.tsx"));
        assert!(lints[0].message.contains("read `ready` from `useStore()`"));
        // The provider gate comes first in every fix.
        assert!(lints.iter().all(|lint| lint
            .message
            .contains("make `lib/store.tsx` render a loading state instead of its children")));
        // The fix: a loading state until ready, then notFound(); the form
        // keyed by the id and the flag.
        let fixed = DETAIL
            .replace(
                "const { state, setState } = useStore();",
                "const { state, setState, ready } = useStore();",
            )
            .replace(
                "  if (!task) {\n    notFound();\n  }\n",
                "  if (!ready) return <p>Loading</p>;\n  if (!task) {\n    notFound();\n  }\n",
            );
        assert!(ready_files(&ready_store(), &fixed).is_empty());
        let keyed = DETAIL
            .replace(
                "const { state, setState } = useStore();",
                "const { state, setState, ready } = useStore();",
            )
            .replace("if (!task) {", "if (ready && !task) {")
            .replace(
                "<TaskForm task={task}",
                "<TaskForm key={`${task.id}-${ready}`} task={task}",
            );
        assert!(ready_files(&ready_store(), &keyed).is_empty());
        // A resync effect on the record is no fix any more (it discards the
        // drafts on every change: `draft-resync`), and no fix is advised.
        let resync = DETAIL.replace(
            "  const [title, setTitle] = useState(task.title);\n",
            "  const [title, setTitle] = useState(task.title);\n  useEffect(() => setTitle(task.title), [task]);\n",
        );
        let lints = ready_files(&ready_store(), &resync);
        assert_eq!(lints.len(), 2, "{lints:?}");
        assert!(lints[0].message.contains("notFound()"));
        assert!(lints[1]
            .message
            .contains("never re-set its fields in a useEffect on `task`"));
        assert!(lints
            .iter()
            .all(|lint| !lint.message.contains("re-initialise")));
        // An effect on the flag copies the stored record once it was read.
        let on_flag = keyed
            .replace("<TaskForm key={`${task.id}-${ready}`} task={task}", "<TaskForm task={task}")
            .replace(
                "  const [title, setTitle] = useState(task.title);\n",
                "  const [title, setTitle] = useState(task.title);\n  useEffect(() => setTitle(task.title), [ready]);\n",
            );
        assert!(ready_files(&ready_store(), &on_flag).is_empty());
    }

    /// Audit 5: with the provider gate, every page under it is fine, the form
    /// keyed by the record id alone included.
    #[test]
    fn render_before_ready_accepts_the_provider_gate_and_a_key_by_id() {
        let gated = ready_store().replace(
            "  return <Ctx.Provider",
            "  if (!ready) return <p>Loading</p>;\n  return <Ctx.Provider",
        );
        let by_id = DETAIL.replace(
            "<TaskForm task={task}",
            "<TaskForm key={task.id} task={task}",
        );
        assert!(ready_files(&gated, &by_id).is_empty());
        assert!(ready_files(&gated, DETAIL).is_empty());
        let branch = ready_store().replace(
            "<Ctx.Provider value={{ state, setState, ready }}>{children}</Ctx.Provider>",
            "<Ctx.Provider value={{ state, setState, ready }}>{ready ? children : <p>Loading</p>}</Ctx.Provider>",
        );
        assert_ne!(branch, ready_store());
        assert!(ready_files(&branch, &by_id).is_empty());
        // Review: the other shapes of the gate (a negated ternary, a ternary
        // around the provider, `props.children`) are the same fix.
        let provider =
            "  return <Ctx.Provider value={{ state, setState, ready }}>{children}</Ctx.Provider>;";
        assert!(ready_store().contains(provider));
        let props = ready_store()
            .replace("StoreProvider({ children })", "StoreProvider(props)")
            .replace(
                "{children}</Ctx",
                "{ready ? props.children : <p>Loading</p>}</Ctx",
            );
        for gate in [
            ready_store().replace("{children}</Ctx", "{!ready ? <p>Loading</p> : children}</Ctx"),
            ready_store().replace(
                provider,
                "  return ready ? (\n    <Ctx.Provider value={{ state, setState, ready }}>{children}</Ctx.Provider>\n  ) : (\n    <p>Loading</p>\n  );",
            ),
            props.clone(),
            props.replace(
                "{ready ? props.children : <p>Loading</p>}",
                "{!ready ? <p>Loading</p> : props.children}",
            ),
        ] {
            assert_ne!(gate, ready_store());
            assert!(ready_files(&gate, &by_id).is_empty(), "{gate}");
            let files = [
                ("lib/storage.ts", STORAGE),
                ("lib/store.tsx", gate.as_str()),
                ("app/tasks/[id]/page.tsx", DETAIL),
            ];
            assert!(conventions_planned(&files, "ungated-store").is_empty(), "{gate}");
        }
        // Not a gate: the children rendered next to a branch on the flag.
        assert!(!gates_children(
            "  return <Ctx.Provider value={v}>{ready && <Banner />}{children}</Ctx.Provider>;"
        ));
        assert!(!gates_children(
            "  const label = ready ? 'a' : 'b';\n  return <Ctx.Provider value={v}>{props.children}</Ctx.Provider>;"
        ));
        // Without the gate the key by id alone is no fix: a seed record keeps
        // its id.
        let lints = ready_files(&ready_store(), &by_id);
        assert!(
            lints
                .iter()
                .any(|lint| lint.message.contains("<TaskForm> copies the fields")),
            "{lints:?}"
        );
    }

    #[test]
    fn render_before_ready_skips_a_provider_that_waits_and_a_page_that_waits() {
        // The provider renders a loading state until the stored state is read.
        let gated = SEED_STORE
            .replace("useState(seedState)", "useState(null)")
            .replace(
                "  return <Ctx.Provider",
                "  if (!state) {\n    return <p>Loading</p>;\n  }\n  return <Ctx.Provider",
            );
        assert!(ready_files(&gated, DETAIL).is_empty());
        // A layout component that shows the pages once the store is ready.
        let shell = "import { useStore } from \"../lib/store\";\nexport function Shell({ children }) {\n  const { ready } = useStore();\n  return <main>{ready ? children : <p>Loading</p>}</main>;\n}\n";
        let store = ready_store();
        let files = [
            ("lib/storage.ts", STORAGE),
            ("lib/store.tsx", store.as_str()),
            ("components/Shell.tsx", shell),
            ("app/tasks/[id]/page.tsx", DETAIL),
        ];
        assert!(conventions(&files, "render-before-ready").is_empty());
        // No store: a page that loads in an effect and waits for its null state.
        let page = "\"use client\";\nimport { useEffect, useState } from \"react\";\nimport { useParams, notFound } from \"next/navigation\";\nimport { loadState } from \"../../../lib/storage\";\nfunction TaskForm({ task }) {\n  const [title, setTitle] = useState(task.title);\n  return <input value={title} onChange={(e) => setTitle(e.target.value)} />;\n}\nexport default function TaskPage() {\n  const params = useParams();\n  const [state, setState] = useState(null);\n  useEffect(() => {\n    setState(loadState());\n  }, []);\n  if (state === null) {\n    return <p>Loading</p>;\n  }\n  const task = state.tasks.find((t) => t.id === params.id);\n  if (!task) notFound();\n  return <TaskForm task={task} />;\n}\n";
        let files = [
            ("lib/storage.ts", STORAGE),
            ("app/tasks/[id]/page.tsx", page),
        ];
        assert!(conventions(&files, "render-before-ready").is_empty());
        // The same page with a seed first render: the page itself.
        let seeded = page
            .replace("useState(null)", "useState(seedState())")
            .replace(
                "  if (state === null) {\n    return <p>Loading</p>;\n  }\n",
                "",
            );
        let files = [
            ("lib/storage.ts", STORAGE),
            ("app/tasks/[id]/page.tsx", seeded.as_str()),
        ];
        let lints = conventions(&files, "render-before-ready");
        assert_eq!(lints.len(), 2, "{lints:?}");
        assert!(lints[0].message.contains("Keep a `ready` flag"));
        // A record that no route parameter names is not this lint's.
        let local = DETAIL.replace("(t) => t.id === id", "(t) => t.done");
        assert!(ready_files(SEED_STORE, &local).is_empty());
    }

    /// Review: a store that starts null or empty, an inverse `loading` flag,
    /// a resync on the id only, a flag named elsewhere in the page.
    #[test]
    fn render_before_ready_reads_the_first_render_and_the_flag() {
        assert!(seeds("seedState") && seeds("() => freshState()") && seeds("loadState()"));
        assert!(seeds("[{ id: 't1' }]"));
        assert!(!seeds("null") && !seeds("{ tasks: [], labels: [] }") && !seeds("false"));
        assert!(!seeds("() => ({ tasks: [] })") && !seeds("''"));
        // A null first render, and a page that waits for the store's state.
        let null_store = SEED_STORE.replace("useState(seedState)", "useState(null)");
        let waits = DETAIL.replace(
            "  const task = state.tasks.find",
            "  if (!state) return <p>Loading</p>;\n  const task = state.tasks.find",
        );
        assert!(ready_files(&null_store, &waits).is_empty());
        // The same page without the wait: the 404 stays, the form does not
        // (it mounts once a record is there).
        let lints = ready_files(&null_store, DETAIL);
        assert_eq!(lints.len(), 2, "{lints:?}");
        assert!(lints[0].message.contains("holds an empty state"));
        assert!(lints[1].message.contains("first render is an empty state"));
        // An empty first render and a page with its own 'not found' text.
        let empty_store = SEED_STORE.replace("useState(seedState)", "useState({ tasks: [] })");
        let text = DETAIL.replace("    notFound();\n", "    return <p>Task not found</p>;\n");
        assert!(ready_files(&empty_store, &text).is_empty());
        // `loading`: true until the load.
        let loading_store = ready_store()
            .replace(
                "[ready, setReady] = useState(false)",
                "[loading, setLoading] = useState(true)",
            )
            .replace("setReady(true)", "setLoading(false)")
            .replace("setState, ready }}", "setState, loading }}");
        let loading_page = DETAIL
            .replace(
                "const { state, setState } = useStore();",
                "const { state, setState, loading } = useStore();",
            )
            .replace(
                "  if (!task) {\n    notFound();\n  }\n",
                "  if (loading) return <p>Loading</p>;\n  if (!task) {\n    notFound();\n  }\n",
            );
        assert!(ready_files(&loading_store, &loading_page).is_empty());
        // `if (!loading) return ...` waits for nothing.
        let wrong = loading_page.replace("if (loading) return", "if (!loading) return");
        let lints = ready_files(&loading_store, &wrong);
        assert!(
            lints.iter().any(|lint| lint.message.contains("notFound()")
                && lint.message.contains("while `loading` is true")),
            "{lints:?}"
        );
        // A resync on the id only is not a fix: a seed record keeps its id.
        let page = DETAIL
            .replace(
                "const { state, setState } = useStore();",
                "const { state, setState, ready } = useStore();",
            )
            .replace("if (!task) {", "if (ready && !task) {");
        let by_id = page.replace(
            "  const [title, setTitle] = useState(task.title);\n",
            "  const [title, setTitle] = useState(task.title);\n  useEffect(() => setTitle(task.title), [task.id]);\n",
        );
        let lints = ready_files(&ready_store(), &by_id);
        assert_eq!(lints.len(), 1, "{lints:?}");
        assert!(lints[0].message.contains("`key={task.id + String(ready)}`"));
        // A field of the record is no fix either; the flag is.
        let by_field = by_id.replace("[task.id]", "[task.id, task.title]");
        assert_eq!(ready_files(&ready_store(), &by_field).len(), 1);
        let by_flag = by_id.replace("[task.id]", "[task.id, ready]");
        assert!(ready_files(&ready_store(), &by_flag).is_empty());
        // A flag named elsewhere (a theme's `mounted`) does not excuse the 404.
        let mounted = DETAIL.replace(
            "  const task = state.tasks.find",
            "  const [mounted, setMounted] = useState(false);\n  const label = mounted ? \"x\" : \"y\";\n  const task = state.tasks.find",
        );
        let lints = ready_files(SEED_STORE, &mounted);
        assert!(
            lints.iter().any(|lint| lint.message.contains("notFound()")),
            "{lints:?}"
        );
    }

    // ---- qlint2, audit 5 ---------------------------------------------------

    fn conventions_planned(files: &[(&str, &str)], name: &str) -> Vec<Lint> {
        let files = app(files);
        let all = lint_app_with(&files, false, true, true);
        assert!(all.iter().all(|lint| NAMES2.contains(&lint.name)));
        named(&all, name)
    }

    /// A detail page like audit 5's: the drafts re-set in an effect on the
    /// record, so a status change on the same page discards them.
    const RESYNC_PAGE: &str = "\"use client\";\nimport { useEffect, useState } from \"react\";\nimport { useParams } from \"next/navigation\";\nimport { useStore } from \"../../../lib/store\";\nexport default function TaskPage() {\n  const params = useParams();\n  const { state, setState, ready } = useStore();\n  const task = state.tasks.find((t) => t.id === params.id);\n  const [title, setTitle] = useState(\"\");\n  const [dueDate, setDueDate] = useState(\"\");\n  useEffect(() => {\n    if (!task) return;\n    setTitle(task.title);\n    setDueDate(task.dueDate);\n  }, [task]);\n  if (!ready) return <p>Loading</p>;\n  if (!task) return <p>Not found</p>;\n  return <form><input value={title} onChange={(e) => setTitle(e.target.value)} /><input value={dueDate} onChange={(e) => setDueDate(e.target.value)} /></form>;\n}\n";

    #[test]
    fn draft_resync_names_the_effect_on_the_record() {
        let gated = ready_store().replace(
            "  return <Ctx.Provider",
            "  if (!ready) return <p>Loading</p>;\n  return <Ctx.Provider",
        );
        let lint = |store: &str, page: &str| {
            let files = [
                ("lib/storage.ts", STORAGE),
                ("lib/store.tsx", store),
                ("app/tasks/[id]/page.tsx", page),
            ];
            conventions(&files, "draft-resync")
        };
        let lints = lint(&gated, RESYNC_PAGE);
        assert_eq!(lints.len(), 1, "{lints:?}");
        assert_eq!(
            (lints[0].path.as_str(), lints[0].line, lints[0].code),
            ("app/tasks/[id]/page.tsx", 11, "TS9030")
        );
        let message = &lints[0].message;
        assert!(
            message.contains(
                "<TaskPage> keeps the fields of `task` in form drafts (`title`, `dueDate`)"
            ),
            "{message}"
        );
        assert!(
            message.contains("move the form into a component of its own"),
            "{message}"
        );
        assert!(message.contains("`key={task.id}`"), "{message}");
        assert!(!message.contains("also make"), "the store gates: {message}");
        assert!(
            message.contains("An effect on `task.id` alone is fine."),
            "{message}"
        );
        // An ungated seed store: the gate too, and no effect on the id (a
        // seed record keeps its id).
        let lints = lint(&ready_store(), RESYNC_PAGE);
        let message = &lints[0].message;
        assert!(
            message.contains("also make `lib/store.tsx` render a loading state"),
            "{message}"
        );
        assert!(message.contains("An effect on `task.id` alone is no fix"));
        assert!(!message.contains("alone is fine"), "{message}");
        // Negative: an effect on the id only, a one-shot copy, a dependency
        // that only names the record in an expression, drafts bound to no
        // control.
        for page in [
            RESYNC_PAGE.replace("}, [task]);", "}, [task?.id]);"),
            RESYNC_PAGE.replace("}, [task]);", "}, [task ? task.id : '']);"),
            RESYNC_PAGE
                .replace("  const [dueDate, setDueDate] = useState(\"\");\n", "  const [dueDate, setDueDate] = useState(\"\");\n  const [init, setInit] = useState(false);\n")
                .replace("    if (!task) return;\n", "    if (!task || init) return;\n    setInit(true);\n"),
            RESYNC_PAGE.replace("value={title}", "defaultValue=\"\"").replace("value={dueDate}", "defaultValue=\"\""),
        ] {
            assert!(lint(&gated, &page).is_empty(), "{page}");
        }
        // The fix: a component of its own, keyed by the id, no effect.
        let fixed = "\"use client\";\nimport { useState } from \"react\";\nimport { useParams } from \"next/navigation\";\nimport { useStore } from \"../../../lib/store\";\nfunction TaskForm({ task }) {\n  const [title, setTitle] = useState(task.title);\n  return <input value={title} onChange={(e) => setTitle(e.target.value)} />;\n}\nexport default function TaskPage() {\n  const params = useParams();\n  const { state } = useStore();\n  const task = state.tasks.find((t) => t.id === params.id);\n  if (!task) return <p>Not found</p>;\n  return <TaskForm key={task.id} task={task} />;\n}\n";
        assert!(lint(&gated, fixed).is_empty());
        let files = [
            ("lib/storage.ts", STORAGE),
            ("lib/store.tsx", gated.as_str()),
            ("app/tasks/[id]/page.tsx", fixed),
        ];
        assert!(conventions(&files, "render-before-ready").is_empty());
        // A form component with the record as a prop: key it where it is
        // rendered.
        let form = "import { useEffect, useState } from \"react\";\nexport function TaskForm({ task }) {\n  const [title, setTitle] = useState(task.title);\n  useEffect(() => {\n    setTitle(task.title);\n  }, [task]);\n  return <input value={title} onChange={(e) => setTitle(e.target.value)} />;\n}\n";
        let page = fixed
            .replace("import { useState } from \"react\";\n", "import { TaskForm } from \"../../../components/TaskForm\";\n")
            .replace("function TaskForm({ task }) {\n  const [title, setTitle] = useState(task.title);\n  return <input value={title} onChange={(e) => setTitle(e.target.value)} />;\n}\n", "")
            .replace("<TaskForm key={task.id} task={task} />", "<TaskForm task={task} />");
        let files = [
            ("lib/storage.ts", STORAGE),
            ("lib/store.tsx", gated.as_str()),
            ("components/TaskForm.tsx", form),
            ("app/tasks/[id]/page.tsx", page.as_str()),
        ];
        let lints = conventions(&files, "draft-resync");
        assert_eq!(lints.len(), 1, "{lints:?}");
        assert_eq!(lints[0].path, "components/TaskForm.tsx");
        assert!(lints[0]
            .message
            .contains("render <TaskForm> with `key={task.id}` (in `app/tasks/[id]/page.tsx`)"));
        // Review: props destructured in the body are props too, and a record
        // that may be missing is keyed with a fallback.
        let in_body = form.replace(
            "TaskForm({ task }) {\n",
            "TaskForm(props) {\n  const { task, onSave } = props;\n",
        );
        let nullable_form = in_body
            .replace("useState(task.title)", "useState(task ? task.title : \"\")")
            .replace(
                "setTitle(task.title);",
                "setTitle(task ? task.title : \"\");",
            );
        for (form, key) in [
            (
                in_body.as_str(),
                "`key={task.id}` (in `app/tasks/[id]/page.tsx`)",
            ),
            (
                nullable_form.as_str(),
                "`key={task?.id ?? 'new'}` (in `app/tasks/[id]/page.tsx`)",
            ),
        ] {
            let files = [
                ("lib/storage.ts", STORAGE),
                ("lib/store.tsx", gated.as_str()),
                ("components/TaskForm.tsx", form),
                ("app/tasks/[id]/page.tsx", page.as_str()),
            ];
            let lints = conventions(&files, "draft-resync");
            assert_eq!(lints.len(), 1, "{lints:?}");
            assert!(lints[0].message.contains(key), "{}", lints[0].message);
            assert!(!lints[0].message.contains("move the form"));
        }
        // An object without an id (the settings): nothing to key by.
        let settings = "\"use client\";\nimport { useEffect, useState } from \"react\";\nimport { useStore } from \"../../lib/store\";\nexport default function SettingsPage() {\n  const { state } = useStore();\n  const settings = state.settings;\n  const [currency, setCurrency] = useState(settings.currency);\n  useEffect(() => {\n    setCurrency(settings.currency);\n  }, [settings]);\n  return <input value={currency} onChange={(e) => setCurrency(e.target.value)} />;\n}\n";
        let files = [
            ("lib/storage.ts", STORAGE),
            ("lib/store.tsx", &ready_store()),
            ("app/settings/page.tsx", settings),
        ];
        assert!(conventions(&files, "draft-resync").is_empty());
        // A record that may be missing, not a word of a string.
        assert!(nullable("setTitle(task ? task.title : '')", "task"));
        assert!(nullable("if (!task) return null;", "task"));
        assert!(!nullable(
            "confirm(\"Delete this task?\");
  setTitle(task.title);",
            "task"
        ));
    }

    #[test]
    fn ungated_store_names_the_provider_with_what_the_seed_breaks() {
        let lint = |store: &str, page: &str| {
            let files = [
                ("lib/storage.ts", STORAGE),
                ("lib/store.tsx", store),
                ("app/tasks/[id]/page.tsx", page),
            ];
            conventions_planned(&files, "ungated-store")
        };
        let lints = lint(&ready_store(), DETAIL);
        assert_eq!(lints.len(), 1, "{lints:?}");
        assert_eq!(
            (lints[0].path.as_str(), lints[0].line, lints[0].code),
            ("lib/store.tsx", 6, "TS9031")
        );
        assert!(lints[0]
            .message
            .contains("`app/tasks/[id]/page.tsx` looks a record up by a route or query parameter"));
        assert!(lints[0]
            .message
            .contains("using `ready`, which `useStore()` already returns"));
        // It replaces TS9026's line on the store file (no flag).
        let files = app(&[
            ("lib/storage.ts", STORAGE),
            ("lib/store.tsx", SEED_STORE),
            ("app/tasks/[id]/page.tsx", DETAIL),
        ]);
        let all = lint_app_with(&files, false, true, true);
        let on_store: Vec<&str> = all
            .iter()
            .filter(|lint| lint.path == "lib/store.tsx")
            .map(|lint| lint.name)
            .collect();
        assert_eq!(on_store, vec!["ungated-store"], "{all:?}");
        assert!(all
            .iter()
            .find(|lint| lint.name == "ungated-store")
            .is_some_and(|lint| lint
                .message
                .contains("a `ready` flag (`const [ready, setReady]")));
        // An effect that picks a seed id once.
        let board = "\"use client\";\nimport { useEffect, useState } from \"react\";\nimport { useStore } from \"../../lib/store\";\nexport default function Board() {\n  const { state } = useStore();\n  const [projectId, setProjectId] = useState(\"\");\n  useEffect(() => {\n    if (projectId !== \"\") return;\n    setProjectId(state.settings.defaultProjectId);\n  }, [projectId, state.settings.defaultProjectId]);\n  return <p>{projectId}</p>;\n}\n";
        let files = [
            ("lib/storage.ts", STORAGE),
            ("lib/store.tsx", &ready_store()),
            ("app/board/page.tsx", board),
        ];
        let lints = conventions_planned(&files, "ungated-store");
        assert_eq!(lints.len(), 1, "{lints:?}");
        assert!(lints[0]
            .message
            .contains("the effect at `app/board/page.tsx:7`"));
        // Negative: an effect that re-runs on the stored state, a gated
        // provider, a store that starts empty, an app planned without
        // `qsemantic`.
        let rerun = board.replace("    if (projectId !== \"\") return;\n", "");
        let files = [
            ("lib/storage.ts", STORAGE),
            ("lib/store.tsx", &ready_store()),
            ("app/board/page.tsx", rerun.as_str()),
        ];
        assert!(conventions_planned(&files, "ungated-store").is_empty());
        // Review: a default pick that only fills an empty value is weak
        // evidence (nothing else breaks: silent).
        let pick = board
            .replace("projectId", "author")
            .replace("setProjectId", "setAuthor")
            .replace(
                "    if (author !== \"\") return;\n    setAuthor(state.settings.defaultProjectId);\n",
                "    if (author === \"\" && state.members.length > 0) {\n      setAuthor(state.members[0].id);\n    }\n",
            )
            .replace("state.settings.defaultProjectId]", "state.members]");
        assert!(pick.contains("setAuthor(state.members[0].id)"), "{pick}");
        let files = [
            ("lib/storage.ts", STORAGE),
            ("lib/store.tsx", &ready_store()),
            ("app/board/page.tsx", pick.as_str()),
        ];
        assert!(conventions_planned(&files, "ungated-store").is_empty());
        let gated = ready_store().replace(
            "  return <Ctx.Provider",
            "  if (!ready) return <p>Loading</p>;\n  return <Ctx.Provider",
        );
        assert!(lint(&gated, DETAIL).is_empty());
        let branch = ready_store().replace("}>{children}</Ctx", "}>{ready ? children : null}</Ctx");
        assert!(lint(&branch, DETAIL).is_empty());
        assert!(lint(
            &SEED_STORE.replace("useState(seedState)", "useState(null)"),
            DETAIL
        )
        .is_empty());
        let files = [
            ("lib/storage.ts", STORAGE),
            ("lib/store.tsx", SEED_STORE),
            ("app/tasks/[id]/page.tsx", DETAIL),
        ];
        assert!(conventions(&files, "ungated-store").is_empty());
    }

    const DATES: &str = "const DATE_RE = /^(\\d{4})-(\\d{2})-(\\d{2})$/;\nfunction parseDate(value) {\n  const m = DATE_RE.exec(value);\n  if (!m) throw new RangeError(`Invalid date: ${value}`);\n  return new Date(Number(m[1]), Number(m[2]) - 1, Number(m[3]));\n}\nexport function formatDate(value) {\n  const d = parseDate(value);\n  return d.toDateString();\n}\n";

    #[test]
    fn empty_format_names_the_formatter_that_throws_on_an_empty_field() {
        let card = "import { formatDate } from \"../lib/dates\";\nexport function TaskCard({ task }) {\n  return <span>{formatDate(task.dueDate)}</span>;\n}\n";
        let detail = "import { formatDate } from \"../../../lib/dates\";\nexport default function TaskPage({ task }) {\n  return <p>Due: {task.dueDate ? formatDate(task.dueDate) : \"-\"}</p>;\n}\n";
        let lint = |dates: &str, card: &str, detail: &str| {
            let files = [
                ("lib/dates.ts", dates),
                ("components/TaskCard.tsx", card),
                ("app/tasks/[id]/page.tsx", detail),
            ];
            conventions(&files, "empty-format")
        };
        let lints = lint(DATES, card, detail);
        assert_eq!(lints.len(), 1, "{lints:?}");
        assert_eq!(
            (lints[0].path.as_str(), lints[0].line, lints[0].code),
            ("lib/dates.ts", 7, "TS9032")
        );
        let message = &lints[0].message;
        assert!(
            message.contains(
                "`components/TaskCard.tsx:3` calls it while rendering with `task.dueDate`"
            ),
            "{message}"
        );
        assert!(
            message.contains("`app/tasks/[id]/page.tsx:3` shows it only when it is set"),
            "{message}"
        );
        assert!(message.contains("`if (!value) return '';`"), "{message}");
        // An optional field of the types file is evidence too.
        let types = "export interface Task { id: string; dueDate?: string }\n";
        let files = [
            ("lib/types.ts", types),
            ("lib/dates.ts", DATES),
            ("components/TaskCard.tsx", card),
        ];
        assert_eq!(conventions(&files, "empty-format").len(), 1);
        // The fix: the formatter returns '' for an empty value.
        let fixed = DATES.replace(
            "export function formatDate(value) {\n",
            "export function formatDate(value) {\n  if (!value) return '';\n",
        );
        assert!(lint(&fixed, card, detail).is_empty());
        // Review: the other early returns for an empty value, and a parse
        // that runs only for a non-empty value.
        for first in [
            "  if (!value?.trim()) return '';\n",
            "  if (!DATE_RE.test(value)) return '';\n",
            "  if (value == null || value === '') return '';\n",
        ] {
            let fixed = DATES.replace(
                "export function formatDate(value) {\n",
                &format!("export function formatDate(value) {{\n{first}"),
            );
            assert!(lint(&fixed, card, detail).is_empty(), "{fixed}");
        }
        let guarded_parse = DATES.replace(
            "  const d = parseDate(value);\n  return d.toDateString();\n",
            "  const d = value ? parseDate(value) : null;\n  if (!d) return '';\n  return d.toDateString();\n",
        );
        assert_ne!(guarded_parse, DATES);
        assert!(lint(&guarded_parse, card, detail).is_empty());
        // A test of something else is no fix.
        let other = DATES.replace(
            "export function formatDate(value) {\n",
            "export function formatDate(value) {\n  if (!ENABLED) return '';\n",
        );
        assert_eq!(lint(&other, card, detail).len(), 1);
        // Negative: every call guarded, no evidence that the field is
        // optional, a comparison is no evidence, a formatter that never throws.
        let guarded = card.replace(
            "{formatDate(task.dueDate)}",
            "{task.dueDate && formatDate(task.dueDate)}",
        );
        assert!(lint(DATES, &guarded, detail).is_empty());
        let plain = detail.replace(
            "task.dueDate ? formatDate(task.dueDate) : \"-\"",
            "formatDate(task.dueDate)",
        );
        assert!(lint(DATES, card, &plain).is_empty());
        let compared = "import { formatDate } from \"../../../lib/dates\";\nexport default function TaskPage({ task, today }) {\n  if (today > task.dueDate) {\n    return <p>Late: {formatDate(task.dueDate)}</p>;\n  }\n  return null;\n}\n";
        assert!(lint(DATES, card, compared).is_empty());
        let lenient =
            "export function formatDate(value) {\n  return new Date(value).toDateString();\n}\n";
        assert!(lint(lenient, card, detail).is_empty());
    }

    const TASKS_LIB: &str = "export function addTask(tasks, input) {\n  if (!input.title.trim()) throw new Error(\"title required\");\n  return [...tasks, input];\n}\nexport function listTasks(tasks) {\n  return tasks;\n}\n";

    /// Audit 3 (H2): a validation error thrown inside the updater.
    #[test]
    fn throw_in_updater_names_the_call_inside_the_setter() {
        let board = "\"use client\";\nimport { useState } from \"react\";\nimport { useStore } from \"../../lib/store\";\nimport { addTask, listTasks } from \"../../lib/tasks\";\nexport default function Board() {\n  const { state, setState } = useStore();\n  const [error, setError] = useState(\"\");\n  function create(title) {\n    try {\n      setState((s) => ({ ...s, tasks: addTask(s.tasks, { title }) }));\n      setError(\"\");\n    } catch (err) {\n      setError(String(err));\n    }\n  }\n  return <button onClick={() => create(\"\")}>{listTasks(state.tasks).length}</button>;\n}\n";
        let lint = |page: &str| {
            let files = [
                ("lib/storage.ts", STORAGE),
                ("lib/store.tsx", SEED_STORE),
                ("lib/tasks.ts", TASKS_LIB),
                ("app/board/page.tsx", page),
            ];
            conventions(&files, "throw-in-updater")
        };
        let lints = lint(board);
        assert_eq!(lints.len(), 1, "{lints:?}");
        assert_eq!(
            (lints[0].path.as_str(), lints[0].line),
            ("app/board/page.tsx", 10)
        );
        assert!(lints[0]
            .message
            .contains("`addTask` (`lib/tasks.ts`) throws"));
        assert!(lints[0].message.contains("Validate before setState"));
        // A local useState setter too.
        let local = board
            .replace(
                "const { state, setState } = useStore();",
                "const { state } = useStore();\n  const [tasks, setTasks] = useState([]);",
            )
            .replace(
                "setState((s) => ({ ...s, tasks: addTask(s.tasks, { title }) }));",
                "setTasks((prev) => addTask(prev, { title }));",
            );
        assert_eq!(lint(&local).len(), 1);
        // The fix: the lib call first, then the setter with its result.
        let fixed = board.replace(
            "setState((s) => ({ ...s, tasks: addTask(s.tasks, { title }) }));",
            "const tasks = addTask(state.tasks, { title });\n      setState((s) => ({ ...s, tasks }));",
        );
        assert!(lint(&fixed).is_empty());
        // No try: not this lint's; a function that does not throw: fine.
        let bare = board
            .replace("    try {\n", "    {\n")
            .replace(" catch (err) {\n      setError(String(err));\n    }", "");
        assert!(lint(&bare).is_empty());
        let safe = board.replace("addTask(s.tasks, { title })", "listTasks(s.tasks)");
        assert!(lint(&safe).is_empty());
        // A store whose setter computes the next state outside React's
        // updater: the catch sees the error.
        let eager = SEED_STORE.replace(
            "setStateValue((prev) => updater(prev));",
            "setStateValue(updater(current.current));",
        );
        let files = [
            ("lib/storage.ts", STORAGE),
            ("lib/store.tsx", eager.as_str()),
            ("lib/tasks.ts", TASKS_LIB),
            ("app/board/page.tsx", board),
        ];
        assert!(conventions(&files, "throw-in-updater").is_empty());
    }

    const SEARCH_LIB: &str = "export type Hit = { kind: 'task' | 'comment'; id: string; title: string };\nexport function searchAll(q, tasks, comments) {\n  const hits = [];\n  for (const task of tasks) hits.push({ kind: 'task', id: task.id, title: task.title });\n  for (const comment of comments) {\n    hits.push({ kind: 'comment', id: comment.id, title: comment.body });\n  }\n  return hits;\n}\n";

    /// Audit 3 (H3): a comment hit linked to /tasks/<commentId>.
    #[test]
    fn search_link_names_the_link_of_a_comment_hit() {
        let page = "import Link from \"next/link\";\nimport { searchAll } from \"../../lib/search\";\nexport default function Search({ hits }) {\n  return <ul>{hits.map((hit) => {\n    const href = hit.kind === \"task\"\n      ? `/tasks/${hit.id}`\n      : `/tasks/${hit.id}`;\n    return <li key={hit.id}><Link href={href}>{hit.title}</Link></li>;\n  })}</ul>;\n}\n";
        let lint = |lib: &str, page: &str| {
            let files = [("lib/search.ts", lib), ("app/search/page.tsx", page)];
            conventions(&files, "search-link")
        };
        let lints = lint(SEARCH_LIB, page);
        assert_eq!(lints.len(), 1, "{lints:?}");
        assert_eq!(
            (lints[0].path.as_str(), lints[0].line),
            ("app/search/page.tsx", 7)
        );
        assert!(lints[0].message.contains("`taskId: comment.taskId`"));
        // A section of comment hits keyed for comments.
        let section = "import Link from \"next/link\";\nimport { searchAll } from \"../../lib/search\";\nexport default function Search({ commentHits }) {\n  return <ul>{commentHits.map((hit) => (\n    <Link key={`comment-${hit.id}`} href={`/tasks/${hit.id}`}>{hit.title}</Link>\n  ))}</ul>;\n}\n";
        assert_eq!(lint(SEARCH_LIB, section).len(), 1);
        // The fix: the hit carries its task id and the page links by it.
        let lib = SEARCH_LIB.replace(
            "id: comment.id, title",
            "id: comment.id, taskId: comment.taskId, title",
        );
        let fixed = page.replace(
            "      : `/tasks/${hit.id}`;",
            "      : `/tasks/${hit.taskId}`;",
        );
        assert!(lint(&lib, &fixed).is_empty());
        // Comment hits that link elsewhere (a search link), task links only.
        let elsewhere = "import { searchAll } from \"../../lib/search\";\nfunction hitHref(hit) {\n  if (hit.kind === \"task\") return `/tasks/${hit.id}`;\n  return `/search?q=${encodeURIComponent(hit.title)}`;\n}\nexport default function Search() {\n  return <p>comments</p>;\n}\n";
        assert!(lint(SEARCH_LIB, elsewhere).is_empty());
    }

    /// Audit 3 (H4): two label catalogs with different ids.
    #[test]
    fn duplicate_catalog_names_the_copy_beside_the_types_file() {
        let types = "export interface Label { id: string; name: string }\nexport const LABELS: Label[] = [\n  { id: 'l1', name: 'bug' },\n  { id: 'l2', name: 'feature' },\n];\n";
        let labels = "import type { Label } from './types';\n\nexport const LABELS: Label[] = [\n  { id: 'bug', name: 'Bug' },\n  { id: 'feature', name: 'Feature' },\n];\n";
        let lint = |labels: &str| {
            let files = [("lib/types.ts", types), ("lib/labels.ts", labels)];
            conventions(&files, "duplicate-catalog")
        };
        let lints = lint(labels);
        assert_eq!(lints.len(), 1, "{lints:?}");
        assert_eq!(
            (lints[0].path.as_str(), lints[0].line),
            ("lib/labels.ts", 3)
        );
        assert!(lints[0]
            .message
            .contains("import `LABELS` from `lib/types.ts`"));
        // The fix: the copy imports the catalog.
        assert!(lint(
            "export { LABELS } from './types';\nexport function labelName(id) { return id; }\n"
        )
        .is_empty());
        // The same ids, or another catalog's name: fine.
        assert!(lint(&labels.replace("'bug'", "'l1'").replace("'feature'", "'l2'")).is_empty());
        assert!(lint(&labels.replace("LABELS", "TAGS")).is_empty());
        assert!(lints[0].message.contains("Delete this copy"));
        // A seed that uses the copy's ids: align the ids before deleting it.
        let seed = "export const seed = { tasks: [{ id: 't1', labelIds: ['bug', 'feature'] }] };\n";
        let files = [
            ("lib/types.ts", types),
            ("lib/labels.ts", labels),
            ("lib/seed.ts", seed),
        ];
        let lints = conventions(&files, "duplicate-catalog");
        assert_eq!(lints.len(), 1, "{lints:?}");
        assert!(lints[0]
            .message
            .contains("The other files use this copy's ids"));
    }
}
