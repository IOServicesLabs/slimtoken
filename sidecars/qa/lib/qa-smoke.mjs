// qa-smoke.mjs — dependency-free browser smoke QA over the Chrome DevTools
// Protocol. No npm packages: node 22+ global WebSocket + fetch only.
// Shared transport lives in qa-cdp.mjs.
//
// Usage:
//   node qa-smoke.mjs --url http://localhost:3000 --routes /,/review --out qa-out
//   node qa-smoke.mjs --url https://example.com --discover 50 --parallel 6
//   node qa-smoke.mjs --url http://localhost:3000 --expect-build abc123 --routes /
//   node qa-smoke.mjs --url http://localhost:3000 --axe axe.min.js --routes /
//   node qa-smoke.mjs --url http://localhost:3000 --devices devices.json --golden gold --routes /
//
// --expect-build asserts the served build fingerprint (see Build: in the
// report). Mismatch, or no extractable identity, fails closed with exit 2.
//
// What it does, per route: loads the page in headless Chrome, waits for
// load plus network quiet (capped), collects console errors/warnings,
// uncaught exceptions, failed network requests, and the document status;
// detects blank pages; writes a screenshot (PNG, viewport or full page);
// extracts title, headings, and interactive-element counts for the
// reviewer. Results land in <out>/smoke.json, <out>/report.md (the judge
// report), and <out>/NN-route.png.
//
// Exit code 0 when every route loaded with no console errors, exceptions,
// failed requests, or blank pages; 1 otherwise. The judge (human or LLM)
// reads report.md and the screenshots; this layer never judges.

import { spawn } from "node:child_process";
import { createHash } from "node:crypto";
import { existsSync, mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { inflateSync } from "node:zlib";
import { CdpClient, findChrome, newPageTarget, safeName, sendRetry, sleep, waitForDebugger } from "./qa-cdp.mjs";

function parseArgs(argv) {
  const args = {
    url: "http://localhost:3000",
    routes: [],
    out: "qa-out",
    port: 9333,
    width: 1280,
    height: 900,
    settle: 3000,
    quiet: 250,
    floor: 800,
    timeout: 30000,
    parallel: 4,
    discover: 0,
    maxroutes: 200,
    fullpage: false,
    headed: false,
    expectBuild: "",
    visibility: true,
    // F-series audit extensions (opt-in; defaults preserve the original
    // signal set so frozen A/B harness copies keep their meaning):
    viewports: [],        // F1: extra "WxH" viewports, full audit each
    schemes: [],          // F2: "dark" | "light" | "forced", contrast re-audit
    golden: "",           // F3: dir of baseline screenshots to diff against
    goldenUpdate: false,  // F3: write the baseline instead of comparing
    seedFile: "",         // F5: JS run on every new document before load
    maxBytes: 0,          // F4: page-weight budget, DECODED bytes; 0 = off
    maxLcpMs: 0,          // F4: LCP budget (ms); 0 = off
    a11y: false,          // F7: unlabeled controls, img-no-alt, heading skips
    warnGate: false,      // F8: console WARNINGS also fail the route
    domAudit: false,      // F9: duplicate ids, broken same-origin links, clipped text
    states: [],           // F10: "hover" | "focus", pseudo-state sampling audits
    axe: "",              // F13: path to a host-supplied axe-core UMD bundle
    devices: "",          // F14: path to a device-matrix JSON file
    keyboard: false,      // F11: Tab-dispatch trap check (real CDP key events)
    motion: [],           // F12: "reduce", running-animation audit under emulated media
  };
  for (let i = 2; i < argv.length; i += 1) {
    const key = argv[i];
    // Bare flags: the loop's own i += 1 advances past them. (An earlier
    // i -= 1 here re-read the same flag forever — the loop only ended
    // because no bare flag was ever passed. Found 2026-11-05 when
    // --golden-update hung the run.)
    if (key === "--fullpage") { args.fullpage = true; continue; }
    if (key === "--headed") { args.headed = true; continue; }
    if (key === "--no-visibility") { args.visibility = false; continue; }
    if (key === "--golden-update") { args.goldenUpdate = true; continue; }
    if (key === "--a11y") { args.a11y = true; continue; }
    // Hyphenated BARE flags: the generic branch below would swallow the
    // next argv element as their value (found 2026-11-05: --warn-gate ate
    // --dom-audit as its value, so --dom-audit silently never ran).
    if (key === "--warn-gate") { args.warnGate = true; continue; }
    if (key === "--dom-audit") { args.domAudit = true; continue; }
    if (key === "--keyboard") { args.keyboard = true; continue; }
    if (!key.startsWith("--")) continue;
    // Hyphenated flags map to camelCase args keys ("--seed-file" ->
    // args.seedFile). Without this, hyphenated names fall through the
    // `name in args` check and are silently dropped.
    const name = key.slice(2).replace(/-([a-z])/g, (_, c) => c.toUpperCase());
    const value = argv[i + 1];
    if (name === "routes") args.routes = value.split(",").map((r) => r.trim());
    else if (name === "viewports") args.viewports = value.split(",").map((v) => v.trim()).filter(Boolean);
    else if (name === "schemes") args.schemes = value.split(",").map((s) => s.trim()).filter(Boolean);
    else if (name === "states") args.states = value.split(",").map((s) => s.trim()).filter(Boolean);
    else if (name === "motion") args.motion = value.split(",").map((s) => s.trim()).filter(Boolean);
    else if (name in args) args[name] = Number.isNaN(Number(value)) ? value : Number(value);
    i += 1;
  }
  args.parallel = Math.max(1, Math.min(8, args.parallel));
  return args;
}

// --- F3: minimal PNG decode + pixel diff (node:zlib only) ------------------
// Chrome screenshots are 8-bit RGB or RGBA. We decode just enough of the
// format (IHDR/IDAT/IEND, filter types 0-4) to compare pixels; the golden
// gate uses a per-channel tolerance to absorb antialiasing noise.

function decodePng(buf) {
  const SIG = Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]);
  if (buf.length < 8 || !buf.subarray(0, 8).equals(SIG)) throw new Error("not a PNG");
  let off = 8;
  let width = 0; let height = 0; let bitDepth = 0; let colorType = 0;
  const idat = [];
  while (off + 12 <= buf.length) {
    const len = buf.readUInt32BE(off);
    const type = buf.toString("ascii", off + 4, off + 8);
    const data = buf.subarray(off + 8, off + 8 + len);
    if (type === "IHDR") {
      width = data.readUInt32BE(0);
      height = data.readUInt32BE(4);
      bitDepth = data[8];
      colorType = data[9];
    } else if (type === "IDAT") {
      idat.push(data);
    } else if (type === "IEND") {
      break;
    }
    off += 12 + len;
  }
  if (bitDepth !== 8 || (colorType !== 2 && colorType !== 6 && colorType !== 0)) {
    throw new Error(`unsupported PNG (bitDepth ${bitDepth}, colorType ${colorType})`);
  }
  const channels = colorType === 6 ? 4 : colorType === 2 ? 3 : 1;
  const raw = inflateSync(Buffer.concat(idat));
  const stride = width * channels;
  const out = Buffer.alloc(width * height * 4);
  let prev = Buffer.alloc(stride);
  let pos = 0;
  const paeth = (a, b, c) => {
    const p = a + b - c;
    const pa = Math.abs(p - a); const pb = Math.abs(p - b); const pc = Math.abs(p - c);
    return pa <= pb && pa <= pc ? a : pb <= pc ? b : c;
  };
  for (let y = 0; y < height; y += 1) {
    const filter = raw[pos];
    const line = Buffer.from(raw.subarray(pos + 1, pos + 1 + stride));
    pos += 1 + stride;
    for (let x = 0; x < stride; x += 1) {
      const a = x >= channels ? line[x - channels] : 0;
      const b = prev[x];
      const c = x >= channels ? prev[x - channels] : 0;
      if (filter === 1) line[x] = (line[x] + a) & 0xff;
      else if (filter === 2) line[x] = (line[x] + b) & 0xff;
      else if (filter === 3) line[x] = (line[x] + ((a + b) >> 1)) & 0xff;
      else if (filter === 4) line[x] = (line[x] + paeth(a, b, c)) & 0xff;
    }
    for (let x = 0; x < width; x += 1) {
      const si = x * channels;
      const di = (y * width + x) * 4;
      if (colorType === 0) {
        out[di] = line[si]; out[di + 1] = line[si]; out[di + 2] = line[si];
      } else {
        out[di] = line[si]; out[di + 1] = line[si + 1]; out[di + 2] = line[si + 2];
      }
      out[di + 3] = 255;
    }
    prev = line;
  }
  return { width, height, data: out };
}

const GOLDEN_TOLERANCE = 8;      // per-channel, 0-255
const GOLDEN_THRESHOLD_PCT = 0.5; // diff above this = visual regression

function pngDiff(a, b) {
  if (a.width !== b.width || a.height !== b.height) {
    return { pct: 100, note: `size ${a.width}x${a.height} vs golden ${b.width}x${b.height}` };
  }
  let diff = 0;
  const total = a.width * a.height;
  for (let i = 0; i < a.data.length; i += 4) {
    if (Math.abs(a.data[i] - b.data[i]) > GOLDEN_TOLERANCE
      || Math.abs(a.data[i + 1] - b.data[i + 1]) > GOLDEN_TOLERANCE
      || Math.abs(a.data[i + 2] - b.data[i + 2]) > GOLDEN_TOLERANCE) {
      diff += 1;
    }
  }
  return { pct: +(100 * diff / total).toFixed(3), diffPixels: diff, totalPixels: total };
}

async function waitForNetworkQuiet(client, inFlight, args, deadline) {
  let quietSince = null;
  while (Date.now() < deadline) {
    if (inFlight.size === 0) {
      if (quietSince === null) quietSince = Date.now();
      if (Date.now() - quietSince >= args.quiet) return;
    } else {
      quietSince = null;
    }
    await sleep(50);
  }
}

// F10 pseudo-state sampling audits (opt-in, --states hover,focus). Static
// audits cannot see :hover-only defects (d06/d34): CDP CSS.forcePseudoState
// forces the pseudo-class per node and computed style re-resolves, so the
// contrast math runs on the hovered state. Focus forces BOTH focus and
// focus-visible: UA styles gate the default outline behind :focus-visible,
// and forcing only :focus would report "no indicator" on a pristine page.
const HOVER_SAMPLE_LIMIT = 150;
const FOCUS_SAMPLE_LIMIT = 50;

function stateContrastSnippet(idx, sel) {
  return `(() => {
    const el = [...document.querySelectorAll(${JSON.stringify(sel)})][${idx}];
    if (!el) return null;
    // Worst contrast over the element AND its text descendants: parent
    // :hover rules (".stats:hover { color: Canvas }") only show up in a
    // child's computed style, so sampling the forced element alone misses
    // the most common real-world shape of this defect.
    const inner = [el, ...el.querySelectorAll("span, p, div, li, a, button, h1, h2, h3, label, strong, em")].slice(0, 20);
    const chan = (s) => s.replace(/[^0-9.,]/g, "").split(",").map(Number).slice(0, 3);
    const lum = (rgb) => { const c = rgb.map((v) => { v /= 255; return v <= 0.03928 ? v / 12.92 : Math.pow((v + 0.055) / 1.055, 2.4); }); return 0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2]; };
    let worst = null;
    for (const e of inner) {
      if (!e.textContent.trim()) continue;
      const cs = getComputedStyle(e);
      if (cs.display === "none" || cs.visibility === "hidden") continue;
      const fg = cs.color;
      let bg = null; let cur = e;
      while (cur && !bg) {
        const b = getComputedStyle(cur).backgroundColor;
        if (b && b !== "transparent" && b !== "rgba(0, 0, 0, 0)") bg = b;
        cur = cur.parentElement;
      }
      if (!bg) bg = "rgb(255, 255, 255)";
      const l1 = lum(chan(fg)); const l2 = lum(chan(bg));
      const ratio = +((Math.max(l1, l2) + 0.05) / (Math.min(l1, l2) + 0.05)).toFixed(2);
      if (!worst || ratio < worst.ratio) worst = { ratio, color: fg, background: bg, text: e.textContent.trim().slice(0, 40) };
    }
    return worst;
  })()`;
}

