// selftest.mjs — zero-dependency selftest for the slimtoken-coverage pack.
//
// Spawns the dispatcher against the fixtures and asserts exit codes:
//   measure js-app = 0 with a partial total (1..99%), delta worse = 1,
//   delta same with --min-gain 1 = 1, delta improved = 0, measure with no
//   dir = 2, delta with missing args = 2, unknown command = 2, help = 0.
// Also checks that --out writes report.md plus the JSON twin, and that the
// twin parses with the expected shape.
//
// Python fixture assertions are skipped (reported, never faked) when no
// python interpreter is on PATH (`python`, then `py`) or when the coverage
// module is not installed.
import { spawnSync } from "node:child_process";
import { existsSync, readFileSync, rmSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const bin = join(root, "bin", "slimtoken-coverage.mjs");
const fixtures = join(root, "fixtures");
const deltaFixtures = join(root, "test", "fixtures");

let passed = 0;
let failed = 0;
const failures = [];

function check(name, cond, detail) {
  if (cond) {
    passed += 1;
  } else {
    failed += 1;
    failures.push(`${name}${detail ? ` — ${detail}` : ""}`);
  }
}

function cli(...args) {
  return spawnSync(process.execPath, [bin, ...args], { stdio: "pipe", timeout: 360_000 });
}

function pythonCoverageAvailable() {
  for (const name of ["python", "py"]) {
    const probe = spawnSync(name, ["--version"], { stdio: "pipe", timeout: 30_000 });
    if (probe.error || probe.status !== 0) continue;
    const cov = spawnSync(name, ["-c", "import coverage"], { stdio: "pipe", timeout: 30_000 });
    if (!cov.error && cov.status === 0) return name;
  }
  return null;
}

function readJson(path) {
  try {
    return JSON.parse(readFileSync(path, "utf8"));
  } catch (error) {
    return { __unparseable: error.message || String(error) };
  }
}

// --- measure: js-app (partial coverage) ---
const js = cli("measure", join(fixtures, "js-app"));
check("measure js-app exits 0", js.status === 0, `got ${js.status}: ${js.stderr}`);

const outDir = join(root, "test", ".selftest-out");
rmSync(outDir, { recursive: true, force: true });
const jsOut = cli("measure", join(fixtures, "js-app"), "--out", outDir);
check("measure js-app --out exits 0", jsOut.status === 0, `got ${jsOut.status}: ${jsOut.stderr}`);
check("--out writes report.md", existsSync(join(outDir, "report.md")));
check("--out writes coverage.json", existsSync(join(outDir, "coverage.json")));
const covTwin = readJson(join(outDir, "coverage.json"));
check(
  "coverage.json parses with ok === true",
  covTwin.ok === true,
  `got ${JSON.stringify(covTwin.__unparseable ?? covTwin.ok)}`
);
check(
  "coverage.json total.pct is between 1 and 99 (partial coverage)",
  typeof covTwin.total?.pct === "number" && covTwin.total.pct > 1 && covTwin.total.pct < 99,
  `got ${covTwin.total?.pct}`
);
check("coverage.json lists per-file entries", Array.isArray(covTwin.files) && covTwin.files.length > 0);
rmSync(outDir, { recursive: true, force: true });

// --- measure: py-app (only when coverage.py is importable here) ---
const py = pythonCoverageAvailable();
if (py) {
  const pyRun = cli("measure", join(fixtures, "py-app"), "--runner", "python");
  check("measure py-app exits 0", pyRun.status === 0, `got ${pyRun.status}: ${pyRun.stderr}`);

  const pyOutDir = join(root, "test", ".selftest-out-py");
  rmSync(pyOutDir, { recursive: true, force: true });
  const pyOut = cli("measure", join(fixtures, "py-app"), "--runner", "python", "--out", pyOutDir);
  check("measure py-app --out exits 0", pyOut.status === 0, `got ${pyOut.status}: ${pyOut.stderr}`);
  const pyTwin = readJson(join(pyOutDir, "coverage.json"));
  check(
    "py coverage.json total.pct is between 1 and 99 (partial coverage)",
    typeof pyTwin.total?.pct === "number" && pyTwin.total.pct > 1 && pyTwin.total.pct < 99,
    `got ${pyTwin.total?.pct}`
  );
  rmSync(pyOutDir, { recursive: true, force: true });
} else {
  console.log("selftest: python coverage module not available — skipping python measurement assertions");
}

// --- delta ---
const high = join(deltaFixtures, "pct-high.json");
const low = join(deltaFixtures, "pct-low.json");

const worse = cli("delta", high, low);
check("delta worse exits 1", worse.status === 1, `got ${worse.status}: ${worse.stderr}`);
check("delta worse prints movers", /MOVER/.test(worse.stdout.toString()), worse.stdout.toString().trim());

const same = cli("delta", high, high, "--min-gain", "1");
check("delta same with --min-gain 1 exits 1", same.status === 1, `got ${same.status}: ${same.stderr}`);

const improved = cli("delta", low, high);
check("delta improved exits 0", improved.status === 0, `got ${improved.status}: ${improved.stderr}`);

const deltaOutDir = join(root, "test", ".selftest-out-delta");
rmSync(deltaOutDir, { recursive: true, force: true });
const deltaOut = cli("delta", low, high, "--out", deltaOutDir);
check("delta --out exits 0", deltaOut.status === 0, `got ${deltaOut.status}: ${deltaOut.stderr}`);
check("delta --out writes report.md", existsSync(join(deltaOutDir, "report.md")));
check("delta --out writes delta.json", existsSync(join(deltaOutDir, "delta.json")));
const deltaTwin = readJson(join(deltaOutDir, "delta.json"));
check("delta.json parses with ok === true", deltaTwin.ok === true, `got ${JSON.stringify(deltaTwin.__unparseable ?? deltaTwin.ok)}`);
rmSync(deltaOutDir, { recursive: true, force: true });

// --- usage errors ---
const noDir = cli("measure");
check("measure with no dir exits 2", noDir.status === 2, `got ${noDir.status}`);

const missingDir = cli("measure", join(fixtures, "does-not-exist"));
check("measure with missing dir exits 2", missingDir.status === 2, `got ${missingDir.status}`);

const noArgs = cli("delta");
check("delta with missing args exits 2", noArgs.status === 2, `got ${noArgs.status}`);

const unknown = cli("frobnicate");
check("unknown command exits 2", unknown.status === 2, `got ${unknown.status}`);

const help = cli("help");
check("help exits 0", help.status === 0, `got ${help.status}`);

const total = passed + failed;
if (failed > 0) {
  for (const f of failures) console.log(`selftest FAIL: ${f}`);
}
console.log(`selftest: ${passed}/${total} passed`);
process.exit(failed === 0 ? 0 : 1);
