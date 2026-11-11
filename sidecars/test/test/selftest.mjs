// selftest.mjs — zero-dependency selftest for the slimtoken-test pack.
//
// Spawns the dispatcher against the fixtures and asserts exit codes:
//   py-pass = 0, py-fail = 1, js-pass = 0, js-fail = 1, empty = 0,
//   run with no dir = 2, unknown command = 2, help = 0.
// Also checks that --out writes report.md and test.json, and that
// test.json parses with ok === expected.
//
// Python fixtures are skipped (reported, never faked) when no python
// interpreter is on PATH (`python`, then `py`).
import { spawnSync } from "node:child_process";
import { existsSync, readFileSync, rmSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const bin = join(root, "bin", "slimtoken-test.mjs");
const fixtures = join(root, "fixtures");

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

function findPython() {
  for (const name of ["python", "py"]) {
    const probe = spawnSync(name, ["--version"], { stdio: "pipe", timeout: 30_000 });
    if (!probe.error && probe.status === 0) return name;
  }
  return null;
}

const py = findPython();

// Fixture exit codes.
if (py) {
  const pyPass = cli("run", join(fixtures, "py-pass"));
  check("py-pass exits 0", pyPass.status === 0, `got ${pyPass.status}: ${pyPass.stderr}`);

  const pyFail = cli("run", join(fixtures, "py-fail"));
  check("py-fail exits 1", pyFail.status === 1, `got ${pyFail.status}: ${pyFail.stderr}`);
} else {
  console.log("selftest: no python on PATH — skipping python fixture assertions");
}

const jsPass = cli("run", join(fixtures, "js-pass"));
check("js-pass exits 0", jsPass.status === 0, `got ${jsPass.status}: ${jsPass.stderr}`);

const jsFail = cli("run", join(fixtures, "js-fail"));
check("js-fail exits 1", jsFail.status === 1, `got ${jsFail.status}: ${jsFail.stderr}`);

const empty = cli("run", join(fixtures, "empty"));
check("empty exits 0", empty.status === 0, `got ${empty.status}: ${empty.stderr}`);
check("empty prints SKIP", /SKIP/.test(empty.stdout.toString()), empty.stdout.toString().trim());

// Usage errors.
const noDir = cli("run");
check("run with no dir exits 2", noDir.status === 2, `got ${noDir.status}`);

const missingDir = cli("run", join(fixtures, "does-not-exist"));
check("run with missing dir exits 2", missingDir.status === 2, `got ${missingDir.status}`);

const unknown = cli("frobnicate");
check("unknown command exits 2", unknown.status === 2, `got ${unknown.status}`);

const help = cli("help");
check("help exits 0", help.status === 0, `got ${help.status}`);

// --out writes report.md and test.json; the JSON twin parses.
const outDir = join(root, "test", ".selftest-out");
rmSync(outDir, { recursive: true, force: true });
const withOut = cli("run", join(fixtures, "js-pass"), "--out", outDir);
check("--out run exits 0", withOut.status === 0, `got ${withOut.status}: ${withOut.stderr}`);
check("--out writes report.md", existsSync(join(outDir, "report.md")));
check("--out writes test.json", existsSync(join(outDir, "test.json")));
let twinOk = null;
try {
  twinOk = JSON.parse(readFileSync(join(outDir, "test.json"), "utf8")).ok;
} catch (error) {
  twinOk = `unparseable: ${error.message}`;
}
check("test.json parses with ok === true", twinOk === true, `got ${twinOk}`);

const outDirFail = join(root, "test", ".selftest-out-fail");
rmSync(outDirFail, { recursive: true, force: true });
const withOutFail = cli("run", join(fixtures, "js-fail"), "--out", outDirFail);
check("--out fail run exits 1", withOutFail.status === 1, `got ${withOutFail.status}`);
let twinFailOk = null;
try {
  twinFailOk = JSON.parse(readFileSync(join(outDirFail, "test.json"), "utf8")).ok;
} catch (error) {
  twinFailOk = `unparseable: ${error.message}`;
}
check("test.json parses with ok === false", twinFailOk === false, `got ${twinFailOk}`);
rmSync(outDir, { recursive: true, force: true });
rmSync(outDirFail, { recursive: true, force: true });

const total = passed + failed;
if (failed > 0) {
  for (const f of failures) console.log(`selftest FAIL: ${f}`);
}
console.log(`selftest: ${passed}/${total} passed`);
process.exit(failed === 0 ? 0 : 1);
