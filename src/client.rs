use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::fmt;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Typed error for HTTP 429 responses, carrying the server's Retry-After
/// hint so the scheduler can wait out the rate-limit window instead of
/// hammering the API with speculative retries.
#[derive(Debug)]
pub struct RateLimitedError {
    pub retry_after: Option<Duration>,
}

impl fmt::Display for RateLimitedError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.retry_after {
            Some(delay) => write!(f, "rate limited by model server; retry after {delay:?}"),
            None => write!(f, "rate limited by model server"),
        }
    }
}

impl std::error::Error for RateLimitedError {}

/// The completion hit `max_tokens` before emitting any visible output:
/// hidden reasoning consumed the whole budget. Retrying with the same budget
/// usually fails the same way, so the scheduler grows the budget on retry.
#[derive(Debug)]
pub struct BudgetExhaustedError {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub reasoning_tokens: u64,
}

impl fmt::Display for BudgetExhaustedError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "model produced no visible output before max_tokens; hidden reasoning consumed the budget (prompt_tokens={}, completion_tokens={}, reasoning_tokens={})",
            self.input_tokens, self.output_tokens, self.reasoning_tokens
        )
    }
}

impl std::error::Error for BudgetExhaustedError {}

#[derive(Debug, Clone)]
pub struct CompletionRequest {
    pub system: String,
    /// Shared run context rendered as its own user message before `user`.
    /// Kept byte-identical across concurrent workers so a server-side prefix
    /// cache can reuse the prefill across simultaneous streams.
    pub shared_context: Option<String>,
    pub user: String,
    pub max_tokens: u32,
    pub temperature: f32,
    /// Reasoning-effort hint for reasoning models ("low", "medium", "high").
    /// Muse-class models spend hidden reasoning tokens before any visible
    /// output; "low" roughly halves the burn on straightforward tasks.
    pub reasoning_effort: Option<String>,
    /// Server-side prefix-cache routing key (Muse `prompt_cache_key`).
    /// Requests sharing a key are pinned to the same cache shard, so setting
    /// one stable key across runs and sessions maximizes cross-session
    /// prefix-cache reuse. Part of the request body, not the prompt, so it
    /// never disturbs the byte-identity of the cached prefix itself.
    pub prompt_cache_key: Option<String>,
    /// When true, an empty completion text is not an error: the request only
    /// exists to make the server process (and prefix-cache) the prompt —
    /// cache primes use this, since their job is done the moment prefill
    /// happened, regardless of what the model decodes.
    pub allow_empty: bool,
    /// OpenAI-style `response_format` (e.g. a strict `json_schema`), passed
    /// through verbatim. Muse honors strict json_schema, which makes
    /// structured outputs (planner fragments) parse by construction.
    pub response_format: Option<serde_json::Value>,
}

#[derive(Debug, Clone)]
pub struct Completion {
    pub text: String,
    /// Backend that produced this completion, when the client is a labeled
    /// pool (e.g. `FanOutClient`); `None` for single-provider clients.
    /// Propagates into the run report so per-provider throughput and cost
    /// can be measured from one mixed run.
    pub provider: Option<String>,
    pub input_tokens: u64,
    pub output_tokens: u64,
    /// Hidden reasoning tokens billed as completion output, when the server reports them.
    pub reasoning_tokens: Option<u64>,
    /// Prompt tokens served from the server's prefix cache, when reported.
    pub cached_input_tokens: Option<u64>,
    pub finish_reason: Option<String>,
    pub usage_is_estimated: bool,
    pub elapsed: Duration,
    pub time_to_first_token: Option<Duration>,
}

/// The provider seam. Every model request in the engine goes through this
/// trait, so a host integrates by implementing it (or wrapping the shipped
/// OpenAI-compatible client) and handing `Arc<dyn ModelClient>` to the
/// runner.
///
/// # Error contract
///
/// Return errors as `anyhow::Error`. Two typed payloads are recognized:
///
/// - [`RateLimitedError`] — a rate wall (HTTP 429). Set `retry_after` from
///   the server's Retry-After header when present; the scheduler waits out
///   the window and, with `rate_auto_tune` on, decays the admission pace
///   instead of cutting concurrency.
/// - [`BudgetExhaustedError`] — `max_tokens` was hit with no visible output
///   (hidden reasoning burn); the scheduler grows the budget on retry.
/// - Anything else is a generic failure: retried with backoff, then repaired.
///
/// ```rust
/// use std::time::Duration;
/// use slimtoken::{async_trait, Completion, CompletionRequest, ModelClient};
///
/// struct MyProvider;
///
/// #[async_trait]
/// impl ModelClient for MyProvider {
///     async fn complete(&self, request: CompletionRequest) -> anyhow::Result<Completion> {
///         let http = call_provider(&request).await?;
///         if http.status == 429 {
///             return Err(slimtoken::RateLimitedError {
///                 retry_after: http.retry_after,
///             }
///             .into());
///         }
///         Ok(http.completion)
///     }
/// }
/// # struct HttpOutcome { status: u16, retry_after: Option<Duration>, completion: Completion }
/// # async fn call_provider(_: &CompletionRequest) -> anyhow::Result<HttpOutcome> {
/// #     Ok(HttpOutcome {
/// #         status: 200,
/// #         retry_after: None,
/// #         completion: Completion {
/// #             text: String::new(),
/// #             provider: None,
/// #             input_tokens: 0,
/// #             output_tokens: 0,
/// #             reasoning_tokens: None,
/// #             cached_input_tokens: None,
/// #             finish_reason: None,
/// #             usage_is_estimated: false,
/// #             elapsed: Duration::ZERO,
/// #             time_to_first_token: None,
/// #         },
/// #     })
/// # }
/// ```
#[async_trait]
pub trait ModelClient: Send + Sync {
    async fn complete(&self, request: CompletionRequest) -> Result<Completion>;
}

