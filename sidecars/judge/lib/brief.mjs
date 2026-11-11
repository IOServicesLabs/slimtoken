// Verdict brief builder for slimtoken-judge.
//
// `brief` scans the given directories for known pack JSON twins
// (test.json, review.json, migrate.json, docs.json, contract.json,
// coverage.json, research.json, loop.json, ledger.json, replay.json,
// trace.json, security.json, flake.json, fuzz.json, i18n.json,
// types.json — matched by filename) and merges each found twin into one
// compact markdown verdict brief. Zero model tokens; file reads only.
//
// Budget cap: --max-input-bytes N (default 20000). If the merged brief
// exceeds N, per-pack sections drop oldest-first and a WARN line prints.
// The whole verdict is never silently dropped: at least the newest pack
// section always stays, and a truncation note heads the brief.
//
// Contract (see sidecars/README.md):
//   - exit 0: at least one twin found and brief.md written
//   - exit 2: usage error (no dirs, bad dir, zero twins found)
import { readdirSync, readFileSync, statSync, writeFileSync, mkdirSync, existsSync } from "node:fs";
import { join, resolve } from "node:path";

export const TWIN_NAMES = new Set([
  "test", "review", "migrate", "docs", "contract", "coverage",
  "research", "loop", "ledger", "replay", "trace", "security",
  "flake", "fuzz", "i18n", "types",
]);

const DEFAULT_MAX_BYTES = 20000;
const MAX_EXCERPT_LINES = 5;
const EXCERPT_CHAR_CAP = 160;
// Arrays in a twin that carry findings/errors/checks (first 5 become excerpts).
const DETAIL_KEYS = ["findings", "errors", "failures", "violations", "checks", "cases", "suites", "diffs"];

export async function runBrief(args) {
  const parsed = parseArgs(args);
  if (parsed.error) {
    console.error(`brief: ${parsed.error}`);
    return 2;
  }
  const { dirs, outDir, maxBytes } = parsed;

  const sections = [];
  let twins = 0;
  for (const dir of dirs) {
    let entries;
    try {
      if (!statSync(dir).isDirectory()) {
        console.error(`brief: '${dir}' is not a directory`);
        return 2;
      }
      entries = readdirSync(dir);
    } catch (error) {
      console.error(`brief: cannot read '${dir}': ${error.message}`);
      return 2;
    }
    for (const entry of entries) {
      if (!entry.toLowerCase().endsWith(".json")) continue;
      const pack = entry.slice(0, -".json".length).toLowerCase();
      if (!TWIN_NAMES.has(pack)) continue;
      twins += 1;
      const file = join(dir, entry);
      sections.push(buildSection(pack, file));
    }
  }

  if (twins === 0) {
    console.error("brief: no JSON twins found in the given directories (nothing to judge)");
    return 2;
  }

  let truncated = false;
  // Oldest-first: drop the earliest sections until the brief fits the cap.
  while (
    sections.length > 1 &&
    Buffer.byteLength(renderBrief(sections, truncated), "utf8") > maxBytes
  ) {
    const dropped = sections.shift();
    console.log(`WARN brief: input exceeds --max-input-bytes ${maxBytes}; dropped oldest pack section '${dropped.pack}'`);
    truncated = true;
  }
  let brief = renderBrief(sections, truncated);
  if (Buffer.byteLength(brief, "utf8") > maxBytes) {
    // Even one section is over the cap: hard-slice the text, keep the note.
    console.log(`WARN brief: input exceeds --max-input-bytes ${maxBytes}; hard-truncated the remaining section`);
    truncated = true;
    const note = truncationNote(sections, truncated);
    const room = Math.max(0, maxBytes - Buffer.byteLength(note, "utf8"));
    brief = note + Buffer.from(brief, "utf8").subarray(0, room).toString("utf8");
  }

  const abs = resolve(outDir || ".");
  mkdirSync(abs, { recursive: true });
  writeFileSync(join(abs, "brief.md"), brief, "utf8");
  console.log(`brief: ${sections.length} pack section(s), ${Buffer.byteLength(brief, "utf8")} bytes -> ${join(abs, "brief.md")}`);
  return 0;
}

