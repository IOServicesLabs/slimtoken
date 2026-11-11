// Collect PNG screenshots from a directory tree, sorted for a stable run.
import { readdirSync } from "node:fs";
import { join } from "node:path";

// The 8-byte PNG signature. Files without it never reach the vision model:
// a corrupt screenshot would spend a call and return noise.
const PNG_SIG = Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]);

export function isPng(buf) {
  return Buffer.isBuffer(buf) && buf.length >= PNG_SIG.length && buf.subarray(0, PNG_SIG.length).equals(PNG_SIG);
}

export function collectPngs(dir) {
  const out = [];
  walk(dir, out);
  return out.sort();
}

function walk(dir, out) {
  for (const entry of readdirSync(dir, { withFileTypes: true })) {
    const p = join(dir, entry.name);
    if (entry.isDirectory()) {
      walk(p, out);
    } else if (entry.isFile() && entry.name.toLowerCase().endsWith(".png")) {
      out.push(p);
    }
  }
}
