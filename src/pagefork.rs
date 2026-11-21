//! Page fork: the local components of a page are written beside its main
//! component, each by a stream of its own, and joined into the one file.
//!
//! The wave ends with its longest file, and that is a page: a page that
//! holds its local components (`pagelocal`) is 3-5k tokens in one stream,
//! of which the main component is about 2-2.5k (2026-11-18, four large
//! pages: 5.0-14.3 s forked against 8.5-16.9 s whole). There is no outline
//! call and there are no holes to fill:
//!
//! 1. The page's writer is asked ([`ForkRules::order_note`]) to write the
//!    imports, then the props type of every local component, then the
//!    default export, and no other component.
//! 2. When the default export starts in the stream, each props type written
//!    before it starts one more request: the page's own request, the start of
//!    the file as written, and the order to write that one component.
//! 3. The answers are joined: the components go to the end of the page's
//!    code block, their new imports to the page's imports. A component that
//!    the page's writer wrote after all is kept as the writer wrote it.
//!
//! The result is one answer for one file, so the checks, the repair loop
//! and the host see what they saw before. A component whose request fails is
//! missing from the file: the type check names it and the repair writes it.
//! The extra requests do not pass through the scheduler's limits.
//!
//! The marks ([`ForkRules`]) are those of a TSX page. Off unless a host
//! wraps its client in a [`ForkClient`] (the bench switch `pagefork`).

use crate::client::{
    with_text_hook, Completion, CompletionRequest, ModelClient, PrewarmReport, TextHook,
};
use crate::project::strip_fence;
use anyhow::Result;
use async_trait::async_trait;
use std::sync::{Arc, Mutex};
use tokio::task::JoinHandle;

/// Says whether a request is the first writing of a page that may fork.
pub type ForkTest = Arc<dyn Fn(&CompletionRequest) -> bool + Send + Sync>;

/// The names that the page of a request may import, each with its import
/// line in the one-line form `import { name } from "module";` (or a default
/// import).
pub type ImportTable = Arc<dyn Fn(&CompletionRequest) -> Vec<(String, String)> + Send + Sync>;

/// What a fork needs to know of the language.
#[derive(Clone)]
pub struct ForkRules {
    /// True for a request that may fork (a page's first writing; never a
    /// repair, a retry or a continuation).
    pub applies: ForkTest,
    /// Added to the end of the page's request: the order of the file.
    pub order_note: String,
    /// What a component's writer is told (`{name}`, `{head}`).
    pub part_note: String,
    /// A line that starts with one of these starts the main component.
    pub main_marks: Vec<String>,
    /// At most this many components fork (the first in the file).
    pub max_parts: usize,
    /// The importable names of the project (`None`: none known). A
    /// component's writer sees the page's imports but not what the main
    /// component will add, and forgets its own (7 of 8 runs had a page with
    /// "cannot find name"): a name of the table that a component calls and
    /// that neither it nor the page imports or declares gets its import.
    pub imports: Option<ImportTable>,
}

