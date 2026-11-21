//! Screen state: a reducer over the event timeline. Live runs, demos and
//! replays all feed the same reducer, so they draw the same screen.

use crate::events::{Arm, CheckState, Ev, FileStat, Kind, Phase, Timed};
use crate::extract;
use std::collections::{BTreeMap, VecDeque};

/// Characters per output token before any request reports real usage.
const DEFAULT_CHARS_PER_TOKEN: f64 = 3.4;
const TAIL_BYTES: usize = 4_000;

#[derive(Debug, Clone, Default)]
pub struct Meta {
    pub objective: String,
    pub provider: String,
    pub model: String,
    pub source: String,
    pub recorded_at: String,
}

#[derive(Debug, Clone)]
pub struct Stream {
    pub label: String,
    pub kind: Kind,
    pub chars: usize,
    pub tail: String,
    pub open: bool,
    pub input_tokens: Option<u64>,
    /// Input tokens that the provider served from its prompt cache.
    pub cached_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub last_delta: f64,
    /// `nocut`: the stream continues a cut file's row and adds to it.
    pub continues: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowState {
    Queued,
    Rendered,
    Streaming,
    Done,
    Failed,
}

#[derive(Debug, Clone)]
pub struct Row {
    pub path: String,
    pub state: RowState,
    pub chars: usize,
    pub tokens: Option<u64>,
    pub started: Option<f64>,
    pub ended: Option<f64>,
    pub attempts: u32,
    stream: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct FinalInfo {
    pub ok: bool,
    pub files: Vec<FileStat>,
    pub out_dir: String,
    pub note: Option<String>,
}

/// Result of SlimToken's check-and-repair step.
#[derive(Debug, Clone)]
pub struct VerifyInfo {
    pub before_failed: u64,
    pub after_failed: u64,
    pub tests_passed: u64,
    pub repaired: usize,
    pub seconds: f64,
}

/// What an arm's helper tools did (indexio for SlimToken's repair guards),
/// added up from `Ev::Tool`.
#[derive(Debug, Clone, Default)]
pub struct ToolTotals {
    /// "indexio v0.1.15" once indexio started (or why it has no version).
    pub indexio: Option<String>,
    /// The bare version ("0.1.15"); `None` if the version check failed.
    pub indexio_version: Option<String>,
    /// Why indexio was turned off for the rest of the run.
    pub indexio_down: Option<String>,
    /// Files in indexio's mirror.
    pub files: usize,
    pub index_ms: u64,
    pub reindex: usize,
    pub reindex_ms: u64,
    /// Lookups (`impact --file` and `--diff-file`).
    pub queries: usize,
    pub query_ms: u64,
    /// Milliseconds that the clock path waited for indexio.
    pub critical_ms: u64,
    pub fallbacks: usize,
    /// One row per lookup, as the pane shows it during verify.
    pub rows: Vec<String>,
    /// Export locks broken by repair rounds (the guards' summary).
    pub lock_breaks: Option<usize>,
    /// The guards' summary line.
    pub guard: Option<String>,
}

impl ToolTotals {
    /// Calls to indexio: the index, the re-indexes and the lookups.
    pub fn calls(&self) -> usize {
        usize::from(self.index_ms > 0) + self.reindex + self.queries
    }

    /// Milliseconds of all indexio calls.
    pub fn total_ms(&self) -> u64 {
        self.index_ms + self.reindex_ms + self.query_ms
    }

