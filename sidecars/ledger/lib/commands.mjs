// commands.mjs — command implementations for the ledger dispatcher.
//
// Exit codes (see sidecars/README.md and this pack's README):
//   0  command succeeded (add appended; show printed; compare found B
//      matched or improved A on every gated metric)
//   1  compare found a regression on a gated metric (wall or gates)
//   2  usage error: bad args, missing --arm, unreadable run dir,
//      missing ledger, fewer than two arms, unknown arm
import { extractRow } from "./extract.mjs";
import { loadLedger, appendRow, latestByArm } from "./store.mjs";
import { compareArms } from "./compare.mjs";
import { renderCompare, renderShow, writeCompareOut } from "./render.mjs";

const DEFAULT_LEDGER = "ledger.jsonl";

function usageError(message) {
  console.error(`ledger: ${message}`);
  return 2;
}

function parseFlags(args) {
  const flags = {};
  const positional = [];
  for (let i = 0; i < args.length; i += 1) {
    const arg = args[i];
    if (arg.startsWith("--")) {
      const key = arg.slice(2);
      if (i + 1 >= args.length || args[i + 1].startsWith("--")) {
        return { error: `flag ${arg} needs a value` };
      }
      if (key === "arm") {
        (flags.arms ??= []).push(args[i + 1]);
        flags.arm = args[i + 1];
      } else {
        flags[key] = args[i + 1];
      }
      i += 1;
    } else {
      positional.push(arg);
    }
  }
  return { flags, positional };
}

function parseTags(text) {
  const tags = {};
  for (const pair of text.split(",")) {
    const trimmed = pair.trim();
    if (trimmed === "") continue;
    const eq = trimmed.indexOf("=");
    if (eq <= 0) return { error: `bad --tags entry "${trimmed}" (want k=v)` };
    tags[trimmed.slice(0, eq)] = trimmed.slice(eq + 1);
  }
  return { tags };
}

// add <run-dir> --arm <name> [--ledger FILE] [--tags k=v,...]
export function cmdAdd(args) {
  const parsed = parseFlags(args);
  if (parsed.error) return usageError(parsed.error);
  const { flags, positional } = parsed;
  if (positional.length !== 1) {
    return usageError("usage: slimtoken-ledger add <run-dir> --arm <name> [--ledger FILE] [--tags k=v,...]");
  }
  const arm = flags.arm;
  if (typeof arm !== "string" || arm === "") {
    return usageError("add needs --arm <name>");
  }
  let tags = {};
  if (flags.tags !== undefined) {
    const parsedTags = parseTags(flags.tags);
    if (parsedTags.error) return usageError(parsedTags.error);
    tags = parsedTags.tags;
  }
  const ledger = flags.ledger ?? DEFAULT_LEDGER;

  let row;
  try {
    row = extractRow(positional[0], arm, tags);
  } catch (error) {
    return usageError(error.message);
  }
  appendRow(ledger, row);
  const gateSummary = Object.entries(row.gates)
    .map(([name, ok]) => `${name}=${ok === true ? "ok" : ok === false ? "FAIL" : "?"}`)
    .join(", ") || "none";
  console.log(`ledger: appended row arm=${row.arm} wall_ms=${row.wall_ms} gates: ${gateSummary} -> ${ledger}`);
  return 0;
}

function loadLedgerOrExit(ledger) {
  try {
    return { rows: loadLedger(ledger) };
  } catch (error) {
    return { error: error.message };
  }
}

// compare --ledger FILE --arm A --arm B [--out DIR]
export function cmdCompare(args) {
  const parsed = parseFlags(args);
  if (parsed.error) return usageError(parsed.error);
  const { flags } = parsed;
  const arms = flags.arms ?? [];
  if (arms.length !== 2) {
    return usageError("compare needs exactly two arms (--arm A --arm B)");
  }
  const ledger = flags.ledger ?? DEFAULT_LEDGER;
  const loaded = loadLedgerOrExit(ledger);
  if (loaded.error) return usageError(loaded.error);

  const byArm = latestByArm(loaded.rows);
  const rowA = byArm.get(arms[0]);
  const rowB = byArm.get(arms[1]);
  if (!rowA) return usageError(`arm not in ledger: ${arms[0]}`);
  if (!rowB) return usageError(`arm not in ledger: ${arms[1]}`);

  const verdict = compareArms(rowA, rowB);
  process.stdout.write(renderCompare(rowA, rowB, arms[0], arms[1], verdict));
  if (flags.out !== undefined) {
    try {
      writeCompareOut(flags.out, rowA, rowB, arms[0], arms[1]);
    } catch (error) {
      return usageError(`cannot write --out dir: ${error.message}`);
    }
    console.log(`ledger: wrote ${flags.out}/report.md and ledger.json`);
  }
  return verdict.ok ? 0 : 1;
}

// show --ledger FILE [--arm NAME]
export function cmdShow(args) {
  const parsed = parseFlags(args);
  if (parsed.error) return usageError(parsed.error);
  const { flags, positional } = parsed;
  if (positional.length > 0) return usageError("usage: slimtoken-ledger show --ledger FILE [--arm NAME]");
  const ledger = flags.ledger ?? DEFAULT_LEDGER;
  const loaded = loadLedgerOrExit(ledger);
  if (loaded.error) return usageError(loaded.error);
  const rows = flags.arm !== undefined
    ? loaded.rows.filter((row) => row.arm === flags.arm)
    : loaded.rows;
  process.stdout.write(renderShow(rows));
  return 0;
}

export const HELP = `slimtoken-ledger — one JSONL row per run, paired-arm A/B deltas.

usage:
  slimtoken-ledger add <run-dir> --arm <name> [--ledger FILE] [--tags k=v,...]
  slimtoken-ledger compare --ledger FILE --arm A --arm B [--out DIR]
  slimtoken-ledger show --ledger FILE [--arm NAME]
  slimtoken-ledger help

exit codes: 0 pass / matched-or-improved, 1 compare regression, 2 usage.
`;
