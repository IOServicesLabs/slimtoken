# slimtoken-contract

Contract sidecar for the SlimToken engine. The engine drafts API tests
from a spec in a wave. This gate proves the spec itself resolves, for
free. It checks that JSON files parse. For OpenAPI specs it also checks
that info fields exist, paths exist, and every local `$ref` walks to a
real node. Zero runtime dependencies. Node 22 or higher. Zero tokens:
pure parsing, no model calls, no network.

## What is in the pack

| Piece | Role |
|---|---|
| `check` (`lib/check.mjs`) | The contract gate: JSON parse, OpenAPI shape, local `$ref` resolution. |
| `fixtures/` | Good and bad specs the selftest runs against. |
| `test/selftest.mjs` | Zero-dependency selftest; spawns the dispatcher. |

## Install

```
npm install slimtoken-contract
```

Or run it straight from a checkout:

```
node sidecars/contract/bin/slimtoken-contract.mjs help
```

## Use

Check one spec:

```
slimtoken-contract check api/openapi.json
```

Check a directory tree (dirs recurse; `node_modules` and `.git` are
skipped) and write the judge reports:

```
slimtoken-contract check specs/ --out contract-out
```

`--out DIR` writes two files. `report.md` is for a human or LLM judge.
`contract.json` is the machine twin: `{ ok, files: [{ file, status,
errors, warnings }], summary }`. Field names are stable; new fields
append at the end.

Per file, the gate prints one line:

- `OK` — the file parses and, if it is an OpenAPI spec, it resolves.
- `WARN SKIP` — the file is YAML, or it holds only warnings (for
  example an external `$ref`). Never a failure.
- `ERROR` — the file has a hard failure.

Rules the gate enforces on OpenAPI specs (a top-level `openapi` or
`swagger` string marks a spec):

- `info.title` and `info.version` must be present.
- `paths` must be present and an object.
- Every local `$ref` of the form `#/a/b/c` must resolve against the
  document. A missing segment is `dangling $ref: <ref>`, an ERROR.
- An external `$ref` (`other.json#/...` or a URL) is a WARN SKIP: it
  is not resolvable offline and never fails.

YAML files (`.yaml`, `.yml`) are WARN SKIP. This pack has zero runtime
dependencies, so it does not ship a YAML parser.

Exit codes: 0 pass, 1 findings, 2 usage error (no args, no checkable
files, unknown command).

## Host wiring (SlimToken engine)

The engine owns gates; the host owns the judge (see `EMBEDDING.md` at
the repo root and `sidecars/README.md`). Typical wiring:

1. Engine runs a contract-test drafting wave from an API spec.
2. Host runs `slimtoken-contract check` on the spec and any generated
   JSON artifacts.
3. The host reads `report.md` / `contract.json` and judges spec
   mismatches, then steers the next wave.

## Development

The canonical home of this pack is `sidecars/contract/` in the slimtoken
repo. The `selftest` script in `package.json` must stay green:

```
npm run selftest
```
