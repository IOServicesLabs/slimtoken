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
| `planning_complete` | First line, when the caller set `SchedulerConfig.planning` | `planning_ms`, `calls`, `input_tokens`, `output_tokens`, `cached_input_tokens` |
| `run_start` | Wave admission opens | `tasks`, `primes` |
| `concurrency_probe` | Auto-probe finished (opt-in) | `chosen_width`, `steps` |
| `priming_complete` | Cache primes processed (only when the plan has primes) | `primes`, `priming_ms`, `warm_gate` |
| `task_start` | A task is admitted | `task_id`, `depth`, `queue_weight`, `max_tokens`, `est_input_tokens` (last field only when TPM pacing reserves) |
| `task_retry` | A failed attempt inside a task, before the final `task_complete` | `task_id`, `attempt`, `error`, `delay_ms`, `output_tokens` (the failed attempt's reported burn; `null` for transport errors, timeouts, 429s) |
| `task_complete` | A task finishes | always `task_id`, `status`; succeeded adds `attempts`, `duration_ms`, `input_tokens`, `output_tokens`, `cached_input_tokens`, `ttft_ms`; failed adds `attempts`, `duration_ms`, `error`; skipped adds `error` only |
| `repair_queued` | A failure queued behind `max_concurrent_repairs` | `task_id`, `attempts`, `error`, `queue_depth` |
| `repair_start` | An in-run repair starts | `task_id`, `attempts`, `error` |
| `rate_tune` | The rate auto-tuner changed the pace (opt-in) | `task_id`, `multiplier`, `observations` |
| `run_end` | The wave drained | `succeeded`, `failed`, `skipped`, `peak_concurrency`, `output_tokens` |

Ordering invariants a consumer can rely on:

- `planning_complete` (when present) precedes `run_start`; `run_end` is last.
- A task's `task_start` precedes its retry events, which precede its
  `task_complete`. `elapsed_ms` on `task_complete` minus the matching
  `task_start` is queue + execution time.
- The first `task_complete` for a task after its `repair_start` is that
  repair's completion; its `attempts` count includes the failed attempts
  that triggered the repair.
- Skipped tasks appear only as `task_complete` with `status: "skipped"`
  (emitted transitively after an ancestor failure); there is no separate
  skip event.

Known limits: `task_retry` input tokens are not reported (providers do not
bill usage on failed calls); planning tokens ride `planning_complete`, not
the wave totals; reconcile-phase calls are not separately attributed.

## Rules for hosts

- Keep the core generic. Put stack rules in a profile, not in the engine.
- New capabilities are opt-in wrappers at the `ModelClient` seam or flags on
  `SchedulerConfig`.
- The engine is a benchmark when it builds apps. App results do not change
  the core.