function parseArgs(args) {
  const dirs = [];
  let outDir = null;
  let maxBytes = DEFAULT_MAX_BYTES;
  for (let i = 0; i < args.length; i += 1) {
    const a = args[i];
    if (a === "--out") {
      if (i + 1 >= args.length) return { error: "--out needs a directory" };
      outDir = args[i + 1];
      i += 1;
    } else if (a.startsWith("--out=")) {
      outDir = a.slice("--out=".length);
    } else if (a === "--max-input-bytes") {
      if (i + 1 >= args.length) return { error: "--max-input-bytes needs a number" };
      const n = Number(args[i + 1]);
      if (!Number.isInteger(n) || n <= 0) return { error: `--max-input-bytes must be a positive integer, got '${args[i + 1]}'` };
      maxBytes = n;
      i += 1;
    } else if (a.startsWith("--max-input-bytes=")) {
      const n = Number(a.slice("--max-input-bytes=".length));
      if (!Number.isInteger(n) || n <= 0) return { error: `--max-input-bytes must be a positive integer, got '${a}'` };
      maxBytes = n;
    } else if (a.startsWith("--")) {
      return { error: `unknown flag '${a}'` };
    } else {
      dirs.push(a);
    }
  }
  if (dirs.length === 0) return { error: "no directories given (usage: slimtoken-judge brief <dirs...> [--out DIR])" };
  return { dirs, outDir, maxBytes };
}

function buildSection(pack, file) {
  let data;
  try {
    data = JSON.parse(readFileSync(file, "utf8"));
  } catch (error) {
    // A stale or unreadable twin must not vanish: it lands in the brief as
    // a failed section so the judge sees it.
    return {
      pack,
      lines: [
        `## ${pack}`,
        "ok: false",
        `summary: twin '${file}' is not parseable JSON: ${error.message}`,
      ],
    };
  }
  if (data === null || typeof data !== "object" || Array.isArray(data)) {
    return {
      pack,
      lines: [`## ${pack}`, "ok: false", `summary: twin '${file}' is not a JSON object`],
    };
  }

  const lines = [`## ${pack}`, `ok: ${data.ok === true ? "true" : "false"}`];
  const summary = typeof data.summary === "string" ? data.summary.trim() : "";
  if (summary) lines.push(`summary: ${oneLine(summary)}`);

  // Scalar count fields (cases, passed, failed, errors, ...) in stable order.
  const counts = Object.keys(data)
    .filter((k) => typeof data[k] === "number" && Number.isInteger(data[k]))
    .slice(0, 6)
    .map((k) => `${k}=${data[k]}`);
  if (counts.length > 0) lines.push(`counts: ${counts.join(" ")}`);

  // First detail array: its length is the finding count, its first 5
  // entries are the error excerpts.
  for (const key of DETAIL_KEYS) {
    if (!Array.isArray(data[key])) continue;
    lines.push(`${key}: ${data[key].length}`);
    for (const item of data[key].slice(0, MAX_EXCERPT_LINES)) {
      lines.push(`- ${excerpt(item)}`);
    }
    break;
  }
  return { pack, lines };
}

function excerpt(item) {
  let text;
  if (typeof item === "string") text = item;
  else {
    // Prefer a message/error/name field when the item is an object.
    if (item && typeof item === "object") {
      const pick = item.error ?? item.message ?? item.name ?? item.summary;
      if (typeof pick === "string") {
        text = pick;
      } else {
        try {
          text = JSON.stringify(item);
        } catch {
          text = String(item);
        }
      }
    } else {
      text = String(item);
    }
  }
  text = oneLine(text);
  return text.length > EXCERPT_CHAR_CAP ? text.slice(0, EXCERPT_CHAR_CAP) + "…" : text;
}

function oneLine(text) {
  return text.replace(/\s+/g, " ").trim();
}

function truncationNote(sections, truncated) {
  if (!truncated) return "";
  const kept = sections.map((s) => s.pack).join(", ");
  return `> WARN: brief truncated to fit --max-input-bytes; kept newest section(s): ${kept}\n\n`;
}

function renderBrief(sections, truncated) {
  const body = sections.map((s) => s.lines.join("\n")).join("\n\n");
  return `# slimtoken-judge verdict brief\n\n${truncationNote(sections, truncated)}${body}\n`;
}
