// check.mjs — the migration gate for the SlimToken migrate pack.
//
// A migration wave rewrites many files at once. The gate proves the wave
// left every file compiling, for free (no model tokens): the pack shells
// out to the language's own compiler / parser, so the verdict is exactly
// as strict as the toolchain, never a re-implementation.
//
// Contract (see sidecars/README.md):
//   - exit 0: no ERROR line (WARN SKIP is not a failure)
//   - exit 1: at least one ERROR
//   - exit 2: usage error (no dir, missing/unreadable dir, bad flag)
//   - zero tokens: only python / node / cargo / npm are spawned
//
// Checks:
//   1. SYNTAX (always): every .py under <dir> -> python -m py_compile,
//      every .js/.mjs/.cjs -> node --check. Any failure is an ERROR with
//      the tool's first error line.
//   2. MANIFEST (when detected at the top level of <dir>): Cargo.toml ->
//      cargo check --quiet (300s timeout; WARN SKIP if cargo absent);
//      package.json -> npm run build --if-present with --build (600s
//      timeout), else WARN SKIP "build skipped (pass --build)". Non-zero
//      exit is an ERROR with the first error lines (cap 20).
//   3. --out DIR also writes report.md (human) and migrate.json (machine,
//      {ok, checks: [{kind, target, status, detail?}], summary}).
import { spawnSync } from "node:child_process";
import {
  existsSync,
  mkdirSync,
  readdirSync,
  statSync,
  writeFileSync,
} from "node:fs";
import { isAbsolute, join, relative, resolve } from "node:path";

const JS_EXTS = new Set([".js", ".mjs", ".cjs"]);
const SKIP_DIRS = new Set([
  "node_modules",
  ".git",
  "target",
  "__pycache__",
  "dist",
  "build",
]);
const CARGO_TIMEOUT_MS = 300_000;
const NPM_TIMEOUT_MS = 600_000;
const SYNTAX_TIMEOUT_MS = 120_000;

export function runCheck(args) {
  const parsed = parseArgs(args);
  if (parsed.error) {
    console.error(`check: ${parsed.error} (usage: slimtoken-migrate check <dir> [--build] [--out DIR])`);
    return 2;
  }
  const dir = resolve(parsed.dir);
  if (!existsSync(dir) || !statSync(dir).isDirectory()) {
    console.error(`check: directory not found or unreadable: ${parsed.dir}`);
    return 2;
  }

  const checks = [];
  runSyntaxChecks(dir, checks);
  runCargoCheck(dir, checks);
  runNpmBuild(dir, parsed.build, checks);

  const summary = summarize(checks);
  printLines(dir, checks, summary);

  if (parsed.out) {
    writeReports(dir, parsed.out, checks, summary);
  }
  return summary.errors > 0 ? 1 : 0;
}

// --- args -----------------------------------------------------------------

function parseArgs(args) {
  const parsed = { dir: null, build: false, out: null };
  for (let i = 0; i < args.length; i += 1) {
    const arg = args[i];
    if (arg === "--build") {
      parsed.build = true;
    } else if (arg === "--out") {
      parsed.out = args[i + 1];
      i += 1;
      if (!parsed.out) return { error: "--out needs a directory" };
    } else if (arg.startsWith("--")) {
      return { error: `unknown flag '${arg}'` };
    } else if (parsed.dir === null) {
      parsed.dir = arg;
    } else {
      return { error: `unexpected argument '${arg}'` };
    }
  }
  if (parsed.dir === null) return { error: "no directory given" };
  return parsed;
}

// --- syntax ---------------------------------------------------------------

function* walk(dir, base) {
  for (const name of readdirSync(dir).sort()) {
    if (SKIP_DIRS.has(name)) continue;
    const abs = join(dir, name);
    const stat = statSync(abs);
    if (stat.isDirectory()) {
      yield* walk(abs, base);
    } else if (stat.isFile()) {
      yield relative(base, abs);
    }
  }
}

function ext(file) {
  const dot = file.lastIndexOf(".");
  return dot < 0 ? "" : file.slice(dot).toLowerCase();
}

