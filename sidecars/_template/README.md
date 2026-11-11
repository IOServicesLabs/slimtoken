# slimtoken-<name>

<One paragraph: what this pack gates for the SlimToken engine, when a
host adds it, and what it costs (zero tokens / model tokens).>

## What is in the pack

| Piece | Role |
|---|---|
| `gate` (`lib/gate.mjs`) | <what the gate checks>. |

## Install

```
npm install slimtoken-<name>
```

Or run it straight from a checkout:

```
node sidecars/<name>/bin/slimtoken-<name>.mjs help
```

## Use

```
slimtoken-<name> gate build/
```

Exit codes: 0 pass, 1 findings, 2 usage error.

## Host wiring (SlimToken engine)

The engine owns gates; the host owns the judge (see `EMBEDDING.md` at
the repo root and `sidecars/README.md`). Typical wiring:

1. Engine runs a wave and writes artifacts to the run output dir.
2. Host runs `slimtoken-<name> gate` on the artifacts.
3. The host reads the gate output and decides pass/fail, then steers
   the next wave.

## Development

Copy this template from `sidecars/_template/`. Keep zero runtime
dependencies. Keep the gate zero-token (shell out to a real tool). The
`selftest` script in `package.json` must stay green.
