//! TypeScript / Next.js App Router + Vitest: same structure as Python
//! (contracts first, one parallel wave, sharded suites) with its own
//! conventions, rendered toolchain files and route-file fixups.

use super::{keep, Layer, PageSplit, Profile, Quality, Split, Staging};
use crate::architect::Manifest;

pub static NEXTJS: Profile = Profile {
    name: "nextjs",
    markers: &["next.js", "nextjs", "typescript", "react", ".tsx"],
    comment: "//",
    example_module: "lib/module.ts",
    example_export: "export function fn(arg: string): Result",
    example_test: "tests/all.test.ts",
    suite: "Vitest",
    planner_rules: "- Target strict TypeScript with Next.js 16 App Router (`app/`
  directory, React 19 function components) unless the objective says
  otherwise: `.ts` for logic, `.tsx` for components and pages. In Next 16,
  route `params`/`searchParams` props are Promises (await them). React 19
  has no global `JSX` namespace: never write `JSX.Element`; leave component
  return types inferred.
- Put ALL domain logic (game rules, scoring, scheduling, persistence) in pure
  `lib/*.ts` modules with no React imports; components only call lib exports.
  Tests exercise lib modules directly.
- Include `package.json` (dependencies next, react, react-dom; devDependencies
  typescript, @types/react, @types/react-dom, @types/node, vitest; scripts dev,
  build, start, test=`vitest run`), `tsconfig.json` and `next.config.mjs` as
  files with exports []. Imports between project files use relative paths.
- In TypeScript exports, a `// ...` comment after the signature is the
  annotation (instead of `# ...`).
- Route files (`app/**/page.tsx`, `layout.tsx`) export ONLY their default
  component (a layout may also export `metadata`); reusable components go in
  `components/*.tsx`.
- Styling contract: the global stylesheet's exports list EVERY CSS class the
  app uses, one per entry with its role (e.g. `.nav  // top bar, horizontal
  flex links`, `.card  // bordered panel`; up to 16 entries — the stylesheet
  is exempt from the 4-export limit); components use only those class names,
  character for character.",
    edge_rule: EDGE_RULE,
    fragment_note: " (a stylesheet instead lists up to 16 `.class  // role` entries, \
                 every class the app uses)",
    listed_module_exts: &[],
    module_head: "Program contracts — every file in this program. Call the other \
             modules only through these signatures:\n{all}\n\n\
             Rules:\n- Output exactly one fenced code block and nothing else — \
             no explanation, no headers.\n- Strict TypeScript; compact style, no \
             boilerplate comments.\n- Imports between project files use relative \
             paths; use only packages listed in package.json.\n- Invalid input \
             throws an Error (RangeError/TypeError) unless the contract says \
             otherwise.\n- Return EXACTLY the fields named in an export's \
             return-shape annotation — no extra or missing keys (tests assert \
             shapes exactly).\n- lib/ modules are pure: no React imports, no \
             window/document/localStorage access at import time (guard browser \
             APIs with `typeof window !== \"undefined\"`).\n- Components and \
             pages that use state, effects or event handlers start with \
             \"use client\".\n- Route files (app/**/page.tsx, layout.tsx) export \
             ONLY the default component (layouts may add `metadata`); helper \
             components stay local and unexported.\n- React 19: no global `JSX` \
             namespace — never write `JSX.Element`; leave component return types \
             inferred.\n- CSS classes: use exactly the class names the stylesheet's \
             contract lists; the stylesheet defines every one of them.\n- \
             Config/JSON files: emit the file content verbatim in the fenced \
             block.\n\n",
    test_head: "Module contracts — use ONLY these signatures; {visibility}:\n{all}\n\n\
             Rules:\n- Output exactly one fenced code block and nothing else.\n\
             - Vitest: `import { describe, it, expect } from \"vitest\"`; the \
             suite lives in tests/ at the project root, so import modules as \
             `../lib/<name>`.\n- Test the pure lib/ modules only: no DOM, no \
             React rendering; pass explicit timestamps/seeds where a contract \
             takes them.\n- Cover happy paths, invalid input (thrown errors) and \
             edge cases; never write placeholder or tautological tests.\n- Where \
             an export annotates its return shape, assert it exactly \
             (toEqual).\n- Keep tests compact.\n{silent}\n",
    // A/B (`--architect-density`): the built-in head already bans
    // boilerplate comments; this tier bans docstrings and non-contract
    // comments everywhere and re-export facades. Contract `//` annotations
    // (edge rules) are load-bearing and stay.
    density_rule: "- Density: no docstrings or JSDoc anywhere and no comments \
                 except the contract `//` annotations on exports; no \
                 comments inside test functions; do not re-export or wrap \
                 another module's function — import it from its defining \
                 module at the use site.\n\n",
    // the Digest manifest defined `Article` in both lib/types.ts and
    // lib/articles.ts and they drifted (`notes?: string` vs `notes: string`)
    // — 11-12 tsc errors in every replay arm. One owner, import elsewhere.
    type_ownership_rule: "- Every interface/type alias is defined in exactly \
                 ONE owner file (a lib/types.ts when the objective names \
                 shared shapes, else the domain module that owns it) and \
                 listed among that file's exports; every other file imports \
                 it from the owner by relative path. NEVER re-declare a \
                 structurally identical type in a second file.\n",
    validates: |_| false,
    is_testable,
    is_consumer: |path| path.starts_with("app/") || path.starts_with("components/"),
    shard_path: |index, module| {
        let name = module.rsplit('/').next().unwrap_or("part");
        let stem = name.trim_end_matches(".tsx").trim_end_matches(".ts");
        format!("tests/{index:02}_{stem}.test.ts")
    },
    scaffold,
    normalize_skeleton: keep,
    normalize_manifest: restrict_route_exports,
    tidy_manifest,
    // `~~~` fences: the README is one ```markdown block, which a ``` line
    // inside it would close.
    readme_sections: |_| {
        "\n## Setup\n\n~~~bash\nnpm install\nnpm run dev\n~~~\n\nThen open \
         http://localhost:3000.\n\n## Scripts\n\n- `npm run dev`: start the development \
         server\n- `npm run build`: build for production (type check included)\n- `npm start`: \
         serve the production build\n- `npm test`: run the Vitest suites\n"
            .to_string()
    },
    staging: STAGING,
    repair: super::NEXTJS_REPAIR,
};

