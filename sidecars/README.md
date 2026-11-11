# Sidecar packs

A sidecar pack is a bolt-on capability for the SlimToken engine. The
engine ships bare: no pack code, no pack dependency, no pack knowledge.
A host adds a pack when it wants that capability. This file is the
architecture: the contract, the anatomy, and how to add a new pack.

The rule (from `EMBEDDING.md`): capabilities are bolt-ons, never core.
QA gates, agent personas, review passes — all of them live in packs
like this one, never in the engine.

## Quick start

Copy-paste path, from the repo root:

```bash
cargo build --release                              # 1. build the engine
./target/release/slimtoken run --plan plan.json \
  --output run-out                                 # 2. run one wave
npm install --global sidecars/docs                 # 3. turn on one pack
slimtoken-docs check docs/ --out docs-report       # 4. run the gate
```

Exit code 0 means the gate passed. 1 means findings. 2 means a usage
error. The gate writes `report.md` plus a JSON twin; that pair is the
judge material. The engine build does not change when you add a pack.
The full catalog is at the end of this file.

## The contract

A pack touches the engine at three seams. Each seam is stable.

| Seam | Direction | Mechanism | Cost |
|---|---|---|---|
| Plan in | Host → engine | `slimtoken run --plan plan.json` (a ready plan file) | model tokens |
| Artifacts out | Engine → pack | `plan.json`, `report.json`, `trace.jsonl`, task outputs in the run output dir | 0 tokens |
| Live control | Host ↔ engine | REST API (`slimtoken serve`, build with `--features serve`) | 0 tokens |

A pack never imports engine code. It reads run artifacts, runs gates,
and prints a verdict. The engine stays generic; the pack owns the
domain.

## Gate conventions

Every gate command in a pack follows the same contract (see the QA
pack for the reference shape):

- **Exit codes**: 0 pass, 1 findings (the artifact failed a gate),
  2 usage error (bad args, missing input).
- **Zero tokens by default**: a gate shells out to a real tool
  (`python -m py_compile`, `node --check`, CDP, a test runner). It
  never calls a model. The expensive part stays free.
- **Judge output**: a `report.md` (a human or LLM reads it in one
  glance) plus a machine-readable JSON twin with stable field names
  (new fields append at the end).
- **Fail closed**: a stale or unreadable artifact fails, never passes.

## Who owns what

- The **engine** owns the wave: plan, parallel execution, repair,
  report.
- The **pack** owns the gate: it checks the artifacts the wave
  produced.
- The **host** owns the judge: it reads the gate report, decides
  pass/fail, and steers the next wave through the REST routes (or a
  new `--plan` run).

The host loop:

```
plan -> wave -> pack gate (0 tokens) -> host judge -> steer next wave
```

High-speed token generation pays at the wave step: many draft tasks
run at the same time. The gate step then spends nothing. A pack that
fits this shape benefits most from the engine.

## Turning on a pack

The engine ships bare. No pack is active until the host turns it on,
and turning one on never changes the engine build.

1. Run the engine as always: `slimtoken run --plan plan.json`. The
   pack reads the run output dir (`plan.json`, `report.json`,
   `trace.jsonl`, task outputs). No engine flag, no engine change.
2. Put the pack command on PATH. From this repo either install it —
   `npm install --global <path to sidecars/<name>>` or `npm link`
   inside the pack dir — or run it in place:
   `node sidecars/<name>/bin/slimtoken-<name>.mjs <command> ...`.
3. Point the gate at what it checks. Each gate takes the paths it
   checks: a run output dir, a source tree, an app URL, or a report
   file. The pack README names the arguments. Example:
   `slimtoken-docs check docs/ --out docs-report`.
4. Read the exit code: 0 pass, 1 findings, 2 usage. The `report.md`
   plus its JSON twin in the `--out` dir is the judge material.
5. Judge packs (`slimtoken-judge`, `-vision`, `-triage`) also need
   their environment keys set — see each pack README. These three are
   the only packs that spend model tokens.
6. Optional: turn on `slimtoken-loop` to drive the whole loop —
   plan → wave → gates → verdict — from one `loop.toml` file.

## Pack anatomy

The QA pack (`sidecars/qa/`) is the reference:

