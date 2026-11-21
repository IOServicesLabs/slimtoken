# SlimToken REST API

The optional server exposes the same `RunManager` used by the interactive CLI. Start it with `slimtoken serve`; the default listener is `127.0.0.1:3000`.

## Submit a run

`POST /v1/runs`

Returns `202 Accepted` with a `RunSummary`. Planning and execution continue in the background.

Provide exactly one of `objective` or `plan`:

```json
{
  "objective": "Review the scheduler and propose focused fixes",
  "steering": [
    "Prefer actionable findings",
    "Keep the final response concise"
  ],
  "max_tasks": 32,
  "plan_only": false,
  "scheduler": {
    "max_concurrency": 64,
    "max_retries": 2,
    "retry_base_delay_ms": 250,
    "task_timeout_secs": 300,
    "max_dependency_chars": 12000,
    "temperature": 0.2,
    "adaptive": true,
    "reasoning_effort": "low",
    "shared_context_extra": "Project conventions document text...",
    "prompt_cache_key": "my-team-key"
  }
}
```

| Field | Required | Meaning |
|---|---|---|
| `objective` | One of objective/plan | Natural-language objective to decompose |
| `plan` | One of objective/plan | Complete `Plan` JSON to execute directly |
| `steering` | No | Initial user directives for planning and workers |
| `max_tasks` | No | Planner task limit, 1–256; defaults to 48 |
| `plan_only` | No | Save the plan but skip execution |
| `scheduler` | No | Per-run overrides of server scheduler defaults |
| `architect` | No | Greenfield mode: an architect manifest, then one wave of file workers |
| `profile` | No | Architect mode: stack profile name (`python`, `nextjs`, `rust`, or a loaded file profile); detected from the objective when absent |
| `plan_options` | No | Architect mode: `PlanOptions` fields (missing ones take their defaults) |

With `profile` or `plan_options` set (even `plan_options: {}`), an architect run plans through `architect::plan_project` (inside `pipeline::build`, which every run goes through), which also applies the profile's manifest fixups as the CLI does. Without them it plans as before. Both are rejected unless `architect` is true; `plan_options.quota_pct` must be at most 1000 and `plan_options.min_test_shards` at most 256.

`plan_options.today` (default `null`) is a date `[year, month, day]` (year 1-9999, month 1-12, day 1-31). When set, the plan asks the profile's Data-layer files (NEXTJS: `seed.ts` and `seed.*.ts`) for records dated around that day. The date goes only into the plan's copy of the manifest, not into a saved one. A plan with a date changes from day to day, so the host should set it only when it wants dated seed data. `plan_options.tidy`, `one_type_owner`, `split` and `staged` turn on the other measured manifest passes and staging; see the CHANGELOG. `plan_options.declare_types` (with `tidy`) adds a declaration for each type that a planned signature names and no file declares; it is not measured yet.

Four quality options (default `false`, not measured yet) use the profile's quality texts; PYTHON and RUST have none, so there they change nothing. `plan_options.quality_rules` ends every module head with the profile's fixed rules (NEXTJS: the theme only as `document.documentElement.dataset.theme` with the dark values on `:root[data-theme="dark"]`, one store (the plan's store hook or provider, else the storage module's load and save from effects and event handlers), ids from `crypto.randomUUID()`, every field of the settings type read where it applies) and rewrites the planned `.theme-dark` selector. `plan_options.relative_dates` asks the Data-layer files for dates computed when the data loads, relative to the current date, in place of `today`'s fixed day (`today` is then ignored). `plan_options.objective_pages` gives each route file its bullet of the objective's `Pages:` list verbatim, the objective's quality bar as one file's worker reads it (`objective::quality_bar_for_one_file`) and the planned routes, gives each component a scope rule, and lists each file's purpose in the module head. `plan_options.align_literals` rewrites a quoted literal in the plan that equals a member of a shared string union apart from case, spaces and separators to the union's spelling, only where it is a value (compared, a `case`, a union member, an item of a list of literals); mapped text such as `'done' -> 'Done'` and prose stay.

Four more plan options (default `false`, not measured yet) wire the app contract into the plan; with the profile's texts empty (PYTHON, RUST) the first two change nothing. `plan_options.contract` gives each file that the settings comment of the types file names as a reader (`field: meaning -> path`) a line with those settings. `plan_options.wiring_rules` (with `wave_wiring`) adds the profile's wiring rule (every create/add/update/remove export has a caller; pages list the components they render) and each setting's readers to the in-wave wiring prompt, and only staged files wait for the wiring task. `plan_options.consumers_see_css` (with `staged` and `split`) makes pages, layouts and components also wait for the CSS parts so they see the real selectors. `plan_options.deps_first` puts the prerequisite outputs between each task's shared head and its own tail, so sibling prompts share a longer prefix for prompt caching. The planner side (`ArchitectOptions::contract`) is a library option only; a request cannot set it.

