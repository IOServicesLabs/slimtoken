//! Provider failover: a `ModelClient` wrapper holding a primary and a
//! fallback provider. Sustained errors on the primary (429s, 5xx, timeouts —
//! anything the inner client surfaces as `Err`) drain traffic to the
//! fallback; an optional probe after a quiet window tries the primary again
//! and drains back on success. The scheduler above sees one client, so
//! failover composes with primes, the cache-warm gate, pacing, and tracing
//! unchanged — hosts wrap whatever two clients they already built.

use crate::client::{Completion, CompletionRequest, ModelClient};
use anyhow::Result;
use async_trait::async_trait;
use serde::Serialize;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tracing::warn;

#[derive(Debug, Clone)]
pub struct FailoverConfig {
    /// Consecutive primary errors before traffic drains to the fallback.
    /// Below the threshold the error returns to the scheduler, which retries
    /// with its normal backoff; the drain decision needs evidence the wall is
    /// sustained, not a single blip.
    pub error_threshold: u32,
    /// While on the fallback, after this much quiet time one request probes
    /// the primary again: success drains back, failure re-drains. `None`
    /// stays on the fallback for the rest of the run (sticky).
    pub probe_after: Option<Duration>,
    /// Bound on one primary call. A caller-side timeout (the scheduler's
    /// per-attempt timeout) cancels the call without an error ever reaching
    /// this wrapper, so a HUNG primary is invisible to the drain unless the
    /// call is bounded here. Set it below the scheduler's `task_timeout`.
    /// `None` (default) leaves the primary unbounded: only errors the inner
    /// client returns count.
    pub call_timeout: Option<Duration>,
}

impl Default for FailoverConfig {
    fn default() -> Self {
        Self {
            error_threshold: 3,
            probe_after: Some(Duration::from_secs(60)),
            call_timeout: None,
        }
    }
}

/// Per-run counters; hosts log them after the run to see what happened.
#[derive(Debug, Default, Clone, Serialize)]
pub struct FailoverStats {
    pub primary_calls: u64,
    pub fallback_calls: u64,
    pub primary_errors: u64,
    pub switches: u64,
    pub fail_backs: u64,
}

#[derive(Debug, Default)]
struct Inner {
    consecutive_errors: u32,
    on_fallback_since: Option<Instant>,
    /// A recovery probe is on the primary right now.
    probing: bool,
    stats: FailoverStats,
}

struct ProbeGuard<'a>(&'a Mutex<Inner>);

impl Drop for ProbeGuard<'_> {
    fn drop(&mut self) {
        if let Ok(mut inner) = self.0.lock() {
            inner.probing = false;
        }
    }
}

pub struct FailoverClient {
    primary: Arc<dyn ModelClient>,
    fallback: Arc<dyn ModelClient>,
    config: FailoverConfig,
    inner: Mutex<Inner>,
}

impl FailoverClient {
    pub fn new(
        primary: Arc<dyn ModelClient>,
        fallback: Arc<dyn ModelClient>,
        config: FailoverConfig,
    ) -> Self {
        Self {
            primary,
            fallback,
            config,
            inner: Mutex::new(Inner::default()),
        }
    }

    pub fn stats(&self) -> FailoverStats {
        self.inner.lock().unwrap().stats.clone()
    }
}

impl std::fmt::Debug for FailoverClient {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("FailoverClient")
            .field("config", &self.config)
            .field("stats", &self.stats())
            .finish_non_exhaustive()
    }
}

