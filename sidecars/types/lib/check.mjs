// Check command for the types pack: prove that drafted type bindings
// cover every schema in a spec and compile.
//
// Contract (see sidecars/README.md):
//   - exit 0: no ERROR checks (WARN SKIP is allowed)
//   - exit 1: at least one ERROR check
//   - exit 2: usage error (bad args, unreadable/unparseable spec, empty
//     bindings dir, no detectable language)
//   - zero tokens: file parsing + tsc / python -m py_compile; no model
//     calls, no network
import { existsSync, mkdirSync, statSync, writeFileSync } from "node:fs";
import { join } from "node:path";

import { SpecError, auditRefs, extractSchemas, loadSpec } from "./spec.mjs";
import {
  collectFiles, detectLangs, findPyBinding, findTsBinding,
  pythonCommand, runPyCompile, runTsc, tscAvailable,
} from "./bindings.mjs";

export function runCheck(args) {
  const parsed = parseArgs(args);
  if (parsed.error) {
    console.error(`check: ${parsed.error}`);
    return 2;
  }
  const { specFile, bindingsDir, lang, outDir } = parsed;

  // 1. Spec: unreadable or unparseable is a usage-level error (exit 2).
  let doc;
  try {
    doc = loadSpec(specFile);
  } catch (error) {
    if (error instanceof SpecError) {
      console.error(`check: ${error.message}`);
      return 2;
    }
    throw error;
  }

  const checks = [];
  const { kind, schemas } = extractSchemas(doc);
  if (kind === "none") {
    console.error("check: spec is not a JSON object");
    return 2;
  }
  if (schemas.length === 0) {
    // A valid spec with zero schemas is not a failure; there is nothing
    // to prove. WARN SKIP and pass.
    checks.push({
      kind: "spec", target: specFile, status: "warn",
      detail: "no schemas found in spec",
    });
    return finish(checks, schemas, outDir);
  }

  const label = kind === "openapi" ? "OpenAPI" : "schema map";
  checks.push({
    kind: "spec", target: specFile, status: "ok",
    detail: `${label} spec parsed; ${schemas.length} schema(s): ${schemas.join(", ")}`,
  });

  // 2. Dangling local $ref (hard) and external $ref (warn skip).
  const { dangling, external } = auditRefs(doc);
  for (const ref of dangling) {
    checks.push({ kind: "ref", target: ref, status: "error", detail: `dangling $ref: ${ref}` });
  }
  if (external.length > 0) {
    checks.push({
      kind: "ref", target: specFile, status: "warn",
      detail: `external $ref not resolvable offline: ${external.join(", ")}`,
    });
  }

  // 3. Bindings dir must exist and hold at least one binding file.
  if (!existsSync(bindingsDir) || !statSync(bindingsDir).isDirectory()) {
    console.error(`check: bindings dir not found: ${bindingsDir}`);
    return 2;
  }
  const files = collectFiles(bindingsDir);
  const detected = detectLangs(lang, files);
  if (detected.error) {
    console.error(`check: ${detected.error}: ${bindingsDir}`);
    return 2;
  }
  const langs = detected.langs;

  // 4. Binding presence: every schema name must appear as an exported
  //    type. This is the core gate and runs with zero toolchain.
  for (const schema of schemas) {
    if (langs.includes("ts")) {
      const hit = findTsBinding(schema, files.ts);
      checks.push(hit
        ? { kind: "binding", target: `'${schema}' (.ts)`, status: "ok", detail: hit }
        : { kind: "binding", target: `'${schema}' (.ts)`, status: "error", detail: `no binding for schema '${schema}'` });
    }
    if (langs.includes("py")) {
      const hit = findPyBinding(schema, files.py);
      checks.push(hit
        ? { kind: "binding", target: `'${schema}' (.py)`, status: "ok", detail: hit }
        : { kind: "binding", target: `'${schema}' (.py)`, status: "error", detail: `no binding for schema '${schema}'` });
    }
  }

  // 5. Compile checks when a toolchain is present; WARN SKIP otherwise
  //    (never fails alone).
  if (langs.includes("ts")) {
    if (tscAvailable()) {
      const r = runTsc(bindingsDir, files.ts);
      checks.push({
        kind: "compile", target: "ts", status: r.ok ? "ok" : "error", detail: r.detail,
      });
    } else {
      checks.push({
        kind: "compile", target: "ts", status: "warn",
        detail: "tsc not found on PATH; compile check skipped",
      });
    }
  }
  if (langs.includes("py")) {
    const pyCmd = pythonCommand();
    if (pyCmd === null) {
      checks.push({
        kind: "compile", target: "py", status: "warn",
        detail: "python (and py) not found on PATH; compile check skipped",
      });
    } else {
      for (const r of runPyCompile(pyCmd, files.py)) {
        checks.push({
          kind: "compile", target: `py ${r.file}`, status: r.ok ? "ok" : "error", detail: r.detail,
        });
      }
    }
  }

  return finish(checks, schemas, outDir);
}

