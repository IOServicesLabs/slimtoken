#!/usr/bin/env node
// slimtoken-migrate — sidecar CLI for the SlimToken engine's migrate pack.
//
//   slimtoken-migrate check <dir> [--build] [--out DIR]
//
// The migration gate: after a migration wave, prove every file still
// compiles. Syntax checks (.py -> py_compile, .js/.mjs/.cjs -> node --check)
// always run on every source file under <dir>. Manifest checks run when a
// manifest is present at the top level of <dir>: Cargo.toml -> cargo check,
// package.json -> npm run build (only with --build, else a WARN SKIP).
// Zero runtime dependencies; Node 22+. Zero model tokens: only python,
// node, cargo, npm are shelled out to.
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
    console.log(`slimtoken-migrate — migration gate sidecar for the SlimToken engine (zero deps)

usage: slimtoken-migrate <command> [args]

commands:
  check <dir> [--build] [--out DIR]
            Migration gate. Syntax-checks every .py/.js/.mjs/.cjs under <dir>
            (always). Runs the manifest checks detected at the top level of
            <dir>: 'cargo check --quiet' for Cargo.toml, 'npm run build
            --if-present' for package.json (only with --build; without it a
            WARN SKIP line is printed). --out DIR also writes report.md and
            migrate.json next to each other.

exit codes: 0 pass (no ERROR), 1 at least one ERROR, 2 usage error`);
    process.exit(command === undefined ? 2 : 0);
    break;
  default:
    console.error(`slimtoken-migrate: unknown command '${command}' (try 'slimtoken-migrate help')`);
    process.exit(2);
}