/// One backend of a fan-out pool: a labeled client plus the reasoning effort
/// the backend wants applied when a request does not pin one itself (each
/// provider accepts a different effort vocabulary, so the pool carries the
/// per-backend default instead of forcing a global one).
pub struct FanOutBackend {
    pub label: String,
    pub client: Arc<dyn ModelClient>,
    pub default_effort: Option<String>,
}

impl FanOutBackend {
    pub fn new(
        label: impl Into<String>,
        client: Arc<dyn ModelClient>,
        default_effort: Option<String>,
    ) -> Self {
        Self {
            label: label.into(),
            client,
            default_effort,
        }
    }
}

/// Multi-provider fan-out at the `ModelClient` seam: the scheduler sees one
/// client and needs no changes. Each request routes to exactly one backend by
/// a stable hash of the prompt, so every retry of a task lands on the same
/// provider (server-side caches, rate windows, and provider quirks stay
/// per-task consistent). The chosen backend stamps its label onto the
/// completion, which propagates into the run report for per-provider
/// measurement.
///
/// Routing is deliberately not load-balancing: the scheduler already owns
/// admission, and a hash route is what keeps retries sticky. Hash is FNV-1a —
/// `std`'s `DefaultHasher` is randomly seeded per process, which would move
/// tasks between providers on every run.
///
/// `with_health_routing` layers a per-backend circuit breaker over the
/// hash route (opt-in; `new` keeps the pure hash route bit-for-bit). A
/// backend whose error EMA crosses the threshold is skipped in favor of the
/// next eligible backend, so a degraded provider sheds its share instead of
/// failing every task hashed to it. Recovery is half-open: every Nth call
/// hashed to a broken backend probes it anyway, and its EMA decays back
/// below the threshold as successes land; when every backend is broken the
/// hash-chosen one takes the probe by walk-order fallback.
pub struct FanOutClient {
    backends: Vec<FanOutBackend>,
    health: Option<HealthRouting>,
}

/// Opt-in health routing configuration for [`FanOutClient`].
#[derive(Debug, Clone, Copy)]
pub struct HealthRoutingConfig {
    /// EWMA weight for new observations; higher forgets faster. Clamped to
    /// (0, 1] if invalid.
    pub error_alpha: f64,
    /// Error EMA at or above which a backend is circuit-broken (skipped while
    /// a healthier peer exists). Clamped to (0, 1] if invalid.
    pub breaker_threshold: f64,
    /// Observations required before the breaker may trip — small samples
    /// lie. A brand-new backend is always eligible until this count.
    pub min_probes: u32,
    /// Half-open recovery: when the hash-chosen backend is broken, every
    /// Nth call still goes to it as a recovery probe, so a healed backend
    /// re-enters through a successful EMA decay instead of waiting for a
    /// total pool outage. Clamped to >= 1 (1 = probe on every call).
    pub recovery_probe_every: u32,
}

impl Default for HealthRoutingConfig {
    fn default() -> Self {
        Self {
            error_alpha: 0.3,
            breaker_threshold: 0.5,
            min_probes: 3,
            // 1/8 of calls hashed to a broken backend pay one retry while it
            // is down, in exchange for timely self-recovery.
            recovery_probe_every: 8,
        }
    }
}

impl HealthRoutingConfig {
    fn clamped(mut self) -> Self {
        if !(self.error_alpha > 0.0 && self.error_alpha <= 1.0) {
            self.error_alpha = Self::default().error_alpha;
        }
        if !(self.breaker_threshold > 0.0 && self.breaker_threshold <= 1.0) {
            self.breaker_threshold = Self::default().breaker_threshold;
        }
        if self.min_probes == 0 {
            self.min_probes = Self::default().min_probes;
        }
        if self.recovery_probe_every == 0 {
            self.recovery_probe_every = Self::default().recovery_probe_every;
        }
        self
    }
}

/// Per-backend health as recorded by an opt-in health-routed
/// [`FanOutClient`]. `error_ema` is the EWMA of failure outcomes (1.0 = every
/// probe failed); `latency_ema_ms` is the EWMA of call wall time.
#[derive(Debug, Default, Clone, Copy, serde::Serialize)]
pub struct BackendHealth {
    pub probes: u64,
    pub failures: u64,
    pub error_ema: f64,
    pub latency_ema_ms: f64,
}

/// Mutable per-backend state behind the circuit breaker.
struct HealthRouting {
    config: HealthRoutingConfig,
    states: Vec<std::sync::Mutex<BackendHealth>>,
    /// Calls seen, for the half-open recovery probe cadence.
    calls: std::sync::atomic::AtomicU64,
}

