#!/usr/bin/env node
// slimtoken-docs — sidecar CLI for the SlimToken engine's docs pack.
//
//   slimtoken-docs check <dir-or-file> [...] [--out DIR]
//
// The `check` command is the docs drift gate: relative links resolve,
// code fences balance, files are readable. Zero runtime dependencies;
// Node 22+. Exit codes: 0 pass, 1 findings, 2 usage error.
import { fileURLToPath, pathToFileURL } from "node:url";
import { dirname, join } from "node:path";

const lib = join(dirname(fileURLToPath(import.meta.url)), "..", "lib");
const [command, ...rest] = process.argv.slice(2);

switch (command) {
  case "check": {
    const { runCheck } = await import(pathToFileURL(join(lib, "check.mjs")).href);
    process.exit(runCheck(rest));
    break;
  }
  case undefined:
  case "help":
  case "--help":
  case "-h":
    console.log(`slimtoken-docs — docs drift gate for the SlimToken engine (zero deps)

usage: slimtoken-docs <command> [args]

commands:
  check     Docs gate: relative links resolve, code fences balance,
            files readable. Style lints report as WARN (never fail), e.g.
            slimtoken-docs check docs/
            slimtoken-docs check README.md guide/setup.md --out docs-report

exit codes: 0 pass, 1 findings, 2 usage error`);
    process.exit(command === undefined ? 2 : 0);
    break;
  default:
    console.error(`slimtoken-docs: unknown command '${command}' (try 'slimtoken-docs help')`);
    process.exit(2);
}
