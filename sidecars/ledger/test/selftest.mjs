// selftest.mjs — zero-dependency selftest for the slimtoken-ledger pack.
//
// Spawns the dispatcher against fixtures and a TEMP ledger file under
// the OS temp dir (never the repo). Asserts:
//   add run-a = 0 and the row has arm + gates.test === true
//   add run-a with --tags records the tags
//   add a dir with no report.json = 2
//   add without --arm = 2
//   compare base vs regressed cur = 1 (wall up, gate false)
//   compare base vs fabricated improved arm = 0
//   compare with one arm = 2
//   compare with a missing ledger = 2
//   show = 0, show --arm = 0
//   compare --out writes report.md + ledger.json (twin.ok === false)
//   help = 0, no args = 2, unknown command = 2
// Prints "selftest: N/N passed"; exit 0 only when all pass.
import { spawnSync } from "node:child_process";
import { existsSync, mkdtempSync, mkdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const bin = join(root, "bin", "slimtoken-ledger.mjs");
const fixtures = join(root, "fixtures");

const scratch = mkdtempSync(join(tmpdir(), "slimtoken-ledger-selftest-"));
const ledger = join(scratch, "ledger.jsonl");
const improvedRun = join(scratch, "run-improved");
mkdirSync(improvedRun, { recursive: true });
writeFileSync(
  join(improvedRun, "report.json"),
  JSON.stringify({
    objective: "fabricated improved arm",
    elapsed_ms: 50000,
    total_input_tokens: 9000,
    total_output_tokens: 2000,
    tasks: { t1: { status: "succeeded" } },
  }),
);
writeFileSync(join(improvedRun, "test.json"), JSON.stringify({ ok: true, summary: "1/1" }));

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
  return spawnSync(process.execPath, [bin, ...args], { stdio: "pipe", timeout: 60_000 });
}

function ledgerRows() {
  return readFileSync(ledger, "utf8")
    .split(/\r?\n/)
    .filter((line) => line.trim() !== "")
    .map((line) => JSON.parse(line));
}

