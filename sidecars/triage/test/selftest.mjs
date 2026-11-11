#!/usr/bin/env node
// slimtoken-triage selftest: a local mock chat/completions server plus
// the CLI dispatcher, asserting the pack contract end to end.
//
//   node test/selftest.mjs
//
// The mock returns a fixed completion with three candidates whose repros
// are: exit 1 (confirms), exit 0 (rejected), and a command that does not
// exist (spawn/shell error — rejected with the error recorded, no crash).
// Async spawn throughout: spawnSync would freeze this process, and the
// mock server lives in this process. Zero dependencies; Node 22+.
import { spawn } from "node:child_process";
import { createServer } from "node:http";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";
import { readFileSync, writeFileSync, rmSync, mkdirSync } from "node:fs";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const bin = join(root, "bin", "slimtoken-triage.mjs");
const fixtures = join(root, "test", "fixtures");
const outRoot = join(root, "test", ".out");

let passed = 0;
let failed = 0;

function expect(name, cond, detail = "") {
  if (cond) {
    passed += 1;
    console.log(`PASS  ${name}`);
  } else {
    failed += 1;
    console.log(`FAIL  ${name}${detail ? ` -- ${detail}` : ""}`);
  }
}

// Async spawn of the dispatcher. `env` is merged over a copy of
// process.env, so callers can add or remove TRIAGE_* keys per child.
function run(args, env = {}) {
  return new Promise((resolveP) => {
    const child = spawn(process.execPath, [bin, ...args], {
      env: { ...process.env, ...env },
      cwd: root,
    });
    let stdout = "";
    let stderr = "";
    child.stdout.on("data", (d) => { stdout += d; });
    child.stderr.on("data", (d) => { stderr += d; });
    child.on("close", (code) => resolveP({ code, stdout, stderr }));
  });
}

function readJson(path) {
  try {
    return JSON.parse(readFileSync(path, "utf8"));
  } catch {
    return undefined;
  }
}

// --- mock chat/completions server -------------------------------------

let mode = "ok";
const mockCandidates = {
  candidates: [
    { name: "exit-nonzero", hypothesis: "the repro command fails as expected", repro: 'node -e "process.exit(1)"' },
    { name: "exit-zero", hypothesis: "a passing check demonstrates nothing", repro: 'node -e "process.exit(0)"' },
    { name: "no-such-command", hypothesis: "the tool is missing", repro: "definitely-not-a-command-xyz" },
  ],
};
const server = createServer((req, res) => {
  if (req.url !== "/v1/chat/completions") {
    res.writeHead(404).end();
    return;
  }
  req.on("data", () => {});
  req.on("end", () => {
    res.writeHead(200, { "content-type": "application/json" });
    if (mode === "garbage") {
      res.end(JSON.stringify({ choices: [{ message: { content: "this is not json at all" } }], usage: { total_tokens: 2 } }));
      return;
    }
    res.end(JSON.stringify({
      choices: [{ message: { content: JSON.stringify(mockCandidates) } }],
      model: "mock",
      usage: { prompt_tokens: 10, completion_tokens: 20, total_tokens: 30 },
    }));
  });
});
await new Promise((resolveListen) => server.listen(0, "127.0.0.1", resolveListen));
const port = server.address().port;
const mockEnv = {
  TRIAGE_BASE_URL: `http://127.0.0.1:${port}/v1`,
  TRIAGE_API_KEY: "test",
  TRIAGE_MODEL: "mock",
};
const cleanEnv = Object.fromEntries(
  Object.entries(process.env).filter(([k]) => !k.startsWith("TRIAGE_")),
);

rmSync(outRoot, { recursive: true, force: true });
mkdirSync(outRoot, { recursive: true });

// 1. candidates over the fixture failure report: exit 0, 3 candidates.
const outCandidates = join(outRoot, "candidates-ok");
let result = await run(["candidates", join(fixtures, "failure.md"), "--out", outCandidates], mockEnv);
expect("candidates over failure.md exits 0", result.code === 0, `got ${result.code}: ${result.stderr.trim()}`);
const candidatesFile = readJson(join(outCandidates, "candidates.json"));
expect(
  "candidates.json has 3 candidates",
  Array.isArray(candidatesFile?.candidates) && candidatesFile.candidates.length === 3,
  JSON.stringify(candidatesFile?.candidates?.length),
);