    /// The pane line: `indexio v0.1.15 · 46 files · index 112 ms · 15
    /// reindex 0.5 s · 6 impact 0.13 s · on clock path 0.03 s`.
    pub fn line(&self) -> Option<String> {
        let name = self.indexio.as_ref()?;
        let mut parts = vec![name.clone(), format!("{} files", self.files)];
        if self.index_ms > 0 {
            parts.push(format!("index {} ms", self.index_ms));
        }
        if self.reindex > 0 {
            parts.push(format!(
                "{} reindex {:.1} s",
                self.reindex,
                self.reindex_ms as f64 / 1000.0
            ));
        }
        if self.queries > 0 {
            parts.push(format!(
                "{} impact {:.2} s",
                self.queries,
                self.query_ms as f64 / 1000.0
            ));
        }
        parts.push(format!(
            "on clock path {:.2} s",
            self.critical_ms as f64 / 1000.0
        ));
        if self.fallbacks > 0 {
            parts.push(format!("{} fallbacks", self.fallbacks));
        }
        if self.indexio_down.is_some() {
            parts.push("off".to_string());
        }
        Some(parts.join(" · "))
    }

    /// True if the arm did not run on indexio as asked: no version, turned
    /// off, or every lookup fell back.
    pub fn indexio_degraded(&self) -> bool {
        self.indexio_version.is_none()
            || self.indexio_down.is_some()
            || (self.fallbacks > 0 && self.queries == 0)
    }
}

#[derive(Debug, Clone)]
pub struct CheckInfo {
    pub name: String,
    pub state: CheckState,
    pub seconds: f64,
    pub tail: String,
    pub started: f64,
}

#[derive(Debug, Clone)]
pub struct ArmState {
    pub arm: Arm,
    pub phase: Phase,
    /// Show-clock second the arm started.
    pub started: Option<f64>,
    /// Show-clock second the arm's last model output arrived.
    pub ended: Option<f64>,
    /// Show-clock second planning ended and the wave started.
    pub wave_started: Option<f64>,
    /// Show-clock second the wave ended and check-and-repair started.
    pub verify_started: Option<f64>,
    pub verified: Option<VerifyInfo>,
    pub streams: BTreeMap<u64, Stream>,
    pub rows: Vec<Row>,
    /// Output tokens per second over time: (seconds since arm start, rate).
    pub rate: Vec<(f64, f64)>,
    pub peak_rate: f64,
    pub peak_streams: usize,
    pub requests: usize,
    pub failures: usize,
    pub finish: Option<String>,
    pub outcome: Option<FinalInfo>,
    /// Quality checks, in the order they started.
    pub checks: Vec<CheckInfo>,
    /// Helper tools (indexio) and the repair guards.
    pub tools: ToolTotals,
    pub focus: Option<u64>,
    focus_since: f64,
    raw: String,
    raw_dirty: bool,
    samples: VecDeque<(f64, f64)>,
}

impl ArmState {
    fn new(arm: Arm) -> Self {
        Self {
            arm,
            phase: Phase::Waiting,
            started: None,
            ended: None,
            wave_started: None,
            verify_started: None,
            verified: None,
            streams: BTreeMap::new(),
            rows: Vec::new(),
            rate: Vec::new(),
            peak_rate: 0.0,
            peak_streams: 0,
            requests: 0,
            failures: 0,
            finish: None,
            outcome: None,
            checks: Vec::new(),
            tools: ToolTotals::default(),
            focus: None,
            focus_since: 0.0,
            raw: String::new(),
            raw_dirty: false,
            samples: VecDeque::new(),
        }
    }

    pub fn running(&self) -> bool {
        self.started.is_some() && self.ended.is_none()
    }

    pub fn finished(&self) -> bool {
        self.ended.is_some()
    }

    /// Seconds from the arm's start to its last model output (or to `now`).
    pub fn elapsed(&self, now: f64) -> f64 {
        match self.started {
            Some(started) => (self.ended.unwrap_or(now) - started).max(0.0),
            None => 0.0,
        }
    }

    pub fn planning_seconds(&self, now: f64) -> Option<f64> {
        let started = self.started?;
        match (self.arm, self.wave_started) {
            (Arm::Single, _) => None,
            (_, Some(wave)) => Some(wave - started),
            (_, None) => Some(self.ended.unwrap_or(now) - started),
        }
    }

    /// Seconds from the arm's start to the end of code generation (before
    /// check-and-repair). `None` when the arm had no such step.
    pub fn code_seconds(&self) -> Option<f64> {
        Some(self.verify_started? - self.started?)
    }

