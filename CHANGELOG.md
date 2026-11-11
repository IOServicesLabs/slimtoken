# Changelog

All notable changes to this project are documented here. The format is based
on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project
follows [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.2.0] - 2026-11-11

Engine hardening release. Paired A/B runs against v0.1.0 (plan replay,
two providers) show no regression on a 15-task build. A 48-task wave
with an artifact validator is 44% faster on DeepSeek. Hosts that build
`SchedulerConfig`, `TaskRecord`, `RunReport`, `FailoverConfig` or
`HealthRoutingConfig` with a full struct literal must add the new fields.

### Added

- **Anthropic provider preset** (`anthropic`): Claude models over the
  OpenAI-compatible layer at `https://api.anthropic.com/v1`, key from
  `ANTHROPIC_API_KEY`. The pinned knobs are conservative starting
  points, not measurements: effort `low`, planner schema off, burst cap
  8. Run a paired A/B before citing any number for this preset.
- **`llms.txt`** at the repo root: a compact integration brief an agent
  can include to wire SlimToken into any harness.
- **Failure classes** (`FailureClass`: `rate_limited`, `transport`,
  `content`, `permanent`). Each failed attempt has a class. The class
  is on failed `TaskRecord`s and on the trace failure events. Two new
  typed errors feed it: `PermanentError` (HTTP 400, 401, 403, 404, 405,
  413, 422) and `OverloadedError` (a 5xx reply with `Retry-After`).
- **Wasted-token accounting**: `TaskRecord.wasted_input_tokens`,
  `wasted_output_tokens` and the `RunReport.total_wasted_*` totals count
  the tokens of discarded attempts (truncated, unclosed fence, exhausted
  budget, validator rejection). They are not part of `total_*_tokens`.
- **Stream idle timeout** on the OpenAI-compatible client
  (`--stream-idle-timeout-secs`, default 120, 0 disables) and TCP
  keepalive.
- **First-token reports**: `report_first_token` lets a client tell the
  scheduler when the first token arrives. Two features use it.
  `--controller-early-ttft` (default on) lets the controller sample
  health at first token: -14% wall on a 48-task DeepSeek wave, neutral
  on Muse. `--first-token-watchdog FACTOR` (opt-in) abandons an attempt
  with no first token.
- **Run journal and resume**: each run writes `journal.jsonl` with the
  output of each succeeded task. `--resume-from` restores those tasks
  with no model call. Ctrl-C on `slimtoken run` cancels the wave and
  still writes the report.
- **`SharedLimits`**: one set of rate and in-flight budgets for all runs
  on one provider. `RunManager` gives it to each run.
- **`call_timeout`** on `FailoverConfig` and `HealthRoutingConfig`, so a
  provider that hangs is recorded as a failure.

### Changed

- The artifact validator runs in the task, not in the scheduler loop. A
  slow validator no longer stops admission and stream reads.
- Only transport failures (and 429s when the rate tuner is off) decrease
  the concurrency limit. Content failures do not.
- Failed attempts reach the controller when they occur, not when the
  task completes.
- A permanent error is not retried and not repaired.
- A repair after a transport or rate failure sends the original request
  again. Only a content failure adds the error to the prompt.
- Retry jitter is different for each request. A `Retry-After` hint is
  obeyed up to 120 s (was 30 s) and is read on 5xx replies also.
- Precomputed (static) tasks do not use start-rate or TPM budget and do
  not feed the controller.
- Each task runs on its own runtime task. A panic in a host client fails
  that task only.
- `RunManager`: each run has its own trace and journal file, and finished
  runs above 256 are removed from memory.

### Fixed

- A final `data: [DONE]` with no blank line after it is accepted.
- Validator and test shells are killed when their timeout expires.
- Cache primes, cache-warm canaries and the concurrency probe have a
  timeout.
- The failover recovery probe sends one request, not the whole wave.

## [0.1.0] - 2026-11-02

First tracked release. The engine is the result of four measured
improvement rounds; every default below ships with paired A/B evidence
on two providers (Muse and DeepSeek). Highlights:

### Added

- **Concurrency-first DAG runner**: plans are task DAGs; tasks release
  dependents as soon as their prerequisites complete. Failures are captured
  in the report and retried with a repair wave, not fatal.
- **Adaptive concurrency controller (AIMD)**: fast ramp until the first
  overload signal, then additive increase / multiplicative decrease.
  `max_concurrency` stays as the hard safety ceiling. An optional startup
  probe calibrates the cold-start width from a knee ladder.
- **Provider seam**: the `ModelClient` trait is the only integration point.
  Ships with an OpenAI-compatible client, a mock client, a recording /
  replay client, and a multi-provider fan-out client. Provider presets
  (muse, deepseek) fill measured settings for known backends.
- **Architect**: greenfield manifest generation (single-shot or split into
  fragments), export contracts, wiring, type ownership, density rules, and
  test quotas. A content-keyed manifest cache makes rebuilds of the same
  objective planning-free. A size router sends small deliverables out as
  one direct call.
- **Profiles**: stack profiles (python, nextjs, rust) shape plans per stack;
  a file and directory library lets hosts add stacks as data, not code.
- **Validation and repair**: artifact validators, a repair wave with error
  context, effort escalation on repair, and an optional in-run test command
  that converts suite failures into repairs.
- **Rate handling**: request pacing (RPM and TPM leaky buckets), retry with
  backoff, optional auto-tune from 429 history, and patience-bounded repair
  injection under a repair cap.
- **Prefix-cache support**: cache primes warm the server-side prefix cache
  before the wave; a stable `prompt_cache_key` pins requests to one cache
  shard across runs.
- **Host surfaces**: library API (`ModelClient`, `execute_plan`,
  `RunControl`), interactive CLI, REST server mode, run recording, trace
  output, steering, and resume. Host guides: `README.md`, `API.md`,
  `EMBEDDING.md`, `ENGINE.md`.

### Measured

Every default ships with A/B evidence on two providers (Muse and
DeepSeek). Examples: the adaptive ramp beats every fixed width; the
repair error tail diet cut input tokens per repair; the manifest cache
cut end-to-end time by about a third on a hit. Refuted ideas are recorded
with their evidence, not silently dropped.

### Notes

- Minimum supported Rust version (MSRV): 1.85, verified with
  `cargo +1.85 check --all-targets`.
- Licensed MIT OR Apache-2.0.

[Unreleased]: https://github.com/IOServicesLabs/slimtoken/compare/v0.2.0...HEAD
[0.2.0]: https://github.com/IOServicesLabs/slimtoken/releases/tag/v0.2.0
[0.1.0]: https://github.com/IOServicesLabs/slimtoken/releases/tag/v0.1.0
