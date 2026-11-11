// slimtoken-triage: the `repro` command.
//
// Run each candidate's repro command with the shell. A candidate whose
// command exits NON-ZERO is CONFIRMED: the repro demonstrates the failure
// and the hypothesis is provable. Zero model tokens: repros are shell
// commands, 30s timeout each.
//
// Exit codes: 0 at least one candidate confirmed, 1 none confirmed,
// 2 usage error / malformed candidates file.
import { readFileSync, writeFileSync, mkdirSync } from "node:fs";
import { spawn } from "node:child_process";
import { resolve, join } from "node:path";
import { parseArgs, excerpt } from "./util.mjs";

const REPRO_TIMEOUT_MS = 30_000;
// A command the shell could not start does not demonstrate anything.
// Match the shell's "not found" message on cmd.exe and POSIX shells.
const NOT_FOUND_RE = /is not recognized as an internal|command not found|no such file or directory/i;

export async function runRepro(args) {
  const { positional, flags } = parseArgs(args);
  const file = positional[0];
  if (!file) {
    console.error("repro: missing <candidates.json> (usage: slimtoken-triage repro <candidates.json> [--workdir DIR] [--out DIR])");
    return 2;
  }
  let data;
  try {
    data = JSON.parse(readFileSync(file, "utf8"));
  } catch (error) {
    console.error(`repro: cannot read or parse candidates file '${file}': ${error.message}`);
    return 2;
  }
  if (!data || typeof data !== "object" || !Array.isArray(data.candidates) || data.candidates.length === 0) {
    console.error("repro: candidates file has no 'candidates' array");
    return 2;
  }
  const workdir = flags.workdir ? resolve(flags.workdir) : process.cwd();
  const outDir = flags.out ? resolve(flags.out) : null;

  const confirmed = [];
  const rejected = [];
  for (const candidate of data.candidates) {
    if (!candidate || typeof candidate.repro !== "string" || candidate.repro.length === 0) {
      rejected.push({ name: candidate?.name ?? "?", repro: "", reason: "candidate has no repro command" });
      continue;
    }
    const result = await runOne(candidate.repro, workdir);
    if (result.confirmed) {
      confirmed.push({
        name: candidate.name,
        hypothesis: candidate.hypothesis,
        repro: candidate.repro,
        exitCode: result.code,
        stderrExcerpt: excerpt(result.stderr, 10),
      });
      console.log(`CONFIRMED ${candidate.name} (exit ${result.code})`);
    } else {
      rejected.push({ name: candidate.name, repro: candidate.repro, reason: result.reason });
      console.log(`rejected  ${candidate.name} (${result.reason})`);
    }
  }

  const ok = confirmed.length > 0;
  const summary = `${confirmed.length}/${data.candidates.length} candidate(s) confirmed`;

  // confirmed.json is written on every run (empty when none confirmed).
  const confirmedDir = outDir ?? process.cwd();
  mkdirSync(confirmedDir, { recursive: true });
  writeFileSync(
    join(confirmedDir, "confirmed.json"),
    `${JSON.stringify({ ok, confirmed }, null, 2)}\n`,
    "utf8",
  );

  if (outDir) {
    writeFileSync(join(outDir, "triage.json"), `${JSON.stringify({ ok, confirmed, rejected, summary }, null, 2)}\n`, "utf8");
    writeFileSync(join(outDir, "report.md"), renderReport(ok, confirmed, rejected, summary), "utf8");
  }

  console.log(`repro: ${summary}`);
  return ok ? 0 : 1;
}

// Run one repro command. Resolves to { confirmed: true, code, stderr } or
// { confirmed: false, reason }. Spawn errors and timeouts are rejections
// with the error recorded, never a crash.
function runOne(command, cwd) {
  return new Promise((resolveP) => {
    let child;
    try {
      child = spawn(command, { shell: true, cwd, timeout: REPRO_TIMEOUT_MS });
    } catch (error) {
      resolveP({ confirmed: false, reason: `spawn error: ${error.message}` });
      return;
    }
    let stdout = "";
    let stderr = "";
    let settled = false;
    const settle = (value) => {
      if (!settled) {
        settled = true;
        resolveP(value);
      }
    };
    child.stdout?.on("data", (d) => { stdout += d; });
    child.stderr?.on("data", (d) => { stderr += d; });
    child.on("error", (error) => settle({ confirmed: false, reason: `spawn error: ${error.message}` }));
    child.on("close", (code, signal) => {
      if (code === null) {
        settle({ confirmed: false, reason: `timed out after ${REPRO_TIMEOUT_MS / 1000}s (killed, signal ${signal})` });
        return;
      }
      if (code !== 0 && NOT_FOUND_RE.test(stderr)) {
        settle({ confirmed: false, reason: `command not found: ${excerpt(stderr, 1).trim()}` });
        return;
      }
      if (code === 0) {
        settle({ confirmed: false, reason: "repro exited 0 (hypothesis not demonstrated)" });
        return;
      }
      settle({ confirmed: true, code, stderr });
    });
  });
}

function renderReport(ok, confirmed, rejected, summary) {
  const lines = [
    "# Triage report",
    "",
    `Verdict: ${ok ? "PASS" : "FAIL"} — ${summary}.`,
    "",
    "## Confirmed",
    "",
  ];
  if (confirmed.length === 0) {
    lines.push("_None._");
  } else {
    for (const c of confirmed) {
      lines.push(`- **${c.name}** — ${c.hypothesis}`);
      lines.push(`  - repro: \`${c.repro}\` (exit ${c.exitCode})`);
      if (c.stderrExcerpt) {
        lines.push("  - stderr (first 10 lines):");
        lines.push("    ```");
        for (const l of c.stderrExcerpt.split("\n")) lines.push(`    ${l}`);
        lines.push("    ```");
      }
    }
  }
  lines.push("", "## Rejected", "");
  if (rejected.length === 0) {
    lines.push("_None._");
  } else {
    for (const r of rejected) {
      lines.push(`- **${r.name}** — ${r.reason}`);
    }
  }
  lines.push("");
  return lines.join("\n");
}
