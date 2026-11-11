#!/usr/bin/env node
// slimtoken-research — sidecar CLI for the SlimToken engine's research pack.
//
//   slimtoken-research brief <questions-file> --out plan.json
//   slimtoken-research collect <run-output-dir> --out report.md
//   slimtoken-research verify <report.md> [--out DIR]
//
// `brief` turns a list of research sub-questions into an engine plan (one
// researcher task per question). `collect` merges the per-task briefs the
// engine wrote into one report with a deduped Sources section. `verify`
// is the gate: it fetches every citation and checks that every
// load-bearing claim appears in its source. Zero model tokens; fetch and
// string matching only. Zero runtime dependencies; Node 22+.
import { fileURLToPath, pathToFileURL } from "node:url";
import { dirname, join } from "node:path";

const lib = join(dirname(fileURLToPath(import.meta.url)), "..", "lib");
const [command, ...rest] = process.argv.slice(2);

async function load(name) {
  // pathToFileURL: dynamic import needs a file URL on Windows.
  return import(pathToFileURL(join(lib, name)).href);
}

switch (command) {
  case "brief": {
    const { runBrief } = await load("brief.mjs");
    process.exit(await runBrief(rest));
    break;
  }
  case "collect": {
    const { runCollect } = await load("collect.mjs");
    process.exit(await runCollect(rest));
    break;
  }
  case "verify": {
    const { runVerify } = await load("verify.mjs");
    // Natural exit, no hard process.exit: undici keep-alive sockets close
    // during loop drain, and a hard exit while one is closing aborts on
    // Windows (uv async.c handle-closing assert).
    process.exitCode = await runVerify(rest);
    break;
  }
  case undefined:
  case "help":
  case "--help":
  case "-h":
    console.log(`slimtoken-research — research citation gate for the SlimToken engine (zero deps)

usage: slimtoken-research <command> [args]

commands:
  brief     Build an engine plan: one researcher task per sub-question, e.g.
            slimtoken-research brief questions.txt --out plan.json
  collect   Merge the run dir's per-task briefs into one report, e.g.
            slimtoken-research collect run-output/ --out report.md
  verify    The gate: fetch every citation, check each load-bearing
            claim appears in its source, e.g.
            slimtoken-research verify report.md --out verify-out

exit codes: 0 pass, 1 findings, 2 usage error`);
    process.exit(command === undefined ? 2 : 0);
    break;
  default:
    console.error(`slimtoken-research: unknown command '${command}' (try 'slimtoken-research help')`);
    process.exit(2);
}
