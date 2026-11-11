// slimtoken-loop selftest. Zero tokens: builds a temp workspace per case,
// uses a fake engine (a plain node script) and node one-liner gates.
//
//   node test/selftest.mjs
//
// Prints "selftest: N/N passed"; exits 0 only when every case passes.
import { spawnSync } from "node:child_process";
import { existsSync, mkdtempSync, readFileSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const BIN = resolve(fileURLToPath(new URL("../bin/slimtoken-loop.mjs", import.meta.url)));

// A fake engine: writes <output>/plan.json and <output>/report.json, then
// exits 0 — or exits 1 when --fail is anywhere in its argv.
const FAKE_ENGINE = `import { mkdirSync, writeFileSync } from "node:fs";
const args = process.argv.slice(2);
const out = args[args.indexOf("--output") + 1];
mkdirSync(out, { recursive: true });
writeFileSync(out + "/plan.json", JSON.stringify({ ok: true }));
writeFileSync(out + "/report.json", JSON.stringify({ ok: true }));
if (args.includes("--fail")) process.exit(1);
`;

const GATE_PASS = 'node -e "process.exit(0)"';
const GATE_FAIL = 'node -e "process.exit(1)"';
// Writes into the output dir, proving {{output}} expansion.
const GATE_EXPAND =
  'node -e "const fs=require(\'node:fs\');const p=require(\'node:path\');' +
  'fs.mkdirSync(p.dirname(process.argv[1]),{recursive:true});' +
  'fs.writeFileSync(process.argv[1],\'expanded-ok\')" {{output}}/expanded.txt';

let passed = 0;
const cases = [];
function case_(name, fn) {
  cases.push({ name, fn });
}

function workspace() {
  return mkdtempSync(join(tmpdir(), "slimtoken-loop-test-"));
}

function runCli(args, cwd) {
  return spawnSync(process.execPath, [BIN, ...args], { cwd, encoding: "utf8" });
}

function writeConfig(dir, { engine, gates, plan = "plan.json", output = "run-output" }) {
  const gateText = gates
    .map((g) => `[[gates]]\nname = "${g.name}"\ncmd = '${g.cmd}'\n`)
    .join("");
  writeFileSync(
    join(dir, "loop.toml"),
    `plan = "${plan}"\nengine = '${engine}'\noutput = "${output}"\n${gateText}`,
  );
  return join(dir, "loop.toml");
}

function fakeEngineCmd(dir, extra = "") {
  writeFileSync(join(dir, "slimtoken-fake.mjs"), FAKE_ENGINE);
  return `node ${join(dir, "slimtoken-fake.mjs")}${extra}`;
}

// 1. All gates pass: exit 0, loop.json ok:true, {{output}} expanded.
case_("all-pass", () => {
  const dir = workspace();
  const config = writeConfig(dir, {
    engine: fakeEngineCmd(dir),
    gates: [
      { name: "pass1", cmd: GATE_PASS },
      { name: "expand", cmd: GATE_EXPAND },
      { name: "pass2", cmd: GATE_PASS },
    ],
  });
  const res = runCli(["run", "--config", config, "--out", "verdict"], dir);
  if (res.status !== 0) return `exit ${res.status}, want 0`;
  const loopJson = JSON.parse(readFileSync(join(dir, "verdict", "loop.json"), "utf8"));
  if (loopJson.ok !== true) return "loop.json ok is not true";
  if (loopJson.gates.length !== 3) return `gates length ${loopJson.gates.length}, want 3`;
  if (!loopJson.gates.every((g) => g.exit === 0)) return "a passing gate recorded exit != 0";
  if (!existsSync(join(dir, "verdict", "report.md"))) return "report.md missing";
  const expanded = readFileSync(join(dir, "run-output", "expanded.txt"), "utf8");
  if (expanded !== "expanded-ok") return `{{output}} expansion wrote '${expanded}'`;
  return null;
});

// 2. A gate fails: exit 1, the failing gate's exit recorded.
case_("failing-gate", () => {
  const dir = workspace();
  const config = writeConfig(dir, {
    engine: fakeEngineCmd(dir),
    gates: [
      { name: "ok-gate", cmd: GATE_PASS },
      { name: "bad-gate", cmd: GATE_FAIL },
    ],
  });
  const res = runCli(["run", "--config", config, "--out", "verdict"], dir);
  if (res.status !== 1) return `exit ${res.status}, want 1`;
  const loopJson = JSON.parse(readFileSync(join(dir, "verdict", "loop.json"), "utf8"));
  if (loopJson.ok !== false) return "loop.json ok is not false";
  const bad = loopJson.gates.find((g) => g.name === "bad-gate");
  if (!bad || bad.exit !== 1) return "failing gate exit not recorded as 1";
  const good = loopJson.gates.find((g) => g.name === "ok-gate");
  if (!good || good.exit !== 0) return "passing gate exit not recorded as 0";
  return null;
});

// 3. The engine fails: exit 1, gates never ran (gates array empty).
case_("failing-engine", () => {
  const dir = workspace();
  const config = writeConfig(dir, {
    engine: fakeEngineCmd(dir, " --fail"),
    gates: [{ name: "never-runs", cmd: GATE_FAIL }],
  });
  const res = runCli(["run", "--config", config, "--out", "verdict"], dir);
  if (res.status !== 1) return `exit ${res.status}, want 1`;
  const loopJson = JSON.parse(readFileSync(join(dir, "verdict", "loop.json"), "utf8"));
  if (loopJson.ok !== false) return "loop.json ok is not false";
  if (!Array.isArray(loopJson.gates) || loopJson.gates.length !== 0) {
    return "gates ran on a broken engine run";
  }
  if (loopJson.engine.exit !== 1) return `engine exit ${loopJson.engine.exit}, want 1`;
  return null;
});

// 4. Missing config: exit 2.
case_("missing-config", () => {
  const dir = workspace();
  const res = runCli(["run", "--config", join(dir, "nope.toml")], dir);
  if (res.status !== 2) return `exit ${res.status}, want 2`;
  return null;
});

// 5. Engine not on PATH: exit 2.
case_("engine-not-found", () => {
  const dir = workspace();
  const config = writeConfig(dir, {
    engine: "definitely-not-a-real-binary-xyz",
    gates: [{ name: "irrelevant", cmd: GATE_PASS }],
  });
  const res = runCli(["run", "--config", config], dir);
  if (res.status !== 2) return `exit ${res.status}, want 2`;
  return null;
});

// 6. init writes loop.toml.
case_("init-writes", () => {
  const dir = workspace();
  const res = runCli(["init", "--dir", dir], dir);
  if (res.status !== 0) return `exit ${res.status}, want 0`;
  const toml = join(dir, "loop.toml");
  if (!existsSync(toml)) return "loop.toml not written";
  const text = readFileSync(toml, "utf8");
  for (const needle of ['plan = "plan.json"', "engine = ", 'output = "run-output"', "[[gates]]"]) {
    if (!text.includes(needle)) return `template missing '${needle}'`;
  }
  return null;
});

// 7. init refuses to overwrite: exit 2.
case_("init-overwrite-refused", () => {
  const dir = workspace();
  const first = runCli(["init", "--dir", dir], dir);
  if (first.status !== 0) return `first init exit ${first.status}, want 0`;
  const second = runCli(["init", "--dir", dir], dir);
  if (second.status !== 2) return `second init exit ${second.status}, want 2`;
  if (!String(second.stderr).includes("refusing to overwrite")) return "no refusal message";
  return null;
});

// 8. Unknown command: exit 2.
case_("unknown-command", () => {
  const res = runCli(["frobnicate"], tmpdir());
  if (res.status !== 2) return `exit ${res.status}, want 2`;
  return null;
});

// 9. help: exit 0.
case_("help", () => {
  const res = runCli(["help"], tmpdir());
  if (res.status !== 0) return `exit ${res.status}, want 0`;
  if (!String(res.stdout).includes("usage:")) return "help has no usage text";
  return null;
});

// 10. No args: exit 2.
case_("no-args", () => {
  const res = runCli([], tmpdir());
  if (res.status !== 2) return `exit ${res.status}, want 2`;
  return null;
});

for (const { name, fn } of cases) {
  try {
    const error = fn();
    if (error === null) {
      passed += 1;
      console.log(`ok   ${name}`);
    } else {
      console.log(`FAIL ${name}: ${error}`);
    }
  } catch (error) {
    console.log(`FAIL ${name}: threw ${error.message}`);
  }
}

console.log(`selftest: ${passed}/${cases.length} passed`);
process.exit(passed === cases.length ? 0 : 1);
