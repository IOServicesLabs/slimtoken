#!/usr/bin/env node
// slimtoken-types — sidecar CLI for the SlimToken engine's types pack.
//
//   slimtoken-types check <spec-file> <bindings-dir> [--lang auto|ts|py] [--out DIR]
//
// `check` proves that drafted type bindings exist for every schema in a
// spec (OpenAPI or plain schema map) and that they compile (tsc for
// .ts/.d.ts, python -m py_compile for .py). Zero tokens, zero runtime
// dependencies; Node 22+.
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
    console.log(`slimtoken-types — type binding gate for the SlimToken engine (zero deps)

usage: slimtoken-types <command> [args]

commands:
  check     Prove bindings exist for every spec schema and compile, e.g.
            slimtoken-types check api/openapi.json bindings/
            slimtoken-types check spec.json bindings/ --lang ts --out types-out

exit codes: 0 pass, 1 findings, 2 usage error`);
    process.exit(command === undefined ? 2 : 0);
    break;
  default:
    console.error(`slimtoken-types: unknown command '${command}' (try 'slimtoken-types help')`);
    process.exit(2);
}
