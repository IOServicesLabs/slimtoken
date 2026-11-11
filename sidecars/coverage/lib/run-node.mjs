// run-node.mjs — node:test + V8 coverage for the measure gate.
//
// Runs `node --test` with cwd: <dir> and NODE_V8_COVERAGE pointed at a temp
// dir, then merges the raw V8 coverage JSON the runner writes. The stdout
// summary table (`--coverage`) only carries percentages; the raw JSON also
// carries the range counts, so per-file covered/total line counts can be
// computed without extra dependencies.
//
// Line rule (statement-approximate): a source line counts when it is not
// blank and not a comment line, and some V8 range intersects it. It is
// covered when every range that intersects it ran at least once (min count
// over intersecting ranges is > 0). Nested ranges make never-called
// functions keep count 0 even though the script-level range around them
// ran, so dead lines stay uncovered.
import { spawnSync } from "node:child_process";
import { existsSync, mkdtempSync, readdirSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { basename, extname, join, relative, resolve, sep } from "node:path";
import { fileURLToPath } from "node:url";
import {
  TIMEOUT_MS,
  baseName,
  combinedOutput,
  lastNonEmptyLine,
  pctOf,
  walkFiles,
} from "./util.mjs";

const TEST_FILE = /\.test\.[mc]?js$/;
const SOURCE_EXT = new Set([".js", ".mjs", ".cjs"]);

export function detectNodeSuite(dir) {
  const pkg = join(dir, "package.json");
  if (existsSync(pkg)) {
    try {
      const scripts = JSON.parse(readFileSync(pkg, "utf8")).scripts;
      if (scripts && typeof scripts.test === "string" && scripts.test.trim() !== "") return true;
    } catch {
      // unreadable package.json: fall through to the file scan
    }
  }
  for (const f of walkFiles(dir)) {
    if (TEST_FILE.test(baseName(f))) return true;
  }
  return false;
}

function lineStarts(text) {
  const starts = [0];
  for (let i = 0; i < text.length; i += 1) {
    if (text[i] === "\n") starts.push(i + 1);
  }
  return starts;
}

function lineOf(starts, offset) {
  let lo = 0;
  let hi = starts.length - 1;
  let ans = 0;
  while (lo <= hi) {
    const mid = (lo + hi) >> 1;
    if (starts[mid] <= offset) {
      ans = mid;
      lo = mid + 1;
    } else {
      hi = mid - 1;
    }
  }
  return ans;
}

// A line can hold a statement: not blank, not a comment-only line.
function countableLine(snippet) {
  const t = snippet.trim();
  return t !== "" && !t.startsWith("//") && !t.startsWith("/*") && !t.startsWith("*");
}

function computeNodeCoverage(tmpDir, rootDir) {
  const entries = [];
  for (const name of readdirSync(tmpDir)) {
    if (!/^coverage-.*\.json$/.test(name)) continue;
    try {
      const data = JSON.parse(readFileSync(join(tmpDir, name), "utf8"));
      // Node wraps the payload as {"result": [...]}; older builds emit a
      // bare array. Accept both.
      const scripts = Array.isArray(data) ? data : Array.isArray(data?.result) ? data.result : [];
      entries.push(...scripts);
    } catch {
      // partial file from a killed process: ignore
    }
  }
  const root = resolve(rootDir);
  const perFile = new Map(); // path -> { exec: Set<line>, covered: Set<line> }
  for (const entry of entries) {
    if (!entry || typeof entry.url !== "string" || !entry.url.startsWith("file://")) continue;
    let file;
    try {
      file = fileURLToPath(entry.url);
    } catch {
      continue;
    }
    const abs = resolve(file);
    if (abs !== root && !abs.startsWith(root + sep)) continue;
    if (abs.includes(sep + "node_modules" + sep)) continue;
    if (!SOURCE_EXT.has(extname(abs).toLowerCase())) continue;
    if (TEST_FILE.test(basename(abs))) continue;
    let text;
    try {
      text = readFileSync(abs, "utf8");
    } catch {
      continue;
    }
    const starts = lineStarts(text);
    let agg = perFile.get(abs);
    if (!agg) {
      agg = { exec: new Set(), covered: new Set() };
      perFile.set(abs, agg);
    }
    // Per process, a line is covered only when EVERY range that intersects
    // it ran (min count > 0): the script-level range wraps the whole file
    // with count 1, so an OR rule would mark never-called functions
    // covered. Across processes the covered sets merge with OR (a line
    // covered by any test process counts as covered).
    const lineMin = new Map();
    for (const fn of entry.functions || []) {
      for (const r of fn.ranges || []) {
        const count = r.count | 0;
        const s = Math.max(0, r.startOffset | 0);
        const e = Math.min(text.length, Math.max(s, r.endOffset | 0));
        const firstLine = lineOf(starts, s);
        const lastLine = lineOf(starts, Math.max(s, e - 1));
        for (let line = firstLine; line <= lastLine; line += 1) {
          const lineStart = starts[line];
          const lineEnd = line + 1 < starts.length ? starts[line + 1] : text.length;
          if (lineEnd <= s || lineStart >= e) continue;
          if (!countableLine(text.slice(lineStart, lineEnd))) continue;
          lineMin.set(line, Math.min(lineMin.get(line) ?? count, count));
        }
      }
    }
    for (const [line, min] of lineMin) {
      agg.exec.add(line);
      if (min > 0) agg.covered.add(line);
    }
  }
  const files = [];
  for (const [path, agg] of perFile) {
    const total = agg.exec.size;
    const covered = agg.covered.size;
    files.push({
      file: relative(root, path).split(sep).join("/"),
      covered,
      total,
      pct: pctOf(covered, total),
    });
  }
  files.sort((a, b) => (a.file < b.file ? -1 : a.file > b.file ? 1 : 0));
  return files;
}

// Returns { files, warn } on success, { error } when no coverage data came
// out of the runner. `warn` carries the last runner line when the suite
// itself failed (coverage is then partial but still real).
export function runNodeCoverage(dir) {
  const tmp = mkdtempSync(join(tmpdir(), "slimtoken-coverage-node-"));
  try {
    const res = spawnSync(process.execPath, ["--test"], {
      cwd: dir,
      env: { ...process.env, NODE_V8_COVERAGE: tmp },
      stdio: "pipe",
      timeout: TIMEOUT_MS,
    });
    if (res.error && res.error.code === "ETIMEDOUT") {
      return { error: `node --test timed out after ${TIMEOUT_MS / 1000}s` };
    }
    const files = computeNodeCoverage(tmp, dir);
    if (files.length === 0) {
      return { error: lastNonEmptyLine(combinedOutput(res)) || "no coverage data produced" };
    }
    const warn = res.status === 0 ? null : lastNonEmptyLine(combinedOutput(res));
    return { files, warn };
  } finally {
    rmSync(tmp, { recursive: true, force: true });
  }
}
