// util.mjs — small shared helpers for the slimtoken-coverage gates.
import { readdirSync, statSync } from "node:fs";
import { join } from "node:path";

export const TIMEOUT_MS = 300_000;

export function* walkFiles(dir) {
  let names;
  try {
    names = readdirSync(dir);
  } catch {
    return;
  }
  for (const name of names) {
    if (name === "node_modules" || name.startsWith(".")) continue;
    const full = join(dir, name);
    let st;
    try {
      st = statSync(full);
    } catch {
      continue;
    }
    if (st.isDirectory()) yield* walkFiles(full);
    else yield full;
  }
}

export function baseName(file) {
  return file.slice(file.lastIndexOf("/") + 1).slice(file.lastIndexOf("\\") + 1);
}

export function round2(x) {
  return Math.round(x * 100) / 100;
}

export function pctOf(covered, total) {
  return total === 0 ? 100 : round2((100 * covered) / total);
}

export function sumFiles(files) {
  let covered = 0;
  let total = 0;
  for (const f of files) {
    covered += f.covered;
    total += f.total;
  }
  return { covered, total, pct: pctOf(covered, total) };
}

export function combinedOutput(result) {
  return ((result.stdout || "") + "") + ((result.stderr || "") + "");
}

export function lastNonEmptyLine(text) {
  const lines = String(text)
    .split(/\r?\n/)
    .map((l) => l.trim())
    .filter((l) => l.length > 0);
  return lines.length > 0 ? lines[lines.length - 1] : "";
}
