#!/usr/bin/env node
// slimtoken-contract — sidecar CLI for the SlimToken engine's contract pack.
//
//   slimtoken-contract check <files-or-dirs...> [--out DIR]
//
// `check` proves that JSON specs parse and that OpenAPI specs resolve:
// info fields present, paths present, every local $ref walks to a real
// node. Zero tokens, zero runtime dependencies; Node 22+.
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
    console.log(`slimtoken-contract — OpenAPI contract gate for the SlimToken engine (zero deps)

usage: slimtoken-contract <command> [args]

commands:
  check     Prove JSON specs parse and OpenAPI specs resolve, e.g.
            slimtoken-contract check api/openapi.json specs/
            slimtoken-contract check specs/ --out contract-out

exit codes: 0 pass, 1 findings, 2 usage error`);
    process.exit(command === undefined ? 2 : 0);
    break;
  default:
    console.error(`slimtoken-contract: unknown command '${command}' (try 'slimtoken-contract help')`);
    process.exit(2);
}
