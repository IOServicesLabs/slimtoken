//! Record/replay harness: capture real provider exchanges once, then
//! rerun them offline with zero token burn. `RecordingClient` wraps any
//! `ModelClient` and appends one JSON line per exchange (request + response,
//! or request + error). `ReplayClient` loads a recording and serves the
//! recorded responses for matching requests, so scheduler and harness
//! changes can be A/B-tested against a fixed provider behavior trace.
//! Hosts keep full control: recording and replay are opt-in wrappers at the
//! client seam, like `FailoverClient`, and the scheduler above never knows.

use crate::client::{Completion, CompletionRequest, ModelClient};
use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::fs::OpenOptions;
use std::io::Write as _;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// One line of a recording file (JSONL).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecordedExchange {
    pub request: RecordedRequest,
    /// Some when the provider call succeeded; None when `error` is set.
    pub response: Option<RecordedResponse>,
    /// Some when the provider call failed (a 429 wall, a timeout, ...).
    /// Replay returns an error with this message.
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecordedRequest {
    pub system: String,
    pub shared_context: Option<String>,
    pub user: String,
    pub max_tokens: u32,
    pub temperature: f32,
    pub reasoning_effort: Option<String>,
    pub prompt_cache_key: Option<String>,
    pub allow_empty: bool,
    pub response_format: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecordedResponse {
    pub text: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub reasoning_tokens: Option<u64>,
    pub cached_input_tokens: Option<u64>,
    pub finish_reason: Option<String>,
    pub usage_is_estimated: bool,
    pub elapsed_ms: u64,
    pub ttft_ms: Option<u64>,
}

impl From<&CompletionRequest> for RecordedRequest {
    fn from(request: &CompletionRequest) -> Self {
        Self {
            system: request.system.clone(),
            shared_context: request.shared_context.clone(),
            user: request.user.clone(),
            max_tokens: request.max_tokens,
            temperature: request.temperature,
            reasoning_effort: request.reasoning_effort.clone(),
            prompt_cache_key: request.prompt_cache_key.clone(),
            allow_empty: request.allow_empty,
            response_format: request.response_format.clone(),
        }
    }
}

impl From<&Completion> for RecordedResponse {
    fn from(completion: &Completion) -> Self {
        Self {
            text: completion.text.clone(),
            input_tokens: completion.input_tokens,
            output_tokens: completion.output_tokens,
            reasoning_tokens: completion.reasoning_tokens,
            cached_input_tokens: completion.cached_input_tokens,
            finish_reason: completion.finish_reason.clone(),
            usage_is_estimated: completion.usage_is_estimated,
            elapsed_ms: completion.elapsed.as_millis() as u64,
            ttft_ms: completion
                .time_to_first_token
                .map(|delay| delay.as_millis() as u64),
        }
    }
}

impl From<RecordedResponse> for Completion {
    fn from(recorded: RecordedResponse) -> Self {
        Self {
            text: recorded.text,
            provider: None,
            input_tokens: recorded.input_tokens,
            output_tokens: recorded.output_tokens,
            reasoning_tokens: recorded.reasoning_tokens,
            cached_input_tokens: recorded.cached_input_tokens,
            finish_reason: recorded.finish_reason,
            usage_is_estimated: recorded.usage_is_estimated,
            elapsed: Duration::from_millis(recorded.elapsed_ms),
            time_to_first_token: recorded.ttft_ms.map(Duration::from_millis),
        }
    }
}

/// How a replay request is matched against the recording.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MatchMode {
    /// Match on what was asked: system + shared context + user + max_tokens
    /// + response_format. Behavioral hints (temperature, reasoning effort,
    ///   cache key, allow_empty) are ignored, so a recording made with one
    ///   flag set replays under another (scheduler A/B across flag changes).
    Semantic,
    /// Match on every request field. Stricter; use when a hint change must
    /// be served by a different recorded response.
    Exact,
}

#[derive(Debug, Clone)]
pub struct ReplayConfig {
    pub mode: MatchMode,
    /// Multiplier applied to the recorded per-request latency. 0 returns
    /// immediately (unit tests, fast scheduler A/B); 1.0 keeps the
    /// recorded timing, including its tail behavior.
    pub latency_scale: f64,
}

