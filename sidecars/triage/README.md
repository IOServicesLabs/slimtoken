# slimtoken-triage

Failure triage for the SlimToken engine. When a gate fails, this pack
drafts K candidate diagnoses and keeps the one whose repro fails. The
model proposes; the repro is the proof. Repair turns from one shot into
hypothesis elimination.

This is a judge pack: it spends tokens by design. Read the COST section
before you run it.

## Cost (read this first)

- `candidates` makes exactly ONE model call per run. The K diagnoses
  come in one response, not K calls. Cost per run = 1 call with the
  failure report as context.
- `repro` spends ZERO model tokens. Repros are shell commands with a
  30s timeout.
- Budget cap: `--max-bytes N` (default 10000) limits the failure report
  read. A larger report is truncated, and a WARN goes to stderr.
- One retry on HTTP 429 / 5xx with a 2s backoff. Then the command
  exits 1.
- Keys come ONLY from the environment: `TRIAGE_BASE_URL`,
  `TRIAGE_API_KEY`, `TRIAGE_MODEL`. A missing key exits 2 and names
  the key.

## What is in the pack

| Piece | Role |
|---|---|
| `candidates` (`lib/candidates.mjs`) | One model call: K diagnoses from a gate failure report. Writes `candidates.json`. |
| `repro` (`lib/repro.mjs`) | Runs each candidate's repro command (zero tokens). Confirms or rejects. Writes `confirmed.json`, and `report.md` + `triage.json` with `--out`. |

## Install

```
npm install slimtoken-triage
```

Or run it straight from a checkout:

```
node sidecars/triage/bin/slimtoken-triage.mjs help
```

## Use

Point `candidates` at a gate failure report (the gate's `report.md`),
then prove the drafts with `repro`:

```
export TRIAGE_BASE_URL=https://api.example.com/v1
export TRIAGE_API_KEY=sk-...
export TRIAGE_MODEL=...

slimtoken-triage candidates report.md --k 3 --out triage/
slimtoken-triage repro triage/candidates.json --workdir build/ --out triage/
```

`candidates` writes `candidates.json` with the candidate list, the
model name, and the API usage block. `repro` runs each candidate's
`repro` command in `--workdir` (or the invocation directory).

The confirmed candidate is the repair prompt for the next wave: the
host steers a new engine wave at it, or hands it to a human judge.

## Exit codes

| Code | Meaning |
|---|---|
| 0 | `candidates`: parsed and written. `repro`: at least one candidate confirmed. |
| 1 | `candidates`: transport / HTTP / parse error, or the model did not return exactly K well-formed candidates. `repro`: no candidate confirmed. |
| 2 | Usage error: bad args, missing env key, unreadable failure file, malformed `candidates.json`. |

On `repro` exit 1, no hypothesis proved itself: the repros all passed
or could not run. The host widens the search (more candidates, a
different model) or judges manually. A "not confirmed" result is a
result, not a crash.

## The repro-command contract

Each candidate carries one `repro` field: a single shell command.

- It runs with `shell: true` in `--workdir` (or the invocation
  directory).
- It exits non-zero when the hypothesis holds. That non-zero exit is
  the proof: the candidate is CONFIRMED.
- It exits zero when the hypothesis does not hold. The candidate is
  rejected.
- A command the shell cannot start ("not found") is rejected with the
  error recorded. It is not a crash and not a confirmation.
- Timeout is 30s per candidate. A timeout is a rejection.

A confirmed candidate's `confirmed.json` entry carries a stderr excerpt
capped at 10 lines, for the judge.

## Host wiring (SlimToken engine)

The engine owns the wave; the pack owns the gate; the host owns the
judge (see `EMBEDDING.md` at the repo root and `sidecars/README.md`).
Typical wiring:

1. Engine runs a wave; a pack gate fails and writes its `report.md`.
2. Host runs `slimtoken-triage candidates report.md` (one model call).
3. Host runs `slimtoken-triage repro candidates.json` (zero tokens).
4. Host reads `triage.json`: a confirmed candidate is the repair
   prompt for the next wave; an empty `confirmed` list means widen the
   search or judge by hand.

## Development

Copy the shape from `sidecars/_template/`. Zero runtime dependencies;
Node 22+ only. Keep `candidates` at one model call per run; keep
`repro` zero-token. Run the selftest after any change:

```
node test/selftest.mjs
```

The selftest starts a local mock chat/completions server on
127.0.0.1 — no real keys, no real tokens.