    pub fn open_streams(&self) -> usize {
        self.streams.values().filter(|stream| stream.open).count()
    }

    /// Output tokens so far. Finished requests count their reported usage;
    /// requests still streaming count an estimate from their text length.
    pub fn output_tokens(&self, chars_per_token: f64) -> f64 {
        self.streams
            .values()
            .map(|stream| match stream.output_tokens {
                Some(tokens) => tokens as f64,
                None => stream.chars as f64 / chars_per_token,
            })
            .sum()
    }

    pub fn input_tokens(&self) -> u64 {
        self.streams
            .values()
            .filter_map(|stream| stream.input_tokens)
            .sum()
    }

    /// Input tokens that the provider reported as prompt-cache hits.
    pub fn cached_input_tokens(&self) -> u64 {
        self.streams
            .values()
            .filter_map(|stream| stream.cached_tokens)
            .sum()
    }

    /// True when every request reported real usage (no estimate remains).
    pub fn usage_exact(&self) -> bool {
        !self.streams.is_empty()
            && self
                .streams
                .values()
                .all(|stream| stream.output_tokens.is_some())
    }

    pub fn files_done(&self) -> usize {
        self.rows
            .iter()
            .filter(|row| matches!(row.state, RowState::Done | RowState::Rendered))
            .count()
    }

    pub fn lines_written(&self) -> usize {
        self.outcome
            .as_ref()
            .map(|outcome| outcome.files.iter().map(|file| file.lines).sum())
            .unwrap_or(0)
    }

    pub fn current_rate(&self) -> f64 {
        if self.running() {
            self.rate.last().map(|point| point.1).unwrap_or(0.0)
        } else {
            0.0
        }
    }

    pub fn focus_stream(&self) -> Option<&Stream> {
        self.focus.and_then(|id| self.streams.get(&id))
    }

    fn row_for_stream(&mut self, id: u64) -> Option<&mut Row> {
        self.rows.iter_mut().find(|row| row.stream == Some(id))
    }

    /// The single arm's rows come from the text itself: each file appears
    /// when the model starts to write it.
    fn rescan(&mut self, now: f64) {
        if !self.raw_dirty {
            return;
        }
        self.raw_dirty = false;
        let found = if crate::repair::engcheck() {
            slimtoken::project::scan(&self.raw)
        } else {
            extract::scan(&self.raw)
        };
        for found in found {
            let index = match self.rows.iter().position(|row| row.path == found.path) {
                Some(index) => index,
                None => {
                    self.rows.push(Row {
                        path: found.path.clone(),
                        state: RowState::Streaming,
                        chars: 0,
                        tokens: None,
                        started: Some(now),
                        ended: None,
                        attempts: 1,
                        stream: None,
                    });
                    self.rows.len() - 1
                }
            };
            let row = &mut self.rows[index];
            row.chars = found.content.len();
            if found.closed && row.state != RowState::Done {
                row.state = RowState::Done;
                row.ended = Some(now);
            } else if !found.closed {
                row.state = RowState::Streaming;
                row.ended = None;
            }
        }
    }

    fn sample(&mut self, now: f64, chars_per_token: f64) {
        let Some(started) = self.started else {
            return;
        };
        if !self.running() {
            return;
        }
        let tokens = self.output_tokens(chars_per_token);
        self.samples.push_back((now, tokens));
        while self.samples.len() > 2 && self.samples[1].0 <= now - 1.0 {
            self.samples.pop_front();
        }
        let (then, tokens_then) = self.samples[0];
        let span = now - then;
        if span < 0.25 {
            return;
        }
        let rate = ((tokens - tokens_then) / span).max(0.0);
        let t = now - started;
        if self.rate.last().is_none_or(|last| t - last.0 >= 0.2) {
            self.rate.push((t, rate));
            self.peak_rate = self.peak_rate.max(rate);
        }
    }

