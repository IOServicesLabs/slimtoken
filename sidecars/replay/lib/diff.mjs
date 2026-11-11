// The paired per-task diff: two engine run output dirs, one plan.
// Pure file parsing, zero tokens. Exit codes (see sidecars/README.md):
//   0 = zero findings, 1 = findings, 2 = usage / missing dirs / unreadable reports.

import { existsSync, statSync, mkdirSync, writeFileSync } from "node:fs";
import { join, resolve } from "node:path";
import { readPlan, readReport, countRetries } from "./readers.mjs";

// Status ordering: succeeded > failed > skipped. Any downward move is a finding.
const STATUS_RANK = { succeeded: 2, failed: 1, skipped: 0 };
const DEFAULT_TOKENS_TOLERANCE = 10; // percent
const DEFAULT_DURATION_TOLERANCE = 25; // percent

function pctIncrease(a, b) {
  if (a === null || b === null) return null;
  if (a === 0) return b > 0 ? Infinity : 0;
  return ((b - a) / a) * 100;
}

// Compare one paired task. Returns an array of finding strings (may be empty).
function compareTask(id, recA, recB, retriesA, retriesB, opts) {
  const findings = [];
  if (!recA) {
    findings.push("missing-in-a: task present only in run B");
    return findings;
  }
  if (!recB) {
    findings.push("missing-in-b: task present only in run A");
    return findings;
  }
  const rankA = STATUS_RANK[recA.status] ?? null;
  const rankB = STATUS_RANK[recB.status] ?? null;
  if (rankA !== null && rankB !== null && rankB < rankA) {
    findings.push(`status regressed: ${recA.status} -> ${recB.status}`);
  }
  if (recA.attempts !== null && recB.attempts !== null && recB.attempts - recA.attempts > 1) {
    findings.push(`attempts +${recB.attempts - recA.attempts} (${recA.attempts} -> ${recB.attempts})`);
  }
  const durationPct = pctIncrease(recA.durationMs, recB.durationMs);
  if (durationPct !== null && durationPct > opts.durationTolerance) {
    findings.push(`duration +${durationPct.toFixed(1)}% (${recA.durationMs}ms -> ${recB.durationMs}ms)`);
  }
  const tokenPct = pctIncrease(recA.outputTokens, recB.outputTokens);
  if (tokenPct !== null && tokenPct > opts.tokensTolerance) {
    const label = tokenPct === Infinity ? "inf" : tokenPct.toFixed(1);
    findings.push(`output tokens +${label}% (${recA.outputTokens} -> ${recB.outputTokens})`);
  }
  const deltaRetries = (retriesB.get(id) || 0) - (retriesA.get(id) || 0);
  if (deltaRetries >= 3) {
    findings.push(`retry storm: ${retriesA.get(id) || 0} -> ${retriesB.get(id) || 0} task_retry events (+${deltaRetries})`);
  }
  return findings;
}

function renderReportMd(armA, armB, rows, summary) {
  const lines = [
    "# slimtoken-replay diff",
    "",
    `Arms: A = ${armA}, B = ${armB}`,
    "",
    "| Task | Status A | Status B | Findings |",
    "|---|---|---|---|"
  ];
  for (const row of rows) {
    const findings = row.findings.length > 0 ? row.findings.join("; ") : "none";
    lines.push(`| ${row.id} | ${row.status_a ?? "-"} | ${row.status_b ?? "-"} | ${findings} |`);
  }
  lines.push("", `Tasks compared: ${summary.tasks_compared}`, `Findings: ${summary.findings}`);
  for (const [rule, count] of Object.entries(summary.by_rule)) {
    lines.push(`- ${rule}: ${count}`);
  }
  lines.push("");
  return lines.join("\n");
}