/// The rules of a TSX page; `applies` is the host's.
pub fn tsx_rules(applies: ForkTest) -> ForkRules {
    ForkRules {
        applies,
        order_note: "\n\nThis page is written by several writers at the same time. Write the \
                     file in this order:\n1. The \"use client\" line and the imports.\n2. For \
                     each local component of this file (those named above, and every other \
                     component or sub-form you want in this file): its props type only, `type \
                     NameProps = { ... };`, with every prop it needs (data and callbacks) and a `//` comment on each prop: \
                     what it holds, or for a callback what it does and what it returns \
                     (`// adds the task; returns an error message, or null when it was \
                     added`). The other writer sees only these types, so the comments are \
                     the contract: write the default export to them exactly. \
                     Small pure helper functions may follow.\n3. The default export, complete (with its inner component, if it needs a wrapper), \
                     rendering each local component with all its props.\nA local component draws its \
                     whole section itself: its card or frame, its one heading, its fields, \
                     its messages and its confirm steps, with its own interaction state \
                     (form fields, open or closed, the row being edited or dragged, a \
                     pending confirm). So render each one bare and once, `<Name ... />`, \
                     with no card, section heading, message or confirm step of your own around \
                     it, and do not show again what a component shows (a list, results, a \
                     total). The page draws the one page heading (`<h1>`); a component \
                     never does. \
                     the page keeps only the app state and the handlers that change it, and \
                     a callback prop does its change at once (the component has confirmed \
                     already).\nDo NOT write any \
                     component but the default export: another writer writes each local \
                     component from its props type, and it is added to the end of this file. \
                     Every component the page renders that is not imported must have its \
                     props type in step 2. Close the code block after the default export."
            .to_string(),
        part_note: "\n\nThis file is written by several writers at the same time. Its start is \
                    written:\n```tsx\n{head}\n```\nAnother writer writes the default export, \
                    which renders `<{name} />` with exactly the props of `{name}Props`. You \
                    write the local component `{name}` only: one declaration `function \
                    {name}(...)` that takes exactly the props of `{name}Props`, complete, not \
                    exported. Follow the comment on each prop exactly (what a callback \
                    returns, and when). You draw the whole section: its card or frame and \
                    its one heading (an `<h2>`, never the page's `<h1>`), its fields, its messages and its confirm steps; the page \
                    renders `<{name} />` bare, with nothing around it. Interaction state \
                    (form fields, open or closed, the row being edited or dragged, a pending \
                    confirm) is yours, in `useState` here, and both ends of an interaction \
                    (drag start and drop, edit and save, ask and confirm) are handled here. \
                    A callback prop does its change at once: confirm before you call it. It \
                    keeps no copy of the app state: data comes through its props and \
                    changes go through its callbacks. Do not write the imports, the types \
                    or the helpers above again, and no other component. Use only the names \
                    that the start of the file imports or declares: for every other name (a \
                    lib function, a hook such as `useState`, `React`), write its import line \
                    first, before the function. Output exactly one \
                    fenced code block and nothing else."
            .to_string(),
        main_marks: vec![
            "export default function".to_string(),
            "export default async function".to_string(),
        ],
        max_parts: 6,
        imports: None,
    }
}

/// The React names a TSX component may use without their import, for an
/// [`ImportTable`].
pub fn react_imports() -> Vec<(String, String)> {
    let mut table = vec![(
        "React".to_string(),
        "import React from \"react\";".to_string(),
    )];
    for hook in [
        "useState",
        "useEffect",
        "useMemo",
        "useRef",
        "useCallback",
        "useReducer",
        "useId",
        "Fragment",
    ] {
        table.push((
            hook.to_string(),
            format!("import {{ {hook} }} from \"react\";"),
        ));
    }
    table
}

/// What a [`ForkClient`] did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ForkStats {
    /// Page requests that got the order note.
    pub pages: usize,
    /// Pages that started at least one component request.
    pub forked: usize,
    /// Component requests started, joined into their page, failed (or not
    /// the asked component), and dropped (the page's writer wrote it).
    pub parts: usize,
    pub joined: usize,
    pub failed: usize,
    pub dropped: usize,
    /// Import lines added for names a component used without importing.
    pub imports: usize,
}

/// A client that forks the local components of a page (see the module).
pub struct ForkClient {
    inner: Arc<dyn ModelClient>,
    rules: ForkRules,
    stats: Mutex<ForkStats>,
}

impl ForkClient {
    pub fn new(inner: Arc<dyn ModelClient>, rules: ForkRules) -> Self {
        Self {
            inner,
            rules,
            stats: Mutex::new(ForkStats::default()),
        }
    }

    pub fn stats(&self) -> ForkStats {
        self.stats.lock().map(|stats| *stats).unwrap_or_default()
    }
}

/// The start of a streamed file up to the main component, once that has
/// started: the whole lines before the first line that starts with a mark
/// or declares a function with a capital first letter (a component: a page
/// that needs a wrapper writes `function PageInner()` before its default
/// export; a helper's name starts small).
fn head_of<'a>(text: &'a str, marks: &[String]) -> Option<&'a str> {
    let whole = &text[..text.rfind('\n')? + 1];
    let mut at = 0;
    for line in whole.split_inclusive('\n') {
        let component = line
            .strip_prefix("function ")
            .and_then(|rest| rest.chars().next())
            .is_some_and(char::is_uppercase);
        if component || marks.iter().any(|mark| line.starts_with(mark.as_str())) {
            return Some(&whole[..at]);
        }
        at += line.len();
    }
    None
}

/// The components whose props type `head` declares (`type NameProps =`,
/// `interface NameProps`), in order, without repeats.
fn props_names(head: &str) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    for line in head.lines() {
        let rest = line
            .strip_prefix("type ")
            .or_else(|| line.strip_prefix("interface "));
        let Some(rest) = rest else {
            continue;
        };
        let word: String = rest
            .chars()
            .take_while(|c| c.is_alphanumeric() || *c == '_')
            .collect();
        let Some(name) = word.strip_suffix("Props") else {
            continue;
        };
        if !name.is_empty() && !names.iter().any(|known| known == name) {
            names.push(name.to_string());
        }
    }
    names
}

