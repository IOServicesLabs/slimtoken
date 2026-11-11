# slimtoken-judge

Judge layer for the SlimToken engine. This pack replaces the host's
ad-hoc judging loop. It merges every pack's JSON twin into one verdict
brief, calls one judge model, and parses the steer directives back into
a plan fragment the host can turn into the next wave.

This is a judge pack: it spends tokens by design. Read the COST section
before you run it.

## COST

- One judge model call per `judge` invocation. No retries, no fan-out,
  no loops: exactly one call.
- Input: the merged verdict brief, a few KB (cap below). Output: the
  steer JSON, small. Cost per run = 1 judge call, at the host's judge
  model rate.
- `brief` costs nothing: file reads and string work only.
- Budget cap: `--max-input-bytes N` (default 20000). When the brief is
  bigger than N, the pack drops the oldest pack sections (brief) or
  hard-slices the text (judge) and prints a WARN line. It never silently
  drops the whole verdict.
- Keys come only from the environment. Nothing is hardcoded, nothing
  phones home by default, no key lives in this repo:
  - `JUDGE_BASE_URL` — the judge endpoint base, e.g. `https://api.example.com/v1`
  - `JUDGE_API_KEY` — the bearer key
  - `JUDGE_MODEL` — the judge model name
- Missing any of the three = exit 2, and the message names the missing
  variable.

## What is in the pack

| Piece | Role |
|---|---|
| `brief` (`lib/brief.mjs`) | Scans directories for known JSON twins and merges them into `brief.md`. Zero tokens. |
| `judge` (`lib/judge.mjs`) | Sends the brief to one judge call and parses the reply into `steer.json`. One model call. |
| `bin/slimtoken-judge.mjs` | CLI dispatcher: `brief`, `judge`, `help`. |

## Install

```
npm install slimtoken-judge
```

Or run it straight from a checkout:

```
node sidecars/judge/bin/slimtoken-judge.mjs help
```

## Use

Step 1 — build the verdict brief from the pack output directories:

```
slimtoken-judge brief run-out/ qa-out/ --out judge-in
```

`brief` looks for the known twin files by name: `test.json`,
`review.json`, `migrate.json`, `docs.json`, `contract.json`,
`coverage.json`, `research.json`, `loop.json`, `ledger.json`,
`replay.json`, `trace.json`, `security.json`, `flake.json`, `fuzz.json`,
`i18n.json`, `types.json`. Each found twin becomes one compact section
in `brief.md`: the pack name, its `ok` flag, its summary, the finding
count, and up to 5 error excerpt lines.

Finding zero twins is a usage-level mistake (nothing to judge): exit 2.

Step 2 — judge the brief:

```
slimtoken-judge judge --brief judge-in/brief.md --out judge-out
```

This makes one POST to `$JUDGE_BASE_URL/chat/completions`
(OpenAI-compatible: `{model, messages: [system, user], temperature: 0}`).
The system prompt orders the model to answer with one JSON object only:

```
{"verdict": "pass" or "fail", "reasons": [...], "steer": [{"objective": "...", "priority": 1}]}
```

Fences around the JSON are stripped. `judge` writes `steer.json`:

```
{ "verdict": "fail", "reasons": [...], "steer": [...], "model": "...", "usage": {...} }
```

With `--out DIR` it also writes `report.md` (one-glance markdown) and
`judge.json` (the same record under the pack-name twin convention).

The `steer.json` contract the host consumes: `verdict` is the run-level
pass/fail, `reasons` is what mattered, and `steer` is the plan fragment
— one `{objective, priority}` per next-wave task, priority 1 first. The
host turns it into a plan (or feeds the REST routes) for the next wave.

Exit codes: 0 pass (verdict parsed, any value), 1 judge failure
(transport error, HTTP >= 400, unparseable reply), 2 usage error (bad
args, missing env var, missing brief, zero twins).

## Host wiring (SlimToken engine)

The pack spends tokens; everything else in the loop is free. The host
decides when judging is worth it:

1. Engine runs a wave; packs gate the artifacts for zero tokens.
2. Host runs `slimtoken-judge brief` over the pack output dirs (free).
3. Host runs `slimtoken-judge judge` when a verdict is worth one call —
   for example after a gate failure, or on a scheduled cadence.
4. Host reads `steer.json` and steers the next wave.

Keep the judge out of the hot path. A good shape: run it on failure, or
at most once per wave, never per task.

## Development

Copy this template from `sidecars/_template/`. Keep zero runtime
dependencies. The `selftest` script in `package.json` must stay green:

```
node test/selftest.mjs
```

The selftest starts a local `node:http` mock judge on an ephemeral
port. Spawn is async on purpose: `spawnSync` would freeze the event
loop that hosts the mock server. The selftest asserts every exit code,
the WARN lines, and the `--out` report shape. It spends no tokens.