function stateFocusSnippet(idx, sel) {
  return `(() => {
    const el = [...document.querySelectorAll(${JSON.stringify(sel)})][${idx}];
    if (!el) return null;
    const cs = getComputedStyle(el);
    return { outlineStyle: cs.outlineStyle, outlineWidth: cs.outlineWidth, outlineColor: cs.outlineColor, boxShadow: cs.boxShadow };
  })()`;
}

async function collectStateAudit(client, args) {
  const out = { hoverFailures: [], focusFailures: [], error: null };
  const wantHover = args.states.includes("hover");
  const wantFocus = args.states.includes("focus");
  if (!wantHover && !wantFocus) return out;
  const HOVER_SEL = "p, span, div, li, td, th, button, a, h1, h2, h3, h4, h5, h6, label, strong, em";
  const FOCUS_SEL = "a[href], button, input, select, textarea, [tabindex]";
  const pathFn = `(el) => { const bits = []; let cur = el; for (let i = 0; cur && i < 3; i += 1) { let part = cur.tagName ? cur.tagName.toLowerCase() : "?"; const cls = (cur.getAttribute("class") || "").trim(); if (cls) part += "." + cls.split(" ")[0]; bits.unshift(part); cur = cur.parentElement; } return bits.join(" > "); }`;
  try {
    // DOM must be enabled BEFORE CSS in this Chrome: CSS.enable fails with
    // "DOM agent needs to be enabled first" otherwise (repro'd 2026-11-05;
    // the failure surfaces as a silent state-audit miss on the matrix).
    await sendRetry(client, "DOM.enable");
    await sendRetry(client, "CSS.enable");
    const doc = await sendRetry(client, "DOM.getDocument");
    const rootId = doc.root.nodeId;
    if (wantHover) {
      const info = await sendRetry(client, "Runtime.evaluate", {
        expression: `JSON.stringify([...document.querySelectorAll(${JSON.stringify(HOVER_SEL)})].map((el, i) => ({ i, text: el.textContent.trim().slice(0, 40), on: (() => { const cs = getComputedStyle(el); return cs.display !== "none" && cs.visibility !== "hidden"; })(), path: (${pathFn})(el) })).filter((o) => o.text && o.on).slice(0, ${HOVER_SAMPLE_LIMIT}))`,
        returnByValue: true,
      });
      const candidates = JSON.parse(info.result.value);
      const q = await sendRetry(client, "DOM.querySelectorAll", { nodeId: rootId, selector: HOVER_SEL });
      for (const c of candidates) {
        const nodeId = q.nodeIds[c.i];
        if (!nodeId) continue;
        await sendRetry(client, "CSS.forcePseudoState", { nodeId, forcedPseudoClasses: ["hover"] });
        const r = await sendRetry(client, "Runtime.evaluate", {
          expression: stateContrastSnippet(c.i, HOVER_SEL), returnByValue: true,
        });
        await sendRetry(client, "CSS.forcePseudoState", { nodeId, forcedPseudoClasses: [] });
        const v = r.exceptionDetails ? null : r.result.value;
        if (v && v.ratio <= 1.05) {
          out.hoverFailures.push({ selector: c.path, text: v.text || c.text, ratio: v.ratio, color: v.color, background: v.background });
          if (out.hoverFailures.length >= 10) break;
        }
      }
    }
    if (wantFocus) {
      const info = await sendRetry(client, "Runtime.evaluate", {
        expression: `JSON.stringify([...document.querySelectorAll(${JSON.stringify(FOCUS_SEL)})].map((el, i) => { const cs = getComputedStyle(el); const r = el.getBoundingClientRect(); return { i, on: cs.display !== "none" && cs.visibility !== "hidden" && r.width > 0 && r.height > 0, path: (${pathFn})(el) }; }).filter((o) => o.on).slice(0, ${FOCUS_SAMPLE_LIMIT}))`,
        returnByValue: true,
      });
      const candidates = JSON.parse(info.result.value);
      const q = await sendRetry(client, "DOM.querySelectorAll", { nodeId: rootId, selector: FOCUS_SEL });
      for (const c of candidates) {
        const nodeId = q.nodeIds[c.i];
        if (!nodeId) continue;
        await sendRetry(client, "CSS.forcePseudoState", { nodeId, forcedPseudoClasses: ["focus", "focus-visible"] });
        const r = await sendRetry(client, "Runtime.evaluate", {
          expression: stateFocusSnippet(c.i, FOCUS_SEL), returnByValue: true,
        });
        await sendRetry(client, "CSS.forcePseudoState", { nodeId, forcedPseudoClasses: [] });
        const v = r.exceptionDetails ? null : r.result.value;
        if (!v) continue;
        const outlineVisible = v.outlineStyle !== "none" && parseFloat(v.outlineWidth) > 0
          && v.outlineColor !== "transparent" && v.outlineColor !== "rgba(0, 0, 0, 0)";
        const shadowVisible = Boolean(v.boxShadow) && v.boxShadow !== "none";
        if (!outlineVisible && !shadowVisible) {
          out.focusFailures.push({ selector: c.path });
          if (out.focusFailures.length >= 10) break;
        }
      }
    }
  } catch (error) {
    out.error = String(error.message || error);
  } finally {
    try { await sendRetry(client, "CSS.disable"); } catch { /* session may be gone */ }
  }
  return out;
}

// F11 keyboard trap check (opt-in, --keyboard). Synthetic key events cannot
// move focus, so this dispatches real CDP key events: focus the first
// focusable, send Tab, and require focus to actually leave it. A page with
// two or more focusables whose focus survives three Tabs on the starting
// element is trapped (d39: keydown preventDefault holds focus forever).
async function collectKeyboardAudit(client) {
  const out = { trapped: false, visited: [], error: null };
  try {
    const setup = await sendRetry(client, "Runtime.evaluate", {
      expression: `JSON.stringify((() => {
        const els = [...document.querySelectorAll("a[href], button, input, select, textarea, [tabindex]")]
          .filter((el) => { const cs = getComputedStyle(el); const r = el.getBoundingClientRect(); return cs.display !== "none" && cs.visibility !== "hidden" && r.width > 0 && r.height > 0; });
        if (els[0]) els[0].focus();
        const a = document.activeElement;
        return { count: els.length, start: a ? a.tagName + "#" + (a.id || "") + "." + (a.className || "") : null };
      })())`,
      returnByValue: true,
    });
    const s = JSON.parse(setup.result.value);
    out.start = s.start;
    if (s.count < 2 || !s.start) return out; // single-focusable pages cannot trap between elements
    const seen = [];
    for (let i = 0; i < 3; i += 1) {
      await sendRetry(client, "Input.dispatchKeyEvent", { type: "rawKeyDown", key: "Tab", code: "Tab", windowsVirtualKeyCode: 9, nativeVirtualKeyCode: 9 });
      await sendRetry(client, "Input.dispatchKeyEvent", { type: "keyUp", key: "Tab", code: "Tab", windowsVirtualKeyCode: 9, nativeVirtualKeyCode: 9 });
      await sleep(50);
      const cur = await sendRetry(client, "Runtime.evaluate", {
        expression: `document.activeElement ? document.activeElement.tagName + "#" + (document.activeElement.id || "") + "." + (document.activeElement.className || "") : "none"`,
        returnByValue: true,
      });
      seen.push(cur.result.value);
    }
    out.visited = seen;
    out.trapped = seen.every((v) => v === s.start);
  } catch (error) {
    out.error = String(error.message || error);
  }
  return out;
}

