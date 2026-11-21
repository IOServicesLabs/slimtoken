# SlimToken Showdown

Showdown is a terminal benchmark with two sides. The left side is one
model stream that writes a full app in one answer. The right side is
SlimToken: it plans the app, writes the files with many streams at the
same time, then checks and repairs the result.

Both sides get the same objective, the same model and the same
streaming client. Each side writes its app to its own directory. When
the two sides finish, the benchmark installs, builds, tests and opens
the pages of each app, and it can start both apps in your browser.

```text
╭ SINGLE STREAM · 1 request ───────────── ✓ DONE in 95.4s ╮╭ SLIMTOKEN · plan + parallel wave ────── ✓ DONE in 42.7s ╮
│ █▀█ █▀▀   █ █                               ▀▀█ █▀▀ █ █ ││ █ █ ▀▀█   ▀▀█                         ▄█  █▀█ █ █ █▀█ │
│ ▀▀█ ▀▀█   ▀▀█                                ▀█ █▀█ ▀▀█ ││ ▀▀█ █▀▀     █                          █  █ █ ▀▀█ █ █ │
│ ▀▀▀ ▀▀▀ ▀   ▀                               ▀▀▀ ▀▀▀   ▀ ││   ▀ ▀▀▀ ▀   ▀                         ▀▀▀ ▀▀▀   ▀ ▀▀▀ │
│ SECONDS                                AVG TOKENS / SEC ││ SECONDS                                AVG TOKENS / SEC │
│ 61 files · 4,345 lines on disk                          ││ 49 files · 3,940 lines on disk · planned in 7.2s        │
│ install ✓  build ✗  test ✓ 44 passed  pages ✗           ││ checked ✓ 43 fixed in 10 files  install ✓  build ✓  …   │
╰─────────────────────────────────────────────────────────╯╰─────────────────────────────────────────────────────────╯
  SlimToken 2.2x faster  ·  42.7s vs 95.4s  ·  only the SlimToken app builds and passes its tests
```

(The numbers in this picture are from one live run with an earlier
version of the repair step. See [Measured runs](#measured-runs) for
the present version.)

## Build

```bash
cd bench/showdown
cargo build --release
```

The binary is `bench/showdown/target/release/showdown`. Use a terminal
with true color and a size of 120 x 36 or more (the minimum is 84 x 24).
The default objective needs Node.js and npm for the checks.

## Run a live benchmark

Put your API key in the environment variable of the provider. Then
start the run:

```bash
export DEEPSEEK_API_KEY=...
showdown run
```

The default objective is "TaskFlow", a project management app with
Next.js and nine pages (`objectives/taskflow.txt`). Give your own
objective with `--objective "..."` or `--objective-file PATH`.

The run opens with the prompt: the full text that both sides get, the
model, and a countdown of 8 seconds. No request is sent and no clock
runs while it shows. Enter or space starts at once; `--intro-secs N`
sets the time and `--intro-secs 0` leaves the screen out. A replay
opens the same way.

The run writes these files to `slimtoken-out-showdown/<date-time>/`:

| Path | Contents |
|---|---|
| `single/` | The app from the single stream |
| `slimtoken/` | The app from SlimToken |
| `recording.jsonl` | Every event of the run, with its time |
| `results.json` | Wall time, tokens, rates, file counts and check results |
| `_meta/` | The raw single answer, the manifest, the engine report and trace, the repair report |

Keys during a live run:

| Key | Effect |
|---|---|
| `l` | Start both apps and open them in the browser |
| `r` | Show or hide the result panel |
| `c` | Run the checks again |
| `o` | Open the output directory |
| `q` | Quit (this stops the apps that `l` started) |

## Quality checks

When the two sides finish, the same checks run in each app directory.
They run after the clocks stop. The result of each check shows in the
pane of its side and in the result panel.

| Check | Command | Pass |
|---|---|---|
| `install` | `npm install --no-audit --no-fund` | exit code 0 |
| `build` | `npm run build` (type check included) | exit code 0 |
| `test` | `npm test` | exit code 0 |
| `pages` | start the built app, request each static page | each page answers with status 200 |

The verdict line at the bottom shows the speed result and the check
result together, for example "both apps build and pass their tests".

- `--no-check` skips the checks.
- `--check-cmd "..."` runs one command of yours in place of the four
  steps. Use it for an objective that is not a Next.js app.

## Look at the two apps

Press `l` when the run finishes, or give `--launch` to do this
automatically. The benchmark starts the dev server of each app on a
free port and opens the two apps in your browser. The servers stop when
you quit.

To start the apps of a finished run later:

```bash
showdown launch                                   # the newest run
showdown launch slimtoken-out-showdown/<date-time>
```

On Windows with Edge or Chrome installed, each app opens in its own
window without tabs or address bar, the single stream on the left half
of the screen and SlimToken on the right half, as on the benchmark
screen. Each window has its own browser profile, so the two apps do not
share stored data. `SHOWDOWN_BROWSER` names another Chromium browser;
`SHOWDOWN_TILE=0` opens the default browser as before.

`--launch-cmd` sets the command that starts an app (the default is
`npm run dev -- -p {port}`).

## Replay a run

A replay shows the recording of a live run with the measured times.
Use it to capture video or screenshots.

```bash
showdown replay slimtoken-out-showdown/<date-time>/recording.jsonl
```

| Key | Effect |
|---|---|
| `space` | Pause or continue |
| `←` / `→` | Halve or double the speed |
| `0` | Start again |
| `l` | Start both apps (the app directories of the run must still exist) |
| `r` | Show or hide the result panel |
| `q` | Quit |

The header shows `REPLAY` and the date of the live run. A recording
contains the absolute paths of the two app directories.

## Demo mode

`showdown demo` plays simulated data. Use it to examine the layout or
to rehearse without an API key. The data is not a measurement. The
header and the result panel show `SIMULATED DATA` in red. Do not
publish numbers or pictures from demo mode as results.

## How the SlimToken side works

1. **Plan.** The engine's architect (`generate_manifest`,
   `plan_from_manifest`) decides the file layout and the interface
   contracts. Shared-type ownership is on: each shared type has one
   owner file.
