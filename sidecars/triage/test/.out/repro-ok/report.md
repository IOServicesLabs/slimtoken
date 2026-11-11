# Triage report

Verdict: PASS — 1/3 candidate(s) confirmed.

## Confirmed

- **exit-nonzero** — the repro command fails as expected
  - repro: `node -e "process.exit(1)"` (exit 1)

## Rejected

- **exit-zero** — repro exited 0 (hypothesis not demonstrated)
- **no-such-command** — command not found: 'definitely-not-a-command-xyz' is not recognized as an internal or external command,
