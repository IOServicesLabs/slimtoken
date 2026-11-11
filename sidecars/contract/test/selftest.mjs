// Selftest for the slimtoken-contract pack. Zero dependencies: spawns
// the dispatcher as a child process and asserts exit codes and the
// --out report files. Exit 0 only if every assertion holds.
import { spawnSync } from "node:child_process";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";
import { existsSync, mkdtempSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const bin = join(root, "bin", "slimtoken-contract.mjs");
const fx = join(root, "fixtures");

let passed = 0;
let failed = 0;
const failures = [];

function assert(name, condition, detail = "") {
  if (condition) {
    passed += 1;
    console.log(`ok   ${name}`);
  } else {
    failed += 1;
    failures.push(name);
    console.log(`FAIL ${name}${detail ? ` — ${detail}` : ""}`);
  }
}

function run(args) {
  return spawnSync(process.execPath, [bin, ...args], { encoding: "utf8" });
}

// 1-4. Per-fixture exit codes.
let r = run(["check", join(fx, "good", "openapi.json")]);
assert("good openapi exits 0", r.status === 0, `got ${r.status}: ${r.stderr}`);

r = run(["check", join(fx, "bad-ref", "openapi.json")]);
assert("bad-ref openapi exits 1", r.status === 1, `got ${r.status}`);
assert("bad-ref reports dangling $ref", (r.stdout || "").includes("dangling $ref: #/components/schemas/Missing"), r.stdout);

r = run(["check", join(fx, "bad-json", "broken.json")]);
assert("bad-json exits 1", r.status === 1, `got ${r.status}`);
assert("bad-json reports parse error", (r.stdout || "").includes("ERROR"), r.stdout);

r = run(["check", join(fx, "plain", "ok.json")]);
assert("plain json exits 0", r.status === 0, `got ${r.status}: ${r.stderr}`);

// 5. YAML-only dir: WARN SKIP, never fails.
r = run(["check", join(fx, "yaml")]);
assert("yaml dir exits 0 (warn only)", r.status === 0, `got ${r.status}`);
assert("yaml dir reports WARN SKIP", (r.stdout || "").includes("WARN SKIP"), r.stdout);

// 6-9. Usage errors and help.
r = run(["check", join(fx, "empty")]);
assert("empty dir exits 2", r.status === 2, `got ${r.status}`);

r = run([]);
assert("no args exits 2", r.status === 2, `got ${r.status}`);

r = run(["bogus"]);
assert("unknown command exits 2", r.status === 2, `got ${r.status}`);

r = run(["help"]);
assert("help exits 0", r.status === 0, `got ${r.status}: ${r.stderr}`);

// 10-11. --out writes report.md and contract.json with matching ok.
const outGood = mkdtempSync(join(tmpdir(), "slimtoken-contract-good-"));
r = run(["check", join(fx, "good", "openapi.json"), "--out", outGood]);
const reportGood = join(outGood, "report.md");
const contractGood = join(outGood, "contract.json");
assert("--out good exits 0", r.status === 0, `got ${r.status}`);
assert("--out good writes report.md", existsSync(reportGood));
assert("--out good writes contract.json", existsSync(contractGood));
let contract = null;
try {
  contract = JSON.parse(readFileSync(contractGood, "utf8"));
} catch (error) {
  assert("--out good contract.json parses", false, error.message);
}
if (contract) {
  assert("--out good contract.ok is true", contract.ok === true, JSON.stringify(contract.ok));
  assert("--out good contract file status ok",
    Array.isArray(contract.files) && contract.files.length === 1 && contract.files[0].status === "ok",
    JSON.stringify(contract.files));
}

const outBad = mkdtempSync(join(tmpdir(), "slimtoken-contract-bad-"));
r = run(["check", join(fx, "bad-ref", "openapi.json"), "--out", outBad]);
const contractBad = join(outBad, "contract.json");
assert("--out bad-ref exits 1", r.status === 1, `got ${r.status}`);
assert("--out bad-ref writes report.md", existsSync(join(outBad, "report.md")));
let badContract = null;
try {
  badContract = JSON.parse(readFileSync(contractBad, "utf8"));
} catch (error) {
  assert("--out bad-ref contract.json parses", false, error.message);
}
if (badContract) {
  assert("--out bad-ref contract.ok is false", badContract.ok === false, JSON.stringify(badContract.ok));
  assert("--out bad-ref contract carries the dangling $ref error",
    Array.isArray(badContract.files) && badContract.files.length === 1 &&
      badContract.files[0].errors.some((e) => e.includes("dangling $ref")),
    JSON.stringify(badContract.files));
}
rmSync(outGood, { recursive: true, force: true });
rmSync(outBad, { recursive: true, force: true });

console.log(`selftest: ${passed}/${passed + failed} passed`);
if (failed > 0) {
  console.error(`selftest: failures: ${failures.join(", ")}`);
  process.exit(1);
}
process.exit(0);
