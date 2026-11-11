// slimtoken-triage: the `candidates` command.
//
// Read a gate failure report, ask the model for K candidate diagnoses in
// ONE chat/completions call, and write candidates.json. This command is
// the only token-spending step in the pack; `repro` spends zero tokens.
//
// Exit codes: 0 parsed and written, 1 transport / HTTP / parse error,
// 2 usage error (bad args, missing env, unreadable file).
import { readFileSync, writeFileSync, mkdirSync } from "node:fs";
import { resolve, join } from "node:path";
import { parseArgs, parsePositiveInt, missingEnv } from "./util.mjs";

const ENV_NAMES = ["TRIAGE_BASE_URL", "TRIAGE_API_KEY", "TRIAGE_MODEL"];

export async function runCandidates(args) {
  const { positional, flags } = parseArgs(args);
  const failureFile = positional[0];
  if (!failureFile) {
    console.error("candidates: missing <failure-file> (usage: slimtoken-triage candidates <failure-file> [--k N] [--out DIR] [--max-bytes N])");
    return 2;
  }
  const kParsed = parsePositiveInt(flags.k ?? "3", "k");
  if (kParsed.error) {
    console.error(`candidates: ${kParsed.error}`);
    return 2;
  }
  const k = kParsed.value;
  const maxBytesParsed = parsePositiveInt(flags["max-bytes"] ?? "10000", "max-bytes");
  if (maxBytesParsed.error) {
    console.error(`candidates: ${maxBytesParsed.error}`);
    return 2;
  }
  const maxBytes = maxBytesParsed.value;

  const missing = missingEnv(ENV_NAMES);
  if (missing.length > 0) {
    console.error(`candidates: missing environment variable(s): ${missing.join(", ")} (need ${ENV_NAMES.join(", ")}, keys only from the environment)`);
    return 2;
  }

  let context;
  try {
    context = readFileSync(failureFile, "utf8");
  } catch (error) {
    console.error(`candidates: cannot read failure file '${failureFile}': ${error.message}`);
    return 2;
  }
  const rawBytes = Buffer.byteLength(context, "utf8");
  if (rawBytes > maxBytes) {
    context = Buffer.from(context, "utf8").subarray(0, maxBytes).toString("utf8");
    console.error(`WARN: failure context truncated to ${maxBytes} bytes (was ${rawBytes})`);
  }

  const baseUrl = process.env.TRIAGE_BASE_URL.replace(/\/+$/, "");
  const apiKey = process.env.TRIAGE_API_KEY;
  const model = process.env.TRIAGE_MODEL;

  const systemPrompt = [
    "You are a failure triage engine.",
    "You receive a failure report from a gate.",
    "You respond with ONE JSON object only. No prose, no code fences.",
    'The object shape is: {"candidates":[{"name":"...","hypothesis":"...","repro":"<a single shell command that exits non-zero when the hypothesis holds>"}]}',
    "The `repro` field is a single shell command. It must exit non-zero when the hypothesis holds, and zero when it does not.",
  ].join(" ");
  const userPrompt = [
    `Here is the failure report (${failureFile}):`,
    "",
    context,
    "",
    `Respond with ONE JSON object only. It must contain exactly ${k} candidates.`,
  ].join("\n");

  const body = {
    model,
    temperature: 0,
    messages: [
      { role: "system", content: systemPrompt },
      { role: "user", content: userPrompt },
    ],
  };

  let data;
  try {
    data = await postWithRetry(`${baseUrl}/chat/completions`, apiKey, body);
  } catch (error) {
    console.error(`candidates: ${error.message}`);
    return 1;
  }

  const content = data?.choices?.[0]?.message?.content;
  if (typeof content !== "string" || content.length === 0) {
    console.error("candidates: response has no choices[0].message.content");
    return 1;
  }

  const parsed = stripFencesAndParse(content);
  if (!parsed || typeof parsed !== "object" || !Array.isArray(parsed.candidates)) {
    console.error(`candidates: model returned invalid JSON (first 200 chars): ${content.slice(0, 200)}`);
    return 1;
  }
  if (parsed.candidates.length !== k) {
    console.error(`candidates: model returned ${parsed.candidates.length} candidates, expected exactly ${k}`);
    return 1;
  }
  const bad = parsed.candidates.find(
    (c) => !c || typeof c.name !== "string" || typeof c.hypothesis !== "string" || typeof c.repro !== "string" || c.repro.length === 0,
  );
  if (bad) {
    console.error("candidates: every candidate needs string fields name, hypothesis, repro (repro must not be empty)");
    return 1;
  }

  const outDir = flags.out ? resolve(flags.out) : process.cwd();
  mkdirSync(outDir, { recursive: true });
  const outFile = join(outDir, "candidates.json");
  writeFileSync(
    outFile,
    `${JSON.stringify({ candidates: parsed.candidates, model, usage: data.usage ?? null }, null, 2)}\n`,
    "utf8",
  );
  console.log(`candidates: wrote ${parsed.candidates.length} candidate(s) to ${outFile}`);
  return 0;
}

// POST once. Throw with retryable=true on 429 / 5xx so the caller can
// retry; throw on any other HTTP status, transport error, or bad body.
async function postOnce(url, apiKey, body) {
  let res;
  try {
    res = await fetch(url, {
      method: "POST",
      headers: { "content-type": "application/json", authorization: `Bearer ${apiKey}` },
      body: JSON.stringify(body),
    });
  } catch (error) {
    throw new Error(`transport error: ${error.message}`);
  }
  if (res.status === 429 || res.status >= 500) {
    const error = new Error(`HTTP ${res.status} from ${url}`);
    error.retryable = true;
    throw error;
  }
  if (!res.ok) {
    throw new Error(`HTTP ${res.status} from ${url}: ${(await res.text()).slice(0, 200)}`);
  }
  try {
    return await res.json();
  } catch (error) {
    throw new Error(`response is not JSON: ${error.message}`);
  }
}

// One retry on 429 / 5xx with 2s backoff. Then give up (exit 1 upstream).
async function postWithRetry(url, apiKey, body) {
  try {
    return await postOnce(url, apiKey, body);
  } catch (error) {
    if (!error.retryable) throw error;
    console.error(`candidates: ${error.message}; retrying once in 2s`);
    await new Promise((resolveDelay) => setTimeout(resolveDelay, 2000));
    return await postOnce(url, apiKey, body);
  }
}

// Strip markdown code fences, then take the outermost {...} block and
// JSON.parse it. Return null when the text is not a JSON object.
export function stripFencesAndParse(text) {
  let t = text.trim();
  t = t.replace(/^```(?:json)?\s*/i, "").replace(/\s*```$/, "").trim();
  const start = t.indexOf("{");
  const end = t.lastIndexOf("}");
  if (start < 0 || end <= start) return null;
  try {
    return JSON.parse(t.slice(start, end + 1));
  } catch {
    return null;
  }
}
