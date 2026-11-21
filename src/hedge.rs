//! Hedged requests: a second, identical copy of a request that has shown no
//! first token after a threshold, raced against the first.
//!
//! A slow first token is mostly server-side queueing; the same request sent
//! again often lands on a faster replica. The copy that streams its first
//! delta first wins and the other is dropped at once (its stream closes, so
//! nothing is decoded twice). A delta is visible text or hidden reasoning
//! (`reasoning_content`, through [`crate::report_stream_started`]): a
//! reasoning model's race is decided when its reasoning starts, not when its
//! answer does. A copy that finishes before either streams wins too.
//!
//! Copies back off: a rate-limit or overload error from either copy stops
//! all hedging for the server's Retry-After (or [`HedgeConfig::cooldown`]),
//! and no more than [`HedgeConfig::max_in_flight`] copies run at once, so
//! a queueing server never gets the whole budget in one burst. Both copies carry the same request, so the
//! output is drawn from the same distribution as without the hedge: there is
//! no bias towards shorter answers (that would come from keeping the first
//! copy to *finish*).
//!
//! Off unless a host builds a [`Hedger`] (the CLI's `--hedge-after-ms`, the
//! bench switch `hedge`); without one no request changes.

use crate::client::{
    current_first_token_hook, with_first_token_hook, with_stream_start_hook, Completion,
    CompletionRequest, FirstTokenHook, ModelClient, OverloadedError, RateLimitedError,
};
use anyhow::Result;
use async_trait::async_trait;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::Poll;
use std::time::{Duration, Instant};
use tracing::info;

/// Default wait for a first token before a copy is sent.
pub const DEFAULT_HEDGE_AFTER: Duration = Duration::from_millis(2_500);
/// Default number of copies one client may send in its lifetime (one run).
pub const DEFAULT_MAX_HEDGES: usize = 16;
/// Default number of copies that may run at once.
pub const DEFAULT_MAX_IN_FLIGHT: usize = 3;
/// Default pause in hedging after a rate-limit or overload error that came
/// without a Retry-After.
pub const DEFAULT_COOLDOWN: Duration = Duration::from_secs(30);

/// When to send a copy and how many copies one client may send.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HedgeConfig {
    /// A request with no first token after this long gets a copy. Zero sends
    /// the copy at once (both start together).
    pub after: Duration,
    /// Most copies sent over the client's lifetime; 0 sends none.
    pub max_hedges: usize,
    /// Most copies running at once; at the cap a slow request gets none.
    pub max_in_flight: usize,
    /// No copy is sent for this long after a rate-limit or overload error
    /// with no Retry-After (a Retry-After sets the pause itself).
    pub cooldown: Duration,
}

impl Default for HedgeConfig {
    fn default() -> Self {
        Self {
            after: DEFAULT_HEDGE_AFTER,
            max_hedges: DEFAULT_MAX_HEDGES,
            max_in_flight: DEFAULT_MAX_IN_FLIGHT,
            cooldown: DEFAULT_COOLDOWN,
        }
    }
}

/// The server's back-off signal in `error`: `Some(retry_after)` for a
/// rate-limit or overload error, `None` for any other.
fn backoff_signal(error: &anyhow::Error) -> Option<Option<Duration>> {
    if let Some(limited) = error.downcast_ref::<RateLimitedError>() {
        return Some(limited.retry_after);
    }
    error
        .downcast_ref::<OverloadedError>()
        .map(|overloaded| overloaded.retry_after)
}

/// Holds one in-flight copy's place; gives it back when dropped.
struct InFlight<'a>(&'a AtomicUsize);

impl Drop for InFlight<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

/// What a race did, for the caller's own records.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct HedgeOutcome {
    /// A copy was sent.
    pub launched: bool,
    /// The copy that won: 0 is the original request, 1 the hedge.
    pub winner: usize,
    /// When the copy was sent, from the start of the race.
    pub launched_after: Duration,
}

