// slimtoken-loop init: write a commented loop.toml template.
//
// Refuses to overwrite an existing loop.toml (exit 2, a usage conflict).
// Returns 0 on write.
import { existsSync, writeFileSync } from "node:fs";
import { join } from "node:path";

export const LOOP_TEMPLATE = `# SlimToken loop config
# plan: a plan.json the engine runs. objective: a plain text goal instead.
# engine: the engine command line. It may be a bare name on PATH
#         ("slimtoken") or a full command string ("node C:\\...\\engine.mjs").
# output: the run output dir. {{output}} in a gate cmd expands to it.
plan = "plan.json"          # or objective = "..."
engine = "slimtoken"        # engine CLI on PATH
output = "run-output"
[[gates]]
name = "syntax"
cmd = "slimtoken-qa validate {{output}}"     # {{output}} expands to the run output dir
[[gates]]
name = "smoke"
cmd = "slimtoken-qa smoke --url http://localhost:3000 --routes / --out {{output}}/qa"
`;

export function initLoop(dir) {
  const target = join(dir, "loop.toml");
  if (existsSync(target)) {
    console.error(`slimtoken-loop: ${target} already exists (refusing to overwrite)`);
    return 2;
  }
  writeFileSync(target, LOOP_TEMPLATE);
  console.log(`slimtoken-loop: wrote ${target}`);
  return 0;
}
