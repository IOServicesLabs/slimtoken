// Example gate for the sidecar template. Replace with the real gate.
//
// Contract (see sidecars/README.md):
//   - exit 0: every checked artifact passed
//   - exit 1: at least one artifact failed a gate
//   - exit 2: usage error (no input)
//   - zero tokens: shell out to a real tool, never call a model
import { existsSync, statSync, readFileSync } from "node:fs";
import { extname } from "node:path";

export function runGate(args) {
  const files = args.filter((a) => !a.startsWith("--"));
  if (files.length === 0) {
    console.error("gate: no input files (usage: slimtoken-<name> gate <file-or-dir> [...])");
    return 2;
  }
  let bad = 0;
  for (const file of files) {
    const result = checkOne(file);
    if (result.ok) console.log(`OK   ${file}`);
    else { console.log(`ERROR ${file}: ${result.error}`); bad += 1; }
  }
  console.log(`gate: ${files.length - bad}/${files.length} passed`);
  return bad > 0 ? 1 : 0;
}

// Example check: the file exists and is not empty. A real gate shells out
// to the domain tool here (py_compile, node --check, a test runner, CDP)
// and parses the tool verdict.
function checkOne(file) {
  try {
    if (!existsSync(file)) return { ok: false, error: "missing" };
    const text = readFileSync(file, "utf8");
    if (text.trim().length === 0) return { ok: false, error: "empty" };
    return { ok: true };
  } catch (error) {
    return { ok: false, error: String(error.message || error) };
  }
}