async function collectRoute(client, route, args, screenshotPath, shotName, shotStem) {
  const problems = {
    consoleErrors: [],
    consoleWarnings: [],
    exceptions: [],
    failedRequests: [],
    blockedRequests: [],
    documentStatus: null,
    finalUrl: null,
    blank: false,
  };
  const inFlight = new Set();
  let docSeen = false;
  let docHeaders = {};
  let transferBytes = 0;

  let loadFired;
  const loadPromise = new Promise((r) => { loadFired = r; });
  const navigationUrl = new URL(route, args.url).href;

  const detach = client.onEvent((msg) => {
    switch (msg.method) {
      case "Page.loadEventFired":
        loadFired();
        break;
      case "Page.frameNavigated":
        if (msg.params.frame.parentId === undefined) { inFlight.clear(); docSeen = false; }
        break;
      case "Network.requestWillBeSent":
        inFlight.add(msg.params.requestId);
        break;
      case "Network.loadingFinished":
      case "Network.loadingFailed":
        inFlight.delete(msg.params.requestId);
        if (msg.method === "Network.loadingFinished") {
          transferBytes += msg.params.encodedDataLength || 0;
        }
        if (msg.method === "Network.loadingFailed" && !msg.params.canceled) {
          // ORB is a Chrome policy block of a cross-origin read, not a
          // server or transport failure; classify it separately.
          if (msg.params.errorText && msg.params.errorText.includes("ERR_BLOCKED_BY_ORB")) {
            problems.blockedRequests.push(`${msg.params.requestId}: ${msg.params.errorText}`);
          } else {
            problems.failedRequests.push(`${msg.params.requestId}: ${msg.params.errorText}`);
          }
        }
        break;
      case "Runtime.consoleAPICalled": {
        const text = (msg.params.args || []).map((a) => a.value ?? a.description ?? "").join(" ");
        if (msg.params.type === "error") problems.consoleErrors.push(text);
        else if (msg.params.type === "warning") problems.consoleWarnings.push(text);
        break;
      }
      case "Runtime.exceptionThrown": {
        const d = msg.params.exceptionDetails;
        problems.exceptions.push(d.exception?.description || d.text || "unknown exception");
        break;
      }
      case "Network.responseReceived": {
        // The first Document response in the fresh post-navigation window
        // is this route's status, even when it redirected (its URL is
        // then the final one, not the requested one).
        if (msg.params.type === "Document" && !docSeen) {
          problems.documentStatus = msg.params.response.status;
          docHeaders = msg.params.response.headers || {};
          docSeen = true;
        }
        break;
      }
      default:
        break;
    }
  });

  try {
    await sendRetry(client, "Page.enable");
    await sendRetry(client, "Runtime.enable");
    await sendRetry(client, "Network.enable");

    const navTimeout = setTimeout(loadFired, args.timeout);
    try {
      await sendRetry(client, "Page.navigate", { url: navigationUrl });
      await loadPromise;
    } finally {
      clearTimeout(navTimeout);
    }

    // Hydration runs after the load event: keep a fixed floor so async
    // exceptions surface, then exit early once the network is quiet.
    const floorEnd = Date.now() + args.floor;
    await waitForNetworkQuiet(client, inFlight, args, floorEnd);
    if (Date.now() < floorEnd) await sleep(floorEnd - Date.now());
    await waitForNetworkQuiet(client, inFlight, args, Date.now() + args.settle);

    const shotParams = { format: "png" };
    if (args.fullpage) {
      const metrics = await sendRetry(client, "Page.getLayoutMetrics");
      const size = metrics.cssContentSize || metrics.contentSize;
      if (size) {
        shotParams.clip = {
          x: 0, y: 0, width: Math.min(size.width, args.width * 2),
          height: Math.min(size.height, 8000), scale: 1,
        };
      }
    }
    const shot = await sendRetry(client, "Page.captureScreenshot", shotParams);
    writeFileSync(screenshotPath, Buffer.from(shot.data, "base64"));

    const summary = await sendRetry(client, "Runtime.evaluate", {
      expression: `JSON.stringify({
        title: document.title,
        h1: [...document.querySelectorAll("h1")].map((h) => h.textContent.trim()),
        buttons: document.querySelectorAll("button").length,
        links: document.querySelectorAll("a").length,
        inputs: document.querySelectorAll("input, select, textarea").length,
        text: document.body ? document.body.innerText.length : 0,
        finalUrl: location.href,
        lcp: (() => {
          // H-2: largest-contentful-paint entries never reach the
          // Performance Timeline via getEntriesByType in this Chrome — an
          // observer with buffered:true (LCP_RECORDER, injected on every
          // new document) is the only source. Without it this was always
          // null and --max-lcp-ms gated on nothing (d27 escaped that way).
          const rec = window.__qaLcp || [];
          if (rec.length) return rec[rec.length - 1];
          const es = performance.getEntriesByType("largest-contentful-paint");
          return es.length ? es[es.length - 1].startTime : null;
        })(),
        decodedBytes: (() => {
          // Decoded (post-compression) body bytes across all resources --
          // the perf-budget signal. Wire bytes alone undercount compressed
          // payloads (a 4.1MB BMP served gzip measures 0.9MB on the wire).
          const nav = performance.getEntriesByType("navigation")[0];
          return performance.getEntriesByType("resource")
            .reduce((n, e) => n + (e.decodedBodySize || 0), nav ? (nav.decodedBodySize || 0) : 0);
        })(),
        nextBuildId: (() => {
          const el = document.getElementById("__NEXT_DATA__");
          if (!el) return null;
          try { return JSON.parse(el.textContent).buildId ?? null; } catch { return null; }
        })(),
        flightBuildId: (() => {
          // App router carries the buildId in the inline flight payload as
          // "b":"<id>", JSON-escaped inside the push string. (Template
          // literal: double the backslashes so the page sees \\? = optional
          // backslash, not \? = literal question mark.)
          const m = document.documentElement.outerHTML.match(/\\\\?"b\\\\?":\\\\?"([A-Za-z0-9_-]{8,})\\\\?"/);
          return m ? m[1] : null;
        })(),
        scripts: [...document.querySelectorAll("script[src]")].map((s) => s.src),
      })`,
      returnByValue: true,
    });

    const page = JSON.parse(summary.result.value);
    problems.finalUrl = page.finalUrl;
    if (page.text === 0 && page.links + page.buttons + page.inputs === 0) {
      problems.blank = true;
    }

    // Visibility audit: hard fail only on contrast ratio <= 1.05 (equal
    // colors). Warnings and geometry notes are for the judge, not the gate.
    let visibility = null;
    if (args.visibility) {
      try {
        const audit = await sendRetry(client, "Runtime.evaluate", {
          expression: VISIBILITY_EVAL, returnByValue: true,
        });
        if (audit.exceptionDetails) {
          visibility = { error: audit.exceptionDetails.exception?.description || "audit failed" };
        } else {
          visibility = audit.result.value;
        }
      } catch (error) {
        visibility = { error: String(error.message || error) };
      }
    }

    // F7 a11y-lite audit (opt-in): hard fails feed the ok gate.
    let a11y = null;
    if (args.a11y) {
      try {
        const audit = await sendRetry(client, "Runtime.evaluate", {
          expression: A11Y_EVAL, returnByValue: true,
        });
        a11y = audit.exceptionDetails
          ? { error: audit.exceptionDetails.exception?.description || "a11y audit failed" }
          : audit.result.value;
      } catch (error) {
        a11y = { error: String(error.message || error) };
      }
    }

    // F9 DOM-hygiene audit (opt-in): hard fails feed the ok gate.
    let dom = null;
    if (args.domAudit) {
      try {
        const audit = await sendRetry(client, "Runtime.evaluate", {
          expression: DOM_AUDIT_EVAL, returnByValue: true, awaitPromise: true,
        });
        dom = audit.exceptionDetails
          ? { error: audit.exceptionDetails.exception?.description || "dom audit failed" }
          : audit.result.value;
      } catch (error) {
        dom = { error: String(error.message || error) };
      }
    }

    // F13 deep-a11y audit (opt-in): the host supplies an axe-core UMD bundle
    // (--axe path; validated in main, fail-closed there). The bundle is
    // evaluated into the page after settle, then axe.run collects violations.
    // impact critical|serious hard-fails the route; moderate|minor warns.
    let axe = null;
    if (args.axeSource) {
      axe = { violations: 0, fails: 0, warns: 0, rules: [] };
      try {
        const inject = await sendRetry(client, "Runtime.evaluate", {
          expression: args.axeSource, returnByValue: true,
        });
        if (inject.exceptionDetails) {
          axe.error = inject.exceptionDetails.exception?.description || "axe bundle failed to evaluate";
        } else {
          const run = await sendRetry(client, "Runtime.evaluate", {
            expression: `(() => { const fn = (window.axe && typeof window.axe.run === "function") ? window.axe.run.bind(window.axe) : null; if (!fn) return Promise.resolve(JSON.stringify({ __noAxe: true })); return fn(document, { resultTypes: ["violations"] }).then((r) => JSON.stringify({ violations: (r.violations || []).map((v) => ({ ruleId: v.id, impact: v.impact || "", help: v.help || "", nodes: (v.nodes || []).length })) })); })()`,
            awaitPromise: true, returnByValue: true,
          });
          if (run.exceptionDetails) {
            axe.error = run.exceptionDetails.exception?.description || "axe.run failed";
          } else {
            const parsed = JSON.parse(run.result.value || "{}");
            if (parsed.__noAxe) {
              axe.error = "axe bundle did not define window.axe.run";
            } else {
              const violations = parsed.violations || [];
              const rules = violations.map((v) => ({ ruleId: v.ruleId, impact: v.impact, help: v.help, nodes: v.nodes }));
              axe.rules = rules;
              axe.violations = violations.length;
              axe.fails = rules.filter((r) => r.impact === "critical" || r.impact === "serious").length;
              axe.warns = rules.filter((r) => r.impact === "moderate" || r.impact === "minor").length;
            }
          }
        }
      } catch (error) {
        axe.error = String(error.message || error);
      }
    }

    // F3 golden screenshot diff (primary viewport). Variants get their own
    // per-variant diff inside collectVariant when --golden is set.
    let golden = null;
    if (args.golden) {
      const goldenPath = join(args.golden, shotName);
      const stylesPath = join(args.golden, shotName.replace(/\.png$/, ".styles.json"));
      let styleSnapshot = null;
      try {
        const res = await sendRetry(client, "Runtime.evaluate", {
          expression: STYLE_EVAL, returnByValue: true,
        });
        styleSnapshot = res.exceptionDetails ? null : res.result.value;
      } catch {
        styleSnapshot = null;
      }
      if (args.goldenUpdate) {
        mkdirSync(args.golden, { recursive: true });
        writeFileSync(goldenPath, Buffer.from(shot.data, "base64"));
        if (styleSnapshot) writeFileSync(stylesPath, JSON.stringify(styleSnapshot, null, 2));
        golden = { updated: true };
      } else {
        if (existsSync(goldenPath)) {
          try {
            golden = { ...pngDiff(decodePng(Buffer.from(shot.data, "base64")), decodePng(readFileSync(goldenPath))) };
            golden.failed = golden.pct > GOLDEN_THRESHOLD_PCT;
          } catch (error) {
            golden = { error: String(error.message || error) };
          }
        } else {
          golden = { missing: true };
        }
        if (styleSnapshot && existsSync(stylesPath)) {
          try {
            const baseline = JSON.parse(readFileSync(stylesPath, "utf8"));
            const styleChanges = diffStyles(styleSnapshot, baseline);
            golden.styleChanges = styleChanges;
            golden.styleFailed = styleChanges.length > 0;
          } catch (error) {
            golden.styleError = String(error.message || error);
          }
        }
      }
    }

    // F10/F11 state-sampling and keyboard audits (opt-in). Run after the
    // golden capture: forced pseudo-states must not pollute style snapshots.
    let states = null;
    if (args.states.length > 0) {
      states = await collectStateAudit(client, args);
    }
    let keyboard = null;
    if (args.keyboard) {
      keyboard = await collectKeyboardAudit(client);
    }

    // F1/F2/F13 variant passes: extra viewports, color-scheme emulations, and
    // device-matrix entries. Each gets its own screenshot, gate signals, and
    // (with --golden) its own golden diff against per-variant baselines.
    const variants = [];
    for (const variant of args.variants) {
      const suffix = variant.kind === "viewport" ? `v${variant.label}`
        : variant.kind === "device" ? `d${variant.label}`
          : `s${variant.label}`;
      const variantShot = `${shotStem}__${safeName(suffix)}.png`;
      try {
        const v = await collectVariant(client, route, args, variant, join(dirname(screenshotPath), variantShot), args.golden ? join(args.golden, variantShot) : null);
        variants.push({ ...variant, screenshot: variantShot, ...v });
      } catch (error) {
        variants.push({ ...variant, screenshot: null, ok: false, error: String(error.message || error) });
      }
    }

    return { problems, page, docHeaders, visibility, a11y, dom, axe, golden, states, keyboard, variants, transferBytes };
  } finally {
    detach();
  }
}

// F1/F2 variant pass: re-load one route under an emulated viewport or color
// scheme and audit it there. Lighter probes than the primary pass (no build
// fingerprint) but the same hard gates: console errors, exceptions, failed
// requests, blank, and contrast failures. Viewports add overflow /
// hidden-controls / text-overlap / touch-target checks that only make sense
// at that width. With --golden, the variant screenshot and its computed-style
// twin also diff against per-variant baselines (d33: type drift inside a
// media query is invisible to a primary-viewport golden).
const SCHEME_FEATURES = {
  dark: [{ name: "prefers-color-scheme", value: "dark" }],
  light: [{ name: "prefers-color-scheme", value: "light" }],
  forced: [{ name: "forced-colors", value: "active" }],
};

