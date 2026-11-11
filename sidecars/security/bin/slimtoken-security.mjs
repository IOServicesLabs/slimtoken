#!/usr/bin/env node
// slimtoken-security — sidecar CLI for the SlimToken engine's security pack.
//
//   slimtoken-security scan <base-url> --corpus <dir> [--allow-remote] [--timeout ms] [--out DIR]
//
// Dispatcher: keep the contract (see sidecars/README.md):
//   - exit 0: no findings
//   - exit 1: at least one finding (crash or marker)
//   - exit 2: usage error, refused remote target, or nothing was tested
// Zero runtime dependencies; Node 22+.
import { fileURLToPath, pathToFileURL } from "node:url";
import { dirname, join } from "node:path";

const lib = join(dirname(fileURLToPath(import.meta.url)), "..", "lib");
const [command, ...rest] = process.argv.slice(2);

switch (command) {
  case "scan": {
    const { runScan } = await import(pathToFileURL(join(lib, "scan.mjs")).href);
    process.exit(await runScan(rest));
    break;
  }
  case undefined:
  case "help":
  case "--help":
  case "-h":
    console.log(`slimtoken-security — attack-corpus replay gate for the SlimToken engine (zero deps)

usage: slimtoken-security <command> [args]

commands:
  scan      replay an attack corpus against a running app, e.g.
            slimtoken-security scan http://localhost:3000 --corpus corpus/ --out sec-out

            flags: --corpus <dir> (required), --allow-remote,
                   --timeout <ms> (default 10000), --out <dir>

ownership rule: only scan hosts you own. The default posture is localhost
(127.0.0.1, ::1). A remote base URL is refused (exit 2) unless
--allow-remote is passed.

exit codes: 0 pass, 1 findings, 2 usage error / refused remote / nothing tested`);
    process.exit(command === undefined ? 2 : 0);
    break;
  default:
    console.error(`slimtoken-security: unknown command '${command}' (try 'slimtoken-security help')`);
    process.exit(2);
}
