# slimtoken-fuzz

In-process fuzz campaign sidecar for the SlimToken engine. This pack is a
side installable: the engine stays bare, and a host adds this pack when it
wants crash gates. Zero runtime dependencies. Node 22 or higher. Zero
model tokens.

The split, per the sidecar contract: the engine drafts fuzz inputs per
parser or schema in parallel (that is the token spend). This pack runs the
campaign in-process for free: it mutates the seed corpus with a
deterministic PRNG, calls the target's `fuzz(input)` entry point, and
reports every throw as a finding.

## What is in the pack

| Piece | Role |
|---|---|
| `run` (`lib/fuzz.mjs`) | Campaign runner: deterministic PRNG, mutations, target loading, exit codes. |
| `lib/report.mjs` | Judge output: `report.md`, `fuzz.json`, and `finding-N.txt` files. |

## Install

```
npm install slimtoken-fuzz
```

Or run it straight from a checkout:

```
node sidecars/fuzz/bin/slimtoken-fuzz.mjs help
```

## Use

Fuzz a target module against a seed corpus:

```
slimtoken-fuzz run parser.mjs --seeds corpus/ --cases 500 --out fuzz-out
```

The target module contract: an ESM module that exports a `fuzz` function,
sync or async:

```
export function fuzz(input) {
  // parse / validate / handle `input`; throw on a crash
}
```

Flags:

- `--seeds <dir>` — directory of seed files, read as UTF-8 text (required).
- `--cases N` — max cases to run (default 500).
- `--seconds S` — time budget in seconds (default 10). Cases or seconds,
  whichever comes first.
- `--seed R` — PRNG seed (default 1). The campaign is deterministic: same
  seed, same inputs, same findings.
- `--keep-going` — run the full campaign instead of stopping at the first
  finding. Exit code is 1 if any finding occurred.
- `--out DIR` — write `report.md`, `fuzz.json`, and one
  `finding-N.txt` per finding (the full crashing input).

Each case picks a random seed and applies 1-4 mutations: a byte flip,
a truncation, an insertion, or a splice of two seeds. The insertion
alphabet is boundary data: surrogate pairs, NUL, quotes, template
braces, backslash, the replacement character, and long runs of one
character.

Exit codes: 0 no findings, 1 finding(s), 2 usage error or bad target
module (import failure or a missing `fuzz` export).

## What this pack finds

Crashes: inputs that make the target throw. It does not find logic bugs.
A wrong answer that parses cleanly is not a finding; the host judge or a
test gate owns that class.

## Host wiring (SlimToken engine)

The engine owns gates; the host owns the judge (see `EMBEDDING.md` at the
repo root and `sidecars/README.md`). Typical wiring:

1. Engine drafts a seed corpus and boundary alphabet per parser or schema
   in one wave (one task per parser; that is the parallel win).
2. Host writes the corpus to a directory and runs
   `slimtoken-fuzz run <parser-target> --seeds <corpus-dir> --out fuzz-out`.
   One target per parser; targets are thin wrappers that call the parser.
3. The host reads `fuzz.json` / `report.md`, triages the crashing inputs
   (each is saved in full), and steers the next wave.

## Development

The canonical home of this pack is `sidecars/fuzz/` in the slimtoken
repo. Keep zero runtime dependencies. Keep the gate zero-token (pure
in-process execution, never a model call). Run `node test/selftest.mjs`
from the pack directory; the `selftest` npm script must stay green.
