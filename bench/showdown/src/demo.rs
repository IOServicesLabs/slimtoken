//! A simulated timeline, for layout work and rehearsals without an API key.
//!
//! Nothing here is a measurement. The screen marks every demo frame as
//! simulated. The stream rates below sit inside the ranges the SlimToken
//! README reports for DeepSeek, so the picture has a realistic shape.

use crate::events::{Arm, Ev, FileStat, Kind, Phase, PlannedFile, Timed};

/// Simulated single-stream rate, output tokens per second.
const SINGLE_RATE: f64 = 290.0;
/// Simulated characters per token.
const CHARS_PER_TOKEN: f64 = 3.4;
/// Seconds between text deltas.
const STEP: f64 = 0.08;

pub const DEMO_OBJECTIVE: &str = "Build a Next.js habit tracker web app in TypeScript: \
add and archive habits, a daily check-in grid, streaks, a weekly stats page, and local \
storage persistence. Include Vitest tests for the lib modules and a README.";

/// (path, output tokens, rendered by the engine with no model call)
const FILES: &[(&str, u32, bool)] = &[
    ("package.json", 190, true),
    ("tsconfig.json", 210, true),
    ("next.config.mjs", 40, true),
    ("lib/types.ts", 360, false),
    ("lib/habits.ts", 980, false),
    ("lib/streaks.ts", 870, false),
    ("lib/stats.ts", 910, false),
    ("lib/storage.ts", 640, false),
    ("lib/dates.ts", 520, false),
    ("app/globals.css", 1180, false),
    ("app/layout.tsx", 380, false),
    ("app/page.tsx", 1040, false),
    ("app/stats/page.tsx", 890, false),
    ("components/HabitForm.tsx", 760, false),
    ("components/HabitList.tsx", 820, false),
    ("components/CheckinGrid.tsx", 1120, false),
    ("components/StreakBadge.tsx", 410, false),
    ("components/WeekChart.tsx", 940, false),
    ("tests/01_habits.test.ts", 1210, false),
    ("tests/02_stats.test.ts", 1090, false),
    ("README.md", 280, true),
];

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        self.0 >> 33
    }

    /// Uniform in [low, high).
    fn range(&mut self, low: f64, high: f64) -> f64 {
        low + (self.next() % 10_000) as f64 / 10_000.0 * (high - low)
    }
}

const TS_LINES: &[&str] = &[
    "export function toggleHabit(state: HabitState, id: string, day: string): HabitState {",
    "  const habit = state.habits.find((item) => item.id === id);",
    "  if (!habit) throw new RangeError(`unknown habit: ${id}`);",
    "  const done = new Set(habit.checkins);",
    "  if (done.has(day)) done.delete(day); else done.add(day);",
    "  return { ...state, habits: state.habits.map((item) => (item.id === id ? { ...item, checkins: [...done].sort() } : item)) };",
    "}",
    "",
    "export function currentStreak(checkins: string[], today: string): number {",
    "  let streak = 0;",
    "  for (let day = today; checkins.includes(day); day = previousDay(day)) streak += 1;",
    "  return streak;",
    "}",
    "",
    "export function weeklyTotals(habits: Habit[], weekStart: string): WeekTotals {",
    "  const days = Array.from({ length: 7 }, (_, index) => addDays(weekStart, index));",
    "  const perDay = days.map((day) => habits.filter((habit) => habit.checkins.includes(day)).length);",
    "  return { days, perDay, total: perDay.reduce((sum, count) => sum + count, 0) };",
    "}",
    "",
];

const TSX_LINES: &[&str] = &[
    "\"use client\";",
    "import { useEffect, useState } from \"react\";",
    "import { loadState, saveState } from \"../lib/storage\";",
    "import { toggleHabit } from \"../lib/habits\";",
    "",
    "export default function CheckinGrid({ habits, week }: { habits: Habit[]; week: string[] }) {",
    "  const [state, setState] = useState(() => loadState());",
    "  useEffect(() => saveState(state), [state]);",
    "  return (",
    "    <section className=\"grid\">",
    "      {habits.map((habit) => (",
    "        <div key={habit.id} className=\"grid-row\">",
    "          <span className=\"habit-name\">{habit.name}</span>",
    "          {week.map((day) => (",
    "            <button key={day} className={habit.checkins.includes(day) ? \"cell done\" : \"cell\"}",
    "              onClick={() => setState((current) => toggleHabit(current, habit.id, day))} />",
    "          ))}",
    "        </div>",
    "      ))}",
    "    </section>",
    "  );",
    "}",
    "",
];

