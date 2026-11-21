# The Showdown benchmark: what it does and how it is tested

Showdown answers one question: **how much faster does a complete, working
app arrive when many model streams write it together than when one stream
writes it alone?**

It runs both ways on the same task, at the same time, with the same model,
and then proves which apps work.

| | Single stream (left) | SlimToken (right) |
|---|---|---|
| Requests | 1 | about 60 |
| How the app is written | one long answer, file after file | a plan, then the files in parallel waves |
| Repair | none | checked and repaired before the clock stops |
| Typical time (Taskflow, DeepSeek) | 82 to 108 s | 26 to 45 s |
| Typical output rate | 230 to 350 tokens/s | 1,100 to 1,400 tokens/s |

For commands and keys, see [README.md](README.md). This document explains
the method.

## The task

Each run gets one objective: a plain-text brief of about 1,700 characters
for a complete Next.js app with eight or nine pages, shared data, tests and
a README. Nothing else is given to either side.

| Set | Apps | Use |
|---|---|---|
| Main | Taskflow (project management), Ledgerly (personal finance) | development and judged quality |
| Held-out A | Bookly, Pantry, Stockroom, Streaks | became diagnosis material |
| Held-out B | Fleet, Garden, Gradebook, Helpdesk | evaluation only: never read to tune the engine |

A run opens by showing this brief in full, before any request is sent,
so a viewer knows what is being built.

A change counts as a gain only when it holds on apps that the engine was
not tuned on. That is what the held-out sets are for.

## The two sides

```text
                       one objective, one model, one streaming client
                                          |
              +---------------------------+---------------------------+
              |                                                       |
       SINGLE STREAM                                              SLIMTOKEN
   one request writes                              1. plan: file layout and interface contracts
   every file in turn                              2. write: types, then modules, then pages,
              |                                       components and tests, each stage in parallel
              |                                    3. check: type check, tests, real build, pages
              |                                    4. repair the files the check names; check again
              |                                                       |
          clock stops                                             clock stops
              |                                                       |
              +---------------------------+---------------------------+
                                          |
                 the same four checks on both apps, after the clocks:
                          install, build, test, pages
```

**Single stream.** One request: the objective and a short instruction on
the output format, so the answer can be split into files. It may use up to
128,000 output tokens. It gets no repair step, because it is one request.

**SlimToken.**

1. **Plan.** The engine decides the files and writes a contract for each:
   its exports, with types. Every shared type has one owner file.
2. **Write in stages.** The type files come first. The other modules come
   second and see the type files. Pages, components and tests come third
   and see the types and the modules. Files in one stage are written at
   the same time, each by its own stream. Files whose contract is settled
   early start before the plan is final.
3. **Check.** A fast check type-checks the files, runs the tests and
   applies rules that the compiler does not see. When it passes, the real
   check runs: `next build`, then the built app is started and every
   static page is requested.
4. **Repair.** Each file that the check names is repaired by its own
   stream, with its problems and the files it imports. The check runs
   again, for at most eight rounds. The best result is kept.

## What is measured

- **Wall time, the headline number.** Each side has its own clock, from
  its first request to its final files. SlimToken's clock includes the
  plan, every stage, and the whole check-and-repair step with each build.
  The time to write files to disk and the four quality checks are outside
  both clocks.
- **Speedup** = single-stream wall time / SlimToken wall time.
- **Output tokens per second**, from the usage that the server reports.
- **Tokens in and out.** SlimToken sends the contracts and earlier files
  to every stream, so it uses far more input tokens than one request. The
  result panel shows both, and how many of the input tokens the provider
  reported as prompt-cache hits.
- **Files and lines** on disk.

## How the result is tested

A fast app that does not work is not a result. Every run is tested at
three levels.

### 1. Delivery checks (every run, both apps, no model)

The same commands run in each app directory after the clocks stop:

| Check | Command | Passes when |
|---|---|---|
| install | `npm install` | exit code 0 |
| build | `npm run build`, type check included | exit code 0 |
| test | `npm test` | exit code 0 |
| pages | start the built app and request each static page | every page answers 200 |

The verdict line shows speed and checks together, for example "only the
SlimToken app builds and passes its tests".

### 2. Blind quality judge (on request: `showdown judge <run>`)

Passing a build does not show that the app does what the brief asked. The
judge scores both apps of a finished run without knowing which is which.
It never runs during a race.

- **Requirement coverage.** A written list of what the brief asks for
  (Taskflow 50 items, Ledgerly 49). A model marks each item full, partial,
  stub or missing, twice, with the list in a different order each time.
