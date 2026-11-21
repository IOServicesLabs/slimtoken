//! A streaming OpenAI-compatible client that reports every text delta to the
//! screen. Both arms use this one client, so the two sides have the same
//! transport, the same request shape and the same token accounting.
//!
//! The request body and the stream parser follow the engine's shipped
//! `OpenAiClient`. The shipped client reports only the first token; a live
//! display needs each delta, so the benchmark carries its own copy.

use crate::events::{Arm, Emitter, Ev, Kind};
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use slimtoken::client::prewarm_pool;
use slimtoken::hedge::{HedgeConfig, Hedger};
use slimtoken::{
    async_trait, report_first_token, report_stream_started, report_text, BudgetExhaustedError,
    Completion, CompletionRequest, ModelClient, OverloadedError, PermanentError, PrewarmReport,
    RateLimitedError,
};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

pub struct TapClient {
    http: reqwest::Client,
    endpoint: String,
    model: String,
    api_key: Option<String>,
    arm: Arm,
    emitter: Emitter,
    /// False while the architect plans; true once the wave runs.
    in_wave: AtomicBool,
    /// `pchk`: the persistent checker that gets each wave file as its
    /// request closes.
    feed: std::sync::Mutex<Option<std::sync::Arc<crate::pchk::Sidecar>>>,
    /// `SHOWDOWN_INDEX=indexio`: the index that gets each wave file as its
    /// request closes (`codeindex.rs`).
    index_feed: std::sync::Mutex<Option<std::sync::Arc<crate::codeindex::Indexio>>>,
    /// `{base}/models`, the cheap request of a prewarm (`prewarm`).
    models: String,
    /// `hedge`: an identical copy of a request that has no first token
    /// after a threshold (`slimtoken::hedge`); `None` sends none.
    hedge: Option<std::sync::Arc<Hedger>>,
    /// When the client was built (the arm's start): the clock of `timeline`.
    created: Instant,
    /// The planning timeline (`planpipe` A/B), see [`PlanTimeline`].
    timeline: std::sync::Mutex<PlanTimeline>,
    /// `nocut`: continuations of cut answers (`None`: switch off).
    nocut: Option<NoCut>,
    /// `speedrace`: copies of each plan and file request (`None`: off).
    speed: Option<SpeedRace>,
    /// `fencestop`: the count of file answers stopped once their code block
    /// had closed and they went on in prose ([`fence_overrun`]); `None`:
    /// switch off.
    fence_stop: Option<std::sync::atomic::AtomicUsize>,
}

/// `nocut`: how the client continues a cut answer, and what it did.
struct NoCut {
    /// The provider's prefix-completion endpoint, when it has one.
    prefix: Option<String>,
    /// False once the prefix endpoint refused a request: turns from then on.
    prefix_ok: AtomicBool,
    stats: std::sync::Mutex<NoCutStats>,
}

/// `nocut`'s counts for the `nocut` tool event.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NoCutStats {
    /// File streams that ended at their output budget.
    pub truncated: usize,
    /// Continuation requests sent.
    pub continuations: usize,
    /// Of those, by prefix completion and by user turn.
    pub prefix: usize,
    pub turn: usize,
    /// Continuations that failed (an error, not a cut).
    pub failed: usize,
    /// Prefix requests refused as unsupported (HTTP 404/405/422, or a 400
    /// whose body names the prefix, the beta or an unsupported field), after
    /// which the client used turns.
    pub prefix_refused: usize,
    /// Prefix requests that failed otherwise (a 400 of another cause, a 5xx,
    /// a broken stream): that one request was asked again as a turn, and
    /// prefix completion stays on.
    pub prefix_errors: usize,
    /// The first prefix failure: its HTTP status and the start of its body.
    pub prefix_note: Option<String>,
    /// Files whose joined answer was complete.
    pub joined: usize,
    /// Decode time of the cut streams of the joined files: what a restart
    /// writes again.
    pub cut_ms: u64,
    /// Time of the continuations.
    pub continue_ms: u64,
    /// Estimated time saved: for each joined file, its cut stream's time
    /// minus the first-token waits of its continuations (a restart decodes
    /// the cut part again; a continuation waits for its first token).
    pub saved_ms: u64,
    /// The joined files.
    pub files: Vec<String>,
    /// Cut files not yet joined: title -> (cut stream ms, continuation
    /// first-token ms so far).
    pending: std::collections::BTreeMap<String, (u64, u64)>,
}

/// `speedrace`: the settings and the counts of one client.
struct SpeedRace {
    copies: usize,
    window: Duration,
    /// Race only a request that starts with fewer than this many in flight
    /// (0: every request).
    max_in_flight: usize,
    in_flight: std::sync::atomic::AtomicUsize,
    stats: std::sync::Mutex<SpeedStats>,
}

/// What `speedrace` did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SpeedStats {
    /// Requests raced, and the copies they sent.
    pub races: usize,
    pub copies: usize,
    /// Races that the window decided (the others: a copy finished first).
    pub chosen: usize,
    /// Characters that the dropped copies had written.
    pub dropped_chars: usize,
}

/// One copy of a raced request: where it reports its text, the race's
/// start, and whether a copy has told the scheduler of a first delta and of
/// a first token (one report for the race).
struct Probe<'a> {
    progress: &'a slimtoken::speedrace::Progress,
    began: Instant,
    started: &'a AtomicBool,
    first: &'a AtomicBool,
}

/// The shape of a cut answer, for the record: its lines, how many differ,
/// the line that repeats most, where the second half first repeats the
/// first, and its last characters.
fn cut_shape(text: &str) -> String {
    let lines: Vec<&str> = text
        .lines()
        .map(str::trim)
        .filter(|line| line.len() > 8)
        .collect();
    let mut counts: std::collections::HashMap<&str, usize> = Default::default();
    for line in &lines {
        *counts.entry(line).or_default() += 1;
    }
    let top = counts
        .iter()
        .max_by_key(|(line, count)| (**count, line.len()))
        .map(|(line, count)| format!("{count}x `{}`", line.chars().take(80).collect::<String>()))
        .unwrap_or_default();
    // Lines of the last quarter that the first three quarters already had.
    let split = lines.len() * 3 / 4;
    let before: std::collections::HashSet<&str> = lines[..split].iter().copied().collect();
    let again = lines[split..]
        .iter()
        .filter(|line| before.contains(*line))
        .count();
    let tail: String = {
        let chars: Vec<char> = text.chars().collect();
        chars[chars.len().saturating_sub(240)..].iter().collect()
    };
    format!(
        "{} lines, {} distinct, top {top}, last quarter {again}/{} seen before, tail: {}",
        lines.len(),
        counts.len(),
        lines.len() - split,
        tail.replace(['\n', '\r'], " ")
    )
}

