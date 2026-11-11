// Selftest for the slimtoken-types pack. Zero dependencies: spawns the
// dispatcher as a child process and asserts exit codes, finding text, and
// the --out report files. The tsc fixture is asserted only when tsc is on
// PATH; otherwise the case prints SKIP and is never faked. Exit 0 only if
// every assertion holds.
import { spawnSync } from "node:child_process";
import { fileURLToPath, pathToFileURL } from "node:url";
import { dirname, join } from "node:path";
import { existsSync, mkdtempSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const bin = join(root, "bin", "slimtoken-types.mjs");
const fx = join(root, "fixtures");

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

function run(args) {
  return spawnSync(process.execPath, [bin, ...args], { encoding: "utf8" });
}

// tsc probe, same way the pack probes it (npm shim needs a shell).
function tscOnPath() {
  const line = ['"tsc"', '"--version"'].join(" ");
  const r = spawnSync(line, { shell: true, encoding: "utf8", windowsHide: true });
  return r.status === 0;
}

const openapi = join(fx, "openapi.json");

// 1. TS bindings covering all three schemas, with a tsconfig: pass.
let r = run(["check", openapi, join(fx, "ts-ok")]);
assert("ts-ok exits 0", r.status === 0, `got ${r.status}: ${r.stdout}\n${r.stderr}`);
assert("ts-ok reports spec ok", (r.stdout || "").includes("schema(s): Widget, WidgetList, Error"), r.stdout);

// 2. TS bindings missing 'Error': exit 1 and the missing schema is named.
r = run(["check", openapi, join(fx, "ts-missing")]);
assert("ts-missing exits 1", r.status === 1, `got ${r.status}`);
assert("ts-missing names the missing schema", (r.stdout || "").includes("no binding for schema 'Error'"), r.stdout);

// 3. Python bindings covering all three schemas: pass.
r = run(["check", openapi, join(fx, "py-ok")]);
assert("py-ok exits 0", r.status === 0, `got ${r.status}: ${r.stdout}\n${r.stderr}`);

// 4. Python bindings with a syntax error: exit 1.
r = run(["check", openapi, join(fx, "py-bad")]);
assert("py-bad exits 1", r.status === 1, `got ${r.status}: ${r.stdout}`);
assert("py-bad reports a compile error", (r.stdout || "").includes("ERROR"), r.stdout);

// 5. Spec with a dangling local $ref: exit 1.
r = run(["check", join(fx, "dangling-ref.json"), join(fx, "ts-ok")]);
assert("dangling-ref spec exits 1", r.status === 1, `got ${r.status}`);
assert("dangling-ref reports the dangling $ref",
  (r.stdout || "").includes("dangling $ref: #/components/schemas/Missing"), r.stdout);

// 6. Plain schema-map spec (no OpenAPI wrapper): pass.
r = run(["check", join(fx, "plain-schemas.json"), join(fx, "ts-ok")]);
assert("plain schema-map spec exits 0", r.status === 0, `got ${r.status}: ${r.stdout}\n${r.stderr}`);

// 7. Valid spec with zero schemas: WARN SKIP, pass.
r = run(["check", join(fx, "no-schemas.json"), join(fx, "ts-ok")]);
assert("no-schemas spec exits 0", r.status === 0, `got ${r.status}`);
assert("no-schemas spec warns skip", (r.stdout || "").includes("no schemas found in spec"), r.stdout);

// 8-9. Unparseable spec and missing spec: usage errors (exit 2).
r = run(["check", join(fx, "broken.json"), join(fx, "ts-ok")]);
assert("unparseable spec exits 2", r.status === 2, `got ${r.status}`);

r = run(["check", join(fx, "does-not-exist.json"), join(fx, "ts-ok")]);
assert("missing spec exits 2", r.status === 2, `got ${r.status}`);

// 10. Empty bindings dir: usage error (exit 2).
r = run(["check", openapi, join(fx, "empty-bindings")]);
assert("empty bindings dir exits 2", r.status === 2, `got ${r.status}`);

// 11. --lang py against a TS-only bindings dir: usage error (exit 2).
r = run(["check", openapi, join(fx, "ts-ok"), "--lang", "py"]);
assert("forced lang with no matching files exits 2", r.status === 2, `got ${r.status}`);

// 12-14. Usage errors and help.
r = run([]);
assert("no args exits 2", r.status === 2, `got ${r.status}`);

r = run(["bogus"]);
assert("unknown command exits 2", r.status === 2, `got ${r.status}`);

r = run(["help"]);
assert("help exits 0", r.status === 0, `got ${r.status}: ${r.stderr}`);

// 15. tsc compile finding: asserted only when tsc is on PATH.
if (tscOnPath()) {
  r = run(["check", openapi, join(fx, "ts-tsc"), "--lang", "ts"]);
  assert("tsc fixture exits 1", r.status === 1, `got ${r.status}: ${r.stdout}`);
  assert("tsc fixture reports a compile error", (r.stdout || "").includes("compile"), r.stdout);
  assert("tsc fixture bindings all present",
    !(r.stdout || "").includes("no binding for schema"), r.stdout);
} else {
  console.log("SKIP tsc not on PATH — ts-tsc case skipped (never faked)");
}

// 16. --out writes report.md and types.json with matching ok.
const outDir = mkdtempSync(join(tmpdir(), "slimtoken-types-"));
r = run(["check", openapi, join(fx, "ts-ok"), "--out", outDir]);
const reportMd = join(outDir, "report.md");
const typesJson = join(outDir, "types.json");
assert("--out exits 0", r.status === 0, `got ${r.status}`);
assert("--out writes report.md", existsSync(reportMd));
assert("--out writes types.json", existsSync(typesJson));
let types = null;
try {
  types = JSON.parse(readFileSync(typesJson, "utf8"));
} catch (error) {
  assert("--out types.json parses", false, error.message);
}
if (types) {
  assert("--out types.ok is true", types.ok === true, JSON.stringify(types.ok));
  assert("--out types.schemas has all three",
    Array.isArray(types.schemas) && types.schemas.length === 3, JSON.stringify(types.schemas));
  assert("--out types.checks carry kinds and statuses",
    Array.isArray(types.checks) && types.checks.length > 0 &&
      types.checks.every((c) => typeof c.kind === "string" && typeof c.target === "string" && typeof c.status === "string"),
    JSON.stringify(types.checks));
}
rmSync(outDir, { recursive: true, force: true });

console.log(`selftest: ${passed}/${passed + failed} passed`);
if (failed > 0) {
  console.error(`selftest: failures: ${failures.join(", ")}`);
  process.exit(1);
}
process.exit(0);
