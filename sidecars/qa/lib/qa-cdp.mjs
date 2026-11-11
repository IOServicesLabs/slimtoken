// qa-cdp.mjs — shared helpers for the browser QA harness. Zero-dependency:
// node 22+ global WebSocket + fetch only. Imported by qa-smoke.mjs and
// qa-flow.mjs.

import { existsSync } from "node:fs";

export function sleep(ms) {
  return new Promise((r) => setTimeout(r, ms));
}

export function findChrome() {
  if (process.env.CHROME_PATH && existsSync(process.env.CHROME_PATH)) {
    return process.env.CHROME_PATH;
  }
  const candidates = [
    "C:\\Program Files\\Google\\Chrome\\Application\\chrome.exe",
    "C:\\Program Files (x86)\\Google\\Chrome\\Application\\chrome.exe",
    process.env.LOCALAPPDATA + "\\Google\\Chrome\\Application\\chrome.exe",
    "C:\\Program Files (x86)\\Microsoft\\Edge\\Application\\msedge.exe",
    "C:\\Program Files\\Microsoft\\Edge\\Application\\msedge.exe",
  ];
  for (const path of candidates) {
    if (path && existsSync(path)) return path;
  }
  throw new Error("no Chrome/Edge binary found; set CHROME_PATH");
}

export async function waitForDebugger(port, timeoutMs) {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    try {
      const res = await fetch(`http://127.0.0.1:${port}/json/version`);
      if (res.ok) return true;
    } catch {
      // not up yet
    }
    await sleep(200);
  }
  throw new Error(`Chrome debugger did not come up on port ${port}`);
}

export async function newPageTarget(port) {
  // Newer Chrome requires PUT on /json/new; fall back to GET.
  for (const method of ["PUT", "GET"]) {
    try {
      const res = await fetch(`http://127.0.0.1:${port}/json/new?about:blank`, { method });
      if (res.ok) return await res.json();
    } catch {
      // try next method
    }
  }
  throw new Error("could not create a page target");
}

export class CdpClient {
  constructor(wsUrl) {
    this.ws = new WebSocket(wsUrl);
    this.nextId = 1;
    this.pending = new Map();
    this.listeners = [];
    this.ready = new Promise((resolvePromise, reject) => {
      this.ws.addEventListener("open", resolvePromise);
      this.ws.addEventListener("error", (event) => reject(new Error(`websocket error: ${event.message || "unknown"}`)));
    });
    this.ws.addEventListener("message", (event) => {
      const msg = JSON.parse(event.data);
      if (msg.id !== undefined && this.pending.has(msg.id)) {
        const { resolve: resolveCall, reject } = this.pending.get(msg.id);
        this.pending.delete(msg.id);
        if (msg.error) reject(new Error(msg.error.message));
        else resolveCall(msg.result);
      } else if (msg.method) {
        for (const listener of [...this.listeners]) listener(msg);
      }
    });
  }

  onEvent(listener) {
    this.listeners.push(listener);
    return () => {
      const at = this.listeners.indexOf(listener);
      if (at >= 0) this.listeners.splice(at, 1);
    };
  }

  async send(method, params = {}) {
    await this.ready;
    const id = this.nextId;
    this.nextId += 1;
    return new Promise((resolveCall, reject) => {
      this.pending.set(id, { resolve: resolveCall, reject });
      this.ws.send(JSON.stringify({ id, method, params }));
    });
  }

  close() {
    try { this.ws.close(); } catch { /* already closed */ }
  }
}

// Send with one retry: transient CDP hiccups should not fail a step.
export async function sendRetry(client, method, params = {}) {
  try {
    return await client.send(method, params);
  } catch (error) {
    await sleep(150);
    return client.send(method, params);
  }
}

export function safeName(route) {
  const cleaned = String(route).replace(/[^a-zA-Z0-9]+/g, "-").replace(/^-+|-+$/g, "");
  return cleaned || "root";
}
