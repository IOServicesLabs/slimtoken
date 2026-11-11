// Selftest for the slimtoken-security pack. Zero dependencies: starts a
// local node:http app (one endpoint that reflects the request body, one
// that ignores it), spawns the dispatcher as a child process, and asserts
// exit codes, finding lines, and the --out report files. Exit 0 only if
// every assertion holds.
// NOTE: the scan child talks to the in-process server below, so this
// selftest must use async spawn — spawnSync would block the event loop
// and the server could never answer.
import { spawn } from "node:child_process";
import http from "node:http";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";
import { existsSync, mkdtempSync, mkdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const bin = join(root, "bin", "slimtoken-security.mjs");
const fx = join(root, "fixtures");

let passed = 0;
let failed = 0;

function assert(name, condition, detail = "") {
  if (condition) {
    passed += 1;
    console.log(`ok   ${name}`);
  } else {
    failed += 1;
    console.log(`FAIL ${name}${detail ? ` — ${detail}` : ""}`);
  }
}

function run(args) {
  return new Promise((resolve) => {
    const child = spawn(process.execPath, [bin, ...args], { encoding: "utf8" });
    let stdout = "";
    let stderr = "";
    child.stdout.on("data", (d) => {
      stdout += d;
    });
    child.stderr.on("data", (d) => {
      stderr += d;
    });
    child.on("error", (error) => resolve({ status: -1, stdout, stderr: String(error) }));
    child.on("close", (status) => resolve({ status, stdout, stderr }));
  });
}

// The app under test, built in-process: /echo reflects the request body
// (vulnerable: attack markers WILL appear), /safe ignores the body.
const server = http.createServer((req, res) => {
  if (req.url.startsWith("/echo")) {
    let body = "";
    req.on("data", (d) => {
      body += d;
    });
    req.on("end", () => {
      res.writeHead(200, { "content-type": "text/plain" });
      res.end(body);
    });
    return;
  }
  res.writeHead(200, { "content-type": "application/json" });
  res.end(JSON.stringify({ ok: true }));
});

await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
const port = server.address().port;
const base = `http://127.0.0.1:${port}`;

// A port nothing listens on (listen, grab the port, close).
const dead = http.createServer();
await new Promise((resolve) => dead.listen(0, "127.0.0.1", resolve));
const deadPort = dead.address().port;
await new Promise((resolve) => dead.close(resolve));

const tmp = mkdtempSync(join(tmpdir(), "slimtoken-security-"));

try {
  // 1. Clean corpus against the app: no marker ever lands.
  let r = await run(["scan", base, "--corpus", join(fx, "corpus-clean")]);
  assert("clean corpus exits 0", r.status === 0, `got ${r.status}: ${r.stdout}${r.stderr}`);
  assert("clean corpus prints OK line", (r.stdout || "").includes("OK"), r.stdout);

  // 2. Attack corpus against the app: the echo endpoint reflects the body,
  //    so both markers land.
  r = await run(["scan", base, "--corpus", join(fx, "corpus-attack")]);
  assert("attack corpus exits 1", r.status === 1, `got ${r.status}: ${r.stdout}${r.stderr}`);
  assert("attack corpus names the finding case", (r.stdout || "").includes("reflect"), r.stdout);
  assert("attack corpus reports a marker finding", (r.stdout || "").includes("marker reflect"), r.stdout);
  assert("attack corpus reports the reflected marker", (r.stdout || "").includes("'<script>'"), r.stdout);

  // 3. Clean corpus against the vulnerable endpoint still passes: the
  //    inert marker is not in the reflected body.
  r = await run(["scan", base, "--corpus", join(fx, "corpus-clean")]);
  assert("clean corpus on echo endpoint exits 0", r.status === 0, `got ${r.status}: ${r.stdout}${r.stderr}`);

  // 4. Remote base URL without --allow-remote: refused before any fetch.
  r = await run(["scan", "http://example.com", "--corpus", join(fx, "corpus-clean")]);
  assert("remote URL without --allow-remote exits 2", r.status === 2, `got ${r.status}`);
  assert(
    "remote refusal message names the rule",
    ((r.stderr || "") + (r.stdout || "")).includes("refusing to scan remote host 'example.com'"),
    r.stderr
  );

  // 5. Every case unreachable (closed port): nothing was tested, fail closed.
  const deadCorpus = join(tmp, "corpus-dead");
  mkdirSync(deadCorpus, { recursive: true });
  writeFileSync(
    join(deadCorpus, "case.json"),
    JSON.stringify({ name: "closed", method: "GET", path: "/safe", markers: ["unused-marker-aa11"] })
  );
  r = await run(["scan", `http://127.0.0.1:${deadPort}`, "--corpus", deadCorpus, "--timeout", "3000"]);
  assert("all-unreachable exits 2", r.status === 2, `got ${r.status}: ${r.stdout}${r.stderr}`);
  assert("all-unreachable reports WARN UNREACHABLE", (r.stdout || "").includes("WARN UNREACHABLE"), r.stdout);
  assert("all-unreachable says nothing was tested", ((r.stderr || "") + (r.stdout || "")).includes("nothing was tested"), r.stderr);

  // 6. Missing corpus directory: usage error.
  r = await run(["scan", base, "--corpus", join(tmp, "no-such-dir")]);
  assert("missing corpus exits 2", r.status === 2, `got ${r.status}`);

  // 7-9. Usage surface.
  r = await run([]);
  assert("no args exits 2", r.status === 2, `got ${r.status}`);
  r = await run(["bogus"]);
  assert("unknown command exits 2", r.status === 2, `got ${r.status}`);
  r = await run(["help"]);
  assert("help exits 0", r.status === 0, `got ${r.status}: ${r.stderr}`);

  // 10. --out writes both judge files with stable fields.
  const outDir = join(tmp, "out");
  r = await run(["scan", base, "--corpus", join(fx, "corpus-attack"), "--out", outDir]);
  assert("scan with --out exits 1 (findings)", r.status === 1, `got ${r.status}`);
  assert("--out writes report.md", existsSync(join(outDir, "report.md")));
  assert("--out writes security.json", existsSync(join(outDir, "security.json")));
  let twin = null;
  try {
    twin = JSON.parse(readFileSync(join(outDir, "security.json"), "utf8"));
  } catch (error) {
    assert("security.json parses", false, error.message);
  }
  if (twin) {
    assert("security.json ok is false on findings", twin.ok === false, JSON.stringify(twin));
    const reflectCase = (twin.cases ?? []).find((c) => c.name === "reflect");
    assert("security.json carries the case", reflectCase !== undefined, JSON.stringify(twin));
    assert(
      "security.json records markersFound",
      Array.isArray(reflectCase.markersFound) && reflectCase.markersFound.includes("<script>"),
      JSON.stringify(reflectCase)
    );
    assert("security.json has a summary", typeof twin.summary === "string" && twin.summary.length > 0, JSON.stringify(twin));
    const reportMd = readFileSync(join(outDir, "report.md"), "utf8");
    assert("report.md names the marker finding", reportMd.includes("marker '<script>'"), reportMd);
  }
} finally {
  server.close();
  rmSync(tmp, { recursive: true, force: true });
}

console.log(`selftest: ${passed}/${passed + failed} passed`);
process.exit(failed === 0 ? 0 : 1);