async function collectVariant(client, route, args, variant, screenshotPath, goldenPath) {
  const problems = {
    consoleErrors: [], consoleWarnings: [], exceptions: [],
    failedRequests: [], blockedRequests: [],
    documentStatus: null, finalUrl: null, blank: false,
  };
  const inFlight = new Set();
  const navigationUrl = new URL(route, args.url).href;
  let loadFired;
  const loadPromise = new Promise((r) => { loadFired = r; });
  let docSeen = false;

  const detach = client.onEvent((msg) => {
    switch (msg.method) {
      case "Page.loadEventFired":
        loadFired();
        break;
      case "Page.frameNavigated":
        if (msg.params.frame.parentId === undefined) { inFlight.clear(); docSeen = false; }
        break;
      case "Network.requestWillBeSent":
        inFlight.add(msg.params.requestId);
        break;
      case "Network.loadingFinished":
      case "Network.loadingFailed":
        inFlight.delete(msg.params.requestId);
        if (msg.method === "Network.loadingFailed" && !msg.params.canceled
          && !(msg.params.errorText || "").includes("ERR_BLOCKED_BY_ORB")) {
          problems.failedRequests.push(`${msg.params.requestId}: ${msg.params.errorText}`);
        }
        break;
      case "Runtime.consoleAPICalled": {
        const text = (msg.params.args || []).map((a) => a.value ?? a.description ?? "").join(" ");
        if (msg.params.type === "error") problems.consoleErrors.push(text);
        else if (msg.params.type === "warning") problems.consoleWarnings.push(text);
        break;
      }
      case "Runtime.exceptionThrown": {
        const d = msg.params.exceptionDetails;
        problems.exceptions.push(d.exception?.description || d.text || "unknown exception");
        break;
      }
      case "Network.responseReceived":
        if (msg.params.type === "Document" && !docSeen) {
          problems.documentStatus = msg.params.response.status;
          docSeen = true;
        }
        break;
      default:
        break;
    }
  });

  try {
    await sendRetry(client, "Page.enable");
    await sendRetry(client, "Runtime.enable");
    await sendRetry(client, "Network.enable");

    if (variant.kind === "viewport" || variant.kind === "device") {
      // mobile:true makes Chrome run the meta-viewport heuristic: without
      // width=device-width it lays out at a ~980px default and scales the
      // result, so media queries never see 375px and overflow audits lie.
      // Layout audits want CSS width == viewport width: mobile:false. A
      // device entry opts into the mobile heuristic (a phone profile
      // expects it) and may add a scheme emulation and a UA override.
      await sendRetry(client, "Emulation.setDeviceMetricsOverride", {
        width: variant.width, height: variant.height, deviceScaleFactor: variant.scale || 1,
        mobile: variant.kind === "device",
      });
      if (variant.kind === "device" && variant.ua) {
        await sendRetry(client, "Emulation.setUserAgentOverride", { userAgent: variant.ua });
      }
      if (variant.features) {
        await sendRetry(client, "Emulation.setEmulatedMedia", {
          media: "screen", features: variant.features,
        });
      }
    } else {
      await sendRetry(client, "Emulation.setEmulatedMedia", {
        media: "screen", features: variant.features,
      });
    }

    const navTimeout = setTimeout(loadFired, args.timeout);
    try {
      await sendRetry(client, "Page.navigate", { url: navigationUrl });
      await loadPromise;
    } finally {
      clearTimeout(navTimeout);
    }

    const floorEnd = Date.now() + args.floor;
    await waitForNetworkQuiet(client, inFlight, args, floorEnd);
    if (Date.now() < floorEnd) await sleep(floorEnd - Date.now());
    await waitForNetworkQuiet(client, inFlight, args, Date.now() + args.settle);

    const shot = await sendRetry(client, "Page.captureScreenshot", { format: "png" });
    writeFileSync(screenshotPath, Buffer.from(shot.data, "base64"));

    const summary = await sendRetry(client, "Runtime.evaluate", {
      expression: `JSON.stringify({
        text: document.body ? document.body.innerText.length : 0,
        links: document.querySelectorAll("a").length,
        buttons: document.querySelectorAll("button").length,
        inputs: document.querySelectorAll("input, select, textarea").length,
        finalUrl: location.href,
      })`,
      returnByValue: true,
    });
    const page = JSON.parse(summary.result.value);
    problems.finalUrl = page.finalUrl;
    if (page.text === 0 && page.links + page.buttons + page.inputs === 0) {
      problems.blank = true;
    }

    let visibility = null;
    if (args.visibility) {
      const audit = await sendRetry(client, "Runtime.evaluate", {
        expression: VISIBILITY_EVAL, returnByValue: true,
      });
      visibility = audit.exceptionDetails
        ? { error: audit.exceptionDetails.exception?.description || "audit failed" }
        : audit.result.value;
    }

    let checks = null;
    if (variant.kind === "viewport") {
      const res = await sendRetry(client, "Runtime.evaluate", {
        expression: VARIANT_EVAL, returnByValue: true,
      });
      checks = res.exceptionDetails
        ? { error: res.exceptionDetails.exception?.description || "variant audit failed" }
        : res.result.value;
    }

    // Per-variant golden twin (d33): the variant screenshot is already
    // captured; diff it and its computed styles against per-variant
    // baselines. Missing baselines pass for viewport/scheme/motion variants
    // (same policy as the primary golden) but FAIL for device-matrix
    // entries: a device the host named in --devices must have a captured
    // baseline, otherwise the matrix is silently untested.
    let golden = null;
    if (goldenPath) {
      const stylesPath = goldenPath.replace(/\.png$/, ".styles.json");
      let styleSnapshot = null;
      try {
        const res = await sendRetry(client, "Runtime.evaluate", {
          expression: STYLE_EVAL, returnByValue: true,
        });
        styleSnapshot = res.exceptionDetails ? null : res.result.value;
      } catch {
        styleSnapshot = null;
      }
      if (args.goldenUpdate) {
        mkdirSync(dirname(goldenPath), { recursive: true });
        writeFileSync(goldenPath, Buffer.from(shot.data, "base64"));
        if (styleSnapshot) writeFileSync(stylesPath, JSON.stringify(styleSnapshot, null, 2));
        golden = { updated: true };
      } else {
        if (existsSync(goldenPath)) {
          try {
            golden = { ...pngDiff(decodePng(Buffer.from(shot.data, "base64")), decodePng(readFileSync(goldenPath))) };
            golden.failed = golden.pct > GOLDEN_THRESHOLD_PCT;
          } catch (error) {
            golden = { error: String(error.message || error) };
          }
        } else {
          golden = { missing: true };
          if (variant.kind === "device") golden.failed = true;
        }
        if (styleSnapshot && existsSync(stylesPath)) {
          try {
            const baseline = JSON.parse(readFileSync(stylesPath, "utf8"));
            const styleChanges = diffStyles(styleSnapshot, baseline);
            golden.styleChanges = styleChanges;
            golden.styleFailed = styleChanges.length > 0;
          } catch (error) {
            golden.styleError = String(error.message || error);
          }
        }
      }
    }

    const contrastFails = visibility?.contrastFailures?.length || 0;
    const failedChecks = [];
    if (checks?.overflow) failedChecks.push(`horizontal overflow: scrollWidth ${checks.overflow.scrollWidth} > innerWidth ${checks.overflow.innerWidth}`);
    if (checks?.hiddenControls?.length) failedChecks.push(`all form controls hidden at this width: ${checks.hiddenControls.join(", ")}`);
    if (checks?.textOverlap?.length) failedChecks.push(`text elements overlap >70%: ${checks.textOverlap.map((o) => `${o.a} / ${o.b}`).join("; ")}`);
    if (checks?.touchTargets?.length) failedChecks.push(`touch target < 24px: ${checks.touchTargets.join(", ")}`);
    if (variant.kind === "motion") {
      const res = await sendRetry(client, "Runtime.evaluate", {
        expression: `document.getAnimations().filter((a) => a.playState === "running").length`,
        returnByValue: true,
      });
      const running = res.exceptionDetails ? 0 : (res.result.value || 0);
      if (running > 0) failedChecks.push(`${running} animation(s) still running under prefers-reduced-motion: reduce`);
    }
    const ok = problems.consoleErrors.length === 0
      && problems.exceptions.length === 0
      && problems.failedRequests.length === 0
      && !problems.blank
      && contrastFails === 0
      && failedChecks.length === 0
      && !(golden?.failed === true || golden?.styleFailed === true)
      && (problems.documentStatus === null
        || (problems.documentStatus >= 200 && problems.documentStatus < 400));

    return { problems, page, visibility, checks, golden, failedChecks, ok };
  } finally {
    detach();
  }
}

// Build identity of the served app, strongest signal first:
//   1. Next buildId — __NEXT_DATA__.buildId (pages router) or the flight
//      payload field "b" (app router, matched escaped or plain) -> "next:<id>"
//   2. dev server (buildId "development") or non-Next -> sha1 of first-party
//      script URLs + document ETag/Last-Modified, prefixed "dev:"/"static:"
// Per-route ids go in smoke.json; the report's Build: line uses a union
// across routes, because page-specific chunks legitimately differ per route.
// Two distinct buildIds across routes mean a stale or partial serve.
function computeFingerprint(page, docHeaders) {
  const bid = page.nextBuildId || page.flightBuildId;
  if (bid && bid !== "development") {
    return { id: `next:${bid}`, kind: "buildId", input: null };
  }
  const origin = (() => { try { return new URL(page.finalUrl).origin; } catch { return null; } })();
  const scripts = (page.scripts || []).filter((s) => {
    try { return origin === null || new URL(s).origin === origin; } catch { return false; }
  });
  const input = `${scripts.slice().sort().join("\n")}|${docHeaders.etag || ""}|${docHeaders["last-modified"] || ""}|`;
  const hash = createHash("sha1").update(input).digest("hex").slice(0, 16);
  const kind = bid === "development" ? "dev" : "static";
  return { id: `${kind}:${hash}`, kind: `${kind}-scripts+headers`, input };
}

