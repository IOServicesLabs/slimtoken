#!/usr/bin/env node
// slimtoken-trace — sidecar CLI for the SlimToken engine's trace pack.
//
//   slimtoken-trace analyze <trace.jsonl> [--retry-storm K] [--out DIR]
//
// Streams the engine's own trace.jsonl and turns anomalies into a
// SchedulerConfig tuning brief. Zero tokens. Exit 0 pass, 1 findings,
// 2 usage error.
import { fileURLToPath, pathToFileURL } from "node:url";
import { dirname, join } from "node:path";

const lib = join(dirname(fileURLToPath(import.meta.url)), "..", "lib");
const [command, ...rest] = process.argv.slice(2);

switch (command) {
  case "analyze": {
    const { runAnalyze } = await import(pathToFileURL(join(lib, "analyze.mjs")).href);
    process.exit(await runAnalyze(rest));
    break;
  }
  case undefined:
  case "help":
  case "--help":
  case "-h":
    console.log(`slimtoken-trace — analyze trace.jsonl into a SchedulerConfig tuning brief (zero deps)

usage: slimtoken-trace <command> [args]

commands:
  analyze <trace.jsonl> [--retry-storm K] [--out DIR]
          stream a trace and report scheduler anomalies
          e.g. slimtoken-trace analyze run/trace.jsonl --out trace-out

exit codes: 0 pass, 1 findings, 2 usage error`);
    process.exit(command === undefined ? 2 : 0);
    break;
  default:
    console.error(`slimtoken-trace: unknown command '${command}' (try 'slimtoken-trace help')`);
    process.exit(2);
}
