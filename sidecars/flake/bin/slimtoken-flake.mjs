#!/usr/bin/env node
// slimtoken-flake — sidecar CLI for the SlimToken engine's flake pack.
//
//   slimtoken-flake probe <dir> [--runs N] [--out DIR]
//
// The probe gate detects the test suite in <dir> (same rules as the test
// pack), runs it N times, and names the tests that pass in some runs and
// fail in others (FLAKY) or fail in every run (BROKEN). Zero model
// tokens: it shells out to node --test or pytest and reads the JUnit XML
// report. It never calls a network LLM. Zero runtime dependencies;
// Node 22+.
import { fileURLToPath, pathToFileURL } from "node:url";
import { dirname, join } from "node:path";

const lib = join(dirname(fileURLToPath(import.meta.url)), "..", "lib");
const [command, ...rest] = process.argv.slice(2);

switch (command) {
  case "probe": {
    const { runProbe } = await import(pathToFileURL(join(lib, "probe.mjs")).href);
    process.exit(await runProbe(rest));
    break;
  }
  case undefined:
  case "help":
  case "--help":
  case "-h":
    console.log(`slimtoken-flake — flake sidecar for the SlimToken engine (zero deps)

usage: slimtoken-flake <command> [args]

commands:
  probe     Run a suite N times and name the nondeterministic tests, e.g.
            slimtoken-flake probe tests/
            slimtoken-flake probe tests/ --runs 10 --out gate-report/

exit codes: 0 every test passed every run, 1 at least one FLAKY/BROKEN test,
            2 usage error, no suite found, or nothing measurable`);
    process.exit(command === undefined ? 2 : 0);
    break;
  default:
    console.error(`slimtoken-flake: unknown command '${command}' (try 'slimtoken-flake help')`);
    process.exit(2);
}
