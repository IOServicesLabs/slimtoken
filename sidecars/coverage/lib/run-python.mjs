// run-python.mjs — coverage.py + pytest / unittest for the measure gate.
//
// Probe order: `python`, then `py`. When no interpreter is on PATH the
// runner is skipped (the gate reports it, never fakes it). When the
// interpreter exists but the coverage module does not, the gate prints the
// exact line `WARN SKIP python coverage: coverage module not installed` and
// skips. The probe and the suite both run with cwd: <dir>; COVERAGE_FILE
// points at a temp file so the measured directory stays untouched.
import { spawnSync } from "node:child_process";
import { existsSync, mkdtempSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { basename, join } from "node:path";
import {
  TIMEOUT_MS,
  baseName,
  combinedOutput,
  lastNonEmptyLine,
  pctOf,
  walkFiles,
} from "./util.mjs";

const PY_CANDIDATES = ["python", "py"];

export function detectPythonSuite(dir) {
  for (const f of walkFiles(dir)) {
    const b = baseName(f);
    if (/^test_.*\.py$/.test(b) || /^.*_test\.py$/.test(b)) return true;
  }
  if (existsSync(join(dir, "pytest.ini"))) return true;
  if (existsSync(join(dir, "pyproject.toml"))) {
    try {
      if (/\[tool\.pytest/.test(readFileSync(join(dir, "pyproject.toml"), "utf8"))) return true;
    } catch {
      // unreadable pyproject.toml: not a pytest marker
    }
  }
  return false;
}

export function findPython() {
  for (const name of PY_CANDIDATES) {
    const probe = spawnSync(name, ["--version"], { stdio: "pipe", timeout: 30_000 });
    if (!probe.error && probe.status === 0) return name;
  }
  return null;
}

function imports(py, module) {
  const probe = spawnSync(py, ["-c", `import ${module}`], { stdio: "pipe", timeout: 30_000 });
  return !probe.error && probe.status === 0;
}

const PY_TEST_FILE = /^test_.*\.py$/;
const PY_TEST_FILE2 = /^.*_test\.py$/;

// Returns { files, warn } on success, { skipped } when python coverage
// cannot run here, { error } when it should have run but produced nothing.
export function runPythonCoverage(dir) {
  const py = findPython();
  if (!py) return { skipped: "no python interpreter on PATH" };
  if (!imports(py, "coverage")) return { skipped: "coverage module not installed" };

  const tmp = mkdtempSync(join(tmpdir(), "slimtoken-coverage-py-"));
  try {
    const env = { ...process.env, COVERAGE_FILE: join(tmp, ".coverage") };
    const runArgs = imports(py, "pytest")
      ? ["-m", "coverage", "run", "-m", "pytest", "-q"]
      : ["-m", "coverage", "run", "-m", "unittest", "discover", "-q"];
    const res = spawnSync(py, runArgs, { cwd: dir, env, stdio: "pipe", timeout: TIMEOUT_MS });
    if (res.error && res.error.code === "ETIMEDOUT") {
      return { error: `python suite timed out after ${TIMEOUT_MS / 1000}s` };
    }

    const jsonPath = join(tmp, "coverage.json");
    const jsonRes = spawnSync(py, ["-m", "coverage", "json", "-o", jsonPath], {
      cwd: dir,
      env,
      stdio: "pipe",
      timeout: 60_000,
    });
    if (!existsSync(jsonPath)) {
      const text = combinedOutput(res) + combinedOutput(jsonRes);
      return { error: lastNonEmptyLine(text) || "coverage json was not produced" };
    }

    let data;
    try {
      data = JSON.parse(readFileSync(jsonPath, "utf8"));
    } catch (error) {
      return { error: `coverage json unreadable: ${error.message || error}` };
    }

    const files = [];
    for (const [path, info] of Object.entries(data.files || {})) {
      const summary = (info && info.summary) || {};
      const total = summary.num_statements | 0;
      if (total === 0) continue;
      const base = basename(path);
      if (PY_TEST_FILE.test(base) || PY_TEST_FILE2.test(base)) continue;
      const covered = summary.covered_lines | 0;
      const pct =
        typeof summary.percent_covered === "number"
          ? Math.round(summary.percent_covered * 100) / 100
          : pctOf(covered, total);
      files.push({ file: path.replace(/\\/g, "/"), covered, total, pct });
    }
    files.sort((a, b) => (a.file < b.file ? -1 : a.file > b.file ? 1 : 0));
    if (files.length === 0) {
      return { error: lastNonEmptyLine(combinedOutput(res)) || "coverage ran but no source files matched" };
    }
    const warn = res.status === 0 ? null : lastNonEmptyLine(combinedOutput(res));
    return { files, warn };
  } finally {
    rmSync(tmp, { recursive: true, force: true });
  }
}
