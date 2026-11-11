// One vision call per image. OpenAI-compatible chat/completions endpoint.
//
// Retry policy (the only retry this pack has): one automatic retry on
// HTTP 429/5xx with a 2s backoff. Any other failure, or a failed retry,
// returns { ok: false, why } and the caller WARN-SKIPs the image. A judge
// failure is never a gate failure.
//
// Transport is node:http, not fetch: undici keeps a pooled keep-alive
// socket alive past the last response, and process.exit on Windows can
// race that closing handle (libuv assertion in async.c). agent:false
// closes the socket with the response, so nothing outlives the run.
import http from "node:http";
import https from "node:https";

const RETRY_STATUSES = (status) => status === 429 || status >= 500;

function sleep(ms) {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

function postJson(urlStr, headers, body, timeoutMs) {
  return new Promise((resolve, reject) => {
    const u = new URL(urlStr);
    const mod = u.protocol === "https:" ? https : http;
    const req = mod.request(
      u,
      {
        method: "POST",
        headers: { ...headers, "content-length": Buffer.byteLength(body) },
        agent: false,
        timeout: timeoutMs,
      },
      (res) => {
        let data = "";
        res.setEncoding("utf8");
        res.on("data", (c) => (data += c));
        res.on("end", () => resolve({ status: res.statusCode ?? 0, body: data }));
      },
    );
    req.on("timeout", () => {
      req.destroy(new Error(`request timed out after ${timeoutMs}ms`));
    });
    req.on("error", reject);
    req.write(body);
    req.end();
  });
}

export async function judgeImage({ baseUrl, apiKey, model, rubric, imageB64, timeoutMs = 30000 }) {
  const url = `${baseUrl.replace(/\/+$/, "")}/chat/completions`;
  const prompt = `${rubric}\n\nRespond with one JSON object: {score: number, flags: [...], note: string}`;
  const body = JSON.stringify({
    model,
    messages: [
      {
        role: "user",
        content: [
          { type: "text", text: prompt },
          { type: "image_url", image_url: { url: `data:image/png;base64,${imageB64}` } },
        ],
      },
    ],
  });
  const headers = {
    "content-type": "application/json",
    authorization: `Bearer ${apiKey}`,
  };

  for (let attempt = 1; attempt <= 2; attempt += 1) {
    try {
      const res = await postJson(url, headers, body, timeoutMs);
      if (RETRY_STATUSES(res.status) && attempt === 1) {
        await sleep(2000);
        continue;
      }
      if (res.status < 200 || res.status >= 300) return { ok: false, why: `HTTP ${res.status}` };
      let data = null;
      try {
        data = JSON.parse(res.body);
      } catch {
        data = null;
      }
      const parsed = parseJudgeJson(data?.choices?.[0]?.message?.content);
      if (!parsed) return { ok: false, why: "judge response was not parseable JSON" };
      return { ok: true, score: parsed.score, flags: parsed.flags, note: parsed.note };
    } catch (err) {
      // Network errors (server down, refused connection) get no retry:
      // only HTTP 429/5xx retries. A dead judge must fail fast, not wait.
      return { ok: false, why: `request failed: ${err.message || err}` };
    }
  }
  return { ok: false, why: "unreachable" };
}

// Strip code fences, take the outermost {...} block, require a numeric
// score. Anything else is unparseable and the image is skipped, never
// guessed.
export function parseJudgeJson(text) {
  if (typeof text !== "string") return null;
  let t = text.trim();
  const fence = t.match(/^```(?:json)?\s*([\s\S]*?)```$/);
  if (fence) t = fence[1].trim();
  const start = t.indexOf("{");
  const end = t.lastIndexOf("}");
  if (start === -1 || end === -1 || end <= start) return null;
  let obj;
  try {
    obj = JSON.parse(t.slice(start, end + 1));
  } catch {
    return null;
  }
  if (!obj || typeof obj !== "object" || Array.isArray(obj)) return null;
  if (typeof obj.score !== "number" || !Number.isFinite(obj.score)) return null;
  const flags = Array.isArray(obj.flags) ? obj.flags.filter((f) => typeof f === "string") : [];
  const note = typeof obj.note === "string" ? obj.note : undefined;
  return { score: obj.score, flags, note };
}
