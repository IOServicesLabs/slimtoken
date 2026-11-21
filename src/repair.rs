//! Multi-round check and repair, moved from the `bench/showdown` repair step.
//!
//! Parallel workers write files that can disagree at the seams (a property
//! name, a literal value, an import of a file that nobody wrote).
//! [`check_and_repair`] finds those places with a [`Checker`] and repairs
//! the named files, again in parallel:
//!
//! 1. Check the files of the report and read the problems for each file.
//! 2. For each file with problems, ask for a corrected file. The request
//!    carries the file's first prompt (served from the prefix cache), the
//!    problems, and the current text of the project files it imports.
//! 3. Check again and repeat with the problems that are left. A round can
//!    make the count worse for a moment (a corrected shared file breaks its
//!    users until they are corrected too), so the loop goes on until the
//!    check passes, [`RepairPolicy::stale_limit`] rounds in sequence bring
//!    no new best result, or [`RepairPolicy::max_rounds`] is reached. The
//!    files of the best result ([`CheckResult::nearer_than`]: never fewer
//!    tests) are the ones kept.
//!
//! A long file with few problems is repaired with search-and-replace edits
//! ([`apply_edit_blocks`]) and not written again in full. If the edits
//! cannot be placed, the next round writes the file in full.
//!
//! A file that still has problems after its own repair gets a joint repair
//! when the profile's [`RepairHooks::joint_editable`] names files for it:
//! one request sees that file and the files that the problems name, and it
//! can change those files only. A test file that is stuck is first repaired
//! alone with a different temperature ([`RepairPolicy::stuck_tests_alone`]),
//! then jointly with the modules it tests, with more reasoning
//! ([`RepairPolicy::escalate_effort`]). A file whose problems name another
//! project file in backticks as the place to change ([`named_files`]) gets
//! the joint repair with that file at once.
//!
//! What a stack adds (imports, which files a joint repair may change, fixes
//! with no model call) comes from [`RepairHooks`]; with
//! [`NO_REPAIR_HOOKS`](crate::profile::NO_REPAIR_HOOKS) every repair is of
//! one file. A [`CheckResult::patches`] block is put in place with no model
//! call and the files are checked again.
//!
//! `reconcile::reconcile_with` stays the one-round test repair of the CLI's
//! `--check-cmd` without `--repair-rounds`.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::io::Write as _;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::check::{apply_patches, CheckMode, CheckResult, Checker};
use crate::client::ModelClient;
use crate::dag::{Plan, Task};
use crate::guard::{
    lock_block, only_knock_on, root_first, without_lines, CodeIndex, GuardReport, Lock,
};
use crate::profile::{Problem, RepairHooks};
use crate::project::{apply_edit_blocks, report_files, scan};
use crate::scheduler::{execute_plan, RunReport, SchedulerConfig, TaskStatus};

/// How [`check_and_repair`] repairs. The defaults are the measured values of
/// the `bench/showdown` repair step.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RepairPolicy {
    /// Most repair rounds.
    pub max_rounds: usize,
    /// Stop after this many rounds in sequence with no new best result.
    pub stale_limit: usize,
    /// A file is repaired with edits if it has at least this many lines...
    pub edit_min_lines: usize,
    /// ...and the checker reports at most this many lines about it.
    pub edit_max_problem_lines: usize,
    /// Most characters of project files that one repair request carries.
    pub context_chars: usize,
    /// Token limit of a joint repair: its answer can hold some files.
    pub joint_max_tokens: u32,
    /// Temperature of a repair that asks again after a repair that did not
    /// help (a stuck test, a second joint repair).
    pub escalate_temperature: f32,
    /// Reasoning effort of the joint repair of a test and of a second joint
    /// repair; `None` keeps the task's effort.
    pub escalate_effort: Option<String>,
    /// Whether the joint repair of a test file also gets
    /// `escalate_effort`. Off, a stuck test repairs at
    /// `escalate_temperature` without reasoning; code files keep it. A test
    /// repair with reasoning spent its whole budget on hidden tokens and no
    /// answer twice (173 s and 213 s on DeepSeek).
    pub escalate_tests: bool,
    /// A test that failed again after its repair is first repaired alone,
    /// to agree with the code, before a joint repair with the code.
    pub stuck_tests_alone: bool,
    /// Key under which the checker may keep state between the checks of
    /// this loop (its caches); empty: each check starts cold.
    pub check_key: String,
    /// Append each repair answer to this file (for work on the prompts).
    pub answer_log: Option<PathBuf>,
    /// The guards of [`crate::guard`]: export locks in the repair prompts,
    /// root-first repairs of missing exports, and no repair of knock-on
    /// files. Off by default (no change for a host that does not ask).
    pub guard: bool,
    /// A first joint repair whose problems name the other files (no repair
    /// of the file alone came before) asks for edit blocks under each file's
    /// heading ([`JOINT_EDIT_RULE`]) instead of whole files: such a repair
    /// rewrote a 5,277-token page in 14.5 s for one lint line. An answer
    /// whose edits cannot all be placed changes nothing, and those files are
    /// written in full from then on. Off by default.
    pub joint_edits: bool,
    /// A test file of at least `edit_min_lines` lines is repaired with edit
    /// blocks however many lines the checker reports about it (one failing
    /// assertion prints about twelve), and so are a stuck test repaired
    /// alone and its joint repair with the modules. On the bench, edits repaired 23 of 25 test files and whole
    /// rewrites 2 of 14. Off by default.
    pub test_edits: bool,
    /// Contract side (default off): the first repair of a test leaves a
    /// failing case as it is when the contract line of the function states
    /// what the test expects ([`CONTRACT_TEST_RULE`]), and a test that still
    /// fails is then repaired together with the modules that its failing
    /// cases name ([`CONTRACT_SIDE_RULE`]), before `stuck_tests_alone`.
    /// Without it a failing test is always changed to what the module does:
    /// in 13 of 42 failing cases on the bench the module had broken its
    /// contract and shipped that way behind a passing test.
    pub contract_side: bool,
    /// As `stale_limit`, while only test files have problems (the smaller
    /// of the two counts). Rounds after two stale ones repaired no test in
    /// 27 bench runs. The default is `stale_limit`'s: no change.
    pub test_stale_limit: usize,
}

impl Default for RepairPolicy {
    fn default() -> Self {
        Self {
            max_rounds: 8,
            stale_limit: 3,
            edit_min_lines: 60,
            edit_max_problem_lines: 60,
            context_chars: 48_000,
            joint_max_tokens: 16_000,
            escalate_temperature: 0.8,
            escalate_effort: Some("medium".to_string()),
            escalate_tests: true,
            stuck_tests_alone: true,
            check_key: String::new(),
            answer_log: None,
            guard: false,
            joint_edits: false,
            test_edits: false,
            contract_side: false,
            test_stale_limit: 3,
        }
    }
}

/// One repair round.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Round {
    /// The files that the round repaired or added.
    pub files: Vec<String>,
    pub after: CheckResult,
    /// True if this round gave a new best result.
    pub kept: bool,
    pub output_tokens: u64,
    pub seconds: f64,
}

/// What [`check_and_repair`] did.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepairSummary {
    pub before: CheckResult,
    /// The check result for the files that were kept.
    pub after: CheckResult,
    pub rounds: Vec<Round>,
    /// Files changed by a kept round.
    pub repaired: Vec<String>,
    pub seconds: f64,
    /// What the guards did ([`RepairPolicy::guard`]); `None` with them off.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub guard: Option<GuardReport>,
}

/// The rule of a test file's repair: the test changes to what the code does.
pub const TEST_REPAIR_RULE: &str = "Correct the tests so that they compile against the files \
shown. Where a test expects behavior that the implementation shown does not have, change the \
expectation to the behavior shown. Never delete, skip or merge a test.";

/// `RepairPolicy::contract_side`: in place of [`TEST_REPAIR_RULE`] in the
/// first repair of a test. A case whose expectation the contract line states
/// is left as it is, so that the next round can repair the module.
pub const CONTRACT_TEST_RULE: &str = "Correct the tests so that they compile against the files \
shown. Where a test expects behavior that the implementation shown does not have, read the \
contract line of that function in the module contracts above (its signature and the `//` \
comment after it). If the comment states in so many words what the test expects (a unit or \
range of the result, an error that is thrown, a result for an edge input) and the \
implementation does something else, the implementation is wrong: leave that test case exactly \
as it is. In every other case (the comment does not state it, or the test data or the \
arithmetic of the test is wrong) change the expectation to the behavior shown. Never delete, \
skip or merge a test. Before the edit blocks, write one line for each failing case: the \
function, then the words of its contract comment about this point in quotes (or `nothing`), \
then `module wrong` or `test wrong`.";

/// `RepairPolicy::contract_side`: added (in place of [`TRACE_RULE`]) to the
/// joint repair of a test that still fails after its first repair.
pub const CONTRACT_SIDE_RULE: &str = "\n\n# Which side is wrong\nThe cases that still fail \
were left as they are because the contract line of the function (its signature and the `//` \
comment after it, in the module contracts above) seemed to state what the test expects. Check \
each one. If the comment states it and the module does something else, the module is wrong: \
correct that function of the module so that it does what its contract line says, change \
nothing else in the module, and leave the test. If the comment does not state it, the test is \
wrong: change its expectation to what the code gives.";

/// `RepairPolicy::contract_side`: whether a test repair's answer names the
/// module as the wrong side. The verdict lines stand before the first edit
/// block or fence; a line's verdict is the last `module wrong` or `test
/// wrong` in it outside brackets (a reason in brackets may follow or name
/// the other side), and of two lines about the same function (the text
/// before the first dash or colon) the later one counts, since an answer
/// sometimes corrects itself.
pub fn says_module_wrong(answer: &str) -> bool {
    let mut verdicts: Vec<(String, bool)> = Vec::new();
    for line in answer
        .lines()
        .take_while(|line| !line.starts_with("<<<<<<<") && !line.trim_start().starts_with("```"))
    {
        let lower = line.to_lowercase();
        // Without what stands in brackets (an open bracket runs to the end).
        let mut plain = String::new();
        let mut depth = 0usize;
        for ch in lower.chars() {
            match ch {
                '(' => depth += 1,
                ')' => depth = depth.saturating_sub(1),
                _ if depth == 0 => plain.push(ch),
                _ => {}
            }
        }
        let module = plain.rfind("module wrong");
        let test = plain.rfind("test wrong");
        let names_module = match (module, test) {
            (Some(module), Some(test)) => module > test,
            (Some(_), None) => true,
            (None, Some(_)) => false,
            (None, None) => continue,
        };
        let subject: String = lower
            .split(['\u{2014}', '\u{2013}', ':', ','])
            .next()
            .unwrap_or_default()
            .split(" - ")
            .next()
            .unwrap_or_default()
            .chars()
            .filter(|ch| ch.is_alphanumeric() || *ch == '_')
            .collect();
        match verdicts.iter_mut().find(|(known, _)| *known == subject) {
            Some(entry) => entry.1 = names_module,
            None => verdicts.push((subject, names_module)),
        }
    }
    verdicts.iter().any(|(_, names_module)| *names_module)
}

/// Added to the repair of a test file that failed again after a repair.
pub const STUCK_TEST_RULE: &str = "\n\nThis test file failed again after one repair. For each \
test case that fails, read the code of the function and work out what it returns for the input \
of the test, step by step, and change the expectation to that value. If the right value is not \
certain from the code, delete that one test case and keep all the others. Do not change a test \
case that passes.";

/// Added to the joint repair of a test and the modules that it tests.
pub const TRACE_RULE: &str = "\n\n# Which side is wrong\nTwo repairs of the test did not help, \
so do not write the same test again. Follow each failing case through the code of the module, \
line by line, with the values of the test (for example: which id does a new record get, and \
does it collide with an id that the test data has?). If the module does not do what its plan \
line and its function names promise, the module is wrong: output the corrected module, and the \
test only if it must change too. If the module is right, output the test with the expectation \
that the code gives.";

/// How many import levels below a knock-on file the wait for a file that
/// the root-first guard fixed looks.
const KNOCK_ON_DEPTH: usize = 3;

/// Most files that a joint repair may change once the export lock adds the
/// users of a broken export.
const JOINT_GUARD_FILES: usize = 6;

/// The output contract of an edit repair (the bench's wording; the same
/// blocks as [`project::EDIT_FORMAT`](crate::project::EDIT_FORMAT)).
pub const EDIT_RULE: &str = "Output ONLY search/replace edit blocks, never the whole file:\n\
<<<<<<< SEARCH\n(lines copied exactly from your current version)\n=======\n\
(their replacement)\n>>>>>>> REPLACE\nEach SEARCH must match exactly one place in the current \
version: include enough lines around the change to make it unique. Use as many blocks as the \
correction needs.";