/// Whether a refused prefix request says the endpoint does not take prefix
/// completion: HTTP 404, 405 or 422, or a 400 whose body names the prefix,
/// the beta or an unsupported field. A 400 of another cause (a prompt plus
/// partial over the context window) or an auth error says nothing about it.
fn prefix_unsupported(error: &PermanentError) -> bool {
    match error.status {
        404 | 405 | 422 => true,
        400 => {
            let body = error.message.to_ascii_lowercase();
            ["prefix", "beta", "unsupported", "not support"]
                .iter()
                .any(|word| body.contains(word))
        }
        _ => false,
    }
}

/// Non-blank characters of prose after a closed code block that mark a file
/// answer as running on (two or three sentences; a closing remark is less).
const FENCE_OVERRUN_CHARS: usize = 160;

/// `fencestop`: the byte offset just after the first code block of `text`
/// when the answer starts with a fenced block, that block has closed, and at
/// least [`FENCE_OVERRUN_CHARS`] non-blank characters of prose follow it.
/// `None` for an answer with no closed block yet, one whose next thing is
/// another block, and edit-block answers.
fn fence_overrun(text: &str) -> Option<usize> {
    if !text.trim_start().starts_with("```") || text.contains("<<<<<<< SEARCH") {
        return None;
    }
    let mut offset = 0;
    let mut opened = false;
    for line in text.split_inclusive('\n') {
        offset += line.len();
        let trimmed = line.trim();
        if !opened {
            opened = trimmed.starts_with("```");
            continue;
        }
        if trimmed == "```" && line.ends_with('\n') {
            let rest = &text[offset..];
            if rest.trim_start().starts_with("```") {
                return None;
            }
            let prose = rest.chars().filter(|c| !c.is_whitespace()).count();
            return (prose >= FENCE_OVERRUN_CHARS).then_some(offset);
        }
    }
    None
}

/// When planning's steps happened, in ms from the client's construction: the
/// skeleton call (a stream with the skeleton prompt, [`SKELETON_PROMPT`])
/// last closed, so a retried skeleton counts its accepted attempt; the first
/// export fragment ([`FRAGMENT_PROMPT`]) opened; planning ended
/// ([`TapClient::mark_planning_done`]); and the first wave file opened.
/// `None` until seen. Salvage and other architect calls count for none.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PlanTimeline {
    pub skeleton_ms: Option<u64>,
    pub first_fragment_start_ms: Option<u64>,
    pub planning_done_ms: Option<u64>,
    pub wave_first_open_ms: Option<u64>,
}

/// A line of the architect's skeleton prompt.
pub const SKELETON_PROMPT: &str = "paths and one-sentence purposes ONLY";
/// A line of the architect's export-fragment prompt.
pub const FRAGMENT_PROMPT: &str = "Design the interface contracts";

impl TapClient {
    pub fn new(
        base_url: &str,
        model: &str,
        api_key: Option<String>,
        arm: Arm,
        emitter: Emitter,
    ) -> Result<Self> {
        let base_url = base_url.trim_end_matches('/');
        if base_url.is_empty() {
            bail!("model base URL cannot be empty");
        }
        let http = build_http(false)?;
        Ok(Self {
            http,
            endpoint: format!("{base_url}/chat/completions"),
            models: slimtoken::client::models_url(base_url),
            hedge: None,
            model: model.to_string(),
            api_key,
            arm,
            emitter,
            in_wave: AtomicBool::new(false),
            created: Instant::now(),
            timeline: std::sync::Mutex::new(PlanTimeline::default()),
            feed: std::sync::Mutex::new(None),
            index_feed: std::sync::Mutex::new(None),
            nocut: None,
            speed: None,
            fence_stop: None,
        })
    }

    /// `nocut`: continue an answer cut at its output budget
    /// ([`ModelClient::continue_completion`]) by prefix completion where the
    /// provider has it (`slimtoken::continuation::prefix_endpoint`;
    /// `SHOWDOWN_NOCUT_PREFIX` set to a URL names the endpoint, `off` uses
    /// turns), else by a user turn; a cut file lands on the feeds only once
    /// joined. Off: as before.
    pub fn with_nocut(mut self, enabled: bool) -> Self {
        self.nocut = enabled.then(|| {
            let prefix = match std::env::var("SHOWDOWN_NOCUT_PREFIX") {
                Ok(value) if value.trim() == "off" => None,
                Ok(value) if !value.trim().is_empty() => Some(value.trim().to_string()),
                _ => slimtoken::continuation::prefix_endpoint(&self.endpoint),
            };
            NoCut {
                prefix,
                prefix_ok: AtomicBool::new(true),
                stats: std::sync::Mutex::new(NoCutStats::default()),
            }
        });
        self
    }

    /// `speedrace`: each plan and file request is sent as some copies
    /// (`SHOWDOWN_SPEED_COPIES`, default 3) and the copy with the most text
    /// after a window (`SHOWDOWN_SPEED_WINDOW_MS`, default 1500, from the
    /// first text of any copy) is kept; see `slimtoken::speedrace`. Cache
    /// primes, continuations and hedged requests are sent once. Off: as
    /// before.
    pub fn with_speed_race(mut self, enabled: bool) -> Self {
        let number = |name: &str, default: u64| {
            std::env::var(name)
                .ok()
                .and_then(|text| text.trim().parse::<u64>().ok())
                .unwrap_or(default)
        };
        self.speed = enabled.then(|| SpeedRace {
            copies: number("SHOWDOWN_SPEED_COPIES", 3).clamp(1, 8) as usize,
            window: Duration::from_millis(number("SHOWDOWN_SPEED_WINDOW_MS", 1_500)),
            max_in_flight: number("SHOWDOWN_SPEED_MAX_INFLIGHT", 0) as usize,
            in_flight: std::sync::atomic::AtomicUsize::new(0),
            stats: std::sync::Mutex::new(SpeedStats::default()),
        });
        self
    }

    /// `speedrace`'s counts so far (`None`: switch off).
    pub fn speed_stats(&self) -> Option<SpeedStats> {
        let speed = self.speed.as_ref()?;
        speed.stats.lock().ok().map(|stats| *stats)
    }

    /// `fencestop`: a file answer whose code block has closed and which goes
    /// on in prose (the model argues with its contract and then writes the
    /// file again; one such answer took 28 s for a 550-token file) is
    /// stopped there and keeps its first block. Markdown files, edit-block
    /// answers and continuations are left alone. Off: as before.
    pub fn with_fence_stop(mut self, enabled: bool) -> Self {
        self.fence_stop = enabled.then(|| std::sync::atomic::AtomicUsize::new(0));
        self
    }

