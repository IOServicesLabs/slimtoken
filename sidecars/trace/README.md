# slimtoken-trace

Trace analysis sidecar for the SlimToken engine. This pack turns the
engine's own `trace.jsonl` into a `SchedulerConfig` tuning brief. It
dogfoods the engine's trace contract: the event names and fields it
parses are the stable vocabulary in `EMBEDDING.md` ("Trace
vocabulary"). Zero runtime dependencies. Node 22 or higher. Zero model
tokens — the pack only reads and counts.

## What is in the pack

| Piece | Role |
|---|---|
| `analyze` (`lib/analyze.mjs`) | Streams a trace line by line, tracks the scheduler signals, and reports anomalies. |
| `lib/report.mjs` | Builds the judge material: `report.md` with the tuning brief plus a machine-readable `trace.json` twin. |

## Install

```
npm install slimtoken-trace
```

Or run it straight from a checkout:

```
node sidecars/trace/bin/slimtoken-trace.mjs help
```

## Use

```
slimtoken-trace analyze run/trace.jsonl
slimtoken-trace analyze run/trace.jsonl --retry-storm 5 --out trace-out
```

Exit codes: 0 pass (no findings), 1 findings, 2 usage error (bad
args, missing file). Corrupt lines are skipped with a WARN and a count
in the stats; they never crash the run. `--out DIR` writes
`report.md` and `trace.json` (`{ok, findings: [{rule, detail,
evidence}], stats, summary}`; stable fields, append-only).

## The six finding rules

- `retry-storm <task_id>: N retries` — a task retried N times, N >= K
  (`--retry-storm`, default 3).
- `repair-queue-depth <n>` — a `repair_queued` event with
  `queue_depth` >= 3.
- `rate-churn` — 5 or more `rate_tune` events in one run.
- `low-cache-hit` — run total `cached_input_tokens / input_tokens`
  below 0.2 (only when input tokens > 0 and cached fields exist).
- `concurrency-collapse` — `run_end peak_concurrency` <= 2 with at
  least 10 tasks and no `concurrency_probe` event in the trace.
- `failures-present` — `run_end failed` > 0.

## The tuning brief

Each finding names one `SchedulerConfig` knob (or prompt surface) the
host should look at first:

| Finding | Knob |
|---|---|
| `retry-storm` | `reasoning_effort` or the task prompt |
| `repair-queue-depth` | `max_concurrent_repairs` |
| `rate-churn` | `rate_auto_tune` |
| `low-cache-hit` | `shared_context_extra` + `prompt_cache_key` |
| `concurrency-collapse` | the probe / auto width settings |
| `failures-present` | the failed tasks' prompts or inputs |

The brief is judge material, not doctrine. The knobs interact (width
feeds retry pressure, cache layout feeds width), so the host measures
with an A/B run before trusting any single line.

## Host wiring (SlimToken engine)

The engine owns the wave; the pack owns the gate; the host owns the
judge (see `EMBEDDING.md` at the repo root and `sidecars/README.md`).
Typical wiring:

1. The host runs the engine with `trace_path` set, so the run writes
   `trace.jsonl` in the run output dir.
2. The host runs `slimtoken-trace analyze run/trace.jsonl --out
   trace-out` (zero tokens).
3. The host (or its judge) reads `trace-out/report.md`, treats the
   tuning brief as steer material, and adjusts `SchedulerConfig` for
   the next wave.

## Development

The canonical home of this pack is `sidecars/trace/` in the slimtoken
repo. Keep zero runtime dependencies and keep the analyzer zero-token
(it parses; it never calls a model). Run `node test/selftest.mjs` in
this directory after any change; it spawns the dispatcher against the
fixtures in `fixtures/` and asserts exit codes, the `--out` report
shape, and corrupt-line tolerance.
