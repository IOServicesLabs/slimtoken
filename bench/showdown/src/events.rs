//! The event timeline. A live run emits these events, the screen is a pure
//! function of them, and a recording is the same events as JSON lines. A
//! replay therefore shows exactly what the live run showed.

use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::mpsc::UnboundedSender;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Arm {
    Single,
    Slim,
}

impl Arm {
    pub const ALL: [Arm; 2] = [Arm::Single, Arm::Slim];

    pub fn index(self) -> usize {
        match self {
            Arm::Single => 0,
            Arm::Slim => 1,
        }
    }

    pub fn dir_name(self) -> &'static str {
        match self {
            Arm::Single => "single",
            Arm::Slim => "slimtoken",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Waiting,
    Planning,
    Wave,
    Streaming,
    /// SlimToken's check-and-repair step: gate, repair, gate again.
    Verify,
    Done,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    /// The one request of the single-stream arm.
    Single,
    /// An architect (planning) request.
    Plan,
    /// A worker request that writes one file.
    File,
    /// A request that is not a deliverable (cache prime, repair, wiring).
    Aux,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlannedFile {
    pub path: String,
    /// Rendered by the engine with no model call (toolchain files, README).
    #[serde(default)]
    pub rendered: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileStat {
    pub path: String,
    pub lines: usize,
    pub bytes: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckState {
    Running,
    Pass,
    Fail,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "ev", rename_all = "snake_case")]
pub enum Ev {
    /// First line of every recording.
    Meta {
        objective: String,
        provider: String,
        model: String,
        /// "live" for a measured run, "demo" for simulated data.
        source: String,
        recorded_at: String,
        #[serde(default)]
        check_cmd: Option<String>,
    },
    ArmStart {
        arm: Arm,
    },
    Phase {
        arm: Arm,
        phase: Phase,
    },
    /// The wave plan: every file the arm will deliver.
    Planned {
        arm: Arm,
        files: Vec<PlannedFile>,
    },
    Open {
        arm: Arm,
        id: u64,
        label: String,
        kind: Kind,
        /// `nocut`: the file row that this stream continues (its path); the
        /// stream adds to that row instead of opening one of its own. Absent
        /// for every other stream.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        continues: Option<String>,
    },
    Delta {
        arm: Arm,
        id: u64,
        text: String,
    },
    Close {
        arm: Arm,
        id: u64,
        ok: bool,
        #[serde(default)]
        input_tokens: Option<u64>,
        #[serde(default)]
        output_tokens: Option<u64>,
        #[serde(default)]
        cached_tokens: Option<u64>,
        #[serde(default)]
        finish: Option<String>,
        #[serde(default)]
        error: Option<String>,
    },
    /// Result of SlimToken's check-and-repair step.
    Verified {
        arm: Arm,
        /// Failures (type errors + failed tests) the first gate run found.
        before_failed: u64,
        /// Failures after the repair round; equal to `before_failed` when
        /// the first gate run passed.
        after_failed: u64,
        /// Tests that pass after the step.
        tests_passed: u64,
        /// Files the engine repaired.
        repaired: usize,
        seconds: f64,
    },
    /// The arm is finished and its files are on disk.
    Final {
        arm: Arm,
        ok: bool,
        files: Vec<FileStat>,
        out_dir: String,
        #[serde(default)]
        note: Option<String>,
    },
    Check {
        arm: Arm,
        /// Step name: "install", "build", "test".
        #[serde(default)]
        name: String,
        state: CheckState,
        #[serde(default)]
        seconds: f64,
        #[serde(default)]
        tail: String,
    },
    /// A call of a helper tool of an arm (indexio for SlimToken's repair
    /// guards), or a summary of the guards (`tool` "guard"). Recordings
    /// from before this event replay without it.
    Tool {
        arm: Arm,
        /// "indexio" or "guard".
        #[serde(default)]
        tool: String,
        /// "start", "index", "reindex", "impact", "diff", "fallback",
        /// "error"; "summary" for the guards.
        #[serde(default)]
        op: String,
        /// Milliseconds of the call itself.
        #[serde(default)]
        ms: u64,
        /// Milliseconds that the arm's clock path waited for it.
        #[serde(default)]
        critical_ms: u64,
        #[serde(default)]
        files: usize,
        #[serde(default)]
        hits: usize,
        #[serde(default)]
        note: String,
    },
}

impl Ev {
    pub fn arm(&self) -> Option<Arm> {
        match self {
            Ev::Meta { .. } => None,
            Ev::ArmStart { arm }
            | Ev::Phase { arm, .. }
            | Ev::Planned { arm, .. }
            | Ev::Open { arm, .. }
            | Ev::Delta { arm, .. }
            | Ev::Close { arm, .. }
            | Ev::Verified { arm, .. }
            | Ev::Final { arm, .. }
            | Ev::Check { arm, .. }
            | Ev::Tool { arm, .. } => Some(*arm),
        }
    }
}

/// One event with its time on the show clock, in milliseconds.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Timed {
    pub t: u64,
    #[serde(flatten)]
    pub ev: Ev,
}

/// Stamps events with the show clock and sends them to the screen.
#[derive(Clone)]
pub struct Emitter {
    start: Instant,
    tx: UnboundedSender<Timed>,
    next_id: Arc<std::sync::atomic::AtomicU64>,
}

impl Emitter {
    pub fn new(tx: UnboundedSender<Timed>) -> Self {
        Self {
            start: Instant::now(),
            tx,
            next_id: Arc::new(std::sync::atomic::AtomicU64::new(1)),
        }
    }

    pub fn emit(&self, ev: Ev) {
        let t = self.start.elapsed().as_millis() as u64;
        // A closed receiver means the screen is gone; the run ends soon.
        let _ = self.tx.send(Timed { t, ev });
    }

    /// Show-clock seconds since this emitter was made.
    pub fn now(&self) -> f64 {
        self.start.elapsed().as_secs_f64()
    }

    pub fn stream_id(&self) -> u64 {
        self.next_id
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    }
}
