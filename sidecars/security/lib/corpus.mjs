// Corpus loading and validation for the security pack.
//
// A corpus is a directory of .json case files. Each case is one replay:
//
//   { name, method?, path, headers?, body?, markers? }
//
//   name    string (required)  — case label, printed per line and in reports.
//   method  string (optional)  — HTTP method, default GET.
//   path    string (required)  — request path, must start with "/".
//   headers object (optional)  — extra request headers.
//   body    string (optional)  — raw request body.
//   markers string[] (optional)— strings that must NOT appear in the response
//                                body; if one appears, the injection landed.
//
// At least one of body / markers is recommended: without both, a case can
// only prove reachability. That is a WARN, never a failure.
//
// Fail closed: a missing corpus directory, a directory with no .json case
// files, or a case that does not parse or validate is a corpus error (exit
// 2), never a silent pass.
import { readdirSync, readFileSync, statSync } from "node:fs";
import { join } from "node:path";

export function loadCorpus(dir) {
  let entries;
  try {
    const st = statSync(dir);
    if (!st.isDirectory()) return { error: `corpus path is not a directory: ${dir}` };
    entries = readdirSync(dir);
  } catch {
    return { error: `corpus directory not found: ${dir}` };
  }
  const files = entries.filter((e) => e.toLowerCase().endsWith(".json")).sort();
  if (files.length === 0) {
    return { error: `no .json case files in corpus directory: ${dir}` };
  }
  const cases = [];
  for (const file of files) {
    let doc;
    try {
      doc = JSON.parse(readFileSync(join(dir, file), "utf8"));
    } catch (error) {
      return { error: `corpus case unreadable (${file}): ${error.message}` };
    }
    const problem = validateCase(doc);
    if (problem) return { error: `corpus case invalid (${file}): ${problem}` };
    cases.push({ ...doc, file });
  }
  return { cases };
}

function validateCase(c) {
  if (c === null || typeof c !== "object" || Array.isArray(c)) {
    return "case must be a JSON object";
  }
  if (typeof c.name !== "string" || c.name.length === 0) return "missing 'name' (string)";
  if (typeof c.path !== "string" || !c.path.startsWith("/")) {
    return "'path' is required and must start with '/'";
  }
  if (c.method !== undefined && typeof c.method !== "string") {
    return "'method' must be a string";
  }
  if (c.headers !== undefined && (c.headers === null || typeof c.headers !== "object" || Array.isArray(c.headers))) {
    return "'headers' must be an object of header-name to value";
  }
  if (c.body !== undefined && typeof c.body !== "string") return "'body' must be a string";
  if (c.markers !== undefined) {
    if (!Array.isArray(c.markers) || c.markers.some((m) => typeof m !== "string")) {
      return "'markers' must be an array of strings";
    }
  }
  return null;
}