/// Counters over a client's lifetime.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct HedgeStats {
    pub launched: usize,
    pub won: usize,
    /// Times a rate-limit or overload error paused hedging.
    pub cooldowns: usize,
}

/// Races copies of a request; shared by every request of one client so
/// [`HedgeConfig::max_hedges`] caps the whole run.
#[derive(Debug)]
pub struct Hedger {
    config: HedgeConfig,
    launched: AtomicUsize,
    won: AtomicUsize,
    in_flight: AtomicUsize,
    cooldowns: AtomicUsize,
    /// No copy is sent before this instant.
    cooled_until: Mutex<Option<Instant>>,
}

const UNDECIDED: usize = usize::MAX;

enum Step {
    Timer,
    Decided(usize),
    Done(usize, Result<Completion>),
}

impl Hedger {
    pub fn new(config: HedgeConfig) -> Self {
        Self {
            config,
            launched: AtomicUsize::new(0),
            won: AtomicUsize::new(0),
            in_flight: AtomicUsize::new(0),
            cooldowns: AtomicUsize::new(0),
            cooled_until: Mutex::new(None),
        }
    }

    pub fn config(&self) -> HedgeConfig {
        self.config
    }

    pub fn stats(&self) -> HedgeStats {
        HedgeStats {
            launched: self.launched.load(Ordering::Relaxed),
            won: self.won.load(Ordering::Relaxed),
            cooldowns: self.cooldowns.load(Ordering::Relaxed),
        }
    }

    /// True while a rate-limit or overload pause is on.
    fn cooling(&self) -> bool {
        self.cooled_until
            .lock()
            .map(|until| until.is_some_and(|until| Instant::now() < until))
            .unwrap_or(false)
    }

    /// Note a request's error: a rate-limit or overload error pauses
    /// hedging for its Retry-After, or [`HedgeConfig::cooldown`].
    fn observe(&self, error: &anyhow::Error) {
        let Some(retry_after) = backoff_signal(error) else {
            return;
        };
        let pause = retry_after.unwrap_or(self.config.cooldown);
        let until = Instant::now() + pause;
        if let Ok(mut cooled) = self.cooled_until.lock() {
            if cooled.is_none_or(|current| current < until) {
                *cooled = Some(until);
            }
        }
        let cooldowns = self.cooldowns.fetch_add(1, Ordering::Relaxed) + 1;
        info!(
            pause_ms = pause.as_millis() as u64,
            cooldowns, "hedge: rate limited or overloaded, no copies during the pause"
        );
    }