    /// `nocut` with `endpoint` as the prefix-completion endpoint.
    #[cfg(test)]
    fn with_prefix_endpoint(mut self, endpoint: &str) -> Self {
        if let Some(nocut) = &mut self.nocut {
            nocut.prefix = Some(endpoint.to_string());
        }
        self
    }

    /// `nocut`'s counts so far (`None`: switch off), and whether it
    /// continues by prefix completion.
    pub fn nocut_stats(&self) -> Option<(NoCutStats, bool)> {
        let nocut = self.nocut.as_ref()?;
        let stats = nocut.stats.lock().ok()?.clone();
        Some((
            stats,
            nocut.prefix.is_some() && nocut.prefix_ok.load(Ordering::Relaxed),
        ))
    }

    /// Hand a whole file to the persistent checker and to indexio.
    pub fn land(&self, title: &str, text: &str) {
        if let Some(sidecar) = self.feed.lock().ok().and_then(|feed| feed.clone()) {
            sidecar.land(title, text);
        }
        if let Some(index) = self.index_feed.lock().ok().and_then(|feed| feed.clone()) {
            index.land(title, &crate::arms::file_text(title, text));
        }
    }

    fn emit_close(&self, id: u64, result: &Result<Completion>) {
        let arm = self.arm;
        // An answer cut at a long length: what it looks like goes on record
        // (the text itself is thrown away by the retry).
        if let Ok(done) = result {
            if done.finish_reason.as_deref() == Some("length") && done.output_tokens >= 4_000 {
                self.emitter.emit(Ev::Tool {
                    arm,
                    tool: "runaway".to_string(),
                    op: "cut".to_string(),
                    ms: 0,
                    critical_ms: 0,
                    files: 0,
                    hits: done.output_tokens as _,
                    note: format!("stream {id}: {}", cut_shape(&done.text)),
                });
            }
        }
        match result {
            Ok(done) => self.emitter.emit(Ev::Close {
                arm,
                id,
                ok: true,
                input_tokens: Some(done.input_tokens),
                output_tokens: Some(done.output_tokens),
                cached_tokens: done.cached_input_tokens,
                finish: done.finish_reason.clone(),
                error: None,
            }),
            Err(error) => self.emitter.emit(Ev::Close {
                arm,
                id,
                ok: false,
                input_tokens: None,
                output_tokens: None,
                cached_tokens: None,
                finish: None,
                error: Some(format!("{error:#}").chars().take(300).collect()),
            }),
        }
    }

    /// `h2`: offer HTTP/2 in the TLS handshake (the server chooses); off,
    /// HTTP/1.1 only, as before.
    pub fn with_http2(mut self, enabled: bool) -> Result<Self> {
        self.http = build_http(enabled)?;
        Ok(self)
    }

    /// `hedge`: race an identical copy of each request (cache primes
    /// excepted) that has no first token after `config.after`.
    pub fn with_hedge(mut self, config: Option<HedgeConfig>) -> Self {
        self.hedge = config.map(|config| std::sync::Arc::new(Hedger::new(config)));
        self
    }

    /// True with `hedge` on.
    #[cfg(test)]
    pub fn hedged(&self) -> bool {
        self.hedge.is_some()
    }

    /// The planning timeline so far.
    pub fn timeline(&self) -> PlanTimeline {
        self.timeline
            .lock()
            .map(|timeline| *timeline)
            .unwrap_or_default()
    }

    /// Note a stream opening (`opened`) or closing in the timeline. The
    /// skeleton and the fragments are told apart by their prompts, so a
    /// retry, a salvage call or a plan without a skeleton call cannot shift
    /// the steps.
    fn mark(&self, request: &CompletionRequest, kind: &Kind, opened: bool) {
        let now = self.created.elapsed().as_millis() as u64;
        let Ok(mut timeline) = self.timeline.lock() else {
            return;
        };
        let skeleton = request.user.contains(SKELETON_PROMPT);
        let fragment = request.user.contains(FRAGMENT_PROMPT);
        match (kind, opened) {
            (Kind::Plan, true) if fragment => {
                timeline.first_fragment_start_ms.get_or_insert(now);
            }
            (Kind::Plan, false) if skeleton => timeline.skeleton_ms = Some(now),
            (Kind::File, true) => {
                timeline.wave_first_open_ms.get_or_insert(now);
            }
            _ => {}
        }
    }

    /// Note the end of planning on the timeline's clock (the first call
    /// counts).
    pub fn mark_planning_done(&self) {
        let now = self.created.elapsed().as_millis() as u64;
        if let Ok(mut timeline) = self.timeline.lock() {
            timeline.planning_done_ms.get_or_insert(now);
        }
    }

    pub fn set_in_wave(&self, value: bool) {
        self.in_wave.store(value, Ordering::Relaxed);
    }

    /// Hand each file that a request writes (its `Title:` line) to
    /// `sidecar` when the request closes; `None` stops that. Returns the
    /// checker that was set.
    pub fn set_feed(
        &self,
        sidecar: Option<std::sync::Arc<crate::pchk::Sidecar>>,
    ) -> Option<std::sync::Arc<crate::pchk::Sidecar>> {
        match self.feed.lock() {
            Ok(mut feed) => std::mem::replace(&mut *feed, sidecar),
            Err(_) => None,
        }
    }

    /// Hand each file that a request writes to `index` when the request
    /// closes; `None` stops that.
    pub fn set_index_feed(&self, index: Option<std::sync::Arc<crate::codeindex::Indexio>>) {
        if let Ok(mut feed) = self.index_feed.lock() {
            *feed = index;
        }
    }

    /// Label a request for the screen. A wave worker prompt carries a
    /// `Title: <path>` line (the engine writes it for every task).
    fn classify(&self, request: &CompletionRequest) -> (String, Kind) {
        if request.allow_empty {
            return ("cache prime".to_string(), Kind::Aux);
        }
        let title = request
            .user
            .lines()
            .find_map(|line| line.strip_prefix("Title: "))
            .map(str::trim);
        match title {
            Some(title) if !title.is_empty() => (title.to_string(), Kind::File),
            _ if self.in_wave.load(Ordering::Relaxed) => ("repair".to_string(), Kind::Aux),
            _ => ("architect".to_string(), Kind::Plan),
        }
    }

