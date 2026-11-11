// Contract gate for the SlimToken engine's contract pack.
//
// Contract (see sidecars/README.md):
//   - exit 0: every checked file passed (warnings allowed)
//   - exit 1: at least one file failed a gate
//   - exit 2: usage error (no args, no checkable files)
//   - zero tokens: pure parsing, no model calls, no network
//
// The gate reads .json files and proves they parse. A file that looks
// like an OpenAPI spec (top-level `openapi` or `swagger` string) gets
// three more hard checks: info.title, info.version, a paths object, and
// every local $ref (#/a/b/c) resolves by walking the pointer segments.
// External refs (other.json#/... or URLs) and YAML files are WARN SKIP:
// not resolvable offline, never a failure.
import { readFileSync, readdirSync, mkdirSync, writeFileSync, statSync, existsSync } from "node:fs";
import { extname, join } from "node:path";

const SKIP_DIRS = new Set(["node_modules", ".git"]);
const YAML_SKIP = "YAML validation not available (zero-dependency pack)";

export function runCheck(args) {
  const parsed = parseArgs(args);
  if (parsed.error) {
    console.error(`check: ${parsed.error}`);
    return 2;
  }
  const { paths, outDir } = parsed;
  if (paths.length === 0) {
    console.error("check: no input (usage: slimtoken-contract check <files-or-dirs...> [--out DIR])");
    return 2;
  }
  const files = collect(paths);
  if (files.length === 0) {
    console.error(`check: no checkable files under: ${paths.join(", ")}`);
    return 2;
  }

  const results = files.map((file) => checkOne(file));
  let errors = 0;
  let warns = 0;
  for (const r of results) {
    if (r.status === "error") {
      console.log(`ERROR ${r.file}: ${r.errors.join("; ")}`);
      errors += 1;
    } else if (r.status === "warn") {
      console.log(`WARN SKIP ${r.file}: ${r.warnings.join("; ")}`);
      warns += 1;
    } else if (r.warnings.length > 0) {
      console.log(`OK   ${r.file} (warn: ${r.warnings.join("; ")})`);
    } else {
      console.log(`OK   ${r.file}`);
    }
  }
  const ok = errors === 0;
  console.log(`check: ${results.length - errors - warns}/${results.length} ok, ${warns} warn skip, ${errors} error`);

  if (outDir) writeReports(outDir, ok, results);
  return ok ? 0 : 1;
}

function parseArgs(args) {
  const paths = [];
  let outDir = null;
  for (let i = 0; i < args.length; i += 1) {
    const arg = args[i];
    if (arg === "--out") {
      if (i + 1 >= args.length) return { error: "--out needs a directory" };
      outDir = args[i + 1];
      i += 1;
    } else if (arg.startsWith("--out=")) {
      outDir = arg.slice("--out=".length);
    } else if (arg.startsWith("--")) {
      return { error: `unknown flag '${arg}'` };
    } else {
      paths.push(arg);
    }
  }
  if (outDir !== null && outDir.length === 0) return { error: "--out needs a directory" };
  return { paths, outDir };
}

function collect(paths) {
  const files = [];
  const seen = new Set();
  for (const p of paths) {
    if (!existsSync(p)) continue;
    const st = statSync(p);
    if (st.isDirectory()) walkDir(p, files, seen);
    else maybeAdd(p, files, seen);
  }
  return files;
}

function walkDir(dir, files, seen) {
  for (const entry of readdirSync(dir, { withFileTypes: true })) {
    const full = join(dir, entry.name);
    if (entry.isDirectory()) {
      if (SKIP_DIRS.has(entry.name) || entry.name.startsWith(".")) continue;
      walkDir(full, files, seen);
    } else {
      maybeAdd(full, files, seen);
    }
  }
}

function maybeAdd(file, files, seen) {
  const ext = extname(file).toLowerCase();
  if (ext !== ".json" && ext !== ".yaml" && ext !== ".yml") return;
  const key = file.toLowerCase();
  if (seen.has(key)) return;
  seen.add(key);
  files.push(file);
}

