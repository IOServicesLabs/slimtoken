// selftest.mjs — zero-dependency selftest for the slimtoken-flake pack.
//
// Spawns the dispatcher against the fixtures and asserts exit codes and
// verdict lines:
//   flaky --runs 2  = 1 and names the FLAKY test (not BROKEN)
//   stable --runs 2 = 0
//   broken --runs 2 = 1 and names the BROKEN test (not FLAKY)
//   missing dir = 2, empty dir = 2, probe with no dir = 2,
//   no args = 2, unknown command = 2, help = 0
// Also checks that --out writes report.md and flake.json with a JSON
// twin whose ok matches the exit code.
//
// The flaky fixture reads and writes a counter FILE next to itself
// (fails when the counter is 0, passes otherwise). This selftest resets
// that counter before every scenario that uses the fixture. The pack
// lib uses async spawn only, so this selftest's event loop stays free
// (no server is hosted here).
import { spawnSync } from "node:child_process";
import { existsSync, readFileSync, rmSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const bin = join(root, "bin", "slimtoken-flake.mjs");
const fixtures = join(root, "fixtures");
const counterFile = join(fixtures, "flaky", ".counter");

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
  return spawnSync(process.execPath, [bin, ...args], { stdio: "pipe", timeout: 360_000, cwd: root });
}

function resetCounter() {
  rmSync(counterFile, { force: true });
}

function linesMatching(text, re) {
  return text.split(/\r?\n/).filter((l) => re.test(l));
}

// Flaky fixture: first of 2 runs fails, second passes.
resetCounter();
const flaky = cli("probe", join(fixtures, "flaky"), "--runs", "2");
const flakyOut = flaky.stdout.toString();
check("flaky exits 1", flaky.status === 1, `got ${flaky.status}: ${flaky.stderr}`);
const flakyLines = linesMatching(flakyOut, /^FLAKY /);
check("flaky prints one FLAKY line", flakyLines.length === 1, flakyOut.trim());
check(
  "flaky names the test",
  flakyLines.length === 1 && flakyLines[0].includes("flaky counter"),
  flakyLines.join(" | ")
);
check("flaky is not BROKEN", !/^BROKEN /m.test(flakyOut), flakyOut.trim());

// Stable fixture: both runs pass.
const stable = cli("probe", join(fixtures, "stable"), "--runs", "2");
const stableOut = stable.stdout.toString();
check("stable exits 0", stable.status === 0, `got ${stable.status}: ${stable.stderr}`);
check(
  "stable has no findings",
  !/^(FLAKY|BROKEN|SKIP) /m.test(stableOut) && /^OK /m.test(stableOut),
  stableOut.trim()
);

// Broken fixture: both runs fail, so the verdict is BROKEN, not FLAKY.
const broken = cli("probe", join(fixtures, "broken"), "--runs", "2");
const brokenOut = broken.stdout.toString();
check("broken exits 1", broken.status === 1, `got ${broken.status}: ${broken.stderr}`);
const brokenLines = linesMatching(brokenOut, /^BROKEN /);
check("broken prints one BROKEN line", brokenLines.length === 1, brokenOut.trim());
check(
  "broken names the test",
  brokenLines.length === 1 && brokenLines[0].includes("broken always fails"),
  brokenLines.join(" | ")
);
check("broken is not FLAKY", !/^FLAKY /m.test(brokenOut), brokenOut.trim());

// Usage and detection errors.
const missing = cli("probe", join(fixtures, "does-not-exist"));
check("missing dir exits 2", missing.status === 2, `got ${missing.status}`);

const empty = cli("probe", join(fixtures, "empty"));
check("empty dir exits 2", empty.status === 2, `got ${empty.status}`);
check(
  "empty dir says no suite",
  /no measurable suite/i.test(`${empty.stdout}\n${empty.stderr}`),
  empty.stdout.toString().trim()
);

const noDir = cli("probe");
check("probe with no dir exits 2", noDir.status === 2, `got ${noDir.status}`);

const noArgs = cli();
check("no args exits 2", noArgs.status === 2, `got ${noArgs.status}`);

const unknown = cli("frobnicate");
check("unknown command exits 2", unknown.status === 2, `got ${unknown.status}`);

const help = cli("help");
check("help exits 0", help.status === 0, `got ${help.status}`);

// --out: report.md + flake.json, JSON twin ok matches the exit code.
const outOk = join(root, "test", ".selftest-out");
rmSync(outOk, { recursive: true, force: true });
const stableOutRun = cli("probe", join(fixtures, "stable"), "--runs", "2", "--out", outOk);
check("--out clean run exits 0", stableOutRun.status === 0, `got ${stableOutRun.status}: ${stableOutRun.stderr}`);
check("--out writes report.md", existsSync(join(outOk, "report.md")));
check("--out writes flake.json", existsSync(join(outOk, "flake.json")));
let twinOk = null;
let twinRuns = null;
try {
  const twin = JSON.parse(readFileSync(join(outOk, "flake.json"), "utf8"));
  twinOk = twin.ok;
  twinRuns = twin.runs;
} catch (error) {
  twinOk = `unparseable: ${error.message}`;
}
check("flake.json ok === true", twinOk === true, `got ${twinOk}`);
check("flake.json runs === 2", twinRuns === 2, `got ${twinRuns}`);

const outFail = join(root, "test", ".selftest-out-fail");
rmSync(outFail, { recursive: true, force: true });
resetCounter();
const flakyOutRun = cli("probe", join(fixtures, "flaky"), "--runs", "2", "--out", outFail);
check("--out flaky run exits 1", flakyOutRun.status === 1, `got ${flakyOutRun.status}`);
let twinFail = null;
let failVerdicts = [];
try {
  const twin = JSON.parse(readFileSync(join(outFail, "flake.json"), "utf8"));
  twinFail = twin.ok;
  failVerdicts = twin.tests.map((t) => t.verdict);
} catch (error) {
  twinFail = `unparseable: ${error.message}`;
}
check("flake.json ok === false", twinFail === false, `got ${twinFail}`);
check("flake.json names a FLAKY verdict", failVerdicts.includes("FLAKY"), failVerdicts.join(","));

rmSync(outOk, { recursive: true, force: true });
rmSync(outFail, { recursive: true, force: true });
rmSync(counterFile, { force: true });

const total = passed + failed;
if (failed > 0) {
  for (const f of failures) console.log(`selftest FAIL: ${f}`);
}
console.log(`selftest: ${passed}/${total} passed`);
process.exit(failed === 0 ? 0 : 1);