try {
  // add run-a: exit 0, row has arm + gates.test === true.
  const addA = cli("add", join(fixtures, "run-a"), "--arm", "base", "--ledger", ledger);
  check("add run-a exits 0", addA.status === 0, `got ${addA.status}: ${addA.stderr}`);
  const rowsAfterA = existsSync(ledger) ? ledgerRows() : [];
  const rowA = rowsAfterA[rowsAfterA.length - 1];
  check("add run-a appends a row with arm=base", rowA && rowA.arm === "base", `rows: ${rowsAfterA.length}`);
  check("add run-a row has gates.test === true", rowA && rowA.gates && rowA.gates.test === true, JSON.stringify(rowA && rowA.gates));
  check("add run-a row has wall_ms 60000", rowA && rowA.wall_ms === 60000, `got ${rowA && rowA.wall_ms}`);
  check("add run-a row counts tasks by status", rowA && rowA.tasks && rowA.tasks.succeeded === 4, JSON.stringify(rowA && rowA.tasks));

  // add with --tags records the tags.
  const addTags = cli("add", join(fixtures, "run-a"), "--arm", "tagged", "--ledger", ledger, "--tags", "engine=fixture,kind=selftest");
  check("add with --tags exits 0", addTags.status === 0, `got ${addTags.status}: ${addTags.stderr}`);
  const rowTagged = ledgerRows().find((row) => row.arm === "tagged");
  check("--tags row records tags", rowTagged && rowTagged.tags && rowTagged.tags.engine === "fixture" && rowTagged.tags.kind === "selftest", JSON.stringify(rowTagged && rowTagged.tags));

  // add a run dir with no report.json: exit 2 (fail closed).
  const addBroken = cli("add", join(fixtures, "run-broken"), "--arm", "broken", "--ledger", ledger);
  check("add run-broken (no report.json) exits 2", addBroken.status === 2, `got ${addBroken.status}`);

  // add without --arm: exit 2.
  const addNoArm = cli("add", join(fixtures, "run-a"), "--ledger", ledger);
  check("add without --arm exits 2", addNoArm.status === 2, `got ${addNoArm.status}`);

  // add run-b, then compare base vs cur: wall up, gate false -> 1.
  const addB = cli("add", join(fixtures, "run-b"), "--arm", "cur", "--ledger", ledger);
  check("add run-b exits 0", addB.status === 0, `got ${addB.status}: ${addB.stderr}`);
  const cmpRegress = cli("compare", "--ledger", ledger, "--arm", "base", "--arm", "cur");
  check("compare regressed B exits 1", cmpRegress.status === 1, `got ${cmpRegress.status}: ${cmpRegress.stdout}${cmpRegress.stderr}`);
  check("compare regressed B prints REGRESSED", /REGRESSED/.test(cmpRegress.stdout.toString()), cmpRegress.stdout.toString().trim());

  // compare base vs fabricated improved arm: faster wall, gate ok -> 0.
  const addImproved = cli("add", improvedRun, "--arm", "cur2", "--ledger", ledger);
  check("add improved run exits 0", addImproved.status === 0, `got ${addImproved.status}: ${addImproved.stderr}`);
  const cmpImprove = cli("compare", "--ledger", ledger, "--arm", "base", "--arm", "cur2");
  check("compare improved B exits 0", cmpImprove.status === 0, `got ${cmpImprove.status}: ${cmpImprove.stdout}${cmpImprove.stderr}`);

  // latest row wins: re-add arm cur with the improved run dir, compare flips to 0.
  const addCurLatest = cli("add", improvedRun, "--arm", "cur", "--ledger", ledger);
  check("re-add arm cur exits 0", addCurLatest.status === 0, `got ${addCurLatest.status}: ${addCurLatest.stderr}`);
  const cmpLatest = cli("compare", "--ledger", ledger, "--arm", "base", "--arm", "cur");
  check("compare uses latest row per arm", cmpLatest.status === 0, `got ${cmpLatest.status}: ${cmpLatest.stdout}${cmpLatest.stderr}`);

  // usage errors.
  const cmpOneArm = cli("compare", "--ledger", ledger, "--arm", "base");
  check("compare with one arm exits 2", cmpOneArm.status === 2, `got ${cmpOneArm.status}`);

  const cmpMissing = cli("compare", "--ledger", join(scratch, "no-such-ledger.jsonl"), "--arm", "base", "--arm", "cur");
  check("compare with missing ledger exits 2", cmpMissing.status === 2, `got ${cmpMissing.status}`);

  // show.
  const show = cli("show", "--ledger", ledger);
  check("show exits 0", show.status === 0, `got ${show.status}: ${show.stderr}`);
  check("show prints rows", /base/.test(show.stdout.toString()) && /cur/.test(show.stdout.toString()), show.stdout.toString().trim());
  const showArm = cli("show", "--ledger", ledger, "--arm", "base");
  check("show --arm exits 0", showArm.status === 0, `got ${showArm.status}: ${showArm.stderr}`);

  // compare --out writes report.md + ledger.json; the twin parses.
  const outDir = join(scratch, "compare-out");
  const cmpOut = cli("compare", "--ledger", ledger, "--arm", "base", "--arm", "tagged", "--out", outDir);
  // base vs tagged is a tie (same run dir): exit 0, twin ok === true.
  check("compare --out exits 0 on tie", cmpOut.status === 0, `got ${cmpOut.status}: ${cmpOut.stdout}${cmpOut.stderr}`);
  check("--out writes report.md", existsSync(join(outDir, "report.md")));
  check("--out writes ledger.json", existsSync(join(outDir, "ledger.json")));
  let twinOk = null;
  try {
    twinOk = JSON.parse(readFileSync(join(outDir, "ledger.json"), "utf8")).ok;
  } catch (error) {
    twinOk = `unparseable: ${error.message}`;
  }
  check("ledger.json parses with ok === true", twinOk === true, `got ${twinOk}`);

  // A regressed compare writes ok === false in the twin. Re-add arm cur
  // from run-b first so its latest row is the regressed one.
  const outDirFail = join(scratch, "compare-out-fail");
  const addCurBack = cli("add", join(fixtures, "run-b"), "--arm", "cur", "--ledger", ledger);
  check("re-add arm cur from run-b exits 0", addCurBack.status === 0, `got ${addCurBack.status}: ${addCurBack.stderr}`);
  const cmpOutFail = cli("compare", "--ledger", ledger, "--arm", "base", "--arm", "cur", "--out", outDirFail);
  check("compare --out regressed exits 1", cmpOutFail.status === 1, `got ${cmpOutFail.status}`);
  let twinFailOk = null;
  try {
    twinFailOk = JSON.parse(readFileSync(join(outDirFail, "ledger.json"), "utf8")).ok;
  } catch (error) {
    twinFailOk = `unparseable: ${error.message}`;
  }
  check("regressed ledger.json parses with ok === false", twinFailOk === false, `got ${twinFailOk}`);
  check("regressed --out writes report.md", existsSync(join(outDirFail, "report.md")));

  // help / no args / unknown.
  const help = cli("help");
  check("help exits 0", help.status === 0, `got ${help.status}`);
  check("help prints usage", /usage:/.test(help.stdout.toString()), help.stdout.toString().trim());

  const noArgs = cli();
  check("no args exits 2", noArgs.status === 2, `got ${noArgs.status}`);

  const unknown = cli("frobnicate");
  check("unknown command exits 2", unknown.status === 2, `got ${unknown.status}`);
} finally {
  rmSync(scratch, { recursive: true, force: true });
}

const total = passed + failed;
if (failed > 0) {
  for (const f of failures) console.log(`selftest FAIL: ${f}`);
}
console.log(`selftest: ${passed}/${total} passed`);
process.exit(failed === 0 ? 0 : 1);
