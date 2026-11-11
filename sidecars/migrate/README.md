# slimtoken-migrate

Migration gate sidecar for the SlimToken engine. A migration wave
rewrites many files at once. The wave is only good if every file still
compiles. This gate proves that for free: it shells out to the
language's own compiler or parser, so it spends zero model tokens. A
host adds this pack when it runs per-file migration waves and needs a
fast, strict pass/fail line before it judges semantic drift.

## What is in the pack

| Piece | Role |
|---|---|
| `check` (`lib/check.mjs`) | The migration gate: syntax-checks every source file, then runs the manifest checks it detects. |
| `fixtures/` | Tiny broken and clean crates and scripts the selftest runs against. |
| `test/selftest.mjs` | Zero-dep self test that spawns the dispatcher on the fixtures. |

## What the gate does

`slimtoken-migrate check <dir>` runs two kinds of checks.

1. Syntax (always). Every `.py` file under `<dir>` goes to
   `python -m py_compile`. Every `.js`, `.mjs`, and `.cjs` file goes to
   `node --check`. Any failure is an ERROR with the tool's error line.
   `node_modules`, `.git`, `target`, and other build output dirs are
   not scanned.
2. Manifest (only when present at the top level of `<dir>`). A
   `Cargo.toml` runs `cargo check --quiet` (300s timeout). If cargo is
   not on PATH, the check is a WARN SKIP with the reason. A
   `package.json` runs `npm run build --if-present` (600s timeout) only
   when you pass `--build`; without `--build` it is a WARN SKIP with
   `build skipped (pass --build)`. A non-zero exit is an ERROR.

Every check prints one line: `OK`, `WARN SKIP`, or `ERROR`. The run
ends with a summary line. Exit is 0 when no ERROR line exists, 1 when
at least one exists, and 2 for a usage error (no dir, unreadable dir,
unknown flag).

## Install

```
npm install slimtoken-migrate
```

Or run it straight from a checkout:

```
node sidecars/migrate/bin/slimtoken-migrate.mjs help
```

## Use

```
slimtoken-migrate check build/
slimtoken-migrate check repo/ --build --out reports/
```

The `--build` flag runs `npm run build --if-present` when the target
dir has a `package.json`. Use it when the migrated tree must also
build. Skip it for a fast syntax-only pass on a tree you do not want
to build.

The `--out DIR` flag also writes two reports:

- `report.md` — a human-readable list a reviewer can read in one
  glance.
- `migrate.json` — the machine twin: `{ ok, checks, summary }` with
  stable field names. New fields append at the end.

Exit codes: 0 pass, 1 findings (at least one ERROR), 2 usage error.

## Host wiring (SlimToken engine)

The engine owns the wave; the pack owns the gate; the host owns the
judge (see `EMBEDDING.md` at the repo root and `sidecars/README.md`).
Typical wiring for a migration:

1. The host's profile rules describe the migration (for example: move
   this module from callback style to async style, file by file).
2. The engine runs a per-file migration wave from those rules.
3. The host runs `slimtoken-migrate check` on the migrated tree. This
   is the zero-token proof: every file still compiles, and the crate
   or package still builds.
4. The host reads `report.md` (or `migrate.json`) and judges semantic
   drift — did the wave keep behavior, not just syntax? That judgment
   may use a model; this pack never does.

Stack rules stay in the host profile, never in this pack. This pack
knows how to prove compilation, not what your migration means.

## Development

The canonical home of this pack is `sidecars/migrate/` in the slimtoken
repo. Zero runtime dependencies; Node 22 or higher. Keep the gate
zero-token: shell out to a real tool, never call a model. The
`selftest` script must stay green:

```
npm run selftest
```

It spawns the dispatcher against the fixtures in `fixtures/` and
asserts the exit codes, including the cargo cases when cargo is on
PATH. The fixtures stay clean: the selftest sends cargo's target dir
to a temp dir and removes any `Cargo.lock` the check creates.
