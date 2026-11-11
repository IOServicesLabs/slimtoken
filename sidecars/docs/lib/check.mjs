// slimtoken-docs — docs drift gate (lib/check.mjs).
//
//   slimtoken-docs check <dir-or-file> [...] [--out DIR]
//
// Contract (see sidecars/README.md):
//   - exit 0: every checked file passed (WARN lines do not fail)
//   - exit 1: at least one file failed a gate
//   - exit 2: usage error (no args, no checkable files)
//   - zero tokens: pure file parsing, no model calls, no network
//
// HARD checks (fail the run):
//   1. Relative links resolve: the target file exists (anchor stripped);
//      a `#anchor` on a local .md target must match a heading's
//      lowercased, space-to-dash form. Empty target `[text]()` is an
//      error. http/https/mailto/#-only targets are skipped.
//   2. Code fences balance: an odd ``` count is an unclosed fence.
//   3. The file is UTF-8 readable and non-empty.
//   4. Front matter: a `---` block on line 1 must close with `---` and
//      hold `key: value` pairs. A README.md with front matter needs
//      `title` and `description` keys; `title` must be non-empty.
//   5. API tables: when front matter sets `api: true`, every table row
//      must have the same cell count as its header row, and every table
//      needs a header separator line.
//
// WARN checks (reported, never fail):
//   - heading-level skips (h1 -> h3 with no h2)
//   - sentences longer than 40 words
//   - trailing whitespace on lines
//   - front matter keys outside the known set (title, description, api)
import {
  existsSync, mkdirSync, readFileSync, readdirSync, statSync, writeFileSync,
} from "node:fs";
import { dirname, join, resolve } from "node:path";

