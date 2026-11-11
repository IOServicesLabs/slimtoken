# slimtoken-docs

Docs drift gate for the SlimToken engine. This pack is a side
installable: the engine stays bare, and a host adds this pack when it
wants a docs gate. Zero runtime dependencies. Node 22 or higher.

The engine can draft docs in a wave: many files at the same time. This
gate proves the mechanical part for free: links resolve, code fences
balance, files are readable. Accuracy stays with the host judge.

## What is in the pack

| Piece | Role |
|---|---|
| `check` (`lib/check.mjs`) | Docs gate: relative links, code fences, readability, front matter, API tables. Style lints report as WARN. |

## Install

```
npm install slimtoken-docs
```

Or run it straight from a checkout:

```
node sidecars/docs/bin/slimtoken-docs.mjs help
```

## Use

Check a docs tree:

```
slimtoken-docs check docs/
```

Check listed files and write a judge report:

```
slimtoken-docs check README.md guide/setup.md --out docs-report
```

Exit codes: 0 pass, 1 findings, 2 usage error.

The gate scans every `.md` file under the given dirs (or the listed
files directly; it skips `node_modules` and `.git`). HARD checks fail
the run:

- Relative links resolve. The target file must exist (the `#anchor`
  is stripped first). An anchor on a local `.md` target must match a
  heading's lowercased, space-to-dash form. A link with an empty target
  is an error. `http`, `https`, `mailto`, and `#`-only targets are
  skipped.
- Code fences balance. An odd fence-marker count means an unclosed
  fence.
- The file is UTF-8 readable and non-empty.
- Front matter is well formed. A `---` block on line 1 must close with
  `---` and hold one-level `key: value` pairs. A block with no closing
  `---`, or a line without a colon, is an error. A `README.md` with
  front matter needs `title` and `description` keys; `title` must be
  non-empty. A `README.md` with no front matter stays fine.
- API tables are well formed when the front matter sets `api: true`.
  Every table needs a header separator line, and every row must have
  the header row's cell count. Files without `api: true` skip this
  check.

WARN checks never fail the run; they print as WARN lines:

- Heading-level skips (h1 -> h3 with no h2).
- Sentences longer than 40 words.
- Trailing whitespace on lines.
- Front matter keys outside the known set (`title`, `description`,
  `api`).

With `--out DIR` the gate also writes two judge files: `report.md` (a
human reads it in one glance) and `docs.json` (a machine twin with
stable field names; new fields append at the end).

## Host wiring (SlimToken engine)

The engine owns the wave; the pack owns the gate; the host owns the
judge (see `EMBEDDING.md` at the repo root and `sidecars/README.md`).
Typical wiring:

1. Engine runs a docs-drafting wave (new pages, drift updates) and
   writes the files to the run output dir.
2. Host runs `slimtoken-docs check` on the drafted docs. The gate
   spends no model tokens: pure file parsing, no network.
3. The gate proves links and fences. The host reads `report.md` or
   `docs.json`, judges accuracy, and steers the next wave.

## Development

The canonical home of this pack is `sidecars/docs/` in the slimtoken
repo. Keep zero runtime dependencies. Keep the gate zero-token: pure
file parsing, never a model call. Run `npm run selftest` — the
dispatcher runs against `fixtures/good/` (must pass), `fixtures/bad/`
(must fail with link, anchor, and fence errors), and
`fixtures/empty/` (usage error).
