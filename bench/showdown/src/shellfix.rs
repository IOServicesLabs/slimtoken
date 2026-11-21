//! `shellfix` (default off): the TS9021 (`duplicate-shell`) fix that needs
//! no model call, run in the repair hooks' `prepare`, so before every gate
//! run and every check of the landed files during the wave (`inwave`).
//!
//! In the 40 runs before it, TS9021 was the after-wave check's most common
//! problem (114 pages, often 8-9 pages a run): the root layout renders
//! `<AppShell>`, whose `<main className="shell">` holds every page, and the
//! page wraps its own content in `<div className="shell">` again. The fix
//! (`qlint::fix_duplicate_shell`) returns the children of that wrapper
//! instead, and is kept only when the lint passes on the fixed page; a page
//! whose change is not clear is left for the repair.

use slimtoken::dag::Plan;
use slimtoken::scheduler::{RunReport, TaskStatus};
use std::collections::BTreeSet;
use std::sync::Mutex;

/// The pages fixed and the pages left (named by the lint, change not
/// clear) since the last [`take`].
static SEEN: Mutex<(BTreeSet<String>, BTreeSet<String>)> =
    Mutex::new((BTreeSet::new(), BTreeSet::new()));

/// True with `shellfix` on (off unless `SHOWDOWN_ON` names it).
pub fn on() -> bool {
    crate::arms::step_on("shellfix")
}

/// The pages fixed and the pages left since the last call (and reset
/// both). Each page counts once, however many checks fixed it.
pub fn take() -> (BTreeSet<String>, BTreeSet<String>) {
    match SEEN.lock() {
        Ok(mut seen) => std::mem::take(&mut *seen),
        Err(_) => Default::default(),
    }
}

/// The `prepare` step of `shellfix`: fix the pages of `report` that draw the
/// root layout's frame again. Returns the number of pages fixed.
pub fn fix_pages(plan: &Plan, report: &mut RunReport) -> usize {
    if !on() {
        return 0;
    }
    let mut files: Vec<(String, String)> = Vec::new();
    let mut ids: Vec<String> = Vec::new();
    for task in &plan.tasks {
        let Some(record) = report.tasks.get(&task.id) else {
            continue;
        };
        if record.status != TaskStatus::Succeeded {
            continue;
        }
        if let Some(output) = &record.output {
            files.push((
                task.title.clone(),
                crate::arms::file_text(&task.title, output),
            ));
            ids.push(task.id.clone());
        }
    }
    let (fixed, left) = crate::qlint::fix_duplicate_shell(&files);
    let mut count = 0;
    for (path, text) in &fixed {
        let Some(index) = files.iter().position(|(title, _)| title == path) else {
            continue;
        };
        if let Some(record) = report.tasks.get_mut(&ids[index]) {
            record.output = Some(slimtoken::project::fenced(text));
            count += 1;
        }
    }
    if let Ok(mut seen) = SEEN.lock() {
        seen.0.extend(fixed.into_iter().map(|(path, _)| path));
        let done = seen.0.clone();
        seen.1
            .extend(left.into_iter().filter(|path| !done.contains(path)));
    }
    count
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_fix_is_off_in_the_library_and_on_in_the_showdown() {
        assert!(crate::arms::DEFAULT_OFF.contains(&"shellfix"));
        assert!(crate::arms::RECOMMENDED_ON.contains(&"shellfix"));
    }
}