const SKIP_DIRS = new Set(["node_modules", ".git"]);
const SENTENCE_MAX_WORDS = 40;
const LINK_RE = /\[([^\]]*)\]\(([^)]*)\)/g;
const HEADING_RE = /^ {0,3}(#{1,6})\s+(.*)$/;
const TRAILING_WS_RE = /[ \t]+$/;
const FM_BOUNDARY_RE = /^---\s*$/;
const FM_KNOWN_KEYS = new Set(["title", "description", "api"]);

export function runCheck(args) {
  const { paths, outDir, usage } = parseArgs(args);
  if (usage || paths.length === 0) {
    console.error("usage: slimtoken-docs check <dir-or-file> [...] [--out DIR]");
    return 2;
  }

  const entries = collect(paths);
  if (entries.length === 0) {
    console.error("slimtoken-docs: no checkable .md files found");
    return 2;
  }

  const results = entries.map((entry) => checkOne(entry));
  for (const result of results) printResult(result);

  const errorCount = results.reduce((n, r) => n + r.errors.length, 0);
  const warnCount = results.reduce((n, r) => n + r.warnings.length, 0);
  const cleanCount = results.filter((r) => r.errors.length === 0).length;
  console.log(
    `docs: ${cleanCount}/${results.length} files clean; `
    + `${errorCount} errors, ${warnCount} warnings`,
  );

  if (outDir) writeReports(outDir, results, errorCount, warnCount);

  return errorCount > 0 ? 1 : 0;
}

// --args -> { paths, outDir, usage }. Supports `--out DIR` and
// `--out=DIR`. Any other `--flag` is a usage error.
function parseArgs(args) {
  const paths = [];
  let outDir = null;
  let usage = false;
  for (let i = 0; i < args.length; i += 1) {
    const arg = args[i];
    if (arg === "--out") {
      if (i + 1 >= args.length || args[i + 1].startsWith("--")) usage = true;
      else outDir = args[i + 1];
      i += 1;
    } else if (arg.startsWith("--out=")) {
      outDir = arg.slice("--out=".length);
    } else if (arg.startsWith("--")) {
      usage = true;
    } else {
      paths.push(arg);
    }
  }
  return { paths, outDir, usage };
}

// Dirs contribute every .md file below them (skipping node_modules and
// .git); listed files are checked as given.
function collect(paths) {
  const entries = [];
  for (const p of paths) {
    if (!existsSync(p)) {
      entries.push({ path: p, missing: true });
      continue;
    }
    const st = statSync(p);
    if (st.isDirectory()) walk(p, entries);
    else entries.push({ path: p });
  }
  return entries;
}

function walk(dir, entries) {
  for (const entry of readdirSync(dir, { withFileTypes: true })) {
    if (SKIP_DIRS.has(entry.name)) continue;
    const full = join(dir, entry.name);
    if (entry.isDirectory()) walk(full, entries);
    else if (entry.name.toLowerCase().endsWith(".md")) entries.push({ path: full });
  }
}

function checkOne(entry) {
  const file = entry.path;
  const errors = [];
  const warnings = [];

  if (entry.missing) {
    errors.push("file not found");
    return { file, status: "error", errors, warnings, frontMatter: null };
  }

  let text;
  try {
    text = readFileSync(file, "utf8");
  } catch (error) {
    errors.push(`unreadable: ${error.message || error}`);
    return { file, status: "error", errors, warnings, frontMatter: null };
  }
  if (text.trim().length === 0) {
    errors.push("empty file");
    return { file, status: "error", errors, warnings, frontMatter: null };
  }

  checkFences(text, errors);
  checkLinks(file, text, errors);
  checkStyle(text, warnings);

  const frontMatter = parseFrontMatter(text, errors);
  if (frontMatter !== null) {
    checkFrontMatterRules(file, frontMatter, errors, warnings);
    if (frontMatter.api === "true") checkApiTables(text, errors);
  }

  const status = errors.length > 0 ? "error" : warnings.length > 0 ? "warn" : "ok";
  return { file, status, errors, warnings, frontMatter };
}

function checkFences(text, errors) {
  const count = (text.match(/```/g) || []).length;
  if (count % 2 === 1) {
    errors.push(`unclosed code fence (${count} fence markers)`);
  }
}

function checkLinks(file, text, errors) {
  // Links inside fenced code are not real links; check the prose only.
  const prose = stripFences(text);
  for (const match of prose.matchAll(LINK_RE)) {
    const target = match[2].trim();
    if (target === "") {
      errors.push("empty link target");
      continue;
    }
    if (/^(https?:|mailto:)/i.test(target) || target.startsWith("#")) continue;

    const hashAt = target.indexOf("#");
    const filePart = hashAt === -1 ? target : target.slice(0, hashAt);
    const anchor = hashAt === -1 ? "" : target.slice(hashAt + 1);
    if (filePart === "") continue;

    const resolved = resolve(dirname(file), safeDecode(filePart));
    if (!existsSync(resolved)) {
      errors.push(`broken link: ${target} (file not found)`);
      continue;
    }
    if (anchor !== "" && resolved.toLowerCase().endsWith(".md")) {
      const slugs = headingSlugs(resolved);
      if (!slugs.has(safeDecode(anchor))) {
        errors.push(`broken anchor: ${target} (no matching heading)`);
      }
    }
  }
}

function checkStyle(text, warnings) {
  const prose = stripFences(text);
  const proseLines = prose.split(/\r?\n/);

  // Heading-level skips: a heading more than one level below the
  // previous heading (h1 -> h3 with no h2).
  let prevLevel = 0;
  for (const line of proseLines) {
    const match = HEADING_RE.exec(line);
    if (!match) continue;
    const level = match[1].length;
    if (prevLevel > 0 && level > prevLevel + 1) {
      warnings.push(`heading-level skip: h${prevLevel} -> h${level}`);
    }
    prevLevel = level;
  }

  // Sentences longer than SENTENCE_MAX_WORDS words. Table rows are
  // not prose: their cells join into one false "sentence" without
  // sentence breaks, so they are excluded. Heading lines and lines
  // that end with a colon are treated as sentence ends: they are
  // lead-ins to lists or code blocks, not part of the next sentence.
  const sentenceProse = proseLines
    .filter((line) => !line.trimStart().startsWith("|"))
    .map((line) => {
      const trimmed = line.trimEnd();
      if (/^#{1,6}\s/.test(trimmed) || trimmed.endsWith(":")) {
        return trimmed + ".";
      }
      return line;
    })
    .join("\n");
  for (const sentence of sentenceProse.split(/[.!?]+/)) {
    const words = sentence.trim().split(/\s+/).filter(Boolean).length;
    if (words > SENTENCE_MAX_WORDS) {
      warnings.push(`sentence with ${words} words (max ${SENTENCE_MAX_WORDS})`);
    }
  }

  // Trailing whitespace (checked on raw lines, fences included).
  text.split(/\r?\n/).forEach((line, index) => {
    if (TRAILING_WS_RE.test(line)) {
      warnings.push(`trailing whitespace on line ${index + 1}`);
    }
  });
}

// A `---` block on line 1 is front matter: simple one-level `key: value`
// pairs up to the closing `---`. Returns the pairs object, or null when
// the file has no front matter. A malformed block (no closing `---`, a
// line without a colon, an empty key) is a HARD error.
function parseFrontMatter(text, errors) {
  const lines = text.split(/\r?\n/);
  if (!FM_BOUNDARY_RE.test(lines[0] ?? "")) return null;

  const frontMatter = {};
  let closed = false;
  for (let i = 1; i < lines.length; i += 1) {
    if (FM_BOUNDARY_RE.test(lines[i])) {
      closed = true;
      break;
    }
    const colonAt = lines[i].indexOf(":");
    if (colonAt === -1) {
      errors.push(`bad front matter (line ${i + 1}): expected "key: value"`);
      continue;
    }
    const key = lines[i].slice(0, colonAt).trim();
    const value = lines[i].slice(colonAt + 1).trim();
    if (key === "") {
      errors.push(`bad front matter (line ${i + 1}): empty key`);
      continue;
    }
    frontMatter[key] = value;
  }
  if (!closed) errors.push("bad front matter: no closing ---");
  return frontMatter;
}

// Front matter content rules. A README.md that carries front matter must
// have `title` and `description`; `title` must be non-empty. Keys outside
// the known set are listed as WARN only.
function checkFrontMatterRules(file, frontMatter, errors, warnings) {
  const name = file.split(/[\\/]/).pop().toLowerCase();
  if (name === "readme.md") {
    if (!("title" in frontMatter)) {
      errors.push("front matter: README.md needs a title key");
    }
    if (!("description" in frontMatter)) {
      errors.push("front matter: README.md needs a description key");
    }
  }
  if ("title" in frontMatter && frontMatter.title === "") {
    errors.push("front matter: title is empty");
  }
  for (const key of Object.keys(frontMatter)) {
    if (!FM_KNOWN_KEYS.has(key)) {
      warnings.push(`front matter unknown key: ${key}`);
    }
  }
}

// API-reference table rules, run only when front matter sets `api: true`.
// A table is a run of lines whose trimmed form starts with `|`. The first
// line is the header row; the next line must be a header separator; every
// body row must have the header's cell count.
function checkApiTables(text, errors) {
  const lines = stripFences(text).split(/\r?\n/);
  let i = 0;
  while (i < lines.length) {
    if (!isTableLine(lines[i])) {
      i += 1;
      continue;
    }
    const headerCount = tableCells(lines[i]).length;
    let j = i + 1;
    if (j >= lines.length || !isSeparatorLine(lines[j])) {
      errors.push(`table missing header separator (line ${i + 1})`);
    } else {
      j += 1;
    }
    while (j < lines.length && isTableLine(lines[j])) {
      const count = tableCells(lines[j]).length;
      if (count !== headerCount) {
        errors.push(
          `table row has ${count} cells, header has ${headerCount} (line ${j + 1})`,
        );
      }
      j += 1;
    }
    i = j;
  }
}

function isTableLine(line) {
  return line.trim().startsWith("|");
}

// `| Method | Path |` -> ["Method", "Path"]. Leading and trailing pipes
// are optional; cells are trimmed.
function tableCells(line) {
  return line
    .trim()
    .replace(/^\|/, "")
    .replace(/\|$/, "")
    .split("|")
    .map((cell) => cell.trim());
}

// A separator line is made of cells that hold only dashes and colons.
function isSeparatorLine(line) {
  if (!isTableLine(line)) return false;
  const cells = tableCells(line);
  return cells.length > 0 && cells.every((cell) => /^:?-+:?$/.test(cell));
}

// Even chunks of a ``` split are prose; odd chunks are fenced code.
// Works for an odd marker count too: the open fence just runs on.
function stripFences(text) {
  return text
    .split("```")
    .filter((_, index) => index % 2 === 0)
    .join("\n");
}

function headingSlugs(file) {
  const slugs = new Set();
  let text;
  try {
    text = readFileSync(file, "utf8");
  } catch {
    return slugs;
  }
  for (const line of text.split(/\r?\n/)) {
    const match = HEADING_RE.exec(line);
    if (match) {
      slugs.add(match[2].trim().toLowerCase().replace(/\s+/g, "-"));
    }
  }
  return slugs;
}

function safeDecode(value) {
  try {
    return decodeURIComponent(value);
  } catch {
    return value;
  }
}

function printResult(result) {
  if (result.status === "ok") {
    console.log(`OK   ${result.file}`);
  } else {
    for (const warning of result.warnings) {
      console.log(`WARN ${result.file}: ${warning}`);
    }
    for (const error of result.errors) {
      console.log(`ERROR ${result.file}: ${error}`);
    }
  }
}

// Judge output contract: report.md (one glance) + docs.json (machine
// twin, stable field names, new fields append at the end).
function writeReports(outDir, results, errorCount, warnCount) {
  mkdirSync(outDir, { recursive: true });
  const ok = errorCount === 0;
  const summary = {
    total: results.length,
    clean: results.filter((r) => r.errors.length === 0).length,
    errors: errorCount,
    warnings: warnCount,
  };
  const docs = {
    ok,
    files: results.map((r) => ({
      file: r.file,
      status: r.status,
      errors: r.errors,
      warnings: r.warnings,
      frontMatter: r.frontMatter ?? null,
    })),
    summary,
  };
  writeFileSync(join(outDir, "docs.json"), `${JSON.stringify(docs, null, 2)}\n`, "utf8");

  const lines = [
    "# slimtoken-docs report",
    "",
    `ok: ${ok}`,
    `files: ${summary.clean}/${summary.total} clean`,
    `errors: ${summary.errors}   warnings: ${summary.warnings}`,
    "",
  ];
  for (const result of docs.files) {
    lines.push(`## ${result.file}`, "", `status: ${result.status}`, "");
    for (const error of result.errors) lines.push(`- ERROR: ${error}`);
    for (const warning of result.warnings) lines.push(`- WARN: ${warning}`);
    if (result.errors.length === 0 && result.warnings.length === 0) {
      lines.push("- no findings");
    }
    lines.push("");
  }
  writeFileSync(join(outDir, "report.md"), lines.join("\n"), "utf8");
}
