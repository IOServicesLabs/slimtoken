# slimtoken-loop

Host-loop sidecar for the SlimToken engine. This pack turns the "host
wiring" prose from `sidecars/README.md` into a runnable: one command
runs the engine, then runs the pack gates, then writes one verdict.
The pack has no gate of its own. Zero runtime dependencies. Node 22
or higher. The loop code never calls a model; it spends no tokens
itself (the engine run it spawns spends the host's engine tokens).

## What is in the pack

| Piece | Role |
|---|---|
| `run` (`lib/run.mjs`) | Runs the loop: engine, then gates, then the verdict. |
| `init` (`lib/init.mjs`) | Writes a commented `loop.toml` template. |
| `parse` (`lib/config.mjs`) | Minimal TOML reader for `loop.toml` (strings + `[[gates]]`). |

## Install

```
npm install slimtoken-loop
```

Or run it straight from a checkout:

```
node sidecars/loop/bin/slimtoken-loop.mjs help
```

## Use

Create a config:

```
slimtoken-loop init
```

This writes `loop.toml`. Then run the loop:

```
slimtoken-loop run --config loop.toml --out verdict
```

A real example that chains `slimtoken-qa` gates after an engine wave:

```toml
plan = "plan.json"
engine = "slimtoken"
output = "run-output"
[[gates]]
name = "syntax"
cmd = "slimtoken-qa validate {{output}}"
[[gates]]
name = "smoke"
cmd = "slimtoken-qa smoke --url http://localhost:3000 --routes / --out {{output}}/qa"
```

The loop spawns `slimtoken run --plan plan.json --output run-output`,
then runs each gate in order. `{{output}}` in a gate command expands
to the run output dir. The verdict is a console table; with `--out DIR`
the loop also writes `report.md` (for a human judge) and `loop.json`:

```json
{ "ok": true, "engine": { "exit": 0 },
  "gates": [ { "name": "syntax", "cmd": "...", "exit": 0 } ],
  "summary": { "gates": 1, "passed": 1, "failed": 0, "verdict": "pass" } }
```

Exit codes: 0 the engine run passed and every gate exited 0; 1 the
engine run or at least one gate failed; 2 usage error, missing config,
or the engine binary was not found.

## loop.toml reference

| Key | Where | Meaning |
|---|---|---|
| `plan` | top level | Path to a `plan.json` the engine runs. Ignored when `objective` is set. One of `plan` or `objective` is required. |
| `objective` | top level | A plain text goal. When set, the loop runs `engine run --objective "<text>"` instead of `--plan`. |
| `engine` | top level | The engine command line. A bare name must be on PATH (`slimtoken`). A full command string also works (`node C:\app\engine.mjs`). Required. |
| `output` | top level | The run output dir, relative to the config file. Default `run-output`. |
| `[[gates]]` | table | One gate. Gates run in file order, each through a shell. |
| `name` | inside `[[gates]]` | The gate name in the verdict. |
| `cmd` | inside `[[gates]]` | The gate command. `{{output}}` expands to the run output dir. Exit 0 is a pass; any other exit is a failure. |

Relative paths (`plan`, `output`, `--out`) resolve against the config
file's directory. A non-zero engine exit stops the loop: the gates
never run on a broken run, and the loop exits 1.

## Ownership reminder

The engine owns the wave. The packs own the gates. The host owns the
judge. This loop is a convenience, not a judge: it runs the sequence
and reports the verdict, and nothing more. A green loop is not a
decision; the host still reads `report.md` and decides what ships and
what the next wave does.

## Development

The canonical home of this pack is `sidecars/loop/` in the slimtoken
repo. Copy the template from `sidecars/_template/`. Keep zero runtime
dependencies. The loop never calls a model. The `selftest` script in
`package.json` must stay green:

```
node test/selftest.mjs
```

The selftest builds a temp workspace per case and uses a fake engine
(a plain node script), so it runs anywhere Node runs and spends no
tokens.
