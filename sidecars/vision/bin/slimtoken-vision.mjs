#!/usr/bin/env node
// slimtoken-vision — sidecar CLI for the SlimToken engine's vision pack.
//
//   slimtoken-vision score <shots-dir> --rubric <file> [--out DIR]
//       [--max-images N] [--max-image-bytes N]
//
// This pack spends model tokens by design: one vision call per screenshot.
// Keys come only from the environment: VISION_BASE_URL, VISION_API_KEY,
// VISION_MODEL. Zero runtime dependencies; Node 22+.
//
// Exit codes: 0 at least one image scored, 1 zero images scored (the run
// judged nothing), 2 usage error / missing env / rubric unreadable / no
// images found.
import { fileURLToPath, pathToFileURL } from "node:url";
import { dirname, join } from "node:path";

const lib = join(dirname(fileURLToPath(import.meta.url)), "..", "lib");
const [command, ...rest] = process.argv.slice(2);

switch (command) {
  case "score": {
    const { runScore } = await import(pathToFileURL(join(lib, "score.mjs")).href);
    process.exit(await runScore(rest));
    break;
  }
  case undefined:
    console.error("slimtoken-vision: no command given (try 'slimtoken-vision help')");
    process.exit(2);
    break;
  case "help":
  case "--help":
  case "-h":
    console.log(`slimtoken-vision — screenshot look judge for the SlimToken engine
(spends model tokens by design; budget-capped)

usage: slimtoken-vision <command> [args]

commands:
  score     Judge every PNG under <shots-dir> against the rubric, one
            vision call per image (OpenAI-compatible endpoint from
            $VISION_BASE_URL, model $VISION_MODEL, key $VISION_API_KEY).
            slimtoken-vision score shots/ --rubric brand.txt --out vision-report
            slimtoken-vision score shots/ --rubric brand.txt --max-images 10

options:
  --rubric <file>          Scoring instructions (required).
  --out DIR                Write report.md + vision.json.
  --max-images N           Judge at most N images (default 20).
  --max-image-bytes N      Skip files over N bytes with a WARN (default 1500000).

exit codes: 0 at least one image scored, 1 zero images scored
(the run judged nothing — a vision outage must not read as a pass),
2 usage error / missing env / rubric unreadable / no images found`);
    process.exit(0);
    break;
  default:
    console.error(`slimtoken-vision: unknown command '${command}' (try 'slimtoken-vision help')`);
    process.exit(2);
}
