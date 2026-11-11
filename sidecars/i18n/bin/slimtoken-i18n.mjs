#!/usr/bin/env node
// slimtoken-i18n — sidecar CLI for the SlimToken engine's i18n pack.
//
//   slimtoken-i18n check <dir> [--source <locale>] [--out DIR]
//
// `check` proves that locale files agree with each other: same key set,
// no empty values, and the same placeholders ({icu}, %s/%d, {{mustache}})
// in every locale. Zero tokens, zero runtime dependencies; Node 22+.
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
    console.log(`slimtoken-i18n — i18n parity gate for the SlimToken engine (zero deps)

usage: slimtoken-i18n <command> [args]

commands:
  check     Prove locale files agree: key-set parity, no empty values,
            placeholder parity. Locale = filename stem. Layouts: flat
            (en.json next to fr.json), nested (locales/en.json), or
            per-locale dirs (en/messages.json), e.g.
            slimtoken-i18n check app/i18n
            slimtoken-i18n check app/i18n --source en --out i18n-out

exit codes: 0 pass, 1 findings, 2 usage error`);
    process.exit(command === undefined ? 2 : 0);
    break;
  default:
    console.error(`slimtoken-i18n: unknown command '${command}' (try 'slimtoken-i18n help')`);
    process.exit(2);
}