    pub async fn complete_as(
        &self,
        request: CompletionRequest,
        label: String,
        kind: Kind,
    ) -> Result<Completion> {
        let id = self.emitter.stream_id();
        let arm = self.arm;
        // The requests that gate what follows: the plan's own and the
        // wiring plan (it starts while early files are in flight).
        let gate = matches!(kind, Kind::Plan) || label == "wiring plan";
        let sidecar = match kind {
            Kind::File => self.feed.lock().ok().and_then(|feed| feed.clone()),
            _ => None,
        };
        let index = match kind {
            Kind::File => self.index_feed.lock().ok().and_then(|feed| feed.clone()),
            _ => None,
        };
        let title = (sidecar.is_some() || index.is_some()).then(|| label.clone());
        self.mark(&request, &kind, true);
        self.emitter.emit(Ev::Open {
            arm,
            id,
            label,
            kind,
            continues: None,
        });
        // `speedrace` with more than one copy takes the place of the hedge:
        // a copy with no first token loses the race to the others.
        // With `SHOWDOWN_SPEED_MAX_INFLIGHT` (0: no limit) only a request
        // that starts while fewer than that many are in flight is raced: the
        // single requests and the plan's own (`gate`), not the wave (raced, the wave took
        // 5 s longer in a mixed hour, 20261120-0048).
        struct InFlight<'a>(Option<&'a std::sync::atomic::AtomicUsize>);
        impl Drop for InFlight<'_> {
            fn drop(&mut self) {
                if let Some(count) = self.0 {
                    count.fetch_sub(1, Ordering::Relaxed);
                }
            }
        }
        let before = self
            .speed
            .as_ref()
            .map(|speed| speed.in_flight.fetch_add(1, Ordering::Relaxed));
        let _in_flight = InFlight(self.speed.as_ref().map(|speed| &speed.in_flight));
        let raced = self.speed.as_ref().is_some_and(|speed| {
            speed.copies > 1
                && (speed.max_in_flight == 0 || gate || before.unwrap_or(0) < speed.max_in_flight)
        });
        let result = match self
            .hedge
            .as_deref()
            .filter(|_| !request.allow_empty && !raced)
        {
            // Both copies stream under the same id: only the winner is
            // polled once a first token is in, so one stream reaches the
            // screen and the recording.
            Some(hedger) => {
                let (result, outcome) =
                    hedger.race(|_| self.stream(&request, id, None, None)).await;
                if outcome.launched {
                    let stats = hedger.stats();
                    self.emitter.emit(Ev::Tool {
                        arm,
                        tool: "hedge".to_string(),
                        op: if outcome.winner == 1 { "won" } else { "lost" }.to_string(),
                        ms: outcome.launched_after.as_millis() as u64,
                        critical_ms: 0,
                        files: stats.launched,
                        hits: stats.won,
                        note: title_line(&request).unwrap_or_default(),
                    });
                }
                result
            }
            None => match &self.speed {
                // `speedrace`: the copies stream under one id; only the
                // kept copy shows its text.
                Some(speed) if !request.allow_empty && raced => {
                    let (started, first) = (AtomicBool::new(false), AtomicBool::new(false));
                    let (result, outcome) = slimtoken::speedrace::speed_race(
                        speed.copies,
                        speed.window,
                        |_, progress, began| {
                            let (request, started, first) = (&request, &started, &first);
                            async move {
                                let probe = Probe {
                                    progress: &progress,
                                    began,
                                    started,
                                    first,
                                };
                                self.stream(request, id, None, Some(probe)).await
                            }
                        },
                    )
                    .await;
                    if let Ok(mut stats) = speed.stats.lock() {
                        stats.races += 1;
                        stats.copies += outcome.copies;
                        stats.chosen += usize::from(outcome.chosen);
                        stats.dropped_chars += outcome.dropped_chars;
                    }
                    result
                }
                _ => self.stream(&request, id, None, None).await,
            },
        };
        self.mark(&request, &kind, false);
        self.emit_close(id, &result);
        // `nocut`: a cut file lands once its continuation joins it.
        if let (Some(nocut), Kind::File, Ok(done)) = (&self.nocut, &kind, &result) {
            if done.finish_reason.as_deref() == Some("length") {
                if let Ok(mut stats) = nocut.stats.lock() {
                    stats.truncated += 1;
                    stats
                        .pending
                        .insert(label_title(&request), (done.elapsed.as_millis() as u64, 0));
                }
                return result;
            }
        }
        // `pchk`: after the close is on the clock. It stays on this task so
        // that every file has landed when the wave's result comes back (the
        // first check must not race a late write).
        if let (Some(sidecar), Some(title), Ok(done)) = (&sidecar, &title, &result) {
            sidecar.land(title, &done.text);
        }
        // indexio: mirror the file; the index runs on its own thread.
        if let (Some(index), Some(title), Ok(done)) = (&index, &title, &result) {
            index.land(title, &crate::arms::file_text(title, &done.text));
        }
        result
    }

    /// `nocut`: the rest of a cut answer, as its own stream that adds to the
    /// file's row (`<path> (continued)`). Prefix completion while the
    /// provider takes it; a prefix request refused as unsupported
    /// ([`prefix_unsupported`]) switches the client to turns for the rest of
    /// the run, another failure asks that one request again as a turn, and a
    /// rate limit or an overload goes back to the scheduler. A file whose
    /// joined answer is complete lands on the feeds.
    async fn continue_as(
        &self,
        nocut: &NoCut,
        request: CompletionRequest,
        partial: String,
    ) -> Result<slimtoken::continuation::Continued> {
        use slimtoken::continuation::{join, turn_request, ContinueMode};
        let (label, kind) = self.classify(&request);
        let title = label_title(&request);
        let id = self.emitter.stream_id();
        // The stream adds to the cut file's own row (`continues`).
        let continues = (kind == Kind::File).then(|| label.clone());
        self.emitter.emit(Ev::Open {
            arm: self.arm,
            id,
            label: format!("{label} (continued)"),
            kind,
            continues,
        });
        let prefix = nocut
            .prefix
            .as_deref()
            .filter(|_| nocut.prefix_ok.load(Ordering::Relaxed));
        let (result, mode) = match prefix {
            Some(endpoint) => match self
                .stream(&request, id, Some((endpoint, &partial)), None)
                .await
            {
                // The provider's limit: the scheduler waits it out (its
                // gate, the Retry-After); a turn would meet the same limit.
                Err(error)
                    if error.downcast_ref::<RateLimitedError>().is_some()
                        || error.downcast_ref::<OverloadedError>().is_some() =>
                {
                    (Err(error), ContinueMode::Prefix)
                }
                Err(error) => {
                    let unsupported = error
                        .downcast_ref::<PermanentError>()
                        .is_some_and(prefix_unsupported);
                    if unsupported {
                        nocut.prefix_ok.store(false, Ordering::Relaxed);
                    }
                    if let Ok(mut stats) = nocut.stats.lock() {
                        if unsupported {
                            stats.prefix_refused += 1;
                        } else {
                            stats.prefix_errors += 1;
                        }
                        if stats.prefix_note.is_none() {
                            let excerpt: String = format!("{error:#}").chars().take(160).collect();
                            stats.prefix_note = Some(excerpt);
                        }
                    }
                    let turn = turn_request(&request, &partial);
                    (self.stream(&turn, id, None, None).await, ContinueMode::Turn)
                }
                other => (other, ContinueMode::Prefix),
            },
            None => {
                let turn = turn_request(&request, &partial);
                (self.stream(&turn, id, None, None).await, ContinueMode::Turn)
            }
        };
        self.emit_close(id, &result);
        let mut joined = None;
        if let Ok(mut stats) = nocut.stats.lock() {
            stats.continuations += 1;
            match mode {
                ContinueMode::Prefix => stats.prefix += 1,
                ContinueMode::Turn => stats.turn += 1,
            }
            match &result {
                Err(_) => stats.failed += 1,
                Ok(done) => {
                    let ms = done.elapsed.as_millis() as u64;
                    let first = done
                        .time_to_first_token
                        .map_or(ms, |wait| wait.as_millis() as u64);
                    stats.continue_ms += ms;
                    let entry = stats.pending.entry(title.clone()).or_insert((0, 0));
                    entry.1 += first;
                    if done.finish_reason.as_deref() != Some("length") {
                        let (cut, waits) = stats.pending.remove(&title).unwrap_or_default();
                        stats.joined += 1;
                        stats.cut_ms += cut;
                        stats.saved_ms += cut.saturating_sub(waits);
                        stats.files.push(title.clone());
                        joined = Some(join(mode, &partial, &done.text));
                    }
                }
            }
        }
        if let (Some(text), Kind::File) = (&joined, &kind) {
            self.land(&title, text);
        }
        result.map(|completion| slimtoken::continuation::Continued { completion, mode })
    }

    /// One streamed request. `prefix` (`nocut`): the prefix-completion
    /// endpoint and the answer so far, sent as the last assistant message
    /// for the provider to continue; the completion is only the new text.
    async fn stream(
        &self,
        request: &CompletionRequest,
        id: u64,
        prefix: Option<(&str, &str)>,
        probe: Option<Probe<'_>>,
    ) -> Result<Completion> {
        let started = Instant::now();
        let mut messages = vec![json!({"role": "system", "content": &request.system})];
        if let Some(shared) = &request.shared_context {
            messages.push(json!({"role": "user", "content": shared}));
        }
        messages.push(json!({"role": "user", "content": &request.user}));
        if let Some((_, partial)) = prefix {
            messages.push(slimtoken::continuation::prefix_message(partial));
        }
        let endpoint = prefix.map_or(self.endpoint.as_str(), |(endpoint, _)| endpoint);

        let mut body = json!({
            "model": &self.model,
            "messages": messages,
            "max_tokens": request.max_tokens,
            "temperature": request.temperature,
            "stream": true,
            "stream_options": {"include_usage": true}
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

        let mut builder = self.http.post(endpoint).json(&body);
        if let Some(api_key) = &self.api_key {
            builder = builder.bearer_auth(api_key);
        }
        let mut response = builder.send().await.context("model request failed")?;
        let status = response.status();
        if !status.is_success() {
            let retry_after = response
                .headers()
                .get(reqwest::header::RETRY_AFTER)
                .and_then(|value| value.to_str().ok())
                .and_then(|text| text.trim().parse::<u64>().ok())
                .map(Duration::from_secs);
            if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
                return Err(RateLimitedError { retry_after }.into());
            }
            let error_body = response.text().await.unwrap_or_default();
            let excerpt: String = error_body.chars().take(1_000).collect();
            let message = format!("model server returned HTTP {status}: {excerpt}");
            if matches!(status.as_u16(), 400 | 401 | 403 | 404 | 405 | 413 | 422) {
                return Err(PermanentError {
                    status: status.as_u16(),
                    message,
                }
                .into());
            }
            if retry_after.is_some() {
                return Err(OverloadedError {
                    retry_after,
                    message,
                }
                .into());
            }
            bail!(message);
        }

        let mut buffered = Vec::<u8>::new();
        let mut text = String::new();
        let mut usage = Usage::default();
        let mut time_to_first_token = None;
        let mut finish_reason = None;
        let mut done = false;
        let mut stream_started = false;
        // Bytes of `text` shown so far (a raced copy shows none until kept).
        let mut shown = 0;
        // `fencestop`: file answers only (a `Title:` line), not Markdown.
        let fence_stop = self.fence_stop.as_ref().filter(|_| {
            prefix.is_none()
                && title_line(request).is_some_and(|title| !title.trim().ends_with(".md"))
        });

        while let Some(chunk) = response.chunk().await.context("model stream failed")? {
            buffered.extend_from_slice(&chunk);
            let before = text.len();
            while let Some((event_end, separator_len)) = find_event_boundary(&buffered) {
                let event_bytes = buffered[..event_end].to_vec();
                drop(buffered.drain(..event_end + separator_len));
                let event = String::from_utf8(event_bytes).context("stream was not UTF-8")?;
                done = consume_event(&event, &mut text, &mut usage, &mut finish_reason)?;
                if !stream_started && (usage.reasoning_seen || !text.is_empty()) {
                    stream_started = true;
                    if probe
                        .as_ref()
                        .is_none_or(|probe| !probe.started.swap(true, Ordering::Relaxed))
                    {
                        report_stream_started(started.elapsed());
                    }
                }
                if done {
                    break;
                }
            }
            if text.len() > before {
                if time_to_first_token.is_none() {
                    let elapsed = started.elapsed();
                    time_to_first_token = Some(elapsed);
                    if probe
                        .as_ref()
                        .is_none_or(|probe| !probe.first.swap(true, Ordering::Relaxed))
                    {
                        report_first_token(elapsed);
                    }
                }
                if let Some(probe) = &probe {
                    probe.progress.report(text.len(), probe.began.elapsed());
                }
                if probe.as_ref().is_none_or(|probe| probe.progress.live()) {
                    // `planpipe`: the architect reads the streaming skeleton
                    // (a no-op for every other call).
                    report_text(&text);
                    self.emitter.emit(Ev::Delta {
                        arm: self.arm,
                        id,
                        text: text[shown..].to_string(),
                    });
                    shown = text.len();
                }
            }
            if let (false, Some(count)) = (done, fence_stop) {
                if let Some(end) = fence_overrun(&text) {
                    text.truncate(end);
                    finish_reason = Some("stop".to_string());
                    done = true;
                    let stopped = count.fetch_add(1, Ordering::Relaxed) + 1;
                    self.emitter.emit(Ev::Tool {
                        arm: self.arm,
                        tool: "fencestop".to_string(),
                        op: "stop".to_string(),
                        ms: started.elapsed().as_millis() as u64,
                        critical_ms: 0,
                        files: stopped as _,
                        hits: 0,
                        note: title_line(request).unwrap_or_default(),
                    });
                }
            }
            if done {
                break;
            }
        }

        if !done && !buffered.is_empty() {
            let event = String::from_utf8(buffered).context("stream was not UTF-8")?;
            done = consume_event(&event, &mut text, &mut usage, &mut finish_reason)?;
        }
        // The rest of the text: the stream's last event, or the whole
        // answer of a raced copy that finished before a copy was kept.
        if done && shown < text.len() {
            self.emitter.emit(Ev::Delta {
                arm: self.arm,
                id,
                text: text[shown..].to_string(),
            });
        }
        if !done {
            bail!(
                "model stream ended before the [DONE] sentinel (finish_reason={finish_reason:?}, {} chars received); treating as truncated",
                text.chars().count()
            );
        }
        if text.is_empty() && !request.allow_empty {
            if finish_reason.as_deref() == Some("length") {
                return Err(BudgetExhaustedError {
                    input_tokens: usage.input.unwrap_or(0),
                    output_tokens: usage.output.unwrap_or(0),
                    reasoning_tokens: usage.reasoning.unwrap_or(0),
                }
                .into());
            }
            bail!("model returned an empty completion");
        }

        let estimated_input = estimate_tokens(&request.system)
            + request
                .shared_context
                .as_deref()
                .map(estimate_tokens)
                .unwrap_or(0)
            + estimate_tokens(&request.user)
            + prefix.map_or(0, |(_, partial)| estimate_tokens(partial));
        let usage_is_estimated = usage.input.is_none() || usage.output.is_none();
        Ok(Completion {
            provider: None,
            input_tokens: usage.input.unwrap_or(estimated_input),
            output_tokens: usage.output.unwrap_or_else(|| estimate_tokens(&text)),
            text,
            reasoning_tokens: usage.reasoning,
            cached_input_tokens: usage.cached,
            finish_reason,
            usage_is_estimated,
            elapsed: started.elapsed(),
            time_to_first_token,
        })
    }
}