impl HealthRouting {
    fn new(config: HealthRoutingConfig, count: usize) -> Self {
        Self {
            config,
            states: (0..count)
                .map(|_| std::sync::Mutex::new(BackendHealth::default()))
                .collect(),
            calls: std::sync::atomic::AtomicU64::new(0),
        }
    }

    fn eligible(&self, index: usize) -> bool {
        let state = self.states[index].lock().expect("backend health lock");
        state.probes < u64::from(self.config.min_probes)
            || state.error_ema < self.config.breaker_threshold
    }

    /// Hash backend first when eligible — and, when broken, again on every
    /// `recovery_probe_every`th call as a half-open probe so a healed
    /// backend decays back in. Otherwise the first eligible successor in
    /// deterministic walk order; if all are broken, the hash backend takes
    /// the probe (its EMA decays as successes land).
    fn choose(&self, hash_index: usize, count: usize) -> usize {
        let tick = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if self.eligible(hash_index) || tick % u64::from(self.config.recovery_probe_every) == 0 {
            return hash_index;
        }
        for step in 1..count {
            let candidate = (hash_index + step) % count;
            if self.eligible(candidate) {
                return candidate;
            }
        }
        hash_index
    }

    fn record(&self, index: usize, latency: Duration, failed: bool) {
        let mut state = self.states[index].lock().expect("backend health lock");
        let alpha = self.config.error_alpha;
        let outcome = if failed { 1.0 } else { 0.0 };
        state.error_ema = if state.probes == 0 {
            outcome
        } else {
            alpha * outcome + (1.0 - alpha) * state.error_ema
        };
        let ms = latency.as_secs_f64() * 1000.0;
        state.latency_ema_ms = if state.probes == 0 {
            ms
        } else {
            alpha * ms + (1.0 - alpha) * state.latency_ema_ms
        };
        state.probes += 1;
        if failed {
            state.failures += 1;
        }
    }
}

impl FanOutClient {
    pub fn new(backends: Vec<FanOutBackend>) -> Result<Self> {
        if backends.is_empty() {
            bail!("fan-out needs at least one backend");
        }
        Ok(Self {
            backends,
            health: None,
        })
    }

    /// enable circuit-breaker health routing over the hash route.
    /// Invalid config fields fall back to their defaults. Legacy pools built
    /// with `new` never record health and never reroute.
    pub fn with_health_routing(mut self, config: HealthRoutingConfig) -> Self {
        self.health = Some(HealthRouting::new(config.clamped(), self.backends.len()));
        self
    }

    /// Per-backend health in pool order. `None` when health routing was not
    /// enabled; `Some` with one entry per backend otherwise.
    pub fn health_snapshot(&self) -> Option<Vec<BackendHealth>> {
        self.health.as_ref().map(|health| {
            health
                .states
                .iter()
                .map(|state| *state.lock().expect("backend health lock"))
                .collect()
        })
    }

    pub fn backend_count(&self) -> usize {
        self.backends.len()
    }

    /// FNV-1a over the prompt bytes; stable across processes and runs.
    fn route(user: &str) -> u64 {
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
        for byte in user.as_bytes() {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
        hash
    }
}

#[async_trait]
impl ModelClient for FanOutClient {
    async fn complete(&self, mut request: CompletionRequest) -> Result<Completion> {
        let hash_index = (Self::route(&request.user) % self.backends.len() as u64) as usize;
        let index = match &self.health {
            None => hash_index,
            Some(health) => health.choose(hash_index, self.backends.len()),
        };
        let backend = &self.backends[index];
        if request.reasoning_effort.is_none() {
            request.reasoning_effort = backend.default_effort.clone();
        }
        let start = Instant::now();
        let result = backend.client.complete(request).await;
        if let Some(health) = &self.health {
            health.record(index, start.elapsed(), result.is_err());
        }
        let mut completion = result?;
        completion.provider = Some(backend.label.clone());
        Ok(completion)
    }
}

/// Billed usage accumulated across a planning phase. Planning paths
/// (architect ladders, fragment salvage, hedged races, wiring, planner
/// retries) make many nested model calls, and their cost was previously
/// invisible: the run report totaled wave tokens only, so overhead read 0.0%
/// by construction. `UsageCountingClient` counts every completion that
/// passes through the wrapped planning client into one of these; the host
/// reads it after planning and stamps the report and the trace.
#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct PlanningUsage {
    /// Successful completions counted. Failed attempts report no usage.
    pub calls: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    /// Prompt tokens served from the server prefix cache, when reported.
    pub cached_input_tokens: u64,
}

/// `ModelClient` decorator that adds every completion's billed usage
/// into a shared [`PlanningUsage`] before returning it. Wrap the client used
/// for planning calls (architect / planner), never the wave client: the wave
/// already accounts its tokens per task. Counting at the seam captures every
/// nested planning call — fragment ladders, salvage, hedged races, wiring —
/// without threading state through each call site. On a manifest-cache hit
/// the wrapped client is never called and the totals stay zero, which is the
/// truthful accounting for a planning-free rebuild.
pub struct UsageCountingClient {
    inner: Arc<dyn ModelClient>,
    usage: Arc<std::sync::Mutex<PlanningUsage>>,
}

