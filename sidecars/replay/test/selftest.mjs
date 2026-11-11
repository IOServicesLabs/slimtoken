// selftest for slimtoken-replay. Spawns the dispatcher as a real process
// and asserts exit codes and report shapes against the fixtures.
// Zero tokens. Prints "selftest: N/N passed"; exit 0 only if all pass.

import { spawn } from "node:child_process";
import { existsSync, mkdtempSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const pack = resolve(here, "..");
const bin = join(pack, "bin", "slimtoken-replay.mjs");
const fx = (name) => join(pack, "fixtures", name);

function run(args) {
  return new Promise((resolvePromise) => {
    const proc = spawn(process.execPath, [bin, ...args], { cwd: pack });
    let stdout = "";
    let stderr = "";
    proc.stdout.on("data", (chunk) => { stdout += chunk; });
    proc.stderr.on("data", (chunk) => { stderr += chunk; });
    proc.on("close", (code) => resolvePromise({ code, stdout, stderr }));
  });
}

const tests = [];
function test(name, fn) {
  tests.push({ name, fn });
}

let outBase = null;

test("a vs b-good exits 0, replay.json ok:true with 5 tasks", async () => {
  const out = join(outBase, "good");
  const r = await run(["diff", fx("run-a"), fx("run-b-good"), "--out", out]);
  if (r.code !== 0) return `exit ${r.code}, stderr: ${r.stderr}`;
  if (!existsSync(join(out, "report.md"))) return "report.md not written";
  const replay = JSON.parse(readFileSync(join(out, "replay.json"), "utf8"));
  if (replay.ok !== true) return "replay.json ok is not true";
  if (replay.tasks.length !== 5) return `expected 5 tasks, got ${replay.tasks.length}`;
  if (replay.arms.length !== 2) return "arms must have 2 entries";
  return null;
});

test("a vs b-bad exits 1 with status flip, retry storm, token bump, missing-in-a", async () => {
  const r = await run(["diff", fx("run-a"), fx("run-b-bad")]);
  if (r.code !== 1) return `exit ${r.code}, expected 1`;
  const need = {
    "status regressed": /status regressed: succeeded -> failed/,
    "retry storm": /retry storm/,
    "output tokens": /output tokens \+50\.0%/,
    "missing-in-a": /FINDING t6: missing-in-a/
  };
  for (const [label, re] of Object.entries(need)) {
    if (!re.test(r.stdout)) return `missing finding: ${label}`;
  }
  return null;
});

test("reversed args (b-bad vs a) still exit 1", async () => {
  const r = await run(["diff", fx("run-b-bad"), fx("run-a")]);
  if (r.code !== 1) return `exit ${r.code}, expected 1`;
  if (!/missing-in-b/.test(r.stdout)) return "expected missing-in-b in reversed diff";
  return null;
});

test("same dir twice exits 0", async () => {
  const r = await run(["diff", fx("run-a"), fx("run-a")]);
  if (r.code !== 0) return `exit ${r.code}, expected 0`;
  return null;
});

test("missing dir exits 2", async () => {
  const r = await run(["diff", fx("run-a"), fx("run-nope")]);
  if (r.code !== 2) return `exit ${r.code}, expected 2`;
  return null;
});

test("one dir exits 2", async () => {
  const r = await run(["diff", fx("run-a")]);
  if (r.code !== 2) return `exit ${r.code}, expected 2`;
  return null;
});

test("unknown command exits 2", async () => {
  const r = await run(["frobnicate"]);
  if (r.code !== 2) return `exit ${r.code}, expected 2`;
  return null;
});

test("no args exits 2", async () => {
  const r = await run([]);
  if (r.code !== 2) return `exit ${r.code}, expected 2`;
  return null;
});

test("help exits 0", async () => {
  const r = await run(["help"]);
  if (r.code !== 0) return `exit ${r.code}, expected 0`;
  if (!/usage:/.test(r.stdout)) return "help text missing usage";
  return null;
});

test("--out writes both report.md and replay.json", async () => {
  const out = join(outBase, "outfiles");
  const r = await run(["diff", fx("run-a"), fx("run-b-good"), "--out", out]);
  if (r.code !== 0) return `exit ${r.code}, stderr: ${r.stderr}`;
  if (!existsSync(join(out, "report.md"))) return "report.md not written";
  if (!existsSync(join(out, "replay.json"))) return "replay.json not written";
  const replay = JSON.parse(readFileSync(join(out, "replay.json"), "utf8"));
  for (const key of ["ok", "arms", "tasks", "summary"]) {
    if (!(key in replay)) return `replay.json missing field ${key}`;
  }
  for (const key of ["tasks_compared", "findings", "by_rule"]) {
    if (!(key in replay.summary)) return `summary missing field ${key}`;
  }
  const row = replay.tasks[0];
  for (const key of ["id", "status_a", "status_b", "findings"]) {
    if (!(key in row)) return `task row missing field ${key}`;
  }
  return null;
});

test("--tokens-tolerance 100 suppresses the token finding, still exit 1", async () => {
  const r = await run(["diff", fx("run-a"), fx("run-b-bad"), "--tokens-tolerance", "100"]);
  if (r.code !== 1) return `exit ${r.code}, expected 1`;
  if (/output tokens/.test(r.stdout)) return "token finding not suppressed at tolerance 100";
  if (!/status regressed/.test(r.stdout)) return "status finding lost at tolerance 100";
  if (!/retry storm/.test(r.stdout)) return "retry storm finding lost at tolerance 100";
  return null;
});

async function main() {
  outBase = mkdtempSync(join(tmpdir(), "slimtoken-replay-"));
  let passed = 0;
  const failures = [];
  for (const { name, fn } of tests) {
    let error = null;
    try {
      error = await fn();
    } catch (err) {
      error = String(err && err.message ? err.message : err);
    }
    if (error === null) {
      passed += 1;
      console.log(`PASS ${name}`);
    } else {
      failures.push({ name, error });
      console.log(`FAIL ${name}: ${error}`);
    }
  }
  rmSync(outBase, { recursive: true, force: true });
  console.log(`selftest: ${passed}/${tests.length} passed`);
  process.exitCode = failures.length === 0 ? 0 : 1;
}

main();