/// Measured in `bench/showdown` (its `stage_plan`): shared types first, then
/// the `lib` modules, then pages, components and tests. The seed data needs
/// the types only, so it is written beside the `lib` stage.
const STAGING: Staging = Staging {
    layers: &[
        ("**/types.ts", Layer::Types),
        ("**/*.d.ts", Layer::Types),
        ("*/**/types/**/*.{ts,tsx}", Layer::Types),
        ("**/seed.{ts,tsx}", Layer::Data),
        ("**/seed.*.{ts,tsx}", Layer::Data),
        ("{lib,src/lib}/**/*.ts", Layer::Core),
        ("**/*.{ts,tsx}", Layer::Consumer),
    ],
    test_globs: &["**/*.test.*", "**/*.spec.*"],
    type_decl_prefixes: &["export interface ", "export type ", "export enum "],
    // Components may each declare their own `Props`: only `.ts` modules
    // share one owner for a type name.
    type_owner_globs: &["*.ts"],
    import_rule: IMPORT_RULE,
    staged_rule: STAGED_RULE,
    types_win_rule: TYPES_WIN_RULE,
    data_note: SEED_NOTE,
    // First match wins: a `.test.tsx` gets the test rule only.
    path_rules: &[("**/*.{test,spec}.*", TEST_RULE), ("**/*.tsx", NEXT_RULE)],
    // Measured only in the bench, together with its manifest passes and
    // wide start. Set after the engine path matches it in paired runs, so
    // a preset does not turn staging on before then.
    measured: false,
    split: &[CSS_SPLIT],
    aggregate: css_imports,
    quality: QUALITY,
};

/// The NEXTJS quality texts (`PlanOptions::quality_rules`,
/// `relative_dates`, `objective_pages`, `page_local`). From the blind audit of the bench's
/// apps: each cross-file mechanism that no prompt fixed was decided again
/// by each worker (the theme in three ways, state read during render,
/// counter ids that collide with seed ids, settings saved but never read).
const QUALITY: Quality = Quality {
    head_rule: QUALITY_HEAD_RULE,
    rewrites: &[(".theme-dark", ":root[data-theme=\"dark\"]")],
    date_note: RELATIVE_DATE_NOTE,
    page_glob: "app/**/page.tsx",
    component_glob: "components/**/*.tsx",
    component_rule: "Scope: the page owns the layout; you own only what your contract lists.",
    purposes_rule: "- If you import a component, do not render again what its purpose says it \
                    renders.\n\n",
    contract_rule: CONTRACT_RULE,
    contract_cap: "At most 4 exports per file; `lib/types.ts` up to 14 (the app contract), \
                   and a lib module that owns a collection up to 6 (its create or add, update and \
                   remove functions among them), each ONE line",
    contract_types_note: "\n\nThis is the app contract file. Every planned file of the app \
                          (path: purpose), for its catalogs and for the readers of each setting:\n",
    settings_type: "Settings",
    state_type: "AppState",
    wiring_rule: WIRING_RULE,
    page_local_rule: PAGE_LOCAL_RULE,
    local_note: LOCAL_NOTE,
    shared_component_rule: SHARED_COMPONENT_RULE,
    layout_glob: "app/**/layout.tsx",
    semantics_rule: SEMANTICS_RULE,
    semantic_page_local_rule: SEMANTIC_PAGE_LOCAL_RULE,
    semantics_types_note: "\n\nThis is the conventions file. Every planned file of the app \
                           (path: purpose), to name the function that owns each status change:\n",
    semantics_cap: "At most 4 exports per file; `lib/types.ts` up to 12 (its interfaces and \
                    unions, with the conventions in their comments), each ONE line",
    conventions_rule: CONVENTIONS_RULE,
    layout_component_rule: LAYOUT_COMPONENT_RULE,
    layout_component_bare_rule: LAYOUT_COMPONENT_BARE_RULE,
    page_split: PAGE_SPLIT,
    page_shell_rule: PAGE_SHELL_RULE,
    page_shell_no_class: "the page wrapper",
    shell_frame_rule: SHELL_FRAME_RULE,
};

/// Added to the shell component's prompt with `PlanOptions::shell_rule`:
/// the pages are told that the shell draws the page wrapper, so the shell's
/// writer is told to draw it.
const SHELL_FRAME_RULE: &str = "Frame: the root layout renders you around every page, and the \
pages draw no frame of their own: render the app's navigation and then the page wrapper \
`{main}` around the page.";

/// Added to each page's prompt with `PlanOptions::shell_rule`. In the 40
/// bench runs before it, 114 pages wrapped their content in
/// `<div className="shell">`: the stylesheet's contract lists `.shell  //
/// page wrapper, max width` and the shell's contract says only that it
/// renders the nav, so a page writer took the page wrapper for its own,
/// while the shell already renders `<main className="shell">{children}</main>`.
const PAGE_SHELL_RULE: &str = "Frame: the root layout already renders {shell} around this page, \
and {shell} draws the app's frame around the page's content (the navigation and {wrapper}). Return \
only this page's own content (its heading and sections, side by side in a fragment `<>...</>` or \
in an element of the page's own): never wrap it in {shell} or in {wrapper} again, and do not \
import {shell}.";

/// The texts of `ArchitectOptions::split_pages`: a page's local components
/// become files in a private folder of its route (`app/<route>/_parts/`,
/// which the App Router never serves), each a presentational part that only
/// that page renders, written at the same time as the page.
const PAGE_SPLIT: PageSplit = PageSplit {
    dir: "_parts",
    ext: ".tsx",
    export_prefix: "export default function ",
    part_note: " Part of the page `{page}`, in a file of its own so that it is written at the \
same time as the page: only that page imports it (its default export) and renders it. A Client \
Component (the first line is \"use client\") and presentational: it shows only what its props give \
it and reports each change through a callback prop; it never reads or writes the store, the storage \
module or `localStorage`, and keeps no copy of the app state (a form keeps only the drafts of its \
inputs). It renders only what its purpose and export comment name, never the page's heading or \
what another part of the page renders.",
    parts_note: " Parts of this page, each in a file of its own and written at the same time as \
this page: {parts}. Import each one as the default export of its path and render it where it \
belongs; do not write its markup in this file. This page holds the state they show and passes each \
one exactly the props of its contract (its data and its callbacks).",
    props_rule: "- Write each line as `Name: export default function Name({ a, onB }: { a: T; \
onB: (value: U) => void })  // what it shows; what each callback reports`, on one line, with the \
props typed inline. Use only the types listed above and TypeScript's own types (string, number, \
boolean, arrays, inline object types, `React.ReactNode`); import nothing else.\n- Give each part \
the data it shows and one callback for each change it makes; the page does the change with the lib \
functions above.",
};

