# slimtoken-types

Type binding sidecar for the SlimToken engine. This pack is a side
installable: the engine stays bare, and a host adds this pack when it
drafts type bindings from a spec and wants a zero-token proof that the
bindings are complete and compile. Zero runtime dependencies. Node 22
or higher.

## What is in the pack

| Piece | Role |
|---|---|
| `check` (`lib/check.mjs`) | Orchestrates the gate: spec parse, schema set, binding presence, compile, report. |
| `lib/spec.mjs` | Spec loading, schema-set extraction (OpenAPI or plain schema map), and the local `$ref` walk. |
| `lib/bindings.mjs` | Binding discovery, language detection, export-name checks, and the tsc / py_compile runs. |

The binding-presence check (every schema name appears as an exported
type) runs with zero toolchain: it reads the binding files and matches
export lines. The compile check runs only when the tool is on PATH and
is WARN SKIP (never a failure) when it is not.

## Install

```
npm install slimtoken-types
```

Or run it straight from a checkout:

```
node sidecars/types/bin/slimtoken-types.mjs help
```

## Use

```
slimtoken-types check api/openapi.json bindings/
slimtoken-types check schemas.json bindings/ --lang ts --out types-out
```

The spec file is JSON. It can be an OpenAPI document (the schema set is
the keys of `components.schemas`, or `definitions` for Swagger 2.0) or
a plain object whose values look like JSON schemas, where the keys of
the schema-like values are the schema set. A valid spec with zero
schemas is WARN SKIP and passes.

The bindings dir holds `.ts`/`.d.ts` files, `.py` files, or both.
`--lang auto` (the default) checks every language it finds in the dir;
`--lang ts` or `--lang py` forces one. Language detection fails (exit 2)
when the dir has no binding files for the requested language.

Per schema name:

- TS: a line `export interface <Name>` / `export type <Name>` /
  `export class <Name>` / `export enum <Name>` in some `.ts`/`.d.ts`
  file.
- Python: `class <Name>(` or `class <Name>:` or `<Name> =` at the start
  of a line.

Compile checks:

- TS: `tsc --noEmit -p <bindings-dir>` when a `tsconfig.json` is there,
  else `tsc --noEmit <all .ts files>`. `tsc` not on PATH is WARN SKIP.
- Python: `python -m py_compile` each `.py` (probes `python`, then
  `py`). No interpreter on PATH is WARN SKIP.

`--out DIR` writes `report.md` (one-glance table) and `types.json`
(`{ok, schemas, checks, summary}`) for the host judge.

Exit codes: 0 pass (WARN SKIP allowed), 1 findings, 2 usage error
(bad args, unreadable or unparseable spec, empty bindings dir, no
detectable language).

## Host wiring (SlimToken engine)

The engine owns gates; the host owns the judge (see `EMBEDDING.md` at
the repo root and `sidecars/README.md`). Typical wiring:

1. Engine drafts type bindings from the spec, one draft task per schema,
   in one wave.
2. Host runs `slimtoken-types check <spec> <bindings-dir>` on the wave
   output. The gate proves presence (every schema has a binding) and
   compilation (tsc / py_compile). Both are free: no model tokens.
3. The host reads `report.md` / `types.json` and judges semantic drift
   (does the binding mean the same as the schema). That judgment is a
   host concern, not a gate concern.

A dangling local `$ref` in the spec is a hard ERROR: bindings drafted
from a spec that does not resolve cannot be trusted.

## Development

The canonical home of this pack is `sidecars/types/` in the slimtoken
repo. Keep zero runtime dependencies. Keep the gate zero-token (parse
files, shell out to tsc / python). The `selftest` script in
`package.json` must stay green: `node test/selftest.mjs`. The tsc
fixture case (`fixtures/ts-tsc/`) is asserted only when `tsc` is on
PATH; on a host without tsc the case prints SKIP and is never faked.
