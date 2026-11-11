// Scan gate for the SlimToken engine's security pack.
//
// Contract (see sidecars/README.md):
//   - exit 0: no findings (2xx/3xx/4xx responses, no markers reflected)
//   - exit 1: at least one finding
//   - exit 2: usage error, refused remote target, or nothing was tested
//   - zero tokens: pure HTTP replay with fetch, no model calls
//
// Safe-target rule: the base URL hostname must be localhost, 127.0.0.1,
// or [::1], unless --allow-remote is passed. Without the flag a remote
// host is refused before any fetch (exit 2). With the flag a one-line
// "only scan hosts you own" notice prints and the scan proceeds.
//
// Finding classes (both are ERROR, exit 1):
//   - crash  <name>: HTTP 5xx — the server crashed on the input.
//   - marker <name>: '<m>' reflected/executed — an injection marker from
//     the case came back in the response body.
// A case that cannot connect (connection refused, timeout) is
// WARN UNREACHABLE and never fails alone; if every case is unreachable,
// nothing was tested — fail closed (exit 2).
import { mkdirSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { loadCorpus } from "./corpus.mjs";

const LOCAL_HOSTS = new Set(["localhost", "127.0.0.1", "::1"]);
const REMOTE_NOTICE = "notice: --allow-remote set — only scan hosts you own.";

export async function runScan(args) {
  const parsed = parseArgs(args);
  if (parsed.error) {
    console.error(`scan: ${parsed.error}`);
    return 2;
  }

  let base;
  try {
    base = new URL(parsed.baseUrl);
  } catch {
    console.error(`scan: base URL is not a valid URL: '${parsed.baseUrl}'`);
    return 2;
  }
  if (base.protocol !== "http:" && base.protocol !== "https:") {
    console.error(`scan: base URL must be http or https: '${parsed.baseUrl}'`);
    return 2;
  }
  const host = base.hostname.toLowerCase().replace(/^\[|\]$/g, "");
  if (!LOCAL_HOSTS.has(host)) {
    if (!parsed.allowRemote) {
      console.error(
        `scan: refusing to scan remote host '${base.host}' — safe-target rule: ` +
          "the default posture is localhost (127.0.0.1, ::1), and you should " +
          "only scan hosts you own. Pass --allow-remote to override."
      );
      return 2;
    }
    console.log(REMOTE_NOTICE);
  }

  const corpus = loadCorpus(parsed.corpus);
  if (corpus.error) {
    console.error(`scan: ${corpus.error}`);
    return 2;
  }

  const results = [];
  for (const c of corpus.cases) {
    results.push(await runCase(base, c, parsed.timeout));
  }

  let errors = 0;
  let unreachable = 0;
  for (const r of results) {
    if (r.status === "error") {
      errors += 1;
      if (r.crash) console.log(`ERROR crash ${r.name}: HTTP ${r.statusCode}`);
      for (const m of r.markersFound) {
        console.log(`ERROR marker ${r.name}: '${m}' reflected/executed`);
      }
    } else if (r.kind === "unreachable") {
      unreachable += 1;
      console.log(`WARN UNREACHABLE ${r.name}: ${r.detail}`);
    } else {
      let line = `OK   ${r.name}: HTTP ${r.statusCode}`;
      if (r.noProbe) line += " (warn: no body/markers; only reachability proven)";
      console.log(line);
    }
  }
  console.log(`scan: ${results.length - errors - unreachable}/${results.length} ok, ${unreachable} unreachable, ${errors} error`);

  let exitCode;
  if (errors > 0) {
    exitCode = 1;
  } else if (unreachable === results.length) {
    console.error("scan: nothing was tested (every case unreachable) — fail closed");
    exitCode = 2;
  } else {
    exitCode = 0;
  }

  if (parsed.outDir) writeReports(parsed.outDir, parsed, base, results, exitCode);
  return exitCode;
}

async function runCase(base, c, timeout) {
  const method = (c.method ?? "GET").toUpperCase();
  const noProbe = c.body === undefined && (c.markers === undefined || c.markers.length === 0);
  // GET/HEAD cannot carry a body in fetch; drop it instead of failing the case.
  const body = method === "GET" || method === "HEAD" ? undefined : c.body;

  let res;
  try {
    res = await fetch(new URL(c.path, base), {
      method,
      headers: c.headers,
      body,
      signal: AbortSignal.timeout(timeout),
    });
  } catch (error) {
    const reason =
      error?.name === "TimeoutError"
        ? `timeout after ${timeout}ms`
        : (error?.cause?.message ?? error?.message ?? String(error));
    return { name: c.name, status: "warn", kind: "unreachable", detail: reason };
  }

  const result = { name: c.name, status: "ok", statusCode: res.status, markersFound: [] };
  let text = "";
  try {
    text = await res.text();
  } catch {
    // A body that cannot be read still leaves the status code as evidence.
  }
  const found = (c.markers ?? []).filter((m) => m.length > 0 && text.includes(m));
  if (found.length > 0) result.markersFound = found;
  if (res.status >= 500) result.crash = true;
  if (result.crash || found.length > 0) result.status = "error";
  if (noProbe) result.noProbe = true;
  return result;
}

function writeReports(outDir, parsed, base, results, exitCode) {
  mkdirSync(outDir, { recursive: true });

  const lines = [];
  lines.push("# slimtoken-security scan report", "");
  lines.push(`- Base: ${base.href}`);
  lines.push(`- Corpus: ${parsed.corpus}`);
  lines.push(`- Cases: ${results.length}`);
  lines.push(`- Allow remote: ${parsed.allowRemote ? "yes" : "no"}`);
  lines.push("", "| Case | Status | HTTP | Findings |", "|---|---|---|---|");
  for (const r of results) {
    const findings = [];
    if (r.crash) findings.push(`crash: HTTP ${r.statusCode}`);
    for (const m of r.markersFound ?? []) findings.push(`marker '${m}' reflected/executed`);
    if (r.kind === "unreachable") findings.push(`unreachable: ${r.detail}`);
    if (r.noProbe) findings.push("no body/markers; only reachability proven");
    const http = r.statusCode === undefined ? "—" : String(r.statusCode);
    lines.push(`| ${r.name} | ${r.status.toUpperCase()} | ${http} | ${findings.join("; ") || "—"} |`);
  }
  const errors = results.filter((r) => r.status === "error").length;
  const unreachable = results.filter((r) => r.kind === "unreachable").length;
  const verdict = exitCode === 0 ? "PASS" : exitCode === 1 ? "FINDINGS" : "NOTHING TESTED";
  lines.push("", "## Summary", "");
  lines.push(`${results.length} cases: ${results.length - errors - unreachable} ok, ${unreachable} unreachable, ${errors} error.`);
  lines.push(`Verdict: ${verdict} (exit ${exitCode}).`);
  writeFileSync(join(outDir, "report.md"), lines.join("\n") + "\n", "utf8");

  const twin = {
    ok: exitCode === 0,
    cases: results.map((r) => {
      const c = { name: r.name, status: r.status };
      if (r.statusCode !== undefined) c.statusCode = r.statusCode;
      if (r.markersFound && r.markersFound.length > 0) c.markersFound = r.markersFound;
      if (r.kind === "unreachable") c.detail = `unreachable: ${r.detail}`;
      if (r.noProbe) c.detail = "no body/markers; only reachability proven";
      return c;
    }),
    summary: `${results.length} cases: ${results.length - errors - unreachable} ok, ${unreachable} unreachable, ${errors} error; verdict ${verdict} (exit ${exitCode})`,
  };
  writeFileSync(join(outDir, "security.json"), JSON.stringify(twin, null, 2) + "\n", "utf8");
}

function parseArgs(args) {
  let baseUrl = null;
  let corpus = null;
  let allowRemote = false;
  let timeout = 10000;
  let outDir = null;
  for (let i = 0; i < args.length; i += 1) {
    const a = args[i];
    if (a === "--corpus") {
      if (i + 1 >= args.length) return { error: "--corpus needs a directory" };
      corpus = args[i + 1];
      i += 1;
    } else if (a.startsWith("--corpus=")) {
      corpus = a.slice("--corpus=".length);
    } else if (a === "--allow-remote") {
      allowRemote = true;
    } else if (a === "--timeout") {
      if (i + 1 >= args.length) return { error: "--timeout needs a value in ms" };
      const t = Number(args[i + 1]);
      if (!Number.isInteger(t) || t <= 0) return { error: `--timeout must be a positive integer, got '${args[i + 1]}'` };
      timeout = t;
      i += 1;
    } else if (a.startsWith("--timeout=")) {
      const t = Number(a.slice("--timeout=".length));
      if (!Number.isInteger(t) || t <= 0) return { error: `--timeout must be a positive integer, got '${a}'` };
      timeout = t;
    } else if (a === "--out") {
      if (i + 1 >= args.length) return { error: "--out needs a directory" };
      outDir = args[i + 1];
      i += 1;
    } else if (a.startsWith("--out=")) {
      outDir = a.slice("--out=".length);
    } else if (a.startsWith("--")) {
      return { error: `unknown flag '${a}'` };
    } else if (baseUrl === null) {
      baseUrl = a;
    } else {
      return { error: `unexpected argument '${a}'` };
    }
  }
  if (baseUrl === null) {
    return { error: "no base URL (usage: slimtoken-security scan <base-url> --corpus <dir> [--allow-remote] [--timeout ms] [--out DIR])" };
  }
  if (corpus === null) {
    return { error: "missing --corpus <dir>" };
  }
  if (corpus.length === 0) return { error: "--corpus needs a directory" };
  if (outDir !== null && outDir.length === 0) return { error: "--out needs a directory" };
  return { baseUrl, corpus, allowRemote, timeout, outDir };
}
