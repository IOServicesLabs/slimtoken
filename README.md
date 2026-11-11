# SlimToken

SlimToken is a core harness for fast LLM code generation. It runs many
model streams at the same time. It gives other harnesses and agent swarms
high tokens per second when they build apps.

SlimToken does not make one long response. It plans first, then it writes
all files in parallel. One run of a 20-file app takes approximately 13 s
on DeepSeek, at approximately 1,500 to 1,800 output tokens per second.

## Measured

Every number below comes from paired A/B runs: interleaved arms, one
ledger row per run, no claim without a pair.

| Result | Number | When |
|---|---|---|
| 20-file app, plan to files | ~13 s wall on DeepSeek | 2026-11 |
| Aggregate output rate, one wave | ~1,500–1,800 tok/s (one stream alone: ~130–150) | 2026-11 |
| Concurrency knee sweep (Muse) | 1,147 tok/s at 32 streams; knee at 24–32 | 2026-10 |
| Largest-first admission | −25.7% wave time on Muse | 2026-11 |
| Patience-bounded repair injection | −31% retry volume | 2026-11 |
| Browser QA defect library | all 40 seeded UI defects caught, $0 per run | 2026-11 |
| Current engine vs baseline | quality held; wall wins on the largest apps | 2026-11 |

## How it works

1. **Route.** SlimToken estimates the size of the job. A small job goes to
   one direct response. A large job goes to a parallel wave.
2. **Plan.** The architect writes a manifest. The manifest gives each file
   its path, its purpose and its exact interface contracts.
3. **Wave.** One worker writes each file at the same time. Each worker
   writes against the contracts. No worker waits for another worker.
4. **Check.** An optional test command examines the result.
5. **Repair.** If a file fails, SlimToken repairs only that file. The
   repair sends small search-and-replace edits, not a full new file.

The scheduler adjusts the number of streams while the run continues. It
increases the number when the server is healthy. It decreases the number
when the server is overloaded.

## Stack profiles

The core does not contain rules for one language or framework. A
**profile** contains the rules for one stack. A profile gives:

- The prompt rules for the planner and the workers.
- The syntax of contracts and comments.
- The files that the test suites can examine.
- The toolchain files that SlimToken writes without a model.
- The fixes that SlimToken applies to the manifest.

There are three built-in profiles: `python`, `nextjs`, and `rust`. SlimToken
selects the profile from the objective. To select a profile yourself, use
`--profile`:

```bash
slimtoken run --architect --profile nextjs --objective "..." --output out
```

A host can also load profiles from files, with no recompile. The
`profiles/` directory is a shipped library (today: `fastapi`).
`--profile-dir DIR` loads every JSON file in a directory. Auto
detection consults the library before the built-ins:

```bash
slimtoken run --architect --profile-dir profiles --objective "Build a FastAPI service"
```

Another harness can declare its own `Profile` as a Rust `static` and
give it to `ArchitectOptions::profile`. It can also load a JSON
profile at startup with `Profile::load_file` or `Profile::load_dir`.
See `src/profile/` and `profiles/README.md`.

## Install

You need Rust 1.85 or newer.

```bash
cargo build --release
```

The binary is `target/release/slimtoken`.

## Sidecar packs (optional)

Sidecar packs are bolt-on checks: tests, types, docs, security, and
more. The engine does not need them. Add a pack only when you want
its check. Each pack installs with npm and runs a gate for zero
model tokens:

```bash
npm install --global sidecars/qa
slimtoken-qa validate --strict build/     # 0 pass, 1 findings, 2 usage
```

The catalog and the pack contract live in
[sidecars/README.md](sidecars/README.md).

## Quick start

### Run without a model

Use the mock client to test the harness:

```bash
slimtoken run --mock --objective "Design a concurrent coding agent" --output slimtoken-out
```

### Build an app

Put your API key in an environment variable. Then start a run:

```bash
export DEEPSEEK_API_KEY=...

slimtoken run \
  --provider deepseek \
  --architect \
  --sink assemble \
  --objective "Build a Python CLI expense tracker with a pytest suite." \
  --output slimtoken-out
```

