#!/usr/bin/env node
// slimtoken-judge — sidecar CLI for the SlimToken engine's judge pack.
//
//   slimtoken-judge brief <dirs...> [--out DIR] [--max-input-bytes N]
//   slimtoken-judge judge --brief brief.md [--out DIR] [--max-input-bytes N]
//
// `brief` merges every pack JSON twin it finds in the given directories
// into one compact verdict brief (brief.md). `judge` sends that brief to
// one judge model call and parses the steer directives the model returns
// into steer.json. Exactly ONE model call per invocation — this pack
// spends tokens by design (see README.md, COST section).
import { fileURLToPath, pathToFileURL } from "node:url";
import { dirname, join } from "node:path";

const lib = join(dirname(fileURLToPath(import.meta.url)), "..", "lib");
const [command, ...rest] = process.argv.slice(2);

async function load(name) {
  // pathToFileURL: dynamic import needs a file URL on Windows.
  return import(pathToFileURL(join(lib, name)).href);
}

switch (command) {
  case "brief": {
    const { runBrief } = await load("brief.mjs");
    process.exit(await runBrief(rest));
    break;
  }
  case "judge": {
    const { runJudge } = await load("judge.mjs");
    // Natural exit, no hard process.exit: undici keep-alive sockets close
    // during loop drain, and a hard exit while one is closing aborts on
    // Windows (uv async.c handle-closing assert). The judge request sends
    // Connection: close so the socket shuts down right after the reply.
    process.exitCode = await runJudge(rest);
    break;
  }
  case undefined:
  case "help":
  case "--help":
  case "-h":
    console.log(`slimtoken-judge — judge layer for the SlimToken engine (one model call per run)

usage: slimtoken-judge <command> [args]

commands:
  brief   Merge every pack JSON twin in <dirs...> into one verdict brief, e.g.
          slimtoken-judge brief run-out/ pack-out/ --out judge-in
  judge   Send the brief to ONE judge model call, parse steer directives, e.g.
          slimtoken-judge judge --brief judge-in/brief.md --out judge-out

cost: exactly one judge model call per 'judge' invocation. Keys come only
from the environment: JUDGE_BASE_URL, JUDGE_API_KEY, JUDGE_MODEL.

exit codes: 0 pass, 1 judge/transport failure, 2 usage error`);
    process.exit(command === undefined ? 2 : 0);
    break;
  default:
    console.error(`slimtoken-judge: unknown command '${command}' (try 'slimtoken-judge help')`);
    process.exit(2);
}