- **Working defects.** A model looks for real bugs by class: settings that
  nothing reads, state with two owners, money signs and units, dates,
  dead links.
- **Evidence is verified.** Every claim must quote a line of the app. The
  quote is checked against the file. A bug with no verified quote is
  dropped; a "full" with no verified quote becomes "partial".
- **Pairwise verdict.** The two apps are compared in both orders. A
  verdict that flips with the order is reported as uncertain.
- **Runtime smoke.** Each app is built with one shared toolchain and
  driven in a headless browser: every page must render without an error,
  then real flows run (create a project, create a task, move a card and
  reload, add an expense and see net worth fall by it).
- **Static lints.** Counts of defects found without a model, such as CSS
  classes that no style sheet defines.

The score is `Q = 0.4 * coverage + 0.4 * working + 0.2 * smoke`. The
delivered score applies the delivery checks: an app that does not build
keeps a quarter of its Q, and each failed test costs 2 points (10 at most).

The judge itself was validated against a hand audit of five pairs
(`showdown judge-validate`).

### 3. The benchmark's own tests

- 518 unit and integration tests of the engine and about 160 of the
  benchmark, with `clippy` clean on both.
- Golden files pin the plan that the engine makes from a fixed manifest
  for Next.js, Python and Rust, so a change to planning shows as a diff.
- The streaming client is tested against a local fake server: cut
  answers, continuations and refused requests.

## What keeps the comparison fair

- **Same model, same client.** Both sides send requests through one
  streaming client with the same request shape.
- **Same checks.** The four delivery checks and the judge treat the two
  apps alike. The judge is blind, and its smoke test builds both apps
  with the same toolchain.
- **Repair is inside SlimToken's clock.** Its time is the time to a
  checked app, not the time to the first draft.
- **No hidden help for SlimToken.** It gets the same brief. The check
  toolchain is installed before either clock starts.
- **Race or sequential.** `--order race` starts both at the same moment:
  the best live show, with both sides sharing one API key's limits.
  `--order sequential` runs them one after the other and is the order to
  use for published numbers.
- **Simulated data is marked.** `showdown demo` plays invented data and
  says `SIMULATED DATA` in red. It is never a measurement.

## How changes to the engine are tested

Every pipeline step has a switch. A change is measured as alternating
pairs: the same app, with the step and without it, one after the other,
so both see the same hour on the provider.

- A step joins the recommended set only when it wins on held-out apps.
- Negative results are kept. Of the steps tried, more were rejected than
  adopted: racing copies of every request, forking pages into parts,
  repairing inside the wave, and others.
- Records and raw results of these comparisons are kept for a later write-up.

## Results so far

All with DeepSeek. "Pass" means all four delivery checks pass.

| Date | App | Single stream | SlimToken | Speedup |
|---|---|---|---|---|
| 2026-11-14 | Taskflow | 85.4 s, pass | 26.4 s, pass | 3.24x |
| 2026-11-14 | Taskflow | 82.5 s, pass | 38.6 s, pass | 2.14x |
| 2026-11-14 | Taskflow | 81.4 s, test fails | 28.5 s, pass | 2.86x |
| 2026-11-20 | Taskflow | 92.6 s, build fails | 34.0 s, pass | 2.72x |
| 2026-11-20 | Fleet (held-out) | 108.2 s, build fails | 44.7 s, pass | 2.42x |

Judged quality of the SlimToken app (2026-11-20, delivered score out of
100): Taskflow 95.2 and 79.7, Ledgerly 92.5. In the first of these the
judge also scored the pinned single-stream reference app (written on
2026-11-15): 87.9, against 95.2 for SlimToken.

## Limits: read before you quote a number

- **One run is one sample.** Provider speed changes from minute to minute
  and the model never writes the same app twice. Quote several runs and
  show all of them.
- **One provider, one stack.** The numbers are for DeepSeek and Next.js
  apps. Other providers and stacks are supported by the engine but are
  not measured here.
- **Cost.** SlimToken is faster, not cheaper: it sends many more input
  tokens (hundreds of thousands against a few hundred). The provider
  served about 30% of them from its prompt cache in the two races of
  2026-11-20 (203,392 of 657,026 and 198,656 of 665,191).
- **Judged quality covers two apps.** The requirement lists exist for
  Taskflow and Ledgerly, and a person has not yet reviewed them. The
  held-out apps have delivery checks only.
- **The single stream is one request by design.** A single stream with
  its own repair loop would pass more checks and take longer. That is a
  different comparison, and this benchmark does not make it.
- **A race shares rate limits.** For published numbers use
  `--order sequential`.