2. **Write in three stages.** Files in one stage are written at the
   same time. The shared type files are first. The other `lib` modules
   are second, and they see the type files. Pages, components and tests
   are third, and they see the type files and the `lib` modules. A
   later file is thus written against the real text of the files it
   builds on. `--flat` writes all files in one stage against the plan
   only: that is faster, but the files agree less often.
3. **Check and repair.** A gate checks the assembled files in two
   steps. The fast step type-checks them (`tsc`), runs the tests
   (Vitest), and checks rules that the compiler does not see: the
   `"use client"` directive, the `<Suspense>` boundary for
   `useSearchParams()`, and a context provider that no file renders.
   When the fast step passes, the gate does the real thing: it runs
   `next build`, starts the built app and requests each static page.
   These are the same commands as the `build` and `pages` quality
   checks, so files that pass the gate pass those checks.

   Each file that the gate names is repaired by its own stream, with
   the problems and the current text of the files it imports. A long
   file with few problems gets search-and-replace edits; other files
   are written again. An import that names a file that nobody wrote
   becomes a new file. An import with the wrong number of `../` is
   corrected with no model call. A file that still has problems after
   its own repair gets a joint repair: one stream sees that file, the
   files that its problems name and the shared types, and can change
   all of them (for example two files that each declare the same type).

   The gate runs again after each round, for at most
   `--repair-rounds` rounds (default 8). The loop stops when the gate
   passes or when three rounds in sequence bring no better result. The
   files with the best result are kept. `--no-repair` skips this step.

The gate needs TypeScript, Vitest and the framework types. The first
run installs them in `slimtoken-out-showdown/_deps` before the clocks
start, and later runs use them again. The step is available for
objectives that the engine detects as Next.js.

This staging and this repair loop are code of the benchmark
(`src/arms.rs`, `src/repair.rs`, `src/gate.rs`). They use the engine's
public library (`execute_plan` with task dependencies). The shipped
`slimtoken` command has its own repair step (`reconcile`), which is
different.

## How the measurement works

- **Same request path.** The two sides use one streaming client
  (`src/tap.rs`). It sends the same request shape as the engine's
  shipped client and adds a display hook for each text delta.
- **Single side.** One request: the objective, then a short format
  instruction (a path heading and one code block for each file). The
  benchmark needs the format to split the answer into files. The token
  limit is `--single-max-tokens` (default 128,000). If the answer stops
  at the limit, the result panel shows `cut off`.
- **Clock.** Each side has its own clock. It starts when the side
  starts and stops when the side has its final files. For SlimToken
  that includes the plan, all stages, and the full check-and-repair
  step with each gate run. The result panel also shows the time at
  which the code was written (before check and repair). Disk writes and
  the quality checks are not part of the time.
- **The single side gets no repair step.** It is one request, as the
  name says. Its app is checked with the same four checks.
- **Tokens.** A finished request counts the usage that the server
  reports. A request that still streams counts an estimate from its
  text length. A `~` shows that a total contains an estimate. The
  final totals are the reported usage.
- **Input tokens.** SlimToken sends the contracts, and in later stages
  the earlier files, to each worker. It uses many more input tokens
  than one stream (about 650,000 against about 500 in the run below).
  The result panel shows the input tokens of each side and how many of
  them the provider served from its prompt cache (about 30% for
  SlimToken in the races of 2026-11-20).

### Race or sequential

`--order race` (the default) starts the two sides at the same moment.
This is the best live show. The two sides share the server and the
rate limits of one API key during the run.

`--order sequential` runs the single stream first, then SlimToken.
The sides do not share the server. A replay of that recording shows
the two sides side by side, each on its own measured clock. Use this
order for numbers that you publish.

One run is one sample. Server speed changes from minute to minute, and
the model does not write the same app twice. For a claim, do several
runs and report all of them.

## Measured runs

All runs: 2026-11-14, `deepseek-flash`, TaskFlow objective, all
defaults. "Pass" means that all four quality checks pass (install,
build, test, pages).

### Current version

Five races, in sequence. The first three ran on an earlier build of
this version, the last two on the final build:

| Run | Single stream | Checks | SlimToken | Checks | Speedup |
|---|---|---|---|---|---|
| 183809 | 85.4 s | pass | 26.4 s | pass | 3.24x |
| 190030 | 82.5 s | pass | 38.6 s | pass | 2.14x |
| 190225 | 81.4 s | test fails | 28.5 s | pass | 2.86x |
| 195631 | 89.3 s | pass | 24.4 s | pass | 3.67x |
| 195832 | 92.5 s | test fails | 24.9 s | pass | 3.71x |

- SlimToken used 39,000 to 44,000 output tokens for each run; the
  single stream used 30,000 to 33,000.
- Run 190030 used four repair rounds on one test that stayed wrong.
  The changes to the repair of tests (below) came after it.

The SlimToken side alone (`showdown bench-slim`, a new plan for each
run, time to a passing real build), last two batches of ten runs:
21.9 s to 35.9 s, median about 27 s, and 20 of 20 passed the gate's
real build. Four of the 20 needed no repair.

What changed since the earlier version (`SHOWDOWN_OFF=<name>` turns
one change off, to measure it; `SHOWDOWN_ON=<name>` turns on a change
that is off until it is measured):

**Default quality stack (since 2026-11-16).** When `SHOWDOWN_ON` is not
set, `showdown` turns on `qrules, qobjective, qliteral, qcontract, qwiring,
qcss, qlint, pagelocal, qsemantic, qlint2`, and since then `typesfirst,
shellfix, nocut, filebudget` (2026-11-17) and `typesplan, jointedit, testedit,
specwave, hedge` (2026-11-18: 12% faster over 8 alternating pairs on the
held-out apps, level on the blind judge) and `contractside` (2026-11-18: a
failing test whose expectation the module's contract states is not bent to
the module; the module is repaired) (`arms::RECOMMENDED_ON`; the
gate and other child processes inherit it). Set `SHOWDOWN_ON` yourself
(empty for none) to choose. Evidence, blind Claude audits of DeepSeek
runs (coverage and working lenses, 0-100):

