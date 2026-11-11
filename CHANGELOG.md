# Changelog

All notable changes to this project are documented here. The format is based
on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project
follows [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- **Anthropic provider preset** (`anthropic`): Claude models over the
  OpenAI-compatible layer at `https://api.anthropic.com/v1`, key from
  `ANTHROPIC_API_KEY`. The pinned knobs are conservative starting
  points, not measurements: effort `low`, planner schema off, burst cap
  8. Run a paired A/B before citing any number for this preset.
- **`llms.txt`** at the repo root: a compact integration brief an agent
  can include to wire SlimToken into any harness.

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

[Unreleased]: https://github.com/IOServicesLabs/slimtoken/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/IOServicesLabs/slimtoken/releases/tag/v0.1.0
