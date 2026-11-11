// lint.mjs — zero-token lint gate over a source tree.
//
// The engine drafts code in waves; a model-drafted review can claim
// mechanical findings that are not real. This gate proves them: it shells
// out to the language's own tools (cargo clippy, falling back to cargo
// check, and node --check) and reports exactly what they say. No model
// calls. Zero tokens.
//
//   node lint.mjs <dir> [--out DIR]
//
// Contract (see sidecars/README.md):
//   - exit 0: no ERROR checks
//   - exit 1: at least one ERROR check
//   - exit 2: usage error (no dir, missing/unreadable dir)
// Output: one line per check (OK / WARN SKIP / ERROR), a summary line,
// and with --out a report.md plus a review.json twin (stable fields,
// append-only).
import { spawnSync } from "node:child_process";
import { existsSync, statSync, readdirSync, mkdirSync, writeFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";

const SKIP_DIRS = new Set(["node_modules", ".git", "target", "dist", "build"]);
const JS_EXT = new Set([".js", ".mjs", ".cjs"]);
const TOOL_TIMEOUT_MS = 300000; // 5 min
const MAX_FINDINGS = 20;

function* walk(dir) {
  let entries;
  try {
    entries = readdirSync(dir, { withFileTypes: true });
  } catch {
    return;
  }
  for (const entry of entries) {
    const p = join(dir, entry.name);
    if (entry.isDirectory()) {
      if (!SKIP_DIRS.has(entry.name)) yield* walk(p);
    } else if (entry.isFile()) {
      yield p;
    }
  }
}

function ext(file) {
  const dot = file.lastIndexOf(".");
  return dot < 0 ? "" : file.slice(dot).toLowerCase();
}

function runTool(command, args, options = {}) {
  try {
    const result = spawnSync(command, args, {
      stdio: "pipe",
      encoding: "utf8",
      timeout: TOOL_TIMEOUT_MS,
      ...options,
    });
    if (result.error) return { error: String(result.error.message || result.error) };
    if (result.status === null) return { error: `timed out after ${TOOL_TIMEOUT_MS / 1000}s` };
    return {
      code: result.status,
      text: `${result.stdout || ""}${result.stderr || ""}`,
    };
  } catch (error) {
    return { error: String(error.message || error) };
  }
}

// Pull the first `error[...]` / `error:` lines out of tool output, capped.
function extractErrors(text) {
  const lines = [];
  for (const line of (text || "").split(/\r?\n/)) {
    if (/^\s*error(\[|:)/.test(line)) {
      lines.push(line.trim());
      if (lines.length >= MAX_FINDINGS) break;
    }
  }
  if (lines.length === 0) {
    const first = (text || "").split(/\r?\n/).find((l) => l.trim());
    lines.push(first ? first.trim() : "tool failed with no output");
  }
  return lines;
}

function probe(command, args) {
  return runTool(command, args).code === 0;
}

function runCargoChecks(crateDirs) {
  const checks = [];
  if (!probe("cargo", ["--version"])) {
    checks.push({
      kind: "cargo",
      target: crateDirs.join(", "),
      status: "skip",
      detail: "cargo not on PATH",
    });
    return checks;
  }
  const useClippy = probe("cargo", ["clippy", "--version"]);
  const tool = useClippy ? "clippy" : "check";
  for (const dir of crateDirs) {
    const result = runTool("cargo", [tool, "--quiet"], { cwd: dir });
    if (result.error !== undefined) {
      checks.push({ kind: `cargo-${tool}`, target: dir, status: "error", detail: result.error });
    } else if (result.code === 0) {
      checks.push({ kind: `cargo-${tool}`, target: dir, status: "ok" });
    } else {
      checks.push({
        kind: `cargo-${tool}`,
        target: dir,
        status: "error",
        detail: extractErrors(result.text).join(" | "),
        findings: extractErrors(result.text),
      });
    }
  }
  return checks;
}

function runNodeChecks(jsFiles) {
  const checks = [];
  for (const file of jsFiles) {
    const result = runTool(process.execPath, ["--check", file]);
    if (result.error !== undefined) {
      checks.push({ kind: "node-check", target: file, status: "error", detail: result.error });
    } else if (result.code === 0) {
      checks.push({ kind: "node-check", target: file, status: "ok" });
    } else {
      const errors = extractErrors(result.text);
      checks.push({ kind: "node-check", target: file, status: "error", detail: errors.join(" | "), findings: errors });
    }
  }
  return checks;
}

function writeReport(outDir, checks, ok, summary) {
  mkdirSync(outDir, { recursive: true });
  const lines = [];
  lines.push("# slimtoken-review lint report");
  lines.push("");
  lines.push(`Result: ${ok ? "PASS" : "FAIL"}`);
  lines.push("");
  lines.push("| Suite | Target | Status | Detail |");
  lines.push("|---|---|---|---|");
  for (const c of checks) {
    lines.push(`| ${c.kind} | ${c.target} | ${c.status} | ${(c.detail || "").replace(/\|/g, "\\|")} |`);
  }
  lines.push("");
  const failed = checks.filter((c) => c.status === "error");
  if (failed.length > 0) {
    lines.push("## Findings (exact tool text)");
    lines.push("");
    for (const c of failed) {
      lines.push(`### ${c.kind} — ${c.target}`);
      lines.push("");
      lines.push("```");
      for (const f of c.findings || [c.detail]) lines.push(f);
      lines.push("```");
      lines.push("");
    }
  } else {
    lines.push("## Findings");
    lines.push("");
    lines.push("None.");
    lines.push("");
  }
  writeFileSync(join(outDir, "report.md"), lines.join("\n"), "utf8");
  const review = {
    ok,
    checks: checks.map((c) => {
      const row = { kind: c.kind, target: c.target, status: c.status };
      if (c.detail !== undefined) row.detail = c.detail;
      return row;
    }),
    summary,
  };
  writeFileSync(join(outDir, "review.json"), JSON.stringify(review, null, 2) + "\n", "utf8");
}

export function runLint(args) {
  const rest = [...args];
  let outDir = null;
  const outIdx = rest.indexOf("--out");
  if (outIdx >= 0) {
    if (outIdx + 1 >= rest.length) {
      console.error("lint: --out needs a directory (usage: slimtoken-review lint <dir> [--out DIR])");
      return 2;
    }
    outDir = rest[outIdx + 1];
    rest.splice(outIdx, 2);
  }
  const dir = rest[0];
  if (!dir) {
    console.error("lint: no directory (usage: slimtoken-review lint <dir> [--out DIR])");
    return 2;
  }
  let stat = null;
  try {
    stat = statSync(dir);
  } catch {
    console.error(`lint: directory not found or unreadable: ${dir}`);
    return 2;
  }
  if (!stat.isDirectory()) {
    console.error(`lint: not a directory: ${dir}`);
    return 2;
  }

  const files = [...walk(resolve(dir))];
  const crateDirs = [...new Set(
    files.filter((f) => f.endsWith("Cargo.toml")).map((f) => dirname(f))
  )];
  const jsFiles = files.filter((f) => JS_EXT.has(ext(f)));

  let checks = [];
  if (crateDirs.length === 0 && jsFiles.length === 0) {
    checks.push({ kind: "lint", target: dir, status: "skip", detail: "no checkable files" });
  } else {
    if (crateDirs.length > 0) checks.push(...runCargoChecks(crateDirs));
    if (jsFiles.length > 0) checks.push(...runNodeChecks(jsFiles));
  }

  let errors = 0;
  let skips = 0;
  for (const c of checks) {
    if (c.status === "ok") {
      console.log(`OK ${c.kind} ${c.target}`);
    } else if (c.status === "skip") {
      console.log(`WARN SKIP ${c.kind} ${c.target}: ${c.detail}`);
      skips += 1;
    } else {
      console.log(`ERROR ${c.kind} ${c.target}: ${c.detail}`);
      errors += 1;
    }
  }
  const ok = errors === 0;
  const summary = `lint: ${checks.length} check(s), ${errors} error(s), ${skips} skipped`;
  console.log(summary);

  if (outDir) writeReport(outDir, checks, ok, summary);
  return ok ? 0 : 1;
}
