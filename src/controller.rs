//! Adaptive concurrency controller (PLAN.md Phase 2).
//!
//! An additive-increase / multiplicative-decrease controller that replaces the
//! fixed in-flight cap with a dynamic target, using `--max-concurrency` only
//! as a hard ceiling. Each evaluation window looks at completed requests and:
//!
//! - adds `additive_step` slots after a healthy window (no failures, no TTFT
//!   regression), unless the controller is cooling down after a decrease;
//! - keeps only `decrease_percent` of the limit after failures or sustained
//!   TTFT regression, then waits `cooldown_windows` before climbing again.
//!
//! The goal is to sit near the measured throughput knee without prior
//! knowledge of the server's batch capacity, and to shed load faster than it
//! is re-admitted when the server starts queueing.

use serde::Serialize;
use std::time::{Duration, Instant};

#[derive(Debug, Clone)]
pub struct ControllerConfig {
    /// Lower bound for the dynamic limit.
    pub min_limit: usize,
    /// Slots added after each healthy window.
    pub additive_step: usize,
    /// Percentage of the current limit kept after a decrease (60 = keep 60%).
    pub decrease_percent: u32,
    /// How often the observation window closes.
    pub window: Duration,
    /// Healthy windows to skip after a decrease before increasing again.
    pub cooldown_windows: u32,
    /// Close the window early once this many events arrive.
    pub max_events_per_window: usize,
    /// Percentage of the ceiling the limit starts at (1-100). The A/B data
    /// showed a 25% start costs ~2x wall-clock on short healthy bursts versus
    /// a known-safe fixed cap; 50% matches the sweep-measured knee for this
    /// class of server while the AIMD machinery still protects the ramp.
    pub start_percent: u32,
    /// Absolute TTFT ceiling in milliseconds. A window whose median TTFT
    /// exceeds this is treated as overload regardless of the EMA baseline
    /// (which can be seeded already-degraded under a burst). Zero disables
    /// the check. Defaults to 12s: healthy Muse-class effort=low TTFT sits
    /// around 2-10s even under moderate concurrency; beyond it, queues are
    /// forming and streams begin truncating.
    pub max_ttft_ms: u64,
    /// Hard cap on the admission limit at cold start (see
    /// `DEFAULT_COLD_START_BURST_CAP`). Configurable so the cap itself can be
    /// A/B'd: it was measured under default-effort load shedding, and waves
    /// now start after cache primes have warmed the server.
    pub cold_burst_cap: usize,
    /// Task-count-aware cold start (measured follow-up to the
    /// knee sweep). When the scheduler knows how many tasks are ready at wave
    /// start, the admission limit seeds at that count instead of the fixed
    /// burst share: a 5-task DAG starts at 5 (below the old floor of 8), a
    /// 48-task wave starts at `cold_hint_cap` instead of climbing one
    /// additive step per window. The cap default is 16: the fixed-width
    /// sweep found no provider knee to c=48 with ~0 retries, and the adaptive
    /// plateau sat at peak 16-20, so 16 is the measured fast-and-safe start.
    /// Zero disables the hint (legacy burst-share start). The AIMD rails are
    /// unchanged: overload still cuts the limit 60% per window.
    pub cold_hint_cap: usize,
    /// Fast ramp (measured follow-up to the knee sweep): while no
    /// overload has ever fired, the additive step DOUBLES after each healthy
    /// window (4 -> 8 -> 16, capped), so a healthy wave reaches the measured
    /// no-knee region (c=48 fixed width, ~0 retries, on both providers) in 2-3
    /// windows instead of 8+. The first real overload (failure fraction, TTFT
    /// regression, or TTFT ceiling) permanently reverts to the fixed
    /// `additive_step` — the controller then behaves exactly as legacy AIMD.
    /// The safety rails are unchanged: every window is still evaluated for
    /// overload before any growth is applied. Default on since the A/B:
    /// -24.4% wall / +32.8% tok/s Muse and -11.9% / +9.9% DeepSeek on 49-task
    /// healthy waves, neutral when the server is the binding constraint, and
    /// legacy reaches the same ceiling on long waves where the ramp's edge
    /// shrinks to zero.
    pub fast_ramp: bool,
}

