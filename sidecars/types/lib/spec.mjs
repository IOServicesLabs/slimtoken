// Spec loading and schema-set extraction for the types pack.
//
// A spec file is JSON. It can be an OpenAPI document (top-level `openapi`
// or `swagger` string) — then the schema set is the keys of
// `components.schemas` (OpenAPI 3) or `definitions` (Swagger 2.0) — or a
// plain object whose values look like JSON schemas (objects with `type`,
// `properties`, or `$ref`), where the keys of the schema-like values are
// the schema set. A valid spec with zero schemas is a WARN SKIP, not an
// error.
//
// The $ref walk mirrors sidecars/contract/lib/check.mjs: every local
// pointer (#/a/b) must resolve to a real node. External refs (other.json#/…
// or URLs) are not resolvable offline and are WARN SKIP, never a failure.
import { readFileSync } from "node:fs";

// Thrown for usage-level input errors (unreadable file, JSON parse
// failure). The caller maps this to exit 2.
export class SpecError extends Error {}

export function loadSpec(file) {
  let text;
  try {
    text = readFileSync(file, "utf8");
  } catch (error) {
    throw new SpecError(`cannot read spec '${file}': ${error.message}`);
  }
  try {
    return JSON.parse(text);
  } catch (error) {
    throw new SpecError(`spec parse error in '${file}': ${error.message}`);
  }
}

export function isOpenApi(doc) {
  if (doc === null || typeof doc !== "object" || Array.isArray(doc)) return false;
  return typeof doc.openapi === "string" || typeof doc.swagger === "string";
}

function looksLikeSchema(value) {
  if (value === null || typeof value !== "object" || Array.isArray(value)) return false;
  return (
    typeof value.type === "string" ||
    (typeof value.properties === "object" && value.properties !== null) ||
    typeof value.$ref === "string"
  );
}

// Returns { kind, schemas }. kind is "openapi", "map", or "none" (the
// document is not an object at all). schemas is the sorted-free key order
// of the document itself.
export function extractSchemas(doc) {
  if (doc === null || typeof doc !== "object" || Array.isArray(doc)) {
    return { kind: "none", schemas: [] };
  }
  if (isOpenApi(doc)) {
    let bucket = null;
    if (
      doc.components !== null && typeof doc.components === "object" &&
      doc.components.schemas !== null && typeof doc.components.schemas === "object" &&
      !Array.isArray(doc.components.schemas)
    ) {
      bucket = doc.components.schemas;
    } else if (
      doc.definitions !== null && typeof doc.definitions === "object" &&
      !Array.isArray(doc.definitions)
    ) {
      bucket = doc.definitions;
    }
    return { kind: "openapi", schemas: bucket === null ? [] : Object.keys(bucket) };
  }
  const schemas = [];
  for (const [key, value] of Object.entries(doc)) {
    if (looksLikeSchema(value)) schemas.push(key);
  }
  return { kind: "map", schemas };
}

// Every string value of a "$ref" key, at any depth. Cycles are guarded.
export function collectRefs(doc) {
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
export function resolvePointer(doc, ref) {
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

// Split the refs of a spec into { dangling, external, internal }. A "#"
// self-ref and empty pointer are ignored. Local refs that fail to resolve
// are dangling (hard ERROR); anything not starting with "#/" is external
// (WARN SKIP, not resolvable offline).
export function auditRefs(doc) {
  const dangling = [];
  const external = [];
  for (const ref of collectRefs(doc)) {
    if (ref === "#") continue;
    if (ref.startsWith("#/")) {
      if (!resolvePointer(doc, ref)) dangling.push(ref);
    } else {
      external.push(ref);
    }
  }
  return { dangling, external };
}
