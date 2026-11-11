#!/usr/bin/env node
// slimtoken-qa — sidecar CLI for the SlimToken engine's browser QA pack.
//
//   slimtoken-qa smoke  --url URL --routes /,/review [--out DIR] [audit flags]
//   slimtoken-qa flow   --url URL --flow flow.json [--out DIR]
//   slimtoken-qa validate <file-or-dir> [...]   (zero-token syntax gate)
//
// `smoke` and `flow` are the packaged qa-smoke.mjs / qa-flow.mjs; all
// flags pass through verbatim. `validate` closes the artifact-validation
// hole where syntactically invalid code passes shape checks (see
// lib/validate.mjs). Zero runtime dependencies; Node 22+.
import { fileURLToPath, pathToFileURL } from "node:url";
import { dirname, join } from "node:path";
import { spawnSync } from "node:child_process";

const lib = join(dirname(fileURLToPath(import.meta.url)), "..", "lib");
const [command, ...rest] = process.argv.slice(2);

function run(script, args) {
  const result = spawnSync(process.execPath, [join(lib, script), ...args], {
    stdio: "inherit",
  });
  process.exit(result.status ?? 1);
}

switch (command) {
  case "smoke":
    run("qa-smoke.mjs", rest);
    break;
  case "flow":
    run("qa-flow.mjs", rest);
    break;
  case "validate": {
    const { runValidate } = await import(pathToFileURL(join(lib, "validate.mjs")).href);
    process.exit(runValidate(rest));
    break;
  }
  case undefined:
  case "help":
  case "--help":
  case "-h":
    console.log(`slimtoken-qa — browser QA sidecar for the SlimToken engine (zero deps)

usage: slimtoken-qa <command> [args]

commands:
  smoke     Load gate: every route, console, network, visibility, screenshot.
            Same flags as qa-smoke.mjs, e.g.
            slimtoken-qa smoke --url http://localhost:3000 --routes /,/review --out qa-out
  flow      Interaction gate: scripted user flows over CDP, e.g.
            slimtoken-qa flow --url http://localhost:3000 --flow flow.json --out qa-flow-out
  validate  Zero-token syntax gate for generated artifacts (py/js/mjs/cjs), e.g.
            slimtoken-qa validate src/app.py lib/util.mjs
            slimtoken-qa validate --strict build/        # scan a directory

exit codes: 0 ok, 1 findings or syntax errors, 2 usage error`);
    process.exit(command === undefined ? 2 : 0);
    break;
  default:
    console.error(`slimtoken-qa: unknown command '${command}' (try 'slimtoken-qa help')`);
    process.exit(2);
}
