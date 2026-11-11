# slimtoken-flake

Flake sidecar for the SlimToken engine. A test that fails one time in
five costs tokens: the engine drafts a fix for a defect that is not
there. This pack proves which tests are nondeterministic before anyone
spends tokens on a fix. It runs the suite N times and counts, per test,
how many runs passed and how many failed. A test that passes every run
is clean. A test that fails every run is broken — a real defect, not a
flake. A test that passes some runs and fails others is flaky. That
last list is the one worth spending tokens on.

The gate costs zero model tokens. It shells out to the real test runner
(`node --test` or `pytest`) and reads the JUnit XML report. It never
calls a network LLM.

## The trade

N runs trade wall clock for confidence. A test that fails 20% of the
time passes three runs in a row about half the time, so `--runs 3` can
miss that flake. With `--runs 10` the same test passes all ten runs
about one time in nine. Raise N when a missed flake is expensive; keep
N low when wall clock is the budget.

## What is in the pack

| Piece | Role |
|---|---|
| `probe` (`lib/probe.mjs`) | Detect the suite, run it N times, parse the JUnit reports, name the FLAKY and BROKEN tests. |
| `lib/junit.mjs` | Minimal JUnit XML parser. Imported by the probe gate. |
| `fixtures/` | A known-flaky suite (counter file), a stable suite, a broken suite, and an empty dir. |
| `test/selftest.mjs` | Zero-dependency selftest. Spawns the dispatcher against the fixtures and asserts exit codes. |

## Install

```
npm install slimtoken-flake
```

Or run it straight from a checkout:

```
node sidecars/flake/bin/slimtoken-flake.mjs help
```

## Use

```
slimtoken-flake probe tests/
slimtoken-flake probe tests/ --runs 10 --out gate-report/
```

Detection is automatic and uses the same rules as the test pack. Node
runs when the directory has a `package.json` with a `test` script or a
`*.test.js` / `*.test.mjs` file. Python runs when the directory has a
`test_*.py` or `*_test.py` file, a `pytest.ini`, or a `pyproject.toml`
with a pytest section — and only when `python -m pytest --version`
answers. Without pytest there is no usable per-test reporter
(unittest), so the probe prints WARN SKIP; when that is the only suite,
nothing is measurable and the probe exits 2.

The probe always runs `node --test` itself for Node suites, with the
working directory set to the target directory and a JUnit reporter
writing to a temp file. It does not replay a custom `npm test` script.
Python suites run as `python -m pytest <dir> -q --junitxml=<tempfile>`.
Every run has a 300 second timeout.

Output is one line per test, findings first, then a summary:

```
FLAKY flaky counter (1/2 passed)
OK stable always passes (2/2)
flake probe: 2 tests — 1 ok, 1 flaky, 0 broken (2 runs)
```

A test that fails every run is a different verdict — not a flake, still
a finding:

```
BROKEN broken always fails (0/2)
flake probe: 1 test — 0 ok, 0 flaky, 1 broken (2 runs)
```

With `--out DIR` the probe also writes `report.md` (a judge report: a
per-test table with pass/fail counts and verdicts) and `flake.json`
(a machine twin: `{ok, runs, tests: [{name, passed, failed, verdict}], summary}`).
Field names are stable; new fields append at the end.

Exit codes: 0 every test passed every run; 1 at least one FLAKY or
BROKEN test; 2 usage error, no suite found, or the suite errored in
every run (nothing measurable — the probe fails closed).

## Host wiring (SlimToken engine)

The engine owns the wave; the pack owns the gate; the host owns the
judge (see `EMBEDDING.md` at the repo root and `sidecars/README.md`).
Typical wiring:

1. Host runs `slimtoken-flake probe src/ --runs 5 --out flake-report/`.
   This step spends zero model tokens.
2. Host reads `flake.json`. The FLAKY test names go to the engine as
   fix tasks — one task per FLAKY test, in one wave. BROKEN tests are
   triage material, not flake fixes. This step spends model tokens.
3. Engine drafts fixes only for the named FLAKY tests.
4. Host proves the fix with the test pack (`slimtoken-test run`) and
   re-runs the probe. A fix that turns a FLAKY test into a BROKEN test
   removed the flake but broke the test — the verdict line says which.

## Development

The canonical home of this pack is `sidecars/flake/` in the slimtoken
repo. Keep zero runtime dependencies. Keep the gate zero-token (shell
out to a real runner, never call a model). The `fixtures/flaky` suite
reads and writes a counter file next to itself: it fails when the
counter is 0 and passes otherwise. The selftest must reset that counter
between scenarios, or the first-run failure never happens again. The
`selftest` script in `package.json` spawns the dispatcher against
`fixtures/` and must stay green:

```
npm run selftest
```
