// Selftest for the slimtoken-research pack. Zero dependencies: starts a
// local node:http server on an ephemeral port (the citation sources),
// writes fixture reports to temp files, spawns the dispatcher as a child
// process, and asserts exit codes, output lines, and --out report shape.
// Exit 0 only if every assertion holds.
import { spawn } from "node:child_process";
import { createServer } from "node:http";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";
import { mkdtempSync, mkdirSync, writeFileSync, readFileSync, existsSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { once } from "node:events";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const bin = join(root, "bin", "slimtoken-research.mjs");

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

// Async on purpose: spawnSync would freeze this process's event loop,
// and this process hosts the fixture HTTP server the children fetch.
function run(args) {
  return new Promise((resolve) => {
    const child = spawn(process.execPath, [bin, ...args], { windowsHide: true });
    let stdout = "";
    let stderr = "";
    child.stdout.on("data", (chunk) => { stdout += chunk; });
    child.stderr.on("data", (chunk) => { stderr += chunk; });
    child.on("error", (error) => resolve({ status: -1, stdout, stderr: String(error) }));
    child.on("close", (status) => resolve({ status, stdout, stderr }));
  });
}

// --- Local citation source. /fact-a and /mirror carry the same fact so a
// consensus claim can cite two distinct hostnames (127.0.0.1 vs localhost)
// against one server. ---
const FACT = "the release train leaves on tuesdays";
const server = createServer((req, res) => {
  if (req.url === "/fact-a" || req.url === "/mirror") {
    res.writeHead(200, { "content-type": "text/html" });
    res.end(`<html><body><p>${FACT}</p></body></html>`);
  } else {
    res.writeHead(404, { "content-type": "text/plain" });
    res.end("not found");
  }
});
// "::" is dual-stack on Windows: both localhost and 127.0.0.1 reach it.
server.listen(0, "::");
await once(server, "listening");
const port = server.address().port;
const LOCALHOST = `http://localhost:${port}`;
const LOOPBACK = `http://127.0.0.1:${port}`;

// A port that is guaranteed closed, to play the "server stopped" host.
const probe = createServer();
probe.listen(0, "127.0.0.1");
await once(probe, "listening");
const deadPort = probe.address().port;
probe.close();
await once(probe, "close");
const DEAD = `http://127.0.0.1:${deadPort}`;

const tmp = mkdtempSync(join(tmpdir(), "slimtoken-research-"));
function fixture(name, text) {
  const file = join(tmp, name);
  writeFileSync(file, text, "utf8");
  return file;
}

// --- verify fixtures ---
let r = await run(["verify", fixture("good.md", `The schedule is fixed. [${FACT}](${LOCALHOST}/fact-a)`)]);
assert("good report exits 0", r.status === 0, `got ${r.status}: ${r.stdout}${r.stderr}`);
assert("good report prints VERIFIED", (r.stdout || "").includes("VERIFIED"), r.stdout);

r = await run(["verify", fixture("hallucinated.md", `A fact from nowhere. [the moon is made of green cheese](${LOCALHOST}/fact-a)`)]);
assert("hallucinated claim exits 1", r.status === 1, `got ${r.status}: ${r.stdout}${r.stderr}`);
assert("hallucinated claim reports not found", (r.stdout || "").includes("claim not found in source"), r.stdout);

r = await run(["verify", fixture("dead-link.md", `A cited page that is gone. [${FACT}](${LOCALHOST}/missing)`)]);
assert("404 citation exits 1", r.status === 1, `got ${r.status}: ${r.stdout}${r.stderr}`);
assert("404 citation reports citation does not exist", (r.stdout || "").includes("citation does not exist"), r.stdout);

r = await run(["verify", fixture("unfetchable.md",
  `One good claim. [${FACT}](${LOCALHOST}/fact-a)\n\nOne dead host. [${FACT}](${DEAD}/fact-a)`)]);
assert("unfetchable host with good claims exits 0", r.status === 0, `got ${r.status}: ${r.stdout}${r.stderr}`);
assert("unfetchable host prints WARN UNFETCHABLE", (r.stdout || "").includes("WARN UNFETCHABLE"), r.stdout);

r = await run(["verify", fixture("consensus-one-host.md",
  `Sources agree. [${FACT} (consensus)](${LOCALHOST}/fact-a)`)]);
assert("consensus with one hostname exits 1", r.status === 1, `got ${r.status}: ${r.stdout}${r.stderr}`);
assert("consensus shortfall reported", (r.stdout || "").includes("needs >= 2"), r.stdout);

r = await run(["verify", fixture("consensus-two-hosts.md",
  `Sources agree. [${FACT} (consensus)](${LOCALHOST}/fact-a) and [${FACT} (consensus)](${LOOPBACK}/mirror)`)]);
assert("consensus with two hostnames exits 0", r.status === 0, `got ${r.status}: ${r.stdout}${r.stderr}`);

r = await run(["verify", fixture("paraphrase.md",
  `Restated. [the release train departs on tuesdays (paraphrase)](${LOCALHOST}/fact-a)`)]);
assert("paraphrase with passing overlap exits 0", r.status === 0, `got ${r.status}: ${r.stdout}${r.stderr}`);
assert("paraphrase prints PARTIAL", (r.stdout || "").includes("PARTIAL"), r.stdout);

// --- verify usage + judge output ---
r = await run(["verify"]);
assert("verify no args exits 2", r.status === 2, `got ${r.status}`);

r = await run(["bogus"]);
assert("unknown command exits 2", r.status === 2, `got ${r.status}`);

r = await run(["help"]);
assert("help exits 0", r.status === 0, `got ${r.status}: ${r.stderr}`);

const outDir = join(tmp, "verify-out");
r = await run(["verify", join(tmp, "good.md"), "--out", outDir]);
assert("verify --out exits 0", r.status === 0, `got ${r.status}: ${r.stdout}${r.stderr}`);
assert("verify --out writes report.md", existsSync(join(outDir, "report.md")));
assert("verify --out writes research.json", existsSync(join(outDir, "research.json")));
let research = null;
try {
  research = JSON.parse(readFileSync(join(outDir, "research.json"), "utf8"));
} catch (error) {
  assert("research.json parses", false, error.message);
}
if (research) {
  assert("research.ok is true", research.ok === true, JSON.stringify(research.ok));
  assert("research claims carry the verified claim",
    Array.isArray(research.claims) &&
      research.claims.some((c) => c.status === "VERIFIED" && c.claim.includes("release train")),
    JSON.stringify(research.claims));
}

// --- brief ---
const questions = join(tmp, "questions.txt");
writeFileSync(questions, [
  "# research questions for the wave",
  "",
  "What day does the release train leave?",
  "  ",
  "Who owns the release calendar?",
  "# trailing comment",
  "Which tickets ride the train?",
  "",
].join("\n"), "utf8");
const planFile = join(tmp, "plan.json");
r = await run(["brief", questions, "--out", planFile]);
assert("brief exits 0", r.status === 0, `got ${r.status}: ${r.stdout}${r.stderr}`);
let plan = null;
try {
  plan = JSON.parse(readFileSync(planFile, "utf8"));
} catch (error) {
  assert("plan.json parses", false, error.message);
}
if (plan) {
  assert("plan has one task per question",
    Array.isArray(plan.tasks) && plan.tasks.length === 3, JSON.stringify(plan.tasks && plan.tasks.length));
  assert("plan task ids are research-<n>",
    plan.tasks.every((t, i) => t.id === `research-${i + 1}`),
    JSON.stringify(plan.tasks.map((t) => t.id)));
  const prompt = plan.tasks.length > 0 ? plan.tasks[0].prompt : "";
  assert("prompt demands inline [claim text](url) citations", prompt.includes("[claim text](url)"), prompt);
  assert("prompt demands consensus marks", prompt.includes("(consensus)"), prompt);
  assert("prompt demands opening every cited source", /[Nn]ever cite a source you did not open/.test(prompt), prompt);
  assert("prompt demands a 10-line brief", prompt.includes("10-line brief"), prompt);
}

r = await run(["brief", join(tmp, "no-such-file.txt"), "--out", planFile]);
assert("brief missing questions file exits 2", r.status === 2, `got ${r.status}`);

r = await run(["brief", questions]);
assert("brief without --out exits 2", r.status === 2, `got ${r.status}`);

// --- collect ---
const runDir = join(tmp, "run-output");
mkdirSync(runDir, { recursive: true });
writeFileSync(join(runDir, "research-1.md"),
  `Brief one. [${FACT}](${LOCALHOST}/fact-a)`, "utf8");
writeFileSync(join(runDir, "research-2.md"),
  `Brief two. [${FACT}](${LOCALHOST}/fact-a)\n\nAnother cite. [${FACT}](${LOOPBACK}/mirror)`, "utf8");
const collectedFile = join(tmp, "report.md");
r = await run(["collect", runDir, "--out", collectedFile]);
assert("collect exits 0", r.status === 0, `got ${r.status}: ${r.stdout}${r.stderr}`);
let collected = "";
try {
  collected = readFileSync(collectedFile, "utf8");
} catch (error) {
  assert("collected report readable", false, error.message);
}
assert("collect merges both briefs", collected.includes("Brief one.") && collected.includes("Brief two."), collected);
assert("collect writes a Sources section", collected.includes("## Sources"), collected);
const sourcesTail = collected.split("## Sources")[1] || "";
const factUrlCount = sourcesTail.split(`${LOCALHOST}/fact-a`).length - 1;
assert("collect dedupes sources by URL", factUrlCount === 1, collected);

r = await run(["collect", join(tmp, "empty-run"), "--out", collectedFile]);
assert("collect missing run dir exits 2", r.status === 2, `got ${r.status}`);

// A round trip: the collected report passes the gate.
r = await run(["verify", collectedFile]);
assert("collected report passes verify", r.status === 0, `got ${r.status}: ${r.stdout}${r.stderr}`);

server.close();
rmSync(tmp, { recursive: true, force: true });

console.log(`selftest: ${passed}/${passed + failed} passed`);
if (failed > 0) {
  console.error(`selftest: failures: ${failures.join(", ")}`);
  process.exit(1);
}
process.exit(0);
