// Readers for the replay pack. Pure file parsing, zero tokens.
//
// Tolerates the real engine shapes defensively: report tasks keyed by id
// (object) or as an array of records with an id; missing fields become
// null, never a crash. trace.jsonl is streamed line by line.

import { existsSync, readFileSync } from "node:fs";
import { join } from "node:path";
import { createReadStream } from "node:fs";
import { createInterface } from "node:readline";

// Read a JSON file; returns { ok, data } or { ok: false, error }.
export function readJson(file) {
  try {
    const text = readFileSync(file, "utf8");
    return { ok: true, data: JSON.parse(text) };
  } catch (error) {
    return { ok: false, error: String(error.message || error) };
  }
}

// Read plan.json from a run dir. Returns { ok, ids, prompts, error }.
// The plan is advisory for pairing: task ids come from it when present.
export function readPlan(dir) {
  const file = join(dir, "plan.json");
  if (!existsSync(file)) return { ok: false, ids: [], prompts: {}, error: `missing ${file}` };
  const parsed = readJson(file);
  if (!parsed.ok) return { ok: false, ids: [], prompts: {}, error: `unreadable ${file}: ${parsed.error}` };
  const plan = parsed.data;
  const tasks = Array.isArray(plan?.tasks) ? plan.tasks : [];
  const ids = [];
  const prompts = {};
  for (const task of tasks) {
    if (task && typeof task.id === "string") {
      ids.push(task.id);
      if (typeof task.prompt === "string") prompts[task.id] = task.prompt;
    }
  }
  return { ok: true, ids, prompts, error: null };
}

// Normalize one report task record. Missing fields -> null.
function normalizeRecord(rec) {
  const num = (v) => (typeof v === "number" && Number.isFinite(v) ? v : null);
  return {
    status: typeof rec?.status === "string" ? rec.status : null,
    attempts: num(rec?.attempts),
    durationMs: num(rec?.duration_ms ?? rec?.duration),
    inputTokens: num(rec?.input_tokens),
    outputTokens: num(rec?.output_tokens),
    cachedInputTokens: num(rec?.cached_input_tokens)
  };
}

// Read report.json from a run dir. Returns { ok, tasks: Map, error }.
// Accepts tasks as an object keyed by id or an array of records with id.
export function readReport(dir) {
  const file = join(dir, "report.json");
  if (!existsSync(file)) return { ok: false, tasks: new Map(), error: `missing ${file}` };
  const parsed = readJson(file);
  if (!parsed.ok) return { ok: false, tasks: new Map(), error: `unreadable ${file}: ${parsed.error}` };
  const report = parsed.data;
  const tasks = new Map();
  const raw = report?.tasks;
  if (Array.isArray(raw)) {
    for (const rec of raw) {
      if (rec && typeof rec.id === "string") tasks.set(rec.id, normalizeRecord(rec));
    }
  } else if (raw && typeof raw === "object") {
    for (const [id, rec] of Object.entries(raw)) {
      tasks.set(id, normalizeRecord(rec));
    }
  }
  return { ok: true, tasks, error: null };
}

// Stream trace.jsonl and count task_retry events per task id.
// Returns { ok, retries: Map<id, count>, error }. A missing trace is
// fine (empty counts), an unreadable one is an error.
export async function countRetries(tracePath) {
  const retries = new Map();
  if (!existsSync(tracePath)) return { ok: true, retries, error: null };
  try {
    const rl = createInterface({
      input: createReadStream(tracePath, { encoding: "utf8" }),
      crlfDelay: Infinity
    });
    for await (const line of rl) {
      const trimmed = line.trim();
      if (trimmed.length === 0) continue;
      let event;
      try {
        event = JSON.parse(trimmed);
      } catch {
        continue; // tolerate a corrupt line; count what parses
      }
      if (event?.event !== "task_retry") continue;
      const taskId = event?.fields?.task_id;
      if (typeof taskId !== "string") continue;
      retries.set(taskId, (retries.get(taskId) || 0) + 1);
    }
    return { ok: true, retries, error: null };
  } catch (error) {
    return { ok: false, retries, error: `unreadable ${tracePath}: ${String(error.message || error)}` };
  }
}