const CSS_LINES: &[&str] = &[
    ".grid { display: grid; gap: 0.5rem; padding: 1rem; }",
    ".grid-row { display: flex; align-items: center; gap: 0.375rem; }",
    ".habit-name { flex: 1; font-weight: 600; }",
    ".cell { width: 2rem; height: 2rem; border-radius: 0.5rem; border: 1px solid var(--line); }",
    ".cell.done { background: var(--accent); border-color: var(--accent); }",
    ".card { border: 1px solid var(--line); border-radius: 0.75rem; padding: 1rem; }",
    ".nav { display: flex; gap: 1rem; padding: 0.75rem 1rem; }",
    ".streak { font-variant-numeric: tabular-nums; color: var(--accent); }",
    "",
];

const TEST_LINES: &[&str] = &[
    "import { describe, it, expect } from \"vitest\";",
    "import { addHabit, toggleHabit, currentStreak } from \"../lib/habits\";",
    "",
    "describe(\"toggleHabit\", () => {",
    "  it(\"adds a check-in for a new day\", () => {",
    "    const state = toggleHabit(seed(), \"h1\", \"2026-11-02\");",
    "    expect(state.habits[0].checkins).toEqual([\"2026-11-02\"]);",
    "  });",
    "  it(\"throws for an unknown habit\", () => {",
    "    expect(() => toggleHabit(seed(), \"nope\", \"2026-11-02\")).toThrow(RangeError);",
    "  });",
    "});",
    "",
];

const JSON_LINES: &[&str] = &[
    "{",
    "  \"name\": \"habit-tracker\",",
    "  \"private\": true,",
    "  \"scripts\": { \"dev\": \"next dev\", \"build\": \"next build\", \"test\": \"vitest run\" },",
    "  \"dependencies\": { \"next\": \"^16.4.3\", \"react\": \"^19.3.1\", \"react-dom\": \"^19.3.1\" }",
    "}",
];

const MD_LINES: &[&str] = &[
    "# Habit Tracker",
    "",
    "A small Next.js app that tracks daily habits and streaks.",
    "",
    "## Run",
    "",
    "    npm install",
    "    npm run dev",
    "",
];

fn bank(path: &str) -> &'static [&'static str] {
    if path.contains(".test.") {
        TEST_LINES
    } else if path.ends_with(".tsx") {
        TSX_LINES
    } else if path.ends_with(".css") {
        CSS_LINES
    } else if path.ends_with(".json") {
        JSON_LINES
    } else if path.ends_with(".md") {
        MD_LINES
    } else {
        TS_LINES
    }
}

fn fake_file(path: &str, tokens: u32) -> String {
    let lines = bank(path);
    let target = (tokens as f64 * CHARS_PER_TOKEN) as usize;
    let mut content = String::with_capacity(target + 80);
    let mut index = 0;
    while content.len() < target {
        content.push_str(lines[index % lines.len()]);
        content.push('\n');
        index += 1;
    }
    content
}

fn lang(path: &str) -> &'static str {
    match path.rsplit('.').next().unwrap_or("") {
        "tsx" => "tsx",
        "ts" => "ts",
        "css" => "css",
        "json" => "json",
        "mjs" => "js",
        "md" => "markdown",
        _ => "",
    }
}

fn ms(seconds: f64) -> u64 {
    (seconds * 1000.0).round() as u64
}

/// Emit `text` as deltas at `rate` tokens per second, starting at `start`.
/// Returns the second the last delta lands.
fn stream_text(
    out: &mut Vec<Timed>,
    arm: Arm,
    id: u64,
    text: &str,
    start: f64,
    rate: f64,
    rng: &mut Rng,
) -> f64 {
    let mut t = start;
    let mut offset = 0;
    while offset < text.len() {
        // Rate wobbles a little from step to step, like a real stream.
        let step_chars = (rate * rng.range(0.8, 1.2) * CHARS_PER_TOKEN * STEP).max(1.0) as usize;
        let mut end = (offset + step_chars).min(text.len());
        while !text.is_char_boundary(end) {
            end += 1;
        }
        t += STEP;
        out.push(Timed {
            t: ms(t),
            ev: Ev::Delta {
                arm,
                id,
                text: text[offset..end].to_string(),
            },
        });
        offset = end;
    }
    t
}

