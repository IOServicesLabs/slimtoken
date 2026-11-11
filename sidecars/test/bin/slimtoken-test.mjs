#!/usr/bin/env node
// slimtoken-test — sidecar CLI for the SlimToken engine's test pack.
//
//   slimtoken-test run <dir> [--out DIR]
//
// The run gate detects the test stack in <dir> (Python tests or Node
// tests, or both), shells out to the real runner (pytest / unittest /
// npm test / node --test), and prints a per-suite verdict. Zero model
// tokens: it never calls a network LLM. Zero runtime dependencies;
// Node 22+.
import { fileURLToPath, pathToFileURL } from "node:url";
import { dirname, join } from "node:path";

const lib = join(dirname(fileURLToPath(import.meta.url)), "..", "lib");
const [command, ...rest] = process.argv.slice(2);

switch (command) {
  case "run": {
    const { runTests } = await import(pathToFileURL(join(lib, "run.mjs")).href);
    process.exit(runTests(rest));
    break;
  }
  case undefined:
  case "help":
  case "--help":
  case "-h":
    console.log(`slimtoken-test — test sidecar for the SlimToken engine (zero deps)

usage: slimtoken-test <command> [args]

commands:
  run       Detect and run the test suites in a directory, e.g.
            slimtoken-test run drafted-tests/
            slimtoken-test run drafted-tests/ --out gate-report/

exit codes: 0 all suites passed (or nothing to run), 1 a suite failed,
            2 usage error`);
    process.exit(command === undefined ? 2 : 0);
    break;
  default:
    console.error(`slimtoken-test: unknown command '${command}' (try 'slimtoken-test help')`);
    process.exit(2);
}
