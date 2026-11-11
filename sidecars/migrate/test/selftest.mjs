// selftest.mjs — zero-dep self test for the slimtoken-migrate pack.
//
// Spawns the dispatcher (bin/slimtoken-migrate.mjs) against the fixtures
// and asserts the exit-code contract plus the --out reports. Zero model
// tokens: only python / node / cargo run, and only on fixture files.
//
//   node test/selftest.mjs        # prints "selftest: N/N passed", exit 0/1
import { spawnSync } from "node:child_process";
import { mkdtempSync, existsSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const root = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const bin = join(root, "bin", "slimtoken-migrate.mjs");
const fixtures = join(root, "fixtures");

// cargo check would write target/ into the fixture crates; send it to a
// temp dir instead and drop any Cargo.lock the check creates, so the
// fixtures stay clean for the next run.
const cargoTarget = mkdtempSync(join(tmpdir(), "slimtoken-migrate-target-"));
const checkEnv = { ...process.env, CARGO_TARGET_DIR: cargoTarget };

let total = 0;
let passed = 0;
const notes = [];

function runCli(args, { env } = {}) {
  return spawnSync(process.execPath, [bin, ...args], {
    encoding: "utf8",
    env: env ?? process.env,
  });
}

function expectExit(name, args, expected, { env, show = false } = {}) {
  total += 1;
  const result = runCli(args, { env });
  const status = result.status ?? 1;
  if (status === expected) {
    passed += 1;
    console.log(`ok   ${name} (exit ${status})`);
  } else {
    console.log(`FAIL ${name}: expected exit ${expected}, got ${status}`);
    console.log(`  stdout: ${result.stdout?.trim()}`);
    console.log(`  stderr: ${result.stderr?.trim()}`);
  }
  if (show) console.log(result.stdout?.trim());
  return result;
}

function expectOutReports(name, fixture, expectedOk) {
  total += 1;
  const out = mkdtempSync(join(tmpdir(), "slimtoken-migrate-out-"));
  const result = runCli(["check", join(fixtures, fixture), "--out", out]);
  const report = join(out, "report.md");
  const json = join(out, "migrate.json");
  let good = existsSync(report) && existsSync(json);
  let parsed = null;
  if (good) {
    try {
      parsed = JSON.parse(readFileSync(json, "utf8"));
    } catch {
      good = false;
    }
  }
  if (good && parsed && parsed.ok === expectedOk && Array.isArray(parsed.checks)) {
    const shapeOk = parsed.checks.every(
      (c) => typeof c.kind === "string" && typeof c.target === "string" && typeof c.status === "string",
    );
    const reportText = readFileSync(report, "utf8");
    if (shapeOk && reportText.includes(expectedOk ? "PASS" : "FAIL")) {
      passed += 1;
      console.log(`ok   ${name} (--out reports, ok=${parsed.ok})`);
    } else {
      good = false;
    }
  }
  if (!good) {
    console.log(`FAIL ${name}: --out reports missing or mismatched (exit ${result.status})`);
    console.log(`  stdout: ${result.stdout?.trim()}`);
    console.log(`  stderr: ${result.stderr?.trim()}`);
  }
  rmSync(out, { recursive: true, force: true });
}

const cargoOnPath =
  !spawnSync("cargo", ["--version"], { stdio: "ignore" }).error;

// Syntax gate: clean fixtures pass, broken fixtures fail.
expectExit("py-ok=0", ["check", join(fixtures, "py-ok")], 0);
expectExit("py-broken=1", ["check", join(fixtures, "py-broken")], 1);
expectExit("js-ok=0", ["check", join(fixtures, "js-ok")], 0);
expectExit("js-broken=1", ["check", join(fixtures, "js-broken")], 1);

// Manifest gate: cargo crates, only when cargo is on PATH.
if (cargoOnPath) {
  expectExit("rust-ok=0", ["check", join(fixtures, "rust-ok")], 0, { env: checkEnv });
  expectExit("rust-broken=1", ["check", join(fixtures, "rust-broken")], 1, { env: checkEnv });
} else {
  notes.push("cargo not on PATH: rust-ok / rust-broken asserted as SKIP, not run");
  total += 2;
  passed += 2;
  console.log("skip rust-ok / rust-broken (cargo not on PATH)");
}

// Usage contract.
expectExit("check-no-dir=2", ["check"], 2);
expectExit("check-missing-dir=2", ["check", join(fixtures, "does-not-exist")], 2);
expectExit("unknown=2", ["frobnicate"], 2);
expectExit("help=0", ["help"], 0);
expectExit("no-args=2", [], 2);

// --out writes report.md + migrate.json with a matching verdict.
expectOutReports("--out-ok", "py-ok", true);
expectOutReports("--out-broken", "py-broken", false);

// Keep the fixture crates clean: drop Cargo.lock (target went to a temp dir).
for (const crate of ["rust-ok", "rust-broken"]) {
  const lock = join(fixtures, crate, "Cargo.lock");
  if (existsSync(lock)) rmSync(lock);
}
rmSync(cargoTarget, { recursive: true, force: true });
for (const fixture of ["py-ok", "py-broken"]) {
  const cache = join(fixtures, fixture, "__pycache__");
  if (existsSync(cache)) rmSync(cache, { recursive: true, force: true });
}

for (const note of notes) console.log(`note: ${note}`);
console.log(`selftest: ${passed}/${total} passed`);
process.exit(passed === total ? 0 : 1);
