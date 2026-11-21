# Embedding SlimToken

SlimToken is a Rust library. A host program embeds the library and gets a
parallel DAG engine for LLM work. This guide shows the smallest integration
and the main knobs. It is for engineers who build an agent system or a coding
harness on top of the engine.

## What the engine gives a host

- A plan step: the architect turns one objective into a task graph.
- A wave step: the engine runs many tasks at the same time. Each task is
  one model request.
- A control loop: an AIMD controller sets the number of in-flight tasks.
  It reads failure events from the run.
- A repair step: a failed task gets one in-run repair with the error as
  context.
- A report: per-task tokens, duration, attempts, cache hits, and a trace
  file with wave events.

## The seam

All provider logic sits behind one trait: `ModelClient`. The engine does not
know about any provider. The host implements this trait, or it wraps the
shipped OpenAI-compatible client. The host passes `Arc<dyn ModelClient>` to
`RunManager::new`. This is the only integration point.

```rust,no_run
use std::sync::Arc;
use slimtoken::{RunManager, SchedulerConfig};

let manager = RunManager::new(
    Arc::new(MyProvider),
    SchedulerConfig::default(),
    "slimtoken-runs".into(),
    false,
);
```

A host that wants multi-provider workers without changing the engine builds
one `FanOutClient` over labeled `FanOutBackend`s and passes it the same way.
Requests route by a stable hash of the prompt (retries stay sticky), and the
serving backend's label lands on each completion for per-provider reporting.
`with_health_routing` (opt-in) adds a per-backend circuit breaker: a backend
whose error EMA crosses the threshold sheds its hashed share to a healthy
peer, with half-open recovery probes so a healed backend returns on its own.

## Minimal host

A run needs one objective or one ready `Plan`:

```rust,no_run
let summary = manager
    .submit(slimtoken::RunRequest {
        objective: Some("Summarize the event log crate".into()),
        ..Default::default()
    })
    .await?;
```

`RunRequest` also carries steering directives, a task cap, and per-run
scheduler overrides. See `API.md` for the full request shape and the REST
routes.

### One call from an objective to files on disk

`pipeline::build` runs the steps that the CLI and `RunManager` run, in
order: plan (`architect::plan_project_with`, or a plan you made:
`PlanSource::Given`), the wave, write the project files
(`BuildOptions::out_dir`), and, with `BuildOptions::checker`, check and
repair them (`repair::check_and_repair_indexed`; the kept files are
written again).

```rust,no_run
use std::sync::Arc;
use slimtoken::pipeline::{build, BuildOptions};
use slimtoken::profile::NEXTJS;
use slimtoken::provider::DEEPSEEK_PRESET;

async fn host(
    client: Arc<dyn slimtoken::ModelClient>,
    checker: &dyn slimtoken::check::Checker,
) -> anyhow::Result<()> {
    let options = BuildOptions {
        out_dir: Some("app".into()),
        checker: Some(checker),
        ..BuildOptions::for_stack(&NEXTJS, Some(&DEEPSEEK_PRESET))
    };
    let outcome = build("Build a task tracker in Next.js", client, options).await?;
    println!("{} files written", outcome.files.len());
    Ok(())
}
```

`BuildOptions::default()` plans and runs as the library always did.
`BuildOptions::for_stack(profile, preset)` adds the preset's knobs and,
only for a profile whose staging is measured (`staging.measured`), the
staged wave, one owner for each type, the manifest passes, the test-shard
floor, and `Sink::Assemble` when that plan is staged (the staging needs
the DeepSeek preset; without it the sink stays `Sink::Model`). No
built-in profile is measured yet. The sink is advice for the host:
`build` makes no final document, so join the files yourself for
`Sink::Assemble`, or append a sink task in `BuildHooks::planned` for
`Sink::Model`.
`BuildHooks` (`planned`, `executed`, `finished`, plus a `checker` or
`index` made after planning) run your code between the steps: the CLI
saves `plan.json` and appends its sink there, `RunManager` records the
plan and stops a cancelled or plan-only run. A cancelled
`BuildOptions::control` stops planning with `pipeline::Cancelled`.

## Embed from any language (the plugin path)

