#!/usr/bin/env node
// slimtoken-<name> — sidecar CLI for the SlimToken engine's <name> pack.
//
//   slimtoken-<name> gate <file-or-dir> [...]
//
// Template dispatcher: replace the example `gate` command with the real
// gates. Keep the contract: exit 0 pass, 1 findings, 2 usage error.
// Zero runtime dependencies; Node 22+.
import { fileURLToPath, pathToFileURL } from "node:url";
import { dirname, join } from "node:path";

const lib = join(dirname(fileURLToPath(import.meta.url)), "..", "lib");
const [command, ...rest] = process.argv.slice(2);

switch (command) {
  case "gate": {
    const { runGate } = await import(pathToFileURL(join(lib, "gate.mjs")).href);
    process.exit(runGate(rest));
    break;
  }
  case undefined:
  case "help":
  case "--help":
  case "-h":
    console.log(`slimtoken-<name> — <one line: what this pack does> (zero deps)

usage: slimtoken-<name> <command> [args]

commands:
  gate      <what the gate checks>, e.g.
            slimtoken-<name> gate build/

exit codes: 0 pass, 1 findings, 2 usage error`);
    process.exit(command === undefined ? 2 : 0);
    break;
  default:
    console.error(`slimtoken-<name>: unknown command '${command}' (try 'slimtoken-<name> help')`);
    process.exit(2);
}
