// store.mjs — JSONL ledger read and append.
//
// One JSON object per line. Appends use an explicit "\n": Windows text
// mode would write "\r\n" and break line-based readers elsewhere.
import { appendFileSync, existsSync, readFileSync } from "node:fs";

// Load every row. A missing file is an error (the caller exits 2); a
// blank line is skipped; an unparseable line throws with its number —
// a ledger that cannot be read must not be silently truncated.
export function loadLedger(file) {
  if (!existsSync(file)) {
    throw new Error(`ledger not found: ${file}`);
  }
  const text = readFileSync(file, "utf8");
  const rows = [];
  const lines = text.split(/\r?\n/);
  for (let i = 0; i < lines.length; i += 1) {
    const line = lines[i].trim();
    if (line === "") continue;
    let row;
    try {
      row = JSON.parse(line);
    } catch (error) {
      throw new Error(`${file}:${i + 1}: unparseable row: ${error.message}`);
    }
    if (!row || typeof row !== "object" || Array.isArray(row)) {
      throw new Error(`${file}:${i + 1}: row is not a JSON object`);
    }
    rows.push(row);
  }
  return rows;
}

export function appendRow(file, row) {
  appendFileSync(file, JSON.stringify(row) + "\n", "utf8");
}

// One row per arm: when an arm has several rows, the LATEST (last
// appended) wins. That is the current state of the arm.
export function latestByArm(rows) {
  const byArm = new Map();
  for (const row of rows) {
    if (typeof row.arm !== "string" || row.arm === "") continue;
    byArm.set(row.arm, row);
  }
  return byArm;
}