| Comparison | Pairs | Without | With | Judgments won |
|---|---|---|---|---|
| TaskFlow: stack without vs with `qsemantic`/`qlint2` (after the hydration fixes) | 4 | 63.0 | 84.2 | 8 of 8 |
| Ledgerly: same comparison (before the hydration fixes) | 2 | 57 | 74 | 4 of 4 |

Time cost of `qsemantic`/`qlint2` (4 runs each, medians): TaskFlow
32.2 s to 34.3 s, Ledgerly 41.2 s to 50.2 s (extra lint repair rounds).
Measured and left off: `prefix` (no wave or cache gain) and `thinktests`.

- Tests are written as six files, not one (`shards`); the seed data
  starts with the `lib` stage (`seed`); the scheduler starts with 64
  streams, not about 5 (`wide`); four streams write the style sheet as
  parts that `globals.css` imports (`csssplit`; after the wave the parts
  are joined into `globals.css`, so the app has one style sheet). Together: the writing
  stage went from about 23 s to about 13 s.
- The plan is corrected before a worker reads it: one owner for each
  type, pages with a route parameter or a query string are planned as
  client pages (`onetype`, `oneowner`, `dynroute`, `clientpages`).
- Mistakes that need no model are corrected with no model call: the
  "use client" line, the form of an import, an import from the wrong
  file (`autofix`).
- The real build starts at the same time as the fast checks, it stays
  valid while only tests are repaired, it does not check the types a
  second time, and the checks keep their caches between rounds
  (`specdeep`, `notypes`, `keepdir`). A run with one repair round
  spends about 6 s in the check step; a run with none, about 3.5 s.
- The gate reports every wrong assertion of a test, not only the first
  (`softexpect`). A test that stays wrong after two repairs is repaired
  together with the module that it tests, with reasoning on, because
  the module can be the wrong side (`testonly`, `think`).
- A request that waits too long for its first token starts again
  (`watchdog`).
- The plan is built by the engine's own planning call, with the steps
  above as its manifest pass (`engplan`). Off, the bench makes the same
  calls itself; the plan is the same.
- The six test files come from the engine's `min_test_shards` option
  (`engshards`). Off, the bench adds the test files to the manifest
  itself; the plan is the same (the saved manifest then lists six test
  files, not one). With `shards` off, neither runs. `rerepair` plans a
  run that wrote the manifest's own test files without the engine's
  option. `rerepair` leaves the `q*` quality steps off: the saved
  manifest reaches the repair prompts as it is. The switch is temporary: it and the bench's own step go once
  A/B runs show the same results.
- The stages, the prompt rules and the dropped cache primes come from
  the engine's `staged` option with the NEXTJS staging data
  (`engstage`). Off, the bench stages the plan itself; the plan is the
  same. The rule, seed and prime switches keep their meaning on both
  paths (the table at `slim_profile` in `src/arms.rs`). The switch is
  temporary, as `engshards`.
- The style sheet's parts and the `globals.css` that imports them come
  from the engine's `split` option with the NEXTJS `staging.split` data
  and `aggregate` hook (`engcss`). Off, the bench splits the style sheet
  and renders the imports itself; the plan is the same. With `csssplit`
  off, neither runs. The switch is temporary, as `engshards`.
- The gate runs through the engine's `check::CommandChecker` and its
  output is read by `check::parse_check`; the file blocks, the edit
  blocks and the text of each file come from `slimtoken::project`
  (`engcheck`). The checker sends the files as a document of its own
  (one `## path` heading and one fence for each file, in path order) and
  sets `SLIMTOKEN_CHECK_MODE`/`SLIMTOKEN_CHECK_DIR` next to the
  `SHOWDOWN_GATE_*` names; the gate reads either. The engine's edit
  format words one sentence differently from the bench's. A `PATCH`
  block in the gate output is applied with no model call (the gate
  writes none today). Off, the bench pipes its own document and parses
  the output itself. A test checks that both parsers give the same
  result for gate outputs that runs recorded. The switch is temporary,
  as `engshards`.
- The trace (`_meta/slimtoken-trace.jsonl`) keeps the wave and every
  repair round: the engine appends to it (`SchedulerConfig.trace_append`)
  and opens each run with a `plan_start` line that carries the run's
  number and the wall-clock time (`traceall`). Off, each round overwrites
  the trace and only the last run is left.
- After a passing build, the gate requests only the pages that the build
  did not prerender with status 200 (a route of
  `.next/prerender-manifest.json` is skipped only when neither its entry
  nor its saved `.next/server/app/<path>.meta` has another status or a
  redirect `location`, so a page that calls `notFound()` or `redirect()`
  is still requested), and starts no server when none is left
  (`prerendered`). A prerendered page already rendered without an error
  during the build, and the server sends its saved HTML. On 20 saved apps
  and 4 broken copies (`notFound()`, `redirect()`, a throw at request
  time, a throw during the build), 3 runs each with the switch on and
  off, the verdicts were the same; the page step took 0.19 s against
  0.83 s on average, and the deep check 2.73 s against 3.36 s.
- The reason of a page that fails starts at the server's first `⨯` line,
  so that the start-up lines the server prints before it do not fill the
  reason (`gatereason`, its own switch so that an A/B of `prerendered`
  measures only the skipped pages).
- Each type that a planned signature names and no file declares gets a
  line in an owner file (`typedecl`; the engine's
  `PlanOptions::declare_types`, or the same pass after the bench's tidy
  steps with `engtidy` off). 285 of the 296 saved manifests gain at
  least one line, 228 a real declaration (an interface from a comment
  shape, or an alias such as `TaskStatus = Status`); the other 57 gain
  only `export type X` lines that ask the worker for one.
- A persistent type checker (a Node process with the TypeScript builder
  API, `src/pchk.cjs`) gets each wave file as its request closes and
  checks in the background; Vitest runs once every planned test and the
  files it imports have landed (`pchk`). The repair step asks it and the
  command-line gate at the same time: when it names problems, the first
  repair round starts at once and the command-line result is compared in
  the background (`pchk` in `_meta/slimtoken-repair.json`: checks,
  agreements, false fails, times). A pass always comes from the command
  line. The next real build waits for the one before it inside its own
  job, not before the fast checks. Replayed without the model
  (`showdown pchk-replay --deps <_deps> <run>...`: the files land in the
  recorded close order, then the saved files and six changed versions
  are checked) on 12 saved apps: first result 0.53 s against 1.54 s
  (medians), later checks 0.48 s against 1.11 s, 84 of 84 results agree.