/// The first bullets of the page-local rule, shared by [`PAGE_LOCAL_RULE`]
/// and [`SEMANTIC_PAGE_LOCAL_RULE`].
macro_rules! page_local_head {
    () => {
        "
- Pages own their features: a component that only one page renders is not a
  file of its own; that part of the page is written in the page's file (say
  it in the page's export comment). `components/*.tsx` holds only what the
  root layout renders and what two or more pages render; a page names each
  shared component it renders in its export comment (`renders <TaskCard/>`).
- A shared component is presentational: it takes its data as props and
  reports each change through a callback prop (`onSave`, `onMove`);"
    };
}

/// Added to the planner rules with `ArchitectOptions::page_local`. From the
/// blind judge (8 TaskFlow apps): a page's feature split between the page
/// and the components that separate workers write rendered comments,
/// subtasks and a rename form twice, and components that loaded their own
/// copy of the state overwrote what the page saved.
const PAGE_LOCAL_RULE: &str = concat!(
    page_local_head!(),
    " only a
  page (or the store provider of the layout) loads and saves the app state."
);

/// [`PAGE_LOCAL_RULE`] with `ArchitectOptions::semantics`: one state owner,
/// as the conventions say (pages that loaded and saved their own copy beside
/// the provider showed empty data and overwrote the seed, audit 2).
const SEMANTIC_PAGE_LOCAL_RULE: &str = concat!(
    page_local_head!(),
    " only the
  store provider of the layout loads and saves the app state; pages and the
  layout's shell component read and change it through the store hook."
);

/// Added to the planner rules with `ArchitectOptions::semantics`. From the
/// second blind audit (5 race pairs, all quality switches on): each file was
/// plausible alone, but the files disagreed on what nobody had stated once:
/// the sign of an amount (a validator that rejects the negative expenses the
/// form and the seed send), the format of a date (a parser of 'YYYY-MM-DD'
/// fed full ISO timestamps, so every metric was 0), which function moves a
/// status (a board move that never set `completedAt`), who owns the stored
/// state (a nav and pages that each saved their own copy), who renders the
/// shell (the nav twice) and how a record is shown (raw ids, a fixed 'USD').
const SEMANTICS_RULE: &str = "
- Conventions: `lib/types.ts` states, as code comments on its exports, the
  conventions that every file follows, once for the whole app:
  * Money: each money field gives its unit and its sign. An amount that
    moves money is integer cents, always > 0, and a field of the record
    (`kind: 'expense' | 'income'`) gives the direction; say it on the field
    (`amount: integer cents, always > 0, kind says expense or income`). A
    balance or opening balance is signed (`openingBalance: integer cents,
    signed, below 0 is owed`): 0 and debts are valid. Every lib validator,
    form and seed record follows these: a form sends the positive amount and
    the kind, never a negated amount.
  * Dates: each date field gives its format: a calendar date `'YYYY-MM-DD'`
    (`date`, `dueDate`) or a timestamp, full ISO (`createdAt`, every `*At`
    field). One dates module (`lib/dates.ts`) parses both forms and exports
    `today()`, the local calendar date.
  * Status: each status or stage union gives its transitions, the
    timestamps each one sets or clears, and the ONE lib function that makes
    them (`export type Status = 'todo' | 'doing' | 'done'  // any -> any via
    setTaskStatus (lib/tasks.ts); done sets completedAt, leaving done clears
    it`). That function is an export of its lib module, and every page that
    changes a status (board, detail, list) calls it.
  * Schedules (only when a record recurs: a bill, a subscription, a
    repeating task): its type stores `frequency` and `nextDue` (calendar
    date), the one source of the next occurrence, and one lib function pays
    or completes an occurrence and advances `nextDue`.
  Example: `export interface Transaction { id: string; accountId: string;
  amount: number; kind: Kind; date: string; createdAt: string }  // amount:
  integer cents, always > 0, kind says expense or income; date: calendar date
  'YYYY-MM-DD'; createdAt: timestamp, full ISO`.
