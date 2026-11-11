// delta.mjs — the slimtoken-coverage delta gate.
//
//   slimtoken-coverage delta <before.json> <after.json> [--min-gain N] [--out DIR]
//
// Contract (see sidecars/README.md):
//   - exit 0: after.total.pct >= before.total.pct + N (N defaults to 0)
//   - exit 1: the gain is below N; the per-file movers are printed
//   - exit 2: usage error (bad flag, missing args, unreadable input file)
//   - a stale or malformed artifact fails closed (exit 1), never passes
//   - zero tokens: pure file comparison, never call a model
import { existsSync, readFileSync } from "node:fs";
import { writeOut } from "./out.mjs";
import { round2 } from "./util.mjs";

function usageError(message) {
  console.error(`delta: ${message}`);
  console.error("usage: slimtoken-coverage delta <before.json> <after.json> [--min-gain N] [--out DIR]");
  return 2;
}

function readArtifact(path) {
  if (!existsSync(path)) return { missing: path };
  let data;
  try {
    data = JSON.parse(readFileSync(path, "utf8"));
  } catch (error) {
    return { malformed: `${path}: ${error.message || error}` };
  }
  const pct = data && data.total && typeof data.total.pct === "number" ? data.total.pct : null;
  if (pct === null) return { malformed: `${path}: missing numeric total.pct` };
  const files = new Map();
  for (const f of (data.files || [])) {
    if (f && typeof f.file === "string" && typeof f.pct === "number") files.set(f.file, f.pct);
  }
  return { pct, files };
}

function computeMovers(before, after) {
  const names = new Set([...before.files.keys(), ...after.files.keys()]);
  const movers = [];
  for (const name of names) {
    const b = before.files.has(name) ? before.files.get(name) : null;
    const a = after.files.has(name) ? after.files.get(name) : null;
    const delta = round2((a ?? 0) - (b ?? 0));
    if (delta !== 0 || b === null || a === null) {
      movers.push({ file: name, before: b, after: a, delta });
    }
  }
  movers.sort((x, y) => (x.delta < y.delta ? -1 : x.delta > y.delta ? 1 : x.file < y.file ? -1 : 1));
  return movers;
}

function reportMarkdown(beforePath, afterPath, before, after, gain, minGain, movers, ok) {
  const lines = [
    "# slimtoken-coverage delta report",
    "",
    `Before: \`${beforePath}\` (${before.pct}%)`,
    `After: \`${afterPath}\` (${after.pct}%)`,
    `Gain: ${gain >= 0 ? "+" : ""}${gain} pp (min-gain ${minGain})`,
    `Verdict: ${ok ? "PASS" : "FAIL"}`,
    "",
    "## Per-file movers",
    "",
    "| File | Before | After | Delta |",
    "|---|---|---|---|",
  ];
  for (const m of movers) {
    const fmt = (v) => (v === null ? "—" : `${v}%`);
    lines.push(`| ${m.file} | ${fmt(m.before)} | ${fmt(m.after)} | ${m.delta >= 0 ? "+" : ""}${m.delta} pp |`);
  }
  if (movers.length === 0) lines.push("| (no per-file movement) |  |  |  |");
  lines.push("");
  return lines.join("\n");
}

export function runDelta(args) {
  const positionals = [];
  const flags = {};
  for (let i = 0; i < args.length; i += 1) {
    const a = args[i];
    if (a.startsWith("--")) {
      const name = a.slice(2);
      if (name !== "min-gain" && name !== "out") return usageError(`unknown flag '${a}'`);
      if (i + 1 >= args.length) return usageError(`flag '${a}' needs a value`);
      flags[name] = args[i + 1];
      i += 1;
    } else {
      positionals.push(a);
    }
  }
  if (positionals.length !== 2) return usageError("pass exactly two coverage.json files");

  const minGain = flags["min-gain"] === undefined ? 0 : Number(flags["min-gain"]);
  if (!Number.isFinite(minGain)) return usageError(`--min-gain must be a number (got '${flags["min-gain"]}')`);

  const before = readArtifact(positionals[0]);
  if (before.missing) return usageError(`'${before.missing}' is not readable`);
  if (before.malformed) {
    console.error(`delta: ERROR stale or malformed artifact: ${before.malformed}`);
    return 1;
  }
  const after = readArtifact(positionals[1]);
  if (after.missing) return usageError(`'${after.missing}' is not readable`);
  if (after.malformed) {
    console.error(`delta: ERROR stale or malformed artifact: ${after.malformed}`);
    return 1;
  }

  const gain = round2(after.pct - before.pct);
  const ok = gain >= minGain;
  const movers = computeMovers(before, after);

  if (!ok) {
    for (const m of movers) {
      const fmt = (v) => (v === null ? "—" : `${v}%`);
      console.log(`MOVER ${m.file}: ${fmt(m.before)} -> ${fmt(m.after)} (${m.delta >= 0 ? "+" : ""}${m.delta} pp)`);
    }
  }
  console.log(`delta: ${gain >= 0 ? "+" : ""}${gain} pp (min-gain ${minGain}) -> ${ok ? "PASS" : "FAIL"}`);

  if (flags.out) {
    const twin = { ok, before: { pct: before.pct }, after: { pct: after.pct }, gain, minGain, movers };
    writeOut(
      flags.out,
      "delta.json",
      reportMarkdown(positionals[0], positionals[1], before, after, gain, minGain, movers, ok),
      twin
    );
  }

  return ok ? 0 : 1;
}
