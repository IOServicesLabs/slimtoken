# Changelog

All notable changes to this project are documented here. The format is based
on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project
follows [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.3.0] - 2026-11-20

Planning pipeline, staged waves, check and repair, and the Showdown
benchmark. The engine can now plan an app as interface contracts, write
it in stages of parallel streams, check it with a real build and repair
the files that the check names. Every new step is an option that is off
by default: a host that sets none of them gets the plans and waves of
0.2.0.

### Measured

`bench/showdown` builds one complete Next.js app from a plain-text brief
two ways with the same model (DeepSeek): one stream that writes every
file, and SlimToken. SlimToken's time includes its plan, every wave and
the whole check-and-repair step. "Pass" means that install, build, test
and pages all pass. One run is one sample; provider speed changes from
minute to minute.

| Point | SlimToken wall time | Delivery checks | Against the single stream |
|---|---|---|---|
| 0.2.0 engine, first benchmark runs (14 Nov, Taskflow, 3 runs) | 109.9, 48.7 and 71.3 s | 2 of 3 pass | level to 1.8x (single stream 82 to 87 s) |
| Planner pipeline, staged waves and repair (14 Nov, Taskflow, 8 runs) | median 29.7 s | 8 of 8 pass | about 2.9x |
| This release, default set (20 Nov, five apps of which four are held-out, 9 runs) | median 37.5 s, range 35.1 to 53.8 s | 8 of 9 pass | about 2.2x (estimate: the single stream was not rerun for each) |

Full races, both sides run together:

| Date | App | Single stream | SlimToken | Speedup |
|---|---|---|---|---|
| 2026-11-14 | Taskflow | 85.4 s, pass | 26.4 s, pass | 3.24x |
| 2026-11-14 | Taskflow | 82.5 s, pass | 38.6 s, pass | 2.14x |
| 2026-11-14 | Taskflow | 81.4 s, test fails | 28.5 s, pass | 2.86x |
| 2026-11-20 | Taskflow | 92.6 s, build fails | 34.0 s, pass | 2.72x |
| 2026-11-20 | Fleet (held-out) | 108.2 s, build fails | 44.7 s, pass | 2.42x |

Blind judge, delivered score out of 100 for the SlimToken app
(2026-11-20): Taskflow 95.2 and 79.7, Ledgerly 92.5. In the first of
these the pinned single-stream reference app scored 87.9.

SlimToken is faster, not cheaper: it sends many more input tokens than
one request (about 660,000 against about 460 in the two races of 20
November). The provider served about 30% of them from its prompt cache
(203,392 of 657,026 and 198,656 of 665,191). The method, the fairness rules and the
limits are in [bench/showdown/BENCHMARK.md](bench/showdown/BENCHMARK.md).

### Upgrading

