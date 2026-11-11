// Judge caller for slimtoken-judge.
//
// `judge` sends ONE verdict brief to ONE model call:
//   POST $JUDGE_BASE_URL/chat/completions  (OpenAI-compatible)
//   {model: $JUDGE_MODEL, messages: [system, user], temperature: 0}
// The system prompt orders the model to answer with one JSON object only:
//   {"verdict": "pass"|"fail", "reasons": [...], "steer": [{"objective": "...", "priority": 1}]}
// The reply is parsed robustly (markdown fences stripped). The parsed
// object lands in steer.json — the plan fragment the host consumes.
//
// Cost (see README.md, COST section): exactly one model call per
// invocation. Input ~= the merged brief (a few KB). Output ~= the steer
// JSON (small). No open-ended loops, no retries, no fan-out.
//
// Keys come only from the environment: JUDGE_BASE_URL, JUDGE_API_KEY,
// JUDGE_MODEL. Missing any = exit 2, with the missing name in the message.
//
// Contract:
//   - exit 0: verdict parsed (any verdict value)
//   - exit 1: transport error, HTTP >= 400, or unparseable body
//   - exit 2: usage error, missing brief, missing env var
import { readFileSync, writeFileSync, mkdirSync, existsSync } from "node:fs";
import { join, resolve } from "node:path";

const ENV_KEYS = ["JUDGE_BASE_URL", "JUDGE_API_KEY", "JUDGE_MODEL"];
const DEFAULT_MAX_BYTES = 20000;
const JUDGE_TIMEOUT_MS = 60_000;

const SYSTEM_PROMPT = `You are the judge for a SlimToken engine run. Several gate packs checked the run artifacts and each wrote a verdict section into the brief below. Read every section. Decide if the run passes as a whole. Then steer the next wave.

Answer with ONE JSON object only. No prose, no markdown, no code fences. The object must have exactly this shape:
{"verdict": "pass" or "fail", "reasons": ["one short reason per failed or notable pack"], "steer": [{"objective": "a concrete next-wave objective", "priority": 1}]}

Rules:
- verdict is "fail" when any pack section reports ok: false, else "pass".
- reasons lists only what matters; keep each reason under 20 words.
- steer is a plan fragment for the next wave: one objective per thing to fix or do next, priority 1 is most urgent. Empty array when the run passes.
- Respond with the JSON object and nothing else.`;

export async function runJudge(args) {
  const parsed = parseArgs(args);
  if (parsed.error) {
    console.error(`judge: ${parsed.error}`);
    return 2;
  }
  const { briefFile, outDir, maxBytes } = parsed;

  const missing = ENV_KEYS.filter((k) => !process.env[k] || process.env[k].trim() === "");
  if (missing.length > 0) {
    console.error(`judge: missing environment variable(s): ${missing.join(", ")} (set them to point at the judge model; see README.md)`);
    return 2;
  }

  if (!existsSync(briefFile)) {
    console.error(`judge: brief '${briefFile}' does not exist`);
    return 2;
  }
  let brief;
  try {
    brief = readFileSync(briefFile, "utf8");
  } catch (error) {
    console.error(`judge: cannot read brief '${briefFile}': ${error.message}`);
    return 2;
  }

  let truncated = false;
  if (Buffer.byteLength(brief, "utf8") > maxBytes) {
    // Budget cap: hard-slice the brief, keep a note at the head. The call
    // still happens — the verdict is never silently dropped.
    console.log(`WARN judge: brief exceeds --max-input-bytes ${maxBytes}; truncated to fit`);
    truncated = true;
    const note = "> WARN: brief truncated to fit --max-input-bytes; tail kept\n\n";
    const room = Math.max(0, maxBytes - Buffer.byteLength(note, "utf8"));
    brief = note + Buffer.from(brief, "utf8").subarray(0, room).toString("utf8");
  }

  const baseUrl = process.env.JUDGE_BASE_URL.replace(/\/+$/, "");
  const payload = {
    model: process.env.JUDGE_MODEL,
    messages: [
      { role: "system", content: SYSTEM_PROMPT },
      { role: "user", content: brief },
    ],
    temperature: 0,
  };

  let bodyText;
  try {
    const res = await fetch(`${baseUrl}/chat/completions`, {
      method: "POST",
      headers: {
        "content-type": "application/json",
        authorization: `Bearer ${process.env.JUDGE_API_KEY}`,
        // Close the socket right after the reply so the child process can
        // exit naturally on Windows (no undici keep-alive handle left).
        connection: "close",
      },
      body: JSON.stringify(payload),
      signal: AbortSignal.timeout(JUDGE_TIMEOUT_MS),
    });
    if (!res.ok) {
      const errText = await res.text().catch(() => "");
      console.error(`judge: HTTP ${res.status} from judge endpoint${errText ? `: ${errText.slice(0, 200)}` : ""}`);
      return 1;
    }
    bodyText = await res.text();
  } catch (error) {
    console.error(`judge: transport error: ${error.message}`);
    return 1;
  }

  let envelope;
  try {
    envelope = JSON.parse(bodyText);
  } catch {
    console.error(`judge returned unparseable JSON: ${bodyText.trim().slice(0, 200)}`);
    return 1;
  }

  const content = envelope?.choices?.[0]?.message?.content;
  if (typeof content !== "string" || content.trim() === "") {
    console.error("judge returned unparseable JSON: (empty or missing choices[0].message.content)");
    return 1;
  }

  let verdict;
  try {
    verdict = JSON.parse(stripFences(content));
  } catch {
    console.error(`judge returned unparseable JSON: ${content.trim().slice(0, 200)}`);
    return 1;
  }
  if (verdict === null || typeof verdict !== "object" || Array.isArray(verdict)) {
    console.error(`judge returned unparseable JSON: ${content.trim().slice(0, 200)}`);
    return 1;
  }

  const record = {
    verdict: verdict.verdict ?? null,
    reasons: Array.isArray(verdict.reasons) ? verdict.reasons : [],
    steer: Array.isArray(verdict.steer) ? verdict.steer : [],
    model: process.env.JUDGE_MODEL,
    usage: envelope?.usage ?? null,
  };

  const abs = resolve(outDir || ".");
  mkdirSync(abs, { recursive: true });
  writeFileSync(join(abs, "steer.json"), JSON.stringify(record, null, 2) + "\n", "utf8");
  if (outDir) {
    // Judge output per the sidecar contract: one-glance markdown plus the
    // machine-readable twin (judge.json mirrors steer.json for hosts that
    // name every pack twin after its pack).
    writeFileSync(join(abs, "judge.json"), JSON.stringify(record, null, 2) + "\n", "utf8");
    writeFileSync(join(abs, "report.md"), renderReport(record, briefFile, truncated), "utf8");
  }
  console.log(`judge: verdict ${JSON.stringify(record.verdict)}; steer ${record.steer.length} objective(s) -> ${join(abs, "steer.json")}`);
  return 0;
}