/// The output contract of a joint repair that answers with edits
/// ([`RepairPolicy::joint_edits`]), in place of [`JOINT_WHOLE_RULE`].
pub const JOINT_EDIT_RULE: &str = "Output ONLY the changes, never a whole file. For each file \
that you change: a heading line with the path in backticks (### `path/to/file`), then the \
search/replace edit blocks for that file:\n<<<<<<< SEARCH\n(lines copied exactly from the file \
as shown above)\n=======\n(their replacement)\n>>>>>>> REPLACE\nEach SEARCH must match exactly \
one place in its file: include enough lines around the change to make it unique. Use as many \
blocks as the correction needs. Do not output a file that you do not change.";

/// The output contract of a joint repair that answers with whole files.
const JOINT_WHOLE_RULE: &str = "Output each file that you change, complete: a heading line \
with the path in backticks (### `path/to/file`), then one fenced code block with the whole \
file. Do not output a file that you do not change.";

/// The parts of a joint edit answer ([`JOINT_EDIT_RULE`]): for each heading
/// line that names a file of `editable` in backticks, that file and the text
/// up to the next such heading. Text before the first heading belongs to
/// `first` (the file that the repair is of).
fn joint_edit_sections(answer: &str, first: &str, editable: &[String]) -> Vec<(String, String)> {
    let mut sections: Vec<(String, String)> = vec![(first.to_string(), String::new())];
    for line in answer.lines() {
        let heading = line
            .trim_start()
            .starts_with('#')
            .then(|| {
                editable
                    .iter()
                    .find(|file| line.contains(&format!("`{file}`")))
            })
            .flatten();
        match heading {
            Some(file) => sections.push((file.clone(), String::new())),
            None => {
                if let Some((_, text)) = sections.last_mut() {
                    text.push_str(line);
                    text.push('\n');
                }
            }
        }
    }
    sections.retain(|(_, text)| text.contains("<<<<<<< SEARCH"));
    sections
}

/// The text of project files for a prompt: `wanted` in order, within
/// `budget` characters, each file once, never `skip` or a stylesheet.
fn sources(wanted: &[String], skip: &str, files: &BTreeMap<&str, &str>, budget: usize) -> String {
    let mut context = String::new();
    let mut seen: BTreeSet<&str> = BTreeSet::new();
    for file in wanted {
        let Some((path, source)) = files.get_key_value(file.as_str()) else {
            continue;
        };
        let source = source.trim();
        if *path == skip
            || path.ends_with(".css")
            || !seen.insert(path)
            || context.len() + source.len() > budget
        {
            continue;
        }
        let _ = write!(context, "\n`{path}`:\n{source}\n");
    }
    context
}

/// The imports of `current` (the file `path`): the project files they name,
/// and the specifiers that name no project file.
pub(crate) fn imports(
    path: &str,
    current: &str,
    files: &BTreeMap<&str, &str>,
    hooks: &RepairHooks,
) -> (Vec<String>, Vec<String>) {
    let known: BTreeSet<&str> = files.keys().copied().collect();
    let mut found = Vec::new();
    let mut missing = Vec::new();
    for spec in (hooks.local_specifiers)(current) {
        match (hooks.resolve_import)(path, &spec, &known) {
            Some(file) => found.push(file),
            None => missing.push(spec),
        }
    }
    (found, missing)
}

/// The repair request for one file: its first prompt, its `problems`, the
/// project files it imports and the shared types, its `current` text and the
/// output rule (edit blocks with `edits`, else the whole file). `added`
/// lists the missing imports that another worker writes in this round.
#[allow(clippy::too_many_arguments)]
pub fn repair_prompt(
    task: &Task,
    problems: &str,
    current: &str,
    files: &BTreeMap<&str, &str>,
    added: &[String],
    edits: bool,
    hooks: &RepairHooks,
    context_chars: usize,
) -> String {
    let path = task.title.as_str();
    let (mut wanted, missing) = imports(path, current, files, hooks);
    // The files that the problems name.
    wanted.extend(
        files
            .keys()
            .filter(|file| problems.contains(**file))
            .map(|file| (*file).to_string()),
    );
    // The shared types always: a file that declares its own copy of a type
    // does not import the owner.
    wanted.extend(
        files
            .keys()
            .filter(|file| (hooks.is_shared_types)(file))
            .map(|file| (*file).to_string()),
    );
    let context = sources(&wanted, path, files, context_chars);
    let mut prompt = task.prompt.clone();
    let _ = write!(
        prompt,
        "\n\n# Check results\nThe assembled project was type-checked and its tests were run. \
         Problems in `{path}`:\n```\n{problems}```\n"
    );
    if !context.is_empty() {
        let _ = write!(
            prompt,
            "\nProject files as they are now (the files that `{path}` imports, and the shared \
             types). They do not change: use only the names, properties, signatures and literal \
             values that they declare.\n{context}"
        );
    }
    for spec in &missing {
        if added.contains(spec) {
            let _ = writeln!(
                prompt,
                "\nThe file behind the import `{spec}` is being written now by another worker. \
                 Keep that import and the way you use it."
            );
        } else {
            let _ = writeln!(
                prompt,
                "\nThe import `{spec}` names a file that is not in the project and will not be \
                 added. Write what you need from it inside `{path}`, in full."
            );
        }
    }
    let rules = if (hooks.is_test)(path) {
        TEST_REPAIR_RULE
    } else {
        "Correct this file so that it compiles against the files shown and the problems above \
         are gone. If it declares a type that the shared types export, delete that declaration \
         and import the type. Keep every feature and every export of the file. Do not shorten \
         it and do not leave a placeholder."
    };
    let output_rule = if edits {
        EDIT_RULE
    } else {
        "Output the COMPLETE corrected file in exactly one fenced code block."
    };
    let _ = write!(
        prompt,
        "\n{rules}\n\nYour current version of `{path}`:\n{}\n\n{output_rule}",
        current.trim()
    );
    prompt
}

/// The most tokens the answer of a joint repair of `editable` needs: its
/// files whole, half as much again, and 1,024 (a character is at most a
/// third of a token here; at least 2,048). Without it the limit is
/// `RepairPolicy::joint_max_tokens` whatever the files' size, and an
/// answer that ran away wrote 14,098 tokens (96 s) for a page of 5,400.
fn joint_room(editable: &[String], files: &BTreeMap<&str, &str>) -> u32 {
    let chars: usize = editable
        .iter()
        .filter_map(|file| files.get(file.as_str()))
        .map(|text| text.len())
        .sum();
    ((chars / 3 * 3 / 2 + 1_024).max(2_048)).min(u32::MAX as usize) as u32
}

/// The repair task of one file alone (the request of [`repair_prompt`]):
/// edit blocks when the file is long, its problems are few and `full_only`
/// is not set (and no import of it is being written, `writing`), else the
/// whole file. Returns the task and whether it asks for edit blocks. The
/// repair loop and the in-wave repair (`crate::inwave`) build it here.
#[allow(clippy::too_many_arguments)]
pub(crate) fn file_repair(
    task: &Task,
    problems: &str,
    text: &str,
    outputs: &BTreeMap<&str, &str>,
    writing: &[String],
    full_only: bool,
    policy: &RepairPolicy,
    hooks: &RepairHooks,
) -> (Task, bool) {
    let edits = writing.is_empty()
        && !full_only
        && text.lines().count() >= policy.edit_min_lines
        && (problems.lines().count() <= policy.edit_max_problem_lines
            || (policy.test_edits && (hooks.is_test)(&task.title)));
    let mut repair = task.clone();
    repair.prompt = repair_prompt(
        task,
        problems,
        text,
        outputs,
        writing,
        edits,
        hooks,
        policy.context_chars,
    );
    if policy.contract_side && (hooks.is_test)(&task.title) {
        repair.prompt = repair.prompt.replace(TEST_REPAIR_RULE, CONTRACT_TEST_RULE);
    }
    repair.deps.clear();
    if edits {
        // The answer is edit blocks, not a file.
        repair.require_closed_fences = false;
        repair.validate_artifact = false;
        // Edit blocks are shorter than the file: an answer past the file's
        // own room ran away (a test's edit answer wrote 8,192 tokens in 20 s
        // and was thrown away; the next answer had 209).
        let files = BTreeMap::from([(task.title.as_str(), text)]);
        repair.max_tokens = repair
            .max_tokens
            .min(joint_room(std::slice::from_ref(&task.title), &files));
    }
    (repair, edits)
}

/// The task for a file `new_path` that `importer` imports as `spec` but that
/// nobody wrote.
pub fn missing_module_task(
    importer: &Task,
    current: &str,
    spec: &str,
    new_path: &str,
    files: &BTreeMap<&str, &str>,
    hooks: &RepairHooks,
    context_chars: usize,
) -> Task {
    let path = importer.title.as_str();
    let (mut wanted, _) = imports(path, current, files, hooks);
    wanted.extend(
        files
            .keys()
            .filter(|file| (hooks.is_shared)(file))
            .map(|file| (*file).to_string()),
    );
    let context = sources(&wanted, path, files, context_chars);
    let mut task = importer.clone();
    task.id = format!(
        "added-{}",
        new_path
            .chars()
            .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '-' })
            .collect::<String>()
    );
    task.title = new_path.to_string();
    task.deps.clear();
    let _ = write!(
        task.prompt,
        "\n\n# Change of assignment\nDo NOT write `{path}`. Write the new file `{new_path}`. \
         `{path}` (shown below) imports it as `{spec}`, but nobody wrote it. Write `{new_path}` \
         in full, so that `{path}` compiles and the feature that `{path}` hands to it works \
         completely: export exactly what `{path}` imports, give it the props that `{path}` \
         passes{} Import only project files that are shown below.\n\n`{path}`:\n{}\n\nProject \
         files as they are now:\n{context}\nOutput the COMPLETE file `{new_path}` in exactly \
         one fenced code block.",
        hooks.new_file_rule,
        current.trim()
    );
    task
}

/// Types that the problems show with two declarations: the compiler writes
/// `import("lib/storage").AppState` and `import("components/Store").AppState`.
/// Returns (type name, modules that declare it).
fn double_declarations(problems: &str) -> Vec<(String, Vec<String>)> {
    let mut found: Vec<(String, Vec<String>)> = Vec::new();
    for part in problems.split("import(\"").skip(1) {
        let Some((module, rest)) = part.split_once("\").") else {
            continue;
        };
        let name: String = rest
            .chars()
            .take_while(|ch| ch.is_ascii_alphanumeric() || *ch == '_')
            .collect();
        if name.is_empty() {
            continue;
        }
        match found.iter_mut().find(|(known, _)| *known == name) {
            Some((_, modules)) => {
                if !modules.iter().any(|known| known == module) {
                    modules.push(module.to_string());
                }
            }
            None => found.push((name, vec![module.to_string()])),
        }
    }
    found.retain(|(_, modules)| modules.len() > 1);
    found
}

/// The request of a joint repair of `task`, which may change `editable`.
pub fn joint_prompt(
    task: &Task,
    problems: &str,
    editable: &[String],
    files: &BTreeMap<&str, &str>,
    hooks: &RepairHooks,
    context_chars: usize,
) -> String {
    let path = task.title.as_str();
    let text = files.get(path).copied().unwrap_or_default();
    let wanted: Vec<String> = imports(path, text, files, hooks)
        .0
        .into_iter()
        .filter(|file| !editable.contains(file))
        .collect();
    let mut doubles = String::new();
    for (name, modules) in double_declarations(problems) {
        let _ = writeln!(
            doubles,
            "The type `{name}` has a declaration in each of these files: `{}`. They differ. \
             Keep ONE declaration, give it every field that the other one has, delete the \
             other declaration, and import `{name}` there from the file that keeps it.",
            modules.join("`, `")
        );
    }
    let mut prompt = task.prompt.clone();
    let _ = write!(
        prompt,
        "\n\n# Joint repair\nA repair of `{path}` alone did not remove these problems:\n\
         ```\n{problems}```\nThe cause can be between two files: two files declare the same type \
         differently, or a file uses a property that the shared type does not have. Decide \
         which side is right for the app, and make the files agree. {}If two files declare the \
         same type, keep one declaration (in the shared types file if there is one) and import \
         it in the other file. Keep every feature.\n{doubles}\nYou may change these files:\n{}\n\
         Other project files, which do not change:\n{}\n{JOINT_WHOLE_RULE}",
        hooks.joint_rules,
        sources(editable, "", files, context_chars),
        sources(&wanted, path, files, context_chars),
    );
    prompt
}

/// At most this many other files join a repair because its problems name them.
pub const NAMED_FILES_MAX: usize = 2;

