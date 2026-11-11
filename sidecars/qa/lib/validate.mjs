// validate.mjs — zero-token syntax gate for generated artifacts.
//
// The engine's artifact validation checks output shape (closed code
// fences) but cannot see invalid code: a syntactically broken file passes
// the fence check, is marked succeeded, and only fails later when a test
// suite dies at collection. This gate closes that hole before the run is
// called green. No dependencies: shelling out to the language's own
// parser (python -m py_compile / node --check), so it is exactly as
// strict as the runtime, never a re-implementation.
//
//   node validate.mjs file.py app.js lib.mjs ...
//   node validate.mjs --strict dir/            # dir: every *.py *.js *.mjs *.cjs
//
// Exit 0: every file parsed. Exit 1: at least one syntax error (details
// on stdout, one line per file).
import { execFileSync } from "node:child_process";
import { readdirSync, statSync } from "node:fs";
import { join } from "node:path";

const CHECKABLE = new Set([".py", ".js", ".mjs", ".cjs"]);

function* expand(args, strict) {
  for (const arg of args) {
    let stat = null;
    try {
      stat = statSync(arg);
    } catch {
      yield { file: arg, error: "file not found" };
      continue;
    }
    if (stat.isDirectory()) {
      if (!strict) {
        yield { file: arg, error: "is a directory (pass --strict to scan it)" };
        continue;
      }
      for (const name of readdirSync(arg)) {
        if (CHECKABLE.has(ext(name))) yield { file: join(arg, name) };
      }
    } else {
      yield { file: arg };
    }
  }
}

function ext(file) {
  const dot = file.lastIndexOf(".");
  return dot < 0 ? "" : file.slice(dot).toLowerCase();
}

function checkOne(file) {
  const kind = ext(file);
  try {
    if (kind === ".py") {
      execFileSync("python", ["-m", "py_compile", file], { stdio: "pipe" });
    } else if (CHECKABLE.has(kind)) {
      execFileSync(process.execPath, ["--check", file], { stdio: "pipe" });
    } else {
      return { file, skipped: `no syntax checker for '${kind || "file"}'` };
    }
    return { file, ok: true };
  } catch (error) {
    const text = (error.stderr?.toString() || error.stdout?.toString() || String(error)).trim();
    const line = text.split(/\r?\n/).find((l) => l.trim()) || "syntax error";
    return { file, ok: false, error: line.trim() };
  }
}

export function runValidate(args) {
  const strict = args.includes("--strict");
  const files = args.filter((a) => a !== "--strict");
  if (files.length === 0) {
    console.error("usage: validate [--strict] <file-or-dir> [...]");
    return 2;
  }
  let bad = 0;
  let skipped = 0;
  for (const entry of expand(files, strict)) {
    if (entry.error) {
      console.log(`ERROR ${entry.file}: ${entry.error}`);
      bad += 1;
      continue;
    }
    const result = checkOne(entry.file);
    if (result.ok) {
      console.log(`OK    ${result.file}`);
    } else if (result.skipped) {
      console.log(`SKIP  ${result.file}: ${result.skipped}`);
      skipped += 1;
    } else {
      console.log(`ERROR ${result.file}: ${result.error}`);
      bad += 1;
    }
  }
  console.log(`validate: ${files.length} input(s), ${bad} error(s), ${skipped} skipped`);
  return bad > 0 ? 1 : 0;
}