fn close(arm: Arm, id: u64, t: f64, text_len: usize, input: u64, cached: Option<u64>) -> Timed {
    Timed {
        t: ms(t),
        ev: Ev::Close {
            arm,
            id,
            ok: true,
            input_tokens: Some(input),
            output_tokens: Some((text_len as f64 / CHARS_PER_TOKEN).round() as u64),
            cached_tokens: cached,
            finish: Some("stop".to_string()),
            error: None,
        },
    }
}

fn stats(files: &[(String, String)]) -> Vec<FileStat> {
    files
        .iter()
        .map(|(path, content)| FileStat {
            path: path.clone(),
            lines: content.lines().count(),
            bytes: content.len(),
        })
        .collect()
}

/// Build the simulated timeline, sorted by time.
pub fn timeline(seed: u64) -> Vec<Timed> {
    let mut rng = Rng(seed ^ 0x5EED_5EED);
    let mut out = Vec::new();
    let mut next_id = 1u64;
    let mut id = || {
        next_id += 1;
        next_id - 1
    };

    out.push(Timed {
        t: 0,
        ev: Ev::Meta {
            objective: DEMO_OBJECTIVE.to_string(),
            provider: "deepseek".to_string(),
            model: "deepseek-flash".to_string(),
            source: "demo".to_string(),
            recorded_at: "simulated".to_string(),
            check_cmd: None,
        },
    });

    let files: Vec<(String, String, bool)> = FILES
        .iter()
        .map(|(path, tokens, rendered)| (path.to_string(), fake_file(path, *tokens), *rendered))
        .collect();

    // ---- Single stream: one answer, one file after another --------------
    let arm = Arm::Single;
    out.push(Timed {
        t: 0,
        ev: Ev::ArmStart { arm },
    });
    out.push(Timed {
        t: 0,
        ev: Ev::Phase {
            arm,
            phase: Phase::Streaming,
        },
    });
    let single_id = id();
    out.push(Timed {
        t: 0,
        ev: Ev::Open {
            arm,
            id: single_id,
            label: "single response".to_string(),
            kind: Kind::Single,
            continues: None,
        },
    });
    let mut answer = String::from("Here is the complete habit tracker app.\n\n");
    for (path, content, _) in &files {
        answer.push_str(&format!(
            "### `{path}`\n\n```{}\n{content}```\n\n",
            lang(path)
        ));
    }
    answer.push_str("Run `npm install`, then `npm run dev`.\n");
    let end = stream_text(
        &mut out,
        arm,
        single_id,
        &answer,
        0.7,
        SINGLE_RATE,
        &mut rng,
    );
    out.push(close(arm, single_id, end + 0.05, answer.len(), 212, None));
    out.push(Timed {
        t: ms(end + 0.05),
        ev: Ev::Phase {
            arm,
            phase: Phase::Done,
        },
    });
    out.push(Timed {
        t: ms(end + 0.1),
        ev: Ev::Final {
            arm,
            ok: true,
            files: stats(
                &files
                    .iter()
                    .map(|(path, content, _)| (path.clone(), content.clone()))
                    .collect::<Vec<_>>(),
            ),
            out_dir: "(simulated: no files written)".to_string(),
            note: None,
        },
    });

    // ---- SlimToken: plan, then every file at the same time ---------------
    let arm = Arm::Slim;
    out.push(Timed {
        t: 0,
        ev: Ev::ArmStart { arm },
    });
    out.push(Timed {
        t: 0,
        ev: Ev::Phase {
            arm,
            phase: Phase::Planning,
        },
    });
    // Skeleton call, then the export-contract fragments in parallel.
    let skeleton = id();
    out.push(Timed {
        t: 0,
        ev: Ev::Open {
            arm,
            id: skeleton,
            label: "architect".to_string(),
            kind: Kind::Plan,
            continues: None,
        },
    });
    let skeleton_text = "{\"files\": [{\"path\": \"lib/habits.ts\", \"purpose\": \"habit state operations\"}, {\"path\": \"lib/streaks.ts\", \"purpose\": \"streak math\"}, {\"path\": \"app/page.tsx\", \"purpose\": \"daily check-in page\"}], \"test_files\": [], \"readme\": false}\n".repeat(3);
    let skeleton_end = stream_text(
        &mut out,
        arm,
        skeleton,
        &skeleton_text,
        0.55,
        260.0,
        &mut rng,
    );
    out.push(close(
        arm,
        skeleton,
        skeleton_end,
        skeleton_text.len(),
        640,
        None,
    ));
    let mut plan_end = skeleton_end;
    for _ in 0..8 {
        let fragment = id();
        let start = skeleton_end + rng.range(0.02, 0.12);
        out.push(Timed {
            t: ms(start),
            ev: Ev::Open {
                arm,
                id: fragment,
                label: "architect".to_string(),
                kind: Kind::Plan,
                continues: None,
            },
        });
        let text = "{\"files\": [{\"path\": \"lib/habits.ts\", \"purpose\": \"habit state operations\", \"exports\": [\"export function toggleHabit(state: HabitState, id: string, day: string): HabitState  // unknown id -> throws RangeError\", \"export function addHabit(state: HabitState, name: string): HabitState  // empty name -> throws TypeError\"]}], \"test_files\": [], \"readme\": false}\n".repeat(2);
        let end = stream_text(
            &mut out,
            arm,
            fragment,
            &text,
            start + rng.range(0.45, 0.8),
            rng.range(150.0, 210.0),
            &mut rng,
        );
        out.push(close(arm, fragment, end, text.len(), 910, Some(520)));
        plan_end = plan_end.max(end);
    }
    let wave_start = plan_end + 0.15;
    out.push(Timed {
        t: ms(wave_start),
        ev: Ev::Planned {
            arm,
            files: files
                .iter()
                .map(|(path, _, rendered)| PlannedFile {
                    path: path.clone(),
                    rendered: *rendered,
                })
                .collect(),
        },
    });
    out.push(Timed {
        t: ms(wave_start),
        ev: Ev::Phase {
            arm,
            phase: Phase::Wave,
        },
    });
    let mut wave_end = wave_start;
    for (path, content, rendered) in &files {
        if *rendered {
            continue;
        }
        let worker = id();
        let start = wave_start + rng.range(0.0, 0.25);
        out.push(Timed {
            t: ms(start),
            ev: Ev::Open {
                arm,
                id: worker,
                label: path.clone(),
                kind: Kind::File,
                continues: None,
            },
        });
        let text = format!("```{}\n{content}```", lang(path));
        let end = stream_text(
            &mut out,
            arm,
            worker,
            &text,
            start + rng.range(0.5, 1.1),
            rng.range(118.0, 152.0),
            &mut rng,
        );
        out.push(close(arm, worker, end, text.len(), 1_480, Some(1_150)));
        wave_end = wave_end.max(end);
    }
    out.push(Timed {
        t: ms(wave_end + 0.05),
        ev: Ev::Phase {
            arm,
            phase: Phase::Done,
        },
    });
    out.push(Timed {
        t: ms(wave_end + 0.1),
        ev: Ev::Final {
            arm,
            ok: true,
            files: stats(
                &files
                    .iter()
                    .map(|(path, content, _)| (path.clone(), content.clone()))
                    .collect::<Vec<_>>(),
            ),
            out_dir: "(simulated: no files written)".to_string(),
            note: Some(format!("planned in {wave_start:.1}s")),
        },
    });

    out.sort_by_key(|timed| timed.t);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::Show;

    #[test]
    fn demo_timeline_finishes_both_arms_and_the_wave_wins() {
        let mut show = Show::new();
        let events = timeline(7);
        let last = events.last().expect("events").t as f64 / 1000.0;
        for event in &events {
            show.apply(event);
        }
        show.tick(last);
        assert!(show.all_finished());
        assert!(show.is_demo());
        let single = show.arm(Arm::Single);
        let slim = show.arm(Arm::Slim);
        assert_eq!(
            single.rows.len(),
            FILES.len(),
            "single rows parsed from text"
        );
        assert_eq!(slim.rows.len(), FILES.len());
        assert!(single.usage_exact() && slim.usage_exact());
        assert!(show.speedup().expect("speedup") > 1.5);
    }
}
