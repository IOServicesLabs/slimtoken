# slimtoken-security

Security sidecar for the SlimToken engine. The engine drafts an attack
corpus per endpoint (injection strings, auth bypass probes) in one
parallel wave. This pack executes the corpus against the running app
over plain HTTP replay. A finding is a failure: the server crashed, or
an injection marker came back in the response. Zero runtime
dependencies. Node 22 or higher. Zero tokens: pure HTTP replay, no model
calls.

## Ownership rule: only scan hosts you own

This pack sends attack inputs to a live app. Point it only at hosts you
own or hosts you have written permission to test. The default posture is
localhost: `scan` accepts base URLs on `localhost`, `127.0.0.1`, or
`[::1]` and refuses every other host with exit 2. To scan anything
else, pass `--allow-remote` — an explicit decision. The pack then prints
one line as a reminder: only scan hosts you own. There is no silent
remote path.

## What is in the pack

| Piece | Role |
|---|---|
| `scan` (`lib/scan.mjs`) | The replay gate: safe-target check, per-case HTTP replay, crash and marker findings, judge reports. |
| `lib/corpus.mjs` | Corpus loading and validation. Fail closed on a missing, empty, or malformed corpus. |
| `fixtures/` | Example corpora the selftest runs against. |
| `test/selftest.mjs` | Zero-dependency selftest; starts a local app in-process and spawns the dispatcher. |

## Install

```
npm install slimtoken-security
```

Or run it straight from a checkout:

```
node sidecars/security/bin/slimtoken-security.mjs help
```

## Use

Replay a corpus against a running app:

```
slimtoken-security scan http://localhost:3000 --corpus corpus/ --out sec-out
```

Flags:

- `--corpus <dir>` (required): directory of `.json` case files.
- `--allow-remote`: permit a non-localhost base URL (see the ownership
  rule above).
- `--timeout <ms>`: per-request timeout, default 10000.
- `--out <dir>`: write the judge reports (see below).

### Corpus file format

One case per `.json` file in the corpus directory:

```json
{
  "name": "reflect",
  "method": "POST",
  "path": "/echo",
  "headers": { "content-type": "text/plain" },
  "body": "<script>{{7*7}}</script>",
  "markers": ["49", "<script>"]
}
```

Fields:

- `name` (string, required): the case label. Printed per line and in the
  reports.
- `method` (string, optional): HTTP method. Default `GET`. A body on a
  GET or HEAD is dropped, because fetch rejects it.
- `path` (string, required): the request path. Must start with `/`. It
  is resolved against the base URL.
- `headers` (object, optional): extra request headers, name to value.
- `body` (string, optional): the raw request body. An injection string
  in a real corpus.
- `markers` (array of strings, optional): strings that must NOT appear
  in the response body. If one appears, the injection landed.

At least one of `body` / `markers` is recommended. A case with neither
can only prove reachability; it is a WARN, never a failure.

### Verdict per case

- `ERROR crash <name>: HTTP 5xx` — the server crashed on the input.
- `ERROR marker <name>: '<m>' reflected/executed` — a marker string came
  back in the response body.
- `OK <name>: HTTP <code>` — a 2xx, 3xx, or 4xx with no marker. The app
  blocked the input or was not affected. A 4xx is not a finding: a
  well-behaved app rejects attack input.
- `WARN UNREACHABLE <name>` — connection refused or timeout. A single
  unreachable case never fails the scan.

### Exit codes

- 0: no findings.
- 1: at least one finding (crash or marker).
- 2: usage error, remote host refused, or nothing was tested. A corpus
  where every case is unreachable tests nothing, so it fails closed.

### Judge reports (`--out DIR`)

Two files, per the pack contract:

- `report.md` — one table row per case, plus a summary and verdict. A
  human or LLM judges severity from it.
- `security.json` — the machine twin:
  `{ ok, cases: [{ name, status, statusCode?, markersFound? }], summary }`.
  Field names are stable; new fields append at the end.

The pack proves that an input landed or crashed the server. It does not
rank severity: reflected text is not always exploitable, and a crash on
one input may or may not matter. The host judges what a finding means
and what to fix first.

## Host wiring (SlimToken engine)

The engine owns the wave; the pack owns the gate; the host owns the
judge (see `sidecars/README.md`). Typical wiring:

1. Engine drafts an attack corpus per endpoint (injection, auth bypass)
   in one wave — many cases in parallel, which is where the speed is.
2. Host serves the built app and runs `slimtoken-security scan` against
   it with the drafted corpus.
3. The host reads `report.md` / `security.json`. A finding is a real
   vulnerability signal: the app reflected an injection marker or
   crashed on attack input. The host judges severity and steers the next
   wave — a repair task per finding, then a re-scan.

## Development

The canonical home of this pack is `sidecars/security/` in the slimtoken
repo. Keep zero runtime dependencies. Keep the gate zero-token (pure
HTTP replay). The `selftest` script in `package.json` must stay green:

```
npm run selftest
```
