// contrast.mjs — selftest for the smoke gate's contrast audit. Serves pages
// over a throwaway node http server, runs lib/qa-smoke.mjs against each and
// reads smoke.json. Zero dependencies; Chrome must be installed.
//
// 2026-11-16, production: white text on a blue button was measured against
// the white page behind the button (1.0:1) and failed a page that read
// perfectly. The text's own element's background counts, semi-transparent
// layers blend, the root's background counts (dark pages), and colours in
// any CSS syntax are read as they paint. A real white-on-white still fails.
import { spawn } from "node:child_process";
import { createServer } from "node:http";
import { existsSync, mkdirSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const qaSmoke = join(here, "..", "lib", "qa-smoke.mjs");
const work = join(tmpdir(), `qa-contrast-${process.pid}`);
mkdirSync(work, { recursive: true });

const page = (body, rootStyle = "") => `<!doctype html><html lang="en" style="${rootStyle}"><head><meta charset="utf-8"><title>contrast</title>
<style>body{margin:0;font:16px sans-serif} .chip{display:inline-block;padding:6px 10px}</style></head><body>${body}</body></html>`;

const PAGES = {
  // Everything here reads well: none may be a failure.
  "/good.html": page(`
    <p style="color:#222">Plain text on the page.</p>
    <button class="cite" style="background:#2557d6;color:#fff;border:0;padding:4px 8px">1</button>
    <span class="chip" style="background:rgba(0,0,0,.85);color:#fff">dark glass</span>
    <span class="chip" style="background:oklch(0.45 0.15 260);color:oklch(0.98 0 0)">oklch</span>
    <span class="chip" style="background:rgb(20 40 160 / 1);color:rgb(255 255 255)">space syntax</span>`),
  // A dark page: light text on the root's own dark background.
  "/dark.html": page(`<p style="color:#eee">Light text on a dark page.</p>`, "background:#111"),
  // A real failure: white on white.
  "/bad.html": page(`<p style="color:#fff">Invisible words.</p><p style="color:#222">Visible words.</p>`),
};

const server = createServer((req, res) => {
  const body = PAGES[(req.url || "/").split("?")[0]];
  if (!body) { res.writeHead(404).end("not found"); return; }
  res.writeHead(200, { "content-type": "text/html; charset=utf-8" }).end(body);
});
await new Promise((r) => server.listen(0, "127.0.0.1", r));
const base = `http://127.0.0.1:${server.address().port}`;

function smoke(route, port) {
  const out = join(work, route.replace(/\W/g, ""));
  return new Promise((resolve) => {
    const child = spawn(process.execPath, [qaSmoke, "--url", base, "--routes", route, "--out", out, "--port", String(port), "--settle", "400", "--floor", "200", "--quiet", "80"], { stdio: ["ignore", "pipe", "pipe"] });
    let log = "";
    child.stdout.on("data", (d) => { log += d; });
    child.stderr.on("data", (d) => { log += d; });
    const guard = setTimeout(() => child.kill("SIGKILL"), 120000);
    child.on("close", () => {
      clearTimeout(guard);
      const p = join(out, "smoke.json");
      const json = existsSync(p) ? JSON.parse(readFileSync(p, "utf8")) : null;
      const r = json && (json.routes || [])[0];
      resolve({ log, vis: r && r.visibility, ok: r && r.ok });
    });
  });
}

let failures = 0;
const check = (name, cond, detail = "") => {
  console.log(`${cond ? "PASS" : "FAIL"} ${name}${!cond && detail ? ` — ${detail}` : ""}`);
  if (!cond) failures += 1;
};

try {
  const good = await smoke("/good.html", 9471);
  check("good page measured", !!good.vis, good.log.slice(-400));
  check("white on a blue button, glass, oklch and space syntax all read", good.vis && good.vis.contrastFailures.length === 0, JSON.stringify(good.vis && good.vis.contrastFailures));
  check("none of them is even a warning", good.vis && good.vis.contrastWarnings.length === 0, JSON.stringify(good.vis && good.vis.contrastWarnings));

  const dark = await smoke("/dark.html", 9472);
  check("light text on a dark root passes", dark.vis && dark.vis.contrastFailures.length === 0 && dark.vis.contrastWarnings.length === 0, JSON.stringify(dark.vis));

  const bad = await smoke("/bad.html", 9473);
  const fails = (bad.vis && bad.vis.contrastFailures) || [];
  check("white on white is still a failure", fails.length === 1 && fails[0].text === "Invisible words.", JSON.stringify(fails));
} finally {
  server.close();
  rmSync(work, { recursive: true, force: true });
}
console.log(failures ? `${failures} failed` : "contrast: all passed");
process.exit(failures ? 1 : 0);