impl Default for ControllerConfig {
    fn default() -> Self {
        Self {
            min_limit: 4,
            additive_step: 4,
            decrease_percent: 60,
            window: Duration::from_millis(2_000),
            cooldown_windows: 2,
            max_events_per_window: 16,
            start_percent: 25,
            max_ttft_ms: 12_000,
            cold_burst_cap: DEFAULT_COLD_START_BURST_CAP,
            cold_hint_cap: 16,
            fast_ramp: true,
        }
    }
}

impl ControllerConfig {
    pub fn validate(&self) -> Result<(), String> {
        if self.min_limit == 0 {
            return Err("controller min_limit must be greater than zero".to_string());
        }
        if self.additive_step == 0 {
            return Err("controller additive_step must be greater than zero".to_string());
        }
        if !(1..99).contains(&self.decrease_percent) {
            return Err("controller decrease_percent must be between 1 and 99".to_string());
        }
        if self.window.is_zero() {
            return Err("controller window must be greater than zero".to_string());
        }
        if self.max_events_per_window == 0 {
            return Err("controller max_events_per_window must be greater than zero".to_string());
        }
        if !(1..=100).contains(&self.start_percent) {
            return Err("controller start_percent must be between 1 and 100".to_string());
        }
        Ok(())
    }
}

/// One completed request observed by the controller.
#[derive(Debug, Clone, Copy)]
pub struct WindowEvent {
    pub success: bool,
    pub output_tokens: u64,
    pub duration: Duration,
    pub time_to_first_token: Option<Duration>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct ControllerStats {
    pub windows_evaluated: u64,
    pub increases: u64,
    pub decreases: u64,
}

/// AIMD concurrency controller. Deterministic: no clocks are read inside
/// `evaluate`, the caller passes `now`, which keeps the policy unit-testable.
pub struct ConcurrencyController {
    config: ControllerConfig,
    ceiling: usize,
    limit: usize,
    window_started: Instant,
    events: Vec<WindowEvent>,
    cooldown: u32,
    ttft_baseline_ms: Option<f64>,
    stats: ControllerStats,
    /// Current climb step; doubles per healthy window while the fast ramp is
    /// active, resets to `config.additive_step` on the first overload.
    ramp_step: usize,
    /// Whether the next healthy window still earns a doubled step. Cleared
    /// permanently on the first overload signal.
    fast_ramp_active: bool,
}

/// TTFT is considered regressed when it exceeds this multiple of the EMA
/// baseline observed over healthy windows.
const TTFT_REGRESSION_FACTOR: f64 = 1.5;
/// Minimum events with TTFT before a regression reading is trusted.
const MIN_TTFT_SAMPLES: usize = 4;
/// EMA weight for the healthy-window TTFT baseline.
const BASELINE_EMA: f64 = 0.3;
/// Upper bound for the fast-ramp doubling step. From the seeded hint of
/// 16, a capped step of 16 reaches the measured no-knee ceiling (48) in
/// two healthy windows; without a cap, a long quiet period could double the
/// step to absurd per-window growth before any overload signal could land.
const FAST_RAMP_MAX_STEP: usize = 16;
/// Fraction of window events that must fail before the window counts as
/// overload. Measured against the Muse A/B data: a 32-request burst sharing a
/// 150 RPM team window routinely eats a few 429s; cutting concurrency 40% for
/// that punished the arm exactly when the minute window freed up, costing ~4x
/// wall-clock versus the fixed cap. Sparse failures hold the climb instead.
const DECREASE_FAILURE_FRACTION: f64 = 0.25;
/// Hard cap on the admission limit at cold start. With the CLI's 128-wide
/// safety ceiling, a 25% start share would launch 32 simultaneous cold
/// requests; Muse-class servers at effort=low shed that burst with empty
/// completions (measured: an 11-wide cold burst lost 10/11 tasks). The
/// additive increase widens the window once traffic is warm, so the cap
/// only costs a couple of windows on healthy ramps.
pub const DEFAULT_COLD_START_BURST_CAP: usize = 8;

impl ConcurrencyController {
    pub fn new(config: ControllerConfig, ceiling: usize) -> Self {
        Self::new_at(config, ceiling, None, Instant::now())
    }

