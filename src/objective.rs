//! Reading the parts of an objective that the plan hands to single files:
//! the app's name, its summary, its page list ("Pages:" bullets) and its
//! quality bar, and which page bullet belongs to which route file. Plain
//! text rules, no model call; used by the README and by
//! `PlanOptions::objective_pages`.

/// One bullet of the objective's page list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PageBullet {
    /// The name before the first `:` ("Board", "Task detail").
    pub name: String,
    /// The bullet without its marker, verbatim ("Board: a kanban board …").
    pub text: String,
}

/// The paragraphs of `objective` (blank-line separated, trimmed).
fn paragraphs(objective: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current: Vec<&str> = Vec::new();
    for line in objective.lines() {
        if line.trim().is_empty() {
            if !current.is_empty() {
                out.push(current.join("\n"));
                current.clear();
            }
        } else {
            current.push(line.trim_end());
        }
    }
    if !current.is_empty() {
        out.push(current.join("\n"));
    }
    out
}

/// A list item's text (`- x`, `* x`), or `None` for any other line.
fn bullet(line: &str) -> Option<&str> {
    let line = line.trim_start();
    line.strip_prefix("- ")
        .or_else(|| line.strip_prefix("* "))
        .map(str::trim)
}

/// The bullets of the objective's page list: the list items after a line
/// `Pages:` (any case), up to the first line that is not a list item. Each
/// has a name before a `:` of at most 40 bytes; an item without one is left
/// out. Empty when the objective has no page list.
pub fn page_bullets(objective: &str) -> Vec<PageBullet> {
    let mut lines = objective.lines();
    let found = lines
        .by_ref()
        .any(|line| line.trim().eq_ignore_ascii_case("pages:"));
    if !found {
        return Vec::new();
    }
    let mut bullets = Vec::new();
    for line in lines {
        if line.trim().is_empty() && bullets.is_empty() {
            continue;
        }
        let Some(text) = bullet(line) else {
            break;
        };
        let Some((name, _)) = text.split_once(':') else {
            continue;
        };
        let name = name.trim();
        if name.is_empty() || name.len() > 40 {
            continue;
        }
        bullets.push(PageBullet {
            name: name.to_string(),
            text: text.to_string(),
        });
    }
    bullets
}

/// The objective's quality-bar paragraph (the one that starts with
/// "Quality bar", any case), or `None`.
pub fn quality_bar(objective: &str) -> Option<String> {
    paragraphs(objective)
        .into_iter()
        .find(|paragraph| paragraph.to_lowercase().starts_with("quality bar"))
}

/// The quality bar as one file's worker reads it: one line, without its
/// leading "Quality bar:", and with "a file that you do not write" (meant
/// for one writer of the whole app) read as "a file that the plan does not
/// list", since each worker writes one file and imports the others.
pub fn quality_bar_for_one_file(objective: &str) -> Option<String> {
    let text = quality_bar(objective)?.replace('\n', " ");
    let mut text = text.trim().to_string();
    let lower = text.to_lowercase();
    if lower.starts_with("quality bar") {
        let rest = text["quality bar".len()..].trim_start();
        text = rest
            .strip_prefix(':')
            .unwrap_or(rest)
            .trim_start()
            .to_string();
    }
    for phrase in [
        "that you do not write",
        "that you don't write",
        "you do not write",
        "you don't write",
    ] {
        text = text.replace(phrase, "that the plan does not list");
    }
    let text = text.replace("that that the plan", "that the plan");
    (!text.is_empty()).then_some(text)
}

/// The app's name: the first double-quoted text of the objective's first
/// paragraph, when it is a short name (1 to 40 bytes, one line).
pub fn app_name(objective: &str) -> Option<String> {
    let first = paragraphs(objective).into_iter().next()?;
    let (_, rest) = first.split_once('"')?;
    let (name, _) = rest.split_once('"')?;
    let name = name.trim();
    (!name.is_empty() && name.len() <= 40 && !name.contains('\n')).then(|| name.to_string())
}

/// A summary of the app for its README: the objective's first paragraph
/// without its list items. `Build "Name", a …` reads `Name is a …`.
pub fn summary(objective: &str) -> String {
    let first = paragraphs(objective).into_iter().next().unwrap_or_default();
    let text = first
        .lines()
        .take_while(|line| bullet(line).is_none())
        .collect::<Vec<_>>()
        .join(" ");
    let text = text.trim();
    if let Some(name) = app_name(objective) {
        for verb in ["Build", "Create", "Make", "Write"] {
            let prefix = format!("{verb} \"{name}\", ");
            if let Some(rest) = text.strip_prefix(&prefix) {
                return format!("{name} is {rest}");
            }
        }
    }
    text.to_string()
}

