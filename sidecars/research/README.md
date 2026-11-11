# slimtoken-research

Research sidecar for the SlimToken engine. The engine runs N researcher
tasks in one wave, one per sub-question; the wave spends model tokens.
This pack supplies the plan builder, the report collector, and the
citation gate. The gate fetches every cited source and checks that every
load-bearing claim appears in it. The gate costs zero model tokens: it
uses fetch and string matching only. Zero runtime dependencies. Node 22
or higher.

Gate sentence: "Fetch every citation; every load-bearing claim must
appear in its source."

## What is in the pack

| Piece | Role |
|---|---|
| `brief` (`lib/brief.mjs`) | Builds an engine plan: one researcher task per sub-question. The prompt template demands citation discipline. |
| `collect` (`lib/collect.mjs`) | Merges the run dir's per-task briefs into one report with a deduped `## Sources` section. |
| `verify` (`lib/verify.mjs`) | The gate: fetches every citation and checks each load-bearing claim against its source. |
| `test/selftest.mjs` | Zero-dependency selftest; builds fixtures in-process against a local server. |

## Install

```
npm install slimtoken-research
```

Or run it straight from a checkout:

```
node sidecars/research/bin/slimtoken-research.mjs help
```

## Use

Build a plan from a questions file (one research sub-question per line;
blank lines and `#` comments are ignored):

```
slimtoken-research brief questions.txt --out plan.json
```

The plan holds one task per question: `{ tasks: [{ id: "research-<n>",
prompt }] }`. The researcher prompt demands several things. Search and
read primary sources. Write a 10-line brief. Every load-bearing
sentence carries an inline citation
`[claim text](https://example.com/source)`. Mark claims that
independent sources agree on with `(consensus)`. Mark restated claims
with `(paraphrase)`. Never cite a source you did not open.

Merge the briefs the engine wrote into one report:

```
slimtoken-research collect run-output/ --out report.md
```

The collector reads `report.json` task outputs (succeeded tasks) and any
top-level `*.md` brief files in the run dir. It writes one section per
brief and a `## Sources` section with every citation deduped by URL.

Run the gate on the report:

```
slimtoken-research verify report.md --out verify-out
```

Per claim the gate prints one line: `VERIFIED`, `PARTIAL` (a
`(paraphrase)` claim that passes on token overlap >= 0.6), `UNVERIFIED`,
or `UNFETCHABLE`. Claim rules:

- A claim is load-bearing unless it is marked `(opinion)`. Opinion
  claims are skipped.
- A `(consensus)` claim must verify in at least 2 distinct hostnames.
- A claim verifies when its text appears in the fetched page (the page
  is stripped to text, whitespace-normalized, lowercased). A
  `(paraphrase)` claim may instead pass on token overlap >= 0.6.

Fetch rules:

- HTTP 404 or 410 on a load-bearing citation is an ERROR: `citation
  does not exist`.
- Any other fetch failure (DNS, timeout, other non-OK status) is a WARN
  `UNFETCHABLE`. It never fails the gate alone.
- A fetched page that does not contain the claim is an ERROR: `claim
  not found in source`.

`--out DIR` writes two judge files. `report.md` is for a human or LLM
judge. `research.json` is the machine twin: `{ ok, claims: [{ claim,
url, status, hosts? }], summary }`. Field names are stable; new fields
append at the end. `hosts` is present on consensus claims.

Exit codes: 0 pass (warnings allowed), 1 findings (at least one ERROR),
2 usage error (bad args, missing file, no briefs).

## Host wiring (SlimToken engine)

The engine owns the wave; the pack owns the gate; the host owns the
judge (see `EMBEDDING.md` at the repo root and `sidecars/README.md`).
The full loop:

1. Host runs `slimtoken-research brief questions.txt --out plan.json`.
2. Host runs the engine on the plan: `slimtoken run --plan plan.json`.
   The engine runs the N researcher tasks in one wave and writes the
   run output dir.
3. Host runs `slimtoken-research collect <run-output-dir> --out
   report.md`.
4. Host runs `slimtoken-research verify report.md --out verify-out`.
5. The host reads `report.md` / `research.json` and judges: which
   unverifiable claims to drop, which sources to replace, then steers
   the next wave.

```
brief -> slimtoken run --plan -> collect -> verify -> host judges
```

## Limits

State these limits plainly to any user of this pack:

- The gate proves the source says what the brief claims. It does not
  prove the source is true, current, or unbiased.
- The matching is mechanical: substring, or token overlap for
  `(paraphrase)` claims. A claim worded very differently from its
  source may read as UNVERIFIED even when the source supports it.
- Research that cannot cite is out of scope. Opinion and taste work has
  no source to fetch, so the gate can only skip it.
- A `(consensus)` mark means the brief found two hostnames that both
  contain the claim text. It is not a check of independence between
  the sources.
- Paywalled and rate-limited pages read as UNFETCHABLE, not as
  failures. A report full of unfetchable citations passes the gate with
  warnings; the host must notice.

The fetch layer is a thin seam. A fast-read backend (searchio is the
candidate) can sit behind it later without a pack change: the gate only
needs page text per URL.

## Development

The canonical home of this pack is `sidecars/research/` in the slimtoken
repo. Keep zero runtime dependencies. Keep the gate zero-token (fetch
and string matching only; no model calls). The `selftest` script in
`package.json` must stay green:

```
npm run selftest
```

The selftest builds its fixtures in-process: a local `node:http` server
plays the citation sources, and fixture reports are written to a temp
directory. No fixtures are needed on disk. One Windows note: the
selftest spawns the dispatcher with async `spawn`, not `spawnSync`,
because this process hosts the fixture server and `spawnSync` would
freeze the event loop the children fetch against.