    /// Task-count-aware cold start: seed the admission limit with the
    /// number of tasks ready at wave start, capped by
    /// `ControllerConfig.cold_hint_cap`. See the field doc for the measured
    /// basis. AIMD rails unchanged.
    pub fn new_with_hint(config: ControllerConfig, ceiling: usize, ready: usize) -> Self {
        Self::new_at(config, ceiling, Some(ready), Instant::now())
    }

    /// Testable constructor with an explicit window start.
    fn new_at(
        config: ControllerConfig,
        ceiling: usize,
        ready: Option<usize>,
        now: Instant,
    ) -> Self {
        let ceiling = ceiling.max(1);
        let floor = config.min_limit.max(1).min(ceiling);
        let limit = match ready {
            // Hinted start: ready count, clamped to [floor, ceiling] and
            // capped by the measured hint cap. Ignores cold_burst_cap — the
            // hint IS the burst plan (see cold_hint_cap docs).
            Some(ready) if config.cold_hint_cap > 0 => ready
                .clamp(floor, ceiling)
                .min(config.cold_hint_cap.max(floor)),
            // Legacy start: a configurable share of the ceiling, never below
            // the floor and never above the ceiling itself, and never above
            // the cold-start burst cap.
            _ => {
                let burst_cap = floor.max(config.cold_burst_cap).min(ceiling);
                (ceiling * config.start_percent as usize / 100)
                    .clamp(floor, ceiling)
                    .min(burst_cap)
            }
        };
        let fast_ramp_active = config.fast_ramp;
        let ramp_step = config.additive_step;
        Self {
            config,
            ceiling,
            limit,
            window_started: now,
            events: Vec::new(),
            cooldown: 0,
            ttft_baseline_ms: None,
            stats: ControllerStats::default(),
            ramp_step,
            fast_ramp_active,
        }
    }

    /// Current dynamic admission cap.
    pub fn limit(&self) -> usize {
        self.limit
    }

    pub fn stats(&self) -> &ControllerStats {
        &self.stats
    }

    pub fn window(&self) -> Duration {
        self.config.window
    }

    pub fn record(&mut self, event: WindowEvent) {
        self.events.push(event);
    }

    /// Evaluate the window if it has closed or filled. Returns the new limit
    /// when it changed.
    pub fn maybe_evaluate(&mut self, now: Instant) -> Option<usize> {
        let window_closed = now.duration_since(self.window_started) >= self.config.window;
        let window_full = self.events.len() >= self.config.max_events_per_window;
        if window_closed || window_full {
            self.evaluate(now)
        } else {
            None
        }
    }