A host that is not a Rust program embeds the engine over its REST API.
Start the server once:

```bash
slimtoken serve            # see API.md for bind and provider flags
                           # (build with --features serve; the default
                           #  build is the bare engine, no server deps)
```

The host then treats the engine as one service with a stable contract:

1. `POST /v1/runs` with an objective (or a ready plan). The response is a
   `RunSummary` with the run id.
2. `GET /v1/runs/{id}/tasks` to poll task progress, or
   `GET /v1/runs/{id}` for summary plus steering state.
3. `POST /v1/runs/{id}/steer` to send directives to tasks that have not
   started yet.
4. `GET /v1/runs/{id}/report` for the final per-task record: tokens,
   attempts, cache hits, durations. `GET /v1/runs/{id}/plan` for the
   generated graph.

This is the whole integration. A coding harness in any language — Python,
TypeScript, Go — becomes an engine host with an HTTP client. The shipped
CLI (`slimtoken run`, `slimtoken serve`) and the library share the same
run manager, so local CLI runs and hosted runs behave the same.

### Stability contract at the process boundary

- JSON response shapes grow by ADDED fields. A host must ignore unknown
  fields; the engine does not rename or remove fields within a major
  version.
- The trace vocabulary (see Telemetry) is a matchable contract: event
  names and existing field names are stable; new events and fields may
  appear.
- Route paths and method names in API.md are stable within a major
  version.
- Exit codes and `--json` output shapes of the CLI follow the same rule.

### Capabilities are bolt-ons, not core

The engine core stays generic (see Rules for hosts). A capability —
QA gates, agent personas, review passes — is a separate artifact the
host orchestrates around the run, never a change inside the engine:

- In-process (Rust host): wrap the `ModelClient` seam or set opt-in
  flags on `SchedulerConfig`.
- Out-of-process (any host): run the capability as a sidecar step in
  the host's own loop. Consume run artifacts (report, trace, task
  outputs on disk), apply the capability's gate, then steer or cancel
  through the REST routes.

The browser-QA harness (packaged as the `slimtoken-qa` npm sidecar in
`sidecars/qa/`) is the reference bolt-on: a zero-dependency sidecar gate
that builds, serves, and audits the UI the engine produced. Any host can
run the same gate on its own loop's artifacts. Nothing in the engine
knows the harness exists.

The recommended host loop for a coding harness:

```
plan -> run wave -> sidecar gates on artifacts -> judge -> steer next wave
```

The engine owns the middle of that loop (plan, wave, repair, report).
The host owns the gates and the judge. That split is the product: the
engine is the fast parallel execution core; capability and policy live
in the host, where they can change without a new engine build.

## Error taxonomy

`ModelClient::complete` returns `anyhow::Result<Completion>`. The engine
recognizes two typed errors inside it:

| Error | Meaning | Engine behavior |
|---|---|---|
| `RateLimitedError` | HTTP 429 from the provider | Wait out `retry_after`; with `rate_auto_tune`, decay the pace and shield the controller |
| `BudgetExhaustedError` | `max_tokens` hit with no visible output | Grow the budget on retry |
| Any other error | Generic failure | Retry with backoff, then repair |

A host that maps its provider's 429 to `RateLimitedError` gets correct
rate behavior without extra code.

Host-side validation errors come from `RunRequest::validate` and
`SchedulerConfig::validate` before any model call. Terminal run failures
surface in `RunSummary::error`.

## Knobs that matter

`SchedulerConfig` is the tuning surface. Library defaults are deterministic;
the CLI opts into the aggressive settings. The knobs a host usually sets:

- `max_concurrency` — hard ceiling on in-flight tasks.
- `adaptive` + `controller` — AIMD width control. On in the CLI, off in the
  library.
- `max_tokens_per_minute` / `max_starts_per_minute` — admission pacing for
  providers with token or request quotas.
- `rate_auto_tune` — pace decay on typed 429s. Off by default.
- `reasoning_effort` — effort hint forwarded to workers.
- `shared_context_extra` + `prompt_cache_key` — byte-identical prefix content
  and the cache routing key for cross-run prefix-cache reuse.
- `validator` — an artifact validator; rejections become repairs.
- `trace_path` — a JSONL trace of wave events for host observability.

