// qa-flow.mjs — scripted interaction QA over the Chrome DevTools Protocol.
// Zero-dependency (node 22+ global WebSocket + fetch), same transport as
// qa-smoke.mjs. Runs a JSON flow of user-like steps and reports each
// step's pass/fail, so interaction QA is deterministic, repeatable, and
// costs no API tokens. The LLM writes the flow; this runner executes it.
//
// Usage:
//   node qa-flow.mjs --url http://localhost:3000 --flow flow.json --out qa-flow-out
//
// Flow JSON:
//   {
//     "name": "pairs-game",
//     "steps": [
//       { "goto": "/pairs" },
//       { "click": ".tile" },                     // CSS selector (first match)
//       { "type": ["#search", "Rust"] },          // [selector, text]
//       { "key": "Enter" },
//       { "wait": 500 },                          // ms
//       { "waitFor": "h1" },                      // selector, up to 10s
//       { "expectUrl": "/wiki/Special:Search" },  // substring of location.href
//       { "expectText": "general-purpose" },      // substring of body text
//       { "expectTitle": "Search results" },      // substring of title
//       { "submit": "#search" },                  // submit the nearest form
//       { "screenshot": "01-result" },            // saved as NN-<name>.png
//       { "eval": "const n = document.querySelectorAll('p').length; n" }
//                                                 // full statements; last
//                                                 // expression value recorded
//     ]
//   }
//
// eval bodies are evaluated as direct eval: semicolons, declarations,
// and if/for statements are all legal. The completion value of the last
// expression statement is recorded; undefined is recorded as "undefined"
// and passes.
//
// Console errors and uncaught exceptions during the flow fail it. Exit 0
// when every step passes; 1 otherwise. Output: flow.json + report.md +
// screenshots in <out>.