- Guards against problems that a repair round creates in files it did
  not touch (`guard`; the engine's `RepairPolicy::guard`, see
  `slimtoken::guard`). The repair of a file that other files import
  lists the exports they use and where ("Used elsewhere"); after the
  round a changed or removed one counts as a lock break and its users
  join the next joint repair. When two or more files report that one
  module does not export a name (TS2305, TS2724, TS2614, TS2459), a file
  that declares it gets `export` (or the imports point at it) with no
  model call; else the module gets one repair and the importers wait a
  round. A file with only TS7006 lines waits while a file it imports is
  repaired. Before each gate run the root layout gets an unrendered
  provider (TS9004) or frame component (TS9005) with no model call.
  `slimtoken-repair.json` (`guard`, `layout_fixes`), the pane and the
  `bench-slim` line (`lock_breaks`) show what they did. On copies of 5
  saved apps with no model (`cargo test --release -- --ignored dry_`):
  an export removed from a type used by 10 to 17 files went from 17 to
  30 problems to a pass in one round with no model call (guards off: two
  rounds, the problems stay); a provider taken out of the layout passed
  the first check (guards off: TS9004 stays). Not measured with the
  model yet.
- `SHOWDOWN_INDEX=indexio` (default `text`) gives the guards' lookups to
  indexio (`SHOWDOWN_INDEXIO=<exe>`; default the build in
  `../indexio/target/release` if there is one, else
  `~/.cargo/bin/indexio`). Each closed wave file is mirrored to
  `_meta/indexio/src` and indexed on a thread of its own (the first close
  runs `index`, later ones `reindex --worktree`, one at a time, at least
  250 ms apart); the export lock asks `impact --file`, a lock break
  `impact --diff-file`; each importer is confirmed by resolving its
  import; each call has a 2 s timeout and falls back to the text index
  (`fallbacks`). Every call passes `--data-dir _meta/indexio/data`. The
  pane shows one line (`indexio v0.1.15 · 46 files · index 112 ms · ...`)
  and the lookups during verify; the results show an indexio row;
  `results.json` (`indexio`), `slimtoken-repair.json` and the
  `bench-slim` line carry the version and the counters. On 5 saved apps
  (indexio 0.1.14 and 0.1.15): a full index 96 to 132 ms, a re-index
  30 to 35 ms, a lookup about 22 ms; for 128 modules with importers the
  confirmed importers were the text index's in 128 of 128 (0.1.15: the
  same), and the broken-export lookups agreed in 5 of 5.
- Not measured yet, so off unless `SHOWDOWN_ON=examples` turns it on:
  each pure calculation of the plan gets one worked example
  (`e.g. f(...) -> result`), the module prompts ask the code to give that
  result, and the test prompts ask the tests to assert it (`examples`).
  The tests and the code then come from the same examples.