function runSyntaxChecks(dir, checks) {
  let files;
  try {
    files = [...walk(dir, dir)].filter((f) => {
      const e = ext(f);
      return e === ".py" || JS_EXTS.has(e);
    });
  } catch (error) {
    checks.push({
      kind: "syntax",
      target: ".",
      status: "error",
      detail: `directory unreadable: ${error.message || error}`,
    });
    return;
  }
  if (files.length === 0) {
    checks.push({ kind: "syntax", target: ".", status: "ok", detail: "no source files found" });
    return;
  }
  for (const file of files) {
    const e = ext(file);
    if (e === ".py") checks.push(checkPython(dir, file));
    else checks.push(checkJavaScript(dir, file));
  }
}

let pythonProbe = null;
function findPython() {
  if (pythonProbe !== null) return pythonProbe;
  for (const candidate of ["python", "py"]) {
    const result = spawnSync(candidate, ["-c", "pass"], {
      timeout: 15_000,
      stdio: "ignore",
    });
    if (!result.error && result.status === 0) {
      pythonProbe = candidate;
      return pythonProbe;
    }
  }
  pythonProbe = false;
  return pythonProbe;
}

function checkPython(dir, file) {
  const abs = join(dir, file);
  const python = findPython();
  if (!python) {
    return {
      kind: "syntax",
      target: file,
      status: "warn-skip",
      detail: "python not found on PATH (tried python, py)",
    };
  }
  const result = spawnSync(python, ["-m", "py_compile", abs], {
    timeout: SYNTAX_TIMEOUT_MS,
    encoding: "utf8",
    env: { ...process.env, PYTHONDONTWRITEBYTECODE: "1" },
  });
  if (timedOut(result)) {
    return { kind: "syntax", target: file, status: "error", detail: `py_compile timed out after ${SYNTAX_TIMEOUT_MS / 1000}s` };
  }
  if (result.status === 0) {
    return { kind: "syntax", target: file, status: "ok" };
  }
  const line = firstLines(result.stderr || result.stdout, 1);
  return { kind: "syntax", target: file, status: "error", detail: line || "py_compile failed" };
}

function checkJavaScript(dir, file) {
  const abs = join(dir, file);
  const result = spawnSync(process.execPath, ["--check", abs], {
    timeout: SYNTAX_TIMEOUT_MS,
    encoding: "utf8",
  });
  if (timedOut(result)) {
    return { kind: "syntax", target: file, status: "error", detail: `node --check timed out after ${SYNTAX_TIMEOUT_MS / 1000}s` };
  }
  if (result.status === 0) {
    return { kind: "syntax", target: file, status: "ok" };
  }
  const line = firstLines(result.stderr || result.stdout, 1);
  return { kind: "syntax", target: file, status: "error", detail: line || "node --check failed" };
}

// --- manifests ------------------------------------------------------------

function runCargoCheck(dir, checks) {
  if (!existsSync(join(dir, "Cargo.toml"))) return;
  const cargo = findTool("cargo");
  if (!cargo) {
    checks.push({
      kind: "manifest",
      target: "Cargo.toml",
      status: "warn-skip",
      detail: "cargo not found on PATH",
    });
    return;
  }
  const result = spawnSync(cargo, ["check", "--quiet"], {
    cwd: dir,
    timeout: CARGO_TIMEOUT_MS,
    encoding: "utf8",
  });
  if (timedOut(result)) {
    checks.push({ kind: "manifest", target: "Cargo.toml", status: "error", detail: `cargo check timed out after ${CARGO_TIMEOUT_MS / 1000}s` });
    return;
  }
  if (result.status === 0) {
    checks.push({ kind: "manifest", target: "Cargo.toml", status: "ok", detail: "cargo check --quiet" });
    return;
  }
  checks.push({
    kind: "manifest",
    target: "Cargo.toml",
    status: "error",
    detail: firstLines(result.stderr || result.stdout, 20) || "cargo check failed",
  });
}

