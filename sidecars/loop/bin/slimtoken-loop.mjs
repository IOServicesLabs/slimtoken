#!/usr/bin/env node
// slimtoken-loop — host-loop sidecar for the SlimToken engine.
//
//   slimtoken-loop init [--dir DIR]
//   slimtoken-loop run [--config loop.toml] [--out DIR]
//   slimtoken-loop help
//
// This pack has no gate of its own. It runs the engine, then runs the
// pack gates the config lists, then writes one verdict. The engine still
// owns the wave; the packs still own the gates; this loop only runs the
// sequence and reports the result.
// Exit codes: 0 all green, 1 engine or a gate failed, 2 usage error.
// Zero runtime dependencies; Node 22+.
import { fileURLToPath, pathToFileURL } from "node:url";
import { dirname, join } from "node:path";

const lib = join(dirname(fileURLToPath(import.meta.url)), "..", "lib");
const [command, ...argv] = process.argv.slice(2);

// Pull a "--name value" pair out of argv. ok:false means the flag was
// present but had no value (a usage error).
function takeOpt(args, name) {
  const i = args.indexOf(name);
  if (i === -1) return { ok: true, value: undefined };
  const value = args[i + 1];
  if (value === undefined || value.startsWith("--")) return { ok: false };
  args.splice(i, 2);
  return { ok: true, value };
}

switch (command) {
  case "init": {
    const dir = takeOpt(argv, "--dir");
    if (!dir.ok || argv.length > 0) {
      console.error("usage: slimtoken-loop init [--dir DIR]");
      process.exit(2);
    }
    const { initLoop } = await import(pathToFileURL(join(lib, "init.mjs")).href);
    process.exit(initLoop(dir.value ?? "."));
    break;
  }
  case "run": {
    const config = takeOpt(argv, "--config");
    const out = takeOpt(argv, "--out");
    if (!config.ok || !out.ok || argv.length > 0) {
      console.error("usage: slimtoken-loop run [--config loop.toml] [--out DIR]");
      process.exit(2);
    }
    const { runLoop } = await import(pathToFileURL(join(lib, "run.mjs")).href);
    process.exit(runLoop(config.value ?? "loop.toml", out.value));
    break;
  }
  case "help":
  case "--help":
  case "-h":
    console.log(`slimtoken-loop — the reference host loop as a runnable (zero deps)

usage: slimtoken-loop <command> [args]

commands:
  init [--dir DIR]              write a commented loop.toml template
  run [--config loop.toml] [--out DIR]
                                engine run, then each pack gate, then a verdict
                                (console table; --out also writes report.md + loop.json)
  help                          this text

exit codes: 0 all green, 1 engine or a gate failed, 2 usage error`);
    process.exit(0);
    break;
  case undefined:
    console.error("slimtoken-loop: no command (try 'slimtoken-loop help')");
    process.exit(2);
    break;
  default:
    console.error(`slimtoken-loop: unknown command '${command}' (try 'slimtoken-loop help')`);
    process.exit(2);
}