export async function runDiff(args) {
  const positional = [];
  let tokensTolerance = DEFAULT_TOKENS_TOLERANCE;
  let outDir = null;
  for (let i = 0; i < args.length; i += 1) {
    const arg = args[i];
    if (arg === "--tokens-tolerance") {
      const raw = args[++i];
      const value = Number(raw);
      if (!Number.isFinite(value) || raw === undefined) {
        console.error("diff: --tokens-tolerance needs a number (usage: slimtoken-replay diff <run-a-dir> <run-b-dir> [--tokens-tolerance N] [--out DIR])");
        return 2;
      }
      tokensTolerance = value;
    } else if (arg.startsWith("--tokens-tolerance=")) {
      const value = Number(arg.slice("--tokens-tolerance=".length));
      if (!Number.isFinite(value)) {
        console.error("diff: --tokens-tolerance needs a number");
        return 2;
      }
      tokensTolerance = value;
    } else if (arg === "--out") {
      outDir = args[++i];
      if (outDir === undefined) {
        console.error("diff: --out needs a directory");
        return 2;
      }
    } else if (arg.startsWith("--out=")) {
      outDir = arg.slice("--out=".length);
    } else if (arg.startsWith("--")) {
      console.error(`diff: unknown flag ${arg}`);
      return 2;
    } else {
      positional.push(arg);
    }
  }
  if (positional.length !== 2) {
    console.error("usage: slimtoken-replay diff <run-a-dir> <run-b-dir> [--tokens-tolerance N] [--out DIR]");
    return 2;
  }
  const [dirA, dirB] = positional.map((d) => resolve(d));
  for (const dir of [dirA, dirB]) {
    if (!existsSync(dir) || !statSync(dir).isDirectory()) {
      console.error(`diff: not a directory: ${dir}`);
      return 2;
    }
  }

  const plan = readPlan(dirA);
  if (!plan.ok) {
    console.error(`diff: ${plan.error}`);
    return 2;
  }
  const reportA = readReport(dirA);
  if (!reportA.ok) {
    console.error(`diff: ${reportA.error}`);
    return 2;
  }
  const reportB = readReport(dirB);
  if (!reportB.ok) {
    console.error(`diff: ${reportB.error}`);
    return 2;
  }
  const traceA = await countRetries(join(dirA, "trace.jsonl"));
  if (!traceA.ok) {
    console.error(`diff: ${traceA.error}`);
    return 2;
  }
  const traceB = await countRetries(join(dirB, "trace.jsonl"));
  if (!traceB.ok) {
    console.error(`diff: ${traceB.error}`);
    return 2;
  }

  const opts = { tokensTolerance, durationTolerance: DEFAULT_DURATION_TOLERANCE };
  const ids = [...new Set([...plan.ids, ...reportA.tasks.keys(), ...reportB.tasks.keys()])].sort();
  const rows = [];
  const byRule = {};
  let findingsTotal = 0;
  for (const id of ids) {
    const recA = reportA.tasks.get(id) || null;
    const recB = reportB.tasks.get(id) || null;
    const findings = compareTask(id, recA, recB, traceA.retries, traceB.retries, opts);
    for (const f of findings) {
      const rule = f.split(":")[0];
      byRule[rule] = (byRule[rule] || 0) + 1;
    }
    findingsTotal += findings.length;
    rows.push({ id, status_a: recA?.status ?? null, status_b: recB?.status ?? null, findings });
    if (findings.length === 0) {
      console.log(`OK ${id}`);
    } else {
      for (const f of findings) console.log(`FINDING ${id}: ${f}`);
    }
  }
  const summary = { tasks_compared: rows.length, findings: findingsTotal, by_rule: byRule };
  console.log(`diff: ${rows.length} tasks compared, ${findingsTotal} findings`);
  const ok = findingsTotal === 0;
  console.log(ok ? "diff: OK" : "diff: FINDINGS");

  if (outDir) {
    const out = resolve(outDir);
    mkdirSync(out, { recursive: true });
    const replay = {
      ok,
      arms: [dirA, dirB],
      tasks: rows.map((r) => ({ id: r.id, status_a: r.status_a, status_b: r.status_b, findings: r.findings })),
      summary
    };
    writeFileSync(join(out, "report.md"), renderReportMd(dirA, dirB, rows, summary), "utf8");
    writeFileSync(join(out, "replay.json"), JSON.stringify(replay, null, 2) + "\n", "utf8");
    console.log(`diff: wrote ${join(out, "report.md")} and ${join(out, "replay.json")}`);
  }

  return ok ? 0 : 1;
}
