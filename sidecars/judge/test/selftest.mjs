// Selftest for the slimtoken-judge pack. Zero dependencies: starts a
// local node:http server on an ephemeral port (the judge model double,
// OpenAI-compatible), writes fixture twins to temp dirs, spawns the
// dispatcher as a child process, and asserts exit codes, WARN lines, and
// --out report shape. Async spawn on purpose: spawnSync would freeze this
// process's event loop, and this process hosts the mock judge server.
// Exit 0 only if every assertion holds.
import { spawn } from "node:child_process";
import { createServer } from "node:http";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";
import { mkdtempSync, mkdirSync, writeFileSync, readFileSync, existsSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { once } from "node:events";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const bin = join(root, "bin", "slimtoken-judge.mjs");

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

function run(args, envOverride) {
  return new Promise((resolve) => {
    const child = spawn(process.execPath, [bin, ...args], {
      windowsHide: true,
      env: envOverride ?? { ...process.env },
    });
    let stdout = "";
    let stderr = "";
    child.stdout.on("data", (chunk) => { stdout += chunk; });
    child.stderr.on("data", (chunk) => { stderr += chunk; });
    child.on("error", (error) => resolve({ status: -1, stdout, stderr: String(error) }));
    child.on("close", (status) => resolve({ status, stdout, stderr }));
  });
}

// --- mock judge model. One fixed steer answer; /v1f fences it, /v1g
// returns garbage. ---
const STEER = {
  verdict: "fail",
  reasons: ["fixture: the fuzz gate found findings"],
  steer: [{ objective: "fix the failing fuzz cases", priority: 1 }],
};
const GOOD_BODY = {
  id: "chatcmpl-mock",
  object: "chat.completion",
  model: "mock",
  choices: [{
    index: 0,
    message: { role: "assistant", content: JSON.stringify(STEER) },
    finish_reason: "stop",
  }],
  usage: { prompt_tokens: 120, completion_tokens: 30, total_tokens: 150 },
};
const server = createServer((req, res) => {
  res.writeHead(200, { "content-type": "application/json" });
  if (req.url === "/v1f/chat/completions") {
    res.end(JSON.stringify({
      ...GOOD_BODY,
      choices: [{ ...GOOD_BODY.choices[0], message: { role: "assistant", content: "```json\n" + JSON.stringify(STEER) + "\n```" } }],
    }));
  } else if (req.url === "/v1g/chat/completions") {
    res.end("this is not json at all");
  } else {
    res.end(JSON.stringify(GOOD_BODY));
  }
});
server.listen(0, "127.0.0.1");
await once(server, "listening");
const port = server.address().port;

// A port that is guaranteed closed, to play the "judge endpoint down" case.
const probe = createServer();
probe.listen(0, "127.0.0.1");
await once(probe, "listening");
const deadPort = probe.address().port;
probe.close();
await once(probe, "close");

const judgeEnv = (suffix) => ({
  ...process.env,
  JUDGE_BASE_URL: `http://127.0.0.1:${port}${suffix}`,
  JUDGE_API_KEY: "test",
  JUDGE_MODEL: "mock",
});
const noKeyEnv = (() => {
  const env = { ...process.env };
  delete env.JUDGE_BASE_URL;
  delete env.JUDGE_API_KEY;
  delete env.JUDGE_MODEL;
  return env;
})();

const tmp = mkdtempSync(join(tmpdir(), "slimtoken-judge-"));

// --- fixtures: one dir with two twins, one empty dir ---
const twinDir = join(tmp, "twins");
mkdirSync(twinDir, { recursive: true });
writeFileSync(join(twinDir, "test.json"), JSON.stringify({
  ok: true,
  summary: "test gate: 2/2 suites passed",
  suites: [{ stack: "node", command: "node --test", passed: 2, failed: 0 }],
}), "utf8");
writeFileSync(join(twinDir, "fuzz.json"), JSON.stringify({
  ok: false,
  summary: "2 finding(s) in 500 cases",
  cases: 500,
  findings: [
    { case: 3, error: "TypeError: cannot read length of undefined" },
    { case: 41, error: "RangeError: index out of bounds" },
  ],
}), "utf8");
const emptyDir = join(tmp, "empty");
mkdirSync(emptyDir, { recursive: true });

// --- brief ---
const briefOut = join(tmp, "brief-out");
let r = await run(["brief", twinDir, "--out", briefOut]);
assert("brief over two twins exits 0", r.status === 0, `got ${r.status}: ${r.stdout}${r.stderr}`);
const briefPath = join(briefOut, "brief.md");
let briefText = "";
try {
  briefText = readFileSync(briefPath, "utf8");
} catch (error) {
  assert("brief.md readable", false, error.message);
}
assert("brief.md names the test pack", briefText.includes("## test"), briefText);
assert("brief.md names the fuzz pack", briefText.includes("## fuzz"), briefText);
assert("brief.md carries the fuzz ok flag", briefText.includes("ok: false"), briefText);
assert("brief.md caps finding excerpts at 5 lines",
  (briefText.match(/^- /gm) || []).length <= 5, briefText);

r = await run(["brief", emptyDir, "--out", join(tmp, "empty-out")]);
assert("brief over empty dir exits 2", r.status === 2, `got ${r.status}: ${r.stdout}${r.stderr}`);

r = await run(["brief"]);
assert("brief no dirs exits 2", r.status === 2, `got ${r.status}`);

// --- judge: happy path ---
const judgeOut = join(tmp, "judge-out");
r = await run(["judge", "--brief", briefPath, "--out", judgeOut], judgeEnv("/v1"));
assert("judge with mock server exits 0", r.status === 0, `got ${r.status}: ${r.stdout}${r.stderr}`);
assert("judge --out writes report.md", existsSync(join(judgeOut, "report.md")));
assert("judge --out writes judge.json", existsSync(join(judgeOut, "judge.json")));
let steer = null;
try {
  steer = JSON.parse(readFileSync(join(judgeOut, "steer.json"), "utf8"));
} catch (error) {
  assert("steer.json parses", false, error.message);
}
if (steer) {
  assert("steer.json verdict parsed", steer.verdict === "fail", JSON.stringify(steer.verdict));
  assert("steer.json steer array present", Array.isArray(steer.steer) && steer.steer.length === 1, JSON.stringify(steer.steer));
  assert("steer.json carries model + usage", steer.model === "mock" && steer.usage !== null, JSON.stringify({ model: steer.model, usage: steer.usage }));
}

// --- judge: fenced JSON still parses ---
const fencedOut = join(tmp, "judge-fenced");
r = await run(["judge", "--brief", briefPath, "--out", fencedOut], judgeEnv("/v1f"));
assert("judge with fenced JSON exits 0", r.status === 0, `got ${r.status}: ${r.stdout}${r.stderr}`);
let fencedSteer = null;
try {
  fencedSteer = JSON.parse(readFileSync(join(fencedOut, "steer.json"), "utf8"));
} catch (error) {
  assert("fenced steer.json parses", false, error.message);
}
if (fencedSteer) {
  assert("fenced steer verdict parsed", fencedSteer.verdict === "fail", JSON.stringify(fencedSteer.verdict));
}

// --- judge: garbage reply ---
r = await run(["judge", "--brief", briefPath, "--out", join(tmp, "judge-garbage")], judgeEnv("/v1g"));
assert("judge with garbage reply exits 1", r.status === 1, `got ${r.status}: ${r.stdout}${r.stderr}`);
assert("judge garbage prints unparseable JSON", (r.stderr || "").includes("judge returned unparseable JSON"), r.stderr);

// --- judge: server down ---
r = await run(["judge", "--brief", briefPath, "--out", join(tmp, "judge-down")], {
  ...process.env,
  JUDGE_BASE_URL: `http://127.0.0.1:${deadPort}/v1`,
  JUDGE_API_KEY: "test",
  JUDGE_MODEL: "mock",
});
assert("judge with server down exits 1", r.status === 1, `got ${r.status}: ${r.stdout}${r.stderr}`);

// --- judge: missing env ---
r = await run(["judge", "--brief", briefPath, "--out", join(tmp, "judge-noenv")], noKeyEnv);
assert("judge missing env exits 2", r.status === 2, `got ${r.status}: ${r.stdout}${r.stderr}`);
assert("judge missing env names the var", (r.stderr || "").includes("JUDGE_API_KEY"), r.stderr);

// --- judge: budget cap ---
const tinyOut = join(tmp, "judge-tiny");
r = await run(["judge", "--brief", briefPath, "--out", tinyOut, "--max-input-bytes", "10"], judgeEnv("/v1"));
assert("judge with tiny --max-input-bytes exits 0", r.status === 0, `got ${r.status}: ${r.stdout}${r.stderr}`);
assert("judge tiny cap prints WARN", (r.stdout + r.stderr).includes("WARN"), r.stdout + r.stderr);
assert("judge tiny cap still writes steer.json", existsSync(join(tinyOut, "steer.json")));

// --- brief: budget cap drops oldest sections ---
const tinyBriefOut = join(tmp, "brief-tiny");
r = await run(["brief", twinDir, "--out", tinyBriefOut, "--max-input-bytes", "300"]);
assert("brief with small --max-input-bytes exits 0", r.status === 0, `got ${r.status}: ${r.stdout}${r.stderr}`);
assert("brief small cap prints WARN", (r.stdout + r.stderr).includes("WARN"), r.stdout + r.stderr);
let tinyBrief = "";
try {
  tinyBrief = readFileSync(join(tinyBriefOut, "brief.md"), "utf8");
} catch (error) {
  assert("small brief.md readable", false, error.message);
}
assert("brief small cap keeps exactly one pack",
  tinyBrief.includes("## test") !== tinyBrief.includes("## fuzz"), tinyBrief);
assert("brief small cap is under the cap", Buffer.byteLength(tinyBrief, "utf8") <= 300, String(Buffer.byteLength(tinyBrief, "utf8")));

// --- usage ---
r = await run([]);
assert("no args exits 2", r.status === 2, `got ${r.status}`);

r = await run(["bogus"]);
assert("unknown command exits 2", r.status === 2, `got ${r.status}`);

r = await run(["help"]);
assert("help exits 0", r.status === 0, `got ${r.status}: ${r.stderr}`);

server.close();
rmSync(tmp, { recursive: true, force: true });

console.log(`selftest: ${passed}/${passed + failed} passed`);
if (failed > 0) {
  console.error(`selftest: failures: ${failures.join(", ")}`);
  process.exit(1);
}
process.exit(0);
