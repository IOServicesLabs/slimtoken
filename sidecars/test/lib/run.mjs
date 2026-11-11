// run.mjs — the slimtoken-test gate.
//
// Contract (see sidecars/README.md):
//   - exit 0: every suite that ran passed (or no suite was detected)
//   - exit 1: at least one suite failed
//   - exit 2: usage error (no dir, or dir missing/unreadable)
//   - zero tokens: shell out to python / node / npm, never call a model
//
//   slimtoken-test run <dir> [--out DIR]
//
// Detection:
//   - Python: any test_*.py / *_test.py file, a pytest.ini, or a
//     pyproject.toml with a [tool.pytest...] section.
//   - Node: a package.json with a "test" script, or any *.test.js /
//     *.test.mjs file.
//   - Both present: both run. Neither: SKIP, exit 0 (reported, not green).
//
// Runners:
//   - Python: `python -m pytest <dir> -q` when pytest is importable,
//     else `python -m unittest discover -s <dir> -q` (stdlib fallback).
//   - Node: `npm test` when a test script exists, else `node --test <dir>`.
//   - Every runner gets a 300 s timeout.
//
// Output: one `OK <suite> (N passed)` or `ERROR <suite>: <line>` per
// suite, a summary line, and (with --out DIR) report.md plus test.json
// (the machine twin with stable field names; new fields append at end).
import { spawnSync } from "node:child_process";
import { existsSync, mkdirSync, readdirSync, readFileSync, statSync, writeFileSync } from "node:fs";
import { join, resolve } from "node:path";

const TIMEOUT_MS = 300_000;
const PY_CANDIDATES = ["python", "py"];

function* walkFiles(dir) {
  let names;
  try {
    names = readdirSync(dir);
  } catch {
    return;
  }
  for (const name of names) {
    if (name === "node_modules" || name.startsWith(".")) continue;
    const full = join(dir, name);
    let st;
    try {
      st = statSync(full);
    } catch {
      continue;
    }
    if (st.isDirectory()) yield* walkFiles(full);
    else yield full;
  }
}

function detectPython(dir, files) {
  for (const f of files) {
    const base = f.slice(f.lastIndexOf("/") + 1).slice(f.lastIndexOf("\\") + 1);
    if (/^test_.*\.py$/.test(base) || /^.*_test\.py$/.test(base)) return true;
  }
  if (existsSync(join(dir, "pytest.ini"))) return true;
  const pj = join(dir, "pyproject.toml");
  if (existsSync(pj)) {
    try {
      if (/\[\s*tool\.pytest[^\]]*\]/m.test(readFileSync(pj, "utf8"))) return true;
    } catch {
      /* unreadable pyproject: not a pytest marker */
    }
  }
  return false;
}

function nodeTestScript(dir) {
  const pj = join(dir, "package.json");
  if (!existsSync(pj)) return null;
  try {
    const pkg = JSON.parse(readFileSync(pj, "utf8"));
    if (pkg.scripts && typeof pkg.scripts.test === "string") return pkg.scripts.test;
  } catch {
    /* unreadable package.json: fall through to file detection */
  }
  return null;
}

function detectNode(dir, files) {
  if (nodeTestScript(dir) !== null) return true;
  return files.some((f) => /\.test\.(js|mjs)$/.test(f));
}

function findPython() {
  for (const name of PY_CANDIDATES) {
    const probe = spawnSync(name, ["--version"], { stdio: "pipe", timeout: 30_000 });
    if (!probe.error && probe.status === 0) return name;
  }
  return null;
}

function hasPytest(py) {
  const probe = spawnSync(py, ["-m", "pytest", "--version"], { stdio: "pipe", timeout: 30_000 });
  return !probe.error && probe.status === 0;
}

function combinedOutput(result) {
  return `${result.stdout?.toString() || ""}\n${result.stderr?.toString() || ""}`;
}

// Last line that names a failure. Falls back to the last non-empty line.
function lastFailingLine(text) {
  const lines = text
    .split(/\r?\n/)
    .map((l) => l.trim())
    .filter(Boolean);
  const hits = lines.filter((l) => /failed|error|assert|not ok|no such/i.test(l) && !/^\d+ passed/.test(l));
  return hits[hits.length - 1] || lines[lines.length - 1] || "unknown failure";
}

function parseCounts(text) {
  const passed = text.match(/(\d+) passed/);
  const failed = text.match(/(\d+) failed/);
  return {
    passed: passed ? Number(passed[1]) : 0,
    failed: failed ? Number(failed[1]) : 0,
  };
}

