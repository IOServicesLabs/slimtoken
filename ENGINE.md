# SlimToken engine — host integration guide

SlimToken is a concurrency-first DAG engine for LLM coding work. One request
becomes a plan; the plan runs as a parallel wave of streams with shared
prefix caching, adaptive admission, and in-run validation. This guide is for
agent systems and coding harnesses that want the engine underneath them.

Goal: a host plugs in and gets the wave speedup with no core edits.

## Three ways to plug in

1. **Rust crate** (`slimtoken`, `src/lib.rs`). The full engine surface.
   Best for Rust hosts that want in-process control.
2. **REST server** (`slimtoken serve`, see `API.md`). Run manager with
   submit / steer / cancel / report endpoints. Best for hosts in any other
   language.
3. **CLI** (`slimtoken run ...`). Batch scripts and benches.

## Module map

| Module | What a host uses it for |
|---|---|
| `dag` | `Plan` / `Task`: build your own task graph if the architect is not the right planner. |
| `client` | `ModelClient` trait: point the engine at any OpenAI-compatible endpoint, or implement the trait for a custom provider. `OpenAiClient` and `MockClient` are the built-ins. |
| `scheduler` | `execute_plan` / `execute_plan_with_control`: run a plan, get a `RunReport` (tokens, TTFT, cache, per-task records). |
| `controller` | `ControllerConfig`: adaptive admission (AIMD) knobs. |
| `control` | `RunControl`: steer or cancel a live run. |
| `runner` | `RunManager`: shared multi-run state behind the REST API. |
| `architect` | `generate_manifest` + `plan_from_manifest`: contract-first planner (one parallel wave, sharded suites). `ArchitectOptions` carries profiles and feature flags. |
| `planner` | `generate_plan_with_steering`: lighter planner for non-architect flows. |
| `profile` | `Profile`: everything stack-specific (prompt rules, contract syntax, validators). |
| `reconcile` | Post-wave checks and repairs (edit-block reconcile, tool checks). |
| `api` | Axum routers for the REST surface. |

## Extension points

- **Custom provider** — implement `ModelClient` (`async fn complete(CompletionRequest) -> Result<Completion>`) and pass `Arc<dyn ModelClient>` to the scheduler. Streaming, usage, and cache fields on `Completion` feed the report.
- **Custom stack profile** — a `Profile` is plain data plus plain functions, so a Rust host declares its own `static Profile` and passes it through `ArchitectOptions`. Built-ins: `PYTHON`, `NEXTJS`.
- **Custom validation** — `Task.validate_artifact` plus a `Validator` (shell command, non-zero exit = failure with stderr excerpt) gates both the main loop and repairs.
- **Custom planning** — skip the architect, build `Plan` yourself, keep the scheduler / controller / cache behavior.

## Host contract (what the engine promises)

- Prompt prefixes that are byte-identical across tasks hit the provider
  prefix cache; the engine primes them on purpose.
- Failed artifacts become repairs with the error in context; unrelated
  branches keep running.
- `RunReport` is the single source of truth: wall, per-task TTFT, input /
  output / reasoning / cached tokens, concurrency curve.

## Known gaps

- Profiles are now file-loadable (`--profile-file`): data fields
  override a built-in `base`, behavior hooks are inherited. The returned
  profile is leased to 'static — load once at startup.
- No server-sent event stream for live task progress; hosts poll
  `GET /runs/{id}/tasks` today.
- Multi-provider runs (planner on one provider, workers on another) are
  manual; per-role provider routing is future work.
