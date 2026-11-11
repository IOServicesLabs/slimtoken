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
