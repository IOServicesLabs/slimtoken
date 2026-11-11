#!/usr/bin/env node
// slimtoken-coverage — sidecar CLI for the SlimToken engine's coverage pack.
//
//   slimtoken-coverage measure <dir> [--runner node|python] [--out DIR]
//   slimtoken-coverage delta <before.json> <after.json> [--min-gain N] [--out DIR]
//
// The measure gate detects the test stack in <dir> (node:test or pytest /
// unittest), runs the suite with line coverage, and prints per-file
// {file, covered, total, pct} plus a suite total. The delta gate reads two
// coverage.json files and exits 0 only when the total percent grew by at
// least N points. Zero model tokens: it never calls a network LLM. Zero
// runtime dependencies; Node 22+.
import { fileURLToPath, pathToFileURL } from "node:url";
import { dirname, join } from "node:path";

const lib = join(dirname(fileURLToPath(import.meta.url)), "..", "lib");
const [command, ...rest] = process.argv.slice(2);

switch (command) {
  case "measure": {
    const { runMeasure } = await import(pathToFileURL(join(lib, "measure.mjs")).href);
    process.exit(runMeasure(rest));
    break;
  }
  case "delta": {
    const { runDelta } = await import(pathToFileURL(join(lib, "delta.mjs")).href);
    process.exit(runDelta(rest));
    break;
  }
  case undefined:
  case "help":
  case "--help":
  case "-h":
    console.log(`slimtoken-coverage — coverage sidecar for the SlimToken engine (zero deps)

usage: slimtoken-coverage <command> [args]

commands:
  measure   Detect and run the suite in a directory with line coverage,
            e.g. slimtoken-coverage measure app/
                 slimtoken-coverage measure app/ --runner node
                 slimtoken-coverage measure app/ --out gate-report/
  delta     Prove coverage grew between two coverage.json files,
            e.g. slimtoken-coverage delta before.json after.json
                 slimtoken-coverage delta before.json after.json --min-gain 5

exit codes: 0 coverage ran (measure) or the delta passed (delta),
            1 the delta did not reach the minimum gain,
            2 usage error (bad args, missing dir, no suite found)`);
    process.exit(command === undefined ? 2 : 0);
    break;
  default:
    console.error(`slimtoken-coverage: unknown command '${command}' (try 'slimtoken-coverage help')`);
    process.exit(2);
}