function parseArgs(args) {
  let briefFile = null;
  let outDir = null;
  let maxBytes = DEFAULT_MAX_BYTES;
  for (let i = 0; i < args.length; i += 1) {
    const a = args[i];
    if (a === "--brief") {
      if (i + 1 >= args.length) return { error: "--brief needs a brief.md file" };
      briefFile = args[i + 1];
      i += 1;
    } else if (a.startsWith("--brief=")) {
      briefFile = a.slice("--brief=".length);
    } else if (a === "--out") {
      if (i + 1 >= args.length) return { error: "--out needs a directory" };
      outDir = args[i + 1];
      i += 1;
    } else if (a.startsWith("--out=")) {
      outDir = a.slice("--out=".length);
    } else if (a === "--max-input-bytes") {
      if (i + 1 >= args.length) return { error: "--max-input-bytes needs a number" };
      const n = Number(args[i + 1]);
      if (!Number.isInteger(n) || n <= 0) return { error: `--max-input-bytes must be a positive integer, got '${args[i + 1]}'` };
      maxBytes = n;
      i += 1;
    } else if (a.startsWith("--max-input-bytes=")) {
      const n = Number(a.slice("--max-input-bytes=".length));
      if (!Number.isInteger(n) || n <= 0) return { error: `--max-input-bytes must be a positive integer, got '${a}'` };
      maxBytes = n;
    } else {
      return { error: `unknown argument '${a}' (usage: slimtoken-judge judge --brief brief.md [--out DIR])` };
    }
  }
  if (!briefFile) return { error: "no brief given (usage: slimtoken-judge judge --brief brief.md [--out DIR])" };
  return { briefFile, outDir, maxBytes };
}

// The model sometimes wraps the JSON in ```json fences despite the system
// prompt. Strip one outer fence pair; leave the payload itself untouched.
function stripFences(text) {
  let t = text.trim();
  const fence = t.match(/^```[a-zA-Z]*\s*\n?([\s\S]*?)\n?\s*```$/);
  if (fence) t = fence[1].trim();
  return t;
}

function renderReport(record, briefFile, truncated) {
  const lines = [
    "# slimtoken-judge report",
    "",
    `- Brief: ${briefFile}`,
    `- Model: ${record.model}`,
    `- Truncated: ${truncated ? "yes" : "no"}`,
    "",
    `## Verdict: ${record.verdict}`,
    "",
    "## Reasons",
    "",
  ];
  if (record.reasons.length === 0) lines.push("(none)");
  for (const reason of record.reasons) lines.push(`- ${String(reason)}`);
  lines.push("", "## Steer", "");
  if (record.steer.length === 0) lines.push("(none)");
  for (const s of record.steer) {
    lines.push(`- [${s.priority ?? "?"}] ${String(s.objective ?? JSON.stringify(s))}`);
  }
  if (record.usage) {
    lines.push("", "## Usage", "", `- prompt tokens: ${record.usage.prompt_tokens ?? "?"}`,
      `- completion tokens: ${record.usage.completion_tokens ?? "?"}`,
      `- total tokens: ${record.usage.total_tokens ?? "?"}`);
  }
  return lines.join("\n") + "\n";
}
