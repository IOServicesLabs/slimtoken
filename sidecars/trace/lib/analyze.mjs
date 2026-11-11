// Analyzer for the slimtoken-trace pack.
//
// Streams the engine's trace.jsonl line by line (never whole-file
// JSON.parse) and turns scheduler anomalies into findings. The event
// names and fields parsed here are the stable trace vocabulary from
// EMBEDDING.md ("Trace vocabulary"); new fields may appear, existing
// ones are not renamed or removed within a major version.
//
// Contract (see sidecars/README.md):
//   - exit 0: no findings
//   - exit 1: at least one finding
//   - exit 2: usage error (bad args, missing input)
//   - zero tokens: pure parse, never a model
import { createReadStream } from "node:fs";
import { createInterface } from "node:readline";
import { statSync } from "node:fs";
import { writeReports } from "./report.mjs";

// Finding thresholds. retry-storm K is host-tunable on the CLI.
const REPAIR_QUEUE_DEPTH_MIN = 3;
const RATE_CHURN_MIN = 5;
const CACHE_HIT_MIN = 0.2;
const COLLAPSE_PEAK_MAX = 2;
const COLLAPSE_TASKS_MIN = 10;

export async function runAnalyze(args) {
  const parsed = parseArgs(args);
  if (parsed.error) {
    console.error(`analyze: ${parsed.error}`);
    return 2;
  }
  const { tracePath, retryStormK, outDir } = parsed;
  let st;
  try {
    st = statSync(tracePath);
  } catch {
    console.error(`analyze: cannot read trace file '${tracePath}'`);
    return 2;
  }
  if (!st.isFile()) {
    console.error(`analyze: '${tracePath}' is not a file`);
    return 2;
  }

  const result = await analyzeTrace(tracePath, { retryStormK });

  for (const finding of result.findings) {
    console.log(`FINDING ${finding.rule}: ${finding.detail}`);
  }
  if (result.stats.warn_lines > 0) {
    console.log(`WARN ${result.stats.warn_lines} corrupt line(s) skipped`);
  }
  console.log(
    `analyze: ${result.findings.length} finding(s), ` +
      `${result.stats.events} event(s), ` +
      `${result.stats.completions.succeeded}/${result.stats.completions.failed}/${result.stats.completions.skipped} ` +
      `succeeded/failed/skipped` +
      (result.stats.cache_ratio !== null
        ? `, cache hit ${(result.stats.cache_ratio * 100).toFixed(1)}%`
        : "")
  );

  if (outDir) {
    try {
      writeReports(outDir, result);
      console.log(`analyze: wrote report.md and trace.json to ${outDir}`);
    } catch (error) {
      console.error(`analyze: cannot write --out dir: ${error.message}`);
      return 2;
    }
  }

  return result.findings.length > 0 ? 1 : 0;
}

function parseArgs(args) {
  let tracePath = null;
  let retryStormK = 3;
  let outDir = null;
  for (let i = 0; i < args.length; i += 1) {
    const arg = args[i];
    if (arg === "--retry-storm") {
      const value = args[i + 1];
      const k = Number(value);
      if (value === undefined || !Number.isInteger(k) || k < 1) {
        return { error: "--retry-storm needs an integer K >= 1" };
      }
      retryStormK = k;
      i += 1;
    } else if (arg.startsWith("--retry-storm=")) {
      const k = Number(arg.slice("--retry-storm=".length));
      if (!Number.isInteger(k) || k < 1) {
        return { error: "--retry-storm needs an integer K >= 1" };
      }
      retryStormK = k;
    } else if (arg === "--out") {
      const value = args[i + 1];
      if (value === undefined || value.startsWith("--")) {
        return { error: "--out needs a directory" };
      }
      outDir = value;
      i += 1;
    } else if (arg.startsWith("--")) {
      return { error: `unknown flag '${arg}'` };
    } else if (tracePath === null) {
      tracePath = arg;
    } else {
      return { error: `unexpected extra argument '${arg}'` };
    }
  }
  if (tracePath === null) {
    return { error: "no trace file (usage: slimtoken-trace analyze <trace.jsonl> [--retry-storm K] [--out DIR])" };
  }
  return { tracePath, retryStormK, outDir };
}

