// Report collector for the SlimToken engine's research pack.
//
// `collect` reads a run output dir and merges the per-task briefs the
// engine wrote into one report.md with a "## Sources" section (every
// citation found, deduped by URL). Briefs come from two places:
//   - report.json in the run dir: each succeeded task's `output` field
//   - any top-level *.md file in the run dir (excluding report.md itself)
//
// Contract (see sidecars/README.md):
//   - exit 0: at least one brief was found and the report was written
//   - exit 2: usage error (missing --out, missing dir, no briefs)
import { readFileSync, readdirSync, writeFileSync, existsSync, statSync } from "node:fs";
import { join, extname } from "node:path";

const CITATION = /\[([^\]]+)\]\((https?:\/\/[^)\s]+)\)/g;

export async function runCollect(args) {
  const parsed = parseArgs(args);
  if (parsed.error) {
    console.error(`collect: ${parsed.error}`);
    return 2;
  }
  const { runDir, outFile } = parsed;

  if (!existsSync(runDir) || !statSync(runDir).isDirectory()) {
    console.error(`collect: run output dir '${runDir}' is missing or not a directory`);
    return 2;
  }

  const briefs = readBriefs(runDir);
  if (briefs.length === 0) {
    console.error(`collect: no task briefs found in '${runDir}' (need report.json task outputs or *.md files)`);
    return 2;
  }

  const report = buildReport(briefs);
  try {
    writeFileSync(outFile, report, "utf8");
  } catch (error) {
    console.error(`collect: cannot write report '${outFile}': ${error.message}`);
    return 2;
  }
  console.log(`collect: ${briefs.length} brief(s) -> ${outFile}`);
  return 0;
}

function readBriefs(runDir) {
  const briefs = [];

  // Engine seam: report.json holds a tasks map of id -> { status, output }.
  const reportJson = join(runDir, "report.json");
  if (existsSync(reportJson)) {
    try {
      const doc = JSON.parse(readFileSync(reportJson, "utf8"));
      const tasks = doc && typeof doc === "object" ? doc.tasks : null;
      if (tasks && typeof tasks === "object") {
        for (const id of Object.keys(tasks).sort()) {
          const task = tasks[id];
          if (!task || typeof task !== "object") continue;
          if (task.status !== undefined && task.status !== "succeeded") continue;
          const output = typeof task.output === "string" ? task.output.trim() : "";
          if (output.length > 0) briefs.push({ id: String(task.id || id), text: output });
        }
      }
    } catch {
      // A broken report.json is not fatal here: loose *.md briefs may exist.
    }
  }

  // Loose briefs: top-level markdown files the run (or a host) dropped in.
  for (const name of readdirSync(runDir).sort()) {
    if (extname(name).toLowerCase() !== ".md") continue;
    if (name.toLowerCase() === "report.md") continue;
    const full = join(runDir, name);
    if (!statSync(full).isFile()) continue;
    try {
      const text = readFileSync(full, "utf8").trim();
      if (text.length > 0) briefs.push({ id: name.replace(/\.md$/i, ""), text });
    } catch {
      // Unreadable brief: skip, the verify gate fails closed on missing cites.
    }
  }

  return briefs;
}

function buildReport(briefs) {
  const lines = ["# Research report", ""];
  const seenUrls = new Set();
  const sources = [];

  for (const brief of briefs) {
    lines.push(`## ${brief.id}`, "", brief.text, "");
    CITATION.lastIndex = 0;
    let match;
    while ((match = CITATION.exec(brief.text)) !== null) {
      const url = match[2];
      if (!seenUrls.has(url)) {
        seenUrls.add(url);
        sources.push(url);
      }
    }
  }

  lines.push("## Sources", "");
  if (sources.length === 0) {
    lines.push("(no citations found)", "");
  } else {
    for (const url of sources) lines.push(`- ${url}`);
    lines.push("");
  }
  return lines.join("\n");
}

function parseArgs(args) {
  let runDir = null;
  let outFile = null;
  for (let i = 0; i < args.length; i += 1) {
    const arg = args[i];
    if (arg === "--out") {
      if (i + 1 >= args.length) return { error: "--out needs a file path" };
      outFile = args[i + 1];
      i += 1;
    } else if (arg.startsWith("--out=")) {
      outFile = arg.slice("--out=".length);
    } else if (arg.startsWith("--")) {
      return { error: `unknown flag '${arg}'` };
    } else if (runDir === null) {
      runDir = arg;
    } else {
      return { error: `unexpected extra argument '${arg}'` };
    }
  }
  if (runDir === null) {
    return { error: "no run output dir (usage: slimtoken-research collect <run-output-dir> --out report.md)" };
  }
  if (outFile === null || outFile.length === 0) {
    return { error: "missing --out (usage: slimtoken-research collect <run-output-dir> --out report.md)" };
  }
  return { runDir, outFile };
}