    /// Take one copy from the budget and a place among the copies in
    /// flight; `None` once the budget is spent, at the in-flight cap, or
    /// during a rate-limit pause.
    fn take(&self) -> Option<InFlight<'_>> {
        if self.cooling() {
            info!("hedge: skipped, rate-limit pause");
            return None;
        }
        let mut flying = self.in_flight.load(Ordering::Relaxed);
        loop {
            if flying >= self.config.max_in_flight {
                info!(
                    in_flight = flying,
                    "hedge: skipped, in-flight copies at the cap"
                );
                return None;
            }
            match self.in_flight.compare_exchange(
                flying,
                flying + 1,
                Ordering::AcqRel,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(actual) => flying = actual,
            }
        }
        let place = InFlight(&self.in_flight);
        let mut current = self.launched.load(Ordering::Relaxed);
        loop {
            if current >= self.config.max_hedges {
                return None;
            }
            match self.launched.compare_exchange(
                current,
                current + 1,
                Ordering::AcqRel,
                Ordering::Relaxed,
            ) {
                Ok(_) => return Some(place),
                Err(actual) => current = actual,
            }
        }
    }

    /// Run `start(0)`; if it reports no first token (through
    /// [`crate::report_first_token`]) within [`HedgeConfig::after`] and the
    /// budget allows, run `start(1)` beside it. The first copy to report a
    /// token wins and the other is dropped; a copy that finishes first wins
    /// too; a copy that fails while the other still runs leaves the race to
    /// the other. The scheduler's own first-token hook hears the winner's
    /// first token, timed from the start of the race, and so do the
    /// returned `elapsed` and `time_to_first_token`.
    pub async fn race<F, Fut>(&self, mut start: F) -> (Result<Completion>, HedgeOutcome)
    where
        F: FnMut(usize) -> Fut,
        Fut: Future<Output = Result<Completion>>,
    {
        let began = Instant::now();
        let outer = current_first_token_hook();
        let decided = Arc::new(AtomicUsize::new(UNDECIDED));
        // The first visible token decides the race (when no earlier delta
        // did) and reaches the scheduler's hook from the winner only.
        let token_hook_for = |copy: usize, offset: Duration| -> FirstTokenHook {
            let decided = decided.clone();
            let outer = outer.clone();
            Arc::new(move |elapsed: Duration| {
                let _ =
                    decided.compare_exchange(UNDECIDED, copy, Ordering::AcqRel, Ordering::Acquire);
                if decided.load(Ordering::Acquire) == copy {
                    if let Some(outer) = &outer {
                        outer(offset + elapsed);
                    }
                }
            })
        };
        // Any first delta, hidden reasoning included, decides the race.
        let start_hook_for = |copy: usize| -> FirstTokenHook {
            let decided = decided.clone();
            Arc::new(move |_: Duration| {
                let _ =
                    decided.compare_exchange(UNDECIDED, copy, Ordering::AcqRel, Ordering::Acquire);
            })
        };
        let mut make = |copy: usize, offset: Duration| {
            with_first_token_hook(
                token_hook_for(copy, offset),
                with_stream_start_hook(start_hook_for(copy), start(copy)),
            )
        };
        let mut slots = [Some(Box::pin(make(0, Duration::ZERO))), None];
        let mut offsets = [Duration::ZERO; 2];
        let mut timer = Some(Box::pin(tokio::time::sleep(self.config.after)));
        let mut errors: [Option<anyhow::Error>; 2] = [None, None];
        let mut outcome = HedgeOutcome::default();
        // The copy's place among the copies in flight, held while it runs.
        let mut place: Option<InFlight<'_>> = None;
        loop {
            let step = std::future::poll_fn(|cx| {
                for (copy, slot) in slots.iter_mut().enumerate() {
                    let Some(future) = slot.as_mut() else {
                        continue;
                    };
                    // Never poll a copy that has lost: it must not stream.
                    let now = decided.load(Ordering::Acquire);
                    if now != UNDECIDED && now != copy {
                        continue;
                    }
                    if let Poll::Ready(result) = Pin::as_mut(future).poll(cx) {
                        return Poll::Ready(Step::Done(copy, result));
                    }
                    if decided.load(Ordering::Acquire) == copy {
                        return Poll::Ready(Step::Decided(copy));
                    }
                }
                if let Some(sleep) = timer.as_mut() {
                    if sleep.as_mut().poll(cx).is_ready() {
                        return Poll::Ready(Step::Timer);
                    }
                }
                Poll::Pending
            })
            .await;
            match step {
                Step::Timer => {
                    timer = None;
                    if decided.load(Ordering::Acquire) == UNDECIDED && slots[0].is_some() {
                        place = self.take();
                    }
                    if place.is_some() {
                        let offset = began.elapsed();
                        offsets[1] = offset;
                        slots[1] = Some(Box::pin(make(1, offset)));
                        outcome.launched = true;
                        outcome.launched_after = offset;
                        info!(
                            after_ms = offset.as_millis() as u64,
                            hedges = self.launched.load(Ordering::Relaxed),
                            "hedge: no first token yet, sent an identical copy"
                        );
                    }
                }
                Step::Decided(copy) => {
                    // Drop the loser before the winner streams on.
                    slots[1 - copy] = None;
                    if slots[1].is_none() {
                        drop(place.take());
                    }
                    let winner = slots[copy].take().expect("decided copy is running");
                    let result = winner.await;
                    if let Err(error) = &result {
                        self.observe(error);
                    }
                    return self.finish(copy, offsets[copy], result, outcome, began);
                }
                Step::Done(copy, Ok(done)) => {
                    // Returning drops the other copy.
                    return self.finish(copy, offsets[copy], Ok(done), outcome, began);
                }
                Step::Done(copy, Err(error)) => {
                    self.observe(&error);
                    slots[copy] = None;
                    if slots[1].is_none() {
                        drop(place.take());
                    }
                    if decided.load(Ordering::Acquire) == copy {
                        // The winner failed after its first delta (in the
                        // same poll): the other copy is never polled again,
                        // so its error ends the race, as a winner's would.
                        slots[1 - copy] = None;
                        drop(place);
                        return self.finish(copy, offsets[copy], Err(error), outcome, began);
                    }
                    if slots[1 - copy].is_some() {
                        info!(copy, error = %format!("{error:#}"), "hedge: one copy failed, the other runs on");
                        errors[copy] = Some(error);
                        timer = None;
                        continue;
                    }
                    // Nothing left: a rate-limit or overload error first, so
                    // the scheduler's back-off sees it; else the original's,
                    // so typed errors reach the scheduler's retry as they
                    // would.
                    errors[copy] = Some(error);
                    let typed = (0..2).find(|&index| {
                        errors[index]
                            .as_ref()
                            .is_some_and(|error| backoff_signal(error).is_some())
                    });
                    let pick = typed.unwrap_or(if errors[0].is_some() { 0 } else { copy });
                    let error = errors[pick].take().expect("picked copy has an error");
                    outcome.winner = copy;
                    return (Err(error), outcome);
                }
            }
        }
    }

    fn finish(
        &self,
        copy: usize,
        offset: Duration,
        result: Result<Completion>,
        mut outcome: HedgeOutcome,
        began: Instant,
    ) -> (Result<Completion>, HedgeOutcome) {
        outcome.winner = copy;
        if outcome.launched {
            if copy == 1 {
                self.won.fetch_add(1, Ordering::Relaxed);
            }
            info!(
                winner = if copy == 1 { "hedge" } else { "original" },
                after_ms = outcome.launched_after.as_millis() as u64,
                total_ms = began.elapsed().as_millis() as u64,
                won = self.won.load(Ordering::Relaxed),
                launched = self.launched.load(Ordering::Relaxed),
                "hedge: race decided"
            );
        }
        let result = result.map(|mut done| {
            done.elapsed += offset;
            done.time_to_first_token = done.time_to_first_token.map(|ttft| ttft + offset);
            done
        });
        (result, outcome)
    }
}