impl UsageCountingClient {
    pub fn new(inner: Arc<dyn ModelClient>, usage: Arc<std::sync::Mutex<PlanningUsage>>) -> Self {
        Self { inner, usage }
    }

    /// Snapshot of the totals so far (clone; safe to read after planning).
    pub fn usage(&self) -> PlanningUsage {
        self.usage.lock().expect("planning usage lock").clone()
    }
}

#[async_trait]
impl ModelClient for UsageCountingClient {
    async fn complete(&self, request: CompletionRequest) -> Result<Completion> {
        let completion = self.inner.complete(request).await?;
        let mut usage = self.usage.lock().expect("planning usage lock");
        usage.calls += 1;
        usage.input_tokens += completion.input_tokens;
        usage.output_tokens += completion.output_tokens;
        usage.cached_input_tokens += completion.cached_input_tokens.unwrap_or(0);
        Ok(completion)
    }
}

/// Streaming client for llama.cpp, vLLM, LM Studio, and other
/// OpenAI-compatible `/v1/chat/completions` endpoints.
pub struct OpenAiClient {
    http: reqwest::Client,
    endpoint: String,
    model: String,
    api_key: Option<String>,
    include_stream_usage: bool,
}

impl OpenAiClient {
    pub fn new(base_url: &str, model: impl Into<String>, api_key: Option<String>) -> Result<Self> {
        let base_url = base_url.trim_end_matches('/');
        if base_url.is_empty() {
            bail!("model base URL cannot be empty");
        }
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(15))
            .build()
            .context("failed to construct HTTP client")?;
        Ok(Self {
            http,
            endpoint: format!("{base_url}/chat/completions"),
            model: model.into(),
            api_key,
            include_stream_usage: false,
        })
    }

    pub fn with_stream_usage(mut self, enabled: bool) -> Self {
        self.include_stream_usage = enabled;
        self
    }
}

#[derive(Default)]
struct StreamUsage {
    input: Option<u64>,
    output: Option<u64>,
    reasoning: Option<u64>,
    cached: Option<u64>,
}

#[async_trait]
impl ModelClient for OpenAiClient {
    async fn complete(&self, request: CompletionRequest) -> Result<Completion> {
        let started = Instant::now();
        let mut messages = vec![json!({"role": "system", "content": &request.system})];
        if let Some(shared) = &request.shared_context {
            messages.push(json!({"role": "user", "content": shared}));
        }
        messages.push(json!({"role": "user", "content": &request.user}));

        let mut body = json!({
            "model": &self.model,
            "messages": messages,
            "max_tokens": request.max_tokens,
            "temperature": request.temperature,
            "stream": true
        });
        if let Some(effort) = &request.reasoning_effort {
            body["reasoning_effort"] = json!(effort);
        }
        if let Some(key) = &request.prompt_cache_key {
            body["prompt_cache_key"] = json!(key);
        }
        if let Some(format) = &request.response_format {
            body["response_format"] = format.clone();
        }
        if self.include_stream_usage {
            body["stream_options"] = json!({"include_usage": true});
        }

        let mut builder = self.http.post(&self.endpoint).json(&body);
        if let Some(api_key) = &self.api_key {
            builder = builder.bearer_auth(api_key);
        }
        let mut response = builder.send().await.context("model request failed")?;
        let status = response.status();
        if !status.is_success() {
            if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
                // Surfaced as a typed error so the scheduler can honor the
                // server's Retry-After instead of guessing a backoff.
                let retry_after = response
                    .headers()
                    .get(reqwest::header::RETRY_AFTER)
                    .and_then(|value| value.to_str().ok())
                    .and_then(|text| text.parse::<u64>().ok())
                    .map(Duration::from_secs);
                return Err(RateLimitedError { retry_after }.into());
            }
            let error_body = response.text().await.unwrap_or_default();
            let excerpt: String = error_body.chars().take(1_000).collect();
            bail!("model server returned HTTP {status}: {excerpt}");
        }

        let mut buffered = Vec::<u8>::new();
        let mut text = String::new();
        let mut usage = StreamUsage::default();
        let mut time_to_first_token = None;
        let mut finish_reason = None;
        let mut done = false;

        while let Some(chunk) = response.chunk().await.context("model stream failed")? {
            buffered.extend_from_slice(&chunk);
            while let Some((event_end, separator_len)) = find_event_boundary(&buffered) {
                let event_bytes = buffered[..event_end].to_vec();
                drop(buffered.drain(..event_end + separator_len));
                let event = String::from_utf8(event_bytes).context("stream was not UTF-8")?;
                done = consume_event(&event, &mut text, &mut usage, &mut finish_reason)?;
                if time_to_first_token.is_none() && !text.is_empty() {
                    time_to_first_token = Some(started.elapsed());
                }
                if done {
                    break;
                }
            }
            if done {
                break;
            }
        }

