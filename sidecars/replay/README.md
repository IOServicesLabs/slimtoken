# slimtoken-replay

Replay-verification sidecar for the SlimToken engine. It makes the
verification A/B routine: run one `plan.json` against two engine builds
(the host does the runs), then diff the two run output dirs per task.
The pack never re-runs anything and never calls a model. It reads
`plan.json`, `report.json`, and `trace.jsonl` from both run dirs and
reports quality and wall regressions. Cost: zero tokens.

## What is in the pack

| Piece | Role |
|---|---|
| `diff` (`lib/diff.mjs`) | Pairs tasks by id across the two run dirs and applies the finding rules. |
| `readers` (`lib/readers.mjs`) | Reads `plan.json`, `report.json`, and streams `trace.jsonl` line by line. Tolerates the real engine shapes; missing fields become `null`. |

## Install

```
npm install slimtoken-replay
```

Or run it straight from a checkout:

```
node sidecars/replay/bin/slimtoken-replay.mjs help
```

## Use

The host replays the same plan against two binaries into two run output
dirs (each holding `plan.json`, `report.json`, `trace.jsonl` from the
engine), then diffs them:

```
slimtoken run --plan plan.json --out run-a/
slimtoken run --plan plan.json --out run-b/
slimtoken-replay diff run-a/ run-b/ --out verdict/
```

Output: one `OK <id>` or `FINDING <id>: <what>` line per task, then a
summary. With `--out DIR` the pack also writes `report.md` (a paired
table a judge reads in one glance) and `replay.json` (the machine
twin: `{ok, arms, tasks: [{id, status_a, status_b, findings}], summary}`;
field names are stable, new fields append only).

Finding rules (each stated in one line):

- `missing-in-a` / `missing-in-b`: the task id exists in only one run.
- `status regressed`: status moved down the ordering succeeded > failed > skipped.
- `attempts +N`: attempt count grew by more than 1.
- `duration +N%`: duration grew by more than 25% (the wall-regression tripwire).
- `output tokens +N%`: output tokens grew by more than the tolerance (default 10%, set with `--tokens-tolerance N`).
- `retry storm`: the task's `task_retry` event count in `trace.jsonl` grew by 3 or more (an instability signal the report alone hides).

Exit codes: 0 pass (zero findings), 1 findings, 2 usage error, missing
run dirs, or unreadable reports (fail closed).

## Host wiring (SlimToken engine)

The engine owns the wave; the pack owns the gate; the host owns the
judge (see `EMBEDDING.md` at the repo root and `sidecars/README.md`):

1. The host runs the same plan against arm A and arm B (two engine
   builds), each into its own run output dir.
2. The host runs `slimtoken-replay diff run-a/ run-b/ --out verdict/`.
3. The ledger pack (`slimtoken-ledger`) appends the verdict row (arm,
   wall, tokens, gate results) to the run ledger.
4. The judge reads `verdict/report.md` (or the `replay.json` twin) and
   decides whether the change earned its keep.

## Development

Copy the shape from `sidecars/_template/`. Keep zero runtime
dependencies and keep the gate zero-token (pure file parsing). Run the
selftest after any change:

```
node test/selftest.mjs
```

The fixtures under `fixtures/` are fabricated run dirs: `run-a/` and
`run-b-good/` diff green; `run-b-bad/` carries one status flip, one
retry storm, one token bump, and one task id that exists only in arm B.