#[async_trait]
impl ModelClient for TapClient {
    async fn complete(&self, request: CompletionRequest) -> Result<Completion> {
        let (label, kind) = self.classify(&request);
        self.complete_as(request, label, kind).await
    }

    async fn continue_completion(
        &self,
        request: CompletionRequest,
        partial: String,
    ) -> Result<slimtoken::continuation::Continued> {
        match &self.nocut {
            Some(nocut) => self.continue_as(nocut, request, partial).await,
            // The engine's default: a user turn through `complete`.
            None => {
                let turn = slimtoken::continuation::turn_request(&request, &partial);
                Ok(slimtoken::continuation::Continued {
                    completion: self.complete(turn).await?,
                    mode: slimtoken::continuation::ContinueMode::Turn,
                })
            }
        }
    }

    async fn prewarm(&self, connections: usize) -> Result<PrewarmReport> {
        Ok(prewarm_pool(
            &self.http,
            &self.models,
            self.api_key.as_deref(),
            connections,
        )
        .await)
    }
}

/// The run's client for the repairs sent during the wave (`inwave`): each
/// streams as `in-wave repair <path>` beside the wave's files, and its
/// answer (often edit blocks) never reaches the persistent checker's or
/// indexio's feed as a landed file.
pub struct InWaveRepairTap(pub std::sync::Arc<TapClient>);

