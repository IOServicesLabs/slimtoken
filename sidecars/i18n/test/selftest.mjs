// Selftest for the slimtoken-i18n pack. Zero dependencies: spawns the
// dispatcher as a child process and asserts exit codes, finding text, and
// the --out report files. Exit 0 only if every assertion holds.
import { spawnSync } from "node:child_process";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";
import { existsSync, mkdtempSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const bin = join(root, "bin", "slimtoken-i18n.mjs");
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

// 1. Good fixture (nested locales/ layout): fully translated, matching
//    placeholders ({icu}, %s, {{mustache}}) -> exit 0.
let r = run(["check", join(fx, "locales-good")]);
assert("good locales exit 0", r.status === 0, `got ${r.status}: ${r.stderr}`);
assert("good locales reports OK per locale", (r.stdout || "").includes("OK   fr"), r.stdout);

// 2. Missing key in fr -> exit 1, key named.
r = run(["check", join(fx, "locales-missing")]);
assert("missing-key locales exit 1", r.status === 1, `got ${r.status}`);
assert("missing-key names the key", (r.stdout || "").includes("missing key 'b' in fr"), r.stdout);

// 3. Placeholder mismatch -> exit 1, mismatch named with both sets.
r = run(["check", join(fx, "locales-placeholder")]);
assert("placeholder locales exit 1", r.status === 1, `got ${r.status}`);
assert("placeholder names the mismatch",
  (r.stdout || "").includes("placeholder mismatch for 'greet': fr has {name}, source has {user}"),
  r.stdout);

// 4. Untranslated value (identical to source) -> WARN only, exit 0.
r = run(["check", join(fx, "locales-untranslated")]);
assert("untranslated locales exit 0", r.status === 0, `got ${r.status}: ${r.stderr}`);
assert("untranslated prints WARN", (r.stdout || "").includes("WARN") &&
  (r.stdout || "").includes("untranslated 'b' in fr"), r.stdout);

// 5. Empty value -> exit 1, key named.
r = run(["check", join(fx, "locales-empty")]);
assert("empty-value locales exit 1", r.status === 1, `got ${r.status}`);
assert("empty-value names the key", (r.stdout || "").includes("empty value for 'b' in fr"), r.stdout);

// 6-8. Usage errors and help.
r = run([]);
assert("no args exits 2", r.status === 2, `got ${r.status}`);

r = run(["bogus"]);
assert("unknown command exits 2", r.status === 2, `got ${r.status}`);

r = run(["help"]);
assert("help exits 0", r.status === 0, `got ${r.status}: ${r.stderr}`);

// 9-10. --out writes report.md and i18n.json with matching ok.
const outGood = mkdtempSync(join(tmpdir(), "slimtoken-i18n-good-"));
r = run(["check", join(fx, "locales-good"), "--out", outGood]);
const reportGood = join(outGood, "report.md");
const i18nGood = join(outGood, "i18n.json");
assert("--out good exits 0", r.status === 0, `got ${r.status}`);
assert("--out good writes report.md", existsSync(reportGood));
assert("--out good writes i18n.json", existsSync(i18nGood));
let good = null;
try {
  good = JSON.parse(readFileSync(i18nGood, "utf8"));
} catch (error) {
  assert("--out good i18n.json parses", false, error.message);
}
if (good) {
  assert("--out good i18n.ok is true", good.ok === true, JSON.stringify(good.ok));
  assert("--out good i18n locales carry per-locale arrays",
    Array.isArray(good.locales) && good.locales.length === 2 &&
      good.locales.every((l) =>
        Array.isArray(l.missing) && Array.isArray(l.empty) &&
        Array.isArray(l.placeholderMismatches) && Array.isArray(l.untranslated)),
    JSON.stringify(good.locales));
}

const outBad = mkdtempSync(join(tmpdir(), "slimtoken-i18n-bad-"));
r = run(["check", join(fx, "locales-placeholder"), "--out", outBad]);
const i18nBad = join(outBad, "i18n.json");
assert("--out bad exits 1", r.status === 1, `got ${r.status}`);
assert("--out bad writes report.md", existsSync(join(outBad, "report.md")));
let bad = null;
try {
  bad = JSON.parse(readFileSync(i18nBad, "utf8"));
} catch (error) {
  assert("--out bad i18n.json parses", false, error.message);
}
if (bad) {
  assert("--out bad i18n.ok is false", bad.ok === false, JSON.stringify(bad.ok));
  const fr = Array.isArray(bad.locales) ? bad.locales.find((l) => l.locale === "fr") : null;
  assert("--out bad fr carries the mismatch",
    fr && fr.placeholderMismatches.some((m) => m.includes("placeholder mismatch for 'greet'")),
    JSON.stringify(bad.locales));
}
rmSync(outGood, { recursive: true, force: true });
rmSync(outBad, { recursive: true, force: true });

console.log(`selftest: ${passed}/${passed + failed} passed`);
if (failed > 0) {
  console.error(`selftest: failures: ${failures.join(", ")}`);
  process.exit(1);
}
process.exit(0);
