# slimtoken-ledger

	Ledger sidecar for the SlimToken engine. The engine runs a wave and
writes a run dir (`report.json`, pack JSON twins). This pack turns run
dirs into evidence: one JSONL row per run, then paired-arm A/B deltas
with a regression exit code. It operationalizes the A/B rule — no
claim without a pair. It is small and unglamorous; it is also the
difference between discipline and habit.

	The pack costs zero model tokens. It reads JSON and writes JSONL.
It never calls a model and never shells out to a network tool.

## What is in the pack

| Piece | Role |
|---|---|
| `add` (`lib/extract.mjs`, `lib/store.mjs`) | Read one engine run dir and append one JSONL row to the ledger. |
| `compare` (`lib/compare.mjs`, `lib/render.mjs`) | Pair two arms, print side-by-side deltas, exit 1 on any regression. |
| `show` (`lib/render.mjs`) | Print the ledger (or one arm) as a table. |
| `fixtures/` | Two fabricated run dirs (green, regressed) plus one dir with no `report.json`. |
| `test/selftest.mjs` | Zero-dependency selftest against a temp ledger. |

## Install

```
npm install slimtoken-ledger
```

Or run it straight from a checkout:

```
node sidecars/ledger/bin/slimtoken-ledger.mjs help
```

## Use

A full A/B session: run arm A, add it, run arm B, add it, compare.

```
# 1. Run the engine with the base build; it writes out/run-base/.
slimtoken run --plan plan.json --output out/run-base

# 2. Record it.
slimtoken-ledger add out/run-base --arm base --ledger ledger.jsonl

# 3. Run the engine with the current build.
slimtoken run --plan plan.json --output out/run-cur

# 4. Record it.
slimtoken-ledger add out/run-cur --arm cur --ledger ledger.jsonl

# 5. The A/B bar: exit 1 when cur regressed base on anything gated.
slimtoken-ledger compare --ledger ledger.jsonl --arm base --arm cur
```

`add` reads the engine `report.json` in the run dir. The parser is
defensive; this is the field list it reads:

| Row field | Read from `report.json` (first match wins) |
|---|---|
| `wall_ms` | `elapsed_ms`, `wall_ms`, `duration_ms`, `total_duration_ms`; else the sum of `phases.planning_ms + wave_ms + reconcile_ms` |
| `input_tokens` | `total_input_tokens`, `input_tokens`, `tokens.input`, `usage.input_tokens`, `usage.prompt_tokens` |
| `output_tokens` | `total_output_tokens`, `output_tokens`, `tokens.output`, `usage.output_tokens`, `usage.completion_tokens` |
| `tasks` | `tasks` as a status histogram (`succeeded`, `failed`, `skipped`, `cancelled`, `unknown`); accepts both an array and an object map |

`add` also scans the run dir top level for known pack JSON twins
(`test.json`, `review.json`, `migrate.json`, `docs.json`,
`contract.json`, `coverage.json`, `research.json`, `loop.json`) and
records each one's top-level `ok`. A field the report does not have is
stored as `null`. A run dir with no readable `report.json` exits 2 — a
missing run is never a pass.

When an arm has several rows, `compare` uses the LATEST row.

`compare` prints a side-by-side table (wall, tokens, per-gate ok) with
deltas as B - A, plus a percent when the base value is non-zero. With
`--out DIR` it also writes `report.md` (a judge report) and
`ledger.json` (a machine twin: `{ok, arm_a, arm_b, a, b, deltas,
regressions}`). Field names are stable; new fields append at the end.

`show` prints every row (or one arm with `--arm NAME`) as a table.

## The row schema

One JSON object per line in the ledger file:

| Field | Type | Meaning |
|---|---|---|
| `ts` | string | ISO-8601 time the row was appended. |
| `arm` | string | The arm name from `--arm`. |
| `run_dir` | string | The run dir the row was read from. |
| `wall_ms` | number \| null | Wall or elapsed time of the run. |
| `input_tokens` | number \| null | Total input tokens billed. |
| `output_tokens` | number \| null | Total output tokens billed. |
| `tasks` | object \| null | Task counts by status. |
| `gates` | object | Gate name to `ok` (true/false/null) from pack JSON twins. |
| `tags` | object | Free-form `k=v` pairs from `--tags`. |

## Exit codes

| Code | Meaning |
|---|---|
| 0 | Command succeeded. For `compare`: B matched or improved A on every gated metric. |
| 1 | `compare` found a regression. This gives the A/B bar teeth: a CI script that runs `compare` fails when wall went up or a gate A passed now fails. |
| 2 | Usage error: bad args, missing `--arm`, unreadable run dir, missing ledger, fewer than two arms, unknown arm or command. |

The gated metrics are wall and gates. Token counts are reported but
not gated: token burn is a judge concern, not a pass/fail bar.

## Host wiring (SlimToken engine)

The A/B rule lives here, in the host, not in the engine. Typical
wiring:

1. Engine runs arm A (base build) and arm B (current build) and writes
   two run dirs. These steps spend model tokens, in parallel.
2. Host runs `slimtoken-ledger add <run-dir> --arm <name>` after each
   run. Zero tokens.
3. Host runs `slimtoken-ledger compare --arm base --arm cur`. Exit 1
   means the change did not earn its keep; the host stops or steers.
   Zero tokens.
4. Host reads `report.md` / `ledger.json` when it needs judge material
   (which metric regressed, and by how much).

## Development

The canonical home of this pack is `sidecars/ledger/` in the slimtoken
repo. Keep zero runtime dependencies. Keep every command zero-token
(pure JSON / JSONL file IO). Appends must use an explicit `"\n"` line
ending. The `selftest` script spawns the dispatcher against a temp
ledger and must stay green:

```
npm run selftest
```