impl Default for ReplayConfig {
    fn default() -> Self {
        Self {
            mode: MatchMode::Semantic,
            latency_scale: 0.0,
        }
    }
}

fn match_key(mode: MatchMode, request: &RecordedRequest) -> String {
    match mode {
        // serde_json::to_string cannot fail on these owned structures.
        MatchMode::Exact => serde_json::to_string(request).unwrap_or_default(),
        MatchMode::Semantic => serde_json::to_string(&(
            &request.system,
            &request.shared_context,
            &request.user,
            request.max_tokens,
            &request.response_format,
        ))
        .unwrap_or_default(),
    }
}

#[derive(Debug, Default, Clone)]
pub struct ReplayStats {
    pub matched: u64,
    pub unmatched: u64,
}

/// Serves recorded responses for matching requests. Repeated identical
/// requests pop recorded responses in FIFO order (recorded order), which
/// covers cache primes and canary readers replaying a prime's byte-prefix.
pub struct ReplayClient {
    exchanges: Mutex<HashMap<String, VecDeque<RecordedExchange>>>,
    config: ReplayConfig,
    stats: Mutex<ReplayStats>,
}

impl ReplayClient {
    pub fn load(path: &Path, config: ReplayConfig) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read recording {}", path.display()))?;
        let mut exchanges: HashMap<String, VecDeque<RecordedExchange>> = HashMap::new();
        for (index, line) in text.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            let exchange: RecordedExchange = serde_json::from_str(line).with_context(|| {
                format!("invalid recording line {} in {}", index + 1, path.display())
            })?;
            let key = match_key(config.mode, &exchange.request);
            exchanges.entry(key).or_default().push_back(exchange);
        }
        Ok(Self {
            exchanges: Mutex::new(exchanges),
            config,
            stats: Mutex::new(ReplayStats::default()),
        })
    }

    pub fn stats(&self) -> ReplayStats {
        self.stats.lock().unwrap().clone()
    }
}

impl std::fmt::Debug for ReplayClient {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ReplayClient")
            .field("config", &self.config)
            .field("stats", &self.stats())
            .finish_non_exhaustive()
    }
}

#[async_trait]
impl ModelClient for ReplayClient {
    async fn complete(&self, request: CompletionRequest) -> Result<Completion> {
        let key = match_key(self.config.mode, &RecordedRequest::from(&request));
        let exchange = {
            let mut exchanges = self.exchanges.lock().unwrap();
            match exchanges.get_mut(&key) {
                Some(queue) => {
                    let exchange = queue.pop_front();
                    if queue.is_empty() {
                        exchanges.remove(&key);
                    }
                    exchange
                }
                None => None,
            }
        };
        let Some(exchange) = exchange else {
            self.stats.lock().unwrap().unmatched += 1;
            bail!(
                "replay: no recorded response matches this request (mode {:?}); \
                 first user line: {:?}",
                self.config.mode,
                request.user.lines().next().unwrap_or("")
            );
        };
        self.stats.lock().unwrap().matched += 1;
        if let Some(error) = exchange.error {
            bail!("replay: recorded provider error: {error}");
        }
        let recorded = exchange.response.expect("response set when error is None");
        let delay_ms = (recorded.elapsed_ms as f64 * self.config.latency_scale) as u64;
        if delay_ms > 0 {
            tokio::time::sleep(Duration::from_millis(delay_ms)).await;
        }
        Ok(Completion::from(recorded))
    }
}

/// Appends one JSON line per exchange to `path`. The inner client sees the
/// live traffic; the recording sees every outcome, errors included, so an
/// incident (a 429 wall, a truncated stream) is replayable later.
pub struct RecordingClient {
    inner: Arc<dyn ModelClient>,
    file: Mutex<std::fs::File>,
}

impl RecordingClient {
    pub fn new(inner: Arc<dyn ModelClient>, path: &Path) -> Result<Self> {
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .with_context(|| format!("failed to open recording {}", path.display()))?;
        Ok(Self {
            inner,
            file: Mutex::new(file),
        })
    }
}

impl std::fmt::Debug for RecordingClient {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RecordingClient")
            .finish_non_exhaustive()
    }
}

