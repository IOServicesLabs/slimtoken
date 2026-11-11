#!/usr/bin/env node
// slimtoken-ledger — dispatcher.
//
// Commands: add, compare, show, help. Exit codes: 0 pass, 1 compare
// regression, 2 usage. No args exits 2. Zero runtime deps, zero model
// tokens: pure JSON / JSONL file IO.
import { cmdAdd, cmdCompare, cmdShow, HELP } from "../lib/commands.mjs";

const [, , command, ...args] = process.argv;

let code;
switch (command) {
  case "add":
    code = cmdAdd(args);
    break;
  case "compare":
    code = cmdCompare(args);
    break;
  case "show":
    code = cmdShow(args);
    break;
  case "help":
  case "--help":
  case "-h":
    process.stdout.write(HELP);
    code = 0;
    break;
  default:
    if (command === undefined) {
      console.error("ledger: no command (try: slimtoken-ledger help)");
    } else {
      console.error(`ledger: unknown command "${command}" (try: slimtoken-ledger help)`);
    }
    code = 2;
}

process.exit(code);