/// The other project files that the problems of `path` name in backticks as
/// the place to change (``give the hit its task id in `lib/search.ts` ``):
/// files of `files` only, not `node_modules`, a test file only for a test, at
/// most [`NAMED_FILES_MAX`], in order. A file named as context only
/// (``import `X` from `lib/types.ts` ``, ``declared in `lib/types.ts` ``) is
/// not the place to change.
pub fn named_files(
    path: &str,
    problems: &str,
    files: &BTreeMap<&str, &str>,
    is_test: fn(&str) -> bool,
) -> Vec<String> {
    const CONTEXT: &[&str] = &["from", "declared in", "defined in", "exported by"];
    let test = is_test(path);
    let mut named: Vec<String> = Vec::new();
    let mut before = "";
    for part in problems.split('`') {
        let prose = std::mem::replace(&mut before, part)
            .trim_end()
            .to_lowercase();
        let candidate = part.trim().trim_start_matches("./");
        if candidate.is_empty()
            || CONTEXT.iter().any(|word| prose.ends_with(word))
            || candidate == path
            || candidate.contains("node_modules")
            || (!test && is_test(candidate))
            || !files.contains_key(candidate)
            || named.iter().any(|known| known == candidate)
        {
            continue;
        }
        named.push(candidate.to_string());
        if named.len() == NAMED_FILES_MAX {
            break;
        }
    }
    named
}

/// A [`joint_prompt`] for a file whose problems name the other files: no
/// repair of the file alone came before it.
pub fn named_joint_prompt(joint: &str, path: &str) -> String {
    joint.replacen(
        &format!("A repair of `{path}` alone did not remove these problems:"),
        &format!("These problems of `{path}` name another file as the place to change:"),
        1,
    )
}

/// Check the files of `report`; put the checker's `PATCH` files in place
/// with no model call and check once more if any applied.
async fn check_and_patch(
    checker: &dyn Checker,
    report: &mut RunReport,
    key: &str,
) -> Result<CheckResult> {
    let result = checker
        .check(&report_files(report), CheckMode::Full, key)
        .await?;
    if result.patches.is_empty() || apply_patches(report, &result.patches).is_empty() {
        return Ok(result);
    }
    checker
        .check(&report_files(report), CheckMode::Full, key)
        .await
}

/// Append to the answer log of `policy`, if it names one.
pub(crate) fn log_answer(policy: &RepairPolicy, text: &str) {
    let Some(path) = &policy.answer_log else {
        return;
    };
    if let Ok(mut log) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = writeln!(log, "{text}");
    }
}

/// Check the files in `report` with `checker` and repair them, for at most
/// `policy.max_rounds` rounds. `report` ends with the best files that the
/// checker saw (the tokens of every round stay counted). A round can add a
/// file to `plan`: a module that a file imports but that nobody wrote
/// (only with hooks that name one). `hooks` is usually the profile's
/// `repair`. The checker's [`Checker::finish`] runs at the end, also after
/// an error; an error of `finish` alone is logged and the summary is still
/// returned (`report` already holds the kept files).
pub async fn check_and_repair(
    plan: &mut Plan,
    report: &mut RunReport,
    client: Arc<dyn ModelClient>,
    config: SchedulerConfig,
    checker: &dyn Checker,
    policy: RepairPolicy,
    hooks: &RepairHooks,
) -> Result<RepairSummary> {
    check_and_repair_indexed(plan, report, client, config, checker, policy, hooks, None).await
}

/// [`check_and_repair`] whose guards ([`RepairPolicy::guard`]) look up who
/// uses what in `index`; `None` reads the sources
/// ([`TextIndex`](crate::guard::TextIndex)). With the guards off `index` is
/// not used.
#[allow(clippy::too_many_arguments)]
pub async fn check_and_repair_indexed(
    plan: &mut Plan,
    report: &mut RunReport,
    client: Arc<dyn ModelClient>,
    config: SchedulerConfig,
    checker: &dyn Checker,
    policy: RepairPolicy,
    hooks: &RepairHooks,
    index: Option<&dyn CodeIndex>,
) -> Result<RepairSummary> {
    let seed = RepairSeed::default();
    check_and_repair_seeded(
        plan, report, client, config, checker, policy, hooks, index, &seed,
    )
    .await
}

/// What a repair before the loop did (the in-wave repair, `crate::inwave`):
/// the loop takes it as its round before. Empty: the loop as it always ran.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepairSeed {
    /// Files whose repair answered (its output was put in place, or its edit
    /// blocks could not be placed): a file among them that still has
    /// problems is stuck, as after a round of the loop.
    pub repaired: BTreeSet<String>,
    /// Files whose edit blocks could not be placed: written in full.
    pub full_only: BTreeSet<String>,
    /// The wave's own output of each file that a repair replaced, by task
    /// id: the loop keeps it when it checks nearer to a pass, and a build
    /// that runs no check after the wave puts it back ([`Self::restore`]).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub unrepaired: BTreeMap<String, String>,
}

impl RepairSeed {
    /// Put the wave's own outputs back in `report` (the repairs' tokens stay
    /// counted); returns how many files changed.
    pub fn restore(&self, report: &mut RunReport) -> usize {
        let mut restored = 0;
        for (id, output) in &self.unrepaired {
            if let Some(record) = report.tasks.get_mut(id) {
                record.output = Some(output.clone());
                restored += 1;
            }
        }
        restored
    }
}

/// [`check_and_repair_indexed`] after the repairs of `seed`.
#[allow(clippy::too_many_arguments)]
pub async fn check_and_repair_seeded(
    plan: &mut Plan,
    report: &mut RunReport,
    client: Arc<dyn ModelClient>,
    config: SchedulerConfig,
    checker: &dyn Checker,
    policy: RepairPolicy,
    hooks: &RepairHooks,
    index: Option<&dyn CodeIndex>,
    seed: &RepairSeed,
) -> Result<RepairSummary> {
    let outcome = repair_rounds(
        plan, report, client, config, checker, &policy, hooks, index, seed,
    )
    .await;
    if let Err(error) = checker.finish(&policy.check_key).await {
        tracing::warn!(%error, key = %policy.check_key, "the checker could not finish");
    }
    outcome
}