- Not measured yet, so off unless `SHOWDOWN_ON=engpipe` turns it on:
  the SlimToken arm is one call of the engine's `pipeline::build`
  (`engpipe`; it needs `engplan`, `engrepair` and `engcheck`, else the
  arm runs as before: the pipeline writes the files with the engine's
  `project` helpers, which `SHOWDOWN_OFF=engcheck` would swap for the
  bench's). It plans, writes, checks and repairs with the options, profile
  and repair hooks that the `eng*` switches give today; the bench parts
  (the staging steps, `_meta`, the events, `pchk`, the guards' index with
  `SHOWDOWN_INDEX`, the gate with `prerendered`, the trace) run in the
  pipeline's hooks at the same points, and the files go to disk after
  `Done`, as before. The other switches keep their meaning. Differences:
  only files whose title is a path are written (`project::write_project`;
  every file of the Next.js app is one), and `slimtoken-report.json` has
  `phases.reconcile_ms`. The bench copies stay until A/B runs show the
  same results.
- Not measured yet, so off unless `SHOWDOWN_ON` names them: nine
  quality steps from the blind audit (the single stream's app won 10 of
  10 judgments; most problems were in the plan or the prompts). `qrules`,
  `qobjective` and `qliteral` add no model call; the Q2 steps below them
  (`qcontract`, `qwiring`, `qcss`, `prefix`) wire the app contract into the
  plan and add at most one planner round (`qcontract`, when its check
  fires) and one wiring task in the wave (`qwiring`); the Q3 step `qlint`
  adds no call of its own, but each `ERROR` line it finds costs a repair
  round (about 10-20 s) until the file is fixed. `pagelocal` (from the
  blind judge) adds no call; it makes fewer, longer page tasks. The Q4
  steps from the second blind audit (`qsemantic`, `qlint2`) add at most
  one planner round (`qsemantic`, shared with `qcontract`) and, like
  `qlint`, a repair round per `ERROR` file (`qlint2`).

  | Switch | Engine option | What changes |
  |---|---|---|
  | `qrules` | `PlanOptions::quality_rules`, and `relative_dates` in place of `today` (with `seeddates`) | Every module head ends with fixed rules: the theme only as `document.documentElement.dataset.theme` set by one store or theme function in an effect, dark values on `:root[data-theme="dark"]` (`.theme-dark` in part 1 of the style sheet and in a planned class is rewritten); one store (the plan's store hook or provider, else the storage module's load and save from effects and handlers; components with data props use their props), only the storage module touches `localStorage`, load in an effect, every write saves the whole state; ids from `crypto.randomUUID()`; each field of the settings type read where it applies. The seed gets dates computed at load (`daysAgo(n)`, covering a 12-month chart and this week, nothing after today but due dates) instead of "Today is ...". |
  | `qobjective` | `PlanOptions::objective_pages` | Each `app/**/page.tsx` prompt gets its `Pages:` bullet verbatim ("You own every feature in this bullet: ..."), the quality bar without its prefix and with "no import of a file that you do not write" read as "that the plan does not list", and the route list (a static detail page links as `/task?id=${id}`); each component "the page owns the layout; you own only what your contract lists"; the head lists purposes and "do not render again what an imported component's purpose says it renders". A detail bullet goes to a dynamic route before a list page (`/tasks/[id]`, not `/tasks`). On the 5 saved manifests all 45 pages match their bullet. |
  | `qliteral` | `PlanOptions::align_literals` | A quoted literal in the plan that equals a member of a `types.ts` string union apart from case and separators gets its spelling (`'Done'` -> `'done'`) where it is a value: compared, a `case`, a union member, an item of a list of literals. Mapped text (`'done' -> 'Done'`), prose and labels stay. On the 5 saved manifests: 15 literals in 083407 and 083947. |
  | `qcontract` | `ArchitectOptions::contract`, `PlanOptions::contract` | The planner rules ask `lib/types.ts` for the app contract as code (unions with value/label arrays, catalogs with fixed ids, `STORAGE_KEY`, one `AppState` with every collection, `Settings` and `DEFAULT_SETTINGS`, and in the Settings comment each field's meaning and readers, `field: meaning -> path`), one owner per collection (its create/add, update and remove exports) and per UI control, and pages that name the components they render. The types fragment sees every planned file; export limits rise (types file 14, collection owners 6). `contract::contract_problems` checks the merged manifest; each fragment that owns a problem is asked once more (one concurrent round, only when it fires). Each named reader gets "Settings this file reads". The problems left are in `_meta/slimtoken-contract.json`. On the 5 saved manifests the check reports 14, 8, 14, 5 and 3 problems. Adds one planner request round when it fires. |
  | `qwiring` | `PlanOptions::wave_wiring` + `wiring_rules` | The in-wave wiring task (it runs beside the types and lib stages) also lists each setting's readers and requires a caller for every create/add/update/remove export; pages and components (not the CSS parts) wait for it and get "Calls: ...". |
  | `qcss` | `PlanOptions::consumers_see_css` (`stage_plan` with `engstage` off) | Pages, layouts and components also wait for the CSS parts and see their real selectors. The parts end before the lib stage in 4 of the 5 saved runs (0.05 s later in 083407); +1.5-3.2k input tokens per consumer. |
  | `prefix` | `PlanOptions::deps_first` (`Task::prefix_split`) | The prerequisite outputs (sorted by task id) come between the shared head and each task's own tail, so sibling prompts share a long prefix. Offline on the saved runs (with `qcss`): pages and components share 9.3-14.9k tokens instead of 2.7-5.1k. |
  | `qlint` | none (bench gate, `qlint.rs`) | The gate (and `pchk`) adds cross-file lints of a Next.js app. `ERROR` lines, repaired like type errors, each naming the file that should change and saying what to change: `settings-unread` (TS9010, a settings field that only the settings UI, storage, seed and types mention; names the page that should read it: the board for a default project (the page that links to a dynamic `[id]` board), the money pages for a currency, budgets for a month start, the state owner for a theme), `literal-case` (TS9011, `task.status !== "Done"` where every declaration of `status` that the file can see (its own, the shared types file's, its imports') spells it `"done"`; a type name resolves through the file's own declaration or import, bare parameters and a line that also checks `"done"` are skipped), `theme-selector` (TS9012, the code sets `data-theme` but the CSS has only `.theme-dark`, or the reverse, or nothing applies the theme), `dead-route` (TS9013, an internal `href`/`router.push` with no page, or `/x/new` caught by `[id]`), `server-storage` (TS9014, a page or layout without "use client" that reads `localStorage`/`window` or calls a function of a module that does; strings ignored in both; a layout or a file exporting `metadata` is told to keep the Server Component and move the read into a client child), `partial-save` (TS9015, a save of an object literal without spread that leaves out fields of the save function's state type or of the seed's state; a `Partial`/`New*`/`Create*`/`*Input`/`*Patch` parameter type is skipped), `id-collision` (TS9016, a module counter id with no time or random part; the fix keeps the id format the tests pin: a `syncIds(ids)` in the id module, and a second line on the state provider to call it after the state loads), `seed-dates` (TS9017, more than 3 fixed ISO dates in a seed file and no date arithmetic; an `ERROR` only with `qrules` and `seeddates`, whose plan asks for relative dates, else a `WARN`, since the `seeddates` note alone asks for fixed dates around the run day: measure `qlint` together with `qrules`). `WARN` lines (hints, never a failure): `readme-spec` (TS9018: the README is the plan's static output, which a repair cannot change) and `css-undefined-class` (TS9019). On the saved apps: 0 `ERROR` lines on the 5 single-stream apps of the audit, 19 on the 5 SlimToken apps (every one a defect the audit names or that the code shows); on all 41 saved single-stream apps 8 `ERROR` lines, each a real unread setting (checked by hand); 33 `css-undefined-class` hints in 32 of 42 SlimToken apps against 5 in 3 of 41 single apps. |
  | `pagelocal` | `ArchitectOptions::page_local`, `PlanOptions::page_local` | Pages own their features (from the blind judge: about 6 duplicate-UI and 4.8 state-out-of-sync defects per app). The planner rules say a component that only one page renders is written in that page's file and shared components are presentational. After the other manifest passes, each component that exactly one page names (`renders <X/>`, directly or through a component folded into it) leaves the manifest and goes into that page's purpose as local, unexported components; a component that no file names goes into the one page its stem, id prop or purpose points to. Every page gets `max_tokens` 8192 (a folded manifest's pages keep it under `rerepair`). Layout components, components of 2+ pages, state owners (providers, hooks, contexts), components a lib module or a test needs, and unnamed components that match no single page stay; each one left (holding no state, not on a layout, props not only ids) is told to take data through props and report changes through callbacks, never the store or storage. With `qobjective`, each page also gets its shared components ("do not render again what its purpose says"), links from the routes (tasks link to `/tasks/${id}`, `/board/[projectId]` is a project's page; a page "for one project" reads `?project=` and project lists link there, for declared record types only) and a control for each `createX`/`addX`/`upsertX`/`saveX` lib export of a record it owns. Offline on the saved manifests: the 5 `qcontract` bench runs go from 50-52 to 34-38 model tasks (9 pages absorb 1-4 components each; the task page about 4.0-4.2k tokens of code, the slowest stream); in 4 of the 8 older or base runs 6-7 pages absorb one unnamed component each (2 in qbase-2, none in the Ledgerly plans). |
  | `qsemantic` | `ArchitectOptions::semantics`, `PlanOptions::semantics` | From the second blind audit (judges preferred the single stream's code 5 of 5: files disagreed on money sign, date format, status lifecycle, state owner, shell). The planner rules ask the comments of `lib/types.ts` to state each convention once (money unit and sign, each date field's format, each status union's transition function and its timestamps, the one store provider, the shell the layout renders); the types fragment sees every file and `semantic_problems` sends what is missing back in the repair round that `qcontract` uses. Every module head ends with one conventions block (the stated convention, else the default: an amount in integer cents always > 0 with an expense/income kind, a balance signed, a limit, target or price >= 0, a part such as a split with the sign of its record; `*At` full ISO, other dates 'YYYY-MM-DD'), and a component that the layout renders keeps no copy of the state (it uses the store hook, or, in a plan with no store provider, loads the state again right before each save). The store owner is the provider that holds the app state, never a theme provider. `_meta/slimtoken-semantics.json` holds the problems left and the block. Offline on the 5 audited plans: it names the audit's money, date, status, state-owner and shell problems before any file is written; module prompts +1.3-2k characters (shared head). Audit 3 (TaskFlow got worse: one store whose first render is the seed): the State line says the store hook also returns `ready` (false until the stored state was read), and the fixed rules add a loading state before 'not found' for a record looked up by id (never `notFound()` or a redirect before `ready`), a form keyed by the record id and `ready` (or resynced in an effect), a throwing lib call made before the state setter (never inside its updater), search hits that link to their owning record, and one catalog per fixed list in `lib/types.ts`. Audit 5 (the default stack against the single stream; the single stream better 7 of 10): the store provider renders a loading state instead of its children until `ready` (no page or effect sees the seed in place of the stored data); a form is keyed by the record id alone under that gate and never re-sets its drafts in an effect on the record (the old resync advice made a status change discard unsaved edits); an activity feed reads an activity list that each mutation appends to; optional values may be empty (formatters and comparisons skip '' and undefined, never throw in render); a chart draws every series its lib function returns. Tradeoff (not yet measured in a race): behind the gate the server render of each page is the loading state, so `next build` prerendering and the gate's page-status check no longer catch a render-time exception (a formatter that throws on an empty field); it shows only in the browser after hydration. |
  | `qlint2` | none (bench gate, `qlint.rs`; with or without `qlint`) | Ten convention lints (six from audit 2, four from audit 3), each an `ERROR` that names the file to change: `storage-outside-store` (TS9020, a page or component that loads or saves storage beside a store provider; with no provider a layout component that saves a copy it loaded once), `duplicate-shell` (TS9021, a page that renders the layout's shell or frame again), `sign-mismatch` (TS9022, a lib validator that throws on the negative amounts the forms or the seed send; the producers instead when the types file states a positive amount), `date-format-mismatch` (TS9023, a strict 'YYYY-MM-DD' helper given a stored ISO timestamp), `transition-timestamps` (TS9024, a status change that a screen calls and that never sets `completedAt`), `hardcoded-currency` (TS9025, a money formatter given a literal currency code while the settings have `currency`). TS9020 needs a store provider that the root layout mounts and skips context providers and files that keep their own key (a theme); TS9022 reads the convention from the field's comment, its JSDoc or its type's comment, and with `qsemantic` an open field follows the positive default of the prompts; TS9023 resolves each call to the helper it reaches; TS9025 words the fix by file kind (a props-only component takes an optional `currency` prop, and each renderer that passes none gets its own line). On the saved races: 0 hits in 47 + 5 single-stream apps; 29 of 48 SlimToken apps get at least one (all 5 audited apps, every convention class of audit 2). `showdown lint` and the judges' lint counts include them when `SHOWDOWN_ON` names `qlint2`. From audit 3, four more: `render-before-ready` (TS9026, a page that calls `notFound()`/`redirect()`/`router.replace()` on a record read by a route parameter without a guard on the store's `ready` flag (or an inverse `loading`), or a form that copies that record into useState once without a key with the flag or a resync effect on the record or the flag (`[task.id]` alone is not one); a store without a flag gets its own line; a provider or layout component that shows the pages only once the flag is set, a page that waits for its null state, and forms under a store that starts empty are fine), `throw-in-updater` (TS9027, a lib function containing `throw` called inside a state updater in a try block, through a `useState` setter or a store function that runs its callback in one), `search-link` (TS9028, a comment hit that keeps the comment's id and is linked by it), `duplicate-catalog` (TS9029, a second exported `{ id }` catalog with the name of one in the types file and other ids, on the copy). On the saved races: 0 hits in 47 single-stream apps, 39 of 431 SlimToken apps get at least one (TS9026 12, TS9027 27, TS9028 7, TS9029 2); the audit-3 qsemantic TaskFlow apps get TS9026-TS9028 (and TS9029 where it applies), the base ones none. From audit 5, three more, and TS9026 now puts the provider gate first, accepts an effect on the flag but not one on the record, and flags nothing under a gating provider (a form keyed by the id alone included): `draft-resync` (TS9030, a component that re-sets its form drafts from a record's fields in a useEffect on the record object; an effect on the id, an expression of it, a one-shot copy or an object without an id is not flagged; the line says where to key the form by the id, with `record?.id ?? 'new'` for a prop that may be missing, and calls an effect on the id alone fine only under a gating store), `ungated-store` (TS9031, only with `qsemantic`: a store provider that starts from the seed and renders its children before `ready` while a page looks a record up by a route or query parameter or an effect copies the seed into local state and keeps it, not a default pick that only fills an empty value; replaces TS9026's store line; any shape of the gate counts: `if (!ready) return`, a ternary on the flag with `children` or `props.children` in either branch, a ternary around the provider), `empty-format` (TS9032, a lib `format*` function that throws on an empty value, called unguarded in render with a field that is optional in the types file or that another call shows only behind a truthiness test). On the saved races: 0 hits of the three in 52 single-stream apps (13 `ungated-store` if `qsemantic` is forced on them); 53 SlimToken apps: 5 `draft-resync`, 9 `ungated-store`, 1 `empty-format`; the 5 audit-5 SlimToken apps all get `ungated-store`, d3 and d5 `draft-resync`, d5 `empty-format`. `showdown lint` prints the switches it used on stderr; with no `SHOWDOWN_ON` it turns `qsemantic` on, so `ungated-store` then reports on single-stream apps too. | Plus TS9033 `activity-unwritten` (audit 7, a `WARN`): an activity log in the state that only the seed fills.
| `thinktests` | `think` on the joint repair of a stuck test file | Off by default. Reasoning on a test repair ran 213 s (16k hidden tokens, no answer, a retry) and 47 s in race 20261116-051055; with it off, stuck tests repair at temperature 0.8 without reasoning and code files keep `think`. |

  Also without a switch (fixes): the README is rendered with the app's
  name, a summary, the page list as features, setup and scripts and the
  file purposes without engine notes (the old one ended at "## Run"); a
  page that reads `searchParams` without `async function` becomes a
  Client Component too (`clientpages`); a dynamic page keeps its planned
  note and may render its planned components (`dynroute`).

### Blind quality judge

`showdown judge <run dir>` judges the two apps of a finished run blind and
writes `judge.json` in the run directory (`_judge/` holds the bundles sent,
the raw answers and `mapping.json`, which unblinds the labels). It makes
model calls through a provider preset (`--provider deepseek` by default,
`--model`, `--effort high`). It never runs during a race, so it never
touches the measured times.

- **Static lint** (no model call): the `qlint` lints on each app, as counts
  (`--no-llm` stops here).
- **Requirements**: `objectives/<name>.reqs.json` (TaskFlow 50 items,
  Ledgerly 49; both `"reviewed": false` until a person reviews them). The
  judge refuses a run whose objective fingerprint (SHA-256 of the trimmed
  text) differs.
- **Model judge**: per app, `--samples` (2) coverage calls (one status per
  requirement, the list shuffled per sample) and `--samples` working calls
  (defects by class: settings, contract, owner, state, seed, readme,
  duplicate, and cross-file: money sign and unit, date against timestamp,
  completedAt/startedAt on every path, one state owner, the shell rendered
  once, names not ids, the currency setting), the numbered bundle as the
  shared context (cached); then two
  pairwise calls with the apps in both orders. Every quote is checked
  against the cited file within 5 lines: an unverified bug is dropped, an
  unverified "full" becomes "partial", a "missing" whose search terms occur
  in the UI code is disputed. A cross-file bug needs a second quote
  (`other`) that verifies in another file (paths compared after `./` and
  `\` are normalised), and both quotes must be specific (12+ non-space
  characters, or an identifier of 4+ characters that is not a keyword:
  `=` or `return` would match almost any line), else it is dropped. The
  prompt's worked examples use invented paths that match no fixture app.
  A requirement on which the samples disagree
  (full against missing/stub, or disputed) gets one tiebreak call.
- **Delivery** (no model call): install, build, tests and pages, read from
  the run's `results.json` (or `_meta/slimtoken-repair.json`), and filled in
  by the judge's own `next build` and vitest run where nothing is recorded
  (a difference with the recorded result is noted, not used).
- **Runtime smoke** (no model call): the app is copied to
  `<deps>/.check/judge-<pid>-<tag>` and built with the shared toolchain
  (both arms alike, never an arm's own `node_modules` or `.next`; the next
  version is recorded in the notes), served with `next start`, and driven in
  headless Chrome or Edge by `sidecars/qa/lib/qa-judge-smoke.mjs`: every
  static route must answer below HTTP 400, show text that is not a Next
  error page, and throw no uncaught exception; then the objective's
  `smoke.flows` run (TaskFlow: create project, create task, move a card to
  done and reload, the dashboard's "completed this week" goes up from the
  reading taken before the move (no reading: the flow fails), comment on a
  task; Ledgerly: add an expense and net worth drops by it, edit an expense
  and net worth moves by the signed difference, mark one bill paid and its
  own row changes without an error and stays changed on reload, the
  expense survives a reload, the reports show non-zero values). Controls
  are found by label, placeholder, aria name or text; a step that finds no
  control is `not_found`, one whose check fails is `failed`; both fail the
  flow. smoke = 100·(passed flows + routes check)/(all flows + 1): a flow
  the time budget cut off, a route it never loaded, or a flow missing from
  the report counts as failed, so a slow app is scored on the same flows
  as a fast one. Every CDP call has a timeout (a script that loops is
  terminated and fails its step); JS dialogs are answered (an alert that
  reads like an error fails the flow); a watchdog writes the report 10 s
  after the budget. A built app whose browser run writes no report scores
  0 (it hung the browser), not "absent". A hydration mismatch is reported
  but fails nothing. Flags: `--no-smoke`, `--no-checks` (no local vitest), `--deps`
  (default `slimtoken-out-showdown/_deps`), `--smoke-script`,
  `--smoke-budget` (90 s per app for the browser part; the build has its
  own 300 s limit and the server 45 s to start), `--smoke-parallel` (2).
  About 5 s build and 15-20 s browser per app.
- **Score**: cov = 100·Σ(w·s)/Σw (full 1, partial 0.5); work = max(0, 100 −
  8·broken − 3·wrong − cosmetic) from the mean verified bug counts;
  q_if_built = Q = 0.4 cov + 0.4 work + 0.2 smoke, renormalised over the
  layers present (0.5 cov + 0.5 work without smoke; the summary says which).
  q_delivered: an app that does not run (install or build fails, or no page
  renders) gets 0.25·Q; otherwise Q − 2 per failed test (at most −10), times
  the share of pages that render when some fail. The winner is the higher
  q_delivered (`winner_basis`), except that an app that runs always beats
  one that does not (0.25·Q of a high-Q app that does not build can exceed
  the score of a running app with few pages); `winner_if_built` is beside it;
  "confirmed" when both pairwise orders pick the if-built winner.
- **Blinding**: labels X and Y in an order drawn from the run's name and
  `--seed`; no folder or run names in the text; the bundle is refused if it
  names `slimtoken`, a single stream or the run.
- A `bench-slim` run has no single arm: the reference is `--ref DIR`, by
  default the pinned single app of the objective
  (`slimtoken-out-showdown/20261115-083407/single` for TaskFlow,
  `.../20261115-083658/single` for Ledgerly). `--objective` names the
  objective when the run has no `results.json`.

`showdown judge-validate` runs the judge on the pairs of the blind audits
(`--fixture`, repeatable, a built-in name or a file; default both:
`audit-20261115`, five pairs, and `audit-20261116`, five pairs with a
delivered and an if-built score and winner per app; apps from
`slimtoken-out-showdown/<run>/<arm>`, or `--apps DIR` for the blind copies
`DIR/<pair>/<label>`) and prints, per fixture, the winner per pair and per
lens (delivered, if built, cov, work, pairwise, delivery, smoke, lint)
against the audit, Pearson and Spearman correlation of cov, work,
q_if_built and q_delivered with the audit's scores (all apps, and within
each arm) and of smoke, the pairwise agreement and flips, and the recall
of the audit's cited defect lines; `--repeats N` judges each pair N times.
A tie is not counted as a pick. The q_delivered correlation is also given
over the apps that run only: with the apps that do not build in it, it is
dominated by them (their low judge and audit scores sit far from the rest).
`--no-llm` reports the lint, delivery and smoke layers only;
`--from-judge` scores each run's saved `judge.json` (no model call): its
cov, work and model part of Q come from the prompts of the time it was
written, so it measures the current delivery and smoke layers only. The
runtime flags above apply.

In-sample, not held out: each fixture states what was tuned on it
(`sample`, printed with its report). `audit-20261116` is in-sample for the
cross-file checklist, the delivery formula and the smoke flows, and
`audit-20261115` partly (the first prompts and the smoke locators), so
agreement with either measures fit; a held-out measure needs a new audit.

Offline, on 2026-11-16 (in-sample, see above): from the saved `judge.json`
of the five `audit-20261116` pairs (cov and work from the earlier prompt),
q_delivered picks the audit's delivered winner in 5 of 5 pairs and
q_if_built its if-built winner in 5 of 5; q_delivered against the audit's
delivered score, Pearson 0.93 / Spearman 0.88 (n 10), but 0.73 / 0.79 over
the 8 apps that run (the two apps that do not build carry the rest).
The smoke score alone: Pearson 0.79 / Spearman 0.85 against the audit on
`audit-20261116` (n 8), 0.47 / 0.44 on `audit-20261115` (n 9), where
4 of the 5 TaskFlow pairs have no way to create a task in either app.

`showdown lint <app dir>...` prints the `qlint` findings of app directories.

Tested and not kept: pages that start before the `lib` code is written
(2 s faster, 4 of 6 runs then failed the build); one file for each
planning request (0.5 s faster, about 10% more tokens); three more
prompt rules about props, strict mode and the types file (no gain in
ten runs against ten).

Tests change to agree with the code when a test fails, unless the
repair finds that the module is wrong. A passing test run is therefore
a check that the app is consistent with its tests, not an independent
proof of each feature.

### Earlier version

Nine races in sequence, before the changes above.

| Run | Single stream | Checks | SlimToken | Checks | Speedup |
|---|---|---|---|---|---|
| 152940 | 101.3 s | pass | 86.9 s | pass | 1.17x |
| 155117 | 82.1 s | build fails | 65.3 s | pass | 1.26x |
| 155315 | 85.8 s | build and test fail | 49.4 s | pass | 1.74x |
| 155507 | 87.4 s | pass | 45.5 s | pass | 1.92x |
| 155720 | 97.1 s | build fails | 74.9 s | build fails | 1.30x |
| 160206 | 79.3 s | pass | 58.6 s | pass | 1.35x |
| 160355 | 93.6 s | pass | 48.3 s | build fails | 1.94x |
| 160716 | 88.7 s | build fails | 87.8 s | pass | 1.01x |
| 160911 | 85.7 s | pass | 56.9 s | pass | 1.51x |

- The SlimToken app passed in 7 of 9 runs, the single-stream app in 5
  of 9. The median time was 58.6 s for SlimToken and 87.4 s for the
  single stream.
- The two SlimToken failures led to two corrections during this series,
  so the runs are not samples of one version. Run 155720 used all
  repair rounds on a context provider with an empty first value; the
  repair request now explains that case and the round limit is 8. In
  run 160355 the gate passed, but the file writer wrote a file with two
  code blocks where the gate had read one; the writer now writes
  exactly what the gate read. The two runs after the last correction
  passed.
- The SlimToken time depends on the number of repair rounds: 2 rounds
  in the fastest runs, 5 in the slowest.
- SlimToken used 41,000 to 68,000 output tokens for each run; the
  single stream used 28,000 to 36,000.

Five races are a small sample. Do more runs, in sequential order,
before you publish a number.

## Disk space

Each run installs the dependencies of two apps (about 0.7 GB). A new
run removes `node_modules` and `.next` from the earlier runs in
`slimtoken-out-showdown`; the app sources, recordings and results stay,
and `launch` installs again when necessary. `--keep-deps` keeps them.

## Other providers

```bash
showdown run --provider muse
showdown run --provider none --base-url http://127.0.0.1:8000/v1 --model my-model
```

A provider preset reads its key from its own environment variable
(`DEEPSEEK_API_KEY`, `MUSE_API_KEY`, `ANTHROPIC_API_KEY`) or from a
`.<provider>-key` file in the current directory. Do not put a key in a
file that git tracks.

## Test

```bash
cargo fmt
cargo test
cargo clippy --all-targets -- -D warnings
```

To see frames as plain text (no terminal necessary):

```bash
showdown demo --snapshot 150x44@9,end
```
