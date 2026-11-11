//! SlimToken: a concurrency-first DAG harness for LLM coding and agent
//! workloads. A host hands over an objective or a ready-made task graph; the
//! engine decomposes, schedules, retries, repairs, and reports — at wide
//! concurrency with adaptive AIMD control, TPM/RPM pacing, and per-task
//! telemetry.
//!
//! # The provider seam
//!
//! Everything model-specific sits behind one trait, [`ModelClient`]. A host
//! embeds the engine by implementing this trait for its provider (or reusing
//! the shipped OpenAI-compatible client) and passing `Arc<dyn ModelClient>`
//! to [`RunManager::new`]. Core scheduling knows nothing about providers; the
//! seam is the only integration point.
//!
//! ```rust
//! use std::time::Duration;
//! use slimtoken::{async_trait, Completion, CompletionRequest, ModelClient};
//!
//! struct EchoClient;
//!
//! #[async_trait]
//! impl ModelClient for EchoClient {
//!     async fn complete(&self, request: CompletionRequest) -> anyhow::Result<Completion> {
//!         Ok(Completion {
//!             text: request.user,
//!             provider: Some("echo".to_string()),
//!             input_tokens: 0,
//!             output_tokens: 0,
//!             reasoning_tokens: None,
//!             cached_input_tokens: None,
//!             finish_reason: Some("stop".to_string()),
//!             usage_is_estimated: false,
//!             elapsed: Duration::ZERO,
//!             time_to_first_token: None,
//!         })
//!     }
//! }
//! ```
//!
//! # Error taxonomy
//!
//! [`ModelClient::complete`] returns `anyhow::Result<Completion>`. The
//! engine recognizes two typed payloads inside the error; everything else is
//! a generic failure:
//!
//! - [`RateLimitedError`] — the provider hit a rate wall (HTTP 429). Carry
//!   the server's Retry-After hint in `retry_after` and the scheduler waits
//!   out the window instead of speculative retries. With
//!   [`SchedulerConfig::rate_auto_tune`] on, typed 429s also decay the
//!   admission pace and are shielded from the concurrency controller, so a
//!   rate storm does not poison the width signal.
//! - [`BudgetExhaustedError`] — the completion hit `max_tokens` with no
//!   visible output (hidden reasoning consumed the budget). The scheduler
//!   grows the budget on retry rather than repeating the same failure.
//! - Any other error counts as a generic task failure: retried with
//!   backoff, then handed to the repair wave, and recorded as a controller
//!   failure event (unless the rate tuner is active and the failure was a
//!   typed 429).
//!
//! Host-side request errors surface before execution:
//! [`RunRequest::validate`] rejects ambiguous or malformed submissions, and
//! [`SchedulerConfig::validate`] rejects invalid tuning. Terminal run
//! failures are reported through `RunSummary::error`.
//!
//! # Running a plan
//!
//! Build a [`Plan`] (or let the planner decompose an objective), then submit
//! it through [`RunManager`]:
//!
//! ```rust
//! use slimtoken::{Plan, Task};
//!
//! let plan = Plan {
//!     objective: "Ship a tiny library".to_string(),
//!     tasks: vec![
//!         Task {
//!             id: "types".to_string(),
//!             title: "Core types".to_string(),
//!             prompt: "Define the public types.".to_string(),
//!             ..Default::default()
//!         },
//!         Task {
//!             id: "impl".to_string(),
//!             title: "Implementation".to_string(),
//!             prompt: "Implement the public API.".to_string(),
//!             deps: vec!["types".to_string()],
//!             ..Default::default()
//!         },
//!     ],
//!     cache_primes: Vec::new(),
//! };
//! plan.validate()?;
//! # anyhow::Ok(())
//! ```
//!
//! ```no_run
//! use std::sync::Arc;
//! use slimtoken::{Plan, RunManager, RunRequest, SchedulerConfig, Task};
//!
//! # struct HostClient;
//! # #[slimtoken::async_trait]
//! # impl slimtoken::ModelClient for HostClient {
//! #     async fn complete(&self, _: slimtoken::CompletionRequest) -> anyhow::Result<slimtoken::Completion> {
//! #         unreachable!("example only shows construction")
//! #     }
//! # }
//! async fn doc() -> anyhow::Result<()> {
//! let manager = RunManager::new(
//!     Arc::new(HostClient),
//!     SchedulerConfig::default(),
//!     std::path::PathBuf::from("slimtoken-runs"),
//!     false,
//! );
//! let plan = Plan {
//!     objective: "Ship a tiny library".to_string(),
//!     tasks: vec![Task {
//!         id: "impl".to_string(),
//!         title: "Implementation".to_string(),
//!         prompt: "Implement the public API.".to_string(),
//!         ..Default::default()
//!     }],
//!     cache_primes: Vec::new(),
//! };
//! let summary = manager.submit(RunRequest {
//!     plan: Some(plan),
//!     ..Default::default()
//! }).await?;
//! println!("run {} is {:?}", summary.id, summary.status);
//! # Ok(())
//! # }
//! ```
//!
//! See `EMBEDDING.md` for the full host-integration guide and `API.md` for
//! the optional REST server (enable the `serve` cargo feature; the default
//! build is the bare engine with no server dependencies).

#[cfg(feature = "serve")]
pub mod api;
pub mod architect;
pub mod client;
pub mod control;
pub mod controller;
pub mod dag;
pub mod failover;
pub mod interactive;
mod jsonfix;
pub mod planner;
pub mod profile;
pub mod provider;
pub mod reconcile;
pub mod record;
pub mod runner;
pub mod scheduler;

pub use async_trait::async_trait;
pub use client::{
    report_first_token, BackendHealth, BudgetExhaustedError, Completion, CompletionRequest,
    FanOutBackend, FanOutClient, HealthRoutingConfig, ModelClient, OverloadedError, PermanentError,
    PlanningUsage, RateLimitedError, UsageCountingClient,
};
pub use control::{RunControl, TaskProgress, TaskProgressStatus};
pub use controller::{ConcurrencyController, ControllerConfig, ControllerStats};
pub use dag::{Plan, Task};
pub use runner::{RunDetails, RunManager, RunRequest, RunStatus, RunSummary};
pub use scheduler::{
    ConcurrencyPoint, FailureClass, PlanningSnapshot, ProbeConfig, RunReport, SchedulerConfig,
    SharedLimits, TaskRecord, TaskStatus,
};