    fn refocus(&mut self, now: f64) {
        let open: Vec<u64> = self
            .streams
            .iter()
            .filter(|(_, stream)| stream.open && stream.chars > 0 && stream.kind != Kind::Aux)
            .map(|(id, _)| *id)
            .collect();
        if open.is_empty() {
            return;
        }
        let current_open = self.focus.is_some_and(|id| open.contains(&id));
        if current_open && now - self.focus_since < 2.5 {
            return;
        }
        // Round-robin over the open streams so the tail shows many files.
        let next = match self.focus {
            Some(current) => open
                .iter()
                .find(|id| **id > current)
                .or(open.first())
                .copied(),
            None => open.first().copied(),
        };
        if next != self.focus {
            self.focus = next;
            self.focus_since = now;
        }
    }
}

pub struct Show {
    pub meta: Meta,
    pub arms: [ArmState; 2],
    /// Show-clock seconds.
    pub now: f64,
    pub chars_per_token: f64,
}

impl Show {
    pub fn new() -> Self {
        Self {
            meta: Meta::default(),
            arms: [ArmState::new(Arm::Single), ArmState::new(Arm::Slim)],
            now: 0.0,
            chars_per_token: DEFAULT_CHARS_PER_TOKEN,
        }
    }

    pub fn arm(&self, arm: Arm) -> &ArmState {
        &self.arms[arm.index()]
    }

    pub fn all_finished(&self) -> bool {
        self.arms.iter().all(|arm| arm.outcome.is_some())
    }

    pub fn is_demo(&self) -> bool {
        self.meta.source == "demo"
    }

    /// How many times faster the wave finished. While the single stream
    /// still runs, this is the factor so far.
    pub fn speedup(&self) -> Option<f64> {
        let slim = self.arm(Arm::Slim);
        let single = self.arm(Arm::Single);
        if !slim.finished() || single.started.is_none() {
            return None;
        }
        let wave = slim.elapsed(self.now);
        (wave > 0.0).then(|| single.elapsed(self.now) / wave)
    }