#[allow(clippy::too_many_arguments)]
async fn repair_rounds(
    plan: &mut Plan,
    report: &mut RunReport,
    client: Arc<dyn ModelClient>,
    config: SchedulerConfig,
    checker: &dyn Checker,
    policy: &RepairPolicy,
    hooks: &RepairHooks,
    index: Option<&dyn CodeIndex>,
    seed: &RepairSeed,
) -> Result<RepairSummary> {
    let started = Instant::now();
    let key = policy.check_key.as_str();
    let budget = policy.context_chars;
    // The wave's own text of the files that a repair before the loop
    // replaced (`seed.unrepaired`), as the loop sees it.
    let unrepaired = (!seed.unrepaired.is_empty()).then(|| {
        let mut wave = report.clone();
        seed.restore(&mut wave);
        wave
    });
    (hooks.prepare)(plan, report);
    let before = check_and_patch(checker, report, key).await?;
    // The files of each reported result, for `Checker::confirm` at the end.
    let before_files = report_files(report);
    let mut round_files: Vec<BTreeMap<String, String>> = Vec::new();
    // `current` is the state that the next round repairs; `best` is the
    // state that is kept at the end.
    let mut current = before.clone();
    let mut best = before.clone();
    let mut best_report = report.clone();
    let mut best_planned = plan.tasks.len();
    // The repairs before the loop are its round before: their state is
    // repaired on, but the wave's own text stays the kept state when it
    // checks nearer to a pass (as a round that is not kept).
    if let Some(mut wave) = unrepaired.filter(|_| !before.passed) {
        let check_started = Instant::now();
        (hooks.prepare)(plan, &mut wave);
        let wave_result = check_and_patch(checker, &mut wave, key).await?;
        let kept_wave = wave_result.passed || wave_result.nearer_than(&before);
        tracing::info!(
            files = seed.unrepaired.len(),
            repaired_failed = before.failed,
            wave_failed = wave_result.failed,
            kept_wave,
            check_ms = check_started.elapsed().as_millis() as u64,
            "repair seed: the wave's own text checked as the kept state"
        );
        if kept_wave {
            if wave_result.passed {
                // Nothing left to repair.
                current = wave_result.clone();
                *report = wave.clone();
            }
            best = wave_result;
            best_report = wave;
        }
    }
    let mut rounds: Vec<Round> = Vec::new();
    let mut changed: Vec<String> = Vec::new();
    let mut repaired: BTreeSet<String> = BTreeSet::new();
    let mut stale = 0;
    // Files that are written in full from now on: their edits could not be
    // placed, or a repair did not remove their problems.
    let mut full_only: BTreeSet<String> = seed.full_only.clone();
    // Files whose warnings were given to a repair (one time for each file).
    let mut hinted: BTreeSet<String> = BTreeSet::new();
    // Files that the round before repaired with a joint repair.
    let mut joint_before: BTreeSet<String> = BTreeSet::new();
    // `contract_side`: the tests whose repair named the module as the wrong
    // side.
    let mut module_named: BTreeSet<String> = BTreeSet::new();
    // Files that the round before repaired.
    let mut last_repaired: BTreeSet<String> = seed.repaired.clone();
    // Test files that the round before repaired alone, as stuck tests.
    let mut test_alone_before: BTreeSet<String> = BTreeSet::new();
    // The guards: on with `policy.guard`, with the sources as the index
    // unless the host gives one.
    let text_index = crate::guard::TextIndex;
    let index: Option<&dyn CodeIndex> = policy
        .guard
        .then(|| index.unwrap_or(&text_index as &dyn CodeIndex));
    let mut guard = index.map(|index| GuardReport {
        index: index.name().to_string(),
        ..GuardReport::default()
    });
    // (module, name) pairs that the root-first guard handled.
    let mut root_done: BTreeSet<(String, String)> = BTreeSet::new();
    // Module -> the files that use an export that the round before broke.
    let mut followers: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();

    // `test_stale_limit`: the stale limit while only test files have problems.
    let stale_limit = |result: &CheckResult| {
        let tests_only =
            !result.problems.is_empty() && result.problems.keys().all(|file| (hooks.is_test)(file));
        if tests_only {
            policy.stale_limit.min(policy.test_stale_limit)
        } else {
            policy.stale_limit
        }
    };
    // Joint repairs that answer with edits (`joint_edits`): task ids.
    let mut joint_edit: BTreeSet<String> = BTreeSet::new();

    while !current.passed && rounds.len() < policy.max_rounds && stale < stale_limit(&current) {
        joint_edit.clear();
        let round_started = Instant::now();
        // The problems that this round repairs: the check's, unless a guard
        // fixed or held some lines.
        let mut problems_now = current.problems.clone();
        let mut root_fixed: Vec<String> = Vec::new();
        // Files whose own lines the root-first guard fixed: they wait for
        // the next check.
        let mut root_touched: BTreeSet<String> = BTreeSet::new();
        // The files that a repair may change.
        let repairable: BTreeSet<String> = plan
            .tasks
            .iter()
            .filter(|task| task.static_output.is_none() && !task.internal)
            .map(|task| task.title.clone())
            .collect();
        if let Some(guard) = guard.as_mut() {
            let texts = report_files(report);
            let root = root_first(&problems_now, &texts, hooks, &root_done, &repairable);
            root_done.extend(root.handled.iter().cloned());
            for (path, text) in &root.fixes {
                let record = plan
                    .tasks
                    .iter()
                    .find(|task| task.title == *path)
                    .and_then(|task| report.tasks.get_mut(&task.id));
                if let Some(record) = record {
                    record.output = Some(crate::project::fenced(text));
                    guard.root_fixes += 1;
                    root_fixed.push(path.clone());
                }
            }
            root_touched.extend(root.fixed.keys().cloned());
            for (file, lines) in root.fixed.iter().chain(root.held.iter()) {
                if let Some(text) = problems_now.get_mut(file) {
                    *text = without_lines(text, lines);
                }
            }
            problems_now.retain(|_, text| !text.is_empty());
            guard.held += root.held.len();
            for (module, text) in root.repairs {
                problems_now.entry(module).or_default().push_str(&text);
                guard.root_repairs += 1;
            }
        }
        // The files as this round starts (for the export locks).
        let texts_now = if index.is_some() {
            report_files(report)
        } else {
            BTreeMap::new()
        };
        // The export locks of this round, each with the files that the
        // repair itself may change.
        let mut locks: Vec<(Lock, Vec<String>)> = Vec::new();
        let mut repairs: Vec<Task> = Vec::new();
        let mut added: Vec<Task> = Vec::new();
        let mut edited: BTreeSet<String> = BTreeSet::new();
        let mut test_alone: BTreeSet<String> = BTreeSet::new();
        // Joint repairs: task id -> the files that the answer can change.
        let mut joint: BTreeMap<String, Vec<String>> = BTreeMap::new();
        let ids: BTreeMap<String, String> = plan
            .tasks
            .iter()
            .map(|task| (task.title.clone(), task.id.clone()))
            .collect();
        {
            let outputs: BTreeMap<&str, &str> = plan
                .tasks
                .iter()
                .filter_map(|task| {
                    let record = report.tasks.get(&task.id)?;
                    (record.status == TaskStatus::Succeeded)
                        .then_some(record.output.as_deref())
                        .flatten()
                        .map(|output| (task.title.as_str(), output))
                })
                .collect();
            // The guards' waits. A file whose own lines the root-first guard
            // fixed, or that imports a file it fixed, waits for the next
            // check (a round of fixes alone asks the model nothing): its
            // other lines are often knock-ons of the same missing name. A
            // file whose only problems are knock-on lines waits while a file
            // it imports is sent to the model or was fixed (a fixed file up
            // to `KNOCK_ON_DEPTH` import levels down), never for a file
            // that this round leaves as it is (so two such files never wait
            // for each other).
            let mut root_waiting: BTreeSet<String> = BTreeSet::new();
            let mut knock_on_waiting: BTreeSet<String> = BTreeSet::new();
            if guard.is_some() {
                let mut knock_on: Vec<(&str, Vec<String>)> = Vec::new();
                let mut sending: BTreeSet<&str> = BTreeSet::new();
                for task in plan
                    .tasks
                    .iter()
                    .filter(|task| repairable.contains(&task.title))
                {
                    let (Some(text), Some(problems)) = (
                        outputs.get(task.title.as_str()),
                        problems_now.get(&task.title),
                    ) else {
                        continue;
                    };
                    let imported = imports(&task.title, text, &outputs, hooks).0;
                    if root_touched.contains(&task.title)
                        || imported.iter().any(|file| root_fixed.contains(file))
                    {
                        root_waiting.insert(task.title.clone());
                    } else if only_knock_on(problems, hooks) {
                        knock_on.push((task.title.as_str(), imported));
                    } else {
                        sending.insert(task.title.as_str());
                    }
                }
                for (title, imported) in knock_on {
                    // A fixed module's types reach a file through the files
                    // it imports too (a page that imports a store whose
                    // state type uses the fixed type), so that wait follows
                    // the imports a few levels down.
                    let mut seen: BTreeSet<String> = imported.iter().cloned().collect();
                    let mut level = imported.clone();
                    let mut root_below = false;
                    for _ in 0..KNOCK_ON_DEPTH {
                        if level.iter().any(|file| root_fixed.contains(file)) {
                            root_below = true;
                            break;
                        }
                        let mut next = Vec::new();
                        for file in &level {
                            if let Some(source) = outputs.get(file.as_str()) {
                                for below in imports(file, source, &outputs, hooks).0 {
                                    if seen.insert(below.clone()) {
                                        next.push(below);
                                    }
                                }
                            }
                        }
                        level = next;
                    }
                    if root_below || imported.iter().any(|file| sending.contains(file.as_str())) {
                        knock_on_waiting.insert(title.to_string());
                    }
                }
            }
            // The files that a joint repair of this round may change: a
            // named file joins one joint repair only (two answers for one
            // file: the last would win).
            let mut claimed: BTreeSet<String> = BTreeSet::new();
            for task in plan
                .tasks
                .iter()
                .filter(|task| task.static_output.is_none() && !task.internal)
            {
                let Some(text) = outputs.get(task.title.as_str()) else {
                    continue;
                };
                let mut problems = problems_now.get(&task.title).cloned().unwrap_or_default();
                if let Some(guard) = guard.as_mut() {
                    if root_waiting.contains(&task.title) {
                        guard.root_waits += 1;
                        continue;
                    }
                    if knock_on_waiting.contains(&task.title) {
                        guard.knock_on_skipped += 1;
                        continue;
                    }
                }
                // A file that does not render often fails for a reason
                // outside the file (a provider that the layout does not set
                // up).
                let render = problems
                    .lines()
                    .any(|line| (hooks.classify)(line) == Problem::Render);
                let stuck = !problems.is_empty() && (render || last_repaired.contains(&task.title));
                // A warning about a second copy of a type matters only when
                // the compiler reports a conflict with that copy: its message
                // names the module, as in `import("lib/tasks").Task`.
                let module = task
                    .title
                    .rsplit_once('.')
                    .map_or(task.title.as_str(), |(stem, _)| stem);
                let conflict = format!("import(\"{module}\")");
                let in_conflict = current
                    .problems
                    .values()
                    .any(|text| text.contains(&conflict));
                if let Some(hint) = current.hints.get(&task.title).filter(|_| in_conflict) {
                    if hinted.insert(task.title.clone()) {
                        problems.push_str(hint);
                    }
                }
                if problems.is_empty() {
                    continue;
                }
                let is_test = (hooks.is_test)(&task.title);
                // A test that failed after its own repair and after the
                // repair below: the code can be the wrong side. Repair it
                // together with the modules that the failing cases name.
                let twice =
                    joint_before.contains(&task.title) || test_alone_before.contains(&task.title);
                // `contract_side`: the first repair left the cases that
                // the contract backs (its answer named the module); the modules
                // join at once.
                let contract_side =
                    policy.contract_side && is_test && module_named.contains(&task.title);
                if stuck && is_test && policy.stuck_tests_alone && !twice && !contract_side {
                    test_alone.insert(task.title.clone());
                    // A test changes to agree with the code. Its repair
                    // never changes the code.
                    let mut repair = task.clone();
                    let edits = policy.test_edits
                        && !full_only.contains(&task.title)
                        && text.lines().count() >= policy.edit_min_lines;
                    repair.prompt =
                        repair_prompt(task, &problems, text, &outputs, &[], edits, hooks, budget);
                    repair.prompt.push_str(STUCK_TEST_RULE);
                    if edits {
                        // The stuck rule allows what this sentence forbids.
                        repair.prompt =
                            repair
                                .prompt
                                .replacen(" Never delete, skip or merge a test.", "", 1);
                        repair.require_closed_fences = false;
                        repair.validate_artifact = false;
                        edited.insert(task.id.clone());
                    }
                    // The same request at the same temperature gave the
                    // same wrong answer: ask for another one.
                    repair.temperature = Some(policy.escalate_temperature);
                    repair.deps.clear();
                    repairs.push(repair);
                    continue;
                }
                // The files that the problems name as the place to change
                // join the repair at once, unless they have problems of
                // their own (their own repair writes them this round), a
                // repair may not change them (scaffold, internal) or an
                // earlier repair of this round may change them. A test that
                // is not stuck yet is repaired alone: it names no files.
                let named: Vec<String> = if is_test && !stuck {
                    Vec::new()
                } else {
                    named_files(&task.title, &problems, &outputs, hooks.is_test)
                        .into_iter()
                        .filter(|file| {
                            repairable.contains(file)
                                && !claimed.contains(file)
                                && problems_now
                                    .get(file)
                                    .is_none_or(|text| text.trim().is_empty())
                        })
                        .collect()
                };
                let missing = problems
                    .lines()
                    .any(|line| (hooks.classify)(line) == Problem::MissingModule);
                let joins = !named.is_empty() && (stuck || !missing);
                let named_now = joins && !stuck;
                let mut editable = if stuck {
                    (hooks.joint_editable)(&task.title, &problems, &outputs)
                } else {
                    Vec::new()
                };
                if joins {
                    if editable.is_empty() {
                        editable.push(task.title.clone());
                    }
                    for file in named {
                        if !editable.contains(&file) {
                            editable.push(file);
                        }
                    }
                }
                // The files that use an export that the round before broke
                // join the joint repair of that module, and the module joins
                // theirs.
                if let Some(guard) = guard.as_mut().filter(|_| !editable.is_empty()) {
                    let before = editable.len();
                    let mut wanted: Vec<String> = followers
                        .get(&task.title)
                        .into_iter()
                        .flatten()
                        .cloned()
                        .collect();
                    wanted.extend(
                        followers
                            .iter()
                            .filter(|(_, users)| users.contains(&task.title))
                            .map(|(module, _)| module.clone()),
                    );
                    for file in wanted {
                        if !editable.contains(&file)
                            && outputs.contains_key(file.as_str())
                            && editable.len() < JOINT_GUARD_FILES
                        {
                            editable.push(file);
                        }
                    }
                    guard.followers_added += editable.len() - before;
                }
                if !editable.is_empty() {
                    // Its own repair did not help, or its problems name
                    // another file as the place to change: repair it together
                    // with the files that its problems name.
                    let mut repair = task.clone();
                    repair.prompt =
                        joint_prompt(task, &problems, &editable, &outputs, hooks, budget);
                    if named_now {
                        repair.prompt = named_joint_prompt(&repair.prompt, &task.title);
                    }
                    // Edits: every joint repair (`joint_edits`: also the
                    // second repair of a file whose own repair left a
                    // problem; whole, a 579-line page was 5,431 tokens and
                    // 30 s for one line), and a stuck test with its modules
                    // (`test_edits`).
                    if (policy.joint_edits || (is_test && policy.test_edits))
                        && !editable.iter().any(|file| full_only.contains(file))
                    {
                        repair.prompt =
                            repair.prompt.replacen(JOINT_WHOLE_RULE, JOINT_EDIT_RULE, 1);
                        repair.require_closed_fences = false;
                        joint_edit.insert(task.id.clone());
                    }
                    repair.deps.clear();
                    repair.validate_artifact = false;
                    repair.max_tokens = repair
                        .max_tokens
                        .max(policy.joint_max_tokens)
                        .min(joint_room(&editable, &outputs));
                    if is_test {
                        // The test can be right and the module wrong. To
                        // see which, the model must follow the code: that
                        // needs reasoning.
                        repair.prompt.push_str(if contract_side {
                            CONTRACT_SIDE_RULE
                        } else {
                            TRACE_RULE
                        });
                        if let Some(effort) = &policy.escalate_effort {
                            if policy.escalate_tests {
                                repair.reasoning_effort = Some(effort.clone());
                            }
                        }
                    }
                    if joint_before.contains(&task.title) {
                        // The same request gave an answer that did not
                        // help: ask for a different one, with reasoning.
                        repair.temperature = Some(policy.escalate_temperature);
                        if let Some(effort) = &policy.escalate_effort {
                            if !is_test || policy.escalate_tests {
                                repair.reasoning_effort = Some(effort.clone());
                            }
                        }
                    }
                    if let (Some(index), Some(guard)) = (index, guard.as_mut()) {
                        let users = index.users(&task.title, &texts_now, hooks).await;
                        let source = texts_now.get(&task.title).map_or("", String::as_str);
                        if let Some(lock) = Lock::new(&task.title, source, users, hooks) {
                            let lock = lock.without(&editable);
                            let block = lock_block(&lock);
                            if !block.is_empty() {
                                repair.prompt.push_str(&block);
                                guard.lock_blocks += 1;
                            }
                            locks.push((lock, editable.clone()));
                        }
                    }
                    claimed.extend(editable.iter().cloned());
                    joint.insert(task.id.clone(), editable);
                    repairs.push(repair);
                    continue;
                }
                // A missing import becomes a new file.
                let mut writing = Vec::new();
                for spec in imports(&task.title, text, &outputs, hooks).1 {
                    let Some(new_path) = (hooks.missing_module_path)(&task.title, &spec) else {
                        continue;
                    };
                    if !added.iter().any(|extra| extra.title == new_path) {
                        added.push(missing_module_task(
                            task, text, &spec, &new_path, &outputs, hooks, budget,
                        ));
                    }
                    writing.push(spec);
                }
                // The file itself needs no repair if its only problem is the
                // import of a file that is written now.
                let other_problems = problems.lines().any(|line| {
                    (hooks.classify)(line) != Problem::MissingModule && !line.trim().is_empty()
                });
                if writing.is_empty() || other_problems {
                    let (mut repair, edits) = file_repair(
                        task,
                        &problems,
                        text,
                        &outputs,
                        &writing,
                        full_only.contains(&task.title),
                        policy,
                        hooks,
                    );
                    if let (Some(index), Some(guard)) = (index, guard.as_mut()) {
                        let users = index.users(&task.title, &texts_now, hooks).await;
                        let source = texts_now.get(&task.title).map_or("", String::as_str);
                        if let Some(lock) = Lock::new(&task.title, source, users, hooks) {
                            let block = lock_block(&lock);
                            if !block.is_empty() {
                                repair.prompt.push_str(&block);
                                guard.lock_blocks += 1;
                            }
                            locks.push((lock, Vec::new()));
                        }
                    }
                    if edits {
                        edited.insert(task.id.clone());
                    }
                    repairs.push(repair);
                }
            }
        }
        if repairs.is_empty() && added.is_empty() && root_fixed.is_empty() {
            break;
        }
        last_repaired = repairs.iter().map(|task| task.title.clone()).collect();
        test_alone_before = test_alone;
        joint_before = repairs
            .iter()
            .filter(|task| joint.contains_key(&task.id))
            .map(|task| task.title.clone())
            .collect();
        let mut files: Vec<String> = repairs.iter().map(|task| task.title.clone()).collect();
        files.extend(added.iter().map(|task| task.title.clone()));
        for file in root_fixed {
            if !files.contains(&file) {
                files.push(file);
            }
        }
        let mut tasks = repairs;
        tasks.extend(added.iter().cloned());
        // A round of root-first fixes alone asks the model nothing.
        let mut round_tasks = BTreeMap::new();
        if tasks.is_empty() {
            if let Some(guard) = guard.as_mut() {
                guard.root_only_rounds += 1;
            }
        } else {
            round_tasks = execute_plan(
                &Plan {
                    objective: plan.objective.clone(),
                    tasks,
                    cache_primes: Vec::new(),
                },
                client.clone(),
                config.clone(),
            )
            .await?
            .tasks;
        }

        let mut output_tokens = 0;
        let mut answers: Vec<(String, String, Vec<String>, bool)> = Vec::new();
        for (id, fixed) in &round_tasks {
            let kind = if joint.contains_key(id) {
                "joint"
            } else if edited.contains(id) {
                "edits"
            } else {
                "full"
            };
            log_answer(
                policy,
                &format!(
                    "==== round {} {kind} {id} status {:?} may change {:?}\n{}\n",
                    rounds.len() + 1,
                    fixed.status,
                    joint.get(id),
                    fixed.output.as_deref().unwrap_or("(no output)")
                ),
            );
        }
        for (id, fixed) in &round_tasks {
            output_tokens += fixed.output_tokens;
            // Tokens are spent whether or not the round is kept.
            report.total_input_tokens += fixed.input_tokens;
            report.total_output_tokens += fixed.output_tokens;
            let done = fixed.status == TaskStatus::Succeeded && fixed.output.is_some();
            if let Some(record) = report.tasks.get_mut(id) {
                record.attempts += fixed.attempts;
                record.input_tokens += fixed.input_tokens;
                record.output_tokens += fixed.output_tokens;
                if let Some(editable) = joint.get(id) {
                    if let Some(answer) = fixed.output.clone().filter(|_| done) {
                        answers.push((
                            record.title.clone(),
                            answer,
                            editable.clone(),
                            joint_edit.contains(id),
                        ));
                    }
                } else if done
                    && policy.contract_side
                    && (hooks.is_test)(&record.title)
                    && fixed.output.as_deref().is_some_and(says_module_wrong)
                {
                    // `contract_side`: the answer says that the module breaks
                    // its contract. The test stays as it is (whatever the
                    // answer did to it); it fails again and the next round
                    // repairs it together with the module.
                    tracing::info!(
                        test = %record.title,
                        "contract side: the test repair names the module as the wrong side"
                    );
                    module_named.insert(record.title.clone());
                } else if done && edited.contains(id) {
                    let placed = record
                        .output
                        .as_deref()
                        .zip(fixed.output.as_deref())
                        .and_then(|(current, blocks)| apply_edit_blocks(current, blocks));
                    match placed {
                        Some(text) => record.output = Some(text),
                        None => {
                            full_only.insert(record.title.clone());
                        }
                    }
                } else if done {
                    record.output = fixed.output.clone();
                }
            } else if let Some(extra) = added.iter().find(|task| task.id == *id).filter(|_| done) {
                // A new file: it joins the plan and the report.
                report.tasks.insert(id.clone(), fixed.clone());
                plan.tasks.push(extra.clone());
            }
        }
        // The answers of joint repairs: each names the files that it changes.
        for (stuck, answer, editable, as_edits) in answers {
            log_answer(
                policy,
                &format!("==== joint repair of {stuck}; may change {editable:?}\n{answer}\n"),
            );
            if as_edits {
                // All the edits or none: half an answer leaves the files
                // disagreeing in a new way.
                let mut placed: Vec<(String, String)> = Vec::new();
                let sections = joint_edit_sections(&answer, &stuck, &editable);
                let all = !sections.is_empty()
                    && sections.iter().all(|(file, blocks)| {
                        let current = ids
                            .get(file)
                            .and_then(|id| report.tasks.get(id))
                            .and_then(|record| record.output.as_deref());
                        // Two sections of one file: the second edits the first's result.
                        let current = placed
                            .iter()
                            .rev()
                            .find(|(known, _)| known == file)
                            .map(|(_, text)| text.as_str())
                            .or(current);
                        match current.and_then(|text| apply_edit_blocks(text, blocks)) {
                            Some(text) => {
                                placed.push((file.clone(), text));
                                true
                            }
                            None => false,
                        }
                    });
                if all {
                    for (file, text) in placed {
                        if let Some(record) = ids.get(&file).and_then(|id| report.tasks.get_mut(id))
                        {
                            record.output = Some(text);
                        }
                    }
                } else {
                    full_only.extend(editable.iter().cloned());
                }
                continue;
            }
            let mut applied = false;
            for file in scan(&answer) {
                if !file.closed || !editable.contains(&file.path) {
                    continue;
                }
                if let Some(record) = ids.get(&file.path).and_then(|id| report.tasks.get_mut(id)) {
                    record.output = Some(format!("```\n{}\n```", file.content.trim_end()));
                    applied = true;
                }
            }
            // One code block with no path is the stuck file itself.
            let fences = answer
                .lines()
                .filter(|line| line.trim_start().starts_with("```"))
                .count();
            if !applied && fences == 2 {
                if let Some(record) = ids.get(&stuck).and_then(|id| report.tasks.get_mut(id)) {
                    record.output = Some(answer);
                }
            }
        }
        (hooks.prepare)(plan, report);
        // The export locks: which used exports the round changed, and who
        // uses them (they join the next joint repair).
        if let (Some(index), Some(guard)) = (index, guard.as_mut()) {
            let after_texts = report_files(report);
            let mut next: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
            for (lock, editable) in &locks {
                let Some(new) = after_texts.get(&lock.path) else {
                    continue;
                };
                let broken = lock.broken(new, hooks);
                if broken.is_empty() {
                    continue;
                }
                guard.lock_breaks += broken.len();
                let old = texts_now.get(&lock.path).map_or("", String::as_str);
                let users = index
                    .affected(&lock.path, old, new, &broken, &after_texts, hooks)
                    .await;
                next.entry(lock.path.clone()).or_default().extend(
                    users
                        .into_iter()
                        .filter(|user| *user != lock.path && !editable.contains(user)),
                );
            }
            followers = next;
            index.updated(&after_texts).await;
        }
        let mut after = check_and_patch(checker, report, key).await?;
        changed.extend(files.iter().cloned());
        let mut kept = after.passed || after.nearer_than(&best);
        let after_files = report_files(report);
        // No round follows this result: it must be the checker's exact one
        // (an early answer could end the loop on a problem that the exact
        // tools do not see).
        let last = after.passed
            || rounds.len() + 1 >= policy.max_rounds
            || (!kept && stale + 1 >= stale_limit(&after));
        if last {
            after = checker.confirm(&after_files, after, key).await?;
            kept = after.passed || after.nearer_than(&best);
        }
        if kept {
            best = after.clone();
            best_report = report.clone();
            best_planned = plan.tasks.len();
            repaired.extend(changed.drain(..));
            stale = 0;
        } else {
            stale += 1;
        }
        current = after.clone();
        round_files.push(after_files);
        rounds.push(Round {
            files,
            after,
            kept,
            output_tokens,
            seconds: round_started.elapsed().as_secs_f64(),
        });
    }
    // The reported results are the checker's exact ones.
    let before = checker.confirm(&before_files, before, key).await?;
    for (round, files) in rounds.iter_mut().zip(&round_files) {
        round.after = checker.confirm(files, round.after.clone(), key).await?;
    }
    let best = checker
        .confirm(&report_files(&best_report), best, key)
        .await?;
    // Keep the best files; the tokens of every round stay counted.
    let (input_tokens, output_tokens) = (report.total_input_tokens, report.total_output_tokens);
    *report = best_report;
    report.total_input_tokens = input_tokens;
    report.total_output_tokens = output_tokens;
    plan.tasks.truncate(best_planned);
    Ok(RepairSummary {
        before,
        after: best,
        rounds,
        repaired: repaired.into_iter().collect(),
        seconds: started.elapsed().as_secs_f64(),
        guard,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::{Completion, CompletionRequest};
    use crate::profile::NO_REPAIR_HOOKS;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    /// Answers each request with `answer(prompt, number of the request)`
    /// and keeps the prompts.
    struct Scripted {
        answer: fn(&str, usize) -> String,
        prompts: Mutex<Vec<String>>,
        /// The temperature and the reasoning effort of each request.
        settings: Mutex<Vec<(f32, Option<String>)>>,
    }

    #[async_trait::async_trait]
    impl ModelClient for Scripted {
        async fn complete(&self, request: CompletionRequest) -> Result<Completion> {
            let number = {
                let mut prompts = self.prompts.lock().unwrap();
                prompts.push(request.user.clone());
                self.settings
                    .lock()
                    .unwrap()
                    .push((request.temperature, request.reasoning_effort.clone()));
                prompts.len() - 1
            };
            Ok(Completion {
                text: (self.answer)(&request.user, number),
                provider: None,
                input_tokens: 3,
                output_tokens: 2,
                reasoning_tokens: None,
                cached_input_tokens: None,
                finish_reason: Some("stop".into()),
                usage_is_estimated: false,
                elapsed: std::time::Duration::from_millis(1),
                time_to_first_token: None,
            })
        }
    }

    /// A checker whose verdict is a function of the files.
    struct Judge {
        verdict: fn(&BTreeMap<String, String>) -> CheckResult,
        checks: AtomicUsize,
        finished: AtomicUsize,
        /// `finish` fails (after it counts).
        finish_fails: bool,
    }

    impl Judge {
        fn new(verdict: fn(&BTreeMap<String, String>) -> CheckResult) -> Self {
            Self {
                verdict,
                checks: AtomicUsize::new(0),
                finished: AtomicUsize::new(0),
                finish_fails: false,
            }
        }
    }

    #[async_trait::async_trait]
    impl Checker for Judge {
        async fn check(
            &self,
            files: &BTreeMap<String, String>,
            _: CheckMode,
            _: &str,
        ) -> Result<CheckResult> {
            self.checks.fetch_add(1, Ordering::SeqCst);
            Ok((self.verdict)(files))
        }

        async fn finish(&self, _: &str) -> Result<()> {
            self.finished.fetch_add(1, Ordering::SeqCst);
            if self.finish_fails {
                anyhow::bail!("could not clean up");
            }
            Ok(())
        }
    }

    fn fail(file: &str, failed: u64, line: &str) -> CheckResult {
        CheckResult {
            failed,
            summary: format!("{failed} failed, 0 passed"),
            problems: [(file.to_string(), format!("ERROR {file}(1,1): {line}\n"))]
                .into_iter()
                .collect(),
            ..CheckResult::default()
        }
    }

    fn pass() -> CheckResult {
        CheckResult {
            passed: true,
            summary: "0 failed, 0 passed".into(),
            ..CheckResult::default()
        }
    }

    fn text<'a>(files: &'a BTreeMap<String, String>, path: &str) -> &'a str {
        files.get(path).map_or("", |text| text.trim())
    }

    fn task(id: &str, path: &str) -> Task {
        Task {
            id: id.into(),
            title: path.into(),
            prompt: format!("Write {path}."),
            deps: Vec::new(),
            max_tokens: 64,
            temperature: None,
            require_closed_fences: false,
            validate_artifact: false,
            static_output: None,
            reasoning_effort: None,
            internal: false,
            prefix_split: None,
        }
    }

    /// The plan, the report of its first wave (written by `answer`) and the
    /// client.
    async fn wave(
        paths: &[(&str, &str)],
        answer: fn(&str, usize) -> String,
    ) -> (Plan, RunReport, Arc<Scripted>) {
        let plan = Plan {
            objective: "repair test".into(),
            tasks: paths.iter().map(|(id, path)| task(id, path)).collect(),
            cache_primes: Vec::new(),
        };
        let client = Arc::new(Scripted {
            answer,
            prompts: Mutex::new(Vec::new()),
            settings: Mutex::new(Vec::new()),
        });
        let report = execute_plan(&plan, client.clone(), SchedulerConfig::default())
            .await
            .unwrap();
        (plan, report, client)
    }

    fn output(report: &RunReport, id: &str) -> String {
        crate::project::strip_fence(report.tasks[id].output.as_deref().unwrap_or(""))
            .trim()
            .to_string()
    }

    const JOINT_EDITS: &str = "### `app/search/page.tsx`\n<<<<<<< SEARCH\np1\n=======\nP2\n\
>>>>>>> REPLACE\n### `lib/search.ts`\n<<<<<<< SEARCH\ns1\n=======\nS2\n>>>>>>> REPLACE\n";

    fn search_judge() -> Judge {
        Judge::new(|files| {
            if text(files, "app/search/page.tsx") == "P2" && text(files, "lib/search.ts") == "S2" {
                pass()
            } else {
                CheckResult {
                    failed: 1,
                    summary: "1 failed, 0 passed".into(),
                    problems: [("app/search/page.tsx".to_string(), SEARCH_LINK.to_string())]
                        .into_iter()
                        .collect(),
                    ..CheckResult::default()
                }
            }
        })
    }

    #[tokio::test]
    async fn a_named_joint_repair_answers_with_edits_when_asked() {
        let (mut plan, mut report, client) = wave(
            &[("p", "app/search/page.tsx"), ("s", "lib/search.ts")],
            |prompt, _| {
                if prompt.contains("# Joint repair") {
                    JOINT_EDITS.into()
                } else if prompt.contains("Write app/search/page.tsx.") {
                    "```\np1\n```".into()
                } else {
                    "```\ns1\n```".into()
                }
            },
        )
        .await;
        let summary = check_and_repair(
            &mut plan,
            &mut report,
            client.clone(),
            SchedulerConfig::default(),
            &search_judge(),
            RepairPolicy {
                joint_edits: true,
                ..RepairPolicy::default()
            },
            &NO_REPAIR_HOOKS,
        )
        .await
        .unwrap();
        assert!(summary.after.passed);
        assert_eq!(summary.rounds.len(), 1);
        assert_eq!(output(&report, "p"), "P2");
        assert_eq!(output(&report, "s"), "S2");
        let prompts = client.prompts.lock().unwrap().clone();
        let joint = prompts
            .iter()
            .find(|p| p.contains("# Joint repair"))
            .unwrap();
        assert!(joint.contains(JOINT_EDIT_RULE) && !joint.contains(JOINT_WHOLE_RULE));
    }

    #[tokio::test]
    async fn joint_edits_that_do_not_apply_change_nothing_and_the_next_repair_is_whole() {
        let (mut plan, mut report, client) = wave(
            &[("p", "app/search/page.tsx"), ("s", "lib/search.ts")],
            |prompt, _| {
                if prompt.contains(JOINT_EDIT_RULE) {
                    // The second file's SEARCH is not in the file.
                    JOINT_EDITS.replace("s1", "nowhere")
                } else if prompt.contains("# Joint repair") {
                    "### `app/search/page.tsx`\n```\nP2\n```\n### `lib/search.ts`\n```\nS2\n```\n"
                        .into()
                } else if prompt.contains("Write app/search/page.tsx.") {
                    "```\np1\n```".into()
                } else {
                    "```\ns1\n```".into()
                }
            },
        )
        .await;
        let hooks = RepairHooks {
            joint_editable: |path, _, _| vec![path.to_string(), "lib/search.ts".to_string()],
            ..NO_REPAIR_HOOKS
        };
        let summary = check_and_repair(
            &mut plan,
            &mut report,
            client.clone(),
            SchedulerConfig::default(),
            &search_judge(),
            RepairPolicy {
                joint_edits: true,
                ..RepairPolicy::default()
            },
            &hooks,
        )
        .await
        .unwrap();
        assert!(summary.after.passed);
        assert_eq!(summary.rounds.len(), 2);
        // The first round placed nothing (not the page's edit alone).
        assert!(!summary.rounds[0].kept);
        assert_eq!(output(&report, "p"), "P2");
        let prompts = client.prompts.lock().unwrap().clone();
        assert!(prompts.last().unwrap().contains(JOINT_WHOLE_RULE));
    }

    #[test]
    fn a_joint_repair_has_room_for_its_files_and_half_as_much_again() {
        let big = "x".repeat(18_000);
        let files: BTreeMap<&str, &str> = [("a.ts", big.as_str()), ("b.ts", "short")].into();
        assert_eq!(joint_room(&["a.ts".to_string()], &files), 10_024);
        assert_eq!(
            joint_room(&["b.ts".to_string(), "none.ts".to_string()], &files),
            2_048
        );
    }

    #[test]
    fn an_edit_repair_of_one_file_has_the_room_of_that_file() {
        let mut long = task("t", "tests/a.test.ts");
        long.max_tokens = 8_192;
        let text = "expect(1).toBe(1);
"
        .repeat(100);
        let files: BTreeMap<&str, &str> = [("tests/a.test.ts", text.as_str())].into();
        let policy = RepairPolicy::default();
        let (repair, edits) = file_repair(
            &long,
            "FAILED tests/a.test.ts > a",
            &text,
            &files,
            &[],
            false,
            &policy,
            &NO_REPAIR_HOOKS,
        );
        assert!(edits);
        // 1,900 characters: 950 tokens and 1,024, at least 2,048.
        assert_eq!(repair.max_tokens, 2_048);
        // The whole file keeps the task's budget.
        let (whole, edits) = file_repair(
            &long,
            "FAILED tests/a.test.ts > a",
            &text,
            &files,
            &[],
            true,
            &policy,
            &NO_REPAIR_HOOKS,
        );
        assert!(!edits);
        assert_eq!(whole.max_tokens, 8_192);
    }

    #[test]
    fn joint_edit_sections_split_at_the_headings_of_editable_files() {
        let editable = vec!["a.ts".to_string(), "b.ts".to_string()];
        let answer = "<<<<<<< SEARCH\nx\n=======\ny\n>>>>>>> REPLACE\n### `b.ts`\n<<<<<<< SEARCH\n\
# `c.ts` is not a heading here\n=======\nz\n>>>>>>> REPLACE\n## `a.ts`\nnothing to change\n";
        let sections = joint_edit_sections(answer, "a.ts", &editable);
        assert_eq!(sections.len(), 2);
        assert_eq!(sections[0].0, "a.ts");
        assert!(sections[0].1.contains("x\n=======\ny"));
        assert_eq!(sections[1].0, "b.ts");
        assert!(sections[1].1.contains("# `c.ts` is not a heading here"));
    }

    #[tokio::test]
    async fn test_edits_and_the_test_stale_limit() {
        let (mut plan, mut report, client) =
            wave(&[("t", "a.test.ts"), ("m", "m.ts")], |prompt, number| {
                if prompt.contains("# Check results") {
                    format!(
                        "<<<<<<< SEARCH\nline 42\n=======\nline 42 r{number}\n>>>>>>> REPLACE\n"
                    )
                } else if prompt.contains("a.test.ts") {
                    let lines: Vec<String> = (0..70).map(|n| format!("line {n}")).collect();
                    format!("```\n{}\n```", lines.join("\n"))
                } else {
                    "```\nv1\n```".into()
                }
            })
            .await;
        // Seventy problem lines: over `edit_max_problem_lines`.
        let judge = Judge::new(|_| {
            let mut result = fail("a.test.ts", 1, "expected 1");
            let lines = "  at line\n".repeat(70);
            result
                .problems
                .get_mut("a.test.ts")
                .unwrap()
                .push_str(&lines);
            result
        });
        let hooks = RepairHooks {
            is_test: |path| path.contains(".test."),
            joint_editable: |path, _, _| vec![path.to_string(), "m.ts".to_string()],
            ..NO_REPAIR_HOOKS
        };
        let summary = check_and_repair(
            &mut plan,
            &mut report,
            client.clone(),
            SchedulerConfig::default(),
            &judge,
            RepairPolicy {
                test_edits: true,
                test_stale_limit: 2,
                ..RepairPolicy::default()
            },
            &hooks,
        )
        .await
        .unwrap();
        // Own repair and stuck test alone, both by edits; then the limit.
        assert_eq!(summary.rounds.len(), 2);
        let prompts = client.prompts.lock().unwrap().clone();
        assert_eq!(prompts.len(), 4, "two wave files and two rounds");
        assert!(prompts[2].contains(EDIT_RULE) && prompts[2].contains("Never delete"));
        assert!(prompts[3].contains(EDIT_RULE) && prompts[3].contains(STUCK_TEST_RULE));
        assert!(!prompts[3].contains("Never delete, skip or merge a test."));
        // Without the switch the same file is written whole (too many lines).
        assert!(!RepairPolicy::default().test_edits);
    }

    #[tokio::test]
    async fn rounds_stop_after_three_stale_rounds() {
        let (mut plan, mut report, client) = wave(&[("a", "a.py")], |prompt, _| {
            if prompt.contains("# Check results") {
                "```\nv2\n```".into()
            } else {
                "```\nv1\n```".into()
            }
        })
        .await;
        let wave_tokens = report.total_output_tokens;
        let judge = Judge::new(|_| fail("a.py", 1, "bad"));
        let summary = check_and_repair(
            &mut plan,
            &mut report,
            client.clone(),
            SchedulerConfig::default(),
            &judge,
            RepairPolicy::default(),
            &NO_REPAIR_HOOKS,
        )
        .await
        .unwrap();
        assert_eq!(summary.rounds.len(), 3);
        assert!(summary.rounds.iter().all(|round| !round.kept));
        assert!(summary.repaired.is_empty());
        // No round was better: the wave's file is kept.
        assert_eq!(output(&report, "a"), "v1");
        assert_eq!(judge.checks.load(Ordering::SeqCst), 4);
        assert_eq!(judge.finished.load(Ordering::SeqCst), 1);
        // The tokens of the rounds stay counted.
        assert_eq!(report.total_output_tokens, wave_tokens + 3 * 2);
    }

    #[tokio::test]
    async fn the_best_result_is_restored() {
        let (mut plan, mut report, client) = wave(&[("a", "a.py")], |prompt, number| {
            if prompt.contains("# Check results") {
                // v2, then v3, v4, v5.
                format!("```\nv{}\n```", number + 1)
            } else {
                "```\nv1\n```".into()
            }
        })
        .await;
        let judge = Judge::new(|files| match text(files, "a.py") {
            "v1" => fail("a.py", 2, "bad"),
            "v2" => fail("a.py", 1, "bad"),
            _ => fail("a.py", 3, "bad"),
        });
        let summary = check_and_repair(
            &mut plan,
            &mut report,
            client,
            SchedulerConfig::default(),
            &judge,
            RepairPolicy::default(),
            &NO_REPAIR_HOOKS,
        )
        .await
        .unwrap();
        let kept: Vec<bool> = summary.rounds.iter().map(|round| round.kept).collect();
        assert_eq!(kept, [true, false, false, false]);
        assert_eq!(summary.after.failed, 1);
        assert_eq!(summary.repaired, ["a.py"]);
        assert_eq!(output(&report, "a"), "v2");
    }

    /// Answers each full check early with a wrong failure; `confirm` gives
    /// the exact result (`v1` fails once, anything else passes).
    struct Early;

    #[async_trait::async_trait]
    impl Checker for Early {
        async fn check(
            &self,
            _: &BTreeMap<String, String>,
            _: CheckMode,
            _: &str,
        ) -> Result<CheckResult> {
            Ok(fail("a.py", 5, "early"))
        }

        async fn confirm(
            &self,
            files: &BTreeMap<String, String>,
            _: CheckResult,
            _: &str,
        ) -> Result<CheckResult> {
            Ok(match text(files, "a.py") {
                "v1" => fail("a.py", 1, "bad"),
                _ => pass(),
            })
        }
    }

    #[tokio::test]
    async fn the_result_that_ends_the_loop_and_the_reported_ones_are_confirmed() {
        let (mut plan, mut report, client) = wave(&[("a", "a.py")], |prompt, _| {
            if prompt.contains("# Check results") {
                "```\nv2\n```".into()
            } else {
                "```\nv1\n```".into()
            }
        })
        .await;
        let summary = check_and_repair(
            &mut plan,
            &mut report,
            client,
            SchedulerConfig::default(),
            &Early,
            RepairPolicy::default(),
            &NO_REPAIR_HOOKS,
        )
        .await
        .unwrap();
        // Two early failures; the third would end the loop as stale, so it
        // is confirmed: the exact tools pass it.
        let kept: Vec<bool> = summary.rounds.iter().map(|round| round.kept).collect();
        assert_eq!(kept, [false, false, true]);
        assert!(summary.rounds.iter().all(|round| round.after.passed));
        assert_eq!(summary.before.failed, 1);
        assert!(summary.after.passed);
        assert_eq!(output(&report, "a"), "v2");
    }

    #[tokio::test]
    async fn edits_that_do_not_apply_fall_back_to_the_full_file() {
        let (mut plan, mut report, client) = wave(&[("a", "a.ts")], |prompt, _| {
            if prompt.contains("<<<<<<< SEARCH") {
                "<<<<<<< SEARCH\nnot there\n=======\nx\n>>>>>>> REPLACE".into()
            } else if prompt.contains("# Check results") {
                "```\nfixed\n```".into()
            } else {
                "```\nline 1\nline 2\n```".into()
            }
        })
        .await;
        let judge = Judge::new(|files| match text(files, "a.ts") {
            "fixed" => pass(),
            _ => fail("a.ts", 1, "bad"),
        });
        let policy = RepairPolicy {
            edit_min_lines: 1,
            ..RepairPolicy::default()
        };
        let summary = check_and_repair(
            &mut plan,
            &mut report,
            client.clone(),
            SchedulerConfig::default(),
            &judge,
            policy,
            &NO_REPAIR_HOOKS,
        )
        .await
        .unwrap();
        let prompts = client.prompts.lock().unwrap().clone();
        assert_eq!(prompts.len(), 3, "the wave and two rounds");
        assert!(prompts[1].contains(EDIT_RULE));
        assert!(!prompts[2].contains("<<<<<<< SEARCH"));
        assert!(prompts[2].contains("Output the COMPLETE corrected file"));
        assert!(!summary.rounds[0].kept);
        assert!(summary.after.passed);
        assert_eq!(output(&report, "a"), "fixed");
    }

    #[tokio::test]
    async fn a_joint_answer_writes_only_the_editable_files() {
        let (mut plan, mut report, client) = wave(
            &[("a", "a.ts"), ("b", "b.ts"), ("c", "c.ts")],
            |prompt, _| {
                if prompt.contains("# Joint repair") {
                    "### `a.ts`\n```\nA2\n```\n### `b.ts`\n```\nB2\n```\n### `c.ts`\n```\nC2\n```\n"
                        .into()
                } else if prompt.contains("Write a.ts.") {
                    "```\na1\n```".into()
                } else if prompt.contains("Write b.ts.") {
                    "```\nb1\n```".into()
                } else {
                    "```\nc1\n```".into()
                }
            },
        )
        .await;
        let judge = Judge::new(|files| {
            if text(files, "a.ts") == "A2" && text(files, "b.ts") == "B2" {
                pass()
            } else {
                fail("a.ts", 1, "RENDER")
            }
        });
        let hooks = RepairHooks {
            classify: |line| {
                if line.contains("RENDER") {
                    Problem::Render
                } else {
                    Problem::Other
                }
            },
            joint_editable: |path, _, _| vec![path.to_string(), "b.ts".to_string()],
            ..NO_REPAIR_HOOKS
        };
        let summary = check_and_repair(
            &mut plan,
            &mut report,
            client.clone(),
            SchedulerConfig::default(),
            &judge,
            RepairPolicy::default(),
            &hooks,
        )
        .await
        .unwrap();
        assert!(summary.after.passed);
        assert_eq!(summary.rounds.len(), 1);
        assert_eq!(output(&report, "a"), "A2");
        assert_eq!(output(&report, "b"), "B2");
        assert_eq!(output(&report, "c"), "c1");
        let prompts = client.prompts.lock().unwrap().clone();
        let joint = prompts
            .iter()
            .find(|prompt| prompt.contains("# Joint repair"))
            .unwrap();
        assert!(
            joint.contains("You may change these files:\n\n`a.ts`:\n```\na1\n```\n\n`b.ts`:"),
            "{joint}"
        );
    }

    /// A lint that names another file as the place to change (race
    /// 20261116-154325).
    const SEARCH_LINK: &str = "ERROR app/search/page.tsx(92,1): error TS9028: search-link: a \
        comment search hit has the comment's id (`id: comment.id`, `lib/search.ts`), but this \
        file links hits to `/tasks/${...id}`: a comment result opens a page for a record that \
        does not exist. Search results link to the record that owns them: give the comment hit \
        its task id in `lib/search.ts` (line 75: `taskId: comment.taskId`) and link it to \
        `/tasks/${hit.taskId}`.\n";

    #[test]
    fn problems_that_name_another_file_name_it() {
        let files: BTreeMap<&str, &str> = [
            ("app/search/page.tsx", ""),
            ("lib/search.ts", ""),
            ("lib/types.ts", ""),
            ("tests/search.test.ts", ""),
        ]
        .into_iter()
        .collect();
        let is_test: fn(&str) -> bool = |path| path.starts_with("tests/");
        assert_eq!(
            named_files("app/search/page.tsx", SEARCH_LINK, &files, is_test),
            ["lib/search.ts"]
        );
        // No file named, or only the file itself, a missing file, a module
        // of node_modules or a test: no other file.
        let none = "ERROR app/search/page.tsx(1,1): x in `app/search/page.tsx`, `lib/nothing.ts`, \
                    `node_modules/x/index.ts`, `tests/search.test.ts`\n";
        assert!(named_files("app/search/page.tsx", none, &files, is_test).is_empty());
        // A test failure can name a test; at most two files.
        let many = "`lib/search.ts` `lib/types.ts` `app/search/page.tsx` `tests/search.test.ts`";
        assert_eq!(
            named_files("tests/other.test.ts", many, &files, is_test),
            ["lib/search.ts", "lib/types.ts"]
        );
        // A file named as context only is not the place to change.
        let context = "import `LABELS` from `lib/types.ts`; calls `find` from `./lib/search.ts`, \
                       the setting `x` (declared in `lib/types.ts`)";
        assert!(named_files("app/search/page.tsx", context, &files, is_test).is_empty());
        let joint = named_joint_prompt(
            "x\nA repair of `app/search/page.tsx` alone did not remove these problems:\ny",
            "app/search/page.tsx",
        );
        assert_eq!(
            joint,
            "x\nThese problems of `app/search/page.tsx` name another file as the place to change:\ny"
        );
    }

    #[tokio::test]
    async fn a_named_file_joins_the_first_repair() {
        let (mut plan, mut report, client) = wave(
            &[("p", "app/search/page.tsx"), ("s", "lib/search.ts")],
            |prompt, _| {
                if prompt.contains("# Joint repair") {
                    "### `app/search/page.tsx`\n```\nP2\n```\n### `lib/search.ts`\n```\nS2\n```\n"
                        .into()
                } else if prompt.contains("# Check results") {
                    "```\np2\n```".into()
                } else if prompt.contains("Write app/search/page.tsx.") {
                    "```\np1\n```".into()
                } else {
                    "```\ns1\n```".into()
                }
            },
        )
        .await;
        let judge = Judge::new(|files| {
            if text(files, "app/search/page.tsx") == "P2" && text(files, "lib/search.ts") == "S2" {
                pass()
            } else {
                CheckResult {
                    failed: 1,
                    summary: "1 failed, 0 passed".into(),
                    problems: [("app/search/page.tsx".to_string(), SEARCH_LINK.to_string())]
                        .into_iter()
                        .collect(),
                    ..CheckResult::default()
                }
            }
        });
        let summary = check_and_repair(
            &mut plan,
            &mut report,
            client.clone(),
            SchedulerConfig::default(),
            &judge,
            RepairPolicy::default(),
            &NO_REPAIR_HOOKS,
        )
        .await
        .unwrap();
        assert!(summary.after.passed);
        assert_eq!(summary.rounds.len(), 1);
        assert_eq!(output(&report, "s"), "S2");
        let prompts = client.prompts.lock().unwrap().clone();
        let joint = prompts
            .iter()
            .find(|prompt| prompt.contains("# Joint repair"))
            .unwrap();
        assert!(
            joint.contains("These problems of `app/search/page.tsx` name another file"),
            "{joint}"
        );
    }

    /// A named file joins one joint repair of a round; a scaffold file
    /// (static output) never joins; a test that is not stuck names nothing.
    #[tokio::test]
    async fn a_named_file_joins_one_repair_and_never_a_scaffold_or_a_fresh_test() {
        let (mut plan, mut report, client) = wave(
            &[
                ("a", "app/a/page.tsx"),
                ("b", "app/b/page.tsx"),
                ("s", "lib/search.ts"),
                ("k", "package.json"),
                ("t", "tests/a.test.ts"),
            ],
            |_, _| "```\nx\n```".into(),
        )
        .await;
        plan.tasks
            .iter_mut()
            .find(|task| task.id == "k")
            .unwrap()
            .static_output = Some("{}".into());
        let judge =
            Judge::new(|_| {
                CheckResult {
            failed: 3,
            summary: "3 failed, 0 passed".into(),
            problems: [
                ("app/a/page.tsx", "ERROR app/a/page.tsx(1,1): fix it in `lib/search.ts`\n"),
                (
                    "app/b/page.tsx",
                    "ERROR app/b/page.tsx(1,1): fix it in `lib/search.ts` and in `package.json`\n",
                ),
                ("tests/a.test.ts", "FAIL tests/a.test.ts: fix it in `lib/search.ts`\n"),
            ]
            .into_iter()
            .map(|(file, text)| (file.to_string(), text.to_string()))
            .collect(),
            ..CheckResult::default()
        }
            });
        let hooks = RepairHooks {
            is_test: |path| path.starts_with("tests/"),
            ..NO_REPAIR_HOOKS
        };
        let policy = RepairPolicy {
            max_rounds: 1,
            ..RepairPolicy::default()
        };
        check_and_repair(
            &mut plan,
            &mut report,
            client.clone(),
            SchedulerConfig::default(),
            &judge,
            policy,
            &hooks,
        )
        .await
        .unwrap();
        let prompts = client.prompts.lock().unwrap().clone();
        let joints: Vec<&String> = prompts
            .iter()
            .filter(|prompt| prompt.contains("# Joint repair"))
            .collect();
        assert_eq!(joints.len(), 1, "{joints:?}");
        assert!(
            joints[0].contains("`app/a/page.tsx`:\n```"),
            "{}",
            joints[0]
        );
        assert!(joints[0].contains("`lib/search.ts`:\n```"), "{}", joints[0]);
        assert!(!joints[0].contains("`package.json`"), "{}", joints[0]);
        // The second page and the fresh test get their own repair.
        for path in ["app/b/page.tsx", "tests/a.test.ts"] {
            let own = format!("# Check results\nThe assembled project was type-checked and its tests were run. Problems in `{path}`");
            assert!(prompts.iter().any(|prompt| prompt.contains(&own)), "{path}");
        }
    }

    #[tokio::test]
    async fn checker_patches_need_no_model_call() {
        let (mut plan, mut report, client) =
            wave(&[("a", "a.ts")], |_, _| "```\nold\n```".into()).await;
        let judge = Judge::new(|files| match text(files, "a.ts") {
            "new" => pass(),
            _ => CheckResult {
                patches: [("a.ts".to_string(), "new\n".to_string())]
                    .into_iter()
                    .collect(),
                ..fail("a.ts", 1, "bad")
            },
        });
        let summary = check_and_repair(
            &mut plan,
            &mut report,
            client.clone(),
            SchedulerConfig::default(),
            &judge,
            RepairPolicy::default(),
            &NO_REPAIR_HOOKS,
        )
        .await
        .unwrap();
        assert!(summary.before.passed && summary.rounds.is_empty());
        assert_eq!(client.prompts.lock().unwrap().len(), 1, "the wave only");
        assert_eq!(output(&report, "a"), "new");
    }

    #[tokio::test]
    async fn a_stuck_test_is_repaired_alone_then_jointly_with_more_reasoning() {
        let (mut plan, mut report, client) =
            wave(&[("t", "a.test.ts"), ("m", "m.ts")], |prompt, number| {
                if prompt.contains("# Joint repair") {
                    format!("### `a.test.ts`\n```\nt{number}\n```\n")
                } else if prompt.contains("# Check results") {
                    format!("```\nt{number}\n```")
                } else {
                    "```\nv1\n```".into()
                }
            })
            .await;
        let judge = Judge::new(|_| fail("a.test.ts", 1, "expected 1"));
        let hooks = RepairHooks {
            is_test: |path| path.contains(".test."),
            joint_editable: |path, _, _| vec![path.to_string(), "m.ts".to_string()],
            ..NO_REPAIR_HOOKS
        };
        let summary = check_and_repair(
            &mut plan,
            &mut report,
            client.clone(),
            SchedulerConfig::default(),
            &judge,
            RepairPolicy::default(),
            &hooks,
        )
        .await
        .unwrap();
        // Own repair, stuck test alone, joint: three stale rounds.
        assert_eq!(summary.rounds.len(), 3);
        let prompts = client.prompts.lock().unwrap().clone();
        let settings = client.settings.lock().unwrap().clone();
        assert_eq!(prompts.len(), 5, "two wave files and three rounds");
        let (own, alone, joint) = (&prompts[2], &prompts[3], &prompts[4]);
        assert!(own.contains("Correct the tests") && !own.contains(STUCK_TEST_RULE));
        assert!(alone.contains(STUCK_TEST_RULE) && !alone.contains("# Joint repair"));
        assert_eq!(settings[3].0, 0.8);
        assert!(joint.contains("# Joint repair") && joint.contains(TRACE_RULE));
        assert_eq!(settings[4].1.as_deref(), Some("medium"));
        // No round was better: the wave's test is kept.
        assert_eq!(output(&report, "t"), "v1");
    }

    /// A file whose only problem is the import of a missing module: the
    /// round writes the module (no repair of the importer). The plan keeps
    /// the new file only if the round is kept.
    async fn missing_module_round(
        verdict: fn(&BTreeMap<String, String>) -> CheckResult,
    ) -> (Plan, RunReport, RepairSummary, Vec<String>) {
        let (mut plan, mut report, client) = wave(&[("a", "a.ts")], |prompt, _| {
            if prompt.contains("# Change of assignment") {
                "```\nexport m\n```".into()
            } else {
                "```\nimport m\nuse m\n```".into()
            }
        })
        .await;
        let judge = Judge::new(verdict);
        let hooks = RepairHooks {
            classify: |line| {
                if line.contains("MISSING") {
                    Problem::MissingModule
                } else {
                    Problem::Other
                }
            },
            local_specifiers: |text| {
                text.lines()
                    .filter_map(|line| line.strip_prefix("import "))
                    .map(str::to_string)
                    .collect()
            },
            resolve_import: |_, spec, known| {
                let path = format!("{spec}.ts");
                known.contains(path.as_str()).then_some(path)
            },
            missing_module_path: |_, spec| Some(format!("{spec}.ts")),
            ..NO_REPAIR_HOOKS
        };
        let policy = RepairPolicy {
            max_rounds: 1,
            ..RepairPolicy::default()
        };
        let summary = check_and_repair(
            &mut plan,
            &mut report,
            client.clone(),
            SchedulerConfig::default(),
            &judge,
            policy,
            &hooks,
        )
        .await
        .unwrap();
        let prompts = client.prompts.lock().unwrap().clone();
        (plan, report, summary, prompts)
    }

    #[tokio::test]
    async fn a_missing_module_is_written_and_kept_with_its_round() {
        let (plan, report, summary, prompts) = missing_module_round(|files| {
            if files.contains_key("m.ts") {
                pass()
            } else {
                fail("a.ts", 1, "MISSING m")
            }
        })
        .await;
        assert_eq!(prompts.len(), 2, "the wave and the new file");
        assert!(prompts[1].contains("Write the new file `m.ts`"));
        assert!(summary.after.passed && summary.rounds[0].kept);
        assert_eq!(summary.rounds[0].files, ["m.ts"]);
        assert_eq!(plan.tasks.len(), 2);
        assert_eq!(output(&report, "added-m-ts"), "export m");
    }

    #[tokio::test]
    async fn a_new_file_of_a_round_that_is_not_kept_leaves_the_plan() {
        let (plan, report, summary, _) = missing_module_round(|files| {
            if files.contains_key("m.ts") {
                fail("m.ts", 5, "worse")
            } else {
                fail("a.ts", 1, "MISSING m")
            }
        })
        .await;
        assert!(!summary.rounds[0].kept);
        assert_eq!(plan.tasks.len(), 1);
        assert!(!report.tasks.contains_key("added-m-ts"));
    }

    #[tokio::test]
    async fn an_error_of_finish_keeps_the_summary() {
        let (mut plan, mut report, client) = wave(&[("a", "a.py")], |prompt, _| {
            if prompt.contains("# Check results") {
                "```\nv2\n```".into()
            } else {
                "```\nv1\n```".into()
            }
        })
        .await;
        let judge = Judge {
            finish_fails: true,
            ..Judge::new(|files| match text(files, "a.py") {
                "v2" => pass(),
                _ => fail("a.py", 1, "bad"),
            })
        };
        let summary = check_and_repair(
            &mut plan,
            &mut report,
            client,
            SchedulerConfig::default(),
            &judge,
            RepairPolicy::default(),
            &NO_REPAIR_HOOKS,
        )
        .await
        .unwrap();
        assert_eq!(judge.finished.load(Ordering::SeqCst), 1);
        assert!(summary.after.passed);
        assert_eq!(output(&report, "a"), "v2");
    }

    #[test]
    fn python_hooks_know_the_test_files() {
        let hooks = crate::profile::PYTHON_REPAIR_HOOKS;
        assert!((hooks.is_test)("tests/test_core.py"));
        assert!((hooks.is_test)("test_core.py"));
        assert!(!(hooks.is_test)("app/core.py"));
        assert!(!(NO_REPAIR_HOOKS.is_test)("tests/test_core.py"));
    }

    fn failing(problems: &[(&str, &str)]) -> CheckResult {
        CheckResult {
            failed: problems.len() as u64,
            summary: format!("{} failed, 0 passed", problems.len()),
            problems: problems
                .iter()
                .map(|(file, line)| (file.to_string(), format!("ERROR {file}(1,1): {line}\n")))
                .collect(),
            ..CheckResult::default()
        }
    }

    #[tokio::test]
    async fn the_root_first_guard_exports_a_declared_name_with_no_model_call() {
        let (mut plan, mut report, client) = wave(
            &[("t", "lib/types.ts"), ("a", "app/a.tsx"), ("b", "app/b.tsx")],
            |prompt, _| {
                if prompt.contains("Write lib/types.ts.") {
                    "```\ninterface AppState {\n  a: number;\n}\n```".into()
                } else {
                    "```\nimport { AppState } from '../lib/types';\nexport const x: AppState = { a: 1 };\n```"
                        .into()
                }
            },
        )
        .await;
        let judge = Judge::new(|files| {
            if text(files, "lib/types.ts").starts_with("export interface AppState") {
                pass()
            } else {
                let line =
                    "error TS2305: Module '\"../lib/types\"' has no exported member 'AppState'.";
                let mut result = failing(&[("app/a.tsx", line), ("app/b.tsx", line)]);
                // A knock-on line of another code: the file waits for the
                // check after the fix instead of going to the model.
                result.problems.get_mut("app/b.tsx").unwrap().push_str(
                    "ERROR app/b.tsx(2,1): error TS7053: Element implicitly has an 'any' type.
",
                );
                result
            }
        });
        let policy = RepairPolicy {
            guard: true,
            ..RepairPolicy::default()
        };
        let summary = check_and_repair(
            &mut plan,
            &mut report,
            client.clone(),
            SchedulerConfig::default(),
            &judge,
            policy,
            &crate::profile::NEXTJS_REPAIR,
        )
        .await
        .unwrap();
        assert!(summary.after.passed);
        assert_eq!(summary.rounds.len(), 1);
        // The three wave requests only: the fix asked the model nothing.
        assert_eq!(client.prompts.lock().unwrap().len(), 3);
        let guard = summary.guard.unwrap();
        assert_eq!(guard.index, "text");
        assert_eq!(guard.root_fixes, 1);
        assert_eq!(guard.root_waits, 1);
        assert_eq!(guard.root_only_rounds, 1);
        assert_eq!(summary.repaired, ["lib/types.ts"]);
    }

    #[tokio::test]
    async fn knock_on_files_that_import_each_other_are_still_repaired() {
        let (mut plan, mut report, client) =
            wave(&[("a", "app/a.tsx"), ("b", "app/b.tsx")], |prompt, _| {
                let other = if prompt.contains("Write app/a.tsx.") {
                    "./b"
                } else {
                    "./a"
                };
                let fixed = if prompt.contains("# Check results") {
                    "// fixed
"
                } else {
                    ""
                };
                format!(
                    "```
{fixed}import {{ x }} from '{other}';
export const x = 1;
```"
                )
            })
            .await;
        let judge = Judge::new(|files| {
            let mut left = Vec::new();
            for file in ["app/a.tsx", "app/b.tsx"] {
                if !text(files, file).contains("// fixed") {
                    left.push((
                        file,
                        "error TS7006: Parameter 't' implicitly has an 'any' type.",
                    ));
                }
            }
            if left.is_empty() {
                pass()
            } else {
                failing(&left)
            }
        });
        let policy = RepairPolicy {
            guard: true,
            ..RepairPolicy::default()
        };
        let summary = check_and_repair(
            &mut plan,
            &mut report,
            client.clone(),
            SchedulerConfig::default(),
            &judge,
            policy,
            &crate::profile::NEXTJS_REPAIR,
        )
        .await
        .unwrap();
        assert!(summary.after.passed);
        assert_eq!(summary.rounds.len(), 1);
        assert_eq!(summary.guard.unwrap().knock_on_skipped, 0);
    }

    #[tokio::test]
    async fn the_guards_lock_used_exports_and_skip_knock_on_files() {
        let (mut plan, mut report, client) = wave(
            &[
                ("m", "lib/board.ts"),
                ("p", "app/page.tsx"),
                ("l", "app/list.tsx"),
            ],
            |prompt, _| {
                if prompt.contains("# Check results") {
                    "```\nexport function moveTask(id: number): void {}\n```".into()
                } else if prompt.contains("Write lib/board.ts.") {
                    "```\nexport function moveTask(id: string): void {}\n```".into()
                } else {
                    "```\nimport { moveTask } from '../lib/board';\nmoveTask('a');\n```".into()
                }
            },
        )
        .await;
        let judge = Judge::new(|files| {
            if text(files, "lib/board.ts").contains("number") {
                pass()
            } else {
                failing(&[
                    ("lib/board.ts", "error TS2322: bad"),
                    (
                        "app/list.tsx",
                        "error TS7006: Parameter 't' implicitly has an 'any' type.",
                    ),
                ])
            }
        });
        let policy = RepairPolicy {
            guard: true,
            ..RepairPolicy::default()
        };
        let summary = check_and_repair(
            &mut plan,
            &mut report,
            client.clone(),
            SchedulerConfig::default(),
            &judge,
            policy,
            &crate::profile::NEXTJS_REPAIR,
        )
        .await
        .unwrap();
        assert!(summary.after.passed);
        let prompts = client.prompts.lock().unwrap();
        // Three wave requests and one repair: the knock-on file waited.
        assert_eq!(prompts.len(), 4);
        assert!(prompts[3].contains("# Used elsewhere"));
        assert!(prompts[3].contains("app/page.tsx:2 `moveTask('a');`"));
        let guard = summary.guard.unwrap();
        assert_eq!(guard.knock_on_skipped, 1);
        assert_eq!(guard.lock_blocks, 1);
        assert_eq!(guard.lock_breaks, 1);
    }
}

#[cfg(test)]
mod contract_side_tests {
    use super::*;

    #[test]
    fn a_verdict_line_before_the_edits_names_the_module() {
        let answer = "formatMoney - \"unknown code -> throws RangeError\" - module wrong\n\
                      parseMoney - nothing - test wrong\n\n<<<<<<< SEARCH\na\n=======\nb\n>>>>>>> REPLACE";
        assert!(says_module_wrong(answer));
        assert!(says_module_wrong(
            "`weightedAverage`: \"percent 0-100\": `module wrong`.\n"
        ));
        assert!(!says_module_wrong(
            "parseMoney - nothing - test wrong\n<<<<<<< SEARCH\n// module wrong\n=======\nb\n>>>>>>> REPLACE"
        ));
        assert!(!says_module_wrong("```ts\n// module wrong\n```"));
        // A reason after the verdict (live run 20261119-044834: missed by a
        // check of the line's end, and the test was bent in the next round).
        assert!(says_module_wrong(
            "formatMoney, `unknown code -> throws RangeError` \u{2014} module wrong (leave the test \
             case as is; the implementation accepts \"ZZZ\").\n\n<<<<<<< SEARCH\na\n=======\nb\n>>>>>>> REPLACE"
        ));
        // An answer that corrects itself: the later line of the function counts.
        assert!(!says_module_wrong(
            "`listStudents` \u{2014} \"else classIds includes classId\" \u{2014} module wrong (the test is \
             correct; the implementation is fine,\n\n`listStudents` \u{2014} nothing \u{2014} test wrong\n"
        ));
        assert!(!says_module_wrong(
            "nextDue \u{2014} nothing \u{2014} test wrong (not module wrong: the contract is silent)\n"
        ));
        // Both in one line: the last one outside the brackets (live run
        // 20261119-032523).
        assert!(!says_module_wrong(
            "`updateGoal` \u{2014} \"id not found -> returns state unchanged\" \u{2014} module wrong \
             (implementation returns `state` itself, which is correct) \u{2014} test wrong.\n"
        ));
        assert!(CONTRACT_TEST_RULE.contains("`module wrong` or `test wrong`"));
    }
}
