# slimtoken-qa

Browser QA sidecar for the SlimToken engine. This pack is a side
installable: the engine stays bare, and a host adds this pack when it
wants browser gates. Zero runtime dependencies. Node 22 or higher.

## What is in the pack

| Piece | Role |
|---|---|
| `smoke` (`lib/qa-smoke.mjs`) | Load gate: every route, console, network, visibility, screenshot. |
| `flow` (`lib/qa-flow.mjs`) | Interaction gate: scripted user flows over the Chrome DevTools Protocol (CDP). |
| `validate` (`lib/validate.mjs`) | Zero-token syntax gate for generated artifacts. |
| `lib/qa-cdp.mjs` | Shared CDP transport. Imported by the smoke and flow gates. |

## Install

```
npm install slimtoken-qa
```

Or run it straight from a checkout:

```
node sidecars/qa/bin/slimtoken-qa.mjs help
```

## Use

Smoke gate against a running app:

```
slimtoken-qa smoke --url http://localhost:3000 --routes /,/review --out qa-out
```

Interaction gate from a JSON flow:

```
slimtoken-qa flow --url http://localhost:3000 --flow flows/example-check-title.json --out qa-flow-out
```

Syntax gate on generated artifacts (the check the engine's shape
validator cannot do — a file with a broken `with` one-liner passes a
fence check and kills a test suite at collection):

```
slimtoken-qa validate src/app.py lib/util.mjs
slimtoken-qa validate --strict build/
```

Exit codes: 0 ok, 1 findings or syntax errors, 2 usage error.

The smoke gate also takes audit flags (opt-in): `--viewports`,
`--schemes`, `--golden`, `--max-bytes`, `--max-lcp-ms`, `--a11y`,
`--warn-gate`, `--dom-audit`, `--states`, `--keyboard`, `--motion`,
`--axe`, `--devices`, `--expect-build`. See `slimtoken-qa smoke --help`
style usage via the source header in `lib/qa-smoke.mjs`. `--axe` takes a
host-supplied axe-core UMD bundle (deep a11y audit: critical and serious
violations fail the route). `--devices` takes a device-matrix JSON file
(viewport + scheme library with per-breakpoint golden baselines). Both
default off.

## Host wiring (SlimToken engine)

The engine owns gates; the host owns the judge (see `EMBEDDING.md` at
the repo root). Typical wiring:

1. Engine builds the app (a run, a plan wave, or a replay).
2. Host serves the build and runs `slimtoken-qa smoke` per surface.
3. `validate` runs on every generated source file before a run is
   called green — cheap (no model tokens) and catches the class of
   defect that shape checks miss.
4. The host reads `smoke.json` / flow reports and decides pass/fail.

## Development

The canonical home of this pack is `sidecars/qa/` in the slimtoken
repo. Keep zero runtime dependencies. After any change to `lib/`, run
the selftest:

```
node test/selftest.mjs
```

It must stay green (7 checks).
