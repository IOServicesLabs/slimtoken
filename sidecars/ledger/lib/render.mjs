// render.mjs — console tables and the --out judge report for compare.
import { mkdirSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { compareArms, delta } from "./compare.mjs";

function str(v) {
  if (v === null || v === undefined) return "-";
  return String(v);
}

function fmtDelta(d, unit = "") {
  if (!d) return "-";
  const sign = d.diff > 0 ? "+" : "";
  const pct = d.pct !== null && d.pct !== undefined ? ` (${sign}${d.pct.toFixed(1)}%)` : "";
  return `${sign}${d.diff}${unit}${pct}`;
}

function pad(value, width) {
  const s = str(value);
  return s.length >= width ? s : s + " ".repeat(width - s.length);
}

// Compare table: one row per metric, columns A / B / delta (B - A).
export function renderCompare(rowA, rowB, armA, armB, verdict) {
  const gateNames = [...new Set([
    ...Object.keys(rowA.gates || {}),
    ...Object.keys(rowB.gates || {}),
  ])];
  const lines = [];
  lines.push(`ledger compare: ${armA} -> ${armB}`);
  lines.push("");
  lines.push(`${pad("metric", 14)}${pad(armA, 20)}${pad(armB, 20)}delta (B - A)`);
  lines.push(`${"-".repeat(14)}${"-".repeat(20)}${"-".repeat(20)}${"-".repeat(16)}`);
  lines.push(`${pad("wall_ms", 14)}${pad(str(rowA.wall_ms), 20)}${pad(str(rowB.wall_ms), 20)}${fmtDelta(delta(rowA.wall_ms, rowB.wall_ms), " ms")}`);
  lines.push(`${pad("input_tokens", 14)}${pad(str(rowA.input_tokens), 20)}${pad(str(rowB.input_tokens), 20)}${fmtDelta(delta(rowA.input_tokens, rowB.input_tokens))}`);
  lines.push(`${pad("output_tokens", 14)}${pad(str(rowA.output_tokens), 20)}${pad(str(rowB.output_tokens), 20)}${fmtDelta(delta(rowA.output_tokens, rowB.output_tokens))}`);
  for (const gate of gateNames) {
    const a = rowA.gates ? rowA.gates[gate] : undefined;
    const b = rowB.gates ? rowB.gates[gate] : undefined;
    const mark = a === true && b !== true ? "  REGRESSED" : "";
    lines.push(`${pad(`gate:${gate}`, 14)}${pad(str(a), 20)}${pad(str(b), 20)}${mark}`);
  }
  lines.push("");
  lines.push(`verdict: ${verdict.ok ? "MATCHED OR IMPROVED" : "REGRESSED"} — exit ${verdict.ok ? 0 : 1}`);
  for (const r of verdict.regressions) {
    lines.push(`  regression: ${r.detail}`);
  }
  return lines.join("\n") + "\n";
}

// Show table: one line per row (or per arm when filtered).
export function renderShow(rows) {
  const lines = [];
  lines.push(`${pad("ts", 24)}${pad("arm", 12)}${pad("wall_ms", 10)}${pad("in", 10)}${pad("out", 10)}gates`);
  lines.push(`${"-".repeat(24)}${"-".repeat(12)}${"-".repeat(10)}${"-".repeat(10)}${"-".repeat(10)}${"-".repeat(20)}`);
  for (const row of rows) {
    const gates = Object.entries(row.gates || {})
      .map(([name, ok]) => `${name}=${ok === true ? "ok" : ok === false ? "FAIL" : "?"}`)
      .join(",") || "-";
    lines.push(
      `${pad(str(row.ts), 24)}${pad(str(row.arm), 12)}${pad(str(row.wall_ms), 10)}${pad(str(row.input_tokens), 10)}${pad(str(row.output_tokens), 10)}${gates}`
    );
  }
  lines.push("");
  lines.push(`ledger show: ${rows.length} row${rows.length === 1 ? "" : "s"}`);
  return lines.join("\n") + "\n";
}

// --out for compare: report.md (judge report) + ledger.json (machine
// twin). Field names are stable; new fields append at the end.
export function writeCompareOut(outDir, rowA, rowB, armA, armB) {
  const verdict = compareArms(rowA, rowB);
  mkdirSync(outDir, { recursive: true });

  const md = [];
  md.push("# slimtoken-ledger compare report", "");
  md.push(`Arms: \`${armA}\` (A) vs \`${armB}\` (B). Delta is B - A.`);
  md.push(`Verdict: ${verdict.ok ? "MATCHED OR IMPROVED" : "REGRESSED"}`);
  md.push("");
  md.push("| Metric | A | B | Delta |", "|---|---|---|---|");
  md.push(`| wall_ms | ${str(rowA.wall_ms)} | ${str(rowB.wall_ms)} | ${fmtDelta(delta(rowA.wall_ms, rowB.wall_ms), " ms")} |`);
  md.push(`| input_tokens | ${str(rowA.input_tokens)} | ${str(rowB.input_tokens)} | ${fmtDelta(delta(rowA.input_tokens, rowB.input_tokens))} |`);
  md.push(`| output_tokens | ${str(rowA.output_tokens)} | ${str(rowB.output_tokens)} | ${fmtDelta(delta(rowA.output_tokens, rowB.output_tokens))} |`);
  const gateNames = [...new Set([
    ...Object.keys(rowA.gates || {}),
    ...Object.keys(rowB.gates || {}),
  ])];
  for (const gate of gateNames) {
    const a = rowA.gates ? rowA.gates[gate] : undefined;
    const b = rowB.gates ? rowB.gates[gate] : undefined;
    md.push(`| gate:${gate} | ${str(a)} | ${str(b)} | ${a === true && b !== true ? "REGRESSED" : ""} |`);
  }
  if (verdict.regressions.length > 0) {
    md.push("", "## Regressions", "");
    for (const r of verdict.regressions) md.push(`- ${r.detail}`);
  }
  md.push("");
  writeFileSync(join(outDir, "report.md"), md.join("\n"), "utf8");

  const twin = {
    ok: verdict.ok,
    arm_a: armA,
    arm_b: armB,
    a: {
      ts: rowA.ts ?? null,
      run_dir: rowA.run_dir ?? null,
      wall_ms: rowA.wall_ms ?? null,
      input_tokens: rowA.input_tokens ?? null,
      output_tokens: rowA.output_tokens ?? null,
      tasks: rowA.tasks ?? null,
      gates: rowA.gates ?? {},
      tags: rowA.tags ?? {},
    },
    b: {
      ts: rowB.ts ?? null,
      run_dir: rowB.run_dir ?? null,
      wall_ms: rowB.wall_ms ?? null,
      input_tokens: rowB.input_tokens ?? null,
      output_tokens: rowB.output_tokens ?? null,
      tasks: rowB.tasks ?? null,
      gates: rowB.gates ?? {},
      tags: rowB.tags ?? {},
    },
    deltas: {
      wall_ms: delta(rowA.wall_ms, rowB.wall_ms),
      input_tokens: delta(rowA.input_tokens, rowB.input_tokens),
      output_tokens: delta(rowA.output_tokens, rowB.output_tokens),
    },
    regressions: verdict.regressions,
  };
  writeFileSync(join(outDir, "ledger.json"), JSON.stringify(twin, null, 2) + "\n", "utf8");
}
