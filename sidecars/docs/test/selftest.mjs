#!/usr/bin/env node
// slimtoken-docs selftest: spawn the dispatcher and assert the gate
// contract end to end. Zero dependencies; Node 22+.
//
//   node test/selftest.mjs
//
// Asserts: good fixture exits 0, bad fixture exits 1, empty dir exits 2,
// check with no args exits 2, unknown command exits 2, help exits 0, and
// --out writes report.md + docs.json with a matching `ok` field.
import { spawnSync } from "node:child_process";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";
import { existsSync, readFileSync, rmSync } from "node:fs";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const bin = join(root, "bin", "slimtoken-docs.mjs");
const fixtures = join(root, "fixtures");
const outDir = join(root, "test", ".out");

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

function run(...args) {
  return spawnSync(process.execPath, [bin, ...args], { encoding: "utf8" });
}

function readOk() {
  try {
    return JSON.parse(readFileSync(join(outDir, "docs.json"), "utf8")).ok;
  } catch {
    return undefined;
  }
}

rmSync(outDir, { recursive: true, force: true });

// 1. Good fixture: every check passes.
let result = run("check", join(fixtures, "good"));
expect("good fixture exits 0", result.status === 0, `got ${result.status}`);
expect(
  "good fixture prints OK lines",
  /^OK /m.test(result.stdout),
  result.stdout.trim(),
);

// 2. Bad fixture: hard errors, exit 1.
result = run("check", join(fixtures, "bad"));
expect("bad fixture exits 1", result.status === 1, `got ${result.status}`);
expect("bad fixture reports ERROR lines", /^ERROR /m.test(result.stdout));
expect(
  "bad fixture flags the three hard defects",
  /broken link/.test(result.stdout)
    && /broken anchor/.test(result.stdout)
    && /unclosed code fence/.test(result.stdout),
  result.stdout.trim(),
);
expect(
  "bad fixture reports the long sentence as WARN only",
  /WARN .*sentence with \d+ words/.test(result.stdout),
  result.stdout.trim(),
);

// 3. Empty dir: nothing to check, exit 2.
result = run("check", join(fixtures, "empty"));
expect("empty dir exits 2", result.status === 2, `got ${result.status}`);

// 4. check with no args: usage error, exit 2.
result = run("check");
expect("check with no args exits 2", result.status === 2, `got ${result.status}`);

// 5. Unknown command: exit 2.
result = run("frobnicate");
expect("unknown command exits 2", result.status === 2, `got ${result.status}`);

// 6. help: exit 0.
result = run("help");
expect("help exits 0", result.status === 0, `got ${result.status}`);

// 7. --out on the good fixture: report.md + docs.json, ok true.
result = run("check", join(fixtures, "good"), "--out", outDir);
expect("--out good exits 0", result.status === 0, `got ${result.status}`);
expect("report.md written", existsSync(join(outDir, "report.md")));
expect("docs.json written", existsSync(join(outDir, "docs.json")));
expect("docs.json ok true for good", readOk() === true);

// 8. --out on the bad fixture: exit 1, ok false.
rmSync(outDir, { recursive: true, force: true });
result = run("check", join(fixtures, "bad"), "--out", outDir);
expect("--out bad exits 1", result.status === 1, `got ${result.status}`);
expect("docs.json ok false for bad", readOk() === false);

// 9. Front matter rules: valid front matter passes; a README with front
// matter missing `description` fails; `api: true` tables are checked.
result = run("check", join(fixtures, "fm-ok"));
expect("fm-ok fixture exits 0", result.status === 0, `got ${result.status}`);

result = run("check", join(fixtures, "fm-bad"));
expect("fm-bad fixture exits 1", result.status === 1, `got ${result.status}`);
expect(
  "fm-bad flags the missing description key",
  /README\.md needs a description key/.test(result.stdout),
  result.stdout.trim(),
);

result = run("check", join(fixtures, "api-bad"));
expect("api-bad fixture exits 1", result.status === 1, `got ${result.status}`);
expect(
  "api-bad flags the row cell count",
  /table row has 1 cells, header has 2/.test(result.stdout),
  result.stdout.trim(),
);

result = run("check", join(fixtures, "fm-malformed"));
expect("fm-malformed fixture exits 1", result.status === 1, `got ${result.status}`);
expect(
  "fm-malformed flags the bad front matter",
  /bad front matter/.test(result.stdout),
  result.stdout.trim(),
);

rmSync(outDir, { recursive: true, force: true });

console.log(`selftest: ${passed}/${passed + failed} passed`);
process.exit(failed > 0 ? 1 : 0);