// Visibility audit, run in the page after settle (one round trip, $0).
// Returns contrast failures (ratio <= 1.05, hard fail), contrast warnings
// (< 3.0), geometry warnings (zero-size / offscreen / overlap / img decode,
// warn-only), skip counters, and exact-label disagreement groups
// (informational — never gates ok). Written with no lone backslashes and no
// ${} so the template literal transmits verbatim.
const VISIBILITY_EVAL = `(() => {
  const clamp = (n, lo, hi) => Math.min(hi, Math.max(lo, n));
  const SKIPPED = { textOverImage: 0, offscreenNodes: 0, hidden: 0 };
  const contrastFailures = [];
  const contrastWarnings = [];
  const geometry = { zeroSize: [], offscreen: [], overlap: [], imgDecode: [] };

  const SYSTEM = {
    canvas: "rgb(255, 255, 255)", canvastext: "rgb(0, 0, 0)",
    linktext: "rgb(0, 0, 238)", visitedtext: "rgb(85, 26, 139)",
    buttontext: "rgb(0, 0, 0)", fieldtext: "rgb(0, 0, 0)",
    highlight: "rgb(181, 213, 255)", highlighttext: "rgb(0, 0, 0)",
    graytext: "rgb(128, 128, 128)", windowtext: "rgb(0, 0, 0)",
    window: "rgb(255, 255, 255)",
  };

  // getComputedStyle resolves system colors to rgb() in Chrome; the map is
  // the fallback for engines that return the keyword.
  function parseRGB(css) {
    if (!css) return null;
    const s = String(css).trim().toLowerCase();
    if (SYSTEM[s]) return parseRGB(SYSTEM[s]);
    if (s === "transparent" || s === "rgba(0, 0, 0, 0)") return null;
    const parts = s.slice(s.indexOf("(") + 1, s.lastIndexOf(")")).split(",");
    if (parts.length < 3) return null;
    return [clamp(+parts[0] || 0, 0, 255), clamp(+parts[1] || 0, 0, 255), clamp(+parts[2] || 0, 0, 255)];
  }

  function luminance(rgb) {
    const chan = rgb.map((v) => {
      const c = v / 255;
      return c <= 0.03928 ? c / 12.92 : Math.pow((c + 0.055) / 1.055, 2.4);
    });
    return 0.2126 * chan[0] + 0.7152 * chan[1] + 0.0722 * chan[2];
  }

  function contrast(fg, bg) {
    const lf = luminance(fg);
    const lb = luminance(bg);
    return (Math.max(lf, lb) + 0.05) / (Math.min(lf, lb) + 0.05);
  }

  function cssPath(el) {
    const bits = [];
    let cur = el;
    for (let i = 0; cur && i < 4; i += 1) {
      let part = cur.tagName ? cur.tagName.toLowerCase() : "?";
      if (cur.id) { part = "#" + cur.id; bits.unshift(part); break; }
      const cls = (cur.getAttribute("class") || "").trim();
      if (cls) part += "." + cls.split(" ")[0];
      bits.unshift(part);
      cur = cur.parentElement;
    }
    return bits.join(" > ");
  }

  function isDisplayed(el) {
    const cs = getComputedStyle(el);
    return cs.display !== "none" && cs.visibility !== "hidden";
  }

  // Effective background: nearest ancestor with an opaque color; page
  // default treated as white Canvas (headless defaults to light scheme).
  function backgroundBehind(el) {
    let cur = el.parentElement;
    let overImage = false;
    while (cur && cur !== document.documentElement) {
      const cs = getComputedStyle(cur);
      if (cs.backgroundImage && cs.backgroundImage !== "none") overImage = true;
      const bg = parseRGB(cs.backgroundColor);
      if (bg) return { bg, overImage };
      cur = cur.parentElement;
    }
    return { bg: [255, 255, 255], overImage };
  }

  const VW = window.innerWidth;
  const VH = window.innerHeight;
  const inViewport = (r) => r.bottom > 0 && r.right > 0 && r.top < VH && r.left < VW;

  // --- contrast audit over visible viewport text ---
  const range = document.createRange();
  const walker = document.createTreeWalker(document.body, NodeFilter.SHOW_TEXT);
  let node;
  while ((node = walker.nextNode())) {
    const text = node.textContent.trim();
    if (!text) continue;
    const el = node.parentElement;
    if (!el || !isDisplayed(el)) { SKIPPED.hidden += 1; continue; }
    range.selectNodeContents(node);
    const rect = range.getBoundingClientRect();
    if (rect.width === 0 || rect.height === 0) { SKIPPED.hidden += 1; continue; }
    if (!inViewport(rect)) {
      // sr-only / clip techniques: tiny boxes, not defects
      if (rect.width <= 1 || rect.height <= 1) SKIPPED.hidden += 1;
      else SKIPPED.offscreenNodes += 1;
      continue;
    }
    const behind = backgroundBehind(el);
    if (behind.overImage) { SKIPPED.textOverImage += 1; continue; }
    const fg = parseRGB(getComputedStyle(el).color);
    if (!fg) { SKIPPED.hidden += 1; continue; }
    const ratio = contrast(fg, behind.bg);
    const entry = { selector: cssPath(el), text: text.slice(0, 60), ratio: +ratio.toFixed(2) };
    if (ratio <= 1.05) contrastFailures.push(entry);
    else if (ratio < 3.0) contrastWarnings.push(entry);
    if (contrastFailures.length >= 50 && contrastWarnings.length >= 50) break;
  }

  // --- geometry (warn-only) ---
  const CONTENT = "p, span, div, li, td, th, button, a, h1, h2, h3, h4, h5, h6, label, strong, em";
  for (const el of document.querySelectorAll(CONTENT)) {
    if (geometry.zeroSize.length >= 5) break;
    if (!el.childElementCount && el.textContent.trim() && isDisplayed(el)) {
      const r = el.getBoundingClientRect();
      if (r.width === 0 && r.height === 0) {
        geometry.zeroSize.push({ selector: cssPath(el), text: el.textContent.trim().slice(0, 40) });
      }
    }
  }
  for (const el of document.querySelectorAll(CONTENT)) {
    if (geometry.offscreen.length >= 5) break;
    if (!el.childElementCount && el.textContent.trim() && isDisplayed(el)) {
      const r = el.getBoundingClientRect();
      if (r.width > 1 && r.height > 1 && (r.right < 0 || r.bottom < 0 || r.left > VW || r.top > VH)) {
        geometry.offscreen.push({ selector: cssPath(el), text: el.textContent.trim().slice(0, 40) });
      }
    }
  }
  const interactive = [...document.querySelectorAll("button, a, input, select, textarea, [role=button]")]
    .filter((el) => isDisplayed(el))
    .map((el) => ({ el, r: el.getBoundingClientRect() }))
    .filter((o) => o.r.width > 0 && o.r.height > 0 && inViewport(o.r))
    .slice(0, 150);
  for (let i = 0; i < interactive.length && geometry.overlap.length < 5; i += 1) {
    for (let j = i + 1; j < interactive.length && geometry.overlap.length < 5; j += 1) {
      const a = interactive[i].r;
      const b = interactive[j].r;
      const iw = Math.min(a.right, b.right) - Math.max(a.left, b.left);
      const ih = Math.min(a.bottom, b.bottom) - Math.max(a.top, b.top);
      if (iw <= 0 || ih <= 0) continue;
      const inter = iw * ih;
      const smaller = Math.min(a.width * a.height, b.width * b.height);
      if (smaller > 0 && inter / smaller > 0.8) {
        geometry.overlap.push({
          a: cssPath(interactive[i].el), b: cssPath(interactive[j].el),
        });
      }
    }
  }
  for (const img of document.querySelectorAll("img")) {
    if (geometry.imgDecode.length >= 5) break;
    if (!isDisplayed(img)) continue;
    const r = img.getBoundingClientRect();
    if (!inViewport(r)) continue;
    const src = img.currentSrc || img.src || "";
    // dimensionless SVG renders at CSS size with naturalWidth 0 — not a defect
    if (img.complete && img.naturalWidth === 0 && src.indexOf(".svg") === -1) {
      geometry.imgDecode.push({ selector: cssPath(img), src: src.slice(0, 80) });
    }
  }

  // --- displayed-number disagreement (informational only) ---
  const labels = new Map();
  for (const el of document.querySelectorAll("span, div, p, td, th, li, h1, h2, h3, label, strong")) {
    if (labels.size >= 200) break;
    if (el.childElementCount !== 0) continue; // leaf elements only: a wrapper and its inner p would duplicate
    const t = el.textContent.trim();
    if (!t || t.length > 40 || t.indexOf(":") === -1) continue;
    const label = t.slice(0, t.indexOf(":")).trim().toLowerCase();
    const value = t.slice(t.indexOf(":") + 1).trim();
    if (label.length < 2 || label.length > 20 || !value || value.length > 15) continue;
    const r = el.getBoundingClientRect();
    if (!isDisplayed(el) || r.width === 0 || r.height === 0 || !inViewport(r)) continue;
    if (!labels.has(label)) labels.set(label, []);
    labels.get(label).push({ selector: cssPath(el), value });
  }
  const crossChecks = [];
  for (const [label, spots] of labels) {
    const values = new Set(spots.map((s) => s.value));
    if (spots.length >= 2 && values.size >= 2) crossChecks.push({ label, spots });
  }

  return {
    contrastFailures, contrastWarnings, geometry, skipped: SKIPPED, crossChecks,
  };
})()`;