#[async_trait]
impl ModelClient for InWaveRepairTap {
    async fn complete(&self, request: CompletionRequest) -> Result<Completion> {
        let label = format!(
            "in-wave repair {}",
            title_line(&request).unwrap_or_default()
        );
        self.0.complete_as(request, label, Kind::Aux).await
    }
}

/// What marks a component's request of `pagefork` (the start of the part
/// note's order, `slimtoken::pagefork::tsx_rules`).
const PART_MARK: &str = "You write the local component `";

/// `pagefork`'s inner client: a component's request streams as
/// `<page> · <Name>` and is no landed file; every other request is the
/// run's.
struct PartTap(std::sync::Arc<TapClient>);

#[async_trait]
impl ModelClient for PartTap {
    async fn complete(&self, request: CompletionRequest) -> Result<Completion> {
        let name = request
            .user
            .rsplit_once(PART_MARK)
            .and_then(|(_, rest)| rest.split('`').next());
        match name {
            Some(name) => {
                let label = format!("{} · {name}", title_line(&request).unwrap_or_default());
                self.0.complete_as(request, label, Kind::Aux).await
            }
            None => self.0.complete(request).await,
        }
    }

    async fn continue_completion(
        &self,
        request: CompletionRequest,
        partial: String,
    ) -> Result<slimtoken::continuation::Continued> {
        self.0.continue_completion(request, partial).await
    }
}

/// True for the first writing of a page: a wave worker's request for a
/// `page.tsx` that is no retry, repair or continuation.
fn forks(request: &CompletionRequest) -> bool {
    title_line(request).is_some_and(|title| title.ends_with("page.tsx"))
        && !request.allow_empty
        && ![
            "corrected artifact",
            "# Continuation",
            "# Check results",
            "<<<<<<< SEARCH",
            PART_MARK,
        ]
        .iter()
        .any(|mark| request.user.contains(mark))
}

/// `pagefork`: the run's client with `slimtoken::pagefork::ForkClient`
/// around it. The joined page is landed again when its components are in
/// (the page's own stream landed it without them).
pub struct PageForkTap {
    tap: std::sync::Arc<TapClient>,
    fork: slimtoken::pagefork::ForkClient,
    /// The plan's value exports (name, path of its file), set when the plan
    /// is final: what a component may import.
    exports: std::sync::Arc<std::sync::Mutex<Vec<(String, String)>>>,
    /// False once the wave is over: a repair is never forked.
    wave: std::sync::atomic::AtomicBool,
}

/// The name a planned export line gives a function or a constant.
fn value_export(line: &str) -> Option<&str> {
    let rest = line.trim().strip_prefix("export ")?;
    let rest = rest.strip_prefix("async ").unwrap_or(rest);
    let rest = rest
        .strip_prefix("function ")
        .or_else(|| rest.strip_prefix("const "))?;
    let name = rest
        .split(|c: char| !(c.is_alphanumeric() || c == '_' || c == '$'))
        .next()?;
    (!name.is_empty()).then_some(name)
}

/// The import line of `name` from the project file `path`, for `page`.
fn import_line(page: &str, name: &str, path: &str) -> String {
    let ups = "../".repeat(page.matches('/').count());
    let module = path.rsplit_once('.').map_or(path, |(module, _)| module);
    format!("import {{ {name} }} from \"{ups}{module}\";")
}