export async function analyzeTrace(tracePath, { retryStormK = 3 } = {}) {
  const stats = {
    events: 0,
    warn_lines: 0,
    by_event: {},
    retries: {},
    repairs: { queued: 0, max_queue_depth: 0, worst_line: 0 },
    rate_tune_count: 0,
    completions: { succeeded: 0, failed: 0, skipped: 0 },
    tokens: { input: 0, output: 0, cached_input: 0, cached_seen: false },
    cache_ratio: null,
    concurrency_probe: null,
    run_end: null
  };

  await streamLines(tracePath, (line, lineNo) => {
    const trimmed = line.trim();
    if (trimmed.length === 0) return;
    let record;
    try {
      record = JSON.parse(trimmed);
    } catch {
      stats.warn_lines += 1;
      console.log(`WARN line ${lineNo}: invalid JSON, skipped`);
      return;
    }
    stats.events += 1;
    const event = record.event;
    const fields = record.fields || {};
    if (typeof event === "string") {
      stats.by_event[event] = (stats.by_event[event] || 0) + 1;
    }

    switch (event) {
      case "task_retry": {
        const taskId = String(fields.task_id ?? "unknown");
        stats.retries[taskId] = (stats.retries[taskId] || 0) + 1;
        break;
      }
      case "repair_queued": {
        stats.repairs.queued += 1;
        const depth = Number(fields.queue_depth);
        if (Number.isFinite(depth) && depth > stats.repairs.max_queue_depth) {
          stats.repairs.max_queue_depth = depth;
          stats.repairs.worst_line = lineNo;
        }
        break;
      }
      case "rate_tune": {
        stats.rate_tune_count += 1;
        break;
      }
      case "task_complete": {
        const status = fields.status;
        if (status === "succeeded" || status === "failed" || status === "skipped") {
          stats.completions[status] += 1;
        }
        const input = Number(fields.input_tokens);
        const output = Number(fields.output_tokens);
        const cached = Number(fields.cached_input_tokens);
        if (Number.isFinite(input)) stats.tokens.input += input;
        if (Number.isFinite(output)) stats.tokens.output += output;
        if (Number.isFinite(cached)) {
          stats.tokens.cached_input += cached;
          stats.tokens.cached_seen = true;
        }
        break;
      }
      case "concurrency_probe": {
        const width = Number(fields.chosen_width);
        stats.concurrency_probe = {
          chosen_width: Number.isFinite(width) ? width : null,
          line: lineNo
        };
        break;
      }
      case "run_end": {
        stats.run_end = {
          succeeded: Number(fields.succeeded) || 0,
          failed: Number(fields.failed) || 0,
          skipped: Number(fields.skipped) || 0,
          peak_concurrency: Number(fields.peak_concurrency) || 0,
          line: lineNo
        };
        break;
      }
      default:
        // run_start, task_start, planning_complete, priming_complete,
        // repair_start, and any future event: counted in by_event only.
        break;
    }
  });

  if (stats.tokens.input > 0 && stats.tokens.cached_seen) {
    stats.cache_ratio = stats.tokens.cached_input / stats.tokens.input;
  }

  const findings = evaluate(stats, retryStormK);
  return { ok: findings.length === 0, findings, stats };
}

function evaluate(stats, retryStormK) {
  const findings = [];

  // 1. retry-storm: one task retried at least K times.
  for (const [taskId, count] of Object.entries(stats.retries)) {
    if (count >= retryStormK) {
      findings.push({
        rule: "retry-storm",
        detail: `retry-storm ${taskId}: ${count} retries (>= ${retryStormK})`,
        evidence: { task_id: taskId, retries: count, threshold: retryStormK }
      });
    }
  }

  // 2. repair-queue-depth: a repair queued at depth >= 3.
  if (stats.repairs.max_queue_depth >= REPAIR_QUEUE_DEPTH_MIN) {
    findings.push({
      rule: "repair-queue-depth",
      detail: `repair-queue-depth ${stats.repairs.max_queue_depth} (>= ${REPAIR_QUEUE_DEPTH_MIN})`,
      evidence: {
        queue_depth: stats.repairs.max_queue_depth,
        line: stats.repairs.worst_line
      }
    });
  }

  // 3. rate-churn: the auto-tuner changed pace >= 5 times in one run.
  if (stats.rate_tune_count >= RATE_CHURN_MIN) {
    findings.push({
      rule: "rate-churn",
      detail: `rate-churn ${stats.rate_tune_count} rate_tune events (>= ${RATE_CHURN_MIN})`,
      evidence: { rate_tune_events: stats.rate_tune_count, threshold: RATE_CHURN_MIN }
    });
  }

  // 4. low-cache-hit: cached/input below 0.2 (only when input > 0 and
  //    any cached field was seen).
  if (stats.cache_ratio !== null && stats.cache_ratio < CACHE_HIT_MIN) {
    findings.push({
      rule: "low-cache-hit",
      detail:
        `low-cache-hit ${(stats.cache_ratio * 100).toFixed(1)}% ` +
        `cached/input (< ${CACHE_HIT_MIN * 100}%)`,
      evidence: {
        input_tokens: stats.tokens.input,
        cached_input_tokens: stats.tokens.cached_input,
        ratio: Number(stats.cache_ratio.toFixed(4))
      }
    });
  }

  // 5. concurrency-collapse: peak <= 2 with a real wave and no probe.
  if (stats.run_end) {
    const total = stats.run_end.succeeded + stats.run_end.failed + stats.run_end.skipped;
    if (
      stats.run_end.peak_concurrency <= COLLAPSE_PEAK_MAX &&
      total >= COLLAPSE_TASKS_MIN &&
      stats.concurrency_probe === null
    ) {
      findings.push({
        rule: "concurrency-collapse",
        detail:
          `concurrency-collapse peak ${stats.run_end.peak_concurrency} ` +
          `with ${total} tasks and no concurrency_probe`,
        evidence: {
          peak_concurrency: stats.run_end.peak_concurrency,
          tasks: total,
          line: stats.run_end.line
        }
      });
    }

    // 6. failures-present: the run drained with failed tasks.
    if (stats.run_end.failed > 0) {
      findings.push({
        rule: "failures-present",
        detail: `failures-present run_end failed=${stats.run_end.failed}`,
        evidence: { failed: stats.run_end.failed, line: stats.run_end.line }
      });
    }
  }

  return findings;
}

async function streamLines(filePath, onLine) {
  const stream = createReadStream(filePath, { encoding: "utf8" });
  const rl = createInterface({ input: stream, crlfDelay: Infinity });
  let lineNo = 0;
  for await (const line of rl) {
    lineNo += 1;
    onLine(line, lineNo);
  }
}
