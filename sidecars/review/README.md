# slimtoken-review

Review sidecar for the SlimToken engine. A host adds this pack when it
wants to prove a model-drafted code review with zero tokens. The gate
runs real tools (cargo clippy or cargo check, and node --check) over a
source tree and reports what they say. It never calls a model. Zero
runtime dependencies. Node 22 or higher.

## What is in the pack

| Piece | Role |
|---|---|
| `lint` (`lib/lint.mjs`) | Zero-token lint gate over a source tree. |
| `fixtures/` | Good and bad trees for JavaScript and Rust, plus an empty tree. |
| `test/selftest.mjs` | Zero-dep self test; spawns the real dispatcher. |

## Install

```
npm install slimtoken-review
```

Or run it straight from a checkout:

```
node sidecars/review/bin/slimtoken-review.mjs help
```

## Use

```
slimtoken-review lint build/
slimtoken-review lint src/ --out review-out
```

The gate walks the directory and skips `node_modules`, `.git`, `target`,
`dist`, and `build`.

Rust stack: every crate with a `Cargo.toml` at its top gets
`cargo clippy --quiet` (300 s timeout). If clippy is not installed, the
gate falls back to `cargo check --quiet`. If cargo is not on `PATH`, the
gate prints a SKIP line with the reason. A non-zero exit is a finding;
the gate prints the first `error[...]` / `error:` lines, capped at 20.

JavaScript stack: every `.js`, `.mjs`, and `.cjs` file gets
`node --check` (a zero-token syntax lint, always present). A failing
file is a finding.

If the tree has no `Cargo.toml` and no JavaScript files, the gate prints
`WARN SKIP` with `no checkable files` and exits 0.

With `--out DIR`, the gate also writes two judge artifacts:

- `report.md` — a suites table plus the findings with the exact tool
  text, readable in one glance.
- `review.json` — the machine twin: `{ok, checks, summary}`, where each
  check is `{kind, target, status, detail?}`. Field names are stable;
  new fields append at the end.

Exit codes: 0 pass (no ERROR lines), 1 findings (at least one ERROR
line), 2 usage error (missing directory argument, missing or unreadable
directory).

## Host wiring (SlimToken engine)

The engine owns the wave; the pack owns the gate; the host owns the
judge (see `EMBEDDING.md` at the repo root and `sidecars/README.md`).
Typical wiring for a review pass:

1. Engine runs a parallel review-drafting wave over the changed files
   (one draft task per file, many drafts at once).
2. Host runs `slimtoken-review lint` on the changed tree. The gate
   proves the mechanical findings (compiler and syntax errors) with
   zero tokens.
3. Host judges the model-drafted review against the lint report: a
   claimed finding that the lint report does not support is suspect; a
   lint finding the draft missed is a gap. The host then steers the
   next wave through the REST routes or a new `--plan` run.

## Development

The canonical home of this pack is `sidecars/review/` in the slimtoken
repo. Keep zero runtime dependencies. Keep the gate zero-token: shell
out to a real tool, never a model. Run the self test after any change:

```
node test/selftest.mjs
```

It must print `selftest: N/N passed` and exit 0. The Rust fixture checks
run only when cargo is on `PATH`; without cargo the self test prints a
note and skips them.