    pub fn apply(&mut self, timed: &Timed) {
        let t = timed.t as f64 / 1000.0;
        if let Ev::Meta {
            objective,
            provider,
            model,
            source,
            recorded_at,
            ..
        } = &timed.ev
        {
            self.meta = Meta {
                objective: objective.clone(),
                provider: provider.clone(),
                model: model.clone(),
                source: source.clone(),
                recorded_at: recorded_at.clone(),
            };
            return;
        }
        let Some(arm) = timed.ev.arm() else {
            return;
        };
        let state = &mut self.arms[arm.index()];
        match &timed.ev {
            Ev::Meta { .. } => {}
            Ev::ArmStart { .. } => {
                state.started = Some(t);
            }
            Ev::Phase { phase, .. } => {
                state.phase = *phase;
                match phase {
                    Phase::Wave => state.wave_started = Some(t),
                    Phase::Verify => state.verify_started = Some(t),
                    Phase::Done | Phase::Failed => {
                        state.rescan(t);
                        state.ended.get_or_insert(t);
                    }
                    _ => {}
                }
            }
            Ev::Planned { files, .. } => {
                for file in files {
                    if state.rows.iter().any(|row| row.path == file.path) {
                        continue;
                    }
                    state.rows.push(Row {
                        path: file.path.clone(),
                        state: if file.rendered {
                            RowState::Rendered
                        } else {
                            RowState::Queued
                        },
                        chars: 0,
                        tokens: None,
                        started: None,
                        ended: file.rendered.then_some(t),
                        attempts: 0,
                        stream: None,
                    });
                }
            }
            Ev::Open {
                id,
                label,
                kind,
                continues,
                ..
            } => {
                state.requests += 1;
                state.streams.insert(
                    *id,
                    Stream {
                        label: label.clone(),
                        kind: *kind,
                        chars: 0,
                        tail: String::new(),
                        open: true,
                        input_tokens: None,
                        cached_tokens: None,
                        output_tokens: None,
                        last_delta: t,
                        continues: continues.is_some(),
                    },
                );
                state.peak_streams = state.peak_streams.max(state.open_streams());
                // `nocut`: a continuation streams on into its file's row and
                // keeps what the cut stream wrote.
                let continued = continues
                    .as_ref()
                    .and_then(|path| state.rows.iter().position(|row| row.path == *path));
                if let (Some(index), Kind::File) = (continued, kind) {
                    let row = &mut state.rows[index];
                    row.state = RowState::Streaming;
                    row.ended = None;
                    row.stream = Some(*id);
                } else if *kind == Kind::File {
                    let path = continues.as_ref().unwrap_or(label);
                    let index = match state.rows.iter().position(|row| row.path == *path) {
                        Some(index) => index,
                        None => {
                            state.rows.push(Row {
                                path: path.clone(),
                                state: RowState::Queued,
                                chars: 0,
                                tokens: None,
                                started: None,
                                ended: None,
                                attempts: 0,
                                stream: None,
                            });
                            state.rows.len() - 1
                        }
                    };
                    let row = &mut state.rows[index];
                    row.state = RowState::Streaming;
                    row.chars = 0;
                    row.tokens = None;
                    row.started = Some(t);
                    row.ended = None;
                    row.attempts += 1;
                    row.stream = Some(*id);
                }
                if state.focus.is_none() && *kind != Kind::Aux {
                    state.focus = Some(*id);
                    state.focus_since = t;
                }
            }
            Ev::Delta { id, text, .. } => {
                if let Some(stream) = state.streams.get_mut(id) {
                    stream.chars += text.len();
                    stream.last_delta = t;
                    stream.tail.push_str(text);
                    if stream.tail.len() > TAIL_BYTES * 2 {
                        let mut cut = stream.tail.len() - TAIL_BYTES;
                        while !stream.tail.is_char_boundary(cut) {
                            cut += 1;
                        }
                        stream.tail.drain(..cut);
                    }
                    if stream.kind == Kind::Single {
                        state.raw.push_str(text);
                        state.raw_dirty = true;
                    }
                }
                let length = text.len();
                if let Some(row) = state.row_for_stream(*id) {
                    row.chars += length;
                }
            }
            Ev::Close {
                id,
                ok,
                input_tokens,
                output_tokens,
                cached_tokens,
                finish,
                ..
            } => {
                let mut kind = Kind::Aux;
                let mut continues = false;
                if let Some(stream) = state.streams.get_mut(id) {
                    stream.open = false;
                    stream.input_tokens = *input_tokens;
                    stream.cached_tokens = *cached_tokens;
                    // A failed request reports no usage; keep its estimate
                    // out of the totals by pinning it to the text it sent.
                    stream.output_tokens = *output_tokens;
                    kind = stream.kind;
                    continues = stream.continues;
                }
                if !ok {
                    state.failures += 1;
                }
                if kind == Kind::Single {
                    state.finish = finish.clone();
                }
                if let Some(row) = state.row_for_stream(*id) {
                    row.ended = Some(t);
                    if *ok {
                        row.state = RowState::Done;
                        // A continuation's tokens add to the cut stream's.
                        row.tokens = match (continues, row.tokens, *output_tokens) {
                            (true, Some(cut), Some(more)) => Some(cut + more),
                            (true, cut, None) => cut,
                            (_, _, tokens) => tokens,
                        };
                    } else {
                        row.state = RowState::Failed;
                    }
                }
            }
            Ev::Verified {
                before_failed,
                after_failed,
                tests_passed,
                repaired,
                seconds,
                ..
            } => {
                state.verified = Some(VerifyInfo {
                    before_failed: *before_failed,
                    after_failed: *after_failed,
                    tests_passed: *tests_passed,
                    repaired: *repaired,
                    seconds: *seconds,
                });
            }
            Ev::Final {
                ok,
                files,
                out_dir,
                note,
                ..
            } => {
                state.ended.get_or_insert(t);
                state.outcome = Some(FinalInfo {
                    ok: *ok,
                    files: files.clone(),
                    out_dir: out_dir.clone(),
                    note: note.clone(),
                });
            }
            Ev::Check {
                name,
                state: check,
                seconds,
                tail,
                ..
            } => {
                let info = CheckInfo {
                    name: name.clone(),
                    state: *check,
                    seconds: *seconds,
                    tail: tail.clone(),
                    started: t,
                };
                match state.checks.iter_mut().find(|item| item.name == *name) {
                    Some(existing) => {
                        let started = existing.started;
                        *existing = info;
                        if *check != CheckState::Running {
                            existing.started = started;
                        }
                    }
                    None => state.checks.push(info),
                }
            }
            Ev::Tool {
                tool,
                op,
                ms,
                critical_ms,
                files,
                hits,
                note,
                ..
            } => {
                let tools = &mut state.tools;
                if tool == "guard" {
                    tools.lock_breaks = Some(*hits);
                    tools.guard = Some(note.clone());
                } else if tool == "indexio" {
                    tools.critical_ms += critical_ms;
                    match op.as_str() {
                        "start" => {
                            tools.indexio = Some(note.clone());
                            tools.indexio_version = note
                                .strip_prefix("indexio v")
                                .filter(|version| !version.is_empty())
                                .map(str::to_string);
                        }
                        "down" => {
                            tools.indexio_down = Some(note.clone());
                            tools.rows.push(format!("indexio off: {note}"));
                        }
                        "index" => {
                            tools.index_ms += ms;
                            tools.files = *files;
                        }
                        "reindex" => {
                            tools.reindex += 1;
                            tools.reindex_ms += ms;
                            tools.files = *files;
                        }
                        "impact" | "diff" => {
                            tools.queries += 1;
                            tools.query_ms += ms;
                            tools.rows.push(note.clone());
                        }
                        "fallback" => {
                            tools.fallbacks += 1;
                            tools.rows.push(format!("indexio fallback: {note}"));
                        }
                        _ => {}
                    }
                }
            }
        }
        self.recalibrate();
    }

