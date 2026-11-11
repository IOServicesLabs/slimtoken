// measure.mjs — the slimtoken-coverage measure gate.
//
//   slimtoken-coverage measure <dir> [--runner node|python] [--out DIR]
//
// Contract (see sidecars/README.md):
//   - exit 0: coverage ran for at least one stack (even at 0%); a selected
//     stack can instead be skipped, which is reported, never green
//   - exit 1: a runner was selected, was not skipped, but produced no data
//   - exit 2: usage error (bad flag, no dir, missing dir, no suite found)
//   - zero tokens: shell out to node / python, never call a model
//
// Output: one section per stack with a per-file `{file, covered, total,
// pct}` table and a stack total, then an overall line. With `--out DIR` the
// gate also writes `report.md` and `coverage.json`
// (`{ok, files, total: {covered, total, pct}}` — the machine twin; new
// fields append at the end).
import { existsSync, statSync } from "node:fs";
import { resolve } from "node:path";
import { detectNodeSuite, runNodeCoverage } from "./run-node.mjs";
import { detectPythonSuite, runPythonCoverage } from "./run-python.mjs";
import { writeOut } from "./out.mjs";
import { sumFiles } from "./util.mjs";

function usageError(message) {
  console.error(`measure: ${message}`);
  console.error("usage: slimtoken-coverage measure <dir> [--runner node|python] [--out DIR]");
  return 2;
}

function parseArgs(args) {
  const positionals = [];
  const flags = {};
  for (let i = 0; i < args.length; i += 1) {
    const a = args[i];
    if (a.startsWith("--")) {
      const name = a.slice(2);
      if (name !== "runner" && name !== "out") return { bad: `unknown flag '${a}'` };
      if (i + 1 >= args.length) return { bad: `flag '${a}' needs a value` };
      flags[name] = args[i + 1];
      i += 1;
    } else {
      positionals.push(a);
    }
  }
  return { positionals, flags };
}

function reportMarkdown(dir, results, allFiles, total, ok) {
  const lines = [
    "# slimtoken-coverage measure report",
    "",
    `Directory: \`${dir}\``,
    `Verdict: ${ok ? "MEASURED" : "NOT MEASURED"}`,
    "",
  ];
  for (const r of results) {
    lines.push(`## ${r.stack}`, "");
    if (r.skipped) {
      lines.push(`Skipped: ${r.skipped}`, "");
      continue;
    }
    if (r.error) {
      lines.push(`Error: ${r.error}`, "");
      continue;
    }
    lines.push("| File | Covered | Total | Pct |", "|---|---|---|---|");
    for (const f of r.files) {
      lines.push(`| ${f.file} | ${f.covered} | ${f.total} | ${f.pct}% |`);
    }
    lines.push(`| **total** | **${r.total.covered}** | **${r.total.total}** | **${r.total.pct}%** |`, "");
  }
  lines.push(
    "## Combined",
    "",
    `Files: ${allFiles.length}`,
    `Total: ${total.covered}/${total.total} lines (${total.pct}%)`,
    ""
  );
  return lines.join("\n");
}

export function runMeasure(args) {
  const parsed = parseArgs(args);
  if (parsed.bad) return usageError(parsed.bad);
  if (parsed.positionals.length !== 1) return usageError("pass exactly one directory");
  const dir = resolve(parsed.positionals[0]);
  if (!existsSync(dir) || !statSync(dir).isDirectory()) {
    return usageError(`'${parsed.positionals[0]}' is not a readable directory`);
  }
  const runnerOpt = parsed.flags.runner ?? "auto";
  if (runnerOpt !== "auto" && runnerOpt !== "node" && runnerOpt !== "python") {
    return usageError(`--runner must be node, python, or auto (got '${runnerOpt}')`);
  }

  const detected = { node: detectNodeSuite(dir), python: detectPythonSuite(dir) };
  const runners =
    runnerOpt === "auto" ? ["node", "python"].filter((r) => detected[r]) : [runnerOpt];
  if (runners.length === 0) {
    return usageError(`no ${runnerOpt} test suite found in '${parsed.positionals[0]}'`);
  }

  const results = [];
  let anyRan = false;
  let anyError = false;
  for (const runner of runners) {
    const out = runner === "node" ? runNodeCoverage(dir) : runPythonCoverage(dir);
    if (out.skipped) {
      console.log(`WARN SKIP ${runner} coverage: ${out.skipped}`);
      results.push({ stack: runner, skipped: out.skipped });
      continue;
    }
    if (out.error) {
      console.log(`ERROR ${runner} coverage: ${out.error}`);
      results.push({ stack: runner, error: out.error });
      anyError = true;
      continue;
    }
    anyRan = true;
    if (out.warn) {
      console.log(`WARN ${runner} suite had failures; coverage is partial: ${out.warn}`);
    }
    const total = sumFiles(out.files);
    console.log(`${runner} coverage:`);
    for (const f of out.files) {
      console.log(`  ${f.file}  ${f.covered}/${f.total}  ${f.pct.toFixed(2)}%`);
    }
    console.log(`${runner} total: ${total.covered}/${total.total} lines ${total.pct.toFixed(2)}%`);
    results.push({ stack: runner, files: out.files, total, warn: out.warn });
  }

  const allFiles = results.flatMap((r) => r.files ?? []);
  const total = sumFiles(allFiles);
  const ok = anyRan;
  console.log(`coverage: ${ok ? "measured" : "not measured"}`);

  if (parsed.flags.out) {
    const twin = {
      ok,
      files: allFiles,
      total,
      stacks: results.map((r) =>
        r.files ? { stack: r.stack, files: r.files.length, total: r.total } : { stack: r.stack, skipped: r.skipped ?? null, error: r.error ?? null }
      ),
    };
    writeOut(parsed.flags.out, "coverage.json", reportMarkdown(dir, results, allFiles, total, ok), twin);
  }

  if (ok) return 0;
  return anyError ? 1 : 0;
}