| Piece | Role |
|---|---|
| `package.json` | npm manifest: `slimtoken-<name>`, zero runtime deps, Node 22+, `bin` entry, `selftest` script. |
| `bin/slimtoken-<name>.mjs` | CLI dispatcher: one command per gate, `help`, exit codes 0/1/2. |
| `lib/*.mjs` | The gate implementations. Pure node, no packages. |
| `README.md` | ASD-STE100: install, use, host wiring, development note. |

## How to add a new pack

1. Copy `sidecars/_template/` to `sidecars/<name>/`.
2. Rename `slimtoken-x` to `slimtoken-<name>` in `package.json`,
   `bin/`, and `README.md`.
3. Replace the example gate in `lib/gate.mjs` with the real gate
   (shell out to the domain tool; keep the exit-code contract).
4. Add the command to the dispatcher in `bin/`.
5. Write a `selftest` that runs `help` plus `node --check` on every
   lib file. Keep it zero-token.
6. Write the README in ASD-STE100: what the pack gives, install, use,
   host wiring, development.

Rules for the pack:

- Zero runtime dependencies. Node 22+ only.
- A gate never calls a model. If a step needs judgment, it writes
  judge material (report) and the host judges.
- Stack-specific rules live in the host's profile, not in the pack
  and not in the engine.

## Catalog

| Pack | Status | Engine does (tokens) | Gate does (0 tokens) | Host judges |
|---|---|---|---|---|
| `slimtoken-qa` | shipped | builds the app in a wave | smoke, flow, syntax validate | report.md, screenshots |
| `slimtoken-test` | shipped | drafts tests per module in parallel | runs them (pytest / unittest / node --test) | failure triage |
| `slimtoken-review` | shipped | review passes per changed file | clippy / cargo check, per-file `node --check` | findings |
| `slimtoken-migrate` | shipped | per-file migration waves from profile rules | syntax check + `cargo check` (+ opt-in build) | semantic drift |
| `slimtoken-docs` | shipped | doc draft and drift updates | link + anchor + fence checks, style warnings | accuracy |
| `slimtoken-contract` | shipped | API / schema test generation | JSON parse, OpenAPI fields, `$ref` resolution | spec mismatches |
| `slimtoken-coverage` | shipped | drafts tests only for uncovered lines | runs coverage, asserts the delta grew | which gaps matter |
| `slimtoken-research` | shipped | drafts N briefs in one wave, one per sub-question | fetches every citation, verifies each claim appears | which claims to drop |
| `slimtoken-loop` | shipped | (host helper) | runs plan → wave → gate → verdict in one command | — |
| `slimtoken-ledger` | shipped | (host helper) | appends one JSONL row per run, prints A/B deltas | whether a change earned its keep |
| `slimtoken-replay` | shipped | replays one plan against two builds | diffs reports and traces per task | quality / wall regressions |
| `slimtoken-fuzz` | shipped | drafts fuzz inputs per parser or schema | runs them; crashes are findings | seed quality |
| `slimtoken-i18n` | shipped | drafts translations per locale per string | key-set and placeholder parity | quality per language |
| `slimtoken-trace` | shipped | (analysis pack) | flags retry storms, cache misses, width collapse; tuning brief | which heuristics earn their keep |
| `slimtoken-types` | shipped | drafts type bindings from a spec | `tsc --noEmit` / `py_compile` | which bindings ship first |
| `slimtoken-security` | shipped | drafts an attack corpus per endpoint | executes the corpus against a safe target | exposure |
| `slimtoken-flake` | shipped | drafts fixes for intermittent tests | runs the suite N times, counts nondeterminism | N vs confidence |
| `slimtoken-judge` | shipped (tokens) | normalizes JSON twins into one brief | one judge call → verdict + steer directives | the verdict |
| `slimtoken-vision` | shipped (tokens) | batches qa screenshots | one vision call per image → score + flags | the rubric |
| `slimtoken-triage` | shipped (tokens) | drafts candidate diagnoses on a failure | runs each repro; non-zero exit confirms | the hypothesis |

The `(tokens)` packs break the zero-token default on purpose: they
spend model tokens by design. Each states its cost per run up front,
carries a budget cap, and reads its keys only from environment
variables. The rest of the packs keep the rule: a gate never calls a
model.

Every shipped pack has a `test/selftest.mjs` that asserts its exit
codes and `--out` report shape against fixtures. Run
`node test/selftest.mjs` in the pack directory after any change.

The packs share the QA shape: parallel generation at
the wave step, zero-token proof at the gate step. That is where high-speed
token generation pays: the engine drafts many artifacts at once, and
the pack proves them for free.
