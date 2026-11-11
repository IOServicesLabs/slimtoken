#!/usr/bin/env node
// slimtoken-replay CLI dispatcher.
// Commands: diff, help. Exit codes: 0 pass, 1 findings, 2 usage error.

import { runDiff } from "../lib/diff.mjs";

const USAGE = `slimtoken-replay - paired per-task diff of two engine run output dirs

usage: slimtoken-replay <command> [args]

commands:
  diff <run-a-dir> <run-b-dir> [--tokens-tolerance N] [--out DIR]
      Compare two run output dirs on one plan. Exit 0 = no findings,
      1 = findings, 2 = usage error or unreadable inputs.
      --tokens-tolerance N  output-token increase tolerance in percent
                            (default 10). Duration tolerance is 25%.
      --out DIR             write report.md and replay.json into DIR.
  help
      Print this text. Exit 0.
`;

async function main() {
  const [command, ...rest] = process.argv.slice(2);
  if (command === undefined) {
    process.stderr.write(USAGE);
    return 2;
  }
  if (command === "help" || command === "--help" || command === "-h") {
    process.stdout.write(USAGE);
    return 0;
  }
  if (command === "diff") {
    return await runDiff(rest);
  }
  process.stderr.write(`slimtoken-replay: unknown command "${command}"\n\n`);
  process.stderr.write(USAGE);
  return 2;
}

main().then((code) => {
  process.exitCode = code;
});