/// The route of an App Router page file: `app/page.tsx` -> `/`,
/// `app/tasks/[id]/page.tsx` -> `/tasks/[id]` (route groups `(x)` are not
/// part of the route); `None` for a path that is not a page under `app/`.
pub fn route_of(path: &str) -> Option<String> {
    let rest = path.strip_prefix("src/").unwrap_or(path);
    let rest = rest.strip_prefix("app/")?;
    let name = rest.rsplit('/').next()?;
    if !name.starts_with("page.") {
        return None;
    }
    let dir = rest[..rest.len() - name.len()].trim_end_matches('/');
    let segments: Vec<&str> = dir
        .split('/')
        .filter(|segment| !segment.is_empty() && !segment.starts_with('('))
        .collect();
    Some(format!("/{}", segments.join("/")))
}

/// The link of a route: a dynamic segment `[id]` becomes `${id}`
/// (`/tasks/[id]` -> `/tasks/${id}`); `None` for a static route.
pub fn href_of(route: &str) -> Option<String> {
    if !route.contains('[') {
        return None;
    }
    let href = route
        .split('/')
        .map(|segment| {
            segment
                .strip_prefix('[')
                .and_then(|inner| inner.strip_suffix(']'))
                .map(|inner| format!("${{{}}}", inner.trim_start_matches("...")))
                .unwrap_or_else(|| segment.to_string())
        })
        .collect::<Vec<_>>()
        .join("/");
    Some(href)
}