function checkOne(file) {
  const ext = extname(file).toLowerCase();
  if (ext === ".yaml" || ext === ".yml") {
    return { file, status: "warn", errors: [], warnings: [YAML_SKIP] };
  }

  let text;
  try {
    text = readFileSync(file, "utf8");
  } catch (error) {
    return { file, status: "error", errors: [`unreadable: ${error.message}`], warnings: [] };
  }

  let doc;
  try {
    doc = JSON.parse(text);
  } catch (error) {
    return { file, status: "error", errors: [error.message], warnings: [] };
  }

  if (!isOpenApi(doc)) {
    return { file, status: "ok", errors: [], warnings: [] };
  }

  const errors = [];
  const warnings = [];
  const info = doc.info;
  if (info === null || typeof info !== "object") {
    errors.push("missing info object");
  } else {
    if (typeof info.title !== "string" || info.title.length === 0) errors.push("missing info.title");
    if (typeof info.version !== "string" || info.version.length === 0) errors.push("missing info.version");
  }
  if (doc.paths === null || typeof doc.paths !== "object" || Array.isArray(doc.paths)) {
    errors.push("missing paths object");
  }
  for (const ref of collectRefs(doc)) {
    if (ref === "#") continue;
    if (ref.startsWith("#/")) {
      if (!resolvePointer(doc, ref)) errors.push(`dangling $ref: ${ref}`);
    } else {
      warnings.push(`external $ref not resolvable offline: ${ref}`);
    }
  }

  return { file, status: errors.length > 0 ? "error" : "ok", errors, warnings };
}

function isOpenApi(doc) {
  if (doc === null || typeof doc !== "object" || Array.isArray(doc)) return false;
  return typeof doc.openapi === "string" || typeof doc.swagger === "string";
}

// Every string value of a "$ref" key, at any depth. Cycles are guarded.
function collectRefs(doc) {
  const refs = [];
  const seen = new Set();
  const walk = (node) => {
    if (node === null || typeof node !== "object") return;
    if (seen.has(node)) return;
    seen.add(node);
    if (Array.isArray(node)) {
      for (const item of node) walk(item);
      return;
    }
    for (const [key, value] of Object.entries(node)) {
      if (key === "$ref" && typeof value === "string") refs.push(value);
      else walk(value);
    }
  };
  walk(doc);
  return refs;
}

// Walk "#/a/b/c" against the document. Segments are unescaped per the
// JSON pointer rules (~1 -> /, ~0 -> ~). Returns true when every segment
// resolves to a real node.
function resolvePointer(doc, ref) {
  let node = doc;
  for (const raw of ref.slice(2).split("/")) {
    const seg = raw.replace(/~1/g, "/").replace(/~0/g, "~");
    if (node === null || typeof node !== "object") return false;
    if (Array.isArray(node)) {
      const idx = Number(seg);
      if (!Number.isInteger(idx) || idx < 0 || idx >= node.length) return false;
      node = node[idx];
    } else {
      if (!Object.prototype.hasOwnProperty.call(node, seg)) return false;
      node = node[seg];
    }
  }
  return true;
}

function writeReports(outDir, ok, results) {
  mkdirSync(outDir, { recursive: true });
  const warnCount = results.filter((r) => r.status === "warn").length;
  const errorCount = results.filter((r) => r.status === "error").length;
  const summary = {
    total: results.length,
    ok: results.length - warnCount - errorCount,
    warn: warnCount,
    error: errorCount,
  };

  const lines = [
    "# slimtoken-contract report",
    "",
    `- ok: ${ok}`,
    `- files: ${summary.total} (ok ${summary.ok}, warn skip ${summary.warn}, error ${summary.error})`,
    "",
    "| File | Status | Errors | Warnings |",
    "|---|---|---|---|",
  ];
  for (const r of results) {
    lines.push(`| ${r.file} | ${r.status} | ${r.errors.join("; ")} | ${r.warnings.join("; ")} |`);
  }
  lines.push("");
  writeFileSync(join(outDir, "report.md"), lines.join("\n"), "utf8");

  const contract = {
    ok,
    files: results.map((r) => ({ file: r.file, status: r.status, errors: r.errors, warnings: r.warnings })),
    summary,
  };
  writeFileSync(join(outDir, "contract.json"), JSON.stringify(contract, null, 2) + "\n", "utf8");
  console.log(`check: wrote ${join(outDir, "report.md")} and ${join(outDir, "contract.json")}`);
}
