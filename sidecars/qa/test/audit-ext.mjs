// audit-ext.mjs — selftest for the two opt-in audit extensions of the
// smoke gate: --axe (deep-a11y, host-supplied axe-core bundle) and
// --devices (device-matrix golden mode). Serves a tiny static fixture over
// a throwaway node http server, runs lib/qa-smoke.mjs against it with a
// fake axe bundle and two fake devices, and asserts exit codes plus the
// new smoke.json fields. Zero dependencies; Chrome must be installed.
import { spawn } from "node:child_process";
import { createServer } from "node:http";
import { cpSync, existsSync, mkdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const qaSmoke = join(here, "..", "lib", "qa-smoke.mjs");
const axeFail = join(here, "fixtures", "axe-fail.js");
const axeClean = join(here, "fixtures", "axe-clean.js");
const devicesFile = join(here, "fixtures", "devices.json");
const fixtureDir = join(tmpdir(), `qa-audit-ext-fix-${process.pid}`);
const work = join(tmpdir(), `qa-audit-ext-${process.pid}`);

// The fixture page is written per phase so the route name (and therefore
// the screenshot stem and per-device baseline names) stays identical while
// the rendered content changes.
const PAGE_BASE = `<!doctype html>
<html lang="en">
<head><meta charset="utf-8"><title>audit-ext fixture</title>
<style>body { font-family: sans-serif; background: #ffffff; color: #222222; }</style>
</head>
<body>
<h1>Audit fixture</h1>
<p>Static page for the audit extension selftest.</p>
</body>
</html>
`;
const PAGE_CHANGED = PAGE_BASE.replace("Audit fixture", "Changed heading")
  .replace("<h1>", '<h1 style="color:#ff0000">');

let failures = 0;
function check(name, cond, detail = "") {
  if (cond) {
    console.log(`PASS ${name}`);
  } else {
    failures += 1;
    console.log(`FAIL ${name}${detail ? ` — ${detail}` : ""}`);
  }
}

function writePage(html) {
  writeFileSync(join(fixtureDir, "index.html"), html);
}

function readSmokeJson(outDir) {
  const p = join(outDir, "smoke.json");
  if (!existsSync(p)) return null;
  return JSON.parse(readFileSync(p, "utf8"));
}

function runSmoke(port, extraArgs, outDir) {
  return new Promise((resolve) => {
    const child = spawn(process.execPath, [
      qaSmoke,
      "--url", `http://127.0.0.1:${serverPort}`,
      "--routes", "/index.html",
      "--out", outDir,
      "--port", String(port),
      "--settle", "600",
      "--floor", "250",
      "--quiet", "80",
      ...extraArgs,
    ], { stdio: ["ignore", "pipe", "pipe"] });
    let log = "";
    child.stdout.on("data", (d) => { log += d; });
    child.stderr.on("data", (d) => { log += d; });
    const guard = setTimeout(() => { child.kill("SIGKILL"); }, 150000);
    child.on("close", (code) => {
      clearTimeout(guard);
      resolve({ code: code ?? -1, log });
    });
  });
}

mkdirSync(fixtureDir, { recursive: true });
mkdirSync(work, { recursive: true });

const server = createServer((req, res) => {
  let path = (req.url || "/").split("?")[0];
  if (path === "/") path = "/index.html";
  try {
    const body = readFileSync(join(fixtureDir, path));
    res.writeHead(200, { "content-type": "text/html; charset=utf-8" });
    res.end(body);
  } catch {
    res.writeHead(404);
    res.end("not found");
  }
});
let serverPort = 0;
await new Promise((resolve) => {
  server.listen(0, "127.0.0.1", resolve);
});
serverPort = server.address().port;

try {
  // --axe: a serious-impact violation fails the route (exit 1) and lands
  // in the new smoke.json axe field.
  writePage(PAGE_BASE);
  {
    const out = join(work, "axe-fail");
    const r = await runSmoke(9461, ["--axe", axeFail], out);
    check("axe-serious-fails", r.code === 1, `exit ${r.code}, want 1; log: ${r.log.slice(-400)}`);
    const j = readSmokeJson(out);
    const axe = j?.routes[0].axe;
    check("axe-serious-json", axe && axe.fails === 1 && axe.violations === 1
      && axe.rules?.[0]?.ruleId === "color-contrast" && axe.rules?.[0]?.nodes === 1,
    JSON.stringify(axe));
    check("axe-route-not-ok", j && j.routes[0].ok === false, `ok=${j?.routes[0].ok}`);
  }

  // --axe: an empty violation list passes (exit 0).
  {
    const out = join(work, "axe-clean");
    const r = await runSmoke(9462, ["--axe", axeClean], out);
    check("axe-clean-passes", r.code === 0, `exit ${r.code}, want 0; log: ${r.log.slice(-400)}`);
    const j = readSmokeJson(out);
    const axe = j?.routes[0].axe;
    check("axe-clean-json", axe && axe.violations === 0 && axe.fails === 0 && axe.warns === 0,
      JSON.stringify(axe));
  }

  // --axe: a missing bundle fails closed with exit 2.
  {
    const out = join(work, "axe-missing");
    const r = await runSmoke(9463, ["--axe", join(here, "fixtures", "no-such-axe.js")], out);
    check("axe-missing-bundle-exit2", r.code === 2, `exit ${r.code}, want 2; log: ${r.log.slice(-300)}`);
  }

  // --devices: a missing device file fails closed with exit 2.
  {
    const out = join(work, "devices-missing");
    const r = await runSmoke(9464, ["--devices", join(here, "fixtures", "no-such-devices.json")], out);
    check("devices-missing-file-exit2", r.code === 2, `exit ${r.code}, want 2; log: ${r.log.slice(-300)}`);
  }

  // --devices --golden-update: captures per-device baselines, route passes.
  const gold = join(work, "gold");
  const shotStem = "01-index-html";
  {
    const out = join(work, "devices-update");
    const r = await runSmoke(9465, ["--devices", devicesFile, "--golden", gold, "--golden-update"], out);
    check("devices-golden-update-passes", r.code === 0, `exit ${r.code}, want 0; log: ${r.log.slice(-400)}`);
    check("devices-baselines-captured",
      existsSync(join(gold, `${shotStem}__dphone-6-1in.png`))
      && existsSync(join(gold, `${shotStem}__dphone-6-1in.styles.json`))
      && existsSync(join(gold, `${shotStem}__dtablet-10in.png`))
      && existsSync(join(gold, `${shotStem}__dtablet-10in.styles.json`)),
      "expected NN-route__d<device>.png/.styles.json next to the variant baselines");
    const j = readSmokeJson(out);
    const devices = (j?.routes[0].variants || []).filter((v) => v.kind === "device");
    check("devices-variant-json", devices.length === 2 && devices.every((v) => v.golden?.updated === true),
      JSON.stringify((j?.routes[0].variants || []).map((v) => v.kind)));
    check("devices-sanitized-labels",
      devices.some((v) => v.label === "phone-6-1in") && devices.some((v) => v.label === "tablet-10in"),
      devices.map((v) => v.label).join(","));
  }

  // --devices without update: a missing per-device baseline fails the route.
  {
    const goldPartial = join(work, "gold-partial");
    rmSync(goldPartial, { recursive: true, force: true });
    cpSync(gold, goldPartial, { recursive: true });
    rmSync(join(goldPartial, `${shotStem}__dtablet-10in.png`));
    rmSync(join(goldPartial, `${shotStem}__dtablet-10in.styles.json`));
    const out = join(work, "devices-missing-baseline");
    const r = await runSmoke(9466, ["--devices", devicesFile, "--golden", goldPartial], out);
    check("devices-missing-baseline-fails", r.code === 1, `exit ${r.code}, want 1; log: ${r.log.slice(-400)}`);
  }

  // --devices without update: a changed page fails the pixel/style diff.
  {
    writePage(PAGE_CHANGED);
    const out = join(work, "devices-diff");
    const r = await runSmoke(9467, ["--devices", devicesFile, "--golden", gold], out);
    check("devices-diff-fails", r.code === 1, `exit ${r.code}, want 1; log: ${r.log.slice(-400)}`);
  }
} finally {
  server.close();
  rmSync(fixtureDir, { recursive: true, force: true });
}

console.log(failures === 0
  ? "audit-ext: all checks passed"
  : `audit-ext: ${failures} check(s) FAILED`);
process.exit(failures === 0 ? 0 : 1);