/// Lower-case words of a name (`in-progress` -> `in progress`).
pub(crate) fn words(text: &str) -> Vec<String> {
    text.split(|ch: char| !ch.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .map(str::to_lowercase)
        .collect()
}

/// A plain English singular (`tasks` -> `task`, `categories` ->
/// `category`), enough for route names.
pub(crate) fn singular(word: &str) -> String {
    if let Some(stem) = word.strip_suffix("ies") {
        return format!("{stem}y");
    }
    if word.ends_with("ss") {
        return word.to_string();
    }
    word.strip_suffix('s').unwrap_or(word).to_string()
}

/// Whether a bullet name (its words) names a detail page ("Task detail").
pub(crate) fn is_detail(name: &[String]) -> bool {
    name.iter()
        .any(|word| word == "detail" || word == "details")
}

/// Whether a bullet is a detail bullet ("Task detail: …").
pub fn is_detail_bullet(bullet: &PageBullet) -> bool {
    is_detail(&words(&bullet.name))
}

/// Names of the bullet that a root page (`/`) owns.
const ROOT_NAMES: &[&str] = &["dashboard", "overview", "home"];

/// Which bullet of `bullets` each page of `paths` owns (`None`: no
/// bullet), by name, each bullet at most once:
/// 1. a static route whose last segment is the bullet's name, apart from
///    case, separators and a plural `s` (`/board` - "Board");
/// 2. a route whose last static segment is the first word of a "… detail"
///    bullet (`/tasks/[id]` or `/task` - "Task detail"), a dynamic route
///    before a static one (`/tasks/[id]`, not the list page `/tasks`);
/// 3. the root page (`/`) - "Dashboard", "Overview" or "Home".
///
/// A path that is not a page (`route_of`) owns nothing.
pub fn match_pages(paths: &[&str], bullets: &[PageBullet]) -> Vec<Option<usize>> {
    let mut owner: Vec<Option<usize>> = vec![None; paths.len()];
    let mut taken = vec![false; bullets.len()];
    let routes: Vec<Option<String>> = paths.iter().map(|path| route_of(path)).collect();
    // The last static segment of each route, and whether a dynamic
    // segment follows it.
    let keys: Vec<Option<(String, bool)>> = routes
        .iter()
        .map(|route| {
            let route = route.as_ref()?;
            let segments: Vec<&str> = route.split('/').filter(|s| !s.is_empty()).collect();
            let last_static = segments.iter().rposition(|s| !s.starts_with('['))?;
            let dynamic = last_static + 1 < segments.len();
            Some((words(segments[last_static]).join(" "), dynamic))
        })
        .collect();
    let names: Vec<Vec<String>> = bullets.iter().map(|bullet| words(&bullet.name)).collect();
    let mut claim = |page: usize,
                     test: &dyn Fn(&[String]) -> bool,
                     owner: &mut Vec<Option<usize>>| {
        if owner[page].is_some() {
            return;
        }
        if let Some(index) = (0..bullets.len()).find(|&index| !taken[index] && test(&names[index]))
        {
            taken[index] = true;
            owner[page] = Some(index);
        }
    };
    // 1. Names.
    for (page, key) in keys.iter().enumerate() {
        let Some((key, false)) = key else { continue };
        let key = singular(key);
        claim(
            page,
            &|name: &[String]| {
                let name: Vec<String> = name.iter().map(|word| singular(word)).collect();
                name.join(" ") == key
            },
            &mut owner,
        );
    }
    // 2. Detail pages: dynamic routes first (`/tasks/[id]` before a list
    // page `/tasks`), then a static one (`/task?id=`).
    for want_dynamic in [true, false] {
        for (page, key) in keys.iter().enumerate() {
            let Some((key, dynamic)) = key else { continue };
            if *dynamic != want_dynamic {
                continue;
            }
            let key = singular(key);
            claim(
                page,
                &|name: &[String]| {
                    is_detail(name) && name.first().is_some_and(|first| singular(first) == key)
                },
                &mut owner,
            );
        }
    }
    // 3. The root page.
    for (page, route) in routes.iter().enumerate() {
        if route.as_deref() != Some("/") {
            continue;
        }
        claim(
            page,
            &|name: &[String]| name.len() == 1 && ROOT_NAMES.contains(&name[0].as_str()),
            &mut owner,
        );
    }
    owner
}

#[cfg(test)]
mod tests {
    use super::*;

    const TASKFLOW: &str = "Build \"TaskFlow\", a complete project management web app with \
Next.js (App Router) and TypeScript. No backend.\n\nPages:\n- Dashboard: summary cards.\n- \
Projects: list of projects.\n- Board: a kanban board for one project with columns Backlog, In \
Progress, Review, Done.\n- Task detail: edit title.\n- Settings: theme (light, dark), default \
project.\n\nQuality bar: every page is complete and usable.\n";

    #[test]
    fn reads_the_name_summary_pages_and_quality_bar() {
        assert_eq!(app_name(TASKFLOW).as_deref(), Some("TaskFlow"));
        assert_eq!(
            quality_bar_for_one_file(
                "Build it.\n\nQuality bar: no TODO comments, and no import of a file that \
                 you do not write.\nIt must build."
            )
            .as_deref(),
            Some(
                "no TODO comments, and no import of a file that the plan does not list. It \
                 must build."
            )
        );
        assert!(summary(TASKFLOW).starts_with("TaskFlow is a complete project management"));
        assert!(
            summary(TASKFLOW).ends_with("No backend."),
            "{}",
            summary(TASKFLOW)
        );
        let bullets = page_bullets(TASKFLOW);
        let names: Vec<&str> = bullets.iter().map(|b| b.name.as_str()).collect();
        assert_eq!(
            names,
            ["Dashboard", "Projects", "Board", "Task detail", "Settings"]
        );
        assert!(bullets[2].text.starts_with("Board: a kanban board"));
        assert!(bullets[2].text.ends_with("Review, Done."));
        assert_eq!(
            quality_bar(TASKFLOW).as_deref(),
            Some("Quality bar: every page is complete and usable.")
        );
        assert_eq!(app_name("Build a tracker."), None);
        assert_eq!(
            summary("Build a tracker.\nWith tags."),
            "Build a tracker. With tags."
        );
        assert!(page_bullets("Build a tracker.").is_empty());
    }

    #[test]
    fn routes_and_hrefs() {
        assert_eq!(route_of("app/page.tsx").as_deref(), Some("/"));
        assert_eq!(
            route_of("app/tasks/[id]/page.tsx").as_deref(),
            Some("/tasks/[id]")
        );
        assert_eq!(
            route_of("src/app/(shop)/cart/page.tsx").as_deref(),
            Some("/cart")
        );
        assert_eq!(route_of("app/layout.tsx"), None);
        assert_eq!(route_of("components/Board.tsx"), None);
        assert_eq!(href_of("/tasks/[id]").as_deref(), Some("/tasks/${id}"));
        assert_eq!(href_of("/board"), None);
    }

    #[test]
    fn pages_match_their_bullets_by_name() {
        let bullets = page_bullets(TASKFLOW);
        let paths = [
            "app/layout.tsx",
            "app/page.tsx",
            "app/projects/page.tsx",
            "app/board/page.tsx",
            "app/tasks/[id]/page.tsx",
            "app/settings/page.tsx",
            "app/search/page.tsx",
            "components/Board.tsx",
        ];
        let owners = match_pages(&paths, &bullets);
        let names: Vec<Option<&str>> = owners
            .iter()
            .map(|owner| owner.map(|index| bullets[index].name.as_str()))
            .collect();
        assert_eq!(
            names,
            [
                None,
                Some("Dashboard"),
                Some("Projects"),
                Some("Board"),
                Some("Task detail"),
                Some("Settings"),
                None,
                None
            ]
        );
        // A query-string detail page (`/task?id=`) owns the detail bullet.
        let owners = match_pages(&["app/task/page.tsx"], &bullets);
        assert_eq!(owners, [Some(3)]);
        // A list page `/tasks` does not take the detail bullet from the
        // dynamic route that follows it.
        let owners = match_pages(
            &[
                "app/page.tsx",
                "app/tasks/page.tsx",
                "app/tasks/[id]/page.tsx",
            ],
            &bullets,
        );
        assert_eq!(owners, [Some(0), None, Some(3)]);
        assert!(is_detail_bullet(&bullets[3]) && !is_detail_bullet(&bullets[2]));
    }
}
