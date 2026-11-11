// Selftest for the slimtoken-trace pack. Zero dependencies: spawns
// the dispatcher as a child process and asserts exit codes, the --out
// report files, and corrupt-line tolerance. Exit 0 only if every
// assertion holds.
import { spawnSync } from "node:child_process";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";
import { existsSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const bin = join(root, "bin", "slimtoken-trace.mjs");
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

function readJson(path) {
  return JSON.parse(readFileSync(path, "utf8"));
}

// 1-3. Clean trace: exit 0, stats populated in trace.json.
const outClean = mkdtempSync(join(tmpdir(), "slimtoken-trace-clean-"));
let r = run(["analyze", join(fx, "clean.jsonl"), "--out", outClean]);
assert("clean trace exits 0", r.status === 0, `got ${r.status}: ${r.stderr}`);
const traceClean = readJson(join(outClean, "trace.json"));
assert(
  "clean stats populated",
  traceClean.stats && traceClean.stats.events > 0 && traceClean.stats.completions.succeeded === 10,
  JSON.stringify(traceClean.stats)
);
assert("clean findings empty", traceClean.ok === true && traceClean.findings.length === 0);

// 4-7. Stormy trace: exit 1, all six finding rules named in trace.json.
const outStormy = mkdtempSync(join(tmpdir(), "slimtoken-trace-stormy-"));
r = run(["analyze", join(fx, "stormy.jsonl"), "--out", outStormy]);
assert("stormy trace exits 1", r.status === 1, `got ${r.status}: ${r.stderr}`);
const traceStormy = readJson(join(outStormy, "trace.json"));
const rules = new Set(traceStormy.findings.map((f) => f.rule));
for (const rule of [
  "retry-storm",
  "repair-queue-depth",
  "rate-churn",
  "low-cache-hit",
  "concurrency-collapse",
  "failures-present"
]) {
  assert(`stormy names ${rule}`, rules.has(rule), JSON.stringify([...rules]));
}

// 8-11. Usage errors and help.
r = run(["analyze", join(fx, "missing.jsonl")]);
assert("missing file exits 2", r.status === 2, `got ${r.status}`);

r = run([]);
assert("no args exits 2", r.status === 2, `got ${r.status}`);

r = run(["bogus"]);
assert("unknown command exits 2", r.status === 2, `got ${r.status}`);

r = run(["help"]);
assert("help exits 0", r.status === 0, `got ${r.status}: ${r.stderr}`);

// 12-13. --out writes both report.md and trace.json.
assert("--out writes report.md", existsSync(join(outClean, "report.md")));
assert(
  "--out writes trace.json",
  existsSync(join(outClean, "trace.json")) && existsSync(join(outStormy, "trace.json"))
);
const reportStormy = readFileSync(join(outStormy, "report.md"), "utf8");
assert(
  "report.md carries the tuning brief",
  reportStormy.includes("Tuning brief") && reportStormy.includes("rate_auto_tune"),
  reportStormy.slice(0, 200)
);

// 14-15. Corrupt line: WARN recorded, no crash, exit still 0.
const outCorrupt = mkdtempSync(join(tmpdir(), "slimtoken-trace-corrupt-"));
const corruptPath = join(outCorrupt, "corrupt.jsonl");
writeFileSync(
  corruptPath,
  [
    '{"event":"run_start","elapsed_ms":0,"fields":{"tasks":1,"primes":0}}',
    "this is not json {{{",
    '{"event":"task_complete","elapsed_ms":100,"fields":{"task_id":"t-1","status":"succeeded","attempts":1,"duration_ms":90,"input_tokens":100,"output_tokens":50,"cached_input_tokens":90,"ttft_ms":10}}',
    '{"event":"run_end","elapsed_ms":200,"fields":{"succeeded":1,"failed":0,"skipped":0,"peak_concurrency":1,"output_tokens":50}}',
    ""
  ].join("\n"),
  "utf8"
);
r = run(["analyze", corruptPath, "--out", outCorrupt]);
assert("corrupt line does not crash", r.status === 0, `got ${r.status}: ${r.stderr}`);
assert(
  "corrupt line records WARN",
  (r.stdout || "").includes("WARN") && readJson(join(outCorrupt, "trace.json")).stats.warn_lines === 1,
  r.stdout
);

rmSync(outClean, { recursive: true, force: true });
rmSync(outStormy, { recursive: true, force: true });
rmSync(outCorrupt, { recursive: true, force: true });

console.log(`selftest: ${passed}/${passed + failed} passed`);
if (failed > 0) {
  console.log(`failures: ${failures.join(", ")}`);
  process.exit(1);
}
process.exit(0);
