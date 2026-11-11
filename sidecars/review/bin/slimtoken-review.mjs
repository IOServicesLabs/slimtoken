#!/usr/bin/env node
// slimtoken-review — sidecar CLI for the SlimToken engine's review pack.
//
//   slimtoken-review lint <dir> [--out DIR]
//
// The lint gate shells out to real tools (cargo clippy / cargo check /
// node --check) over a source tree. It never calls a model: zero tokens.
// Contract (see sidecars/README.md): exit 0 pass, 1 findings, 2 usage
// error. Zero runtime dependencies; Node 22+.
import { fileURLToPath, pathToFileURL } from "node:url";
import { dirname, join } from "node:path";

const lib = join(dirname(fileURLToPath(import.meta.url)), "..", "lib");
const [command, ...rest] = process.argv.slice(2);

switch (command) {
  case "lint": {
    const { runLint } = await import(pathToFileURL(join(lib, "lint.mjs")).href);
    process.exit(runLint(rest));
    break;
  }
  case undefined:
  case "help":
  case "--help":
  case "-h":
    console.log(`slimtoken-review — review lint sidecar for the SlimToken engine (zero deps)

usage: slimtoken-review <command> [args]

commands:
  lint      Zero-token lint gate over a source tree. Runs cargo clippy (or
            cargo check as fallback) on every crate and node --check on every
            .js/.mjs/.cjs file. Writes report.md + review.json with --out, e.g.
            slimtoken-review lint build/ --out review-out

exit codes: 0 pass, 1 findings, 2 usage error`);
    process.exit(command === undefined ? 2 : 0);
    break;
  default:
    console.error(`slimtoken-review: unknown command '${command}' (try 'slimtoken-review help')`);
    process.exit(2);
}