        if !done && !buffered.is_empty() {
            let event = String::from_utf8(buffered).context("stream was not UTF-8")?;
            consume_event(&event, &mut text, &mut usage, &mut finish_reason)?;
        }
        ensure_stream_complete(done, finish_reason.as_deref(), text.chars().count())?;
        if text.is_empty() && !request.allow_empty {
            match finish_reason.as_deref() {
                // Reasoning models can spend the whole budget on hidden
                // reasoning and emit no visible content at all. Keep the
                // parsed usage in the error: it is the evidence for how the
                // budget was spent and how to fix the task config.
                Some("length") => {
                    return Err(BudgetExhaustedError {
                        input_tokens: usage.input.unwrap_or(0),
                        output_tokens: usage.output.unwrap_or(0),
                        reasoning_tokens: usage.reasoning.unwrap_or(0),
                    }
                    .into())
                }
                _ => bail!("model returned an empty completion"),
            }
        }

        let estimated_input = estimate_tokens(&request.system)
            + request
                .shared_context
                .as_deref()
                .map(estimate_tokens)
                .unwrap_or(0)
            + estimate_tokens(&request.user);
        let estimated_output = estimate_tokens(&text);
        let usage_is_estimated = usage.input.is_none() || usage.output.is_none();

        Ok(Completion {
            text,
            provider: None,
            input_tokens: usage.input.unwrap_or(estimated_input),
            output_tokens: usage.output.unwrap_or(estimated_output),
            reasoning_tokens: usage.reasoning,
            cached_input_tokens: usage.cached,
            finish_reason,
            usage_is_estimated,
            elapsed: started.elapsed(),
            time_to_first_token,
        })
    }
}

/// Rejects streams whose connection closed before the `[DONE]` sentinel.
///
/// A server may emit a finish_reason chunk first and then drop the connection
/// mid-content — observed on Muse: `finish_reason="stop"` followed by a close
/// that truncated the completion mid-token. Without this check such a response
/// is indistinguishable from a clean finish and the truncated text would be
/// marked successful.
fn ensure_stream_complete(
    done: bool,
    finish_reason: Option<&str>,
    text_chars: usize,
) -> Result<()> {
    if !done {
        bail!(
            "model stream ended before the [DONE] sentinel (finish_reason={finish_reason:?}, {text_chars} chars received); treating as truncated"
        );
    }
    Ok(())
}

/// Parses one server-sent event. Returns true for the `[DONE]` sentinel.
fn consume_event(
    event: &str,
    text: &mut String,
    usage: &mut StreamUsage,
    finish_reason: &mut Option<String>,
) -> Result<bool> {
    let data_lines: Vec<&str> = event
        .lines()
        .filter_map(|line| line.strip_prefix("data:").map(str::trim_start))
        .collect();
    if data_lines.is_empty() {
        return Ok(false);
    }
    let data = data_lines.join("\n");
    if data.trim() == "[DONE]" {
        return Ok(true);
    }

    let value: Value = serde_json::from_str(&data).with_context(|| {
        format!(
            "invalid model stream JSON: {}",
            data.chars().take(200).collect::<String>()
        )
    })?;
    if let Some(error) = value.get("error") {
        bail!("model server returned a stream error: {error}");
    }

    if let Some(content) = value
        .pointer("/choices/0/delta/content")
        .and_then(Value::as_str)
    {
        text.push_str(content);
    }

    if let Some(finish) = value
        .pointer("/choices/0/finish_reason")
        .and_then(Value::as_str)
    {
        *finish_reason = Some(finish.to_string());
    }

    if let Some(stream_usage) = value.get("usage") {
        usage.input = stream_usage
            .get("prompt_tokens")
            .and_then(Value::as_u64)
            .or(usage.input);
        usage.output = stream_usage
            .get("completion_tokens")
            .and_then(Value::as_u64)
            .or(usage.output);
        usage.reasoning = stream_usage
            .pointer("/completion_tokens_details/reasoning_tokens")
            .and_then(Value::as_u64)
            .or(usage.reasoning);
        usage.cached = stream_usage
            .pointer("/prompt_tokens_details/cached_tokens")
            .and_then(Value::as_u64)
            .or(usage.cached);
    }
    Ok(false)
}

fn find_event_boundary(bytes: &[u8]) -> Option<(usize, usize)> {
    let lf = bytes
        .windows(2)
        .position(|window| window == b"\n\n")
        .map(|index| (index, 2));
    let crlf = bytes
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|index| (index, 4));
    match (lf, crlf) {
        (Some(left), Some(right)) => Some(if left.0 < right.0 { left } else { right }),
        (Some(boundary), None) | (None, Some(boundary)) => Some(boundary),
        (None, None) => None,
    }
}

fn estimate_tokens(text: &str) -> u64 {
    // Used only when the server omits usage; reports mark these values as estimates.
    text.chars().count().div_ceil(4) as u64
}

pub struct MockClient {
    latency: Duration,
}

impl MockClient {
    pub fn new(latency: Duration) -> Self {
        Self { latency }
    }
}

