// selftest.mjs — zero-dep self test for the slimtoken-review pack.
// Spawns the real dispatcher and asserts exit codes and --out artifacts.
// Prints "selftest: N/N passed" and exits 0 only if all checks hold.
import { spawnSync } from "node:child_process";
import { fileURLToPath, pathToFileURL } from "node:url";
import { dirname, join } from "node:path";
import { existsSync, readFileSync, rmSync } from "node:fs";
import { mkdtempSync } from "node:fs";
import { tmpdir } from "node:os";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const bin = join(root, "bin", "slimtoken-review.mjs");
const fixtures = join(root, "fixtures");

function run(args) {
  return spawnSync(process.execPath, [bin, ...args], { encoding: "utf8" });
}

function cargoOnPath() {
  return spawnSync("cargo", ["--version"], { stdio: "pipe" }).status === 0;
}

let passed = 0;
let failed = 0;
function check(name, cond, extra = "") {
  if (cond) {
    passed += 1;
    console.log(`ok   ${name}`);
  } else {
    failed += 1;
    console.log(`FAIL ${name}${extra ? ` — ${extra}` : ""}`);
  }
}

// js-good: clean module, exit 0
{
  const r = run(["lint", join(fixtures, "js-good")]);
  console.log(r.stdout.trim());
  check("js-good exit 0", r.status === 0, `got ${r.status}`);
}

// js-bad: syntax error, exit 1
{
  const r = run(["lint", join(fixtures, "js-bad")]);
  console.log(r.stdout.trim());
  check("js-bad exit 1", r.status === 1, `got ${r.status}`);
}

// empty: no checkable files, exit 0 with SKIP
{
  const r = run(["lint", join(fixtures, "empty")]);
  console.log(r.stdout.trim());
  check("empty exit 0", r.status === 0, `got ${r.status}`);
  check("empty prints SKIP", /WARN SKIP/.test(r.stdout));
}

// missing dir: exit 2
{
  const r = run(["lint", join(fixtures, "no-such-dir")]);
  check("missing dir exit 2", r.status === 2, `got ${r.status}`);
}

// lint with no args: exit 2
{
  const r = run(["lint"]);
  check("lint no dir arg exit 2", r.status === 2, `got ${r.status}`);
}

// unknown command: exit 2
{
  const r = run(["frobnicate"]);
  check("unknown command exit 2", r.status === 2, `got ${r.status}`);
}

// no args: exit 2 (usage)
{
  const r = run([]);
  check("no args exit 2", r.status === 2, `got ${r.status}`);
}

// help: exit 0
{
  const r = run(["help"]);
  check("help exit 0", r.status === 0, `got ${r.status}`);
}

// rust fixtures: only asserted when cargo is on PATH
if (cargoOnPath()) {
  {
    const r = run(["lint", join(fixtures, "rust-bad")]);
    console.log(r.stdout.trim());
    check("rust-bad exit 1", r.status === 1, `got ${r.status}`);
  }
  {
    const r = run(["lint", join(fixtures, "rust-good")]);
    console.log(r.stdout.trim());
    check("rust-good exit 0", r.status === 0, `got ${r.status}`);
  }
} else {
  console.log("note: cargo not on PATH — rust fixture checks skipped");
}

// --out: writes report.md + review.json with matching ok value
{
  const out = mkdtempSync(join(tmpdir(), "slimtoken-review-selftest-"));
  const good = run(["lint", join(fixtures, "js-good"), "--out", out]);
  const reviewPath = join(out, "review.json");
  const reportPath = join(out, "report.md");
  check("--out writes review.json", existsSync(reviewPath));
  check("--out writes report.md", existsSync(reportPath));
  if (existsSync(reviewPath)) {
    const review = JSON.parse(readFileSync(reviewPath, "utf8"));
    check("--out ok matches exit 0", review.ok === true && good.status === 0,
      `ok=${review.ok} exit=${good.status}`);
    check("review.json has checks array", Array.isArray(review.checks));
    check("review.json has summary", typeof review.summary === "string");
  }
  const outBad = mkdtempSync(join(tmpdir(), "slimtoken-review-selftest-bad-"));
  const bad = run(["lint", join(fixtures, "js-bad"), "--out", outBad]);
  const reviewBad = JSON.parse(readFileSync(join(outBad, "review.json"), "utf8"));
  check("--out ok matches exit 1", reviewBad.ok === false && bad.status === 1,
    `ok=${reviewBad.ok} exit=${bad.status}`);
  rmSync(out, { recursive: true, force: true });
  rmSync(outBad, { recursive: true, force: true });
}

const total = passed + failed;
console.log(`selftest: ${passed}/${total} passed`);
process.exit(failed === 0 ? 0 : 1);
