# slimtoken-test

Test sidecar for the SlimToken engine. The engine drafts test files in
a wave (model tokens). This pack proves the drafted tests: it detects
the test stack in a directory, runs the real runner, and prints a
verdict. The gate costs zero model tokens — it shells out to python,
node, or npm and never calls a network LLM.

## What is in the pack

| Piece | Role |
|---|---|
| `run` (`lib/run.mjs`) | Detect and run the test suites in a directory: pytest or unittest for Python, `npm test` or `node --test` for Node. |
| `fixtures/` | Tiny pass/fail suites the selftest runs the gate against. |
| `test/selftest.mjs` | Zero-dependency selftest. Spawns the dispatcher against the fixtures and asserts exit codes. |

## Install

```
npm install slimtoken-test
```

Or run it straight from a checkout:

```
node sidecars/test/bin/slimtoken-test.mjs help
```

## Use

Run the gate on a directory of drafted tests:

```
slimtoken-test run drafted-tests/
slimtoken-test run drafted-tests/ --out gate-report/
```

Detection is automatic. Python runs when the directory has a
`test_*.py` or `*_test.py` file, a `pytest.ini`, or a `pyproject.toml`
with a pytest section. Node runs when the directory has a `package.json`
with a `test` script or a `*.test.js` / `*.test.mjs` file. When both
stacks are present, both run. When neither is present, the gate prints
SKIP and exits 0 — a report, not a pass.

Python uses `python -m pytest <dir> -q` when pytest is importable, else
`python -m unittest discover -s <dir> -q` (stdlib). Node uses `npm test`
when a test script exists, else `node --test <dir>`. Every runner has a
300 second timeout.

Output is one line per suite, then a summary:

```
OK python (2 passed)
OK node (2 passed)
test gate: 2/2 suites passed
```

A failing suite prints the last failing line instead:

```
ERROR node: not ok 2 add wrong
test gate: 0/2 suites passed
```

With `--out DIR` the gate also writes `report.md` (a judge report: a
suites table plus the exact failure text) and `test.json` (a machine
twin: `{ok, suites, summary}`). Field names are stable; new fields
append at the end.

Exit codes: 0 all suites passed (or nothing to run), 1 a suite failed,
2 usage error (no dir, missing dir, bad flag).

## Host wiring (SlimToken engine)

The engine owns the wave; the pack owns the gate; the host owns the
judge (see `EMBEDDING.md` at the repo root and `sidecars/README.md`).
Typical wiring for a test-drafting run:

1. Engine runs a test-drafting wave and writes the drafted test files
   to the run output dir. This step spends model tokens, in parallel.
2. Host serves nothing — the tests are plain files, no app server is
   needed for this gate.
3. Host runs `slimtoken-test run <run-output-dir> --out gate-report/`.
   This step spends zero model tokens.
4. Host reads `report.md` / `test.json` and judges: green suite means
   the drafted tests pass against the code; a failure line is the
   triage material for the next wave (fix the test or fix the code).

## Development

The canonical home of this pack is `sidecars/test/` in the slimtoken
repo. Keep zero runtime dependencies. Keep the gate zero-token (shell
out to a real runner, never call a model). The `selftest` script in
`package.json` spawns the dispatcher against `fixtures/` and must stay
green:

```
npm run selftest
```
