// i18n parity gate for the SlimToken engine's i18n pack.
//
// Contract (see sidecars/README.md):
//   - exit 0: every locale passed (warnings allowed)
//   - exit 1: at least one locale failed a gate
//   - exit 2: usage error (bad args, no locale files found)
//   - zero tokens: pure JSON parsing, no model calls, no network
//
// The gate reads locale files and proves they agree. A locale is a .json
// file whose stem is the locale name (en.json -> en). Supported layouts:
//   - flat:   en.json, fr.json directly in <dir>
//   - nested: locales/en.json, locales/fr.json under <dir>
//   - per-locale dirs: en/messages.json, fr/messages.json (locale = dir)
// First layout found wins per locale name. Nested objects are flattened
// to dot paths (nav.cart). Non-string leaves are ignored.
//
// Hard checks (exit 1): key-set parity across the union of all keys;
// empty string values; placeholder parity for {icu}, %s/%d printf, and
// {{mustache}} tokens against the --source locale (default: first locale
// in alphabetical order). WARN (never fails): a value identical to the
// source locale's value for the same key (untranslated).
import { readFileSync, readdirSync, mkdirSync, writeFileSync, existsSync, statSync } from "node:fs";
import { extname, join } from "node:path";

export function runCheck(args) {
  const parsed = parseArgs(args);
  if (parsed.error) {
    console.error(`check: ${parsed.error}`);
    return 2;
  }
  const { dir, sourceOpt, outDir } = parsed;

  const files = collectLocales(dir);
  if (files.length === 0) {
    console.error(`check: no locale files found under: ${dir}`);
    return 2;
  }
  const names = files.map((f) => f.locale).sort();
  const source = sourceOpt || names[0];
  if (!names.includes(source)) {
    console.error(`check: --source '${source}' is not one of the found locales: ${names.join(", ")}`);
    return 2;
  }

  const data = new Map();
  const results = [];
  for (const { locale, file } of files) {
    let doc;
    try {
      doc = JSON.parse(readFileSync(file, "utf8"));
    } catch (error) {
      results.push({ locale, parseError: error.message, missing: [], empty: [], placeholderMismatches: [], untranslated: [] });
      continue;
    }
    data.set(locale, flatten(doc));
  }

  const union = new Set();
  for (const table of data.values()) {
    for (const key of Object.keys(table)) union.add(key);
  }
  const keys = [...union].sort();

  for (const locale of names) {
    if (!data.has(locale)) continue; // parse-error row already pushed
    const table = data.get(locale);
    const missing = [];
    const empty = [];
    const placeholderMismatches = [];
    const untranslated = [];
    for (const key of keys) {
      if (!Object.prototype.hasOwnProperty.call(table, key)) {
        missing.push(key);
        continue;
      }
      const value = table[key];
      if (value.trim() === "") {
        empty.push(key);
        continue; // an empty string has no meaningful placeholders
      }
      if (locale === source) continue;
      const sourceTable = data.get(source);
      if (!sourceTable || !Object.prototype.hasOwnProperty.call(sourceTable, key)) continue;
      const got = placeholders(value);
      const want = placeholders(sourceTable[key]);
      if (!sameSet(got, want)) {
        placeholderMismatches.push(
          `placeholder mismatch for '${key}': ${locale} has ${renderSet(got)}, source has ${renderSet(want)}`,
        );
      } else if (value === sourceTable[key]) {
        untranslated.push(key);
      }
    }
    results.push({ locale, parseError: null, missing, empty, placeholderMismatches, untranslated });
  }
  results.sort((a, b) => (a.locale < b.locale ? -1 : a.locale > b.locale ? 1 : 0));

  let errors = 0;
  let warnings = 0;
  for (const r of results) {
    if (r.parseError) {
      console.log(`ERROR ${r.locale}: unparseable JSON: ${r.parseError}`);
      errors += 1;
      continue;
    }
    for (const key of r.missing) {
      console.log(`ERROR ${r.locale}: missing key '${key}' in ${r.locale}`);
      errors += 1;
    }
    for (const key of r.empty) {
      console.log(`ERROR ${r.locale}: empty value for '${key}' in ${r.locale}`);
      errors += 1;
    }
    for (const m of r.placeholderMismatches) {
      console.log(`ERROR ${r.locale}: ${m}`);
      errors += 1;
    }
    for (const key of r.untranslated) {
      console.log(`WARN  ${r.locale}: untranslated '${key}' in ${r.locale}`);
      warnings += 1;
    }
    if (!r.parseError && r.missing.length === 0 && r.empty.length === 0 &&
        r.placeholderMismatches.length === 0 && r.untranslated.length === 0) {
      console.log(`OK   ${r.locale}`);
    }
  }
  console.log(`check: ${results.length} locales (source: ${source}), ${errors} errors, ${warnings} warnings`);

  const ok = errors === 0;
  if (outDir) writeReports(outDir, ok, source, results);
  return ok ? 0 : 1;
}