`plan_options.semantics` (default `false`, not measured yet; NEXTJS only, PYTHON and RUST change nothing) ends every module head with one conventions block built from the plan (`semantics::conventions`): money fields with their unit and sign (default "integer cents, always > 0", the record's kind says expense or income), date fields with their format (`*At` a full ISO timestamp, the rest 'YYYY-MM-DD'), the dates module and its local `today()`, each status union with the one function that changes it and the timestamps it sets, the one state owner (store provider and hook), the shell the root layout renders, then fixed rules (validators, forms and seeds follow these conventions; no literal currency code). Each component that a layout renders also gets the layout-component rule (it keeps no copy of the state). The prompts grow by about 1.3-2k characters each; the block is part of the shared head. The architect option of the same name (`ArchitectOptions::semantics`, embedding only) asks the types file to state these conventions and checks the merged manifest (`semantics::semantic_problems`, at most one repair round, shared with `contract`).

`plan_options.page_local` (default `false`, not measured yet; NEXTJS only, PYTHON and RUST change nothing) lets each page own its features: each component that exactly one page renders (the page names it in the plan, `renders <X/>`, or no file names it and its name, id prop or purpose points to one page) is written in that page's file as a local component instead of a file of its own (every page gets `max_tokens` 8192), and each component left that no layout renders, that holds no state and whose props are not only ids is told to take its data through props and report changes through callbacks, never owning the app state. With `objective_pages`, each page also gets the shared components it renders, the links between pages that the routes imply and a control for each create/add lib export it owns. Like `contract`, the planner rule (`ArchitectOptions::page_local`) is a library option only.

All scheduler override fields are optional. `max_tasks` is relevant only when the server generates the plan. Scheduler override keys beyond the base set:

| Key | Meaning |
|---|---|
| `adaptive` | Drive the in-flight cap with the AIMD controller |
| `reasoning_effort` | `low`, `medium`, or `high`; forwarded to workers |
| `shared_context_extra` | Cross-session context spliced into the byte-identical worker prefix |
| `prompt_cache_key` | Prefix-cache routing key forwarded to the server |

## Routes

| Method and path | Purpose | Success response |
|---|---|---|
| `GET /health` | Health check | `200 {"status":"ok","service":"slimtoken"}` |
| `POST /v1/runs` | Submit an objective or plan | `202 RunSummary` |
| `GET /v1/runs` | List runs, newest first | `200 [RunSummary]` |
| `GET /v1/runs/{id}` | Live summary, steering, and per-task state | `200 RunDetails` |
| `GET /v1/runs/{id}/tasks` | Retrieve only the live per-task state map | `200 map<string, TaskProgress>` |
| `POST /v1/runs/{id}/steer` | Add a steering directive | `202 RunDetails` |
| `DELETE /v1/runs/{id}` | Request cancellation | `202 RunDetails` |
| `POST /v1/runs/{id}/cancel` | Alternative cancellation route | `202 RunDetails` |
| `GET /v1/runs/{id}/plan` | Retrieve the generated/supplied plan | `200 Plan` |
| `GET /v1/runs/{id}/report` | Retrieve the execution report | `200 RunReport` |

Use the `/tasks` route when polling only task progress; use the full run route when summary counts and steering are also needed.

## Steering

```bash
curl -sS -X POST "http://127.0.0.1:3000/v1/runs/$RUN_ID/steer" \
  -H 'content-type: application/json' \
  -d '{"instruction":"Prioritize verification and concise output"}'
```

A run accepts at most 32 directives, each up to 8,000 characters. Empty directives and steering terminal runs are rejected with `400`.

- Initial steering affects planning and worker prompts.
- Steering during planning affects workers, not the planner request already in flight.
- Steering during execution affects only tasks that have not started.
- Already-running or completed tasks are not retroactively changed.

## Status and results

Run statuses are:

```text
queued, planning, running, cancelling,
plan_ready, succeeded, failed, cancelled
```

Task progress statuses are:

```text
pending, running, succeeded, failed, skipped, cancelled
```

`GET /v1/runs/{id}` returns a structure like:

```json
{
  "summary": {
    "id": "run-...",
    "objective": "Review the scheduler",
    "status": "running",
    "plan_only": false,
    "task_count": 10,
    "pending": 5,
    "running": 1,
    "succeeded": 4,
    "failed": 0,
    "skipped": 0,
    "cancelled": 0,
    "steering_count": 2,
    "has_plan": true,
    "has_report": false,
    "output_dir": "slimtoken-runs/run-..."
  },
  "steering": ["Prefer actionable findings"],
  "tasks": {
    "requirements": {
      "id": "requirements",
      "title": "Requirements",
      "status": "succeeded",
      "attempts": 1,
      "updated_at_ms": 1790000000000
    }
  }
}
```

The summary also includes creation/update timestamps and an optional error.

`GET /v1/runs/{id}/report` returns `409 Conflict` until a report exists. Cancellation during planning can finish without a report. Cancellation during execution produces a report containing `cancelled` task records.

## Errors and operational boundaries

Errors use this shape:

```json
{"error": "human-readable message"}
```

- `400` — invalid request, invalid steering, or steering a terminal run.
- `404` — unknown run ID.
- `409` — plan or report is not ready.
- `500` — artifact persistence or internal submission failure.

Live run state is in memory. Per-run `plan.json` and `report.json` files persist under the server `--output` directory, but old runs are not reloaded after restart.

The server currently has no built-in REST authentication. Keep it on loopback or place it behind an authenticated reverse proxy before exposing it to other machines. The model server's API key and REST client authentication are separate concerns.