#[async_trait]
impl ModelClient for MockClient {
    async fn complete(&self, request: CompletionRequest) -> Result<Completion> {
        let started = Instant::now();
        tokio::time::sleep(self.latency).await;
        let title = request
            .user
            .lines()
            .find(|line| line.starts_with("Title: "))
            .unwrap_or("Title: assigned task");
        let text = format!("Mock output for {title}: completed successfully.");
        Ok(Completion {
            provider: None,
            input_tokens: estimate_tokens(&request.system)
                + request
                    .shared_context
                    .as_deref()
                    .map(estimate_tokens)
                    .unwrap_or(0)
                + estimate_tokens(&request.user),
            output_tokens: estimate_tokens(&text),
            text,
            reasoning_tokens: None,
            cached_input_tokens: None,
            finish_reason: Some("stop".to_string()),
            usage_is_estimated: true,
            elapsed: started.elapsed(),
            time_to_first_token: Some(self.latency),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Records every prompt it sees, tagged with its backend label, so tests
    /// can assert which backend served which request.
    struct RecordingClient {
        label: String,
        seen: std::sync::Mutex<Vec<String>>,
        effort: std::sync::Mutex<Vec<Option<String>>>,
    }

    impl RecordingClient {
        fn new(label: &str) -> Arc<Self> {
            Arc::new(Self {
                label: label.to_string(),
                seen: std::sync::Mutex::new(Vec::new()),
                effort: std::sync::Mutex::new(Vec::new()),
            })
        }
    }

    #[async_trait]
    impl ModelClient for RecordingClient {
        async fn complete(&self, request: CompletionRequest) -> Result<Completion> {
            self.seen.lock().expect("seen").push(request.user.clone());
            self.effort
                .lock()
                .expect("effort")
                .push(request.reasoning_effort.clone());
            let mut done = MockClient::new(Duration::ZERO).complete(request).await?;
            done.provider = Some(self.label.clone());
            Ok(done)
        }
    }

    fn request(user: &str, effort: Option<&str>) -> CompletionRequest {
        CompletionRequest {
            system: "system".into(),
            shared_context: None,
            user: user.into(),
            max_tokens: 16,
            temperature: 0.2,
            reasoning_effort: effort.map(str::to_string),
            prompt_cache_key: None,
            allow_empty: false,
            response_format: None,
        }
    }

    fn pool<A: ModelClient + 'static, B: ModelClient + 'static>(
        a: &Arc<A>,
        b: &Arc<B>,
    ) -> FanOutClient {
        let a: Arc<dyn ModelClient> = a.clone();
        let b: Arc<dyn ModelClient> = b.clone();
        FanOutClient::new(vec![
            FanOutBackend::new("a", a, Some("minimal".to_string())),
            FanOutBackend::new("b", b, Some("none".to_string())),
        ])
        .expect("pool")
    }

    #[tokio::test]
    async fn fanout_routes_by_prompt_and_stamps_the_backend_label() {
        let a = RecordingClient::new("a");
        let b = RecordingClient::new("b");
        let pool = pool(&a, &b);
        // Two prompts; whichever backend each lands on, a retry of the SAME
        // prompt must land on the same one.
        for user in ["task alpha", "task beta"] {
            let first = pool.complete(request(user, None)).await.expect("first");
            let second = pool.complete(request(user, None)).await.expect("second");
            assert_eq!(first.provider, second.provider);
            let (a_count, b_count) = (
                a.seen.lock().expect("seen").len(),
                b.seen.lock().expect("seen").len(),
            );
            // Both attempts of one prompt went to exactly one backend.
            assert!(
                (a_count == 2 && b_count == 0) || (a_count == 0 && b_count == 2),
                "prompt must stick to one backend, got a={a_count} b={b_count}"
            );
            a.seen.lock().expect("seen").clear();
            b.seen.lock().expect("seen").clear();
        }
    }

    #[tokio::test]
    async fn fanout_applies_the_backend_default_effort_only_when_unset() {
        let a = RecordingClient::new("a");
        let b = RecordingClient::new("b");
        let pool = pool(&a, &b);
        pool.complete(request("no effort pinned", None))
            .await
            .expect("default");
        pool.complete(request("effort pinned task", Some("high")))
            .await
            .expect("pinned");
        for backend in [&a, &b] {
            let seen = backend.seen.lock().expect("seen");
            let efforts = backend.effort.lock().expect("effort");
            for (user, effort) in seen.iter().zip(efforts.iter()) {
                let expected = if user == "effort pinned task" {
                    Some("high")
                } else {
                    // Whichever backend served it, the default is ITS default
                    // (a: minimal, b: none) — never the other backend's.
                    Some(if backend.label == "a" {
                        "minimal"
                    } else {
                        "none"
                    })
                };
                assert_eq!(effort.as_deref(), expected, "user={user}");
            }
        }
    }

    #[test]
    fn fanout_rejects_an_empty_pool() {
        assert!(FanOutClient::new(Vec::new()).is_err());
    }

    /// Fails the first `failures` calls, then succeeds; records outcomes.
    struct FlakyClient {
        label: &'static str,
        remaining: std::sync::Mutex<u32>,
        seen: std::sync::Mutex<Vec<String>>,
    }

    impl FlakyClient {
        fn new(label: &'static str, failures: u32) -> Arc<Self> {
            Arc::new(Self {
                label,
                remaining: std::sync::Mutex::new(failures),
                seen: std::sync::Mutex::new(Vec::new()),
            })
        }
    }

    #[async_trait]
    impl ModelClient for FlakyClient {
        async fn complete(&self, request: CompletionRequest) -> Result<Completion> {
            self.seen.lock().expect("seen").push(request.user.clone());
            let fail = {
                let mut remaining = self.remaining.lock().expect("remaining");
                if *remaining > 0 {
                    *remaining -= 1;
                    true
                } else {
                    false
                }
            };
            if fail {
                bail!("{} is degraded", self.label);
            }
            let mut done = MockClient::new(Duration::ZERO).complete(request).await?;
            done.provider = Some(self.label.to_string());
            Ok(done)
        }
    }

    /// A prompt whose FNV hash routes to backend index 0 of a two-backend
    /// pool, so tests control exactly which backend the hash picks.
    fn prompt_hashing_to_a() -> String {
        (0..10_000)
            .map(|n| format!("probe prompt {n}"))
            .find(|candidate| FanOutClient::route(candidate) % 2 == 0)
            .expect("a prompt routing to backend a")
    }

    #[tokio::test]
    async fn health_routing_sheds_a_degraded_backend() {
        let a = FlakyClient::new("a", 100);
        let b = FlakyClient::new("b", 0);
        let pool = pool(&a, &b).with_health_routing(HealthRoutingConfig {
            min_probes: 2,
            ..HealthRoutingConfig::default()
        });
        let prompt = prompt_hashing_to_a();
        // Two failures trip the breaker (min_probes=2, threshold=0.5,
        // error_ema=1.0); the retry of the SAME prompt then sheds to b while
        // the legacy route would keep hammering a.
        assert!(pool.complete(request(&prompt, None)).await.is_err());
        assert!(pool.complete(request(&prompt, None)).await.is_err());
        let rescued = pool
            .complete(request(&prompt, None))
            .await
            .expect("shed to b");
        assert_eq!(rescued.provider.as_deref(), Some("b"));
        assert_eq!(
            a.seen.lock().expect("seen").len(),
            2,
            "a saw exactly the tripping probes"
        );
        let health = pool.health_snapshot().expect("health recorded");
        assert_eq!(health[0].failures, 2);
        assert_eq!(health[0].error_ema, 1.0);
        assert_eq!(health[1].error_ema, 0.0, "b succeeded");
    }

    #[tokio::test]
    async fn health_routing_recovers_via_half_open_probes() {
        let a = FlakyClient::new("a", 3);
        let b = FlakyClient::new("b", 0);
        let pool = pool(&a, &b).with_health_routing(HealthRoutingConfig {
            recovery_probe_every: 2,
            ..HealthRoutingConfig::default()
        });
        let prompt = prompt_hashing_to_a();
        for _ in 0..3 {
            assert!(pool.complete(request(&prompt, None)).await.is_err());
        }
        // Broken: non-probe ticks shed to b.
        assert_eq!(
            pool.complete(request(&prompt, None))
                .await
                .expect("shed")
                .provider
                .as_deref(),
            Some("b")
        );
        // Half-open probes (every 2nd call) hit a; successes decay the error
        // EMA (1.0 -> 0.7 -> 0.49) below the 0.5 threshold within two probes
        // — without the probes a would never see traffic again while b stays
        // healthy.
        let mut recovered = false;
        for _ in 0..20 {
            pool.complete(request(&prompt, None)).await.expect("served");
            if pool.health_snapshot().expect("health")[0].error_ema < 0.5 {
                recovered = true;
                break;
            }
        }
        assert!(recovered, "a never recovered");
        // Hash-to-a traffic returns to a now that it is eligible.
        let done = pool
            .complete(request(&prompt, None))
            .await
            .expect("a serves again");
        assert_eq!(done.provider.as_deref(), Some("a"));
    }

    #[tokio::test]
    async fn health_routing_probes_the_hash_backend_when_all_are_broken() {
        let a = FlakyClient::new("a", 100);
        let b = FlakyClient::new("b", 100);
        let pool = pool(&a, &b).with_health_routing(HealthRoutingConfig::default());
        let prompt = prompt_hashing_to_a();
        // Trip both backends.
        for _ in 0..3 {
            let _ = pool.complete(request(&prompt, None)).await;
        }
        // Trip b the same way through an odd-hash prompt.
        let odd = (0..10_000)
            .map(|n| format!("odd trip {n}"))
            .find(|candidate| FanOutClient::route(candidate) % 2 == 1)
            .expect("an odd-hash prompt");
        for _ in 0..3 {
            let _ = pool.complete(request(&odd, None)).await;
        }
        let health = pool.health_snapshot().expect("health");
        assert!(health[0].error_ema >= 0.5 && health[1].error_ema >= 0.5);
        // No eligible peer: the hash backend takes the recovery probe and
        // the error propagates instead of deadlocking on peer selection.
        assert!(pool.complete(request(&prompt, None)).await.is_err());
        assert_eq!(
            a.seen.lock().expect("seen").len(),
            4,
            "recovery probe hit a"
        );
    }

    #[tokio::test]
    async fn health_routing_is_off_and_hash_route_unchanged_by_default() {
        let a = FlakyClient::new("a", 100);
        let b = FlakyClient::new("b", 0);
        let pool = pool(&a, &b);
        assert!(pool.health_snapshot().is_none());
        let prompt = prompt_hashing_to_a();
        // Legacy pool: same-prompt retries keep hitting the broken backend —
        // this is exactly the behavior health routing opt-outs preserve.
        for _ in 0..4 {
            assert!(pool.complete(request(&prompt, None)).await.is_err());
        }
        assert_eq!(
            a.seen.lock().expect("seen").len(),
            4,
            "no reroute without opt-in"
        );
        assert!(b.seen.lock().expect("seen").is_empty());
    }

    #[test]
    fn health_routing_config_clamps_invalid_fields() {
        let config = HealthRoutingConfig {
            error_alpha: 0.0,
            breaker_threshold: 7.0,
            min_probes: 0,
            recovery_probe_every: 0,
        }
        .clamped();
        assert_eq!(
            config.error_alpha,
            HealthRoutingConfig::default().error_alpha
        );
        assert_eq!(
            config.breaker_threshold,
            HealthRoutingConfig::default().breaker_threshold
        );
        assert_eq!(config.min_probes, HealthRoutingConfig::default().min_probes);
    }

    #[tokio::test]
    async fn usage_counting_client_accumulates_planning_billing() {
        let usage = Arc::new(std::sync::Mutex::new(PlanningUsage::default()));
        let counting: Arc<dyn ModelClient> = Arc::new(UsageCountingClient::new(
            Arc::new(MockClient::new(Duration::ZERO)),
            usage.clone(),
        ));
        for _ in 0..3 {
            counting
                .complete(request("plan me", None))
                .await
                .expect("call");
        }
        let totals = usage.lock().expect("usage").clone();
        assert_eq!(totals.calls, 3);
        assert!(totals.input_tokens > 0);
        assert!(totals.output_tokens > 0);
        // A failed call bills nothing: the mock succeeds, so force the error
        // path through the same seam shape via a rejecting inner client.
        struct ErrClient;
        #[async_trait]
        impl ModelClient for ErrClient {
            async fn complete(&self, _request: CompletionRequest) -> Result<Completion> {
                bail!("down")
            }
        }
        let rejecting: Arc<dyn ModelClient> =
            Arc::new(UsageCountingClient::new(Arc::new(ErrClient), usage.clone()));
        assert!(rejecting.complete(request("x", None)).await.is_err());
        let after = usage.lock().expect("usage").clone();
        assert_eq!(after.calls, 3, "failed attempts report no usage");
        assert_eq!(after.input_tokens, totals.input_tokens);
    }

    #[test]
    fn fanout_route_is_stable_across_calls() {
        assert_eq!(
            FanOutClient::route("same prompt"),
            FanOutClient::route("same prompt")
        );
    }

    #[test]
    fn handles_split_sse_boundaries() {
        assert_eq!(find_event_boundary(b"data: one\n\n"), Some((9, 2)));
        assert_eq!(find_event_boundary(b"data: one\r\n\r\n"), Some((9, 4)));
        assert_eq!(find_event_boundary(b"data: one\n"), None);
    }

    #[test]
    fn premature_eof_is_rejected_even_after_a_stop_chunk() {
        // Regression: Muse closed the connection mid-token right after
        // emitting finish_reason="stop". The truncated completion must fail
        // (and retry), not be marked successful.
        let err = ensure_stream_complete(false, Some("stop"), 12_920).unwrap_err();
        assert!(format!("{err:#}").contains("before the [DONE] sentinel"));

        // A clean stream ([DONE] seen) passes regardless of finish_reason.
        assert!(ensure_stream_complete(true, Some("stop"), 12_920).is_ok());
        assert!(ensure_stream_complete(true, Some("length"), 0).is_ok());
    }

    #[test]
    fn parses_content_and_usage() {
        let mut text = String::new();
        let mut usage = StreamUsage::default();
        let mut finish = None;
        let event = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"hello\"}}],",
            "\"usage\":{\"prompt_tokens\":7,\"completion_tokens\":2}}"
        );
        assert!(!consume_event(event, &mut text, &mut usage, &mut finish).unwrap());
        assert_eq!(text, "hello");
        assert_eq!(usage.input, Some(7));
        assert_eq!(usage.output, Some(2));
    }

    #[test]
    fn parses_muse_reasoning_and_cache_details() {
        let mut text = String::new();
        let mut usage = StreamUsage::default();
        let mut finish = None;
        // Shape observed from api.meta.ai: usage travels in the finish chunk,
        // alongside finish_reason and hidden-reasoning / cache fields.
        let event = concat!(
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}],",
            "\"usage\":{\"prompt_tokens\":18,\"completion_tokens\":311,",
            "\"completion_tokens_details\":{\"reasoning_tokens\":289},",
            "\"prompt_tokens_details\":{\"cached_tokens\":12}}}"
        );
        assert!(!consume_event(event, &mut text, &mut usage, &mut finish).unwrap());
        assert_eq!(usage.input, Some(18));
        assert_eq!(usage.output, Some(311));
        assert_eq!(usage.reasoning, Some(289));
        assert_eq!(usage.cached, Some(12));
        assert_eq!(finish.as_deref(), Some("stop"));
    }

    #[test]
    fn recognizes_done() {
        let mut text = String::new();
        let mut usage = StreamUsage::default();
        let mut finish = None;
        assert!(consume_event("data: [DONE]", &mut text, &mut usage, &mut finish).unwrap());
    }
}