import { spawn } from "node:child_process";
import { mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { basename, join, resolve } from "node:path";
import { CdpClient, findChrome, newPageTarget, safeName, sendRetry, sleep, waitForDebugger } from "./qa-cdp.mjs";

function parseArgs(argv) {
  const args = {
    url: "http://localhost:3000",
    flow: null,
    out: "qa-flow-out",
    port: 9334,
    width: 1280,
    height: 900,
    floor: 500,
    headed: false,
  };
  for (let i = 2; i < argv.length; i += 1) {
    const key = argv[i];
    if (key === "--headed") { args.headed = true; i -= 1; continue; }
    if (!key.startsWith("--")) continue;
    const name = key.slice(2);
    const value = argv[i + 1];
    if (name in args) args[name] = Number.isNaN(Number(value)) ? value : Number(value);
    i += 1;
  }
  return args;
}

// Evaluate a body that returns a JSON-able value.
// The body is evaluated as a direct eval inside an async function: any
// legal JS works — semicolons, declarations, if/for. The completion value
// of the last expression statement is the result (an eval block, unlike a
// function body, has a completion value); undefined is recorded via a
// sentinel and decoded back to undefined.
const UNDEF = "__QA_FLOW_UNDEFINED_7f3d__";

async function evaluate(client, expression) {
  let body = String(expression).trim();
  // A leading "{" would parse as a block statement, not an object literal.
  if (body.startsWith("{") && !body.startsWith("{{")) body = `(${body})`;
  const wrapped = `(async () => { return eval(${JSON.stringify(body)}); })()`
    + `.then(v => JSON.stringify(v === undefined ? ${JSON.stringify(UNDEF)} : v))`;
  const res = await sendRetry(client, "Runtime.evaluate", {
    expression: wrapped,
    returnByValue: true,
    awaitPromise: true,
  });
  if (res.exceptionDetails) {
    const desc = res.exceptionDetails.exception?.description || res.exceptionDetails.text || "eval failed";
    if (/SyntaxError/.test(desc)) {
      throw new Error(
        `eval SyntaxError: ${desc}\n` +
        `  eval bodies run as full statements — write plain JS, no comma-operator workaround needed.\n` +
        `  body sent: ${body.slice(0, 200)}`
      );
    }
    throw new Error(desc);
  }
  if (res.result.value === undefined) return null; // JSON.stringify(undefined) guard
  const parsed = JSON.parse(res.result.value);
  return parsed === UNDEF ? undefined : parsed;
}

async function evalNoReturn(client, expression) {
  const res = await sendRetry(client, "Runtime.evaluate", {
    expression, returnByValue: true, awaitPromise: true,
  });
  if (res.exceptionDetails) {
    throw new Error(res.exceptionDetails.exception?.description || res.exceptionDetails.text || "eval failed");
  }
  return res.result.value;
}

const SELECTOR_CLICK = (selector) => `
  (() => {
    const el = document.querySelector(${JSON.stringify(selector)});
    if (!el) return "no element for selector";
    el.scrollIntoView({ block: "center" });
    el.click();
    return null;
  })()
`;

// React-controlled inputs need the native setter + an input event.
const SELECTOR_TYPE = (selector, text) => `
  (() => {
    const el = document.querySelector(${JSON.stringify(selector)});
    if (!el) return "no element for selector";
    el.scrollIntoView({ block: "center" });
    el.focus();
    const proto = el instanceof HTMLTextAreaElement ? HTMLTextAreaElement.prototype : HTMLInputElement.prototype;
    const setter = Object.getOwnPropertyDescriptor(proto, "value").set;
    setter.call(el, ${JSON.stringify(text)});
    el.dispatchEvent(new Event("input", { bubbles: true }));
    el.dispatchEvent(new Event("change", { bubbles: true }));
    return null;
  })()
`;

async function runStep(client, step, index, args, ctx) {
  const [kind, value] = Object.entries(step)[0];
  const started = Date.now();
  const record = (status, detail = null, extra = {}) => ({
    step: index + 1, kind, arg: typeof value === "object" ? JSON.stringify(value) : String(value),
    status, ms: Date.now() - started, detail, ...extra,
  });

  switch (kind) {
    case "goto": {
      const url = new URL(value, args.url).href;
      let loadFired;
      const loadPromise = new Promise((r) => { loadFired = r; });
      const detach = client.onEvent((msg) => {
        if (msg.method === "Page.loadEventFired") loadFired();
      });
      try {
        const navTimeout = setTimeout(loadFired, 30000);
        try {
          await sendRetry(client, "Page.navigate", { url });
          await loadPromise;
        } finally {
          clearTimeout(navTimeout);
        }
        await sleep(args.floor);
        return record("pass", new URL(url).pathname);
      } finally {
        detach();
      }
    }
    case "click": {
      const err = await evaluate(client, SELECTOR_CLICK(value));
      await sleep(120);
      return err ? record("fail", err) : record("pass");
    }
    case "type": {
      const [selector, text] = value;
      const err = await evaluate(client, SELECTOR_TYPE(selector, text));
      await sleep(120);
      return err ? record("fail", err) : record("pass");
    }
    case "key": {
      const key = value;
      const named = { Escape: "Escape", Tab: "Tab", ArrowUp: "ArrowUp", ArrowDown: "ArrowDown",
        ArrowLeft: "ArrowLeft", ArrowRight: "ArrowRight", Backspace: "Backspace", Delete: "Delete", Enter: "Enter" };
      const code = named[key] || (key.length === 1 ? `Key${key.toUpperCase()}` : key);
      await sendRetry(client, "Input.dispatchKeyEvent", { type: "keyDown", key, code });
      await sendRetry(client, "Input.dispatchKeyEvent", { type: "keyUp", key, code });
      await sleep(120);
      return record("pass");
    }
    case "submit": {
      // Submit the nearest form: requestSubmit (fires validation + submit
      // handler, unlike a synthetic Enter key) with a click fallback.
      // No top-level return: eval bodies reject it.
      const err = await evaluate(client, `
        const el = document.querySelector(${JSON.stringify(value)});
        let err = null;
        if (!el) err = "no element for selector";
        else {
          const form = el.tagName === "FORM" ? el : el.closest("form");
          if (!form) err = "no form for selector";
          else {
            const btn = form.querySelector('[type="submit"]');
            if (form.requestSubmit && btn) form.requestSubmit(btn);
            else if (btn) btn.click();
            else err = "form has no submit button";
          }
        }
        err
      `);
      await sleep(120);
      return err ? record("fail", err) : record("pass");
    }
    case "wait":
      await sleep(Number(value));
      return record("pass");
    case "waitFor": {
      const deadline = Date.now() + 10000;
      while (Date.now() < deadline) {
        const found = await evaluate(client, `document.querySelector(${JSON.stringify(value)}) !== null`);
        if (found) return record("pass");
        await sleep(100);
      }
      return record("fail", `selector ${value} did not appear within 10s`);
    }
    case "expectUrl": {
      const href = await evaluate(client, "location.href");
      return href.includes(value) ? record("pass", href) : record("fail", `location.href is ${href}`);
    }
    case "expectText": {
      const text = await evaluate(client, "document.body ? document.body.innerText : ''");
      return text.includes(value) ? record("pass") : record("fail", `body text does not contain ${JSON.stringify(value)}`);
    }
    case "expectTitle": {
      const title = await evaluate(client, "document.title");
      return title.includes(value) ? record("pass", title) : record("fail", `title is ${JSON.stringify(title)}`);
    }
    case "expectEval": {
      const [expression, expected] = value;
      const actual = await evaluate(client, expression);
      const pass = String(actual) === String(expected);
      return pass ? record("pass", String(actual))
        : record("fail", `expected ${JSON.stringify(expected)} (${typeof expected}), got ${JSON.stringify(actual)} (${typeof actual})`);
    }
    case "screenshot": {
      const shot = await sendRetry(client, "Page.captureScreenshot", { format: "png" });
      const name = `${String(index + 1).padStart(2, "0")}-${safeName(value)}.png`;
      writeFileSync(join(args.outDir, name), Buffer.from(shot.data, "base64"));
      return record("pass", null, { screenshot: name });
    }
    case "eval": {
      const result = await evaluate(client, value);
      return record("pass", result === undefined ? "undefined" : JSON.stringify(result), { value: result ?? null });
    }
    default:
      return record("fail", `unknown step kind ${kind}`);
  }
}

function buildMarkdown(report) {
  const lines = [];
  lines.push(`# Flow QA report — ${report.flow} on ${report.baseUrl}`);
  lines.push("");
  lines.push(`Started: ${report.startedAt} · ${report.summary.passed}/${report.summary.steps} steps passed · ${report.summary.consoleErrors} console errors · ${report.summary.exceptions} exceptions`);
  lines.push("");
  lines.push("| # | step | arg | status | ms | detail |");
  lines.push("|---|---|---|---|---|---|");
  for (const s of report.steps) {
    lines.push(`| ${s.step} | ${s.kind} | ${s.arg} | ${s.status} | ${s.ms} | ${s.detail ?? ""} |`);
  }
  const errors = report.steps.filter((s) => s.status === "fail");
  if (errors.length > 0) {
    lines.push("");
    lines.push("## Failures");
    for (const s of errors) lines.push(`- step ${s.step} ${s.kind} ${s.arg}: ${s.detail}`);
  }
  if (report.consoleErrors.length > 0) {
    lines.push("");
    lines.push("## Console errors");
    for (const e of report.consoleErrors) lines.push(`- ${e.split("\n")[0]}`);
  }
  return lines.join("\n") + "\n";
}

async function main() {
  const args = parseArgs(process.argv);
  if (!args.flow) throw new Error("pass --flow <file.json> (or --flow - for stdin)");
  args.outDir = resolve(args.out);
  mkdirSync(args.outDir, { recursive: true });

  // Windows PowerShell 5.1 pipes strings to native commands with a UTF-8
  // BOM preamble; strip one so `--flow -` works under every host shell.
  const flowText = (args.flow === "-" ? readFileSync(0, "utf8") : readFileSync(args.flow, "utf8"))
    .replace(/^﻿/, "");
  const parsed = JSON.parse(flowText);
  let flow = parsed;
  if (Array.isArray(parsed)) {
    flow = { name: basename(args.flow, ".json"), steps: parsed };
    console.error("note: bare array flow JSON auto-wrapped as {name, steps}; use the {\"name\",\"steps\"} shape for clarity");
  }
  if (!flow || typeof flow !== "object" || !Array.isArray(flow.steps) || flow.steps.length === 0) {
    throw new Error(`flow must be {"name": "...", "steps": [...]} — got ${Array.isArray(parsed) ? "an array" : typeof parsed}`);
  }

  const chrome = findChrome();
  const userDataDir = join(tmpdir(), `qa-flow-${process.pid}`);
  const proc = spawn(chrome, [
    args.headed ? "--headless=new --auto-open-devtools-for-tabs" : "--headless=new",
    `--remote-debugging-port=${args.port}`,
    `--user-data-dir=${userDataDir}`,
    "--no-first-run",
    "--disable-extensions",
    `--window-size=${args.width},${args.height}`,
    "about:blank",
  ], { stdio: "ignore" });

  const report = {
    flow: flow.name || args.flow,
    baseUrl: args.url,
    startedAt: new Date().toISOString(),
    steps: [],
    consoleErrors: [],
    exceptions: [],
    summary: {},
  };

  try {
    await waitForDebugger(args.port, 20000);
    const target = await newPageTarget(args.port);
    const client = new CdpClient(target.webSocketDebuggerUrl);
    await sendRetry(client, "Emulation.setDeviceMetricsOverride", {
      width: args.width, height: args.height, deviceScaleFactor: 1, mobile: false,
    });

    client.onEvent((msg) => {
      if (msg.method === "Runtime.consoleAPICalled" && msg.params.type === "error") {
        const text = (msg.params.args || []).map((a) => a.value ?? a.description ?? "").join(" ");
        report.consoleErrors.push(text);
      } else if (msg.method === "Runtime.exceptionThrown") {
        const d = msg.params.exceptionDetails;
        report.exceptions.push(d.exception?.description || d.text || "unknown exception");
      }
    });
    await sendRetry(client, "Page.enable");
    await sendRetry(client, "Runtime.enable");

    for (let i = 0; i < flow.steps.length; i += 1) {
      const step = flow.steps[i];
      try {
        const result = await runStep(client, step, i, args, {});
        report.steps.push(result);
        console.log(`${result.status} ${i + 1}. ${result.kind} ${result.arg}${result.detail ? ` — ${result.detail}` : ""}`);
        if (result.status === "fail") break; // later steps build on broken state
      } catch (error) {
        report.steps.push({
          step: i + 1, kind: Object.keys(step)[0], arg: String(Object.values(step)[0]),
          status: "fail", ms: 0, detail: String(error.message || error),
        });
        console.log(`FAIL ${i + 1}. ${Object.keys(step)[0]}: ${error.message || error}`);
        break;
      }
    }
    client.close();
  } finally {
    // Killing Chrome while its spawn/stdio handles are still settling trips a
    // libuv assertion on Windows (exit 0xC0000409 instead of the real code).
    // Settle briefly before the kill; report + exit code stay authoritative.
    await sleep(400);
    proc.kill("SIGKILL");
  }

  report.summary = {
    steps: report.steps.length,
    passed: report.steps.filter((s) => s.status === "pass").length,
    consoleErrors: report.consoleErrors.length,
    exceptions: report.exceptions.length,
  };
  writeFileSync(join(args.outDir, "flow.json"), JSON.stringify(report, null, 2));
  writeFileSync(join(args.outDir, "report.md"), buildMarkdown(report));
  const clean = report.summary.passed === report.summary.steps
    && report.consoleErrors.length === 0 && report.exceptions.length === 0;
  console.log(`summary: ${report.summary.passed}/${report.summary.steps} steps passed, ${report.summary.consoleErrors} console errors, ${report.summary.exceptions} exceptions -> ${join(args.out, "report.md")}`);
  process.exit(clean ? 0 : 1);
}

main().catch((error) => {
  console.error(`qa-flow: ${error.message || error}`);
  process.exit(2);
});
