#!/usr/bin/env node
// slimtoken-vision selftest: an in-process mock vision server plus async
// spawns of the real CLI. ASYNC ON PURPOSE: spawnSync would freeze this
// process while a child waits on the mock server, and the mock server's
// event loop would never answer (this exact trap hit sibling packs).
//
//   node test/selftest.mjs
//
// Asserts: 2 good PNGs exit 0 with meanScore 7; a corrupt PNG is WARN SKIP
// but good images still score (exit 0); only-corrupt dir exits 1;
// --max-images 1 scores exactly 1; missing env / missing rubric / empty
// dir exit 2; garbage judge JSON is WARN SKIP with good images still
// scoring; server down exits 1; no args / unknown exit 2; help exits 0.
import { spawn } from "node:child_process";
import http from "node:http";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";
import { mkdtempSync, mkdirSync, writeFileSync, readFileSync, rmSync, existsSync } from "node:fs";
import { tmpdir } from "node:os";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const bin = join(root, "bin", "slimtoken-vision.mjs");

// A real 1x1 PNG (valid 8-byte signature). Written to temp files, not a
// text file with a .png name.
const PNG_1X1 =
  "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg==";
// A second, distinct valid-signature PNG: the mock server returns garbage
// JSON for this one, to prove an unparseable judge answer is a WARN SKIP
// and never fails the run.
const PNG_POISON = Buffer.concat([Buffer.from(PNG_1X1, "base64"), Buffer.from("poison!")]).toString("base64");

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

function run(args, env) {
  return new Promise((resolve, reject) => {
    const child = spawn(process.execPath, [bin, ...args], { env });
    let stdout = "";
    let stderr = "";
    child.stdout.on("data", (d) => (stdout += d));
    child.stderr.on("data", (d) => (stderr += d));
    child.on("error", reject);
    child.on("close", (code) => resolve({ status: code, stdout, stderr }));
  });
}

// Mock vision server: OpenAI-compatible chat/completions. In "poison"
// mode it returns garbage for the poison image only.
let mode = "good";
const server = http.createServer((req, res) => {
  if (req.method !== "POST" || !req.url.endsWith("/chat/completions")) {
    res.writeHead(404, { "content-type": "application/json" });
    res.end('{"error":"not found"}');
    return;
  }
  let body = "";
  req.on("data", (c) => (body += c));
  req.on("end", () => {
    const poisoned = mode === "poison" && body.includes(PNG_POISON);
    const content = poisoned
      ? "this is not json at all"
      : JSON.stringify({ score: 7, flags: ["spacing"], note: "ok" });
    res.writeHead(200, { "content-type": "application/json" });
    res.end(JSON.stringify({ choices: [{ message: { role: "assistant", content } }] }));
  });
});
await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
const port = server.address().port;

// Fixtures in a temp dir.
const tmp = mkdtempSync(join(tmpdir(), "slimtoken-vision-selftest-"));
const goodDir = join(tmp, "good");
const nestedDir = join(goodDir, "nested");
const mixedDir = join(tmp, "mixed");
const corruptDir = join(tmp, "corrupt");
const poisonDir = join(tmp, "poison");
const emptyDir = join(tmp, "empty");
const outDir = join(tmp, "out");
mkdirSync(nestedDir, { recursive: true });
mkdirSync(mixedDir, { recursive: true });
mkdirSync(corruptDir, { recursive: true });
mkdirSync(poisonDir, { recursive: true });
mkdirSync(emptyDir, { recursive: true });
writeFileSync(join(goodDir, "a.png"), Buffer.from(PNG_1X1, "base64"));
writeFileSync(join(nestedDir, "b.png"), Buffer.from(PNG_1X1, "base64"));
writeFileSync(join(mixedDir, "a.png"), Buffer.from(PNG_1X1, "base64"));
writeFileSync(join(mixedDir, "b.png"), Buffer.from(PNG_1X1, "base64"));
writeFileSync(join(mixedDir, "bad.png"), "this is not a png, just text");
writeFileSync(join(corruptDir, "bad1.png"), "not a png either");
writeFileSync(join(corruptDir, "bad2.png"), "still not a png");
writeFileSync(join(poisonDir, "a.png"), Buffer.from(PNG_1X1, "base64"));
writeFileSync(join(poisonDir, "b.png"), Buffer.from(PNG_1X1, "base64"));
writeFileSync(join(poisonDir, "c-poison.png"), Buffer.from(PNG_POISON, "base64"));
const rubricPath = join(tmp, "rubric.txt");
writeFileSync(rubricPath, "Score brand adherence 1-10; flag off-palette colors; check spacing rhythm.\n");

const env = {
  ...process.env,
  VISION_BASE_URL: `http://127.0.0.1:${port}/v1`,
  VISION_API_KEY: "test",
  VISION_MODEL: "mock",
};

// A port that is guaranteed closed: listen, grab the port, close.
const dead = http.createServer();
await new Promise((resolve) => dead.listen(0, "127.0.0.1", resolve));
const deadPort = dead.address().port;
await new Promise((resolve) => dead.close(resolve));
const envDead = { ...env, VISION_BASE_URL: `http://127.0.0.1:${deadPort}/v1` };