- One state owner: the store provider (with its hook, `useStore`) is the
  only file that calls the storage module's load and save; the root layout
  renders the provider and the shell component (nav, theme toggle) around
  `{children}` and names both in its export comment (`renders <StoreProvider>
  + <AppShell/>`); the shell and every page read and change the state, the
  theme included, through the store hook, and no page renders the shell again.
  The hook also returns `ready` (false until the provider's useEffect read the
  stored state; the first render holds the seed data); say it in the hook's
  export line (`useStore(): { state, setState, ready }`). When the app can
  reset or clear its data (\"reset demo data\"), the hook returns that action
  too (`reset`), and no page imports the storage module. The provider
  renders a loading state instead of its children until `ready`, so no page
  or effect sees the seed data in place of the stored data.
- One catalog per fixed list (one the user cannot add to, such as labels):
  `lib/types.ts` declares it with its ids; no lib module or seed declares
  another copy. A list the user edits is a collection of the state.
- Display: pages and exports (CSV, reports) show names and labels, never
  ids or raw union values (resolve an id through its collection, a value
  through its label array); every input of a form is saved. Only when the app shows money
  (prices, amounts, balances): money is shown only through one format helper
  with the currency setting (`Intl.NumberFormat` with that currency's own
  fraction digits: 0 for JPY), in tables and CSV exports too.
  An app without money has no money module.
- Activity: when the prompt asks for an activity feed, log or history, the
  state type holds an `activity` collection (its record type in
  `lib/types.ts`: who or what, the action, a full ISO timestamp), the seed
  fills it, and one lib function appends an entry; every lib function whose
  change the feed shows (create, move, edit, comment, archive) appends one,
  and the feed page reads that collection.
- These comments may make `lib/types.ts` longer: its export limit leaves room for them.";

/// Ends the conventions block of the module head with
/// `PlanOptions::semantics`.
const CONVENTIONS_RULE: &str = "- Every validator, form, seed record and calculation follows the money and \
date conventions above: a validator accepts exactly what the forms and the seed send, and a form \
converts its input to that unit and sign before it saves.\n- Dates are parsed and compared only \
with the dates module, which reads both a 'YYYY-MM-DD' date and a full ISO timestamp; 'today' is \
the local calendar date (never `toISOString().slice(0, 10)`).\n- A status changes only through the \
function that the conventions name, which also sets and clears its timestamps; no other file \
changes `status` on an existing record (a create function sets only the initial status).\n- Show names and labels, never ids or raw union values, on pages and in exports (CSV) \
too (resolve an id through its collection, a value through its label array); in an app \
that shows money, format it only with the one money helper and the currency setting (never a \
literal 'USD'); every \
input of a form is saved in the record it creates or edits.\n- Loading: until `ready`, the state \
is the seed data. With a store provider, it renders a loading state instead of its children until \
`ready`: no page, component or effect under it ever sees the seed data in place of the stored data. \
Without one, a page that waits for its own `ready` returns the loading state after all of its \
hooks. A page that looks up a record by id shows 'not found' only once the state is loaded (never \
`notFound()` or a redirect before `ready`; after a delete it navigates away first).\n\
- Forms: a form that copies a record into local drafts is rendered with a key of the record id \
(`key={task.id}`; without a store provider that gates its children, `key={task.id + \
String(ready)}`); it never re-sets its drafts in a useEffect on the record, so a change made to the \
same record elsewhere on the page (a status select, a label toggle) never discards unsaved \
drafts. Each form has one save model: every field saves on change, or every field waits for its \
Save button, never both; a setting it previews (the theme) is what is saved. A settings form \
that saves on change keeps no drafts; one with a Save button copies the stored settings into \
drafts and is keyed by a value that changes only when the data loads or resets (a count that \
`reset` bumps), never by the settings object, so a theme toggle in the shell keeps its drafts \
and a reset refreshes them. In a form with a Save button, a number or money input keeps the typed \
text as a string draft and converts it on save; a save-on-change number field converts and saves \
on blur (or once the text parses), never re-formatting the value on each keystroke.\n- Domain \
logic: filtering, search, sorting, period and month math (shifting a month, month lists, ranges) \
and summaries use the lib export that the plan lists for them; when it lists none, the page \
computes them with the dates module and never imports a name that the plan does not list.\n- Activity: when the app has an activity feed, log or history, the state holds an activity \
list, every mutation appends an entry to it, and the feed reads that list (never rebuilt from the \
records).\n- Optional values: an optional field may be empty ('' or undefined); formatters and \
comparisons handle that (show nothing, skip it; an empty date is never overdue) and never throw in \
render; a form validates on save.\n- Charts: a chart draws every series that its lib function \
returns for display (an ideal and an actual line both).\n- Validation: call the lib function that may throw first, outside the state \
setter, catch its error there, then set the state from its result; never call it inside an \
updater (`setState((prev) => addTask(prev, ...))`: React runs updaters later, so the catch never \
sees the error).\n- Search: a result links to the record that owns it (a comment hit links to its \
task, by the comment's task id).\n- Catalogs: each fixed list (one the user cannot add to, such as \
labels) is declared once, in `lib/types.ts`; every other file, the seed included, imports it and \
uses its ids; a list the user edits is a collection of the state.\n\n";

/// Added to the prompt of each component that a layout renders with
/// `PlanOptions::semantics` (audit 2: an `AppShell` that loaded its own copy
/// of the state at mount saved it back from its theme toggle, over every
/// change the pages had made since).
const LAYOUT_COMPONENT_RULE: &str =
    "Layout component: the root layout renders it once around every \
page, so it stays mounted while the pages change. It reads and changes the app state, the theme \
included, only through the store hook; it never loads or saves storage itself and keeps no copy \
of the state.";

/// [`LAYOUT_COMPONENT_RULE`] for a plan with no store provider (no hook to
/// use): the same as the conventions block says for such a plan.
const LAYOUT_COMPONENT_BARE_RULE: &str =
    "Layout component: the root layout renders it once around every \
page, so it stays mounted while the pages change. The plan has no store provider: it keeps no copy \
of the app state; when it changes the state (the theme included) it loads the stored state again \
right before it saves and changes only its own part, never saving a state it loaded at mount.";

/// Appended to the purpose of a page that takes in its private components
/// (`PlanOptions::page_local`); `{components}` is the list of them.
const LOCAL_NOTE: &str = " Local components of this file (write each one in this file as an \
unexported function component, never as a file of its own, and do not import it): {components}. \
This page holds the state they show and gives each one its data and callbacks as props.";

/// Added to the prompt of each shared component with
/// `PlanOptions::page_local`.
const SHARED_COMPONENT_RULE: &str = "Shared component: it takes its data through props and \
reports each change through a callback prop (`onSave`, `onChange`, `onMove`). It never reads or \
writes the store, the storage module or `localStorage`, and never loads its own copy of the app \
state: the page that renders it is the only owner of the state on that page.";

/// Added to the planner rules with `ArchitectOptions::contract`. From the
/// blind audit: the plan stated a status set three times, two state shapes,
/// two category catalogs, settings without a reader and features without an
/// owner; each file then followed its own copy.
const CONTRACT_RULE: &str = "
- App contract: `lib/types.ts` declares, as code in its exports, the facts that
  several files share: each string union with a const array of its values and
  display labels (`export const STATUSES: { value: Status; label: string }[]  //
  [{value:'todo', label:'To do'}, {value:'done', label:'Done'}]`); each fixed
  catalog with its ids (categories, members, labels: `export const CATEGORIES:
  Category[]  // [{id:'food', name:'Food'}, ...]`); `export const STORAGE_KEY`;
  one `export interface AppState` with every collection of the app and
  `settings: Settings`; `export interface Settings { ... }` and `export const
  DEFAULT_SETTINGS: Settings`. The comment of the Settings export gives each
  field's meaning and the files that read it: `// defaultProjectId: project the
  board opens -> app/board/page.tsx; theme: color scheme -> lib/store.tsx,
  components/AppShell.tsx`.
- The seed, the lib modules and the pages import these unions, catalogs and
  defaults from `lib/types.ts` and never define their own; every literal in
  another file's comment is a value of these unions.
- One owner file for each collection (its lib module or the store exports its
  create or add, update and remove functions) and for each UI control (a form,
  a filter, a sort). A page names the components it renders in its export
  comment (`renders <TaskForm/>`); every component is rendered by a page.
- Reversible states: a state the user can put a record into (archive, close,
  hide) has its reverse export in the same lib module (unarchive, reopen) and
  a control for both on the page that sets it; archived or closed records stay
  reachable through a filter or section of their page.
- A fixed catalog is a const of `lib/types.ts`, not a collection of
  `AppState`; only data the user creates is a collection.
- These rules replace the export limit below for two kinds of file:
  `lib/types.ts` has up to 14 exports and a lib module that owns a collection
  up to 6; the JSON may grow past ~1800 tokens by those exports.";

/// Added to the wiring prompt with `PlanOptions::wiring_rules`.
const WIRING_RULE: &str = "
Also: every export whose name starts with create, add, update, remove, delete,
toggle or set is called by at least one listed page (or the store owner, or a
component whose contract already takes the store or the action): a component
whose contract takes data as props gets the action as a prop from its page,
not a store call of its own. A page lists the components it renders.";

/// Added to the module head with `quality_rules`: one owner for each
/// mechanism that several files share.
const QUALITY_HEAD_RULE: &str = "- Theme (when the app has one): one function of the store or \
theme module applies the theme, only as `document.documentElement.dataset.theme` (\"light\" or \"dark\"), called from a \
useEffect on the stored theme or from the event that saves it; the style sheet gives the dark values on \
`:root[data-theme=\"dark\"]`. No other file sets a theme class or attribute (a plan line that \
names a theme class means this attribute).\n- State: one store. Pages read and change app \
data only through the plan's store hook or provider, or, when the plan has none, through the \
storage module's load and save functions called from a useEffect and from event handlers (never \
a new store or hook of their own); a component whose contract takes data as props uses its props. \
Only the storage module touches `localStorage`. Stored data loads in a useEffect, so the first client render equals the server \
render (seed data or an empty state); never read storage during render or in a server \
component. Every write saves the whole state, with every collection. When nothing is stored yet, \
that first load saves the seed at once (then sets `ready`), so record ids stay the same across \
reloads and a deep link survives a refresh.\n- Ids: new ids come from \
`crypto.randomUUID()` (or the time plus a random part), never from a module counter.\n- \
Settings: every field of the app's settings (its settings type in the contracts above) is read \
by each file whose behaviour it changes (a default selection, a currency, a start day), not only \
saved; a period start day moves every month or week boundary that the lib computes.\n\n";

/// Added to the purpose of the seed data with `relative_dates`, in place
/// of a fixed day: fixed dates go stale, and dates around one day leave a
/// 12-month chart empty.
const RELATIVE_DATE_NOTE: &str = " Compute every date when the data loads, relative to the \
current date, with a small helper in this file such as `daysAgo(n)`; never write a fixed \
calendar date. Cover what the pages show over time (records in each of the last 12 months for a \
12-month chart, records in this week for this-week figures), and date nothing after today \
except due dates (next due dates and target dates); a record of the current month falls on a day \
up to today, never a fixed later day of the month. Records that belong to a period (a budget \
per month) exist for the current period and the earlier periods the pages show, so a carry-over \
and the previous period have data.";

/// The variables that all parts of the style sheet use. The first part
/// defines them, so the parts agree without seeing each other.
macro_rules! css_variables {
    () => {
        "--bg, --surface, --surface-2, --text, --muted, --border, --accent, \
--accent-contrast, --danger, --success, --warning, --radius, --shadow"
    };
}

/// Measured in `bench/showdown` (its `split_css`): the global style sheet is
/// the longest file that starts with the wave (about 3,000 tokens, 10 s),
/// and a style sheet joins without conflicts: each worker writes the rules
/// of its own classes (7 for each part, 2 to 5 parts) into its own part, and
/// the planned file only imports the parts (`css_imports`). The parts use
/// one fixed list of variables that the first part defines.
const CSS_SPLIT: Split = Split {
    glob: "*globals.css",
    min_exports: 2 * CSS_PART_CLASSES,
    part_exports: CSS_PART_CLASSES,
    max_parts: 5,
    part_pattern: "styles/part{n}.css",
    first_purpose: concat!(
        "Part 1 of {count} of the global style sheet. Starts with the reset, the \
         `body` and base element styles, and the variables ",
        css_variables!(),
        " on `:root` with their dark values on `.theme-dark`. Then only the rules of the \
         classes listed for this part (with their hover, focus and small-screen rules). All \
         colours come from the variables."
    ),
    part_purpose: concat!(
        "Part {n} of {count} of the global style sheet: only the rules of the classes \
         listed for this part (with their hover, focus and small-screen rules). No reset, \
         no `:root`, no `body` rules. All colours, radii and shadows come from these \
         variables, which part 1 defines for the light and the dark theme: ",
        css_variables!(),
        "."
    ),
    joined_purpose: "Imports the parts of the global style sheet.",
};

/// Classes for one part of the style sheet.
const CSS_PART_CLASSES: usize = 7;

/// The fixed text of a global style sheet that only imports its parts
/// (`CSS_SPLIT`), in a `css` code block as a worker gives it; `None` for
/// any other path, or when the manifest has no parts for it.
fn css_imports(manifest: &Manifest, path: &str) -> Option<String> {
    let file = manifest
        .files
        .iter()
        .find(|file| file.path == path && file.exports.is_empty())?;
    if !file.path.ends_with("globals.css") {
        return None;
    }
    let dir = file.path.rsplit_once('/').map_or("", |(dir, _)| dir);
    let prefix = if dir.is_empty() {
        "styles/part".to_string()
    } else {
        format!("{dir}/styles/part")
    };
    let text: String = manifest
        .files
        .iter()
        .filter(|part| part.path.starts_with(&prefix) && part.path.ends_with(".css"))
        .map(|part| {
            format!(
                "@import \"./{}\";\n",
                part.path[dir.len()..].trim_start_matches('/')
            )
        })
        .collect();
    (!text.is_empty()).then(|| format!("```css\n{}\n```", text.trim_end()))
}

/// Added to the plan of the seed data: the same data in fewer tokens.
const SEED_NOTE: &str = " Write each record as one compact object literal on one line, with \
short realistic text values. Generate repeating records (one per month or week the pages show, \
per-period records such as budgets included) with a loop over the periods instead of listing \
each one, so the file stays under 120 lines while every period has data. Every collection and \
sub-collection that the pages show (comments, subtasks, archived records) has at least two \
records.";

/// Added to the prompt of each `lib` file that is written after the types.
const TYPES_WIN_RULE: &str = "\n\nThe attached types file is the authority. If a line of the \
plan for this file names a field, a literal value or a parameter shape that the attached types \
do not have, use the names and the shapes of the attached types, and keep the name and the \
purpose of each planned function. A parameter that takes domain records has the shared type \
(`Item[]` for a shared type `Item`), not an inline object shape (`{ name: string }[]`). A type that holds the whole \
app state has one collection for each entity type that the attached types declare.";

/// Added to the prompt of each `.tsx` file that is not a test.
const NEXT_RULE: &str = "\n\nNext.js rules: a file that uses React hooks, event handlers or \
browser APIs starts with the line \"use client\". A component that calls useSearchParams() is \
an inner component of the same file, and the file renders it inside <Suspense>. Import each \
component in the form that the plan declares for it: `import Name from` for `export default`, \
`import { Name } from` for a named export. Move between the app's pages only with `<Link>` from \
next/link or `router.push`, never with `<a href>` to an app route or `window.location` (external \
links and downloads keep `<a>`).";

/// Added to the prompt of each test file.
const TEST_RULE: &str = "\n\nTest rules: read the attached code of a function before you \
assert on it, and work out each expected value from that code with a small input (one to three \
items). Do not assert a value that depends on today's date, on a generated id or on the order \
of equal items. A few certain assertions are better than many uncertain assertions. For a function whose code is not attached (the seed data), assert only what holds for any data: collections that are not empty, unique ids, references that resolve; never an exact count or value. Give \
test data the shared types (`const items: Item[] = [...]` for a shared type `Item`) and make each record with a small \
factory function that sets every required field, so that literal values keep their types.";

/// Added to the prompt of every staged file.
const IMPORT_RULE: &str = "\n\nImport only project files that the plan lists. Never import a \
project file that is not in the plan (for example a `./SomethingClient` beside a page): write \
that code inside your own file.";

/// Added to the prompt of a task that waits for earlier files.
const STAGED_RULE: &str = "\n\nThe project files that are already written are attached below as \
prerequisite outputs. Import from them. Use exactly the names, fields, signatures and literal \
values that they declare, and do not declare again a type that they export.";

/// `PYTHON`'s edge rule in TypeScript syntax.
const EDGE_RULE: &str = "
- After each export, a `//` comment pinning its edge behavior in a few words:
  empty/zero input result, which invalid inputs throw which error,
  missing-key/not-found result, and exact bounds (e.g.
  `export function make(n: number): number[]  // n===0 -> []; n<0 -> throws RangeError`).
  An export returning an object type starts that comment with its
  `{key: type}` shape and puts the edges after it — an edge note never
  replaces the shape. Pin every derived quantity a test could assert: each
  returned array's length relative to the inputs (e.g. `sequence.length ===
  length`, not nextLength), how returned items relate to input items, and
  whether initial/default collections start empty or seeded (e.g. `decks:
  the built-in decks; cards: []`).";

/// Suites cover the pure logic modules only: components, routes and config
/// need a DOM or a framework runtime that a contract-only test writer
/// cannot drive.
fn is_testable(path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path);
    path.ends_with(".ts")
        && !name.ends_with(".d.ts")
        && !name.contains("config")
        && !path.starts_with("app/")
}

/// Deterministic Next.js toolchain files (fenced, like a worker's output).
/// Toolchain config is rendered, not generated: a worker-written tsconfig
/// enabled noUncheckedIndexedAccess and broke 8 files written to standard
/// strictness (nextjs-memory-2), and model-pinned versions were an
/// advisory-flagged next@14.2.5. Caret ranges let npm resolve the newest
/// patched release in each line. Next 16 / React 19 (current as of 2026-10);
/// TypeScript stays on 5.x because Next's build-time type check drives the
/// TypeScript JS API, which the native 7.x compiler does not ship yet.
fn scaffold(path: &str) -> Option<String> {
    let body = match path {
        "package.json" => {
            r#"```json
{
  "name": "app",
  "version": "0.1.0",
  "private": true,
  "scripts": {
    "dev": "next dev",
    "build": "next build",
    "start": "next start",
    "test": "vitest run"
  },
  "dependencies": {
    "next": "^16.4.3",
    "react": "^19.3.1",
    "react-dom": "^19.3.1"
  },
  "devDependencies": {
    "@types/node": "^24.19.2",
    "@types/react": "^19.3.0",
    "@types/react-dom": "^19.3.0",
    "typescript": "^5.9.3",
    "vitest": "^5.0.5"
  }
}
```"#
        }
        "tsconfig.json" => {
            r#"```json
{
  "compilerOptions": {
    "target": "ES2017",
    "lib": ["dom", "dom.iterable", "esnext"],
    "allowJs": true,
    "skipLibCheck": true,
    "strict": true,
    "noEmit": true,
    "esModuleInterop": true,
    "module": "esnext",
    "moduleResolution": "bundler",
    "resolveJsonModule": true,
    "isolatedModules": true,
    "jsx": "react-jsx",
    "incremental": true,
    "plugins": [{ "name": "next" }],
    "paths": { "@/*": ["./*"] }
  },
  "include": [
    "next-env.d.ts",
    "**/*.ts",
    "**/*.tsx",
    ".next/types/**/*.ts",
    ".next/dev/types/**/*.ts"
  ],
  "exclude": ["node_modules", ".check"]
}
```"#
        }
        "next.config.mjs" | "next.config.js" => {
            "```js\n/** @type {import('next').NextConfig} */\nconst nextConfig = {};\n\nexport default nextConfig;\n```"
        }
        _ => return None,
    };
    Some(body.to_string())
}

/// Exports a Next.js App Router route file may have besides its default.
const ROUTE_EXPORTS: &[&str] = &[
    "metadata",
    "generateMetadata",
    "viewport",
    "generateViewport",
    "generateStaticParams",
    "revalidate",
    "dynamic",
    "runtime",
];

/// Next.js rejects any other named export from `page`/`layout` files at
/// build time (`next build`: "Property 'SessionTable' is incompatible with
/// index signature"). A contract listing one makes the worker export it, so
/// drop those exports: the worker then keeps the helper local to the file.
fn restrict_route_exports(manifest: &mut Manifest) {
    for file in &mut manifest.files {
        // React 19 dropped the global `JSX` namespace; contracts annotated
        // `: JSX.Element` and every component copied it (TS2503 in 8 files,
        // nextjs-memory-3). Inferred return types are correct and portable.
        for export in &mut file.exports {
            *export = export.replace("): JSX.Element", ")");
        }
        let name = file.path.rsplit('/').next().unwrap_or(&file.path);
        let route = file.path.starts_with("app/")
            && ["page.tsx", "layout.tsx", "page.ts", "layout.ts"].contains(&name);
        if !route {
            continue;
        }
        file.exports.retain(|export| {
            let head = export.split("//").next().unwrap_or(export);
            head.contains("export default")
                || head
                    .split(|ch: char| !ch.is_alphanumeric())
                    .any(|word| ROUTE_EXPORTS.contains(&word))
        });
    }
}

/// The measured fixups (`PlanOptions::tidy`), ported from
/// `bench/showdown`: `next_client_pages`, then `next_dyn_route`.
fn tidy_manifest(manifest: &mut Manifest) {
    next_client_pages(manifest);
    next_dyn_route(manifest);
}

/// An app with no server code keeps its data in the browser, so a page
/// that the plan made an `async` Server Component (to read the query
/// string) cannot read that data: the worker then adds hooks to it and the
/// build fails. Such a page (not a dynamic route) becomes a Client
/// Component that reads the query string with `useSearchParams()`.
pub fn next_client_pages(manifest: &mut Manifest) {
    let has_server = manifest.files.iter().any(|file| {
        file.path.contains("/api/")
            || file.path.ends_with("route.ts")
            || file.path.contains("actions")
    });
    if has_server {
        return;
    }
    for file in &mut manifest.files {
        let is_page = file.path.ends_with("page.tsx") && !file.path.contains("/[");
        // Any plan line that reads `searchParams` (an `async` page, or "reads
        // q from awaited searchParams"), unless it is a Client Component.
        let reads_params =
            |line: &str| line.contains("searchParams") && !line.contains("Client Component");
        if !is_page || !file.exports.iter().any(|line| reads_params(line)) {
            continue;
        }
        let has_default = file
            .exports
            .iter()
            .any(|line| line.contains("export default"));
        for line in &mut file.exports {
            // The page itself: its default export, or (with none) the line
            // that reads the query string.
            let page_line = if has_default {
                line.contains("export default")
            } else {
                reads_params(line)
            };
            if !page_line || line.contains("Client Component") {
                continue;
            }
            let Some(name) = line
                .split("function ")
                .nth(1)
                .and_then(|rest| rest.find('(').map(|end| rest[..end].trim().to_string()))
            else {
                continue;
            };
            let note = client_page_note(line.split_once("//").map_or("", |(_, note)| note.trim()));
            *line = format!(
                "export default function {name}()  // Client Component: the first line is \
                 \"use client\"; an inner component reads the query string with \
                 useSearchParams() from next/navigation and the page renders it inside \
                 <Suspense>; {note}"
            );
        }
    }
}

/// The planner's note of a page that `next_client_pages` makes a Client
/// Component, without what only a Server Component does: a clause that
/// awaits the query string goes ("awaits searchParams Promise {q?: string}"
/// keeps its parameters as "query parameters {q?: string}"), "awaited
/// searchParams" reads "the query string", and "client page" (now said in
/// full) goes. A Client Component that awaits a prop does not build.
pub fn client_page_note(note: &str) -> String {
    note.split(';')
        .filter_map(|clause| {
            let clause = clause.trim();
            let lower = clause.to_lowercase();
            if lower == "client page" || lower == "client component" {
                return None;
            }
            if let Some(rest) = clause.strip_prefix("awaits searchParams") {
                let rest = rest.trim_start();
                let rest = rest.strip_prefix("Promise").unwrap_or(rest).trim();
                return (!rest.is_empty()).then(|| format!("query parameters {rest}"));
            }
            if let Some(rest) = clause.strip_prefix("awaits ") {
                // "awaits q": the parameter it names.
                let rest = rest.trim();
                return (!rest.is_empty()).then(|| format!("query parameter {rest}"));
            }
            let clause = clause
                .replace("awaited searchParams", "the query string")
                .replace("awaited ", "");
            (!clause.is_empty()).then_some(clause)
        })
        .collect::<Vec<_>>()
        .join("; ")
}

/// A page with a route parameter (`app/tasks/[id]/page.tsx`) is planned as
/// one Client Component that reads the parameter with `useParams()`: a
/// Server Component page then needs a client file that the plan does not
/// have.
pub fn next_dyn_route(manifest: &mut Manifest) {
    for file in &mut manifest.files {
        let is_page = file.path.ends_with("/page.tsx") && file.path.contains("/[");
        if !is_page {
            continue;
        }
        if file
            .exports
            .iter()
            .any(|line| line.contains("Client Component"))
        {
            continue;
        }
        let page_line = file
            .exports
            .iter()
            .find(|line| line.contains("export default") && line.contains("function "))
            .or_else(|| file.exports.iter().find(|line| line.contains("function ")));
        let name = page_line
            .and_then(|line| {
                let rest = line.split("function ").nth(1)?;
                let end = rest.find('(')?;
                Some(rest[..end].trim().to_string())
            })
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| "Page".to_string());
        // The planner's note says what the page shows; keep it.
        let note = page_line
            .and_then(|line| line.split_once("//"))
            .map(|(_, note)| note.trim())
            .filter(|note| !note.is_empty())
            .map(|note| format!("; {note}"))
            .unwrap_or_default();
        file.exports = vec![format!(
            "export default function {name}()  // Client Component: the first line is \
             \"use client\"; reads the route parameter with useParams() from \
             next/navigation; returns a not-found message when no record has that id, \
             before any other use of the record; a handler that uses the record is an arrow \
             function in a const (`const save = () => ...`), never a `function` declaration \
             (inside a hoisted function TypeScript forgets that the record was found); all the \
             code of the page is in this file, \
             apart from the planned components it renders{note}"
        )];
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::architect::ManifestFile;

    /// Audit 6: TaskFlow plans got an unused `lib/money.ts` (the display
    /// rule asked every app for a money helper), and no plan held an
    /// activity log, so the file rule for the feed had no state to read.
    #[test]
    fn semantics_rule_plans_money_only_when_shown_and_an_activity_log() {
        assert!(SEMANTICS_RULE.contains("An app without money has no money module."));
        assert!(SEMANTICS_RULE.contains("the\n  state type holds an `activity` collection"));
        assert!(CONVENTIONS_RULE.contains("in an app that shows money"));
        assert!(SEMANTICS_RULE.contains(
            "the hook returns that action
  too (`reset`), and no page imports the storage module."
        ));
        assert!(!SEMANTICS_RULE.contains("money is shown only through one format helper with the\n  currency setting; every input"));
    }

    /// Audits 6-8: thin seeds (12-month charts and earlier budgets empty),
    /// one-way archive and close, mixed save models, a stale settings form
    /// after a reset, full reloads for in-app links, a fixed currency.
    #[test]
    fn rules_ask_for_looped_seeds_reversible_states_one_save_model_and_links() {
        for needle in [
            "with a loop over the periods instead of listing each one",
            "so the file stays under 120 lines while every period has data",
            "has at least two records.",
        ] {
            assert!(SEED_NOTE.contains(needle), "{needle}");
        }
        assert!(RELATIVE_DATE_NOTE
            .contains("exist for the current period and the earlier periods the pages show"));
        assert!(CONTRACT_RULE
            .contains("has its reverse export in the same lib module (unarchive, reopen)"));
        assert!(CONTRACT_RULE.contains("reachable through a filter or section"));
        for needle in [
            "Each form has one save model: every field saves on change, or every field waits for \
             its Save button, never both",
            "A settings form that saves on change keeps no drafts",
            "changes only when the data loads or resets (a count that `reset` bumps), never by \
             the settings object",
        ] {
            assert!(CONVENTIONS_RULE.contains(needle), "{needle}");
        }
        // Keying by the whole settings object remounts the form on every
        // keystroke or theme toggle.
        assert!(!CONVENTIONS_RULE.contains("JSON.stringify(state.settings)"));
        assert!(QUALITY_HEAD_RULE.contains("from the event that saves it"));
        assert!(!QUALITY_HEAD_RULE.contains("from the event that changes it"));
        assert!(NEXT_RULE.contains(
            "only with `<Link>` from next/link or `router.push`, never with `<a href>` to an app \
             route or `window.location`"
        ));
        // Schedules and money digits are planned only when the app has them.
        assert!(SEMANTICS_RULE.contains("* Schedules (only when a record recurs"));
        assert!(SEMANTICS_RULE.contains("that currency's own\n  fraction digits: 0 for JPY"));
    }

    /// Audit 9 (Ledgerly h2): a balance planned `always > 0` (empty accounts
    /// and debts rejected), a seed kept only in memory (new ids on every
    /// reload), month math and filters inside pages, an amount input that
    /// re-formatted on each keystroke, future rows in the current month, an
    /// unused month-start setting.
    #[test]
    fn rules_sign_balances_save_the_seed_and_keep_domain_logic_in_lib() {
        assert!(SEMANTICS_RULE.contains("A\n    balance or opening balance is signed"));
        assert!(SEMANTICS_RULE.contains("0 and debts are valid"));
        assert!(!SEMANTICS_RULE.contains("Use ONE convention"));
        assert!(QUALITY_HEAD_RULE.contains(
            "When nothing is stored yet, that first load saves the seed at once (then sets `ready`)"
        ));
        assert!(QUALITY_HEAD_RULE.contains(
            "a period start day moves every month or week boundary that the lib computes"
        ));
        for needle in [
            "- Domain logic: filtering, search, sorting, period and month math",
            "use the lib export that the plan lists for them; when it lists none, the page \
             computes them with the dates module and never imports a name that the plan does not list",
            "In a form with a Save button, a number or money input keeps the typed text as a \
             string draft and converts it on save; a save-on-change number field converts and \
             saves on blur",
        ] {
            assert!(CONVENTIONS_RULE.contains(needle), "{needle}");
        }
        assert!(
            RELATIVE_DATE_NOTE.contains("a record of the current month falls on a day up to today")
        );
    }

    #[test]
    fn route_files_keep_only_default_and_framework_exports() {
        let file = |path: &str, exports: &[&str]| ManifestFile {
            path: path.into(),
            purpose: "x".into(),
            exports: exports.iter().map(|e| e.to_string()).collect(),
            uses: Vec::new(),
        };
        let mut manifest =
            Manifest {
                files: vec![
                file(
                    "app/progress/page.tsx",
                    &[
                        "export default function ProgressPage(): JSX.Element",
                        "export function SessionTable(props: {sessions: Session[]}): JSX.Element",
                    ],
                ),
                file(
                    "app/layout.tsx",
                    &[
                        "export const metadata: Metadata  // {title}",
                        "export default function RootLayout(): JSX.Element",
                    ],
                ),
                file("components/table.tsx", &["export function SessionTable(): JSX.Element"]),
            ],
                test_files: vec![],
                readme: false,
            };
        restrict_route_exports(&mut manifest);
        assert_eq!(manifest.files[0].exports.len(), 1);
        assert!(manifest.files[0].exports[0].contains("export default"));
        assert!(!manifest.files[0].exports[0].contains("JSX"));
        assert_eq!(manifest.files[1].exports.len(), 2, "metadata stays");
        assert_eq!(manifest.files[2].exports.len(), 1, "non-route untouched");
    }
}