function parseTapCounts(text) {
  let passed = null;
  let failed = null;
  for (const raw of text.split(/\r?\n/)) {
    const line = raw.trim();
    let m = line.match(/^[ℹ#]?\s*pass\s+(\d+)\s*$/i);
    if (m) passed = Number(m[1]);
    m = line.match(/^[ℹ#]?\s*fail\s+(\d+)\s*$/i);
    if (m) failed = Number(m[1]);
  }
  if (failed === null) {
    const notOk = text.split(/\r?\n/).filter((l) => /^not ok\s+/i.test(l.trim())).length;
    failed = notOk;
  }
  return { passed: passed ?? 0, failed: failed ?? 0 };
}

function timedOut(result) {
  return result.error?.code === "ETIMEDOUT" || result.signal === "SIGTERM";
}

function runPythonSuite(dir) {
  const py = findPython();
  if (!py) {
    return { stack: "python", command: "python", passed: 0, failed: 1, error: "no python interpreter found on PATH" };
  }
  let command;
  let args;
  let label;
  if (hasPytest(py)) {
    command = `${py} -m pytest ${dir} -q`;
    args = ["-m", "pytest", dir, "-q"];
    label = "python (pytest)";
  } else {
    command = `${py} -m unittest discover -s ${dir} -q`;
    args = ["-m", "unittest", "discover", "-s", dir, "-q"];
    label = "python (unittest)";
  }
  const result = spawnSync(py, args, { stdio: "pipe", timeout: TIMEOUT_MS, cwd: dir });
  if (result.error && result.error.code !== "ETIMEDOUT") {
    return { stack: "python", command, passed: 0, failed: 1, error: String(result.error.message || result.error) };
  }
  const text = combinedOutput(result);
  if (timedOut(result)) {
    return { stack: "python", command, passed: 0, failed: 1, error: `timeout after ${TIMEOUT_MS / 1000}s` };
  }
  if (result.status === 0) {
    // pytest prints "N passed"; unittest -q prints dots then "OK".
    let passed;
    if (label.includes("pytest")) {
      passed = parseCounts(text).passed;
    } else {
      const dots = (text.match(/^[.EFsx]+$/gm) || []).join("").split("").filter((c) => c === ".").length;
      passed = dots;
    }
    return { stack: "python", command, passed, failed: 0 };
  }
  const counts = parseCounts(text);
  const failed = /(\d+) failed/.test(text) ? counts.failed : Math.max(counts.failed, 1);
  return { stack: "python", command, passed: counts.passed, failed, error: lastFailingLine(text) };
}

function runNodeSuite(dir, testScript) {
  let command;
  let result;
  if (testScript !== null) {
    command = "npm test";
    result = spawnSync("npm", ["test"], {
      stdio: "pipe",
      timeout: TIMEOUT_MS,
      cwd: dir,
      shell: process.platform === "win32",
    });
  } else {
    // No path argument: a bare directory can resolve as a module to
    // require() on some Node versions. With no args, the runner scans
    // cwd recursively for *.test.* files.
    command = `node --test ${dir}`;
    result = spawnSync(process.execPath, ["--test"], { stdio: "pipe", timeout: TIMEOUT_MS, cwd: dir });
  }
  if (result.error && result.error.code !== "ETIMEDOUT") {
    return { stack: "node", command, passed: 0, failed: 1, error: String(result.error.message || result.error) };
  }
  const text = combinedOutput(result);
  if (timedOut(result)) {
    return { stack: "node", command, passed: 0, failed: 1, error: `timeout after ${TIMEOUT_MS / 1000}s` };
  }
  const counts = parseTapCounts(text);
  if (result.status === 0) {
    return { stack: "node", command, passed: counts.passed, failed: 0 };
  }
  return { stack: "node", command, passed: counts.passed, failed: Math.max(counts.failed, 1), error: lastFailingLine(text) };
}

function reportMarkdown(dir, suites, summary, ok) {
  const verdict = ok ? "PASS" : "FAIL";
  const lines = [
    "# slimtoken-test gate report",
    "",
    `Directory: \`${dir}\``,
    `Verdict: ${verdict}`,
    "",
    "| Suite | Command | Passed | Failed | Verdict |",
    "|---|---|---|---|---|",
  ];
  for (const s of suites) {
    lines.push(
      `| ${s.stack} | \`${s.command}\` | ${s.passed} | ${s.failed} | ${s.error ? "ERROR" : "OK"} |`
    );
  }
  lines.push("", `Summary: ${summary}`, "");
  const failures = suites.filter((s) => s.error);
  if (failures.length > 0) {
    lines.push("## Failures", "");
    for (const s of failures) {
      lines.push(`### ${s.stack} — \`${s.command}\``, "", "```", s.error, "```", "");
    }
  }
  return lines.join("\n") + "\n";
}

export function runTests(args) {
  const positional = [];
  let outDir = null;
  for (let i = 0; i < args.length; i += 1) {
    if (args[i] === "--out") {
      outDir = args[i + 1];
      i += 1;
    } else if (args[i].startsWith("--")) {
      console.error(`run: unknown flag '${args[i]}'`);
      return 2;
    } else {
      positional.push(args[i]);
    }
  }
  const dir = positional[0];
  if (!dir) {
    console.error("usage: slimtoken-test run <dir> [--out DIR]");
    return 2;
  }
  let isDir = false;
  try {
    isDir = statSync(dir).isDirectory();
  } catch {
    isDir = false;
  }
  if (!isDir) {
    console.error(`run: '${dir}' is not a readable directory`);
    return 2;
  }
  if (outDir && positional.length > 1) {
    console.error("run: pass exactly one directory");
    return 2;
  }

  const absDir = resolve(dir);
  const files = [...walkFiles(dir)];
  const suites = [];
  if (detectPython(dir, files)) {
    suites.push(runPythonSuite(absDir));
  }
  const testScript = nodeTestScript(dir);
  if (detectNode(dir, files)) {
    suites.push(runNodeSuite(absDir, testScript));
  }

  if (suites.length === 0) {
    console.log(`SKIP ${dir}: no Python or Node tests detected`);
    return 0;
  }

  for (const s of suites) {
    if (s.error) {
      console.log(`ERROR ${s.stack}: ${s.error}`);
    } else {
      console.log(`OK ${s.stack} (${s.passed} passed)`);
    }
  }
  const failed = suites.filter((s) => s.error).length;
  const summary = `test gate: ${suites.length - failed}/${suites.length} suites passed`;
  console.log(summary);

  if (outDir) {
    mkdirSync(outDir, { recursive: true });
    const ok = failed === 0;
    const twin = { ok, suites, summary };
    writeFileSync(join(outDir, "test.json"), JSON.stringify(twin, null, 2) + "\n");
    writeFileSync(join(outDir, "report.md"), reportMarkdown(dir, suites, summary, ok));
  }

  return failed > 0 ? 1 : 0;
}