const SKIP_EXTENSIONS = /\.(png|jpe?g|gif|webp|svg|ico|css|js|mjs|map|json|xml|txt|pdf|zip|gz|mp[34]|webm|woff2?|ttf|eot)(\?|#|$)/i;

// F7 a11y-lite audit, run in the page after settle (one round trip, $0).
// H-2: largest-contentful-paint entries never reach the Performance
// Timeline via getEntriesByType in this Chrome — a buffered observer is
// the only source. Injected on every new document so the summary eval can
// read window.__qaLcp. Without this, --max-lcp-ms gated on null forever.
const LCP_RECORDER = `window.__qaLcp = [];
try {
  new PerformanceObserver((list) => {
    for (const e of list.getEntries()) window.__qaLcp.push(e.startTime);
  }).observe({ type: "largest-contentful-paint", buffered: true });
} catch {}`;

// F9 DOM-hygiene audit (opt-in): three hard-fail checks in one eval.
// duplicateIds: id values used by more than one element.
// brokenLinks: same-origin a[href] whose GET returns >= 400 (fetched
// after settle, so the probes do not disturb the network-quiet window).
// clippedText: overflow:hidden on an axis with real text spilling past
// the clip on that axis — silent truncation, not document overflow.
const DOM_AUDIT_EVAL = `(async () => {
  const path = (el) => {
    const parts = [];
    let cur = el;
    for (let d = 0; cur && cur.nodeType === 1 && d < 3; d += 1) {
      const tag = cur.tagName.toLowerCase();
      const par = cur.parentElement;
      if (par) {
        const sibs = [...par.children].filter((c) => c.tagName === cur.tagName);
        parts.unshift(sibs.length > 1 ? tag + ":nth-of-type(" + (sibs.indexOf(cur) + 1) + ")" : tag);
      } else parts.unshift(tag);
      cur = par;
    }
    return parts.join(" > ");
  };
  const duplicateIds = (() => {
    const seen = new Map();
    for (const el of document.querySelectorAll("[id]")) {
      const id = el.getAttribute("id");
      seen.set(id, (seen.get(id) || 0) + 1);
    }
    return [...seen.entries()].filter(([, n]) => n > 1).map(([id, n]) => id + " x" + n);
  })();
  const brokenLinks = [];
  const paths = new Set();
  for (const a of document.querySelectorAll("a[href]")) {
    let u;
    try { u = new URL(a.getAttribute("href"), location.href); } catch { continue; }
    if (u.origin !== location.origin) continue;
    if (u.hash && u.pathname === location.pathname) continue;
    paths.add(u.pathname + u.search);
    if (paths.size >= 25) break;
  }
  for (const p of paths) {
    try {
      const ctl = new AbortController();
      const t = setTimeout(() => ctl.abort(), 5000);
      const res = await fetch(p, { method: "GET", cache: "no-store", redirect: "follow", signal: ctl.signal });
      clearTimeout(t);
      if (res.status >= 400) brokenLinks.push(p + " -> " + res.status);
    } catch (e) {
      brokenLinks.push(p + " -> " + (e && e.name === "AbortError" ? "timeout" : "fetch failed"));
    }
  }
  const clippedText = [];
  for (const el of document.querySelectorAll("body *")) {
    if (clippedText.length >= 20) break;
    if (!el.innerText || !el.innerText.trim()) continue;
    if (!(el.offsetWidth > 0 && el.offsetHeight > 0)) continue;
    const cs = getComputedStyle(el);
    const clipX = cs.overflowX === "hidden" && el.scrollWidth > el.clientWidth + 1;
    const clipY = cs.overflowY === "hidden" && el.scrollHeight > el.clientHeight + 1;
    if (clipX || clipY) clippedText.push(path(el) + (clipX ? " [x]" : " [y]"));
  }
  return { duplicateIds, brokenLinks, clippedText };
})()`;

// Three hard-fail checks: form controls without a real label (placeholder
// does not count — WCAG advisory, and the d15 plant relies on it), img
// without an alt attribute, heading-level skips in document order.
const A11Y_EVAL = `(() => {
  const failures = [];
  const cssPath = (el) => {
    const bits = [];
    let cur = el;
    for (let i = 0; cur && i < 4; i += 1) {
      let part = cur.tagName ? cur.tagName.toLowerCase() : "?";
      if (cur.id) { part = "#" + cur.id; bits.unshift(part); break; }
      const cls = (cur.getAttribute("class") || "").trim();
      if (cls) part += "." + cls.split(" ")[0];
      bits.unshift(part);
      cur = cur.parentElement;
    }
    return bits.join(" > ");
  };
  const controls = [...document.querySelectorAll("input, select, textarea")]
    .filter((el) => (el.getAttribute("type") || "").toLowerCase() !== "hidden");
  for (const el of controls) {
    if (el.hasAttribute("aria-label") || el.hasAttribute("aria-labelledby")) continue;
    if (el.closest("label")) continue;
    const id = el.getAttribute("id");
    if (id && document.querySelector("label[for=\\"" + CSS.escape(id) + "\\"]")) continue;
    failures.push({ kind: "unlabeledControl", selector: cssPath(el), type: el.tagName.toLowerCase() });
  }
  for (const img of document.querySelectorAll("img")) {
    if (!img.hasAttribute("alt")) {
      failures.push({ kind: "imgNoAlt", selector: cssPath(img), src: (img.currentSrc || img.src || "").slice(0, 80) });
    }
  }
  let last = 0;
  for (const h of document.querySelectorAll("h1, h2, h3, h4, h5, h6")) {
    const level = +h.tagName.slice(1);
    if (last && level > last + 1) {
      failures.push({ kind: "headingSkip", selector: cssPath(h), from: "h" + last, to: "h" + level });
    }
    last = level;
  }
  // d29/d30: document-level a11y-lite gaps a control-label audit never sees.
  const lang = document.documentElement.getAttribute("lang");
  if (!lang || !lang.trim()) {
    failures.push({ kind: "noLang", selector: "html" });
  }
  if (!document.title || !document.title.trim()) {
    failures.push({ kind: "emptyTitle", selector: "title" });
  }
  // d32: a viewport meta that disables scaling fails WCAG 1.4.4 resize-text.
  const vp = document.querySelector('meta[name="viewport"]');
  if (vp) {
    const content = (vp.getAttribute("content") || "").toLowerCase();
    const noScale = /user-scalable\\s*=\\s*no/.test(content);
    const m = content.match(/maximum-scale\\s*=\\s*([0-9.]+)/);
    if (noScale || (m && parseFloat(m[1]) < 2)) {
      failures.push({ kind: "zoomLock", selector: 'meta[name="viewport"]', content: content.slice(0, 80) });
    }
  }
  // d36: interactive elements with no accessible name (icon-only links and
  // buttons are invisible to assistive tech). Name sources: aria-label,
  // aria-labelledby, rendered text, alt text of images inside.
  for (const el of document.querySelectorAll("a[href], button, [role=button]")) {
    const labelledby = el.getAttribute("aria-labelledby");
    const ref = labelledby ? document.getElementById(labelledby) : null;
    const name = (el.getAttribute("aria-label") || "").trim()
      || (ref ? ref.textContent.trim() : "")
      || el.textContent.trim()
      || [...el.querySelectorAll("img[alt]")].map((i) => i.getAttribute("alt").trim()).join(" ");
    if (!name) {
      failures.push({ kind: "noName", selector: cssPath(el) });
    }
  }
  // d37: positive tabindex hijacks the document keyboard order (WCAG 2.4.3).
  for (const el of document.querySelectorAll("[tabindex]")) {
    const t = parseInt(el.getAttribute("tabindex"), 10);
    if (t > 0) {
      failures.push({ kind: "positiveTabindex", selector: cssPath(el), tabindex: t });
    }
  }
  return { failures: failures.slice(0, 20) };
})()`;

// F3 style snapshot: the computed-style twin of the pixel golden. Pixel
// diff catches layout regressions; computed-style diff catches cosmetic
// ones that are invisible at full-page pixel scale (d14 border-radius
// 8px->0 is 0.008% of pixels — no threshold reaches it). One eval, JSON
// diff, zero dependencies.
const STYLE_EVAL = `(() => {
  const props = ["color", "backgroundColor", "fontFamily", "fontSize", "fontWeight", "letterSpacing", "padding", "margin", "borderRadius", "borderColor", "lineHeight"];
  const out = {};
  const cssPath = (el) => {
    const bits = [];
    let cur = el;
    for (let i = 0; cur && i < 3; i += 1) {
      let part = cur.tagName ? cur.tagName.toLowerCase() : "?";
      const cls = (cur.getAttribute("class") || "").trim();
      if (cls) part += "." + cls.split(" ")[0];
      bits.unshift(part);
      cur = cur.parentElement;
    }
    return bits.join(" > ");
  };
  const els = [...document.querySelectorAll("body *")].slice(0, 400);
  for (const el of els) {
    const key = cssPath(el);
    if (out[key]) continue; // first occurrence only: deterministic on same DOM
    const cs = getComputedStyle(el);
    const rec = {};
    for (const p of props) rec[p] = cs[p];
    out[key] = rec;
  }
  return out;
})()`;

// Diff two style snapshots -> [{selector, prop, from, to}]. Missing keys or
// props count as changes (an element that vanished changed the page).
function diffStyles(current, golden) {
  const changes = [];
  const selectors = new Set([...Object.keys(current || {}), ...Object.keys(golden || {})]);
  for (const sel of selectors) {
    const a = current?.[sel] || {};
    const b = golden?.[sel] || {};
    for (const prop of new Set([...Object.keys(a), ...Object.keys(b)])) {
      if (a[prop] !== b[prop]) {
        changes.push({ selector: sel, prop, from: b[prop] ?? "(missing)", to: a[prop] ?? "(missing)" });
      }
    }
    if (changes.length >= 50) break;
  }
  return changes;
}

// F1 viewport-variant audit: horizontal overflow, form controls that all
// disappear at this width, and text elements overlapping >70% of the smaller
// box (parent/child pairs and identical text excluded). Hard-fail material
// for viewport variants, not the primary pass.
const VARIANT_EVAL = `(() => {
  const out = { overflow: null, hiddenControls: [], textOverlap: [] };
  const de = document.documentElement;
  if (de.scrollWidth > window.innerWidth + 1) {
    out.overflow = { scrollWidth: de.scrollWidth, innerWidth: window.innerWidth };
  }
  const controls = [...document.querySelectorAll("input, select, textarea")]
    .filter((el) => (el.getAttribute("type") || "").toLowerCase() !== "hidden");
  const displayed = controls.filter((el) => {
    const cs = getComputedStyle(el);
    if (cs.display === "none" || cs.visibility === "hidden") return false;
    const r = el.getBoundingClientRect();
    return r.width > 0 && r.height > 0;
  });
  if (controls.length > 0 && displayed.length === 0) {
    out.hiddenControls = controls.slice(0, 5).map((el) => {
      const t = el.tagName.toLowerCase();
      const ty = el.getAttribute("type");
      return ty ? t + "[type=" + ty + "]" : t;
    });
  }
  const VW = window.innerWidth;
  const VH = window.innerHeight;
  const cssPath = (el) => {
    const bits = [];
    let cur = el;
    for (let i = 0; cur && i < 3; i += 1) {
      let part = cur.tagName ? cur.tagName.toLowerCase() : "?";
      const cls = (cur.getAttribute("class") || "").trim();
      if (cls) part += "." + cls.split(" ")[0];
      bits.unshift(part);
      cur = cur.parentElement;
    }
    return bits.join(" > ");
  };
  const els = [...document.querySelectorAll("p, span, div, li, td, th, button, a, h1, h2, h3, h4, h5, h6, label, strong, em")]
    .filter((el) => !el.childElementCount && el.textContent.trim().length >= 3)
    .filter((el) => {
      const cs = getComputedStyle(el);
      return cs.display !== "none" && cs.visibility !== "hidden";
    })
    .map((el) => ({ el, r: el.getBoundingClientRect() }))
    .filter((o) => o.r.width > 2 && o.r.height > 2 && o.r.bottom > 0 && o.r.right > 0 && o.r.top < VH && o.r.left < VW)
    .slice(0, 120);
  for (let i = 0; i < els.length && out.textOverlap.length < 5; i += 1) {
    for (let j = i + 1; j < els.length && out.textOverlap.length < 5; j += 1) {
      const A = els[i]; const B = els[j];
      if (A.el.contains(B.el) || B.el.contains(A.el)) continue;
      if (A.el.textContent.trim() === B.el.textContent.trim()) continue;
      const iw = Math.min(A.r.right, B.r.right) - Math.max(A.r.left, B.r.left);
      const ih = Math.min(A.r.bottom, B.r.bottom) - Math.max(A.r.top, B.r.top);
      // Any real intersection of two leaf text boxes is suspicious -- text
      // is not normally painted over text. The old >0.7-of-smaller rule
      // missed shallow collisions (a badge clipping a button top: ~33%).
      if (iw < 8 || ih < 4) continue;
      const inter = iw * ih;
      const smaller = Math.min(A.r.width * A.r.height, B.r.width * B.r.height);
      if (smaller > 0 && inter / smaller > 0.25) {
        out.textOverlap.push({ a: cssPath(A.el), b: cssPath(B.el) });
      }
    }
  }
  // d31: interactive boxes under 24px fail WCAG 2.5.8 target-size minimum.
  // Only meaningful at an emulated viewport: a desktop-primary golden must
  // not see a shrink that lives inside a max-width media query.
  out.touchTargets = [];
  const clickables = [...document.querySelectorAll("a[href], button, input, select, textarea, [role=button]")]
    .filter((el) => (el.getAttribute("type") || "").toLowerCase() !== "hidden");
  for (const el of clickables) {
    const cs = getComputedStyle(el);
    if (cs.display === "none" || cs.visibility === "hidden") continue;
    const r = el.getBoundingClientRect();
    if (r.width <= 0 || r.height <= 0) continue; // scrolled away is not a size defect
    if (r.width < 24 || r.height < 24) {
      out.touchTargets.push(cssPath(el) + " " + Math.round(r.width) + "x" + Math.round(r.height));
    }
    if (out.touchTargets.length >= 10) break;
  }
  return out;
})()`;


async function trySitemap(base, cap) {
  const baseUrl = new URL(base);
  const seen = new Set();
  const routes = [];
  const frontier = ["/sitemap.xml", "/sitemap-index.xml", "/sitemap_index.xml"];
  const visited = new Set();
  let subFetches = 0;
  while (frontier.length > 0 && routes.length < cap) {
    const path = frontier.shift();
    if (visited.has(path)) continue;
    visited.add(path);
    let text;
    try {
      const res = await fetch(new URL(path, baseUrl).href, { signal: AbortSignal.timeout(8000) });
      if (!res.ok) continue;
      text = await res.text();
    } catch {
      continue;
    }
    const isIndex = /<sitemapindex/i.test(text);
    if (isIndex && subFetches >= 3) continue; // indexes can list 100s of sub-sitemaps
    if (isIndex) subFetches += 1;
    const locs = [...text.matchAll(/<loc>\s*([^<]+?)\s*<\/loc>/gi)].map((m) => m[1]);
    for (const loc of locs) {
      let u;
      try { u = new URL(loc); } catch { continue; }
      if (u.origin !== baseUrl.origin) continue;
      const route = u.pathname + u.search;
      if (isIndex && route.includes("sitemap")) {
        frontier.push(route);
      } else if (!SKIP_EXTENSIONS.test(route) && !seen.has(route)) {
        seen.add(route);
        routes.push(route);
        if (routes.length >= cap) break;
      }
    }
  }
  return routes;
}

async function crawlRoutes(client, base, cap, depth) {
  const baseUrl = new URL(base);
  const seen = new Set(["/"]);
  const routes = [];
  let frontier = ["/"];
  for (let level = 0; level <= depth && frontier.length > 0; level += 1) {
    const next = [];
    for (const route of frontier) {
      if (routes.length >= cap) break;
      try {
        await sendRetry(client, "Page.navigate", { url: new URL(route, baseUrl).href });
        await sleep(1200);
        const evalRes = await sendRetry(client, "Runtime.evaluate", {
          expression: `[...document.querySelectorAll("a[href]")].map((a) => { const u = new URL(a.href, location.href); return u.origin === location.origin ? u.pathname + u.search : null; }).filter(Boolean)`,
          returnByValue: true,
        });
        routes.push(route);
        for (const found of evalRes.result.value) {
          if (seen.has(found) || SKIP_EXTENSIONS.test(found)) continue;
          seen.add(found);
          if (level < depth) next.push(found);
        }
      } catch {
        // skip unroutable pages during discovery
      }
    }
    frontier = next;
  }
  return routes;
}

async function discoverRoutes(port, args) {
  const fromSitemap = await trySitemap(args.url, args.discover || args.maxroutes);
  if (fromSitemap.length > 0) return fromSitemap;
  const target = await newPageTarget(port);
  const client = new CdpClient(target.webSocketDebuggerUrl);
  try {
    await sendRetry(client, "Page.enable");
    await sendRetry(client, "Runtime.enable");
    const routes = await crawlRoutes(client, args.url, args.discover || 50, 2);
    return routes;
  } finally {
    client.close();
  }
}

function buildMarkdown(report) {
  const lines = [];
  lines.push(`# Smoke QA report — ${report.baseUrl}`);
  lines.push("");
  lines.push(`Started: ${report.startedAt} · ${report.summary.ok}/${report.summary.routes} routes ok · ${report.summary.totalErrors} problems`);
  if (report.build && report.build.run) {
    lines.push(`Build: ${report.build.run}`);
  }
  lines.push("");
  lines.push("| # | route | status | ms | err | warn | exc | net | blk | blank | vis | links | buttons | inputs | text | shot |");
  lines.push("|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|");
  report.routes.forEach((r, i) => {
    if (r.error) {
      lines.push(`| ${i + 1} | ${r.route} | — | — | — | — | — | — | — | — | — | — | — | — | — | ${r.error} |`);
      return;
    }
    const redirect = r.finalUrl && new URL(r.finalUrl).pathname !== r.route ? ` → ${new URL(r.finalUrl).pathname}` : "";
    const v = r.visibility || {};
    const vis = v.error ? "ERR"
      : `${v.contrastFailures?.length || 0}F/${v.contrastWarnings?.length || 0}w`;
    lines.push(`| ${i + 1} | ${r.route}${redirect} | ${r.documentStatus ?? "?"} | ${r.ms} | ${r.consoleErrors.length} | ${r.consoleWarnings.length} | ${r.exceptions.length} | ${r.failedRequests.length} | ${r.blockedRequests.length} | ${r.blank ? "YES" : ""} | ${vis} | ${r.page.links} | ${r.page.buttons} | ${r.page.inputs} | ${r.page.text} | ${r.screenshot} |`);
  });
  const bad = report.routes.filter((r) => !r.ok);
  if (bad.length > 0) {
    lines.push("");
    lines.push("## Problems");
    for (const r of bad) {
      lines.push("");
      lines.push(`### ${r.route}`);
      if (r.error) { lines.push(`- run failed: ${r.error}`); continue; }
      for (const e of r.consoleErrors) lines.push(`- console error: ${e.split("\n")[0]}`);
      for (const e of r.exceptions) lines.push(`- exception: ${e.split("\n")[0]}`);
      for (const e of r.failedRequests) lines.push(`- failed request: ${e}`);
      if (r.blockedRequests.length > 0) lines.push(`- blocked by browser policy (ORB, informational): ${r.blockedRequests.length} request(s)`);
      if (r.blank) lines.push("- blank page: no text and no interactive elements");
      if (r.documentStatus !== null && (r.documentStatus < 200 || r.documentStatus >= 400)) {
        lines.push(`- document status ${r.documentStatus}`);
      }
      for (const c of (r.visibility?.contrastFailures || [])) {
        lines.push(`- contrast ${c.ratio} (<= 1.05): ${JSON.stringify(c.text)} at ${c.selector}`);
      }
      for (const f of (r.a11y?.failures || [])) {
        lines.push(`- a11y ${f.kind}: ${f.selector}${f.from ? ` (${f.from} -> ${f.to})` : ""}${f.src ? ` ${f.src}` : ""}`);
      }
      for (const v of (r.axe?.rules || []).filter((x) => x.impact === "critical" || x.impact === "serious")) {
        lines.push(`- axe ${v.ruleId} | ${v.impact} | ${v.help} | nodes:${v.nodes}`);
      }
      if (r.axe?.error) lines.push(`- axe audit error: ${r.axe.error}`);
      for (const f of (r.states?.hoverFailures || [])) {
        lines.push(`- hover contrast ${f.ratio} (<= 1.05): ${JSON.stringify(f.text)} at ${f.selector} (color ${f.color} on ${f.background})`);
      }
      for (const f of (r.states?.focusFailures || [])) {
        lines.push(`- no visible focus indicator: ${f.selector}`);
      }
      if (r.states?.error) lines.push(`- state audit error: ${r.states.error}`);
      if (r.keyboard?.trapped) {
        lines.push(`- keyboard trap: Tab never moves focus off ${r.keyboard.start} (visited: ${(r.keyboard.visited || []).join(" | ")})`);
      }
      if (r.keyboard?.error) lines.push(`- keyboard audit error: ${r.keyboard.error}`);
      if (r.warnGated) {
        for (const e of r.consoleWarnings) lines.push(`- console warning (warn-gate): ${e.split("\n")[0]}`);
      }
      if (r.dom && !r.dom.error) {
        for (const id of r.dom.duplicateIds) lines.push(`- duplicate id: ${id}`);
        for (const b of r.dom.brokenLinks) lines.push(`- broken link: ${b}`);
        for (const c of r.dom.clippedText) lines.push(`- clipped text: ${c}`);
      }
      for (const f of (r.perf?.failures || [])) {
        lines.push(`- perf budget: ${f}`);
      }
      if (r.golden?.failed) {
        lines.push(`- golden diff ${r.golden.pct}% (> ${GOLDEN_THRESHOLD_PCT}%): ${r.golden.diffPixels}/${r.golden.totalPixels} pixels vs ${r.golden.ref || "baseline"}`);
      }
      for (const sc of (r.golden?.styleChanges || []).slice(0, 10)) {
        lines.push(`- golden style ${sc.selector} ${sc.prop}: ${sc.from} -> ${sc.to}`);
      }
      if (r.golden?.error) lines.push(`- golden diff error: ${r.golden.error}`);
      for (const v of (r.variants || []).filter((x) => !x.ok)) {
        if (v.error) { lines.push(`- variant ${v.kind}:${v.label} run failed: ${v.error}`); continue; }
        lines.push(`- variant ${v.kind}:${v.label} FAIL${v.failedChecks?.length ? `: ${v.failedChecks.join("; ")}` : ""}`);
        for (const c of (v.visibility?.contrastFailures || [])) {
          lines.push(`  - contrast ${c.ratio} (<= 1.05): ${JSON.stringify(c.text)} at ${c.selector}`);
        }
        if (v.golden?.failed && !v.golden?.missing) {
          lines.push(`  - golden diff ${v.golden.pct}% (> ${GOLDEN_THRESHOLD_PCT}%): ${v.golden.diffPixels}/${v.golden.totalPixels} pixels vs variant baseline`);
        }
        if (v.golden?.missing) lines.push(`  - baseline missing: capture it with --golden-update (${v.screenshot || `${v.kind}:${v.label}`})`);
        for (const sc of (v.golden?.styleChanges || []).slice(0, 10)) {
          lines.push(`  - golden style @${v.label} ${sc.selector} ${sc.prop}: ${sc.from} -> ${sc.to}`);
        }
        if (v.golden?.error) lines.push(`  - variant golden error: ${v.golden.error}`);
      }
    }
  }
  // Warnings and geometry notes are judge material, not gate material: they
  // list under every route that has them, not only failing ones.
  const withWarnings = report.routes.filter((r) => !r.error && r.visibility
    && ((r.visibility.contrastWarnings?.length || 0) > 0
      || (r.visibility.geometry?.zeroSize?.length || 0) > 0
      || (r.visibility.geometry?.offscreen?.length || 0) > 0
      || (r.visibility.geometry?.overlap?.length || 0) > 0
      || (r.visibility.geometry?.imgDecode?.length || 0) > 0));
  if (withWarnings.length > 0) {
    lines.push("");
    lines.push("## Visibility warnings (judge material, not gated)");
    for (const r of withWarnings) {
      const v = r.visibility;
      for (const c of (v.contrastWarnings || []).slice(0, 5)) {
        lines.push(`- ${r.route}: contrast ${c.ratio} (< 3.0): ${JSON.stringify(c.text)} at ${c.selector}`);
      }
      for (const g of (v.geometry?.zeroSize || []).slice(0, 5)) {
        lines.push(`- ${r.route}: zero-size text element: ${JSON.stringify(g.text)} at ${g.selector}`);
      }
      for (const g of (v.geometry?.offscreen || []).slice(0, 5)) {
        lines.push(`- ${r.route}: text element entirely offscreen: ${JSON.stringify(g.text)} at ${g.selector}`);
      }
      for (const g of (v.geometry?.overlap || []).slice(0, 5)) {
        lines.push(`- ${r.route}: interactive elements overlap >80%: ${g.a} and ${g.b}`);
      }
      for (const g of (v.geometry?.imgDecode || []).slice(0, 5)) {
        lines.push(`- ${r.route}: img failed to decode: ${g.src} at ${g.selector}`);
      }
      const s = v.skipped || {};
      if ((s.textOverImage || 0) > 0) lines.push(`- ${r.route}: skipped ${s.textOverImage} text node(s) over a background image (not audited)`);
    }
  }
  // Deep-a11y (F13) moderate/minor violations are judge material, like
  // contrast warnings: listed under the route that has them, never gated.
  const axeWarns = report.routes.filter((r) => !r.error && (r.axe?.warns || 0) > 0);
  if (axeWarns.length > 0) {
    lines.push("");
    lines.push("## Axe warnings (judge material, not gated — moderate/minor impact)");
    for (const r of axeWarns) {
      for (const v of (r.axe.rules || []).filter((x) => x.impact !== "critical" && x.impact !== "serious").slice(0, 10)) {
        lines.push(`- ${r.route}: ${v.ruleId} | ${v.impact} | ${v.help} | nodes:${v.nodes}`);
      }
    }
  }
  // Displayed-number disagreements: informational only, never gate ok.
  const cross = report.routes.filter((r) => !r.error && (r.visibility?.crossChecks?.length || 0) > 0);
  if (cross.length > 0) {
    lines.push("");
    lines.push("## Cross-checks (informational — same label, different displayed values)");
    for (const r of cross) {
      for (const c of r.visibility.crossChecks.slice(0, 5)) {
        const spots = c.spots.map((s) => `${s.value} at ${s.selector}`).join("; ");
        lines.push(`- ${r.route}: ${JSON.stringify(c.label)} shown as ${spots}`);
      }
    }
  }
  lines.push("");
  lines.push("Screenshots are named in the table; the JSON twin of this report is smoke.json.");
  return lines.join("\n") + "\n";
}

async function main() {
  const args = parseArgs(process.argv);
  const outDir = resolve(args.out);
  mkdirSync(outDir, { recursive: true });

  // F1/F2: expand the viewport and scheme lists into variant descriptors.
  args.variants = [];
  for (const v of args.viewports) {
    const m = v.match(/^(\d+)x(\d+)$/);
    if (m) args.variants.push({ kind: "viewport", label: v, width: +m[1], height: +m[2] });
    else console.log(`ignoring malformed --viewports entry "${v}" (want WxH)`);
  }
  for (const s of args.schemes) {
    const features = SCHEME_FEATURES[s];
    if (features) args.variants.push({ kind: "scheme", label: s, features });
    else console.log(`ignoring unknown --schemes entry "${s}" (want dark|light|forced)`);
  }
  // F12: reduced-motion audit — a page that keeps animating under an emulated
  // prefers-reduced-motion: reduce violates WCAG 2.3.3. Same emulation knob
  // as schemes, different probe (running animations, not contrast).
  for (const m of args.motion) {
    if (m === "reduce") {
      args.variants.push({ kind: "motion", label: "reduce", features: [{ name: "prefers-reduced-motion", value: "reduce" }] });
    } else {
      console.log(`ignoring unknown --motion entry "${m}" (want reduce)`);
    }
  }

  // F14: the device matrix is host-supplied JSON, loaded before the
  // variant expansion below. Fail-closed policy: an unreadable or malformed
  // matrix must not run as an empty variant set.
  if (args.devices) {
    try {
      const parsed = JSON.parse(readFileSync(resolve(args.devices), "utf8"));
      if (!Array.isArray(parsed)) throw new Error("top level must be an array");
      args.deviceList = parsed;
    } catch (error) {
      console.error(`--devices ${args.devices}: device file not readable or not JSON (${error.message}); failing closed`);
      process.exit(2);
    }
  }

  // F14: device-matrix golden mode — a viewport + scheme library with
  // per-breakpoint baselines. Each device becomes a variant reusing the
  // F1/F2 machinery (metrics override + optional scheme emulation + UA),
  // with its own per-device golden pair (NN-route__d<name>.png/.styles.json)
  // captured by --golden-update and enforced on every later run.
  if (args.devices) {
    const usedLabels = new Set();
    args.deviceList.forEach((d, i) => {
      const cleaned = String(d.name ?? "").toLowerCase().replace(/[^a-z0-9]+/g, "-").replace(/^-+|-+$/g, "");
      let label = cleaned || `device-${i + 1}`;
      let n = 2;
      while (usedLabels.has(label)) { label = `${cleaned || "device"}-${n}`; n += 1; }
      usedLabels.add(label);
      const features = d.scheme ? SCHEME_FEATURES[d.scheme] : undefined;
      if (d.scheme && !features) console.log(`ignoring unknown device scheme "${d.scheme}" for device ${d.name} (want dark|light|forced)`);
      args.variants.push({
        kind: "device", label,
        width: Number(d.width) || args.width,
        height: Number(d.height) || args.height,
        scale: Number(d.scale) || 1,
        ua: typeof d.ua === "string" ? d.ua : "",
        features,
      });
    });
  }

  // F13: the axe bundle is host-supplied (--axe path). Load it here and
  // fail closed (exit 2) when it is missing or unreadable: a --axe run that
  // silently skipped the audit would let violations through the gate.
  if (args.axe) {
    try {
      args.axeSource = readFileSync(resolve(args.axe), "utf8");
    } catch (error) {
      console.error(`--axe ${args.axe}: bundle not readable (${error.message}); failing closed`);
      process.exit(2);
    }
  }

  // F5: seed script rides every new document on every tab (localStorage
  // seeding for state-dependent audits like interaction-hidden contrast).
  if (args.seedFile) {
    args.seedSource = readFileSync(resolve(args.seedFile), "utf8");
    console.log(`seed script: ${args.seedFile} (${args.seedSource.length} chars)`);
  }
  if (args.goldenUpdate && !args.golden) throw new Error("--golden-update needs --golden <dir>");

  const chrome = findChrome();
  const userDataDir = join(tmpdir(), `qa-smoke-${process.pid}`);
  const chromeArgs = [
    args.headed ? "--headless=new --auto-open-devtools-for-tabs" : "--headless=new",
    `--remote-debugging-port=${args.port}`,
    `--user-data-dir=${userDataDir}`,
    "--no-first-run",
    "--disable-extensions",
    "--disable-session-crashed-bubble",
    "--hide-scrollbars",
    `--window-size=${args.width},${args.height}`,
    "--force-device-scale-factor=1",
    "about:blank",
  ];
  const proc = spawn(chrome, chromeArgs, { stdio: "ignore" });

  const report = { baseUrl: args.url, startedAt: new Date().toISOString(), routes: [], summary: {} };
  let exitCode = 0;

  try {
    await waitForDebugger(args.port, 20000);

    let routes = [...args.routes];
    if (args.discover > 0) {
      console.log(`== discovering routes (cap ${args.discover}) ==`);
      try {
        const found = await discoverRoutes(args.port, args);
        for (const r of found) if (!routes.includes(r)) routes.push(r);
        console.log(`discovered ${found.length} routes`);
      } catch (error) {
        console.log(`discovery failed (${error.message}); continuing with --routes only`);
      }
    }
    if (routes.length === 0) throw new Error("no routes: pass --routes or --discover");
    if (routes.length > args.maxroutes) {
      console.log(`capping ${routes.length} routes at --maxroutes ${args.maxroutes}`);
      routes = routes.slice(0, args.maxroutes);
    }

    const targets = [];
    for (let i = 0; i < Math.min(args.parallel, routes.length); i += 1) {
      const target = await newPageTarget(args.port);
      const client = new CdpClient(target.webSocketDebuggerUrl);
      await sendRetry(client, "Emulation.setDeviceMetricsOverride", {
        width: args.width, height: args.height, deviceScaleFactor: 1, mobile: false,
      });
      const inject = [args.seedSource, args.maxLcpMs > 0 ? LCP_RECORDER : ""]
        .filter(Boolean).join("\n;\n");
      if (inject) {
        await sendRetry(client, "Page.enable");
        await sendRetry(client, "Page.addScriptToEvaluateOnNewDocument", { source: inject });
      }
      targets.push(client);
    }

    const results = new Array(routes.length);
    await Promise.all(targets.map(async (client, worker) => {
      for (let i = worker; i < routes.length; i += targets.length) {
        const route = routes[i];
        const shotName = `${String(i + 1).padStart(2, "0")}-${safeName(route)}.png`;
        const shotStem = shotName.replace(/\.png$/, "");
        const started = Date.now();
        try {
          const { problems, page, docHeaders, visibility, a11y, dom, axe, golden, states, keyboard, variants, transferBytes } = await collectRoute(client, route, args, join(outDir, shotName), shotName, shotStem);
          const contrastFails = visibility?.contrastFailures?.length || 0;
          const a11yFails = a11y?.failures?.length || 0;
          const axeFails = axe?.fails || 0;
          const axeError = Boolean(axe?.error);
          const domFails = dom && !dom.error
            ? dom.duplicateIds.length + dom.brokenLinks.length + dom.clippedText.length
            : 0;
          const stateFails = (states?.hoverFailures?.length || 0) + (states?.focusFailures?.length || 0);
          const stateError = Boolean(states?.error);
          const keyboardTrapped = keyboard?.trapped === true;
          const keyboardError = Boolean(keyboard?.error);
          const perfFailures = [];
          if (args.maxBytes > 0 && page.decodedBytes > args.maxBytes) {
            perfFailures.push(`page weight ${(page.decodedBytes / 1048576).toFixed(2)}MB decoded > budget ${(args.maxBytes / 1048576).toFixed(2)}MB`);
          }
          if (args.maxLcpMs > 0 && page.lcp != null && page.lcp > args.maxLcpMs) {
            perfFailures.push(`LCP ${Math.round(page.lcp)}ms > budget ${args.maxLcpMs}ms`);
          }
          const goldenFail = golden?.failed === true || golden?.styleFailed === true;
          const badVariants = variants.filter((v) => !v.ok);
          const ok = problems.consoleErrors.length === 0
            && problems.exceptions.length === 0
            && problems.failedRequests.length === 0
            && (!args.warnGate || problems.consoleWarnings.length === 0)
            && !problems.blank
            && contrastFails === 0
            && a11yFails === 0
            && axeFails === 0
            && !axeError
            && domFails === 0
            && stateFails === 0
            && !stateError
            && !keyboardTrapped
            && !keyboardError
            && perfFailures.length === 0
            && !goldenFail
            && badVariants.length === 0
            && (problems.documentStatus === null
              || (problems.documentStatus >= 200 && problems.documentStatus < 400));
          const fingerprint = computeFingerprint(page, docHeaders);
          results[i] = { route, ok, ms: Date.now() - started, screenshot: shotName, warnGated: args.warnGate && problems.consoleWarnings.length > 0, ...problems, page, fingerprint, visibility, a11y, dom, golden, states, keyboard, variants, perf: { transferBytes, decodedBytes: page.decodedBytes, lcp: page.lcp, failures: perfFailures }, transferBytes, axe };
          const extras = [];
          if (a11yFails) extras.push(`a11y=${a11yFails}`);
          if (axeFails) extras.push(`axe=${axeFails}`);
          if (axeError) extras.push("AXE-ERR");
          if (domFails) extras.push(`dom=${domFails}`);
          if (stateFails) extras.push(`states=${stateFails}`);
          if (stateError) extras.push("STATES-ERR");
          if (keyboardTrapped) extras.push("TAB-TRAPPED");
          if (keyboardError) extras.push("KB-ERR");
          if (perfFailures.length) extras.push(`perf=${perfFailures.length}`);
          if (goldenFail) extras.push(`golden=${golden.pct}%`);
          for (const v of badVariants) extras.push(`${v.kind}:${v.label}=FAIL`);
          const flag = ok ? "ok" : "FAIL";
          console.log(`${flag} ${route} status=${problems.documentStatus ?? "?"} errors=${problems.consoleErrors.length} warnings=${problems.consoleWarnings.length}${args.warnGate && problems.consoleWarnings.length ? " WARN-GATED" : ""} exceptions=${problems.exceptions.length} failedReq=${problems.failedRequests.length}${problems.blank ? " BLANK" : ""} contrast=${contrastFails}${extras.length ? " " + extras.join(" ") : ""} -> ${shotName}`);
        } catch (error) {
          results[i] = { route, ok: false, ms: Date.now() - started, screenshot: null, error: String(error.message || error) };
          console.log(`FAIL ${route}: ${error.message || error}`);
        }
      }
    }));

    for (const client of targets) client.close();
    report.routes = results;
  } finally {
    // Killing Chrome while its spawn/stdio handles are still settling trips a
    // libuv assertion on Windows (exit 0xC0000409 instead of the real code).
    // Settle briefly before the kill; report + exit code stay authoritative.
    await sleep(400);
    proc.kill("SIGKILL");
  }

  report.summary = {
    routes: report.routes.length,
    ok: report.routes.filter((r) => r.ok).length,
    totalErrors: report.routes.reduce((n, r) => n + (r.consoleErrors?.length || 0) + (r.exceptions?.length || 0) + (r.failedRequests?.length || 0) + (r.blank ? 1 : 0), 0),
    totalBlocked: report.routes.reduce((n, r) => n + (r.blockedRequests?.length || 0), 0),
  };

  // Build identity: one line, and the --expect-build gate (fail closed, exit 2).
  const fingerprints = report.routes.filter((r) => r.fingerprint).map((r) => r.fingerprint);
  const buildIds = [...new Set(fingerprints.filter((f) => f.kind === "buildId").map((f) => f.id))];
  let buildLine;
  if (buildIds.length > 0) {
    buildLine = buildIds.join(", ") + (buildIds.length > 1 ? " (MISMATCH across routes)" : "");
  } else if (fingerprints.length > 0) {
    const anyDev = fingerprints.some((f) => f.kind.startsWith("dev"));
    const union = createHash("sha1")
      .update(fingerprints.filter((f) => f.input).map((f) => f.input).sort().join("\n---\n"))
      .digest("hex").slice(0, 16);
    buildLine = `${anyDev ? "dev" : "static"}-union:${union}`;
  } else {
    buildLine = "no identity extracted";
  }
  report.build = {
    run: buildLine,
    perRoute: fingerprints.map((f) => f.id),
    kinds: [...new Set(fingerprints.map((f) => f.kind))],
  };
  console.log(`Build: ${buildLine}`);
  if (args.expectBuild) {
    const wanted = args.expectBuild;
    const bare = (id) => id.replace(/^next:/, "");
    if (fingerprints.length === 0) {
      console.error(`--expect-build ${wanted}: no build identity could be extracted; failing closed`);
      exitCode = 2;
    } else if (!fingerprints.some((f) => f.id === wanted || bare(f.id) === wanted)) {
      console.error(`--expect-build ${wanted}: served build is ${buildLine}`);
      exitCode = 2;
    } else {
      console.log(`build matches --expect-build ${wanted}`);
    }
  }

  writeFileSync(join(outDir, "smoke.json"), JSON.stringify(report, null, 2));
  writeFileSync(join(outDir, "report.md"), buildMarkdown(report));
  if (exitCode === 0 && report.summary.ok < report.routes.length) exitCode = 1;
  console.log(`summary: ${report.summary.ok}/${report.summary.routes} routes ok, ${report.summary.totalErrors} problems -> ${join(args.out, "report.md")}`);
  process.exit(exitCode);
}

main().catch((error) => {
  console.error(`qa-smoke: ${error.message || error}`);
  process.exit(2);
});
