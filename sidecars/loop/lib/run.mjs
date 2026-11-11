// slimtoken-loop run: the reference host loop as a runnable.
//
//   1. Spawn the engine: <engine> run --plan <plan> --output <output>
//      (or --objective when the config sets objective). stdio inherit.
//      A non-zero engine exit ends the loop: gates never run on a broken
//      run, and the loop exits 1.
//   2. Run each [[gates]] entry in order through a shell. {{output}} in a
//      gate cmd expands to the run output dir. Record {name, cmd, exit}.
//   3. Verdict: a console table plus, with --out DIR, report.md and
//      loop.json ({ok, engine: {exit}, gates: [...], summary}).
//
// Exit codes: 0 engine ok and every gate 0; 1 engine or a gate failed;
// 2 usage error, missing config, or engine binary not found.
// Zero tokens: this code never calls a model; it only spawns processes.
import { spawnSync } from "node:child_process";
import { existsSync, mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { delimiter, dirname, isAbsolute, join, resolve } from "node:path";
import { parseLoopToml } from "./config.mjs";

export function runLoop(configPath, outDir) {
  const configFile = resolve(configPath);
  if (!existsSync(configFile)) {
    console.error(`slimtoken-loop: config not found: ${configFile}`);
    return 2;
  }
  const baseDir = dirname(configFile);

  let cfg;
  try {
    cfg = parseLoopToml(readFileSync(configFile, "utf8"), configFile);
  } catch (error) {
    console.error(`slimtoken-loop: ${error.message}`);
    return 2;
  }

  if (cfg.engine === undefined || String(cfg.engine).trim() === "") {
    console.error("slimtoken-loop: config has no engine = \"...\" key");
    return 2;
  }
  const hasObjective = typeof cfg.objective === "string" && cfg.objective.trim() !== "";
  if (!hasObjective && (typeof cfg.plan !== "string" || cfg.plan.trim() === "")) {
    console.error("slimtoken-loop: config needs plan = \"...\" or objective = \"...\"");
    return 2;
  }

  const engineToken = String(cfg.engine).trim().split(/\s+/)[0];
  if (findProgram(engineToken, baseDir) === null) {
    console.error(`slimtoken-loop: engine '${engineToken}' not found (PATH or path)`);
    return 2;
  }

  const outputDir = resolve(baseDir, String(cfg.output ?? "run-output"));
  const runArgs = hasObjective
    ? `run --objective ${q(cfg.objective)} --output ${q(outputDir)}`
    : `run --plan ${q(resolve(baseDir, cfg.plan))} --output ${q(outputDir)}`;
  const engineCmd = `${cfg.engine} ${runArgs}`;

  console.log(`slimtoken-loop: engine: ${engineCmd}`);
  const engineRes = spawnSync(engineCmd, { shell: true, stdio: "inherit", cwd: baseDir });
  const engineExit = engineRes.status === null ? 1 : engineRes.status;

  let gates = [];
  if (engineExit !== 0) {
    console.error(`slimtoken-loop: engine exited ${engineExit} — gates skipped`);
  } else {
    for (const gate of cfg.gates) {
      const cmd = String(gate.cmd ?? "").split("{{output}}").join(outputDir);
      console.log(`slimtoken-loop: gate '${gate.name}': ${cmd}`);
      const res = spawnSync(cmd, { shell: true, stdio: "inherit", cwd: baseDir });
      const exit = res.status === null ? 1 : res.status;
      gates.push({ name: String(gate.name ?? ""), cmd, exit });
    }
    if (gates.length > 0) {
      console.table(gates.map((g) => ({ gate: g.name, exit: g.exit })));
    }
  }

  const failed = gates.filter((g) => g.exit !== 0).length;
  const ok = engineExit === 0 && failed === 0;
  const verdict = {
    ok,
    engine: { exit: engineExit },
    gates,
    summary: {
      gates: gates.length,
      passed: gates.length - failed,
      failed,
      verdict: ok ? "pass" : "fail",
    },
  };

  console.log(`slimtoken-loop: engine exit ${engineExit}, ${gates.length - failed}/${gates.length} gates passed`);
  console.log(`slimtoken-loop: verdict ${ok ? "PASS" : "FAIL"}`);

  if (outDir !== undefined && outDir !== null) {
    const out = resolve(outDir);
    mkdirSync(out, { recursive: true });
    writeFileSync(join(out, "loop.json"), JSON.stringify(verdict, null, 2) + "\n");
    writeFileSync(join(out, "report.md"), renderReport(configFile, cfg, verdict));
    console.log(`slimtoken-loop: wrote ${join(out, "report.md")} and loop.json`);
  }

  return ok ? 0 : 1;
}

function renderReport(configFile, cfg, verdict) {
  const lines = [
    "# slimtoken-loop verdict",
    "",
    `- config: ${configFile}`,
    `- engine: ${cfg.engine} (exit ${verdict.engine.exit})`,
    `- gates: ${verdict.summary.passed}/${verdict.summary.gates} passed`,
    "",
    "| gate | exit |",
    "|---|---|",
  ];
  for (const gate of verdict.gates) {
    lines.push(`| ${gate.name} | ${gate.exit} |`);
  }
  if (verdict.gates.length === 0) {
    lines.push("| (none — gates skipped) | |");
  }
  lines.push("", `verdict: ${verdict.ok ? "PASS" : "FAIL"}`, "");
  return lines.join("\n");
}

// Quote a path for cmd.exe / sh. Strips any double quotes inside; the
// loop's own paths never carry them.
function q(value) {
  return `"${String(value).replace(/"/g, "")}"`;
}

// Locate the engine program. A token with a path separator must exist as
// a file (relative paths resolve against the config dir). A bare token is
// searched on PATH, honoring PATHEXT on Windows.
function findProgram(token, baseDir) {
  if (/[\\/]/.test(token) || /^[A-Za-z]:/.test(token)) {
    const candidate = isAbsolute(token) ? token : join(baseDir, token);
    return existsSync(candidate) ? candidate : null;
  }
  const extensions =
    process.platform === "win32"
      ? ["", ...(process.env.PATHEXT ?? ".EXE;.CMD;.BAT;.COM").split(";")]
      : [""];
  for (const dir of (process.env.PATH ?? "").split(delimiter)) {
    if (dir === "") continue;
    for (const ext of extensions) {
      for (const candidate of [join(dir, token + ext), join(dir, token + ext.toLowerCase())]) {
        if (existsSync(candidate)) return candidate;
      }
    }
  }
  return null;
}