    /// Learn characters-per-token from the requests that reported usage, so
    /// the live estimate for requests still streaming tracks the real rate.
    fn recalibrate(&mut self) {
        let (mut chars, mut tokens) = (0usize, 0u64);
        for arm in &self.arms {
            for stream in arm.streams.values() {
                if let (false, Some(output), Kind::File | Kind::Single) =
                    (stream.open, stream.output_tokens, stream.kind)
                {
                    chars += stream.chars;
                    tokens += output;
                }
            }
        }
        if tokens >= 200 {
            self.chars_per_token = (chars as f64 / tokens as f64).clamp(2.0, 6.0);
        }
    }

    /// Advance the show clock: refresh derived state for a new frame.
    pub fn tick(&mut self, now: f64) {
        self.now = now;
        let chars_per_token = self.chars_per_token;
        for arm in &mut self.arms {
            arm.rescan(now);
            arm.sample(now, chars_per_token);
            arm.refocus(now);
        }
    }
}

#[cfg(test)]
mod tool_tests {
    use super::*;

    fn tool(op: &str, ms: u64, critical_ms: u64, files: usize, note: &str) -> Timed {
        Timed {
            t: 0,
            ev: Ev::Tool {
                arm: Arm::Slim,
                tool: "indexio".to_string(),
                op: op.to_string(),
                ms,
                critical_ms,
                files,
                hits: 0,
                note: note.to_string(),
            },
        }
    }

