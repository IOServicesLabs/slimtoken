#!/usr/bin/env node
// slimtoken-triage — sidecar CLI for the SlimToken engine's triage pack.
//
//   slimtoken-triage candidates <failure-file> [--k N] [--out DIR] [--max-bytes N]
//   slimtoken-triage repro <candidates.json> [--workdir DIR] [--out DIR]
//
// On a gate failure, `candidates` drafts K diagnoses in ONE model call;
// `repro` proves them with shell commands (zero model tokens). A candidate
// whose repro exits non-zero is CONFIRMED. This is a Tier 3 pack: it
// spends tokens by design, one call per `candidates` run (see README).
// Zero runtime dependencies; Node 22+.
import { fileURLToPath, pathToFileURL } from "node:url";
import { dirname, join } from "node:path";

const lib = join(dirname(fileURLToPath(import.meta.url)), "..", "lib");
const [command, ...rest] = process.argv.slice(2);

switch (command) {
  case "candidates": {
    const { runCandidates } = await import(pathToFileURL(join(lib, "candidates.mjs")).href);
    process.exit(await runCandidates(rest));
    break;
  }
  case "repro": {
    const { runRepro } = await import(pathToFileURL(join(lib, "repro.mjs")).href);
    process.exit(await runRepro(rest));
    break;
  }
  case undefined:
  case "help":
  case "--help":
  case "-h":
    console.log(`slimtoken-triage — draft candidate diagnoses for a gate failure and prove them with zero-token repros

usage: slimtoken-triage <command> [args]

commands:
  candidates <failure-file> [--k N] [--out DIR] [--max-bytes N]
             ask the model for N candidate diagnoses (ONE model call per run;
             K diagnoses in one response, not K calls)
  repro      <candidates.json> [--workdir DIR] [--out DIR]
             run each candidate's repro shell command (ZERO model tokens)

environment (candidates only, keys read from the environment):
  TRIAGE_BASE_URL   OpenAI-compatible base URL, e.g. https://api.example.com/v1
  TRIAGE_API_KEY    bearer key for the API
  TRIAGE_MODEL      model name to call

exit codes: 0 ok, 1 transport/HTTP/parse error or no candidate confirmed,
            2 usage error (bad args, missing env, unreadable/malformed input)`);
    process.exit(command === undefined ? 2 : 0);
    break;
  default:
    console.error(`slimtoken-triage: unknown command '${command}' (try 'slimtoken-triage help')`);
    process.exit(2);
}