`PlanOptions` shapes an architect plan (`architect::plan_from_manifest`,
`plan_project_with`, `pipeline::BuildOptions::plan`). Its quality options,
all off by default and not measured yet, use the profile's
`staging.quality` texts (`profile::Quality`; NEXTJS has them, PYTHON and
RUST use `NO_QUALITY`): `quality_rules` (fixed rules for the theme, the
store, ids and settings at the end of the module head, and the dark-theme
selector rewrite), `relative_dates` (seed dates computed at load, in place
of `today`), `objective_pages` (each page gets its objective bullet, the
quality bar and the route list; the head lists purposes) and
`align_literals` (`architect::align_union_literals`, values only: compared,
`case`, union members, lists of literals). The `objective` module has the
text rules they use (`page_bullets`, `quality_bar_for_one_file`,
`match_pages`, ...).

The app contract (also off by default and not measured yet):
`ArchitectOptions::contract` asks the planner for the profile's contract
(`staging.quality.contract_rule`: shared unions with labels, catalogs with
ids, the storage key, one whole-state type, the settings with defaults,
meanings and readers, one owner per collection and UI control), checks the
merged manifest with `contract::contract_problems` and asks each fragment
that owns a problem once more (a repaired answer is kept only when no file
loses exports and the check finds no more problems). On the plan side, `PlanOptions::contract`
gives each settings reader its fields, `wiring_rules` extends the wiring
prompt (`architect::wiring_prompt_with`), `consumers_see_css` makes pages
and components wait for the CSS parts, and `deps_first` sets
`Task::prefix_split` so `scheduler::render_task_prompt` puts the
prerequisite outputs before each task's own tail (a longer shared prefix
for prompt caching). Breaking: a host that builds `Task` values with struct
literals adds `prefix_split: None` (or uses `..Default::default()`).

Pages own their features (also off by default and not measured yet):
`ArchitectOptions::page_local` adds the profile's `page_local_rule` to the
planner rules, and `PlanOptions::page_local` runs
`pagelocal::fold_page_components` as the last manifest pass. A component
that exactly one page renders (or that no file names and whose name, id
prop or purpose points to one page) leaves the manifest and goes into that
page's purpose as local components (one worker writes the page and its
private parts; with the option every page's task gets `max_tokens` 8192, and
so does a page with the fold's note when a folded manifest is replayed with
it off); components of a layout or of two or more pages, state owners and
components that a lib module or a test needs stay, and each one left
(holding no state, props not only ids) gets the profile's
`shared_component_rule` (presentational: props in, callbacks out). With
`objective_pages`, `pagelocal::page_lines` adds each page's shared
components, the links that the routes imply and its create controls.
`profile::Quality` has four new texts for it (`page_local_rule`,
`local_note`, `shared_component_rule`, `layout_glob`; empty in
`NO_QUALITY`).

App conventions (also off by default and not measured yet; NEXTJS only):
`ArchitectOptions::semantics` adds the profile's `semantics_rule` to the
planner rules (the comments of `lib/types.ts` state once the money unit and
sign, each date field's format, each status union's transition function and
the timestamps it sets, the one state owner and the shell the layout
renders; with `page_local` the page-local rule becomes
`semantic_page_local_rule`, where only the store provider owns the state).
The types fragment then sees every file (`semantics_types_note`, cap
`semantics_cap`, +800 characters), and `semantics::semantic_problems`
checks the merged manifest; its problems share the one repair round with
`contract`. `PlanOptions::semantics` ends every module head with
`semantics::conventions(manifest, profile)` (one block, the same for every
worker, ending with `conventions_rule`) and gives each component that a
layout renders `layout_component_rule` (`pagelocal::layout_component_lines`),
or `layout_component_bare_rule` when the plan has no store provider.
`profile::Quality` has seven new texts for it (`semantics_rule`,
`semantic_page_local_rule`, `semantics_types_note`, `semantics_cap`,
`conventions_rule`, `layout_component_rule`, `layout_component_bare_rule`;
empty in `NO_QUALITY`), and
`contract::{types_files, declared_fields, names, texts, stem}` are shared
with the new module.

Every new capability is opt-in. Defaults keep legacy behavior bit-for-bit.