function finish(checks, schemas, outDir) {
  let errors = 0;
  let warns = 0;
  for (const c of checks) {
    if (c.status === "error") {
      console.log(`ERROR     [${c.kind}] ${c.target}: ${c.detail}`);
      errors += 1;
    } else if (c.status === "warn") {
      console.log(`WARN SKIP [${c.kind}] ${c.target}: ${c.detail}`);
      warns += 1;
    } else {
      console.log(`OK        [${c.kind}] ${c.target}: ${c.detail}`);
    }
  }
  const ok = errors === 0;
  console.log(`check: ${checks.length - errors - warns}/${checks.length} ok, ${warns} warn skip, ${errors} error`);
  console.log(`check: verdict ${ok ? "PASS" : "FAIL"}`);

  if (outDir) writeReports(outDir, ok, schemas, checks);
  return ok ? 0 : 1;
}

function parseArgs(args) {
  const positional = [];
  let lang = "auto";
  let outDir = null;
  for (let i = 0; i < args.length; i += 1) {
    const arg = args[i];
    if (arg === "--lang") {
      if (i + 1 >= args.length) return { error: "--lang needs a value (auto|ts|py)" };
      lang = args[i + 1];
      i += 1;
    } else if (arg.startsWith("--lang=")) {
      lang = arg.slice("--lang=".length);
    } else if (arg === "--out") {
      if (i + 1 >= args.length) return { error: "--out needs a directory" };
      outDir = args[i + 1];
      i += 1;
    } else if (arg.startsWith("--out=")) {
      outDir = arg.slice("--out=".length);
    } else if (arg.startsWith("--")) {
      return { error: `unknown flag '${arg}'` };
    } else {
      positional.push(arg);
    }
  }
  if (positional.length !== 2) {
    return { error: "usage: slimtoken-types check <spec-file> <bindings-dir> [--lang auto|ts|py] [--out DIR]" };
  }
  if (lang !== "auto" && lang !== "ts" && lang !== "py") {
    return { error: `--lang must be auto, ts, or py (got '${lang}')` };
  }
  if (outDir !== null && outDir.length === 0) return { error: "--out needs a directory" };
  return { specFile: positional[0], bindingsDir: positional[1], lang, outDir, error: null };
}

function writeReports(outDir, ok, schemas, checks) {
  mkdirSync(outDir, { recursive: true });
  const warnCount = checks.filter((c) => c.status === "warn").length;
  const errorCount = checks.filter((c) => c.status === "error").length;
  const summary = {
    total: checks.length,
    ok: checks.length - warnCount - errorCount,
    warn: warnCount,
    error: errorCount,
  };

  const lines = [
    "# slimtoken-types report",
    "",
    `- ok: ${ok}`,
    `- schemas: ${schemas.length}${schemas.length > 0 ? ` (${schemas.join(", ")})` : ""}`,
    `- checks: ${summary.total} (ok ${summary.ok}, warn skip ${summary.warn}, error ${summary.error})`,
    "",
    "| Check | Target | Status | Detail |",
    "|---|---|---|---|",
  ];
  for (const c of checks) {
    lines.push(`| ${c.kind} | ${c.target} | ${c.status} | ${c.detail || ""} |`);
  }
  lines.push("");
  writeFileSync(join(outDir, "report.md"), lines.join("\n"), "utf8");

  const types = { ok, schemas, checks, summary };
  writeFileSync(join(outDir, "types.json"), JSON.stringify(types, null, 2) + "\n", "utf8");
  console.log(`check: wrote ${join(outDir, "report.md")} and ${join(outDir, "types.json")}`);
}
