#!/usr/bin/env node
// slimtoken-fuzz — sidecar CLI for the SlimToken engine's fuzz pack.
//
//   slimtoken-fuzz run <target.mjs> --seeds <dir>
//       [--cases N] [--seconds S] [--seed R] [--keep-going] [--out DIR]
//
// The target module must export `fuzz(input: string): void` (sync or
// async). The pack mutates the seed corpus with a deterministic PRNG and
// calls target.fuzz in-process. A throw is a finding. Zero runtime
// dependencies; Node 22+. Zero model tokens.
import { fileURLToPath, pathToFileURL } from "node:url";
import { dirname, join } from "node:path";

const lib = join(dirname(fileURLToPath(import.meta.url)), "..", "lib");
const { main } = await import(pathToFileURL(join(lib, "fuzz.mjs")).href);

const code = await main(process.argv.slice(2));
process.exit(code);