## Telemetry

`RunReport` holds per-task records: status, attempts, input and output
tokens, cached tokens, time to first token, duration. Aggregate rates are
computed from the records. `report.phases` attributes wall time per phase;
`planning_input_tokens` / `planning_output_tokens` carry the
planning-phase bill when the caller counts it (wrap the planning client in
`UsageCountingClient`). Wave totals in `total_input_tokens` /
`total_output_tokens` cover wave tasks only — overhead ratios must add the
planning fields.

### Trace vocabulary

With `trace_path` set, the engine writes one JSON object per line:
`{"event": <name>, "elapsed_ms": <ms since wave start>, "fields": {...}}`.
Event names and fields are a stable contract — consumers may match on them;
new fields may appear, existing ones are not renamed or removed within a
major version.

| Event | When | Fields |
|---|---|---|
| `plan_start` | First line of each run, only with `SchedulerConfig.trace_append` (the file is then appended to, not truncated) | `run` (1 + the `plan_start` lines already in the file), `unix_ms` (wall-clock time) |
| `planning_complete` | First line (after `plan_start`), when the caller set `SchedulerConfig.planning` | `planning_ms`, `calls`, `input_tokens`, `output_tokens`, `cached_input_tokens` |
| `run_start` | Wave admission opens | `tasks`, `primes` |
| `concurrency_probe` | Auto-probe finished (opt-in) | `chosen_width`, `steps` |
| `priming_complete` | Cache primes processed (only when the plan has primes) | `primes`, `priming_ms`, `warm_gate` |
| `task_start` | A task is admitted | `task_id`, `depth`, `queue_weight`, `max_tokens`, `est_input_tokens` (last field only when TPM pacing reserves) |
| `task_retry` | A failed attempt inside a task, before the final `task_complete` | `task_id`, `attempt`, `error`, `delay_ms`, `output_tokens` (the failed attempt's reported burn; `null` for transport errors, timeouts, 429s) |
| `task_complete` | A task finishes | always `task_id`, `status`; succeeded adds `attempts`, `duration_ms`, `input_tokens`, `output_tokens`, `cached_input_tokens`, `ttft_ms`; failed adds `attempts`, `duration_ms`, `error`, `class`; skipped adds `error` only |
| `repair_queued` | A failure queued behind `max_concurrent_repairs` | `task_id`, `attempts`, `error`, `class`, `queue_depth` |
| `repair_start` | An in-run repair starts | `task_id`, `attempts`, `error`, `class` |
| `rate_tune` | The rate auto-tuner changed the pace (opt-in) | `task_id`, `multiplier`, `observations` |
| `run_end` | The wave drained | `succeeded`, `failed`, `skipped`, `peak_concurrency`, `output_tokens` |

Ordering invariants a consumer can rely on:

- `planning_complete` (when present) precedes `run_start`; `run_end` is last.
  With `trace_append`, these hold within each run: a run starts at its
  `plan_start` line and ends at the next one (or the end of the file), and
  its `elapsed_ms` counts from its own wave start (`unix_ms` lines runs
  up).
- A task's `task_start` precedes its retry events, which precede its
  `task_complete`. `elapsed_ms` on `task_complete` minus the matching
  `task_start` is queue + execution time.
- The first `task_complete` for a task after its `repair_start` is that
  repair's completion; its `attempts` count includes the failed attempts
  that triggered the repair.
- Skipped tasks appear only as `task_complete` with `status: "skipped"`
  (emitted transitively after an ancestor failure); there is no separate
  skip event.

`class` is the failure class: `rate_limited`, `transport`, `content` or
`permanent`. Only a `content` failure puts the error into the repair
prompt. A `permanent` failure has no repair.

Known limits: `task_retry` input tokens are not reported (providers do not
bill usage on failed calls); planning tokens ride `planning_complete`, not
the wave totals; reconcile-phase calls are not separately attributed.

## Rules for hosts

- Keep the core generic. Put stack rules in a profile, not in the engine.
- New capabilities are opt-in wrappers at the `ModelClient` seam or flags on
  `SchedulerConfig`.
- The engine is a benchmark when it builds apps. App results do not change
  the core.