let result;

// 1. Two good PNGs (one nested): exit 0, mean score 7 in vision.json.
result = await run(["score", goodDir, "--rubric", rubricPath, "--out", outDir], env);
expect("good dir exits 0", result.status === 0, `got ${result.status} -- ${result.stderr.trim()}`);
expect("good dir prints OK lines", (result.stdout.match(/^OK /gm) || []).length === 2, result.stdout.trim());
expect("report.md written", existsSync(join(outDir, "report.md")));
expect("vision.json written", existsSync(join(outDir, "vision.json")));
let vision = null;
try {
  vision = JSON.parse(readFileSync(join(outDir, "vision.json"), "utf8"));
} catch {
  vision = null;
}
expect("vision.json meanScore is 7", vision?.summary?.meanScore === 7, JSON.stringify(vision?.summary));
expect("vision.json ok true", vision?.ok === true);
expect("vision.json scored 2", vision?.summary?.scored === 2);

// 2. Corrupt PNG next to good ones: WARN SKIP, still exit 0.
result = await run(["score", mixedDir, "--rubric", rubricPath], env);
expect("mixed dir exits 0", result.status === 0, `got ${result.status} -- ${result.stderr.trim()}`);
expect(
  "corrupt png is WARN SKIP",
  /WARN SKIP .*bad\.png: not a valid PNG/.test(result.stdout),
  result.stdout.trim(),
);
expect("mixed dir still scores 2", (result.stdout.match(/^OK /gm) || []).length === 2, result.stdout.trim());

// 3. Only corrupt images: nothing scored, exit 1.
result = await run(["score", corruptDir, "--rubric", rubricPath], env);
expect("only-corrupt dir exits 1", result.status === 1, `got ${result.status}`);
expect("only-corrupt dir WARN SKIPs every file", (result.stdout.match(/^WARN SKIP /gm) || []).length === 2);

// 4. --max-images 1 scores exactly 1.
result = await run(["score", mixedDir, "--rubric", rubricPath, "--max-images", "1"], env);
expect("--max-images 1 exits 0", result.status === 0, `got ${result.status}`);
expect("--max-images 1 scores exactly 1", (result.stdout.match(/^OK /gm) || []).length === 1, result.stdout.trim());
expect("--max-images 1 warns about the cap", /WARN cap:/.test(result.stdout), result.stdout.trim());

// 5. Missing env: exit 2 naming the variable.
const envMissing = { ...env };
delete envMissing.VISION_MODEL;
result = await run(["score", goodDir, "--rubric", rubricPath], envMissing);
expect("missing env exits 2", result.status === 2, `got ${result.status}`);
expect("missing env names the var", result.stderr.includes("VISION_MODEL"), result.stderr.trim());

// 6. Missing rubric: exit 2.
result = await run(["score", goodDir, "--rubric", join(tmp, "no-such-rubric.txt")], env);
expect("missing rubric exits 2", result.status === 2, `got ${result.status}`);

// 7. Empty dir: no images, exit 2.
result = await run(["score", emptyDir, "--rubric", rubricPath], env);
expect("empty dir exits 2", result.status === 2, `got ${result.status}`);

// 8. Garbage judge JSON: WARN SKIP that image, good images still exit 0.
mode = "poison";
result = await run(["score", poisonDir, "--rubric", rubricPath], env);
mode = "good";
expect("garbage judge JSON exits 0 with good images", result.status === 0, `got ${result.status} -- ${result.stdout.trim()}`);
expect(
  "poisoned image is WARN SKIP",
  /WARN SKIP .*c-poison\.png/.test(result.stdout),
  result.stdout.trim(),
);
expect("poison dir still scores 2", (result.stdout.match(/^OK /gm) || []).length === 2, result.stdout.trim());

// 9. Server down: every image fails, nothing scored, exit 1.
result = await run(["score", goodDir, "--rubric", rubricPath], envDead);
expect("server down exits 1", result.status === 1, `got ${result.status}`);

// 10. Oversize images skipped by --max-image-bytes.
result = await run(["score", goodDir, "--rubric", rubricPath, "--max-image-bytes", "10"], env);
expect("oversize cap with all skipped exits 1", result.status === 1, `got ${result.status}`);
expect(
  "oversize files are WARN SKIP with byte reason",
  /WARN SKIP .*bytes over --max-image-bytes/.test(result.stdout),
  result.stdout.trim(),
);

// 11. Usage surface.
result = await run([], env);
expect("no args exits 2", result.status === 2, `got ${result.status}`);
result = await run(["frobnicate"], env);
expect("unknown command exits 2", result.status === 2, `got ${result.status}`);
result = await run(["help"], env);
expect("help exits 0", result.status === 0, `got ${result.status}`);

server.close();
rmSync(tmp, { recursive: true, force: true });

console.log(`selftest: ${passed}/${passed + failed} passed`);
process.exit(failed > 0 ? 1 : 0);