impl PageForkTap {
    pub fn new(tap: std::sync::Arc<TapClient>) -> Self {
        let mut rules = slimtoken::pagefork::tsx_rules(std::sync::Arc::new(forks));
        let exports: std::sync::Arc<std::sync::Mutex<Vec<(String, String)>>> = Default::default();
        let table = exports.clone();
        rules.imports = Some(std::sync::Arc::new(move |request: &CompletionRequest| {
            let page = title_line(request).unwrap_or_default();
            let mut names = slimtoken::pagefork::react_imports();
            if let Ok(table) = table.lock() {
                names.extend(
                    table
                        .iter()
                        .map(|(name, path)| (name.clone(), import_line(&page, name, path))),
                );
            }
            names
        }));
        Self {
            wave: std::sync::atomic::AtomicBool::new(true),
            exports,
            fork: slimtoken::pagefork::ForkClient::new(
                std::sync::Arc::new(PartTap(tap.clone())),
                rules,
            ),
            tap,
        }
    }

    pub fn stats(&self) -> slimtoken::pagefork::ForkStats {
        self.fork.stats()
    }

    /// The wave is over: every later request is the run's own.
    pub fn end_wave(&self) {
        self.wave.store(false, Ordering::Relaxed);
    }

    /// The final plan: the functions and constants that exactly one script
    /// file outside the app's routes and tests exports.
    pub fn set_manifest(&self, manifest: &slimtoken::architect::Manifest) {
        let mut found: Vec<(String, String)> = Vec::new();
        let mut twice: Vec<String> = Vec::new();
        for file in &manifest.files {
            let script = file.path.ends_with(".ts") || file.path.ends_with(".tsx");
            if !script || file.path.starts_with("app/") || file.path.contains(".test.") {
                continue;
            }
            for name in file.exports.iter().filter_map(|line| value_export(line)) {
                if found.iter().any(|(known, _)| known == name) {
                    twice.push(name.to_string());
                } else {
                    found.push((name.to_string(), file.path.clone()));
                }
            }
        }
        found.retain(|(name, _)| !twice.contains(name));
        if let Ok(mut exports) = self.exports.lock() {
            *exports = found;
        }
    }
}

#[async_trait]
impl ModelClient for PageForkTap {
    async fn complete(&self, request: CompletionRequest) -> Result<Completion> {
        if !self.wave.load(Ordering::Relaxed) {
            return self.tap.complete(request).await;
        }
        let page = forks(&request).then(|| title_line(&request)).flatten();
        let done = self.fork.complete(request).await?;
        if let Some(page) = page {
            self.tap.land(&page, &done.text);
        }
        Ok(done)
    }

    async fn continue_completion(
        &self,
        request: CompletionRequest,
        partial: String,
    ) -> Result<slimtoken::continuation::Continued> {
        self.tap.continue_completion(request, partial).await
    }
}

/// The bench's HTTP client; `http2` off is HTTP/1.1 only, the traffic of
/// a build without reqwest's `http2` feature.
fn build_http(http2: bool) -> Result<reqwest::Client> {
    let builder = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(15))
        .tcp_keepalive(Duration::from_secs(30))
        .read_timeout(Duration::from_secs(120));
    slimtoken::client::http_version(builder, http2)
        .build()
        .context("failed to construct HTTP client")
}

/// The `Title:` line of a wave worker prompt.
fn title_line(request: &CompletionRequest) -> Option<String> {
    request
        .user
        .lines()
        .find_map(|line| line.strip_prefix("Title: "))
        .map(|title| title.trim().to_string())
}

/// The `Title:` line of a request, empty when it has none (`nocut`'s key).
fn label_title(request: &CompletionRequest) -> String {
    title_line(request).unwrap_or_default()
}

#[derive(Default)]
struct Usage {
    input: Option<u64>,
    output: Option<u64>,
    reasoning: Option<u64>,
    cached: Option<u64>,
    /// A non-empty `reasoning_content` delta arrived.
    reasoning_seen: bool,
}