#[async_trait]
impl ModelClient for RecordingClient {
    async fn complete(&self, request: CompletionRequest) -> Result<Completion> {
        let recorded_request = RecordedRequest::from(&request);
        match self.inner.complete(request).await {
            Ok(completion) => {
                let exchange = RecordedExchange {
                    request: recorded_request,
                    response: Some(RecordedResponse::from(&completion)),
                    error: None,
                };
                let mut file = self.file.lock().unwrap();
                if let Err(error) = writeln!(file, "{}", serde_json::to_string(&exchange)?) {
                    tracing::warn!("recording write failed: {error:#}");
                }
                Ok(completion)
            }
            Err(error) => {
                let exchange = RecordedExchange {
                    request: recorded_request,
                    response: None,
                    error: Some(format!("{error:#}")),
                };
                let mut file = self.file.lock().unwrap();
                if let Err(write_error) = writeln!(file, "{}", serde_json::to_string(&exchange)?) {
                    tracing::warn!("recording write failed: {write_error:#}");
                }
                Err(error)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn temp_recording(name: &str) -> std::path::PathBuf {
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        std::env::temp_dir().join(format!(
            "slimtoken-record-test-{}-{}-{name}.jsonl",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::SeqCst)
        ))
    }

    fn request(user: &str, temperature: f32) -> CompletionRequest {
        CompletionRequest {
            system: "system text".into(),
            shared_context: Some("shared context".into()),
            user: user.into(),
            max_tokens: 128,
            temperature,
            reasoning_effort: None,
            prompt_cache_key: Some("key".into()),
            allow_empty: false,
            response_format: None,
        }
    }

    struct StubClient {
        calls: AtomicUsize,
    }

    #[async_trait]
    impl ModelClient for StubClient {
        async fn complete(&self, request: CompletionRequest) -> Result<Completion> {
            let call = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
            if request.user.starts_with("fail") {
                bail!("stub provider failure {call}");
            }
            Ok(Completion {
                text: format!("response {call} for {}", request.user),
                provider: None,
                input_tokens: 10 * call as u64,
                output_tokens: 5,
                reasoning_tokens: None,
                cached_input_tokens: Some(3),
                finish_reason: Some("stop".into()),
                usage_is_estimated: false,
                elapsed: Duration::from_millis(7),
                time_to_first_token: Some(Duration::from_millis(2)),
            })
        }
    }

    async fn record_exchange(
        path: &std::path::Path,
        request: &CompletionRequest,
    ) -> Result<Completion> {
        let recorder = RecordingClient::new(
            Arc::new(StubClient {
                calls: AtomicUsize::new(0),
            }),
            path,
        );
        recorder
            .expect("recording file opens in temp dir")
            .complete(request.clone())
            .await
    }

    /// One recorder over one stub, so per-call counters accumulate across
    /// the recorded exchanges.
    fn recorder_for(path: &std::path::Path) -> RecordingClient {
        RecordingClient::new(
            Arc::new(StubClient {
                calls: AtomicUsize::new(0),
            }),
            path,
        )
        .expect("recording file opens in temp dir")
    }

    #[tokio::test]
    async fn record_then_replay_roundtrip() {
        let path = temp_recording("roundtrip");
        let recorder = recorder_for(&path);
        let first = recorder.complete(request("alpha", 0.2)).await.unwrap();
        let second = recorder.complete(request("beta", 0.2)).await.unwrap();
        assert_eq!(first.text, "response 1 for alpha");
        assert_eq!(second.text, "response 2 for beta");

        let replay = ReplayClient::load(&path, ReplayConfig::default()).unwrap();
        let replayed = replay.complete(request("alpha", 0.2)).await.unwrap();
        assert_eq!(replayed.text, first.text);
        assert_eq!(replayed.input_tokens, first.input_tokens);
        assert_eq!(replayed.cached_input_tokens, Some(3));
        assert!(replay.complete(request("beta", 0.2)).await.is_ok());
        let stats = replay.stats();
        assert_eq!(stats.matched, 2);
        assert_eq!(stats.unmatched, 0);

        // A request that was never recorded fails loudly instead of serving
        // a fabricated response.
        assert!(replay.complete(request("gamma", 0.2)).await.is_err());
        assert_eq!(replay.stats().unmatched, 1);
        std::fs::remove_file(&path).ok();
    }

    #[tokio::test]
    async fn repeated_requests_pop_recorded_responses_in_order() {
        let path = temp_recording("fifo");
        // Same prompt twice, different responses: the recording records the
        // call counter, so replay must serve them FIFO, not collapse them.
        let recorder = recorder_for(&path);
        recorder.complete(request("dup", 0.2)).await.unwrap();
        recorder.complete(request("dup", 0.2)).await.unwrap();

        let replay = ReplayClient::load(&path, ReplayConfig::default()).unwrap();
        let first = replay.complete(request("dup", 0.2)).await.unwrap();
        let second = replay.complete(request("dup", 0.2)).await.unwrap();
        assert_eq!(first.text, "response 1 for dup");
        assert_eq!(second.text, "response 2 for dup");
        assert!(replay.complete(request("dup", 0.2)).await.is_err());
        std::fs::remove_file(&path).ok();
    }

    #[tokio::test]
    async fn recorded_errors_replay_as_errors() {
        let path = temp_recording("errors");
        let outcome = record_exchange(&path, &request("fail me", 0.2)).await;
        assert!(outcome.is_err());

        let replay = ReplayClient::load(&path, ReplayConfig::default()).unwrap();
        let replayed = replay.complete(request("fail me", 0.2)).await;
        let error = format!("{:#}", replayed.unwrap_err());
        assert!(error.contains("stub provider failure"), "got: {error}");
        std::fs::remove_file(&path).ok();
    }

    #[tokio::test]
    async fn semantic_mode_ignores_behavioral_hints_exact_mode_does_not() {
        let path = temp_recording("modes");
        record_exchange(&path, &request("alpha", 0.2))
            .await
            .unwrap();

        // Semantic: a replay under a different temperature/cache-key/effort
        // still matches (scheduler A/B across flag changes).
        let replay = ReplayClient::load(&path, ReplayConfig::default()).unwrap();
        let mut altered = request("alpha", 0.9);
        altered.prompt_cache_key = Some("other-key".into());
        altered.reasoning_effort = Some("low".into());
        assert!(replay.complete(altered).await.is_ok());
        std::fs::remove_file(&path).ok();

        // Exact: the same alteration must NOT match.
        let path = temp_recording("modes-exact");
        record_exchange(&path, &request("alpha", 0.2))
            .await
            .unwrap();
        let strict = ReplayClient::load(
            &path,
            ReplayConfig {
                mode: MatchMode::Exact,
                ..ReplayConfig::default()
            },
        )
        .unwrap();
        let mut altered = request("alpha", 0.9);
        altered.reasoning_effort = Some("low".into());
        assert!(strict.complete(altered).await.is_err());
        assert_eq!(strict.stats().unmatched, 1);
        assert!(strict.complete(request("alpha", 0.2)).await.is_ok());
        std::fs::remove_file(&path).ok();
    }

    #[tokio::test]
    async fn replay_latency_scale_zero_returns_recorded_elapsed_without_sleeping() {
        let path = temp_recording("latency");
        let recorder = RecordingClient::new(
            Arc::new(StubClient {
                calls: AtomicUsize::new(0),
            }),
            &path,
        );
        // Simulate a slow provider by recording a large elapsed value.
        let mut completion = recorder
            .expect("recording file opens in temp dir")
            .complete(request("alpha", 0.2))
            .await
            .unwrap();
        completion.elapsed = Duration::from_secs(5);
        // Rewrite the recording line so elapsed_ms is 5000.
        let line = std::fs::read_to_string(&path).unwrap();
        let mut exchange: RecordedExchange = serde_json::from_str(line.trim()).unwrap();
        exchange.response.as_mut().unwrap().elapsed_ms = 5000;
        std::fs::write(&path, serde_json::to_string(&exchange).unwrap()).unwrap();

        let replay = ReplayClient::load(&path, ReplayConfig::default()).unwrap();
        let started = std::time::Instant::now();
        let replayed = replay.complete(request("alpha", 0.2)).await.unwrap();
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "scale 0 must not sleep"
        );
        assert_eq!(
            replayed.elapsed,
            Duration::from_secs(5),
            "recorded timing is preserved"
        );
        std::fs::remove_file(&path).ok();
    }
}
