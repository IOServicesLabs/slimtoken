// out.mjs — write the judge output pair (report.md + machine JSON twin).
import { mkdirSync, writeFileSync } from "node:fs";
import { join } from "node:path";

// Contract (see sidecars/README.md): every gate emits a human report and a
// machine twin with stable field names; new fields append at the end.
export function writeOut(dir, jsonName, reportMd, twin) {
  mkdirSync(dir, { recursive: true });
  writeFileSync(join(dir, "report.md"), reportMd);
  writeFileSync(join(dir, jsonName), JSON.stringify(twin, null, 2) + "\n");
}
