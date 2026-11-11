// Binding discovery, presence checks, and compile checks for the types
// pack. Pure node + child processes; zero runtime dependencies.
//
// Windows notes: `tsc` on PATH is an npm shim (tsc.cmd), so tool calls go
// through cmd.exe with quoted arguments. `python` and `py` are real
// executables and spawn directly. Python bytecode cache is redirected to
// the OS temp dir so the bindings directory is never polluted with
// __pycache__ folders.
import { spawnSync } from "node:child_process";
import { existsSync, readdirSync, readFileSync } from "node:fs";
import { join } from "node:path";
import { tmpdir } from "node:os";

const SKIP_DIRS = new Set(["node_modules", ".git"]);

// Recursively collect binding files. Returns { ts: [...], py: [...] } with
// ts holding .ts and .d.ts files. Windows paths are case-insensitive on
// disk but the extension match here is normalized to lower case.
export function collectFiles(dir) {
  const files = { ts: [], py: [] };
  const walk = (current) => {
    for (const entry of readdirSync(current, { withFileTypes: true })) {
      if (entry.name.startsWith(".")) continue;
      const full = join(current, entry.name);
      if (entry.isDirectory()) {
        if (SKIP_DIRS.has(entry.name)) continue;
        walk(full);
      } else {
        const ext = entry.name.toLowerCase();
        if (ext.endsWith(".ts") || ext.endsWith(".d.ts")) files.ts.push(full);
        else if (ext.endsWith(".py")) files.py.push(full);
      }
    }
  };
  walk(dir);
  files.ts.sort();
  files.py.sort();
  return files;
}

// Resolve --lang against the collected files. Returns { langs, error }.
// "auto" picks ts when .ts/.d.ts files exist, py when .py files exist,
// both when both exist. A forced lang with no matching files is a usage
// error. No files at all is a usage error (exit 2), never a pass.
export function detectLangs(forced, files) {
  const hasTs = files.ts.length > 0;
  const hasPy = files.py.length > 0;
  if (forced === "ts" || forced === "py") {
    const ok = forced === "ts" ? hasTs : hasPy;
    if (!ok) {
      return { langs: [], error: `no ${forced === "ts" ? ".ts/.d.ts" : ".py"} binding files found in bindings dir` };
    }
    return { langs: [forced], error: null };
  }
  const langs = [];
  if (hasTs) langs.push("ts");
  if (hasPy) langs.push("py");
  if (langs.length === 0) {
    return { langs: [], error: "no binding files (.ts/.d.ts or .py) found in bindings dir" };
  }
  return { langs, error: null };
}

// TS presence: a line `export (interface|type|class|enum) <Name>` in any
// .ts/.d.ts file under the bindings dir.
export function findTsBinding(name, files) {
  const re = new RegExp(`^export\\s+(?:interface|type|class|enum)\\s+${escapeRe(name)}\\b`);
  for (const file of files) {
    const text = readFileSync(file, "utf8");
    for (const line of text.split(/\r?\n/)) {
      if (re.test(line)) return file;
    }
  }
  return null;
}

// Python presence: `class <Name>(` or `class <Name>:` at the start of a
// line (a base class is optional, so both class styles match), or
// `<Name> = ` (a type alias or assignment).
export function findPyBinding(name, files) {
  const re = new RegExp(`^(?:class\\s+${escapeRe(name)}\\b\\s*(?:\\(|:)|${escapeRe(name)}\\s*=)`);
  for (const file of files) {
    const text = readFileSync(file, "utf8");
    for (const line of text.split(/\r?\n/)) {
      if (re.test(line)) return file;
    }
  }
  return null;
}

function escapeRe(s) {
  return s.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
}

// --- Compile checks -----------------------------------------------------

// Quote one argument for cmd.exe. Embedded quotes are doubled, which is
// the cmd /s /c convention.
function quoteArg(arg) {
  return `"${String(arg).replace(/"/g, '""')}"`;
}

// Run a command through cmd.exe (needed for npm .cmd shims like tsc).
function shellRun(cmd, args) {
  const line = [cmd, ...args].map(quoteArg).join(" ");
  return spawnSync(line, { shell: true, encoding: "utf8", windowsHide: true });
}

// Direct spawn for real executables (python.exe, py.exe).
function directRun(cmd, args, options = {}) {
  return spawnSync(cmd, args, { encoding: "utf8", windowsHide: true, ...options });
}

export function tscAvailable() {
  const r = shellRun("tsc", ["--version"]);
  return r.status === 0;
}

// Compile the TS bindings. Uses `tsc --noEmit -p <dir>` when a
// tsconfig.json exists there, else `tsc --noEmit <all .ts files>`.
// Returns { ok, detail }.
export function runTsc(dir, tsFiles) {
  const hasConfig = existsSync(join(dir, "tsconfig.json"));
  const args = hasConfig ? ["--noEmit", "-p", dir] : ["--noEmit", ...tsFiles];
  const r = shellRun("tsc", args);
  if (r.status === 0) return { ok: true, detail: `tsc --noEmit ${hasConfig ? `-p ${dir}` : `${tsFiles.length} file(s)`} passed` };
  const output = ((r.stdout || "") + (r.stderr || "")).trim();
  const first = output.split(/\r?\n/).filter((l) => l.trim().length > 0).slice(0, 5).join("; ");
  return { ok: false, detail: first.length > 0 ? first : `tsc exited ${r.status}` };
}

// Probe python, then py. Returns the working command name or null.
export function pythonCommand() {
  for (const cmd of ["python", "py"]) {
    const r = directRun(cmd, ["--version"]);
    if (r.status === 0) return cmd;
  }
  return null;
}

// Syntax-check each .py file with `python -m py_compile`. The bytecode
// cache is redirected to the OS temp dir so the bindings dir stays clean.
// Returns a list of { file, ok, detail }.
export function runPyCompile(cmd, pyFiles) {
  const results = [];
  for (const file of pyFiles) {
    const r = directRun(cmd, ["-m", "py_compile", file], {
      env: { ...process.env, PYTHONPYCACHEPREFIX: tmpdir() },
    });
    if (r.status === 0) {
      results.push({ file, ok: true, detail: "py_compile passed" });
    } else {
      const output = ((r.stdout || "") + (r.stderr || "")).trim();
      const first = output.split(/\r?\n/).filter((l) => l.trim().length > 0).slice(0, 3).join("; ");
      results.push({ file, ok: false, detail: first.length > 0 ? first : `py_compile exited ${r.status}` });
    }
  }
  return results;
}