/// True when `code` declares the component `name` at the start of a line.
fn declares(code: &str, name: &str) -> bool {
    code.lines().any(|line| {
        [
            format!("function {name}("),
            format!("function {name}<"),
            format!("const {name} ="),
            format!("const {name}:"),
        ]
        .iter()
        .any(|start| line.starts_with(start.as_str()))
    })
}

fn word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '$'
}

/// The places of `name` in `text` as a whole word.
fn words<'a>(text: &'a str, name: &'a str) -> impl Iterator<Item = usize> + 'a {
    text.match_indices(name).filter_map(move |(at, _)| {
        let before = text[..at].chars().next_back();
        let after = text[at + name.len()..].chars().next();
        (!before.is_some_and(word_char) && !after.is_some_and(word_char)).then_some(at)
    })
}

/// The import lines of `table` that the component `part` needs: a name it
/// calls, renders or reads a member of (`name(`, `<Name`, `name.`; not as a
/// member itself, `x.name(`), that no
/// import line of `page` or of the part names, and that the part does not
/// declare (a `const`, `let` or `function`, or a name in its own
/// parameters).
fn missing_imports(page: &str, part: &str, table: &[(String, String)]) -> Vec<String> {
    let imported: String = page
        .lines()
        .chain(part.lines())
        .filter(|line| line.starts_with("import "))
        .collect::<Vec<_>>()
        .join("\n");
    let code: String = part
        .lines()
        .filter(|line| !line.starts_with("import "))
        .collect::<Vec<_>>()
        .join("\n");
    let parameters = code.split(") {").next().unwrap_or("");
    let mut lines = Vec::new();
    for (name, line) in table {
        let used = words(&code, name).any(|at| {
            let after = code[at + name.len()..].chars().next();
            !code[..at].ends_with('.')
                && (matches!(after, Some('(') | Some('.')) || code[..at].ends_with('<'))
        });
        if !used || words(&imported, name).next().is_some() {
            continue;
        }
        let declared = words(parameters, name).next().is_some()
            || ["const ", "let ", "function ", "var "]
                .iter()
                .any(|word| words(&code, name).any(|at| code[..at].ends_with(word)));
        if !declared && !lines.contains(line) {
            lines.push(line.clone());
        }
    }
    lines
}

/// The head without the line that opens its code block.
fn head_code(head: &str) -> &str {
    match head.split_once('\n') {
        Some((first, rest)) if first.trim_start().starts_with("```") => rest,
        _ => head,
    }
}

/// The module of a one-line `import { a, b } from "m";` and its names.
fn named_import(line: &str) -> Option<(&str, Vec<&str>)> {
    let rest = line.trim().strip_prefix("import {")?;
    let (names, from) = rest.split_once('}')?;
    let module = from
        .trim()
        .strip_prefix("from")?
        .trim()
        .trim_end_matches(';');
    let module = module.trim_matches(|c| c == '"' || c == '\'');
    let names = names
        .split(',')
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .collect();
    Some((module, names))
}

/// `page` (a worker's answer: one code block) with the `parts` (each a
/// component's code, its imports first) joined: the code before the block's
/// closing fence, each new import with the page's imports (a name of a
/// module the page imports from goes into that line).
fn join_parts(page: &str, parts: &[String]) -> String {
    let mut lines: Vec<String> = page.trim_end().lines().map(str::to_string).collect();
    let closed = lines.len() > 1
        && lines
            .last()
            .is_some_and(|line| line.trim().starts_with("```"));
    let fence = closed.then(|| lines.pop()).flatten();
    let mut imports: Vec<String> = Vec::new();
    for part in parts {
        let mut code: Vec<&str> = Vec::new();
        for line in part.trim().lines() {
            if line.starts_with("import ") {
                imports.push(line.trim_end().to_string());
            } else if !(code.is_empty() && (line.trim().is_empty() || line.contains("use client")))
            {
                code.push(line);
            }
        }
        lines.push(String::new());
        lines.extend(code.into_iter().map(str::to_string));
    }
    for import in imports {
        if lines.iter().any(|line| line.trim_end() == import) {
            continue;
        }
        if let Some((module, names)) = named_import(&import) {
            let same = lines.iter().position(|line| {
                line.starts_with("import {")
                    && named_import(line).is_some_and(|(other, _)| other == module)
            });
            if let Some(at) = same {
                let have: Vec<String> = named_import(&lines[at])
                    .map(|(_, names)| names.into_iter().map(str::to_string).collect())
                    .unwrap_or_default();
                let new: Vec<&str> = names
                    .into_iter()
                    .filter(|name| !have.iter().any(|known| known == name))
                    .collect();
                if !new.is_empty() {
                    let all: Vec<&str> = have.iter().map(String::as_str).chain(new).collect();
                    lines[at] = format!("import {{ {} }} from \"{module}\";", all.join(", "));
                }
                continue;
            }
        }
        let at = lines
            .iter()
            .position(|line| line.starts_with("import "))
            .unwrap_or_else(|| {
                // After the block's opening line and a "use client" line.
                lines
                    .iter()
                    .take(3)
                    .rposition(|line| line.starts_with("```") || line.contains("use client"))
                    .map_or(0, |at| at + 1)
            });
        lines.insert(at, import);
    }
    if let Some(fence) = fence {
        lines.push(fence);
    }
    lines.join("\n")
}