Hosts that build `ProviderPreset` with a full struct literal must add
the three new fields. A host `static Profile` literal must add
`staging: NO_STAGING` and `tidy_manifest` (a `fn(&mut Manifest)`; a
no-op keeps today's plans), and a full `PlanOptions` literal must add
the twelve new fields (or end with `..PlanOptions::default()`). A full
`SchedulerConfig` literal must add `trace_append` (`false` keeps
today's trace). A full
`ProfileFile` literal must add the ten new staging fields. A full
`Staging` literal must add `split`, `aggregate` and `quality` (`&[]`,
`profile::no_aggregate` and `profile::NO_QUALITY` keep today's plans). A full
`RunRequest` literal must add `profile` and `plan_options` (or end
with `..RunRequest::default()`). A host `static Profile` literal must
also add `repair: NO_REPAIR_HOOKS` (`profile::NO_REPAIR_HOOKS`; it
changes nothing unless the host calls `repair::check_and_repair`). A
full `RepairHooks` literal must add `guard` (`profile::NO_GUARD_HOOKS`
keeps today's repairs; a full `GuardHooks` literal has `shared_module`), and a full `RepairPolicy` literal `guard`
(`false`).

### Added

- **`pipeline::build(objective, client, BuildOptions) -> BuildOutcome`**:
  one call that plans, runs the wave, writes the project files and, with
  a checker, checks and repairs them. `BuildOptions::for_stack(profile,
  preset)` sets the options that a profile and a provider preset
  recommend. `BuildHooks` let a host act between the steps.
- **Planning as contracts** (`architect::plan_project`,
  `ArchitectOptions`): a skeleton call, then export fragments that
  decode side by side. Options, all off by default: `types_first` and
  `layered` (the type files are planned first and every later fragment
  sees them), `core_whole`, `contract` (contract check and repair),
  `references` (plan references that no file planned), `review` (one
  cross-file review call), `plan_pipe` (fragments start while the
  skeleton still streams), `types_from_paths`, `skel_compact`,
  `split_pages` and `early_manifest`.
- **Staged waves** (`Profile.staging`, `PlanOptions.staged`): types,
  then modules, then pages, components and tests, each stage in
  parallel. With it: `one_type_owner`, `declare_types`,
  `types_from_plan`, `min_test_shards`, `split`, `shell_rule` and
  `tests_see_code`. `ProviderPreset::apply_plan` turns on what a preset
  recommends. `profile::glob_match` matches the layer globs.
- **Check and repair** (`check` and `repair` modules): an async
  `Checker` trait, `repair::check_and_repair` and `RepairPolicy`
  (`max_rounds`, `guard`, `joint_edits`, `test_edits`, `contract_side`).
  Each file that the check names is repaired by its own stream with its
  problems and the files it imports. `Profile.repair` holds a profile's
  repair hooks. CLI: `--check-cmd` (the old name `--test-cmd` still
  works) and `--repair-rounds N`.
- **Scheduler options**, all off by default: `continue_truncated` (an
  answer cut at the output limit is continued), `file_min_tokens`,
  `continue_runaway_tokens` and `trace_append`. A warning is logged when
  a dependency output is cut.
- **Request options**, all off by default: hedged requests
  (`hedge::HedgeClient`, CLI `--hedge-after-ms` and `--hedge-max`),
  connection prewarm (`--prewarm N`), an HTTP/2 offer (`--http2`),
  in-wave repair (`--in-wave-repair`), and the `speedrace` and
  `pagefork` modules for hosts that want them.
- **Quality options for generated apps**, all off by default: the
  `pagelocal` module (pages own their features), the `semantics` module
  (app conventions for money, dates, state and forms), the `objective`
  module (the brief's pages and quality bar reach each file), the
  `stylesheet` module (one style sheet after the wave) and
  `profile::Quality`.
- **`project` module**: file extraction, `write_project`,
  `apply_edit_blocks` and `EDIT_FORMAT` for search-and-replace repairs.
- `ProviderPreset` fields `cold_hint_cap`, `start_percent` and
  `first_token_watchdog`; `RunRequest.profile` and `RunRequest.plan_options`;
  `PlanOptions` derives `Serialize`, `Deserialize`, `PartialEq` and `Eq`;
  `scheduler::render_task_prompt` and `scheduler::run_shell_piped` are
  public.
- **`bench/showdown`** (a separate crate): a terminal benchmark that
  races one model stream against SlimToken on the same brief.
  - `run`, `replay` and `demo` open with the prompt in full, the model
    and a countdown before any request is sent (`--intro-secs N`).
  - `launch` and `run --launch` start both apps; on Windows with Edge or
    Chrome they open as app windows on the left and right halves of the
    screen (`SHOWDOWN_TILE=0` turns this off).
  - The result panel and `results.json` (`cached_input_tokens`) show
    how many input tokens of each side were prompt-cache hits.
  - `judge` scores both apps of a finished run blind: requirement
    coverage, working defects with verified quotes, a pairwise verdict
    and a browser smoke test.
  - Each pipeline step has a switch (`SHOWDOWN_ON`); with it unset the
    benchmark runs its recommended set.
  - Brief sets: Taskflow and Ledgerly, and eight held-out apps.

### Changed

- The CLI's `run` and `RunManager::execute` both go through
  `pipeline::build`. Plans, reports, files and printed lines are the
  same. With `--out-dir` and `--repair-rounds`, the wave's files are
  also written before the check.
- The DeepSeek preset starts the wave wide.
- The rendered README of a generated project is a real README.
- Next.js profile: staging layers and rules, client pages are marked by
  the `tidy` hook, in-app navigation uses `<Link>`, and the test rule
  asserts only what holds for any seed data when the seed's code is not
  attached.

### Fixed

- Scheduler: the first-token watchdog backs off per task. Each attempt
  it abandons doubles that task's limit, and after two it leaves the
  task alone.
- CLI: the repair after the wave no longer truncates
  `<output>/journal.jsonl`, so `--resume-from` finds the wave's journal.
- An objective that asks for a test count near `u64::MAX` no longer
  overflows the test quota.
- A skeleton that lists a test file under `files` no longer writes it
  beside the profile's tests.
- `project::apply_edit_blocks` places nothing when a block was not
  closed, instead of writing the marks into the file.
- A joint repair's output limit follows the size of its files.
- The wrapper clients (`FailoverClient`, `HedgeClient`, `FanOutClient`)
  forward `ModelClient::continue_completion` to their inner client.
- `hedge`: a copy that reports its first token and fails in the same
  poll ends the race with its error at once, and a rate-limit or
  overload error pauses hedging for its Retry-After.

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

[Unreleased]: https://github.com/IOServicesLabs/slimtoken/compare/v0.3.0...HEAD
[0.3.0]: https://github.com/IOServicesLabs/slimtoken/releases/tag/v0.3.0
[0.2.0]: https://github.com/IOServicesLabs/slimtoken/releases/tag/v0.2.0
[0.1.0]: https://github.com/IOServicesLabs/slimtoken/releases/tag/v0.1.0