/// A [`ModelClient`] that hedges every request of `inner` through one
/// [`Hedger`]. Cache primes (`allow_empty`) go through unhedged.
pub struct HedgeClient {
    inner: Arc<dyn ModelClient>,
    hedger: Arc<Hedger>,
}

impl HedgeClient {
    pub fn new(inner: Arc<dyn ModelClient>, config: HedgeConfig) -> Self {
        Self {
            inner,
            hedger: Arc::new(Hedger::new(config)),
        }
    }

    pub fn hedger(&self) -> Arc<Hedger> {
        self.hedger.clone()
    }
}

#[async_trait]
impl ModelClient for HedgeClient {
    async fn complete(&self, request: CompletionRequest) -> Result<Completion> {
        if request.allow_empty {
            return self.inner.complete(request).await;
        }
        let inner = &self.inner;
        let (result, _) = self.hedger.race(|_| inner.complete(request.clone())).await;
        result
    }

    async fn prewarm(&self, connections: usize) -> Result<crate::client::PrewarmReport> {
        self.inner.prewarm(connections).await
    }

    /// A continuation goes to the inner client unhedged, through its own
    /// `continue_completion`, so its prefix mode is kept.
    async fn continue_completion(
        &self,
        request: CompletionRequest,
        partial: String,
    ) -> Result<crate::continuation::Continued> {
        self.inner.continue_completion(request, partial).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::report_first_token;
    use std::sync::atomic::AtomicBool;
    use std::sync::Mutex;

    fn completion(text: &str) -> Completion {
        Completion {
            text: text.to_string(),
            provider: None,
            input_tokens: 10,
            output_tokens: 5,
            reasoning_tokens: None,
            cached_input_tokens: None,
            finish_reason: Some("stop".to_string()),
            usage_is_estimated: false,
            elapsed: Duration::ZERO,
            time_to_first_token: None,
        }
    }

    fn request() -> CompletionRequest {
        CompletionRequest {
            system: "system".to_string(),
            shared_context: None,
            user: "write the file".to_string(),
            max_tokens: 100,
            temperature: 0.0,
            reasoning_effort: None,
            prompt_cache_key: None,
            allow_empty: false,
            response_format: None,
        }
    }

    /// Sets its flag when dropped before it finished.
    struct DropFlag(Arc<AtomicBool>, bool);
    impl Drop for DropFlag {
        fn drop(&mut self) {
            if !self.1 {
                self.0.store(true, Ordering::SeqCst);
            }
        }
    }

    /// Call `n` (0-based) waits `first_token[n]` for its first token, then
    /// `rest` more before it returns `call-n`. Records every request it got
    /// and whether a call was dropped before it finished.
    struct Scripted {
        first_token: Vec<Duration>,
        rest: Duration,
        calls: AtomicUsize,
        requests: Mutex<Vec<CompletionRequest>>,
        cancelled: Vec<Arc<AtomicBool>>,
    }

    impl Scripted {
        fn new(first_token: Vec<Duration>, rest: Duration) -> Arc<Self> {
            let cancelled = first_token
                .iter()
                .map(|_| Arc::new(AtomicBool::new(false)))
                .collect();
            Arc::new(Self {
                first_token,
                rest,
                calls: AtomicUsize::new(0),
                requests: Mutex::new(Vec::new()),
                cancelled,
            })
        }
    }

    #[async_trait]
    impl ModelClient for Scripted {
        async fn complete(&self, request: CompletionRequest) -> Result<Completion> {
            let call = self.calls.fetch_add(1, Ordering::SeqCst);
            self.requests.lock().unwrap().push(request);
            let mut flag = DropFlag(self.cancelled[call].clone(), false);
            let started = Instant::now();
            tokio::time::sleep(self.first_token[call]).await;
            report_first_token(started.elapsed());
            tokio::time::sleep(self.rest).await;
            flag.1 = true;
            let mut done = completion(&format!("call-{call}"));
            done.elapsed = started.elapsed();
            Ok(done)
        }
    }

    #[tokio::test]
    async fn hedge_keeps_the_copy_with_the_first_token_and_drops_the_other() {
        let inner = Scripted::new(
            vec![Duration::from_millis(1_500), Duration::from_millis(20)],
            Duration::from_millis(60),
        );
        let client = HedgeClient::new(
            inner.clone(),
            HedgeConfig {
                after: Duration::from_millis(80),
                max_hedges: 4,
                ..HedgeConfig::default()
            },
        );
        let heard = Arc::new(Mutex::new(Vec::new()));
        let sink = heard.clone();
        let hook: FirstTokenHook = Arc::new(move |elapsed| sink.lock().unwrap().push(elapsed));
        let started = Instant::now();
        let done = with_first_token_hook(hook, client.complete(request()))
            .await
            .unwrap();
        assert_eq!(done.text, "call-1", "the hedge answered first");
        assert!(
            started.elapsed() < Duration::from_millis(1_000),
            "not held by the slow copy"
        );
        assert_eq!(inner.calls.load(Ordering::SeqCst), 2);
        assert!(
            inner.cancelled[0].load(Ordering::SeqCst),
            "the slow copy was dropped"
        );
        assert!(!inner.cancelled[1].load(Ordering::SeqCst));
        // Both copies carried the same request.
        let requests = inner.requests.lock().unwrap();
        assert_eq!(requests[0].user, requests[1].user);
        assert_eq!(requests[0].system, requests[1].system);
        assert_eq!(requests[0].max_tokens, requests[1].max_tokens);
        // The scheduler's hook heard one first token, timed from the start.
        let heard = heard.lock().unwrap();
        assert_eq!(heard.len(), 1);
        assert!(heard[0] >= Duration::from_millis(80));
        assert!(done.elapsed >= Duration::from_millis(80));
        assert_eq!(
            client.hedger().stats(),
            HedgeStats {
                launched: 1,
                won: 1,
                cooldowns: 0
            }
        );
    }

    #[tokio::test]
    async fn no_hedge_when_the_first_token_comes_before_the_threshold() {
        let inner = Scripted::new(
            vec![Duration::from_millis(10), Duration::from_millis(10)],
            Duration::from_millis(200),
        );
        let client = HedgeClient::new(
            inner.clone(),
            HedgeConfig {
                after: Duration::from_millis(100),
                max_hedges: 4,
                ..HedgeConfig::default()
            },
        );
        let done = client.complete(request()).await.unwrap();
        assert_eq!(done.text, "call-0");
        assert_eq!(inner.calls.load(Ordering::SeqCst), 1, "no copy was sent");
        assert!(!inner.cancelled[0].load(Ordering::SeqCst));
        assert_eq!(client.hedger().stats(), HedgeStats::default());
    }

    #[tokio::test]
    async fn the_original_wins_when_its_token_comes_first_after_a_copy_was_sent() {
        let inner = Scripted::new(
            vec![Duration::from_millis(120), Duration::from_millis(1_500)],
            Duration::from_millis(20),
        );
        let hedger = Hedger::new(HedgeConfig {
            after: Duration::from_millis(40),
            max_hedges: 4,
            ..HedgeConfig::default()
        });
        let (done, outcome) = hedger.race(|_| inner.complete(request())).await;
        assert_eq!(done.unwrap().text, "call-0");
        assert!(outcome.launched);
        assert_eq!(outcome.winner, 0);
        assert!(
            inner.cancelled[1].load(Ordering::SeqCst),
            "the copy was dropped"
        );
        assert_eq!(hedger.stats().won, 0);
    }

    #[tokio::test]
    async fn the_budget_caps_the_copies() {
        let inner = Scripted::new(
            vec![Duration::from_millis(150); 4],
            Duration::from_millis(0),
        );
        let hedger = Hedger::new(HedgeConfig {
            after: Duration::from_millis(20),
            max_hedges: 1,
            ..HedgeConfig::default()
        });
        let (_, first) = hedger.race(|_| inner.complete(request())).await;
        assert!(first.launched);
        let (_, second) = hedger.race(|_| inner.complete(request())).await;
        assert!(!second.launched, "the budget is spent");
        assert_eq!(inner.calls.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn a_failed_copy_leaves_the_race_to_the_other() {
        struct FailFirst(AtomicUsize);
        #[async_trait]
        impl ModelClient for FailFirst {
            async fn complete(&self, _: CompletionRequest) -> Result<Completion> {
                let call = self.0.fetch_add(1, Ordering::SeqCst);
                if call == 0 {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    anyhow::bail!("stream reset");
                }
                tokio::time::sleep(Duration::from_millis(150)).await;
                report_first_token(Duration::from_millis(150));
                Ok(completion("copy"))
            }
        }
        let client = HedgeClient::new(
            Arc::new(FailFirst(AtomicUsize::new(0))),
            HedgeConfig {
                after: Duration::from_millis(30),
                max_hedges: 2,
                ..HedgeConfig::default()
            },
        );
        assert_eq!(client.complete(request()).await.unwrap().text, "copy");
    }

    #[tokio::test]
    async fn cache_primes_are_never_hedged() {
        let inner = Scripted::new(
            vec![Duration::from_millis(120), Duration::from_millis(10)],
            Duration::from_millis(0),
        );
        let client = HedgeClient::new(
            inner.clone(),
            HedgeConfig {
                after: Duration::from_millis(10),
                max_hedges: 4,
                ..HedgeConfig::default()
            },
        );
        let mut prime = request();
        prime.allow_empty = true;
        assert_eq!(client.complete(prime).await.unwrap().text, "call-0");
        assert_eq!(inner.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_winner_that_fails_after_its_first_token_ends_the_race_at_once() {
        // Call 0 reports its first token and fails in the same poll; call 1
        // (the copy) is still waiting. The race must not hang on it.
        struct TokenThenFail {
            calls: AtomicUsize,
            cancelled: Arc<AtomicBool>,
        }
        #[async_trait]
        impl ModelClient for TokenThenFail {
            async fn complete(&self, _: CompletionRequest) -> Result<Completion> {
                let call = self.calls.fetch_add(1, Ordering::SeqCst);
                if call == 0 {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    report_first_token(Duration::from_millis(100));
                    anyhow::bail!("stream error after the first token");
                }
                let mut flag = DropFlag(self.cancelled.clone(), false);
                tokio::time::sleep(Duration::from_secs(5)).await;
                flag.1 = true;
                Ok(completion("copy"))
            }
        }
        let inner = Arc::new(TokenThenFail {
            calls: AtomicUsize::new(0),
            cancelled: Arc::new(AtomicBool::new(false)),
        });
        let hedger = Hedger::new(HedgeConfig {
            after: Duration::from_millis(30),
            max_hedges: 2,
            ..HedgeConfig::default()
        });
        let started = Instant::now();
        let (done, outcome) = hedger.race(|_| inner.complete(request())).await;
        assert!(done.is_err());
        assert!(outcome.launched);
        assert_eq!(outcome.winner, 0);
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "the race returned well before the copy would finish"
        );
        assert!(
            inner.cancelled.load(Ordering::SeqCst),
            "the copy was dropped"
        );
    }

    #[tokio::test]
    async fn a_rate_limited_copy_pauses_further_copies() {
        // Call 1 (the first race's copy) is rate limited; the original wins.
        struct LimitTheCopy(AtomicUsize);
        #[async_trait]
        impl ModelClient for LimitTheCopy {
            async fn complete(&self, _: CompletionRequest) -> Result<Completion> {
                let call = self.0.fetch_add(1, Ordering::SeqCst);
                if call == 1 {
                    return Err(RateLimitedError { retry_after: None }.into());
                }
                tokio::time::sleep(Duration::from_millis(150)).await;
                report_first_token(Duration::from_millis(150));
                Ok(completion(&format!("call-{call}")))
            }
        }
        let inner = LimitTheCopy(AtomicUsize::new(0));
        let hedger = Hedger::new(HedgeConfig {
            after: Duration::from_millis(20),
            max_hedges: 8,
            ..HedgeConfig::default()
        });
        let (done, first) = hedger.race(|_| inner.complete(request())).await;
        assert_eq!(done.unwrap().text, "call-0");
        assert!(first.launched);
        assert_eq!(hedger.stats().cooldowns, 1);
        let (done, second) = hedger.race(|_| inner.complete(request())).await;
        assert_eq!(done.unwrap().text, "call-2");
        assert!(!second.launched, "no copy during the pause");
        assert_eq!(inner.0.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn a_typed_rate_limit_wins_over_an_untyped_error_when_both_fail() {
        struct BothFail(AtomicUsize);
        #[async_trait]
        impl ModelClient for BothFail {
            async fn complete(&self, _: CompletionRequest) -> Result<Completion> {
                let call = self.0.fetch_add(1, Ordering::SeqCst);
                if call == 0 {
                    tokio::time::sleep(Duration::from_millis(80)).await;
                    anyhow::bail!("stream reset");
                }
                tokio::time::sleep(Duration::from_millis(120)).await;
                Err(RateLimitedError {
                    retry_after: Some(Duration::from_secs(2)),
                }
                .into())
            }
        }
        let inner = BothFail(AtomicUsize::new(0));
        let hedger = Hedger::new(HedgeConfig {
            after: Duration::from_millis(20),
            max_hedges: 2,
            ..HedgeConfig::default()
        });
        let (done, _) = hedger.race(|_| inner.complete(request())).await;
        let error = done.unwrap_err();
        assert!(error.downcast_ref::<RateLimitedError>().is_some());
    }

    #[tokio::test]
    async fn the_in_flight_cap_holds_back_a_burst_of_copies() {
        let inner = Scripted::new(
            vec![Duration::from_millis(200); 4],
            Duration::from_millis(0),
        );
        let hedger = Hedger::new(HedgeConfig {
            after: Duration::from_millis(20),
            max_hedges: 8,
            max_in_flight: 1,
            ..HedgeConfig::default()
        });
        let ((_, a), (_, b)) = tokio::join!(
            hedger.race(|_| inner.complete(request())),
            hedger.race(|_| inner.complete(request())),
        );
        assert_eq!(
            usize::from(a.launched) + usize::from(b.launched),
            1,
            "one copy at a time"
        );
        assert_eq!(inner.calls.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn hidden_reasoning_decides_the_race_before_any_visible_token() {
        // Call 0 starts reasoning at 50 ms but shows text only at 400 ms;
        // the copy sent at 20 ms would show text at 120 ms. Reasoning
        // started first, so the original wins and the copy is dropped.
        struct Reasoner {
            calls: AtomicUsize,
            cancelled: Arc<AtomicBool>,
        }
        #[async_trait]
        impl ModelClient for Reasoner {
            async fn complete(&self, _: CompletionRequest) -> Result<Completion> {
                let call = self.calls.fetch_add(1, Ordering::SeqCst);
                if call == 0 {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    crate::client::report_stream_started(Duration::from_millis(50));
                    tokio::time::sleep(Duration::from_millis(350)).await;
                    report_first_token(Duration::from_millis(400));
                    return Ok(completion("reasoned"));
                }
                let mut flag = DropFlag(self.cancelled.clone(), false);
                tokio::time::sleep(Duration::from_millis(100)).await;
                report_first_token(Duration::from_millis(100));
                flag.1 = true;
                Ok(completion("copy"))
            }
        }
        let inner = Reasoner {
            calls: AtomicUsize::new(0),
            cancelled: Arc::new(AtomicBool::new(false)),
        };
        let hedger = Hedger::new(HedgeConfig {
            after: Duration::from_millis(20),
            max_hedges: 2,
            ..HedgeConfig::default()
        });
        let heard = Arc::new(Mutex::new(Vec::new()));
        let sink = heard.clone();
        let hook: FirstTokenHook = Arc::new(move |elapsed| sink.lock().unwrap().push(elapsed));
        let (done, outcome) =
            with_first_token_hook(hook, hedger.race(|_| inner.complete(request()))).await;
        assert_eq!(done.unwrap().text, "reasoned");
        assert!(outcome.launched);
        assert_eq!(outcome.winner, 0);
        assert!(
            inner.cancelled.load(Ordering::SeqCst),
            "the copy was dropped"
        );
        // The scheduler still hears the visible first token, once.
        assert_eq!(*heard.lock().unwrap(), vec![Duration::from_millis(400)]);
    }
}