function parseArgs(args) {
  let dir = null;
  let sourceOpt = null;
  let outDir = null;
  for (let i = 0; i < args.length; i += 1) {
    const arg = args[i];
    if (arg === "--source") {
      if (i + 1 >= args.length) return { error: "--source needs a locale" };
      sourceOpt = args[i + 1];
      i += 1;
    } else if (arg.startsWith("--source=")) {
      sourceOpt = arg.slice("--source=".length);
    } else if (arg === "--out") {
      if (i + 1 >= args.length) return { error: "--out needs a directory" };
      outDir = args[i + 1];
      i += 1;
    } else if (arg.startsWith("--out=")) {
      outDir = arg.slice("--out=".length);
    } else if (arg.startsWith("--")) {
      return { error: `unknown flag '${arg}'` };
    } else if (dir !== null) {
      return { error: "check takes exactly one directory" };
    } else {
      dir = arg;
    }
  }
  if (dir === null) return { error: "no directory (usage: slimtoken-i18n check <dir> [--source <locale>] [--out DIR])" };
  if (sourceOpt !== null && sourceOpt.length === 0) return { error: "--source needs a locale" };
  if (outDir !== null && outDir.length === 0) return { error: "--out needs a directory" };
  return { dir, sourceOpt, outDir };
}

// Locale files in all three supported layouts. First find wins per name.
function collectLocales(dir) {
  if (!existsSync(dir)) return [];
  let st;
  try {
    st = statSync(dir);
  } catch {
    return [];
  }
  if (!st.isDirectory()) return [];

  const found = new Map();
  const add = (locale, file) => {
    if (locale && !found.has(locale)) found.set(locale, file);
  };
  const stemOf = (name) => basenameStem(name);

  // Flat: <dir>/*.json
  for (const entry of readdirSync(dir, { withFileTypes: true })) {
    if (entry.isFile() && extname(entry.name).toLowerCase() === ".json") {
      add(stemOf(entry.name), join(dir, entry.name));
    }
  }
  // Nested: <dir>/locales/*.json
  const localesDir = join(dir, "locales");
  if (existsSync(localesDir) && statSync(localesDir).isDirectory()) {
    for (const entry of readdirSync(localesDir, { withFileTypes: true })) {
      if (entry.isFile() && extname(entry.name).toLowerCase() === ".json") {
        add(stemOf(entry.name), join(localesDir, entry.name));
      }
    }
  }
  // Per-locale dirs: <dir>/<locale>/messages.json
  for (const entry of readdirSync(dir, { withFileTypes: true })) {
    if (!entry.isDirectory()) continue;
    const messages = join(dir, entry.name, "messages.json");
    if (existsSync(messages)) add(entry.name, messages);
  }
  return [...found.entries()]
    .map(([locale, file]) => ({ locale, file }))
    .sort((a, b) => (a.locale < b.locale ? -1 : a.locale > b.locale ? 1 : 0));
}