function runNpmBuild(dir, build, checks) {
  if (!existsSync(join(dir, "package.json"))) return;
  if (!build) {
    checks.push({
      kind: "manifest",
      target: "package.json",
      status: "warn-skip",
      detail: "build skipped (pass --build)",
    });
    return;
  }
  const npm = findTool("npm");
  if (!npm) {
    checks.push({
      kind: "manifest",
      target: "package.json",
      status: "warn-skip",
      detail: "npm not found on PATH",
    });
    return;
  }
  const result = spawnSync(npm, ["run", "build", "--if-present"], {
    cwd: dir,
    timeout: NPM_TIMEOUT_MS,
    encoding: "utf8",
    // npm is a .cmd shim on Windows; CreateProcess cannot run it without a
    // shell. Non-Windows shells are harmless, so keep this conditional.
    shell: process.platform === "win32",
  });
  if (timedOut(result)) {
    checks.push({ kind: "manifest", target: "package.json", status: "error", detail: `npm run build timed out after ${NPM_TIMEOUT_MS / 1000}s` });
    return;
  }
  if (result.status === 0) {
    checks.push({ kind: "manifest", target: "package.json", status: "ok", detail: "npm run build --if-present" });
    return;
  }
  checks.push({
    kind: "manifest",
    target: "package.json",
    status: "error",
    detail: firstLines(result.stderr || result.stdout, 20) || "npm run build failed",
  });
}

// --- shared helpers -------------------------------------------------------

const toolProbe = new Map();
function findTool(name) {
  if (!toolProbe.has(name)) {
    const result = spawnSync(name, ["--version"], { timeout: 15_000, stdio: "ignore" });
    toolProbe.set(name, !result.error && result.status === 0);
  }
  return toolProbe.get(name) ? name : null;
}

function timedOut(result) {
  return result.error && result.error.code === "ETIMEDOUT";
}

function firstLines(text, cap) {
  if (!text) return "";
  return text
    .split(/\r?\n/)
    .map((l) => l.trim())
    .filter((l) => l.length > 0)
    .slice(0, cap)
    .join(" | ");
}

// --- output ---------------------------------------------------------------

function summarize(checks) {
  const summary = {
    total: checks.length,
    ok: 0,
    warnSkip: 0,
    errors: 0,
  };
  for (const c of checks) {
    if (c.status === "ok") summary.ok += 1;
    else if (c.status === "warn-skip") summary.warnSkip += 1;
    else summary.errors += 1;
  }
  return summary;
}

function statusLabel(status) {
  if (status === "warn-skip") return "WARN SKIP";
  return status.toUpperCase();
}

function printLines(dir, checks, summary) {
  for (const c of checks) {
    const detail = c.detail ? ` — ${c.detail}` : "";
    console.log(`${statusLabel(c.status).padEnd(9)} ${c.kind.padEnd(8)} ${c.target}${detail}`);
  }
  console.log(
    `check: ${summary.ok}/${summary.total} ok, ${summary.warnSkip} skipped, ${summary.errors} errors — ${summary.errors > 0 ? "FAIL" : "PASS"} (${dir})`,
  );
}

function writeReports(dir, outDir, checks, summary) {
  const out = isAbsolute(outDir) ? outDir : resolve(outDir);
  mkdirSync(out, { recursive: true });
  const ok = summary.errors === 0;

  const report = [
    "# slimtoken-migrate report",
    "",
    `dir: ${dir}`,
    `verdict: ${ok ? "PASS" : "FAIL"}`,
    "",
    `checks: ${summary.total} (ok ${summary.ok}, warn-skip ${summary.warnSkip}, error ${summary.errors})`,
    "",
    "## Checks",
    "",
    ...checks.map((c) => {
      const detail = c.detail ? ` — ${c.detail}` : "";
      return `- ${statusLabel(c.status)} ${c.kind} ${c.target}${detail}`;
    }),
    "",
  ].join("\n");
  writeFileSync(join(out, "report.md"), report, "utf8");

  const json = { ok, checks, summary };
  writeFileSync(join(out, "migrate.json"), `${JSON.stringify(json, null, 2)}\n`, "utf8");
  console.log(`check: wrote ${join(out, "report.md")} and ${join(out, "migrate.json")}`);
}
