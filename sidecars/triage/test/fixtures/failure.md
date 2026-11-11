# Gate failure report — fixture for the slimtoken-triage selftest

App: stockroom
Wave: 3

## smoke — FAIL

The smoke gate ran 4 checks. 1 failed.

- FAIL `build/index.html` references `assets/app-BadHash.js`; the file on
  disk is `assets/app-A1b2C3.js`. The build manifest and the HTML do not
  match.
- PASS `build/assets/app-A1b2C3.js` exists (184 KiB).
- PASS `build/index.html` has a `<title>`.
- PASS `build/index.html` viewport meta is present.

## flow — PASS

3 flows ran. 3 passed.

## verdict

Gate verdict: FAIL. The host must judge and steer the next wave.