    #[test]
    fn tool_events_add_up_per_arm() {
        let mut show = Show::new();
        show.apply(&tool("start", 0, 0, 0, "indexio v0.1.15"));
        show.apply(&tool("index", 112, 0, 40, ""));
        for _ in 0..15 {
            show.apply(&tool("reindex", 33, 0, 46, ""));
        }
        for _ in 0..6 {
            show.apply(&tool("impact", 22, 5, 3, "indexio impact lib/board.ts"));
        }
        let tools = &show.arm(Arm::Slim).tools;
        assert_eq!(
            tools.line().unwrap(),
            "indexio v0.1.15 · 46 files · index 112 ms · 15 reindex 0.5 s · 6 impact 0.13 s · on clock path 0.03 s"
        );
        assert_eq!(tools.calls(), 22);
        assert_eq!(tools.rows.len(), 6);
        assert_eq!(tools.indexio_version.as_deref(), Some("0.1.15"));
        assert!(!tools.indexio_degraded());
        assert!(show.arm(Arm::Single).tools.line().is_none());
        // An old recording without the event, and a line with only the arm.
        let old: Timed = serde_json::from_str(r#"{"t":5,"ev":"tool","arm":"slim"}"#).unwrap();
        show.apply(&old);
        assert_eq!(show.arm(Arm::Slim).tools.queries, 6);
    }

    /// `nocut`: a cut stream and its continuation leave one row for the
    /// file, with the chars and tokens of both.
    #[test]
    fn a_continued_stream_adds_to_the_cut_files_row() {
        let at = |t: u64, ev: Ev| Timed { t, ev };
        let open = |id: u64, label: &str, continues: Option<&str>| Ev::Open {
            arm: Arm::Slim,
            id,
            label: label.to_string(),
            kind: Kind::File,
            continues: continues.map(str::to_string),
        };
        let delta = |id: u64, text: &str| Ev::Delta {
            arm: Arm::Slim,
            id,
            text: text.to_string(),
        };
        let close = |id: u64, tokens: u64, finish: &str| Ev::Close {
            arm: Arm::Slim,
            id,
            ok: true,
            input_tokens: Some(10),
            output_tokens: Some(tokens),
            cached_tokens: Some(6),
            finish: Some(finish.to_string()),
            error: None,
        };
        let mut show = Show::new();
        show.apply(&at(0, open(1, "src/x.ts", None)));
        show.apply(&at(1, delta(1, "abcd")));
        show.apply(&at(2, close(1, 4, "length")));
        show.apply(&at(3, open(2, "src/x.ts (continued)", Some("src/x.ts"))));
        assert_eq!(show.arm(Arm::Slim).rows[0].state, RowState::Streaming);
        show.apply(&at(4, delta(2, "efg")));
        show.apply(&at(5, close(2, 3, "stop")));
        let rows = &show.arm(Arm::Slim).rows;
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert_eq!(rows[0].path, "src/x.ts");
        assert_eq!(rows[0].chars, 7);
        assert_eq!(rows[0].tokens, Some(7));
        assert_eq!(rows[0].attempts, 1);
        assert_eq!(rows[0].state, RowState::Done);
        // Cache hits add up over the requests, beside the input tokens.
        assert_eq!(show.arm(Arm::Slim).input_tokens(), 20);
        assert_eq!(show.arm(Arm::Slim).cached_input_tokens(), 12);
        // The field stays out of a recording that has no continuation.
        let line = serde_json::to_string(&open(3, "src/y.ts", None)).unwrap();
        assert!(!line.contains("continues"), "{line}");
    }

    #[test]
    fn a_failed_version_check_or_a_turned_off_indexio_is_degraded() {
        let mut show = Show::new();
        show.apply(&tool(
            "start",
            0,
            0,
            0,
            "indexio (version check failed: cannot start indexio)",
        ));
        let tools = &show.arm(Arm::Slim).tools;
        assert_eq!(tools.indexio_version, None);
        assert!(tools.indexio_degraded());
        let mut show = Show::new();
        show.apply(&tool("start", 0, 0, 0, "indexio v0.1.15"));
        show.apply(&tool("down", 0, 0, 0, "indexio lookup timed out after 2s"));
        let tools = &show.arm(Arm::Slim).tools;
        assert!(tools.indexio_degraded());
        assert!(tools.line().unwrap().ends_with(" · off"));
    }
}