type PartHandles = Vec<(String, JoinHandle<Result<Completion>>)>;

#[async_trait]
impl ModelClient for ForkClient {
    async fn complete(&self, request: CompletionRequest) -> Result<Completion> {
        if !(self.rules.applies)(&request) {
            return self.inner.complete(request).await;
        }
        let mut main = request.clone();
        main.user.push_str(&self.rules.order_note);
        let table = match &self.rules.imports {
            Some(imports) => imports(&request),
            None => Vec::new(),
        };
        let handles: Arc<Mutex<Option<PartHandles>>> = Arc::default();
        let hook: TextHook = {
            let (handles, inner, rules) = (handles.clone(), self.inner.clone(), self.rules.clone());
            Arc::new(move |text: &str| {
                let Ok(mut handles) = handles.lock() else {
                    return;
                };
                if handles.is_some() {
                    return;
                }
                let Some(head) = head_of(text, &rules.main_marks) else {
                    return;
                };
                let started = props_names(head)
                    .into_iter()
                    .take(rules.max_parts)
                    .map(|name| {
                        let mut part = request.clone();
                        part.user.push_str(
                            &rules
                                .part_note
                                .replace("{head}", head_code(head).trim_end())
                                .replace("{name}", &name),
                        );
                        let inner = inner.clone();
                        (
                            name,
                            tokio::spawn(async move { inner.complete(part).await }),
                        )
                    })
                    .collect();
                *handles = Some(started);
            })
        };
        let done = with_text_hook(hook, self.inner.complete(main)).await;
        let started = handles.lock().ok().and_then(|mut handles| handles.take());
        let mut stats = ForkStats {
            pages: 1,
            ..ForkStats::default()
        };
        let mut done = match done {
            Ok(done) => done,
            Err(error) => {
                for (_, handle) in started.into_iter().flatten() {
                    handle.abort();
                }
                return Err(error);
            }
        };
        let mut parts = Vec::new();
        for (name, handle) in started.into_iter().flatten() {
            stats.parts += 1;
            if declares(&strip_fence(&done.text), &name) {
                handle.abort();
                stats.dropped += 1;
                continue;
            }
            match handle.await {
                Ok(Ok(part)) if declares(&strip_fence(&part.text), &name) => {
                    done.input_tokens += part.input_tokens;
                    done.output_tokens += part.output_tokens;
                    if let Some(cached) = part.cached_input_tokens {
                        *done.cached_input_tokens.get_or_insert(0) += cached;
                    }
                    let code = strip_fence(&part.text);
                    let mut lines = missing_imports(&done.text, &code, &table);
                    stats.imports += lines.len();
                    lines.push(code);
                    parts.push(lines.join("\n"));
                    stats.joined += 1;
                }
                _ => stats.failed += 1,
            }
        }
        stats.forked = usize::from(stats.parts > 0);
        if !parts.is_empty() {
            done.text = join_parts(&done.text, &parts);
        }
        if let Ok(mut total) = self.stats.lock() {
            total.pages += stats.pages;
            total.forked += stats.forked;
            total.parts += stats.parts;
            total.joined += stats.joined;
            total.failed += stats.failed;
            total.dropped += stats.dropped;
            total.imports += stats.imports;
        }
        Ok(done)
    }

    async fn prewarm(&self, connections: usize) -> Result<PrewarmReport> {
        self.inner.prewarm(connections).await
    }