// 2. candidates with the mock returning garbage: exit 1.
mode = "garbage";
result = await run(["candidates", join(fixtures, "failure.md"), "--out", join(outRoot, "candidates-garbage")], mockEnv);
expect("candidates with garbage response exits 1", result.code === 1, `got ${result.code}`);
mode = "ok";

// 3. candidates with the server down: exit 1 (transport error).
result = await run(
  ["candidates", join(fixtures, "failure.md"), "--out", join(outRoot, "candidates-down")],
  { ...mockEnv, TRIAGE_BASE_URL: "http://127.0.0.1:1/v1" },
);
expect("candidates with server down exits 1", result.code === 1, `got ${result.code}: ${result.stderr.trim()}`);

// 4. candidates with a missing env var: exit 2, names the var.
result = await run(["candidates", join(fixtures, "failure.md")], cleanEnv);
expect("candidates with missing env exits 2", result.code === 2, `got ${result.code}`);
expect("missing env message names TRIAGE_BASE_URL", /TRIAGE_BASE_URL/.test(result.stderr), result.stderr.trim());

// 5. repro over the mock-generated candidates.json: exit 0, exactly 1
//    confirmed (the exit-1 one); the not-a-command one is rejected.
const outRepro = join(outRoot, "repro-ok");
result = await run(["repro", join(outCandidates, "candidates.json"), "--out", outRepro]);
expect("repro over mock candidates exits 0", result.code === 0, `got ${result.code}: ${result.stderr.trim()}`);
const triage = readJson(join(outRepro, "triage.json"));
expect(
  "triage.json has exactly 1 confirmed (exit-nonzero)",
  Array.isArray(triage?.confirmed) && triage.confirmed.length === 1 && triage.confirmed[0].name === "exit-nonzero",
  JSON.stringify(triage?.confirmed?.map((c) => c.name)),
);
const rejectedNames = (triage?.rejected ?? []).map((r) => r.name);
expect(
  "no-such-command recorded as rejected",
  rejectedNames.includes("no-such-command"),
  JSON.stringify(rejectedNames),
);
expect(
  "confirmed.json exists and matches triage.json",
  readJson(join(outRepro, "confirmed.json"))?.confirmed?.length === 1,
);

// 6. repro where no candidate confirms (all exit 0): exit 1.
const allPassFile = join(outRoot, "all-pass.json");
writeFileSync(
  allPassFile,
  JSON.stringify({
    candidates: [
      { name: "passes-1", hypothesis: "h", repro: 'node -e "process.exit(0)"' },
      { name: "passes-2", hypothesis: "h", repro: 'node -e "process.exit(0)"' },
    ],
  }),
);
result = await run(["repro", allPassFile]);
expect("repro with no confirmed exits 1", result.code === 1, `got ${result.code}`);

// 7. repro over malformed JSON: exit 2.
const malformedFile = join(outRoot, "malformed.json");
writeFileSync(malformedFile, "this is {{{ not json");
result = await run(["repro", malformedFile]);
expect("repro with malformed JSON exits 2", result.code === 2, `got ${result.code}`);

// 8. candidates with a missing failure file: exit 2.
result = await run(["candidates", join(fixtures, "no-such-file.md")], mockEnv);
expect("candidates with missing failure file exits 2", result.code === 2, `got ${result.code}`);

// 9. no args: exit 2. 10. unknown command: exit 2. 11. help: exit 0.
result = await run([], mockEnv);
expect("no args exits 2", result.code === 2, `got ${result.code}`);
result = await run(["bogus"], mockEnv);
expect("unknown command exits 2", result.code === 2, `got ${result.code}`);
result = await run(["help"], mockEnv);
expect("help exits 0", result.code === 0, `got ${result.code}`);

server.close();
console.log(`selftest: ${passed}/${passed + failed} passed`);
process.exit(failed === 0 ? 0 : 1);