#[async_trait]
impl ModelClient for FailoverClient {
    async fn complete(&self, request: CompletionRequest) -> Result<Completion> {
        enum Route {
            Primary,
            Probe,
            Fallback,
        }
        let route = {
            let mut inner = self.inner.lock().unwrap();
            match inner.on_fallback_since {
                Some(since) => match self.config.probe_after {
                    // Single-flight: one request probes; the rest of the
                    // wave stays on the fallback until the probe reports.
                    Some(window) if since.elapsed() >= window && !inner.probing => {
                        inner.probing = true;
                        Route::Probe
                    }
                    _ => Route::Fallback,
                },
                None => Route::Primary,
            }
        };

        if matches!(route, Route::Fallback) {
            self.inner.lock().unwrap().stats.fallback_calls += 1;
            // Fallback errors stay on the fallback (hysteresis); the error
            // returns to the scheduler, which retries against us and lands
            // here again. A stuck-fallback run surfaces as failed tasks
            // instead of flapping between two dead providers.
            return self.fallback.complete(request).await;
        }
        // Clears the probe flag however this call ends, including a caller
        // that drops the future mid-probe.
        let _probe = matches!(route, Route::Probe).then_some(ProbeGuard(&self.inner));

        self.inner.lock().unwrap().stats.primary_calls += 1;
        let primary = self.primary.complete(request.clone());
        let outcome = match self.config.call_timeout {
            // The scheduler's own timeout DROPS this future, so a hung
            // primary would never reach the error arm below; bounding the
            // call here turns the hang into an error the drain can count.
            Some(limit) => match tokio::time::timeout(limit, primary).await {
                Ok(outcome) => outcome,
                Err(_) => Err(anyhow::anyhow!(
                    "primary provider timed out after {} ms",
                    limit.as_millis()
                )),
            },
            None => primary.await,
        };
        match outcome {
            Ok(completion) => {
                let mut inner = self.inner.lock().unwrap();
                inner.consecutive_errors = 0;
                if inner.on_fallback_since.take().is_some() {
                    inner.stats.fail_backs += 1;
                    warn!("failover: primary healthy again; draining back from fallback");
                }
                Ok(completion)
            }
            Err(error) => {
                let drained = {
                    let mut inner = self.inner.lock().unwrap();
                    inner.consecutive_errors += 1;
                    inner.stats.primary_errors += 1;
                    if inner.consecutive_errors >= self.config.error_threshold {
                        inner.on_fallback_since = Some(Instant::now());
                        inner.stats.switches += 1;
                        true
                    } else {
                        false
                    }
                };
                if drained {
                    let errors = self.config.error_threshold;
                    warn!(
                        errors,
                        threshold = self.config.error_threshold,
                        "failover: primary error wall sustained; draining to fallback"
                    );
                    self.inner.lock().unwrap().stats.fallback_calls += 1;
                    // Serve this very request on the fallback instead of
                    // returning the error: the caller gets a successful
                    // completion, just from the other provider.
                    return self.fallback.complete(request).await;
                }
                Err(error)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::RateLimitedError;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn limited() -> anyhow::Error {
        anyhow::Error::new(RateLimitedError { retry_after: None })
    }

    struct AlwaysFail;
    #[async_trait]
    impl ModelClient for AlwaysFail {
        async fn complete(&self, _request: CompletionRequest) -> Result<Completion> {
            Err(limited())
        }
    }

    struct Healthy;
    #[async_trait]
    impl ModelClient for Healthy {
        async fn complete(&self, _request: CompletionRequest) -> Result<Completion> {
            Ok(Completion {
                text: "ok".into(),
                provider: None,
                input_tokens: 1,
                output_tokens: 1,
                reasoning_tokens: None,
                cached_input_tokens: None,
                finish_reason: Some("stop".into()),
                usage_is_estimated: false,
                elapsed: Duration::ZERO,
                time_to_first_token: None,
            })
        }
    }

    struct FailFirst {
        calls: AtomicUsize,
        errors: usize,
    }
    #[async_trait]
    impl ModelClient for FailFirst {
        async fn complete(&self, request: CompletionRequest) -> Result<Completion> {
            let call = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
            if call <= self.errors {
                return Err(limited());
            }
            Healthy.complete(request).await
        }
    }

    fn request() -> CompletionRequest {
        CompletionRequest {
            system: "s".into(),
            shared_context: None,
            user: "u".into(),
            max_tokens: 8,
            temperature: 0.0,
            reasoning_effort: None,
            prompt_cache_key: None,
            allow_empty: false,
            response_format: None,
        }
    }

    #[tokio::test]
    async fn sustained_errors_drain_and_stay_on_fallback() {
        let client = FailoverClient::new(
            Arc::new(FailFirst {
                calls: AtomicUsize::new(0),
                errors: 5,
            }),
            Arc::new(Healthy),
            FailoverConfig {
                error_threshold: 2,
                probe_after: None,
                call_timeout: None,
            },
        );
        // Attempt 1: below threshold, the error surfaces for the scheduler.
        assert!(client.complete(request()).await.is_err());
        // Attempt 2: threshold hit, this same request is served by fallback.
        assert!(client.complete(request()).await.is_ok());
        // Later requests go straight to the fallback; primary untouched.
        assert!(client.complete(request()).await.is_ok());
        let stats = client.stats();
        assert_eq!(stats.primary_calls, 2);
        assert_eq!(stats.fallback_calls, 2);
        assert_eq!(stats.primary_errors, 2);
        assert_eq!(stats.switches, 1);
        assert_eq!(stats.fail_backs, 0);
    }

    #[tokio::test]
    async fn probe_after_window_drains_back_on_success() {
        let client = FailoverClient::new(
            Arc::new(FailFirst {
                calls: AtomicUsize::new(0),
                errors: 2,
            }),
            Arc::new(Healthy),
            FailoverConfig {
                error_threshold: 2,
                probe_after: Some(Duration::from_millis(20)),
                call_timeout: None,
            },
        );
        assert!(client.complete(request()).await.is_err());
        assert!(client.complete(request()).await.is_ok()); // drains
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert!(client.complete(request()).await.is_ok()); // probes primary
        let stats = client.stats();
        assert_eq!(stats.primary_calls, 3, "probe must hit the primary");
        assert_eq!(stats.fail_backs, 1, "healthy probe drains back");
        assert_eq!(stats.switches, 1);
    }

    #[tokio::test]
    async fn mock_injection_ab_wall_fails_wave_failover_saves_it() {
        use crate::dag::Plan;
        use crate::scheduler::{execute_plan, SchedulerConfig, TaskStatus};

        let tasks: Vec<crate::dag::Task> = (0..4)
            .map(|index| crate::dag::Task {
                id: format!("t{index}"),
                title: format!("Task {index}"),
                prompt: "Do the thing.".into(),
                deps: Vec::new(),
                max_tokens: 64,
                temperature: None,
                reasoning_effort: None,
                validate_artifact: false,
                require_closed_fences: false,
                static_output: None,
                internal: false,
            })
            .collect();
        let plan = Plan {
            objective: "failover a/b".into(),
            cache_primes: Vec::new(),
            tasks,
        };
        let config = SchedulerConfig {
            max_retries: 1,
            retry_base_delay: Duration::from_millis(2),
            ..SchedulerConfig::default()
        };

        // Arm A: primary only, hard 429 wall. Every task exhausts its retries.
        let report_a = execute_plan(&plan, Arc::new(AlwaysFail), config.clone())
            .await
            .unwrap();
        assert_eq!(report_a.status_count(TaskStatus::Failed), 4);

        // Arm B: same wall behind the failover wrapper, threshold 1 — every
        // first attempt drains immediately and the fallback serves the wave.
        let failover = Arc::new(FailoverClient::new(
            Arc::new(AlwaysFail),
            Arc::new(Healthy),
            FailoverConfig {
                error_threshold: 1,
                probe_after: None,
                call_timeout: None,
            },
        ));
        let client_b: Arc<dyn ModelClient> = failover.clone();
        let report_b = execute_plan(&plan, client_b, config).await.unwrap();
        assert!(report_b.is_success(), "failover must save the wave");
        assert_eq!(report_b.tasks.len(), 4);
        let stats = failover.stats();
        // Tasks run concurrently, so the first drain may serve several of
        // them: exactly four fallback completions, at least one switch, and
        // the primary saw between one and four calls.
        assert_eq!(stats.fallback_calls, 4);
        assert!(stats.switches >= 1);
        assert!((1..=4).contains(&stats.primary_calls));
    }

    struct Hang;
    #[async_trait]
    impl ModelClient for Hang {
        async fn complete(&self, _request: CompletionRequest) -> Result<Completion> {
            std::future::pending().await
        }
    }

    #[tokio::test]
    async fn hung_primary_counts_as_an_error_and_drains() {
        let client = FailoverClient::new(
            Arc::new(Hang),
            Arc::new(Healthy),
            FailoverConfig {
                error_threshold: 1,
                probe_after: None,
                call_timeout: Some(Duration::from_millis(20)),
            },
        );
        assert!(client.complete(request()).await.is_ok());
        let stats = client.stats();
        assert_eq!(stats.primary_errors, 1);
        assert_eq!(stats.switches, 1);
        assert_eq!(stats.fallback_calls, 1);
    }

    #[tokio::test]
    async fn only_one_request_probes_the_primary() {
        let client = Arc::new(FailoverClient::new(
            Arc::new(Hang),
            Arc::new(Healthy),
            FailoverConfig {
                error_threshold: 1,
                probe_after: Some(Duration::from_millis(10)),
                call_timeout: Some(Duration::from_millis(60)),
            },
        ));
        assert!(client.complete(request()).await.is_ok()); // drains
        tokio::time::sleep(Duration::from_millis(15)).await;
        let before = client.stats().primary_calls;
        let wave = (0..6).map(|_| client.complete(request()));
        for result in futures::future::join_all(wave).await {
            assert!(result.is_ok());
        }
        assert_eq!(
            client.stats().primary_calls - before,
            1,
            "one probe, not the whole wave"
        );
    }
}
