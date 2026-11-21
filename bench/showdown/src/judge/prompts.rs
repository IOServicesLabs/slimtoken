//! The judge's prompts. The system prompt is the same for every call, and
//! the app bundle goes in `shared_context`, so the provider's prefix cache
//! serves it to every later call on the same app.

/// The rubric, shared by every call.
pub const SYSTEM: &str = "You are a strict reviewer of a generated web app. You get the complete \
source of the app as numbered files: a header `=== path (N lines) ===`, then each line as \
`    N| code`. Judge only what the code does, as a user of the running app would meet it; never \
guess and never credit what a comment, a README or a file name promises. Cite evidence as the file \
path, the line number and an exact quote copied from that line (at most 120 characters, no line \
number in the quote). A quote that does not stand at the cited line is discarded, and so is the \
claim it supports. Answer with one JSON object and nothing else.";

/// The coverage lens: one status per requirement.
pub fn coverage(objective: &str, requirements: &str) -> String {
    format!(
        "COVERAGE. The app was built from this objective:\n\n{objective}\n\nFor each requirement \
         below decide its status:\n\
         - full: a user can reach it from the UI (a page that the navigation or another page links \
         to) and it works as stated, on the data the app stores;\n\
         - partial: it exists but is incomplete, wrong in part, only reachable in part, or a lib \
         function that no page or component calls;\n\
         - stub: a placeholder, fixed text, or a control that does nothing;\n\
         - missing: no code does it.\n\
         For full and partial give one to three evidence items (file, line, quote). For missing and \
         stub list in \"searched\" the identifiers and UI words you looked for.\n\n\
         Requirements (id: text):\n{requirements}\n\n\
         Reply with {{\"requirements\":[{{\"id\":\"G1\",\"status\":\"full\",\"evidence\":[{{\"file\":\
         \"app/page.tsx\",\"line\":12,\"quote\":\"...\"}}],\"searched\":[],\"note\":\"a few words\"}}]}} \
         with exactly one entry for each requirement id above."
    )
}

/// The working lens: the defects a user meets.
pub fn working(objective: &str) -> String {
    format!(
        "WORKING. The app was built from this objective:\n\n{objective}\n\nTrace the app's flows \
         through the code and report the defects that a user of the running app meets: wrong \
         results, broken flows, data that is lost or not shown, settings that change nothing, \
         links to pages that do not exist, state that differs between pages. Check these classes \
         in particular:\n\
         1 settings: a setting is saved but no page uses it\n\
         2 contract: two files disagree on a value (the case of a status, ids, class names, the \
         theme selector in the CSS against the code that sets the theme)\n\
         3 owner: a required feature has no page or control, or a link leads nowhere\n\
         4 state: a save drops other data, pages keep separate copies, ids collide after a reload, \
         storage read on the server or during the first render (hydration)\n\
         5 seed: fixed dates that go stale, seed data that does not fill the pages and charts\n\
         6 readme: the README is a build spec, is wrong, or has empty sections\n\
         7 duplicate: the same control rendered twice on a page, or a control that does nothing\n\
         8 cross-file: a value that one file produces and another file consumes in a different \
         convention. Follow each of these through every file that touches it:\n\
         - money: the sign and the unit (cents or units) agree between the validators in lib, the \
         forms that build the value, the seed data and the formatting (an expense the form makes \
         negative that a validator rejects, a seed in units read as cents);\n\
         - dates: a date-only value (YYYY-MM-DD) against a full timestamp (ISO with time) through \
         the parse helpers into the metrics and charts (a parser that accepts one format and is \
         fed the other makes every value computed from it empty or zero);\n\
         - status transitions: every path that changes a status (the board move, the detail page, \
         a bulk action) sets and clears completedAt / startedAt the same way;\n\
         - state owner: one owner of the stored state (no layout, page or component that keeps \
         its own storage copy and saves it back over the others);\n\
         - shell: the navigation and the page frame render once (not again inside a page);\n\
         - display: names are shown, not ids (an author or category id shown raw);\n\
         - currency: the currency setting reaches every place that formats money.\n\
         Severity: broken (a required flow does not work), wrong (it works but gives a wrong \
         result), cosmetic (it looks wrong but works). Report only defects you can cite, each \
         once, with the requirement id when one fits (else \"\"). For a class 8 defect quote \
         BOTH sides of the mismatch: file/line/quote for one side and \"other\":{{\"file\",\
         \"line\",\"quote\"}} for the other side; a class 8 defect without a verified second \
         quote is discarded.\n\n\
         Reply with {{\"bugs\":[{{\"severity\":\"broken\",\"class\":3,\"req\":\"T6\",\"file\":\
         \"src/screens/Item.tsx\",\"line\":108,\"quote\":\"tags={{[]}}\",\"effect\":\"tags cannot \
         be edited\"}},{{\"severity\":\"wrong\",\"class\":8,\"req\":\"S2\",\"file\":\"src/shipping/quote.ts\",\
         \"line\":40,\"quote\":\"formatWeight(parcel.grams)\",\"other\":{{\"file\":\"src/util/units.ts\",\
         \"line\":7,\"quote\":\"export function formatWeight(kilos: number)\"}},\"effect\":\"the parcel \
         stores grams, the formatter takes kilograms, so every weight shows 1000 times too large\"}}]}}, or \
         {{\"bugs\":[]}} when you find none."
    )
}

/// The second look at requirements on which the samples disagree.
pub fn tiebreak(objective: &str, requirements: &str, claims: &str) -> String {
    format!(
        "TIEBREAK. The app was built from this objective:\n\n{objective}\n\nTwo reviews of this app \
         disagree on the requirements below. Read the code again and decide each one with these \
         rules: full = a user can reach it from the UI and it works (cite it); partial = it exists \
         but is incomplete or not reachable; stub = a placeholder or a control that does nothing; \
         missing = no code does it (list the words you searched for).\n\n\
         Requirements (id: text):\n{requirements}\n\nThe earlier claims:\n{claims}\n\n\
         Reply with {{\"requirements\":[{{\"id\":\"...\",\"status\":\"...\",\"evidence\":[{{\"file\":\
         \"...\",\"line\":1,\"quote\":\"...\"}}],\"searched\":[],\"note\":\"...\"}}]}} for these ids \
         only."
    )
}

/// The context of a pairwise call: both bundles under neutral labels.
pub fn pair_context(first: &str, second: &str) -> String {
    format!("##### APP A #####\n\n{first}\n##### APP B #####\n\n{second}")
}

/// The pairwise question.
pub fn pairwise(objective: &str) -> String {
    format!(
        "PAIRWISE. You got two apps, APP A and APP B, built from the same objective:\n\n{objective}\n\n\
         Which app is better for a user: more of the objective works from the UI, with fewer \
         defects that a user meets? Judge requirement coverage and whether the app works, not code \
         style, length or tests alone. Reply with {{\"better\":\"A\",\"margin\":\"large\",\"why\":\
         \"two or three sentences that cite files\"}}, where better is \"A\", \"B\" or \"tie\" and \
         margin is \"large\", \"clear\" or \"slight\"."
    )
}