/// Parses one server-sent event. Returns true for the `[DONE]` sentinel.
fn consume_event(
    event: &str,
    text: &mut String,
    usage: &mut Usage,
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
    if value
        .pointer("/choices/0/delta/reasoning_content")
        .and_then(Value::as_str)
        .is_some_and(|reasoning| !reasoning.is_empty())
    {
        usage.reasoning_seen = true;
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
    text.chars().count().div_ceil(4) as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::sync::{Arc, Mutex};

    #[test]
    fn cut_shape_counts_the_repeated_lines() {
        let mut text = String::from("const first = 1;\nconst second = 2;\nconst third = 3;\n");
        for _ in 0..9 {
            text.push_str("  .item { color: red; }\n");
        }
        let shape = cut_shape(&text);
        assert!(
            shape.starts_with("12 lines, 4 distinct, top 9x `.item { color: red; }`"),
            "{shape}"
        );
        assert!(shape.contains("last quarter 3/3 seen before"), "{shape}");
        assert!(!shape.contains(['\n', '\r']));
        // An empty answer does not panic.
        assert!(cut_shape("").starts_with("0 lines, 0 distinct"));
    }

    #[test]
    fn fence_overrun_cuts_only_prose_after_the_first_closed_block() {
        let file = "```ts\nexport const a = 1;\n```\n";
        let prose =
            "Wait, the contract says something else. Let me reconsider the whole thing. ".repeat(4);
        // Still streaming, a closed block alone, or a short closing remark.
        assert_eq!(fence_overrun("```ts\nexport const a = 1;\n"), None);
        assert_eq!(fence_overrun(file), None);
        assert_eq!(fence_overrun(&format!("{file}\nDone.\n")), None);
        // Prose after the block: keep the block.
        let long = format!("{file}\n{prose}\n```ts\nexport const a = 2;\n```\n");
        assert_eq!(fence_overrun(&long), Some(file.len()));
        assert_eq!(&long[..file.len()], file);
        // Another block straight after, edit blocks, or no leading fence.
        assert_eq!(fence_overrun(&format!("{file}```ts\n{prose}\n```\n")), None);
        assert_eq!(
            fence_overrun(&format!(
                "```\n<<<<<<< SEARCH\na\n=======\nb\n>>>>>>> REPLACE\n```\n{prose}"
            )),
            None
        );
        assert_eq!(fence_overrun(&format!("{prose}\n{file}{prose}")), None);
    }

    /// What the server saw: `(request line, body)` per request.
    type Seen = Arc<Mutex<Vec<(String, String)>>>;

    /// A one-request-per-connection HTTP server: answers each request with
    /// the next `(status, body)` and records `(request line, body)`.
    fn serve(answers: Vec<(u16, String)>) -> (String, Seen) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = seen.clone();
        std::thread::spawn(move || {
            for (status, body) in answers {
                let Ok((mut socket, _)) = listener.accept() else {
                    return;
                };
                let mut bytes = Vec::new();
                let mut buffer = [0u8; 4096];
                let (head, length) = loop {
                    let read = socket.read(&mut buffer).unwrap();
                    bytes.extend_from_slice(&buffer[..read]);
                    let text = String::from_utf8_lossy(&bytes).to_string();
                    if let Some(end) = text.find("\r\n\r\n") {
                        let length = text[..end]
                            .lines()
                            .find_map(|line| {
                                line.to_ascii_lowercase()
                                    .strip_prefix("content-length:")
                                    .map(|value| value.trim().parse::<usize>().unwrap())
                            })
                            .unwrap_or(0);
                        break (end + 4, length);
                    }
                };
                while bytes.len() < head + length {
                    let read = socket.read(&mut buffer).unwrap();
                    bytes.extend_from_slice(&buffer[..read]);
                }
                let text = String::from_utf8_lossy(&bytes).to_string();
                let line = text.lines().next().unwrap_or_default().to_string();
                log.lock()
                    .unwrap()
                    .push((line, text[head..head + length].to_string()));
                let reply = format!(
                    "HTTP/1.1 {status} X\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                socket.write_all(reply.as_bytes()).unwrap();
            }
        });
        (base, seen)
    }

    fn sse(text: &str, finish: &str) -> String {
        let chunk = json!({"choices": [{"delta": {"content": text}, "finish_reason": finish}],
            "usage": {"prompt_tokens": 10, "completion_tokens": 3}});
        format!("data: {chunk}\n\ndata: [DONE]\n\n")
    }

    fn request() -> CompletionRequest {
        CompletionRequest {
            system: "s".into(),
            shared_context: None,
            user: "Title: lib/a.ts\nWrite it.".into(),
            max_tokens: 64,
            temperature: 0.0,
            reasoning_effort: None,
            prompt_cache_key: None,
            allow_empty: false,
            response_format: None,
        }
    }

    fn tap(base: &str) -> TapClient {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        TapClient::new(base, "m", None, Arm::Slim, Emitter::new(tx))
            .unwrap()
            .with_nocut(true)
    }

    #[tokio::test]
    async fn nocut_continues_by_prefix_completion() {
        let (base, seen) = serve(vec![
            (200, sse("```ts\nconst a = fo", "length")),
            (200, sse("o;\n```\n", "stop")),
        ]);
        let tap = tap(&format!("{base}/v1"))
            .with_prefix_endpoint(&format!("{base}/beta/chat/completions"));
        let cut = tap.complete(request()).await.unwrap();
        assert_eq!(cut.finish_reason.as_deref(), Some("length"));
        let rest = tap
            .continue_completion(request(), cut.text.clone())
            .await
            .unwrap();
        assert_eq!(rest.mode, slimtoken::continuation::ContinueMode::Prefix);
        assert_eq!(
            slimtoken::continuation::join(rest.mode, &cut.text, &rest.completion.text),
            "```ts\nconst a = foo;\n```\n"
        );
        let seen = seen.lock().unwrap();
        assert!(seen[0].0.contains("/v1/chat/completions"), "{:?}", seen[0]);
        assert!(
            seen[1].0.contains("/beta/chat/completions"),
            "{:?}",
            seen[1]
        );
        let body: Value = serde_json::from_str(&seen[1].1).unwrap();
        let last = body["messages"].as_array().unwrap().last().unwrap().clone();
        assert_eq!(last["role"], "assistant");
        assert_eq!(last["prefix"], true);
        assert_eq!(last["content"], "```ts\nconst a = fo");
        let (stats, prefix) = tap.nocut_stats().unwrap();
        assert!(prefix);
        assert_eq!(
            (
                stats.truncated,
                stats.continuations,
                stats.prefix,
                stats.joined
            ),
            (1, 1, 1, 1)
        );
        assert_eq!(stats.files, ["lib/a.ts"]);
    }

    #[tokio::test]
    async fn nocut_falls_back_to_a_turn_when_the_prefix_is_refused() {
        let (base, seen) = serve(vec![
            (400, "{\"error\":\"prefix not supported\"}".to_string()),
            (200, sse("const b = 2;\n```\n", "stop")),
        ]);
        let tap = tap(&format!("{base}/v1"))
            .with_prefix_endpoint(&format!("{base}/beta/chat/completions"));
        let partial = "```ts\nconst a = 1;\nconst b".to_string();
        let rest = tap
            .continue_completion(request(), partial.clone())
            .await
            .unwrap();
        assert_eq!(rest.mode, slimtoken::continuation::ContinueMode::Turn);
        assert_eq!(
            slimtoken::continuation::join(rest.mode, &partial, &rest.completion.text),
            "```ts\nconst a = 1;\nconst b = 2;\n```\n"
        );
        let seen = seen.lock().unwrap();
        assert!(seen[1].0.contains("/v1/chat/completions"));
        assert!(seen[1].1.contains("# Continuation"));
        let (stats, prefix) = tap.nocut_stats().unwrap();
        assert!(!prefix, "turns from now on");
        assert_eq!((stats.prefix_refused, stats.turn), (1, 1));
        assert!(stats.prefix_note.unwrap().contains("400"));
    }

    /// A 400 of another cause (the context window): that request goes as a
    /// turn, and prefix completion stays on.
    #[tokio::test]
    async fn nocut_keeps_the_prefix_after_another_bad_request() {
        let (base, _seen) = serve(vec![
            (
                400,
                "{\"error\":\"maximum context length exceeded\"}".to_string(),
            ),
            (200, sse("const b = 2;\n```\n", "stop")),
        ]);
        let tap = tap(&format!("{base}/v1"))
            .with_prefix_endpoint(&format!("{base}/beta/chat/completions"));
        let rest = tap
            .continue_completion(request(), "```ts\nconst a = 1;\nconst b".to_string())
            .await
            .unwrap();
        assert_eq!(rest.mode, slimtoken::continuation::ContinueMode::Turn);
        let (stats, prefix) = tap.nocut_stats().unwrap();
        assert!(prefix, "the prefix stays on");
        assert_eq!((stats.prefix_refused, stats.prefix_errors), (0, 1));
    }

    #[test]
    fn only_an_unsupported_prefix_turns_it_off() {
        let error = |status: u16, body: &str| PermanentError {
            status,
            message: format!("model server returned HTTP {status}: {body}"),
        };
        assert!(prefix_unsupported(&error(404, "")));
        assert!(prefix_unsupported(&error(422, "")));
        assert!(prefix_unsupported(&error(400, "prefix is not supported")));
        assert!(!prefix_unsupported(&error(400, "context length exceeded")));
        assert!(!prefix_unsupported(&error(401, "bad key")));
    }

    #[test]
    fn nocut_is_off_unless_asked() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let tap = TapClient::new(
            "http://127.0.0.1:9/v1",
            "m",
            None,
            Arm::Slim,
            Emitter::new(tx),
        )
        .unwrap();
        assert!(tap.nocut_stats().is_none());
    }
}