    pub fn evaluate(&mut self, now: Instant) -> Option<usize> {
        if self.events.is_empty() {
            self.window_started = now;
            return None;
        }
        let events = std::mem::take(&mut self.events);
        self.window_started = now;
        self.stats.windows_evaluated += 1;

        let failures = events.iter().filter(|event| !event.success).count();
        let ttft_ms: Vec<f64> = events
            .iter()
            .filter_map(|event| event.time_to_first_token)
            .map(|duration| duration.as_secs_f64() * 1_000.0)
            .collect();
        // The median keeps one outlier (a poisoned backend, a cold cache miss)
        // from masquerading as a whole-window regression; the mean would let
        // a single 3x-slow request trigger a 40% AIMD cut.
        let median_ttft_ms = (!ttft_ms.is_empty()).then(|| {
            let mut sorted = ttft_ms.clone();
            sorted.sort_by(f64::total_cmp);
            let mid = sorted.len() / 2;
            if sorted.len() % 2 == 1 {
                sorted[mid]
            } else {
                (sorted[mid - 1] + sorted[mid]) / 2.0
            }
        });
        let enough_ttft_samples = ttft_ms.len() >= MIN_TTFT_SAMPLES;

        let ttft_regression = matches!(
            (self.ttft_baseline_ms, median_ttft_ms),
            (Some(baseline), Some(median))
                if enough_ttft_samples && median > baseline * TTFT_REGRESSION_FACTOR
        );

        // Absolute TTFT ceiling: the EMA baseline is relative and can be
        // seeded already-degraded under a burst, so a window where every
        // request is slow looks "normal" (measured: width 36 held a
        // 10-17s baseline while median TTFT climbed to 24s and streams
        // started truncating; the relative check never fired). Median
        // above the hard cap is overload regardless of the baseline.
        let ttft_ceiling_breach = matches!(median_ttft_ms, Some(median)
            if enough_ttft_samples
                && self.config.max_ttft_ms > 0
                && median > self.config.max_ttft_ms as f64);

        // Sparse failures (< a quarter of the window) mean "hold", not
        // "decrease": on a shared RPM budget a burst routinely eats a few
        // 429s, and cutting concurrency 40% for that punishes the run exactly
        // when the rate window frees up (measured: ~4x wall-clock vs a fixed
        // cap on short healthy bursts). Overload is a heavy failure fraction
        // or a real TTFT regression.
        let failure_overload = failures as f64 >= events.len() as f64 * DECREASE_FAILURE_FRACTION;
        let new_limit = if !failure_overload && !ttft_regression && !ttft_ceiling_breach {
            // Seed and update the baseline only from windows with enough
            // samples: a 1-2 sample window (common at cold start) would
            // otherwise freeze a lucky-fast baseline and pin the limit at
            // the floor, or freeze an unlucky-slow one and blind the
            // controller to real regressions.
            if enough_ttft_samples {
                if let Some(median) = median_ttft_ms {
                    self.ttft_baseline_ms = Some(match self.ttft_baseline_ms {
                        Some(baseline) => baseline * (1.0 - BASELINE_EMA) + median * BASELINE_EMA,
                        None => median,
                    });
                }
            }
            if failures > 0 {
                // Sparse-failure window: hold the limit. Climbing would pour
                // more arrivals into the edge of a shared rate budget; a
                // decrease for a couple of 429s costs more than it saves.
                None
            } else if self.cooldown > 0 {
                // Hysteresis: absorb a healthy window without climbing.
                self.cooldown -= 1;
                None
            } else {
                let next = self.limit.saturating_add(self.ramp_step).min(self.ceiling);
                if next > self.limit {
                    self.stats.increases += 1;
                    if self.fast_ramp_active {
                        self.ramp_step = (self.ramp_step * 2).min(FAST_RAMP_MAX_STEP);
                    }
                    Some(next)
                } else {
                    None
                }
            }
        } else {
            self.cooldown = self.config.cooldown_windows;
            // The fast ramp ends at the first real overload: from here the
            // controller is legacy AIMD on the fixed additive step.
            self.fast_ramp_active = false;
            self.ramp_step = self.config.additive_step;
            let next = (self.limit * self.config.decrease_percent as usize / 100)
                .max(self.config.min_limit);
            if next < self.limit {
                self.stats.decreases += 1;
                Some(next)
            } else {
                None
            }
        };

        if let Some(limit) = new_limit {
            self.limit = limit;
        }
        new_limit
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> ControllerConfig {
        ControllerConfig {
            min_limit: 4,
            additive_step: 4,
            decrease_percent: 60,
            window: Duration::from_millis(1_000),
            cooldown_windows: 2,
            max_events_per_window: 16,
            start_percent: 25,
            // Disabled in arithmetic-focused tests; the absolute ceiling has
            // its own test below.
            max_ttft_ms: 0,
            cold_burst_cap: DEFAULT_COLD_START_BURST_CAP,
            cold_hint_cap: 16,
            fast_ramp: false,
        }
    }

    fn success(ttft_ms: u64) -> WindowEvent {
        WindowEvent {
            success: true,
            output_tokens: 100,
            duration: Duration::from_millis(500),
            time_to_first_token: Some(Duration::from_millis(ttft_ms)),
        }
    }

    fn failure() -> WindowEvent {
        WindowEvent {
            success: false,
            output_tokens: 0,
            duration: Duration::from_millis(100),
            time_to_first_token: None,
        }
    }

    /// Close the window at `now` with a batch of healthy events.
    fn healthy_window_at(controller: &mut ConcurrencyController, now: Instant) -> Option<usize> {
        for _ in 0..6 {
            controller.record(success(100));
        }
        controller.maybe_evaluate(now)
    }

    #[test]
    fn starts_conservative_and_increases_on_healthy_windows() {
        let t0 = Instant::now();
        let mut controller = ConcurrencyController::new_at(config(), 128, None, t0);
        // 25% of 128 = 32, clamped to the cold-start burst cap.
        assert_eq!(controller.limit(), 8);

        assert_eq!(
            healthy_window_at(&mut controller, t0 + Duration::from_secs(1)),
            Some(12)
        );
        assert_eq!(
            healthy_window_at(&mut controller, t0 + Duration::from_secs(2)),
            Some(16)
        );
        assert_eq!(controller.stats().increases, 2);
        assert_eq!(controller.stats().decreases, 0);
    }

    #[test]
    fn never_exceeds_the_ceiling() {
        let t0 = Instant::now();
        let mut controller = ConcurrencyController::new_at(config(), 40, None, t0);
        assert_eq!(controller.limit(), 8);
        for round in 1..=20 {
            healthy_window_at(&mut controller, t0 + Duration::from_secs(round));
        }
        assert_eq!(controller.limit(), 40);
    }

    #[test]
    fn decreases_multiplicatively_on_failure() {
        let t0 = Instant::now();
        let mut controller = ConcurrencyController::new_at(config(), 128, None, t0);
        controller.record(failure());
        let new_limit = controller.evaluate(t0 + Duration::from_secs(1));
        assert_eq!(new_limit, Some(4)); // 8 * 60%, floored at min_limit
        assert_eq!(controller.limit(), 4);

        // Repeated failures floor out at min_limit instead of reaching zero.
        for round in 2..=12 {
            controller.record(failure());
            controller.evaluate(t0 + Duration::from_secs(round));
        }
        assert_eq!(controller.limit(), 4);
    }

    #[test]
    fn sparse_failures_hold_instead_of_decreasing() {
        // Regression measured on Muse A/B runs: a 32-request burst on a
        // shared 150 RPM budget eats a few 429s; the old any-failure policy
        // cut concurrency 40% for that and the arm never recovered inside a
        // short round (~4x wall-clock vs a fixed cap). Sparse failures must
        // hold (or climb), not decrease.
        let t0 = Instant::now();
        let mut controller = ConcurrencyController::new_at(config(), 128, None, t0);
        for _ in 0..12 {
            controller.record(success(100));
        }
        controller.record(failure());
        controller.record(failure());
        // 2/14 failures = 14% < 25%: no decrease, cooldown not armed.
        assert_eq!(controller.evaluate(t0 + Duration::from_secs(1)), None);
        assert_eq!(controller.limit(), 8);
        assert_eq!(controller.stats().decreases, 0);

        // The next fully healthy window climbs as usual.
        assert_eq!(
            healthy_window_at(&mut controller, t0 + Duration::from_secs(2)),
            Some(12)
        );
    }

    #[test]
    fn heavy_failure_fraction_still_decreases() {
        let t0 = Instant::now();
        let mut controller = ConcurrencyController::new_at(config(), 128, None, t0);
        for _ in 0..4 {
            controller.record(success(100));
        }
        for _ in 0..4 {
            controller.record(failure());
        }
        // 4/8 failures = 50%: real overload, multiplicative decrease fires.
        assert!(controller
            .evaluate(t0 + Duration::from_secs(1))
            .is_some_and(|limit| limit < 32));
        assert!(controller.stats().decreases >= 1);
    }

    #[test]
    fn cooldown_blocks_the_climb_after_a_decrease() {
        let t0 = Instant::now();
        let mut controller = ConcurrencyController::new_at(config(), 128, None, t0);
        controller.record(failure());
        controller.evaluate(t0 + Duration::from_secs(1));
        assert_eq!(controller.limit(), 4);

        // Two healthy windows are absorbed as cooldown; the third climbs.
        assert_eq!(
            healthy_window_at(&mut controller, t0 + Duration::from_secs(2)),
            None
        );
        assert_eq!(
            healthy_window_at(&mut controller, t0 + Duration::from_secs(3)),
            None
        );
        assert_eq!(
            healthy_window_at(&mut controller, t0 + Duration::from_secs(4)),
            Some(8)
        );
        assert_eq!(controller.stats().increases, 1);
    }

    #[test]
    fn ttft_regression_triggers_a_decrease() {
        let t0 = Instant::now();
        let mut controller = ConcurrencyController::new_at(config(), 128, None, t0);
        // Establish a healthy baseline around 100 ms TTFT.
        healthy_window_at(&mut controller, t0 + Duration::from_secs(1));

        // Next window: same shape but TTFT quadruples, past the regression factor.
        for _ in 0..6 {
            controller.record(success(400));
        }
        let new_limit = controller.maybe_evaluate(t0 + Duration::from_secs(2));
        assert!(new_limit.is_some_and(|limit| limit < 36));
        assert!(controller.stats().decreases >= 1);
    }

    #[test]
    fn tiny_first_window_does_not_seed_the_baseline() {
        // Regression: `None => mean` seeded the TTFT baseline from as few as
        // one sample, so a single lucky-fast cold-start completion froze a
        // nonsense baseline and every normal window afterwards read as a
        // regression, pinning the limit at the floor.
        let t0 = Instant::now();
        let mut controller = ConcurrencyController::new_at(config(), 128, None, t0);

        // Window 1: one 5 ms completion — far below MIN_TTFT_SAMPLES.
        // Healthy (no failures), so the limit climbs from the burst-capped
        // start of 8 to 12.
        controller.record(success(5));
        assert_eq!(
            controller.maybe_evaluate(t0 + Duration::from_secs(1)),
            Some(12)
        );

        // Window 2: a full window at the true steady-state TTFT. This must be
        // read as healthy (baseline seeds at 100 ms), not as a regression
        // against a 5 ms baseline.
        for _ in 0..6 {
            controller.record(success(100));
        }
        assert_eq!(
            controller.maybe_evaluate(t0 + Duration::from_secs(2)),
            Some(16)
        );
        assert_eq!(controller.stats().decreases, 0);
    }

    #[test]
    fn single_outlier_does_not_trigger_regression_decrease() {
        // The window median, not the mean, drives the regression check: one
        // poisoned request must not be able to trigger a 40% AIMD cut.
        let t0 = Instant::now();
        let mut controller = ConcurrencyController::new_at(config(), 128, None, t0);
        healthy_window_at(&mut controller, t0 + Duration::from_secs(1));

        // 3 healthy samples + one 4x outlier. Mean = 175 > 150 (old behavior:
        // decrease); median = 100 (healthy climb).
        for _ in 0..3 {
            controller.record(success(100));
        }
        controller.record(success(400));
        assert_eq!(
            controller.maybe_evaluate(t0 + Duration::from_secs(2)),
            Some(16)
        );
        assert_eq!(controller.stats().decreases, 0);
    }

    #[test]
    fn absolute_ttft_ceiling_decreases_regardless_of_baseline() {
        // Regression measured on the architect wave: width grew to 36, every
        // request queued ~10-17s, the EMA baseline seeded at that degraded
        // level, and median TTFT climbing to 24s never tripped the relative
        // 1.5x check. The hard ceiling must cut regardless of the baseline.
        let t0 = Instant::now();
        let mut cfg = config();
        cfg.max_ttft_ms = 12_000;
        let mut controller = ConcurrencyController::new_at(cfg, 128, None, t0);

        // A healthy window first, seeding a ~10s baseline and climbing 8 -> 12.
        for _ in 0..6 {
            controller.record(success(10_000));
        }
        assert_eq!(
            controller.maybe_evaluate(t0 + Duration::from_secs(1)),
            Some(12)
        );

        // Next window: every sample sits ABOVE the 12s cap but BELOW the
        // relative trip line (1.5x the 10s baseline = 15s) — only the
        // absolute ceiling can catch this window.
        for _ in 0..6 {
            controller.record(success(13_000));
        }
        let new_limit = controller.maybe_evaluate(t0 + Duration::from_secs(2));
        assert!(
            new_limit.is_some_and(|limit| limit < 12),
            "ceiling breach must cut"
        );
        assert!(controller.stats().decreases >= 1);
    }

    #[test]
    fn disabled_ceiling_keeps_relative_only_behavior() {
        let t0 = Instant::now();
        let mut controller = ConcurrencyController::new_at(config(), 128, None, t0);
        for _ in 0..6 {
            controller.record(success(500));
        }
        assert_eq!(
            controller.maybe_evaluate(t0 + Duration::from_secs(1)),
            Some(12)
        );
    }

    #[test]
    fn small_ceilings_do_not_panic() {
        // Regression: clamp() with min > ceiling used to panic when tests ran
        // plans with max_concurrency below the controller floor.
        for ceiling in [1, 2, 4] {
            let controller = ConcurrencyController::new(config(), ceiling);
            assert!(controller.limit() >= 1 && controller.limit() <= ceiling);
        }
    }

    #[test]
    fn invalid_config_is_rejected() {
        let mut bad = config();
        bad.decrease_percent = 100;
        assert!(bad.validate().is_err());
        bad.decrease_percent = 60;
        bad.min_limit = 0;
        assert!(bad.validate().is_err());
        bad.min_limit = 4;
        bad.start_percent = 0;
        assert!(bad.validate().is_err());
        bad.start_percent = 101;
        assert!(bad.validate().is_err());
    }

    #[test]
    fn start_percent_shapes_the_initial_limit_up_to_the_burst_cap() {
        let t0 = Instant::now();
        // Below the burst cap the start share applies directly.
        let mut cfg = config();
        cfg.start_percent = 50;
        let controller = ConcurrencyController::new_at(cfg.clone(), 8, None, t0);
        assert_eq!(controller.limit(), 4);

        // Past the cap the cold-start burst is clamped regardless of share:
        // 50% of 256 would launch 128 simultaneous cold requests, which the
        // server sheds with empty completions.
        let controller = ConcurrencyController::new_at(cfg, 256, None, t0);
        assert_eq!(controller.limit(), DEFAULT_COLD_START_BURST_CAP);

        // Clamped to the floor when the ceiling is tiny.
        let mut cfg = config();
        cfg.start_percent = 50;
        let controller = ConcurrencyController::new_at(cfg, 4, None, t0);
        assert!(controller.limit() >= 1 && controller.limit() <= 4);
    }

    #[test]
    fn fast_ramp_doubles_the_step_each_healthy_window_until_the_ceiling() {
        let t0 = Instant::now();
        let mut cfg = config();
        cfg.fast_ramp = true;
        let ceiling = 48;
        let mut controller = ConcurrencyController::new_at(cfg, ceiling, None, t0);
        assert_eq!(controller.limit(), DEFAULT_COLD_START_BURST_CAP);

        // Healthy windows: 8 -> 12 -> 20 -> 36 -> 48, reached in 4 windows.
        let expected = [12, 20, 36, 48];
        for want in expected {
            for _ in 0..16 {
                controller.record(success(100));
            }
            assert_eq!(
                controller.evaluate(t0 + Duration::from_secs(60)),
                Some(want)
            );
            assert_eq!(controller.limit(), want);
        }
        // Ceiling reached: further healthy windows hold, no panic.
        for _ in 0..16 {
            controller.record(success(100));
        }
        assert_eq!(controller.evaluate(t0 + Duration::from_secs(120)), None);
        assert_eq!(controller.limit(), ceiling);
    }

    #[test]
    fn fast_ramp_step_is_capped() {
        let t0 = Instant::now();
        let mut cfg = config();
        cfg.fast_ramp = true;
        cfg.additive_step = 4;
        // Hint-seeded start at 64 (the hint ignores the cold burst cap); the
        // step doubles 4 -> 8 -> 16 and caps at FAST_RAMP_MAX_STEP.
        cfg.cold_hint_cap = 128;
        let mut controller = ConcurrencyController::new_at(cfg, 256, Some(64), t0);
        assert_eq!(controller.limit(), 64);
        let expected = [68, 76, 92, 108]; // +4, +8, +16, +16 (capped)
        for want in expected {
            for _ in 0..16 {
                controller.record(success(100));
            }
            assert_eq!(
                controller.evaluate(t0 + Duration::from_secs(60)),
                Some(want)
            );
        }
    }

    #[test]
    fn fast_ramp_reverts_to_the_fixed_step_after_the_first_overload() {
        let t0 = Instant::now();
        let mut cfg = config();
        cfg.fast_ramp = true;
        let mut controller = ConcurrencyController::new_at(cfg, 256, None, t0);
        // One healthy window: 8 -> 12 (step now 8).
        for _ in 0..16 {
            controller.record(success(100));
        }
        assert_eq!(controller.evaluate(t0 + Duration::from_secs(60)), Some(12));
        // Overload window: 25% failures -> 60% decrease, ramp ends.
        for _ in 0..12 {
            controller.record(success(100));
        }
        for _ in 0..4 {
            controller.record(failure());
        }
        assert_eq!(controller.evaluate(t0 + Duration::from_secs(120)), Some(7));
        // Cooldown (2 windows) absorbs healthy windows without climbing.
        for _ in 0..16 {
            controller.record(success(100));
        }
        assert_eq!(controller.evaluate(t0 + Duration::from_secs(180)), None);
        for _ in 0..16 {
            controller.record(success(100));
        }
        assert_eq!(controller.evaluate(t0 + Duration::from_secs(240)), None);
        // Post-cooldown climb uses the FIXED step (+4), not the doubled one.
        for _ in 0..16 {
            controller.record(success(100));
        }
        assert_eq!(controller.evaluate(t0 + Duration::from_secs(300)), Some(11));
    }

    #[test]
    fn fast_ramp_off_keeps_legacy_additive_climb() {
        let t0 = Instant::now();
        let cfg = config();
        assert!(!cfg.fast_ramp);
        let mut controller = ConcurrencyController::new_at(cfg, 256, None, t0);
        for _ in 0..16 {
            controller.record(success(100));
        }
        assert_eq!(controller.evaluate(t0 + Duration::from_secs(60)), Some(12));
        for _ in 0..16 {
            controller.record(success(100));
        }
        assert_eq!(controller.evaluate(t0 + Duration::from_secs(120)), Some(16));
    }

    #[test]
    fn fast_ramp_survives_a_sparse_failure_hold() {
        let t0 = Instant::now();
        let mut cfg = config();
        cfg.fast_ramp = true;
        let mut controller = ConcurrencyController::new_at(cfg, 256, None, t0);
        // Healthy window: 8 -> 12, step now 8.
        for _ in 0..16 {
            controller.record(success(100));
        }
        assert_eq!(controller.evaluate(t0 + Duration::from_secs(60)), Some(12));
        // Sparse-failure window (1 of 16 < 25%): hold, ramp stays active.
        for _ in 0..15 {
            controller.record(success(100));
        }
        controller.record(failure());
        assert_eq!(controller.evaluate(t0 + Duration::from_secs(120)), None);
        assert_eq!(controller.limit(), 12);
        // Next healthy window doubles from the held step: 12 -> 20.
        for _ in 0..16 {
            controller.record(success(100));
        }
        assert_eq!(controller.evaluate(t0 + Duration::from_secs(180)), Some(20));
    }

    #[test]
    fn cold_start_hint_seeds_the_limit_at_the_ready_count() {
        // A small DAG starts at its actual width instead of the fixed floor.
        let controller = ConcurrencyController::new_with_hint(config(), 128, 5);
        assert_eq!(controller.limit(), 5);

        // A wide wave starts at the measured hint cap (16), not the climb.
        let controller = ConcurrencyController::new_with_hint(config(), 128, 48);
        assert_eq!(controller.limit(), 16);

        // The hint respects the ceiling and the floor.
        assert_eq!(
            ConcurrencyController::new_with_hint(config(), 8, 48).limit(),
            8
        );
        assert_eq!(
            ConcurrencyController::new_with_hint(config(), 128, 2).limit(),
            4
        );

        // cold_hint_cap = 0 disables the hint: legacy burst-share start.
        let mut cfg = config();
        cfg.cold_hint_cap = 0;
        let controller = ConcurrencyController::new_with_hint(cfg, 128, 48);
        assert_eq!(controller.limit(), DEFAULT_COLD_START_BURST_CAP);
    }
}
