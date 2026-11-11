// probe.mjs — the slimtoken-flake gate.
//
// Contract (see sidecars/README.md):
//   - exit 0: every test passed in every run
//   - exit 1: at least one FLAKY or BROKEN test (or a suite gave no
//     signal — fail closed)
//   - exit 2: usage error, no suite found, or the suite errored every
//     run (nothing measurable — fail closed)
//   - zero tokens: shell out to node --test / pytest, never call a model
//
//   slimtoken-flake probe <dir> [--runs N] [--out DIR]
//
// Detection (same rules as the test pack):
//   - Node: a package.json with a "test" script, or any *.test.js /
//     *.test.mjs file. The probe always runs `node --test` itself, with
//     cwd:<dir> and no positional dir arg, and a JUnit reporter writing
//     to a temp file. A custom npm test script is not replayed.
//   - Python: any test_*.py / *_test.py file, a pytest.ini, or a
//     pyproject.toml with a [tool.pytest...] section — but only when
//     `python -m pytest --version` answers. Without pytest there is no
//     usable per-test reporter (unittest), so the probe prints
//     WARN SKIP; alone, that means nothing measurable, exit 2.
//
// Verdicts: a test that failed >= 1 run while passing >= 1 is FLAKY. A
// test that failed every run is BROKEN (a real defect, not a flake —
// a different verdict, still a finding). All passing is clean.
//
// A run counts as "errored" when the runner could not start, timed out,
// or produced no parseable JUnit testcases. When every run of every
// detected suite errors there is nothing measurable: exit 2. When only
// some runs error, the affected tests print SKIP and count as findings
// — a count taken over fewer runs than asked for is not proof.
//
// Async spawn only: keep the selftest's event loop free (never
// spawnSync here).
import { spawn } from "node:child_process";
import { existsSync, mkdirSync, readdirSync, readFileSync, rmSync, statSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { parseJUnit } from "./junit.mjs";

const TIMEOUT_MS = 300_000;
const PY_CANDIDATES = ["python", "py"];
const DEFAULT_RUNS = 3;

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

// Async spawn. Resolves when the child closes; never rejects.
function runCommand(command, args, cwd) {
  return new Promise((resolvePromise) => {
    let child;
    try {
      child = spawn(command, args, { cwd: cwd || undefined, stdio: "pipe" });
    } catch (error) {
      resolvePromise({ error, stdout: "", stderr: "", status: null, signal: null, timedOut: false });
      return;
    }
    let stdout = "";
    let stderr = "";
    let timedOut = false;
    const timer = setTimeout(() => {
      timedOut = true;
      try {
        child.kill();
      } catch {
        /* already dead */
      }
    }, TIMEOUT_MS);
    child.stdout.on("data", (d) => {
      stdout += d.toString();
    });
    child.stderr.on("data", (d) => {
      stderr += d.toString();
    });
    child.on("error", (error) => {
      clearTimeout(timer);
      resolvePromise({ error, stdout, stderr, status: null, signal: null, timedOut });
    });
    child.on("close", (status, signal) => {
      clearTimeout(timer);
      resolvePromise({ error: null, stdout, stderr, status, signal, timedOut });
    });
  });
}

async function findPython() {
  for (const name of PY_CANDIDATES) {
    const probe = await runCommand(name, ["--version"], null);
    if (!probe.error && probe.status === 0) return name;
  }
  return null;
}

async function hasPytest(py) {
  const probe = await runCommand(py, ["-m", "pytest", "--version"], null);
  return !probe.error && probe.status === 0;
}

function readCases(reportFile) {
  let xml;
  try {
    xml = readFileSync(reportFile, "utf8");
  } catch {
    return null;
  }
  if (!xml.includes("<testcase")) return [];
  return parseJUnit(xml);
}

// One node --test run. A failing test is expected (that is the signal),
// so only process-level failure marks the run as errored.
async function runNodeOnce(dir, runIndex) {
  const reportFile = join(tmpdir(), `slimtoken-flake-${process.pid}-${Date.now()}-n${runIndex}.xml`);
  const args = ["--test", "--test-reporter=junit", `--test-reporter-destination=${reportFile}`];
  const result = await runCommand(process.execPath, args, dir);
  if (result.error) {
    return { errored: true, reason: String(result.error.message || result.error), cases: [] };
  }
  if (result.timedOut) {
    rmSync(reportFile, { force: true });
    return { errored: true, reason: `timeout after ${TIMEOUT_MS / 1000}s`, cases: [] };
  }
  const cases = readCases(reportFile);
  rmSync(reportFile, { force: true });
  if (cases === null) {
    return { errored: true, reason: `no JUnit report from node --test (exit ${result.status})`, cases: [] };
  }
  if (cases.length === 0) {
    return { errored: true, reason: "node --test reported zero testcases", cases: [] };
  }
  return { errored: false, reason: "", cases };
}

async function runPytestOnce(py, dir, runIndex) {
  const reportFile = join(tmpdir(), `slimtoken-flake-${process.pid}-${Date.now()}-p${runIndex}.xml`);
  const args = ["-m", "pytest", dir, "-q", `--junitxml=${reportFile}`];
  const result = await runCommand(py, args, dir);
  if (result.error) {
    return { errored: true, reason: String(result.error.message || result.error), cases: [] };
  }
  if (result.timedOut) {
    rmSync(reportFile, { force: true });
    return { errored: true, reason: `timeout after ${TIMEOUT_MS / 1000}s`, cases: [] };
  }
  const cases = readCases(reportFile);
  rmSync(reportFile, { force: true });
  if (cases === null) {
    return { errored: true, reason: `no JUnit report from pytest (exit ${result.status})`, cases: [] };
  }
  if (cases.length === 0) {
    return { errored: true, reason: "pytest reported zero testcases", cases: [] };
  }
  return { errored: false, reason: "", cases };
}

// Stable test identity across runs. pytest sets classname (module);
// node --test leaves it empty or equal to the name, so the enclosing
// testsuite name (the test file path) disambiguates.
function caseKey(c) {
  if (c.classname && c.classname !== c.name) return `${c.classname} :: ${c.name}`;
  if (c.suite && c.suite !== c.name) return `${c.suite} :: ${c.name}`;
  return c.name;
}

function reportMarkdown(dir, runs, tests, summary, ok) {
  const lines = [
    "# slimtoken-flake probe report",
    "",
    `Directory: \`${dir}\``,
    `Runs: ${runs}`,
    `Verdict: ${ok ? "PASS" : "FAIL"}`,
    "",
    "| Test | Passed | Failed | Verdict |",
    "|---|---|---|---|",
  ];
  for (const t of tests) {
    lines.push(`| ${t.name} | ${t.passed} | ${t.failed} | ${t.verdict} |`);
  }
  lines.push("", `Summary: ${summary}`, "");
  return lines.join("\n") + "\n";
}

export async function runProbe(args) {
  const positional = [];
  let runs = DEFAULT_RUNS;
  let outDir = null;
  for (let i = 0; i < args.length; i += 1) {
    if (args[i] === "--runs") {
      const raw = args[i + 1];
      if (raw === undefined || !/^\d+$/.test(raw) || Number(raw) < 1) {
        console.error("probe: --runs needs a positive integer");
        return 2;
      }
      runs = Number(raw);
      i += 1;
    } else if (args[i] === "--out") {
      outDir = args[i + 1];
      if (outDir === undefined) {
        console.error("probe: --out needs a directory");
        return 2;
      }
      i += 1;
    } else if (args[i].startsWith("--")) {
      console.error(`probe: unknown flag '${args[i]}'`);
      return 2;
    } else {
      positional.push(args[i]);
    }
  }
  const dir = positional[0];
  if (!dir) {
    console.error("usage: slimtoken-flake probe <dir> [--runs N] [--out DIR]");
    return 2;
  }
  if (positional.length > 1) {
    console.error("probe: pass exactly one directory");
    return 2;
  }
  let isDir = false;
  try {
    isDir = statSync(dir).isDirectory();
  } catch {
    isDir = false;
  }
  if (!isDir) {
    console.error(`probe: '${dir}' is not a readable directory`);
    return 2;
  }

  const absDir = resolve(dir);
  const files = [...walkFiles(absDir)];

  const suites = [];
  if (detectPython(absDir, files)) {
    const py = await findPython();
    if (py && (await hasPytest(py))) {
      suites.push({ stack: "python (pytest)", runOnce: (i) => runPytestOnce(py, absDir, i) });
    } else {
      console.log("WARN SKIP python: tests found but pytest is not available (unittest has no usable per-test reporter)");
    }
  }
  if (detectNode(absDir, files)) {
    suites.push({ stack: "node", runOnce: (i) => runNodeOnce(absDir, i) });
  }
  if (suites.length === 0) {
    console.log(`probe: no measurable suite in ${dir} (need node --test files or pytest)`);
    return 2;
  }

  const measured = [];
  let deadSuites = 0;
  for (const suite of suites) {
    console.log(`suite: ${suite.stack} — ${runs} runs`);
    const results = [];
    for (let i = 0; i < runs; i += 1) {
      results.push(await suite.runOnce(i));
    }
    const signal = results.filter((r) => !r.errored).length;
    for (const r of results) {
      if (r.errored) console.log(`WARN ${suite.stack}: run errored — ${r.reason}`);
    }
    if (signal === 0) {
      deadSuites += 1;
      console.log(`WARN ${suite.stack}: no measurable run in ${runs} tries`);
    } else {
      measured.push({ suite: suite.stack, results });
    }
  }
  if (measured.length === 0) {
    console.log("probe: the suite errored in every run — nothing measurable (fail closed)");
    return 2;
  }

  // Aggregate per test across all measured runs of all suites. Within
  // one run, a duplicated testcase that fails anywhere fails the run.
  const tests = new Map();
  for (const { results } of measured) {
    for (const r of results) {
      if (r.errored) continue;
      const perRun = new Map();
      for (const c of r.cases) {
        const key = caseKey(c);
        // A skipped testcase did not pass; count it as a failed run so
        // the verdict math stays honest.
        const failed = c.failed || c.skipped;
        const prev = perRun.get(key);
        perRun.set(key, { failed: Boolean(prev && prev.failed) || failed });
      }
      for (const [key, runResult] of perRun) {
        let entry = tests.get(key);
        if (!entry) {
          entry = { name: key, passed: 0, failed: 0 };
          tests.set(key, entry);
        }
        if (runResult.failed) entry.failed += 1;
        else entry.passed += 1;
      }
    }
  }

  const list = [...tests.values()].map((t) => {
    let verdict;
    if (t.failed > 0 && t.passed > 0) verdict = "FLAKY";
    else if (t.failed === runs) verdict = "BROKEN";
    else if (t.passed === runs) verdict = "OK";
    else verdict = "SKIP";
    return { ...t, verdict };
  });
  const order = { FLAKY: 0, BROKEN: 1, SKIP: 2, OK: 3 };
  list.sort((a, b) => order[a.verdict] - order[b.verdict] || a.name.localeCompare(b.name));

  const count = (v) => list.filter((t) => t.verdict === v).length;
  for (const t of list) {
    if (t.verdict === "OK") console.log(`OK ${t.name} (${runs}/${runs})`);
    else if (t.verdict === "FLAKY") console.log(`FLAKY ${t.name} (${t.passed}/${runs} passed)`);
    else if (t.verdict === "BROKEN") console.log(`BROKEN ${t.name} (0/${runs})`);
    else console.log(`SKIP ${t.name} (${t.passed}/${runs} passed, ${runs - t.passed - t.failed} runs errored)`);
  }
  const findings = count("FLAKY") + count("BROKEN") + count("SKIP") + deadSuites;
  let summary = `flake probe: ${list.length} tests — ${count("OK")} ok, ${count("FLAKY")} flaky, ${count("BROKEN")} broken (${runs} runs)`;
  if (deadSuites > 0) summary += `; ${deadSuites} suite(s) gave no signal`;
  console.log(summary);

  const ok = findings === 0;
  if (outDir) {
    mkdirSync(outDir, { recursive: true });
    const twin = { ok, runs, tests: list, summary };
    writeFileSync(join(outDir, "flake.json"), JSON.stringify(twin, null, 2) + "\n");
    writeFileSync(join(outDir, "report.md"), reportMarkdown(dir, runs, list, summary, ok));
  }

  return ok ? 0 : 1;
}
