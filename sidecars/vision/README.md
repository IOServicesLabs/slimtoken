# slimtoken-vision

Judge the look of engine-built UIs. The qa pack smoke gate checks
function: the page loads, the surfaces answer, the console is clean. It
correctly refuses to judge look. This pack is the only way to judge
look: it sends each qa-pack screenshot to a vision model with the
host's rubric and writes a score report.

This pack is a judge pack. It spends model tokens by design. Read the
COST section before you run it.

## COST (read first)

- **One vision call per screenshot.** No batching. Cost per run =
  number of images x 1 call each. A run over 20 screenshots is 20 calls.
- **Keys come only from the environment.** No endpoint and no key ships
  in this pack. Set `VISION_BASE_URL` (OpenAI-compatible base, the pack
  POSTs to `$VISION_BASE_URL/chat/completions`), `VISION_API_KEY`, and
  `VISION_MODEL`. Missing any one of them is an exit 2 that names the
  variable.
- **Budget caps.** `--max-images N` (default 20) judges at most N
  images; the rest are not judged and the run says so with a WARN.
  `--max-image-bytes N` (default 1500000) SKIPS files over the cap
  with a WARN, never silently.
- **Retry policy.** One automatic retry on HTTP 429/5xx with a 2s
  backoff, then the image is WARN SKIP. There is no other retry.
- **A judge failure is never a gate failure.** A skipped image never
  fails the run by itself. The run fails (exit 1) only when nothing was
  judged at all.

## What is in the pack

| Piece | Role |
|---|---|
| `score` (`lib/score.mjs`) | Collects PNGs, applies the caps, calls the judge per image, aggregates. |
| `judge` (`lib/judge.mjs`) | One OpenAI-compatible vision call per image; fence stripping; strict JSON parse. |
| `collect` (`lib/collect.mjs`) | Recursive PNG collect, sorted; PNG signature check before any call is spent. |
| `report` (`lib/report.mjs`) | `report.md` plus the `vision.json` twin. |

## Install

```
npm install slimtoken-vision
```

Or run it straight from a checkout:

```
node sidecars/vision/bin/slimtoken-vision.mjs help
```

## Use

Point it at the qa pack's smoke output directory (or any directory of
PNG screenshots):

```
slimtoken-qa smoke --out runs/qa/smoke ...
slimtoken-vision score runs/qa/smoke --rubric brand.txt
slimtoken-vision score runs/qa/smoke --rubric brand.txt --out runs/vision
```

The rubric file is the host's aesthetic contract. Write it per project.
It is plain scoring instructions, for example: "Score brand adherence
1-10; flag off-palette colors; check spacing rhythm." The pack appends
the required answer shape (`{score, flags, note}` JSON) to it.

Per image, the run prints one line:

```
OK <file> score=7 flags=1
WARN SKIP <file>: <why>
```

and ends with a summary line (scored, skipped, mean score). With
`--out DIR` it also writes `report.md` (a glance table) and
`vision.json`:

```
{ ok, images: [{ file, score, flags, note? }], summary: { scored, skipped, meanScore } }
```

## Exit codes

- **0** — at least one image scored.
- **1** — zero images scored. The run judged nothing: every image was
  skipped (corrupt, oversize, cap, unparseable judge answer) or the
  judge was unreachable. This exit exists on purpose: a vision outage
  must not read as a pass. The host must treat 1 as "no look verdict
  was produced", not as "the UI looks fine".
- **2** — usage error: bad args, missing `VISION_BASE_URL` /
  `VISION_API_KEY` / `VISION_MODEL`, rubric unreadable or empty, shots
  dir unreadable, no PNG images found.

## Host wiring (SlimToken engine)

The engine owns the wave; the pack owns the gate; the host owns the
judge (see `sidecars/README.md`). The qa smoke gate holds a detection
boundary for function and refuses to judge look. This pack closes that
gap with a capped, explicit token spend:

1. Engine runs a wave and builds the app; the qa pack smokes it and
   writes screenshots to its `--out` dir.
2. Host runs `slimtoken-vision score <qa-out> --rubric <file>`.
3. The host reads `report.md` / `vision.json` and decides whether the
   look is good enough to steer the next wave.

Set the three `VISION_*` variables in the host environment. Rotate the
key there; this pack never sees a key file.

## Development

Copy the shape from `sidecars/_template/` (see `sidecars/README.md`).
Zero runtime dependencies; Node 22+. The transport is `node:http`, not
`fetch`: undici keeps a pooled keep-alive socket alive past the last
response, and `process.exit` on Windows can race that closing handle
(libuv `UV_HANDLE_CLOSING` assertion). `agent: false` closes the socket
with the response.

The selftest runs a mock vision server in-process and spawns the real
CLI with async spawns. Do not switch it to `spawnSync`: the sync call
freezes this process, so the in-process mock server cannot answer the
child and the run deadlocks (this trap hit sibling packs).

```
npm run selftest
```