    async fn continue_completion(
        &self,
        request: CompletionRequest,
        partial: String,
    ) -> Result<crate::continuation::Continued> {
        self.inner.continue_completion(request, partial).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::report_text;
    use std::time::Duration;

    const PAGE: &str = "```tsx\n\"use client\";\nimport { useState } from \"react\";\nimport { addCar } from \"../lib/cars\";\n\ntype CarFormProps = { onAdd: (name: string) => void };\ntype CarListProps = { names: string[] };\n\nexport default function CarsPage() {\n  const [names, setNames] = useState<string[]>([]);\n  return <><CarForm onAdd={(n) => setNames([...names, n])} /><CarList names={names} /></>;\n}\n```";

    /// Streams `PAGE` for the page's request and answers a part's request
    /// with its component; `fail` names a part whose request fails.
    struct Fake {
        seen: Mutex<Vec<String>>,
        fail: Option<&'static str>,
        page: String,
    }

    #[async_trait]
    impl ModelClient for Fake {
        async fn complete(&self, request: CompletionRequest) -> Result<Completion> {
            self.seen.lock().unwrap().push(request.user.clone());
            let text = if request
                .user
                .contains("You write the local component `CarForm`")
            {
                if self.fail == Some("CarForm") {
                    anyhow::bail!("refused");
                }
                "```tsx\nimport { useRef } from \"react\";\nimport { cap } from \"../lib/text\";\nfunction CarForm({ onAdd }: CarFormProps) {\n  const ref = useRef(null);\n  return <button onClick={() => onAdd(cap(\"a\"))}>Add</button>;\n}\n```".to_string()
            } else if request
                .user
                .contains("You write the local component `CarList`")
            {
                "```tsx\nfunction CarList({ names }: CarListProps) {\n  return <ul>{names.map((n) => <li key={n}>{n}</li>)}</ul>;\n}\n```".to_string()
            } else {
                let mut sent = String::new();
                for line in self.page.split_inclusive('\n') {
                    sent.push_str(line);
                    report_text(&sent);
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
                self.page.clone()
            };
            Ok(Completion {
                text,
                provider: None,
                input_tokens: 10,
                output_tokens: 5,
                reasoning_tokens: None,
                cached_input_tokens: None,
                finish_reason: Some("stop".into()),
                usage_is_estimated: false,
                elapsed: Duration::ZERO,
                time_to_first_token: None,
            })
        }
    }

    fn request(user: &str) -> CompletionRequest {
        CompletionRequest {
            system: "s".into(),
            shared_context: None,
            user: user.into(),
            max_tokens: 100,
            temperature: 0.0,
            reasoning_effort: None,
            prompt_cache_key: None,
            allow_empty: false,
            response_format: None,
        }
    }

    fn client(fail: Option<&'static str>, page: &str) -> (Arc<Fake>, ForkClient) {
        let fake = Arc::new(Fake {
            seen: Mutex::default(),
            fail,
            page: page.to_string(),
        });
        let applies: ForkTest = Arc::new(|request| request.user.contains("page.tsx"));
        (fake.clone(), ForkClient::new(fake, tsx_rules(applies)))
    }

    #[tokio::test]
    async fn the_components_are_written_beside_the_page_and_joined() {
        let (fake, fork) = client(None, PAGE);
        let done = fork.complete(request("Title: app/page.tsx")).await.unwrap();
        let text = done.text;
        // One block: the page, then the components, then the fence.
        assert!(text.starts_with("```tsx\n\"use client\";\n"));
        assert!(text.ends_with("}\n```"));
        assert_eq!(text.matches("```").count(), 2);
        let main = text.find("export default function CarsPage").unwrap();
        assert!(text.find("function CarForm(").unwrap() > main);
        assert!(text.find("function CarList(").unwrap() > main);
        // A new name of a module the page imports from joins that line; a
        // new module is a new import line with the others.
        assert!(text.contains("import { useState, useRef } from \"react\";"));
        assert!(text.find("import { cap } from \"../lib/text\";").unwrap() < main);
        assert_eq!(text.matches("from \"react\"").count(), 1);
        assert_eq!(done.output_tokens, 15);
        let seen = fake.seen.lock().unwrap();
        assert_eq!(seen.len(), 3);
        assert!(seen[0].contains("Write the file in this order"));
        // A part's request has the head as written and not the order note.
        assert!(seen[1].contains("type CarListProps = { names: string[] };\n```"));
        assert!(!seen[1].contains("Write the file in this order"));
        assert!(!seen[1].contains("export default function"));
        assert_eq!(
            fork.stats(),
            ForkStats {
                pages: 1,
                forked: 1,
                parts: 2,
                joined: 2,
                failed: 0,
                dropped: 0,
                imports: 0
            }
        );
    }

    #[tokio::test]
    async fn a_failed_part_is_left_out_and_a_part_the_page_wrote_is_dropped() {
        let (_, fork) = client(Some("CarForm"), PAGE);
        let text = fork
            .complete(request("Title: app/page.tsx"))
            .await
            .unwrap()
            .text;
        assert!(!text.contains("function CarForm("));
        assert!(text.contains("function CarList("));
        assert_eq!(fork.stats().failed, 1);

        let own = PAGE.replace(
            "}\n```",
            "}\n\nfunction CarList({ names }: CarListProps) {\n  return null;\n}\n```",
        );
        let (_, fork) = client(None, &own);
        let text = fork
            .complete(request("Title: app/page.tsx"))
            .await
            .unwrap()
            .text;
        assert_eq!(text.matches("function CarList(").count(), 1);
        assert!(text.contains("return null;"));
        assert_eq!(fork.stats().dropped, 1);
        assert_eq!(fork.stats().joined, 1);
    }

    #[tokio::test]
    async fn other_requests_and_pages_without_props_types_pass_as_they_are() {
        let (fake, fork) = client(None, PAGE);
        let text = fork
            .complete(request("Title: lib/a.ts"))
            .await
            .unwrap()
            .text;
        assert_eq!(text, PAGE);
        assert_eq!(fake.seen.lock().unwrap().as_slice(), ["Title: lib/a.ts"]);
        assert_eq!(fork.stats(), ForkStats::default());

        let plain = "```tsx\nexport default function Page() {\n  return null;\n}\n```";
        let (fake, fork) = client(None, plain);
        let text = fork
            .complete(request("Title: app/page.tsx"))
            .await
            .unwrap()
            .text;
        assert_eq!(text, plain);
        assert_eq!(fake.seen.lock().unwrap().len(), 1);
        assert_eq!(fork.stats().forked, 0);
    }

    #[test]
    fn a_name_that_a_part_calls_without_importing_gets_its_import() {
        let mut table = react_imports();
        table.push(("cap".into(), "import { cap } from \"../lib/text\";".into()));
        table.push(("low".into(), "import { low } from \"../lib/text\";".into()));
        table.push((
            "total".into(),
            "import { total } from \"../lib/sum\";".into(),
        ));
        let page = "import { useState } from \"react\";\nexport default function P() {}";
        let part = "function A({ low }: AProps) {\n  const [a] = useState(0);\n  const total = 1;\n  const r = React.useRef(null);\n  useEffect(() => {}, []);\n  return <b>{cap(\"x\")}{low(\"y\")}{total}</b>;\n}";
        assert_eq!(
            missing_imports(page, part, &table),
            [
                "import React from \"react\";",
                "import { useEffect } from \"react\";",
                "import { cap } from \"../lib/text\";"
            ]
        );
        // The part's own import counts; a word inside another is no use.
        let own =
            "import { cap } from \"../lib/text\";\nfunction A() {\n  return recap(cap(\"x\"));\n}";
        assert!(missing_imports(page, own, &table).is_empty());
        // Joined: the lines merge into the page's imports.
        let joined = join_parts(
            "```tsx\nimport { useState } from \"react\";\nexport default function P() {}\n```",
            &["import { useEffect } from \"react\";\nfunction A() {}".to_string()],
        );
        assert!(joined.contains("import { useState, useEffect } from \"react\";"));
    }

    #[test]
    fn head_names_and_declarations() {
        let marks = tsx_rules(Arc::new(|_| true)).main_marks;
        assert_eq!(head_of("a\nexport default function P", &marks), None);
        assert_eq!(
            head_of("a\nexport default function P() {\n", &marks),
            Some("a\n")
        );
        assert_eq!(
            props_names("type AProps = {};\ninterface BProps {\ntype AProps = {};\ntype Props = {};\ntype Car = {};"),
            ["A", "B"]
        );
        assert!(declares("const A = () => null;", "A"));
        assert!(!declares("  function A() {}", "A"));
        assert!(!declares("function AB() {}", "A"));
        assert_eq!(
            head_of(
                "a
function low() {}
function PageInner() {
export default",
                &marks
            ),
            Some(
                "a
function low() {}
"
            )
        );
        assert_eq!(head_code("```tsx\nx\n"), "x\n");
    }
}
