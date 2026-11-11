// slimtoken-fuzz selftest. Runs the CLI against the fixtures and asserts
// the exit-code contract. Zero tokens. Exits 0 only if every check passes.
import { spawnSync } from "node:child_process";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";
import { existsSync, readFileSync, rmSync, mkdtempSync } from "node:fs";
import { tmpdir } from "node:os";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const bin = join(root, "bin", "slimtoken-fuzz.mjs");
const fixtures = join(root, "fixtures");
const crashy = join(fixtures, "targets", "crashy.mjs");
const clean = join(fixtures, "targets", "clean.mjs");
const noexport = join(fixtures, "targets", "noexport.mjs");
const seeds = join(fixtures, "seeds");

let passed = 0;
let failed = 0;

function run(...args) {
  const r = spawnSync(process.execPath, [bin, ...args], { encoding: "utf8" });
  return { code: r.status ?? 1, stdout: r.stdout ?? "", stderr: r.stderr ?? "" };
}

function check(name, cond, detail = "") {
  if (cond) {
    passed += 1;
    console.log(`ok   ${name}`);
  } else {
    failed += 1;
    console.log(`FAIL ${name}${detail ? " — " + detail : ""}`);
  }
}

// help exits 0 and prints usage.
{
  const r = run("help");
  check("help exits 0", r.code === 0, `code=${r.code}`);
  check("help prints usage", r.stdout.includes("usage: slimtoken-fuzz run"));
}

// No args: usage error, exit 2.
{
  const r = run();
  check("no args exits 2", r.code === 2, `code=${r.code}`);
}

// Unknown command: exit 2.
{
  const r = run("bogus");
  check("unknown command exits 2", r.code === 2, `code=${r.code}`);
}

// crashy target: deterministic finding with seed 1. Note: seed 1 is the
// default and is verified here to reach the planted 💥 crash; the campaign
// is fully deterministic, so a second run must print identical output.
{
  const r1 = run("run", crashy, "--seeds", seeds, "--seed", "1", "--cases", "500");
  check("crashy exits 1", r1.code === 1, `code=${r1.code} stderr=${r1.stderr}`);
  check("crashy reports exactly 1 finding", r1.stdout.includes("cases: ") && r1.stdout.includes(", findings: 1"), r1.stdout);
  check("crashy finding line names the boom", r1.stdout.includes("boom at"), r1.stdout);
  const r2 = run("run", crashy, "--seeds", seeds, "--seed", "1", "--cases", "500");
  check("crashy run is deterministic", r1.stdout === r2.stdout);
}

// clean target: no findings, exit 0, full case count.
{
  const r = run("run", clean, "--seeds", seeds, "--cases", "500");
  check("clean exits 0", r.code === 0, `code=${r.code} stderr=${r.stderr}`);
  check("clean runs all 500 cases", r.stdout.includes("cases: 500, findings: 0"), r.stdout);
}

// bad target module (no fuzz export): exit 2.
{
  const r = run("run", noexport, "--seeds", seeds);
  check("target without fuzz export exits 2", r.code === 2, `code=${r.code}`);
  check("bad target message names the export", r.stderr.includes("does not export function fuzz"), r.stderr);
}

// missing seeds dir: exit 2.
{
  const r = run("run", crashy, "--seeds", join(fixtures, "no-such-dir"));
  check("missing seeds dir exits 2", r.code === 2, `code=${r.code}`);
}

// --keep-going: full 500 cases still run, exit 1 when findings exist.
{
  const r = run("run", crashy, "--seeds", seeds, "--seed", "1", "--cases", "500", "--keep-going");
  check("keep-going exits 1", r.code === 1, `code=${r.code}`);
  const m = r.stdout.match(/cases: (\d+), findings: (\d+)/);
  check("keep-going runs full 500 cases", m !== null && m[1] === "500", r.stdout);
  check("keep-going records findings", m !== null && Number(m[2]) >= 1, r.stdout);
}

// --out writes report.md + fuzz.json + finding-0.txt.
{
  const out = mkdtempSync(join(tmpdir(), "slimtoken-fuzz-selftest-"));
  try {
    const r = run("run", crashy, "--seeds", seeds, "--seed", "1", "--cases", "500", "--out", out);
    check("out run exits 1", r.code === 1, `code=${r.code}`);
    check("report.md written", existsSync(join(out, "report.md")));
    check("fuzz.json written", existsSync(join(out, "fuzz.json")));
    check("finding-0.txt written", existsSync(join(out, "finding-0.txt")));
    if (existsSync(join(out, "fuzz.json"))) {
      const j = JSON.parse(readFileSync(join(out, "fuzz.json"), "utf8"));
      check("fuzz.json ok=false", j.ok === false);
      check("fuzz.json has cases", typeof j.cases === "number" && j.cases >= 1);
      check("fuzz.json has findings array", Array.isArray(j.findings) && j.findings.length === 1, JSON.stringify(j.findings && j.findings.length));
      check("fuzz.json finding has inputFile", j.findings[0] && j.findings[0].inputFile === "finding-0.txt");
      check("fuzz.json finding has error", j.findings[0] && typeof j.findings[0].error === "string" && j.findings[0].error.includes("boom"));
      check("fuzz.json has summary", typeof j.summary === "string");
    }
    if (existsSync(join(out, "finding-0.txt"))) {
      const full = readFileSync(join(out, "finding-0.txt"), "utf8");
      check("finding-0.txt contains the crash token", full.includes("💥"));
    }
  } finally {
    rmSync(out, { recursive: true, force: true });
  }
}

console.log(`selftest: ${passed}/${passed + failed} passed`);
process.exit(failed === 0 ? 0 : 1);
