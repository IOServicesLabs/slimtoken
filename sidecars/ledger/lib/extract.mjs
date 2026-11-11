// extract.mjs — read one engine run dir and build one ledger row.
//
// The engine writes report.json (see src/scheduler.rs RunReport). The
// real shape is known, but this parser stays defensive: every field is
// looked up under several names and every lookup can return null. A
// missing or unreadable report.json is a usage error (the caller exits
// 2), never a silent row.
import { existsSync, readdirSync, readFileSync } from "node:fs";
import { join } from "node:path";

// Known pack JSON twins: file name -> gate name recorded in the row.
const GATE_TWINS = {
  "test.json": "test",
  "review.json": "review",
  "migrate.json": "migrate",
  "docs.json": "docs",
  "contract.json": "contract",
  "coverage.json": "coverage",
  "research.json": "research",
  "loop.json": "loop",
};

// First finite number wins. Defensive against shape drift.
function num(...values) {
  for (const v of values) {
    if (typeof v === "number" && Number.isFinite(v)) return v;
  }
  return null;
}

function readJsonSafe(path) {
  try {
    return JSON.parse(readFileSync(path, "utf8"));
  } catch {
    return null;
  }
}

// Wall/duration ms: top-level totals first, then a phases breakdown
// (planning + wave + reconcile) as a fallback.
function extractWallMs(report) {
  const top = num(
    report.elapsed_ms,
    report.wall_ms,
    report.duration_ms,
    report.total_duration_ms,
    report.wallMs,
  );
  if (top !== null) return top;
  const phases = report.phases;
  if (phases && typeof phases === "object") {
    const parts = [phases.planning_ms, phases.wave_ms, phases.reconcile_ms]
      .filter((v) => typeof v === "number" && Number.isFinite(v));
    if (parts.length > 0) return parts.reduce((a, b) => a + b, 0);
  }
  return null;
}

function extractInputTokens(report) {
  const usage = report.usage && typeof report.usage === "object" ? report.usage : {};
  const tokens = report.tokens && typeof report.tokens === "object" ? report.tokens : {};
  return num(
    report.total_input_tokens,
    report.input_tokens,
    tokens.input,
    usage.input_tokens,
    usage.prompt_tokens,
  );
}

function extractOutputTokens(report) {
  const usage = report.usage && typeof report.usage === "object" ? report.usage : {};
  const tokens = report.tokens && typeof report.tokens === "object" ? report.tokens : {};
  return num(
    report.total_output_tokens,
    report.output_tokens,
    tokens.output,
    usage.output_tokens,
    usage.completion_tokens,
  );
}

// Task counts by status. The engine serializes tasks as a JSON object
// (BTreeMap<id, TaskRecord>); older shapes may use an array. Anything
// without a string status counts as "unknown".
function extractTaskCounts(report) {
  const tasks = report.tasks;
  if (tasks === null || tasks === undefined) return null;
  const list = Array.isArray(tasks) ? tasks : Object.values(tasks);
  const counts = {};
  for (const task of list) {
    const status = task && typeof task.status === "string" ? task.status : "unknown";
    counts[status] = (counts[status] ?? 0) + 1;
  }
  return counts;
}

// Scan the run dir top level for known pack JSON twins; record each
// one's top-level `ok`. A twin that does not parse, or has no boolean
// `ok`, records null (present but unreadable is not a pass).
function extractGates(runDir) {
  const gates = {};
  let names;
  try {
    names = readdirSync(runDir);
  } catch {
    return gates;
  }
  for (const [file, gate] of Object.entries(GATE_TWINS)) {
    if (!names.includes(file)) continue;
    const parsed = readJsonSafe(join(runDir, file));
    gates[gate] = parsed && typeof parsed === "object" && typeof parsed.ok === "boolean"
      ? parsed.ok
      : null;
  }
  return gates;
}

// Build the row for one run dir. Throws when report.json is missing or
// unparseable; the caller maps that to exit 2.
export function extractRow(runDir, arm, tags) {
  const reportPath = join(runDir, "report.json");
  if (!existsSync(reportPath)) {
    throw new Error(`no report.json in ${runDir}`);
  }
  const report = readJsonSafe(reportPath);
  if (!report || typeof report !== "object" || Array.isArray(report)) {
    throw new Error(`unreadable report.json in ${runDir}`);
  }
  return {
    ts: new Date().toISOString(),
    arm,
    run_dir: runDir,
    wall_ms: extractWallMs(report),
    input_tokens: extractInputTokens(report),
    output_tokens: extractOutputTokens(report),
    tasks: extractTaskCounts(report),
    gates: extractGates(runDir),
    tags: tags && typeof tags === "object" ? tags : {},
  };
}