function basenameStem(name) {
  const dot = name.lastIndexOf(".");
  return dot > 0 ? name.slice(0, dot) : name;
}

// Nested objects (and arrays, keyed by index) become dot paths. Only
// string leaves become keys; numbers, booleans, and null are ignored.
function flatten(doc) {
  const out = {};
  const walk = (node, path) => {
    if (typeof node === "string") {
      if (path.length > 0) out[path.join(".")] = node;
      return;
    }
    if (node === null || typeof node !== "object") return;
    if (Array.isArray(node)) {
      node.forEach((item, i) => walk(item, [...path, String(i)]));
      return;
    }
    for (const [key, value] of Object.entries(node)) {
      walk(value, [...path, key]);
    }
  };
  walk(doc, []);
  return out;
}

// Placeholder tokens in a message: {{mustache}}, {icu}, and %s/%d printf.
// Mustache runs first so {{x}} is not half-eaten by the ICU rule; %% is
// stripped before the printf pass so a literal percent is not a token.
function placeholders(value) {
  const set = new Set();
  for (const m of value.matchAll(/\{\{\s*([^{}]+?)\s*\}\}/g)) {
    set.add(`{{${m[1].trim()}}}`);
  }
  const noMustache = value.replace(/\{\{[^{}]*\}\}/g, " ");
  for (const m of noMustache.matchAll(/\{([^{}]+?)\}/g)) {
    const name = m[1].split(",")[0].trim();
    if (name) set.add(`{${name}}`);
  }
  const noPct = noMustache.replace(/%%/g, "");
  for (const m of noPct.matchAll(/%(s|d)/g)) {
    set.add(`%${m[1]}`);
  }
  return set;
}

function sameSet(a, b) {
  if (a.size !== b.size) return false;
  for (const item of a) {
    if (!b.has(item)) return false;
  }
  return true;
}

function renderSet(set) {
  if (set.size === 0) return "(none)";
  return [...set].sort().join(", ");
}

function writeReports(outDir, ok, source, results) {
  mkdirSync(outDir, { recursive: true });
  let errors = 0;
  let warnings = 0;
  for (const r of results) {
    if (r.parseError) {
      errors += 1;
    } else {
      errors += r.missing.length + r.empty.length + r.placeholderMismatches.length;
      warnings += r.untranslated.length;
    }
  }
  const summary = { locales: results.length, source, errors, warnings };

  const lines = [
    "# slimtoken-i18n report",
    "",
    `- ok: ${ok}`,
    `- locales: ${summary.locales} (source: ${source})`,
    `- errors: ${summary.errors}`,
    `- warnings: ${summary.warnings}`,
    "",
    "| Locale | Status | Missing | Empty | Placeholder mismatches | Untranslated |",
    "|---|---|---|---|---|---|",
  ];
  for (const r of results) {
    const status = r.parseError ? "error" :
      (r.missing.length + r.empty.length + r.placeholderMismatches.length) > 0 ? "error" :
      r.untranslated.length > 0 ? "warn" : "ok";
    lines.push(
      `| ${r.locale} | ${status} | ${r.missing.join(", ")} | ${r.empty.join(", ")} | ` +
      `${r.placeholderMismatches.join("; ")}${r.parseError ? ` unparseable JSON: ${r.parseError}` : ""} | ${r.untranslated.join(", ")} |`,
    );
  }
  lines.push("");
  writeFileSync(join(outDir, "report.md"), lines.join("\n"), "utf8");

  const i18n = { ok, locales: results, summary };
  writeFileSync(join(outDir, "i18n.json"), JSON.stringify(i18n, null, 2) + "\n", "utf8");
  console.log(`check: wrote ${join(outDir, "report.md")} and ${join(outDir, "i18n.json")}`);
}