The run writes these files to the output directory:

| File | Contents |
|---|---|
| `manifest.json` | The file layout and the contracts |
| `plan.json` | The tasks that the wave runs |
| `report.json` | Status, timings, tokens and tokens per second for each task |
| `final-output.md` | All files, in one document |

### Add a check and repairs

Give a command that reads `final-output.md` on stdin. Exit code 0 means
the build passes. If the build fails, SlimToken repairs the files that
failed:

```bash
slimtoken run --provider deepseek --architect --sink assemble \
  --objective "..." --output slimtoken-out \
  --test-cmd "python -I check-build.py <deps-dir>"
```

## Providers

| Provider | Base URL | Notes |
|---|---|---|
| `deepseek` | `https://api.deepseek.com/v1` | Use reasoning effort `none`. Approximately 290 tokens/s for each stream. |
| `muse` | `https://api.meta.ai/v1` | Use reasoning effort `minimal` or `low`. Limit: 150 requests per minute. |
| `anthropic` | `https://api.anthropic.com/v1` | Claude models over the OpenAI-compatible layer. Key from `ANTHROPIC_API_KEY`. Starting points, not measured: effort `low`, schema off, conservative burst cap. |
| Any OpenAI-compatible server | `--base-url` | SlimToken adds `/chat/completions` to the base URL. |

Put the key in `--api-key` or in `OPENAI_API_KEY`. Do not put a key in a
file that git tracks.

## Main commands

| Command | Purpose |
|---|---|
| `slimtoken run` | Plan and run one objective |
| `slimtoken interactive` | Start, steer and cancel runs from a prompt |
| `slimtoken serve` | Start the REST API (see [API.md](API.md)) |
| `slimtoken bench` | Measure server throughput at a given concurrency |
| `slimtoken ab` | Compare two configurations in paired rounds |
| `slimtoken validate` | Examine a saved plan without running it |

## Important run flags

| Flag | Default | Purpose |
|---|---|---|
| `--architect` | off | Plan with a manifest and contracts |
| `--route` | `auto` | `auto`, `direct` or `wave` |
| `--profile` | `auto` | Stack profile: `auto`, `python` or `nextjs` |
| `--sink` | `model` | `assemble` joins all files into `final-output.md` with no model call. `model` merges them with one model call. |
| `--test-cmd` | — | Command that checks the build; failures start repairs |
| `--reconcile-edits` | `true` | Repairs send search-and-replace edits |
| `--max-tasks` | `48` | Upper limit of files in the plan |
| `--max-concurrency` | `128` | Upper limit of streams at the same time |
| `--adaptive` | `true` | Adjust the number of streams during the run |
| `--manifest` | — | Use a saved manifest; do not plan again |
| `--plan-only` | off | Write the plan, but do not run it |

## Measure a change

`slimtoken ab` compares two configurations in paired rounds. It runs
the arms in alternate order and records wall time, tokens and test
results per round. Do not merge a change that has no A/B evidence.

## Test

```bash
cargo fmt --all
cargo test --all-targets
cargo clippy --all-targets -- -D warnings
```

## Code layout

```text
src/
  architect.rs   Manifest planning and plan construction (stack-agnostic)
  profile/       Stack profiles: python.rs, nextjs.rs
  scheduler.rs   Parallel wave, retries, inline repair, metrics
  controller.rs  Adaptive concurrency controller
  reconcile.rs   Test-driven repairs with edit blocks
  client.rs      Streaming OpenAI-compatible client and mock client
  dag.rs         Plan and task model, graph validation
  planner.rs     General task planner (non-architect runs)
  runner.rs      Run manager for the interactive CLI and the REST API
  api.rs         REST routes
  main.rs        Command-line interface
profiles/        Shipped stack profile library (fastapi)
sidecars/        Optional gate packs (see sidecars/README.md)
examples/        Example plan and REST request
```

## Safety

Workers return text only. They do not run shell commands and they do not
write files. Do not give model workers shell access or write access. Keep
the REST server on loopback, because it has no authentication.
