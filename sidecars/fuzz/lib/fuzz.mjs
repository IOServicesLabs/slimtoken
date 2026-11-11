// In-process fuzz campaign runner for slimtoken-fuzz.
//
// Contract (see sidecars/README.md):
//   - exit 0: no findings
//   - exit 1: at least one finding (the target threw)
//   - exit 2: usage error (bad args, unreadable seeds dir, bad target module)
//   - zero tokens: pure in-process execution, never a model call
import { readFileSync, readdirSync, statSync, existsSync } from "node:fs";
import { join, resolve } from "node:path";
import { pathToFileURL } from "node:url";
import { writeReport, displayInput } from "./report.mjs";

// Boundary alphabet: tokens that sit at parser edges. "💥" is required —
// the selftest's crashy fixture plants its crash on it.
export const BOUNDARY_ALPHABET = ["💥", "\x00", "'", "\"", "{{", "}}", "\\", "�"];

const LONG_RUN_CHARS = "aA0{}\n";

// mulberry32: small deterministic PRNG. Same seed, same campaign.
export function mulberry32(seed) {
  let a = seed >>> 0;
  return function () {
    a |= 0;
    a = (a + 0x6d2b79f5) | 0;
    let t = Math.imul(a ^ (a >>> 15), 1 | a);
    t = (t + Math.imul(t ^ (t >>> 7), 61 | t)) ^ t;
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
}

function longRun(rand) {
  const ch = LONG_RUN_CHARS[Math.floor(rand() * LONG_RUN_CHARS.length)];
  const len = 64 + Math.floor(rand() * 960);
  return ch.repeat(len);
}

// --- mutations ------------------------------------------------------------

function mutByteFlip(input, rand) {
  if (input.length === 0) return String.fromCharCode(33 + Math.floor(rand() * 94));
  const i = Math.floor(rand() * input.length);
  const c = String.fromCharCode(1 + Math.floor(rand() * 0x7f));
  return input.slice(0, i) + c + input.slice(i + 1);
}

function mutTruncate(input, rand) {
  if (input.length === 0) return input;
  return input.slice(0, Math.floor(rand() * input.length));
}

function mutInsert(input, rand) {
  const i = Math.floor(rand() * (input.length + 1));
  const token =
    rand() < 0.3
      ? longRun(rand)
      : BOUNDARY_ALPHABET[Math.floor(rand() * BOUNDARY_ALPHABET.length)];
  return input.slice(0, i) + token + input.slice(i);
}

function mutSplice(input, rand, seeds) {
  if (seeds.length < 2) return mutInsert(input, rand);
  const other = seeds[Math.floor(rand() * seeds.length)];
  const a = Math.floor(rand() * (input.length + 1));
  const b = Math.floor(rand() * (other.length + 1));
  return input.slice(0, a) + other.slice(b);
}

const MUTATIONS = [mutByteFlip, mutTruncate, mutInsert, mutSplice];

// Pick a base seed, apply 1-4 random mutations.
export function mutate(base, rand, seeds) {
  const n = 1 + Math.floor(rand() * 4);
  let out = base;
  for (let k = 0; k < n; k++) {
    const m = MUTATIONS[Math.floor(rand() * MUTATIONS.length)];
    out = m(out, rand, seeds);
  }
  return out;
}

// --- inputs ---------------------------------------------------------------

export function loadSeeds(dir) {
  if (!dir) return { ok: false, error: "missing --seeds <dir>" };
  const abs = resolve(dir);
  if (!existsSync(abs)) return { ok: false, error: `seeds dir not found: ${dir}` };
  let names;
  try {
    names = readdirSync(abs);
  } catch {
    return { ok: false, error: `seeds dir not readable: ${dir}` };
  }
  const files = names.filter((n) => {
    try {
      return statSync(join(abs, n)).isFile();
    } catch {
      return false;
    }
  });
  if (files.length === 0) return { ok: false, error: `no seed files in ${dir}` };
  const seeds = files.map((n) => readFileSync(join(abs, n), "utf8"));
  return { ok: true, seeds, files };
}

export async function loadTarget(file) {
  if (!file) return { ok: false, error: "missing target module (usage: slimtoken-fuzz run <target.mjs> --seeds <dir>)" };
  const abs = resolve(file);
  if (!existsSync(abs)) return { ok: false, error: `target not found: ${file}` };
  let mod;
  try {
    // pathToFileURL is required on Windows: a bare absolute path is not a
    // valid URL for dynamic import.
    mod = await import(pathToFileURL(abs).href);
  } catch (err) {
    return { ok: false, error: `cannot import target ${file}: ${err.message}` };
  }
  if (typeof mod.fuzz !== "function") {
    return { ok: false, error: `target ${file} does not export function fuzz(input)` };
  }
  return { ok: true, target: mod };
}

// --- campaign -------------------------------------------------------------

export async function runCampaign({ target, seeds, cases, seconds, seed, keepGoing }) {
  const rand = mulberry32(seed);
  const started = Date.now();
  const deadline = started + seconds * 1000;
  const findings = [];
  let ran = 0;
  while (ran < cases && Date.now() < deadline) {
    const base = seeds[Math.floor(rand() * seeds.length)];
    const input = mutate(base, rand, seeds);
    ran += 1;
    try {
      await target.fuzz(input);
    } catch (err) {
      findings.push({
        case: ran,
        input,
        error: String((err && err.message) || err),
      });
      if (!keepGoing) break;
    }
  }
  return { ran, findings, wallMs: Date.now() - started };
}

// --- CLI ------------------------------------------------------------------

const USAGE = `slimtoken-fuzz — in-process fuzz campaign sidecar for the SlimToken engine (zero deps)

usage: slimtoken-fuzz run <target.mjs> --seeds <dir> [options]

  <target.mjs>        ESM module exporting function fuzz(input: string) (or async)
  --seeds <dir>       directory of seed files, read as UTF-8 text (required)
  --cases N           max cases to run (default 500)
  --seconds S         time budget in seconds (default 10); cases or seconds, whichever first
  --seed R            PRNG seed, default 1 (deterministic: same seed, same campaign)
  --keep-going        run the full campaign instead of stopping at the first finding
  --out DIR           write report.md, fuzz.json, and finding-N.txt files

exit codes: 0 no findings, 1 finding(s), 2 usage error or bad target module`;

function parseRunArgs(rest) {
  const opts = { cases: 500, seconds: 10, seed: 1, keepGoing: false, outDir: null, target: null };
  for (let i = 0; i < rest.length; i++) {
    const a = rest[i];
    if (a === "--keep-going") {
      opts.keepGoing = true;
    } else if (a === "--cases" || a === "--seconds" || a === "--seed" || a === "--out" || a === "--seeds") {
      const v = rest[++i];
      if (v === undefined) return { ok: false, error: `flag ${a} needs a value` };
      if (a === "--cases") opts.cases = v;
      else if (a === "--seconds") opts.seconds = v;
      else if (a === "--seed") opts.seed = v;
      else if (a === "--out") opts.outDir = v;
      else opts.seedsDir = v;
    } else if (a.startsWith("--")) {
      return { ok: false, error: `unknown flag ${a}` };
    } else if (opts.target === null) {
      opts.target = a;
    } else {
      return { ok: false, error: `unexpected argument ${a}` };
    }
  }
  const cases = Number(opts.cases);
  if (!Number.isInteger(cases) || cases <= 0) return { ok: false, error: `--cases must be a positive integer (got ${opts.cases})` };
  opts.cases = cases;
  const seconds = Number(opts.seconds);
  if (!(seconds > 0)) return { ok: false, error: `--seconds must be a positive number (got ${opts.seconds})` };
  opts.seconds = seconds;
  const seed = Number(opts.seed);
  if (!Number.isInteger(seed)) return { ok: false, error: `--seed must be an integer (got ${opts.seed})` };
  opts.seed = seed;
  return { ok: true, opts };
}

export async function main(argv) {
  const [command, ...rest] = argv;
  if (command === undefined) {
    console.error(USAGE);
    return 2;
  }
  if (command === "help" || command === "--help" || command === "-h") {
    console.log(USAGE);
    return 0;
  }
  if (command !== "run") {
    console.error(`slimtoken-fuzz: unknown command '${command}' (try 'slimtoken-fuzz help')`);
    return 2;
  }

  const parsed = parseRunArgs(rest);
  if (!parsed.ok) {
    console.error(`slimtoken-fuzz: ${parsed.error}`);
    return 2;
  }
  const opts = parsed.opts;

  const targetRes = await loadTarget(opts.target);
  if (!targetRes.ok) {
    console.error(`slimtoken-fuzz: ${targetRes.error}`);
    return 2;
  }
  const seedsRes = loadSeeds(opts.seedsDir);
  if (!seedsRes.ok) {
    console.error(`slimtoken-fuzz: ${seedsRes.error}`);
    return 2;
  }

  const { ran, findings, wallMs } = await runCampaign({
    target: targetRes.target,
    seeds: seedsRes.seeds,
    cases: opts.cases,
    seconds: opts.seconds,
    seed: opts.seed,
    keepGoing: opts.keepGoing,
  });

  console.log(`cases: ${ran}, findings: ${findings.length}`);
  findings.forEach((f, i) => {
    const where = opts.outDir ? `input written to finding-${i}.txt` : `input: ${displayInput(f.input)}`;
    console.log(`finding ${i} (case ${f.case}): ${f.error} — ${where}`);
  });

  if (opts.outDir) {
    const written = writeReport(opts.outDir, {
      target: resolve(opts.target),
      seed: opts.seed,
      cases: ran,
      seconds: Math.round(wallMs) / 1000,
      keepGoing: opts.keepGoing,
      findings,
    });
    console.log(`report: ${written}`);
  }

  return findings.length > 0 ? 1 : 0;
}
