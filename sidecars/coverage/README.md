# slimtoken-coverage

Coverage sidecar for the SlimToken engine. The engine drafts tests only
for uncovered lines, one gap per draft task in a wave (model tokens). This
pack measures coverage before and after the wave and proves the delta
grew. The gate costs zero model tokens — it shells out to `node --test`
or `python -m coverage` and never calls a network LLM.

## What is in the pack

| Piece | Role |
|---|---|
| `measure` (`lib/measure.mjs`) | Detect the test stack in a directory, run the suite with line coverage, print per-file `{file, covered, total, pct}` plus a suite total. |
| `delta` (`lib/delta.mjs`) | Compare two `coverage.json` files and exit 0 only when the total percent grew by at least N points. |
| `run-node` (`lib/run-node.mjs`) | Node runner: `node --test` with V8 coverage JSON, merged into per-file line counts. |
| `run-python` (`lib/run-python.mjs`) | Python runner: `python -m coverage run` with pytest (or unittest), then `coverage json`. |
| `fixtures/` | Tiny apps with partial coverage the selftest runs the gate against. |
| `test/selftest.mjs` | Zero-dependency selftest. Spawns the dispatcher against the fixtures and asserts exit codes. |

## Install

```
npm install slimtoken-coverage
```

Or run it straight from a checkout:

```
node sidecars/coverage/bin/slimtoken-coverage.mjs help
```

## Use

Measure coverage for a directory:

```
slimtoken-coverage measure app/
slimtoken-coverage measure app/ --runner node
slimtoken-coverage measure app/ --out gate-report/
```

Detection is automatic. Node runs when the directory has a `package.json`
with a `test` script or a `*.test.js` / `*.test.mjs` file. Python runs
when the directory has a `test_*.py` or `*_test.py` file, a `pytest.ini`,
or a `pyproject.toml` with a pytest section. When both stacks are present,
both run. `--runner node` or `--runner python` forces one stack. Node runs
`node --test` with `cwd: <dir>` and reads the raw V8 coverage JSON, so
per-file covered and total line counts are exact. Python probes
`python -c "import coverage"` (then `py`); when the coverage module is
absent, the gate prints `WARN SKIP python coverage: coverage module not
installed` and skips that stack — reported, never faked. Test files
themselves (`*.test.*`, `test_*.py`) are not counted as source.

Output is one section per stack, then a summary:

```
node coverage:
  src/calc.mjs  3/6  50.00%
  src/str.mjs  3/3  100.00%
node total: 6/9 lines 66.67%
coverage: measured
```

Prove the delta grew between two measurement files:

```
slimtoken-coverage delta before.json after.json
slimtoken-coverage delta before.json after.json --min-gain 5 --out gate-report/
```

`delta` exits 0 when `after.total.pct >= before.total.pct + N` (N defaults
to 0). When the gain is below N, the per-file movers print and the exit
code is 1:

```
MOVER calc.mjs: 100% -> 57.14% (-42.86 pp)
delta: -40 pp (min-gain 0) -> FAIL
```

With `--out DIR` each command writes `report.md` (a judge report) and its
machine twin: `coverage.json` (`{ok, files, total}`) for measure,
`delta.json` (`{ok, before, after, gain, minGain, movers}`) for delta.
Field names are stable; new fields append at the end.

Exit codes:

- `measure`: 0 means coverage ran for at least one stack, even at 0%.
  A selected stack that was skipped is a WARN, not green. 1 means a
  runner was selected but produced no coverage data. 2 means a usage
  error (bad flag, no dir, missing dir, no suite found).
- `delta`: 0 the delta reached the minimum gain, 1 the gain is below the
  minimum (or an artifact is stale or malformed — fail closed), 2 usage
  error (bad flag, missing args, unreadable input file).

## Host wiring (SlimToken engine)

The engine owns the wave; the pack owns the gate; the host owns the judge
(see `EMBEDDING.md` at the repo root and `sidecars/README.md`). Typical
wiring for the find-gap → draft → prove loop:

1. Host runs `slimtoken-coverage measure app/ --out before/`. This step
   costs zero model tokens.
2. Host reads `before/coverage.json`, picks the files and lines with low
   coverage, and gives the engine one draft task per gap. This step spends
   model tokens, in parallel.
3. Engine runs the test-drafting wave and writes the drafted test files.
4. Host runs `slimtoken-coverage measure app/ --out after/` and then
   `slimtoken-coverage delta before/coverage.json after/coverage.json
   --min-gain 0`. Both steps cost zero model tokens.
5. Host reads the delta report and judges: a growing delta means the
   drafted tests cover the gaps; the per-file movers are the material for
   the next wave. Which gaps matter stays a host decision.

## Development

The canonical home of this pack is `sidecars/coverage/` in the slimtoken
repo. Keep zero runtime dependencies. Keep the gate zero-token (shell out
to a real runner, never call a model). The `selftest` script in `package.json` spawns the dispatcher against
`fixtures/`. It must stay green:

```
npm run selftest
```

The Python fixture assertions are skipped with a printed note when no
python interpreter is on PATH. The same is true when the coverage
module is not installed. A skip is reported, never faked as a pass.
