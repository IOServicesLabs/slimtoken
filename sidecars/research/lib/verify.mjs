// Citation gate for the SlimToken engine's research pack.
//
// `verify` is the zero-token gate. Sentence: "Fetch every citation;
// every load-bearing claim must appear in its source." It parses inline
// [claim text](url) citations from a report, fetches each URL (global
// fetch, 10s timeout, a normal User-Agent), strips the HTML to text,
// and checks that each load-bearing claim appears in the page it cites.
// Fetch + string matching only; no model calls.
//
// Claim rules:
//   - a claim is load-bearing unless it is marked (opinion)
//   - a claim marked (paraphrase) may pass on token overlap >= 0.6
//     (status PARTIAL) instead of an exact substring match
//   - a claim marked (consensus) must verify in >= 2 distinct hostnames
//
// Fetch rules:
//   - HTTP 404/410 on a load-bearing citation = ERROR "citation does not exist"
//   - any other fetch failure (DNS, timeout, non-OK status) = WARN
//     UNFETCHABLE; never a failure on its own
//   - claim text absent from a fetched source = ERROR "claim not found"
//
// Contract (see sidecars/README.md):
//   - exit 0: no ERROR (UNFETCHABLE warnings allowed)
//   - exit 1: at least one ERROR
//   - exit 2: usage error (no report, missing file)
import { readFileSync, writeFileSync, mkdirSync, existsSync } from "node:fs";
import { join } from "node:path";

const CITATION = /\[([^\]]+)\]\((https?:\/\/[^)\s]+)\)/g;
const FETCH_TIMEOUT_MS = 10_000;
const PARAPHRASE_OVERLAP = 0.6;
const USER_AGENT = "slimtoken-research/0.1 (+https://github.com/IOServicesLabs/slimtoken)";

export async function runVerify(args) {
  const parsed = parseArgs(args);
  if (parsed.error) {
    console.error(`verify: ${parsed.error}`);
    return 2;
  }
  const { reportFile, outDir } = parsed;

  if (!existsSync(reportFile)) {
    console.error(`verify: report '${reportFile}' does not exist`);
    return 2;
  }
  let text;
  try {
    text = readFileSync(reportFile, "utf8");
  } catch (error) {
    console.error(`verify: cannot read report '${reportFile}': ${error.message}`);
    return 2;
  }

  const gate = new VerifyGate();
  const { claims, errorCount } = await gate.verify(text);

  for (const line of gate.lines) console.log(line);
  console.log(gate.summaryLine());

  if (outDir) writeReports(outDir, errorCount === 0, claims, gate.summary);
  return errorCount === 0 ? 0 : 1;
}

class VerifyGate {
  constructor() {
    this.lines = [];
    this.pages = new Map(); // url -> { ok, status, text }
    this.summary = { claims: 0, verified: 0, partial: 0, unverified: 0, unfetchable: 0, errors: 0, warns: 0 };
  }

  async verify(reportText) {
    const groups = parseClaims(reportText);
    const claims = [];
    let errorCount = 0;

    for (const group of groups) {
      this.summary.claims += 1;
      if (group.opinion) {
        this.lines.push(`SKIP     ${group.display} (opinion, not load-bearing)`);
        claims.push({ claim: group.display, url: null, status: "SKIP" });
        continue;
      }

      // Fetch every citation once, first page text wins per URL.
      for (const citation of group.citations) {
        if (!this.pages.has(citation.url)) {
          this.pages.set(citation.url, await fetchPage(citation.url));
        }
      }

      if (group.consensus) {
        errorCount += await this.verifyConsensus(group, claims);
      } else {
        errorCount += await this.verifySingle(group, claims);
      }
    }
    this.summary.errors = errorCount;
    return { claims, errorCount };
  }

  async verifySingle(group, claims) {
    let errors = 0;
    for (const citation of group.citations) {
      const page = this.pages.get(citation.url);
      const result = checkClaim(group, page);
      claims.push({ claim: group.display, url: citation.url, status: result.status });
      if (result.status === "VERIFIED") {
        this.summary.verified += 1;
        this.lines.push(`VERIFIED ${group.display} -> ${citation.url}`);
      } else if (result.status === "PARTIAL") {
        this.summary.partial += 1;
        this.lines.push(`PARTIAL  ${group.display} -> ${citation.url} (paraphrase, overlap ${result.overlap})`);
      } else if (result.status === "UNFETCHABLE") {
        this.summary.unfetchable += 1;
        this.summary.warns += 1;
        this.lines.push(`WARN UNFETCHABLE ${group.display} -> ${citation.url} (${page.reason}); not verified, not a failure`);
      } else if (result.status === "MISSING") {
        errors += 1;
        this.lines.push(`ERROR    ${group.display} -> ${citation.url}: citation does not exist (${page.status})`);
      } else {
        this.summary.unverified += 1;
        errors += 1;
        this.lines.push(`ERROR    ${group.display} -> ${citation.url}: claim not found in source`);
      }
    }
    return errors;
  }

  async verifyConsensus(group, claims) {
    let errors = 0;
    const verifiedHosts = [];
    for (const citation of group.citations) {
      const page = this.pages.get(citation.url);
      const result = checkClaim(group, page);
      if (result.status === "VERIFIED" || result.status === "PARTIAL") {
        if (!verifiedHosts.includes(citation.host)) verifiedHosts.push(citation.host);
        this.lines.push(`${result.status.padEnd(8)} ${group.display} -> ${citation.url} (consensus source ${citation.host})`);
        this.summary[result.status === "VERIFIED" ? "verified" : "partial"] += 1;
      } else if (result.status === "UNFETCHABLE") {
        this.summary.unfetchable += 1;
        this.summary.warns += 1;
        this.lines.push(`WARN UNFETCHABLE ${group.display} -> ${citation.url} (${page.reason}); consensus source not counted`);
      } else if (result.status === "MISSING") {
        errors += 1;
        this.lines.push(`ERROR    ${group.display} -> ${citation.url}: citation does not exist (${page.status})`);
      } else {
        this.summary.unverified += 1;
        this.lines.push(`UNVERIFIED ${group.display} -> ${citation.url} (not counted as a consensus source)`);
      }
    }
    const distinctHosts = new Set(verifiedHosts).size;
    if (distinctHosts < 2) {
      errors += 1;
      this.lines.push(`ERROR    ${group.display}: consensus claim verified in only ${distinctHosts} hostname(s); needs >= 2`);
    }
    const status = errors > 0 || distinctHosts < 2 ? "UNVERIFIED" : "VERIFIED";
    claims.push({ claim: group.display, url: null, status, hosts: verifiedHosts });
    return errors;
  }

  summaryLine() {
    const s = this.summary;
    return `verify: ${s.claims} claim(s): ${s.verified} verified, ${s.partial} partial, ${s.unverified} unverified, ${s.unfetchable} unfetchable; ${s.errors} error(s), ${s.warns} warn(s)`;
  }
}

// Parse [text](url) citations and group them by normalized claim text.
// Markers: (opinion), (consensus), (paraphrase) inside the link text.
function parseClaims(text) {
  const groups = new Map();
  CITATION.lastIndex = 0;
  let match;
  while ((match = CITATION.exec(text)) !== null) {
    const raw = match[1];
    const url = match[2];
    const opinion = raw.includes("(opinion)");
    const consensus = raw.includes("(consensus)");
    const paraphrase = raw.includes("(paraphrase)");
    const normalized = raw
      .replace(/\(opinion\)|\(consensus\)|\(paraphrase\)/g, " ")
      .replace(/\s+/g, " ")
      .trim();
    if (normalized.length === 0) continue;
    const key = normalized.toLowerCase();
    let host = null;
    try {
      host = new URL(url).hostname.toLowerCase();
    } catch {
      host = url;
    }
    if (!groups.has(key)) {
      groups.set(key, { key, display: normalized, opinion, consensus, paraphrase, citations: [] });
    }
    const group = groups.get(key);
    group.opinion = group.opinion && opinion;
    group.consensus = group.consensus || consensus;
    group.paraphrase = group.paraphrase || paraphrase;
    if (!group.citations.some((c) => c.url === url)) {
      group.citations.push({ url, host });
    }
  }
  return [...groups.values()];
}

async function fetchPage(url) {
  const controller = new AbortController();
  const timer = setTimeout(() => controller.abort(), FETCH_TIMEOUT_MS);
  try {
    const response = await fetch(url, {
      signal: controller.signal,
      headers: { "User-Agent": USER_AGENT, Accept: "text/html,application/xhtml+xml,text/plain;q=0.9,*/*;q=0.8" },
      redirect: "follow",
    });
    if (response.status === 404 || response.status === 410) {
      return { ok: false, kind: "missing", status: response.status, reason: `HTTP ${response.status}` };
    }
    if (!response.ok) {
      return { ok: false, kind: "unfetchable", status: response.status, reason: `HTTP ${response.status}` };
    }
    const body = await response.text();
    return { ok: true, status: response.status, text: htmlToText(body) };
  } catch (error) {
    const reason = error.name === "AbortError" ? "timeout" : String(error.cause?.message || error.message || error);
    return { ok: false, kind: "unfetchable", reason };
  } finally {
    clearTimeout(timer);
  }
}

// Crude tag strip: drop script/style blocks, tags -> space, decode a few
// entities, collapse whitespace, lowercase. Good enough for substring
// and token-overlap checks.
export function htmlToText(html) {
  return html
    .replace(/<script[\s\S]*?<\/script>/gi, " ")
    .replace(/<style[\s\S]*?<\/style>/gi, " ")
    .replace(/<[^>]+>/g, " ")
    .replace(/&amp;/g, "&")
    .replace(/&lt;/g, "<")
    .replace(/&gt;/g, ">")
    .replace(/&quot;/g, '"')
    .replace(/&#0?39;/g, "'")
    .replace(/&nbsp;/g, " ")
    .replace(/\s+/g, " ")
    .trim()
    .toLowerCase();
}

function normalizeClaim(text) {
  return text.replace(/\s+/g, " ").trim().toLowerCase();
}

function tokenize(text) {
  return text.toLowerCase().match(/[a-z0-9]+/g) || [];
}

function tokenOverlap(claim, pageText) {
  const claimTokens = tokenize(claim);
  if (claimTokens.length === 0) return 0;
  const pageSet = new Set(tokenize(pageText));
  const hit = claimTokens.filter((token) => pageSet.has(token)).length;
  return hit / claimTokens.length;
}

// One claim against one fetched page. Returns { status, overlap? } where
// status is VERIFIED | PARTIAL | UNVERIFIED | UNFETCHABLE | MISSING.
function checkClaim(group, page) {
  if (page.ok) {
    const needle = normalizeClaim(group.key);
    if (page.text.includes(needle)) return { status: "VERIFIED" };
    if (group.paraphrase) {
      const overlap = tokenOverlap(group.key, page.text);
      if (overlap >= PARAPHRASE_OVERLAP) return { status: "PARTIAL", overlap: overlap.toFixed(2) };
    }
    return { status: "UNVERIFIED" };
  }
  if (page.kind === "missing") return { status: "MISSING" };
  return { status: "UNFETCHABLE" };
}

function writeReports(outDir, ok, claims, summary) {
  mkdirSync(outDir, { recursive: true });

  const lines = [
    "# slimtoken-research report",
    "",
    `- ok: ${ok}`,
    `- claims: ${summary.claims} (verified ${summary.verified}, partial ${summary.partial}, unverified ${summary.unverified}, unfetchable ${summary.unfetchable})`,
    `- errors: ${summary.errors}`,
    `- warnings: ${summary.warns}`,
    "",
    "| Claim | URL | Status | Hosts |",
    "|---|---|---|---|",
  ];
  for (const claim of claims) {
    lines.push(`| ${claim.claim} | ${claim.url ?? ""} | ${claim.status} | ${(claim.hosts || []).join(", ")} |`);
  }
  lines.push("");
  writeFileSync(join(outDir, "report.md"), lines.join("\n"), "utf8");

  const research = { ok, claims, summary };
  writeFileSync(join(outDir, "research.json"), JSON.stringify(research, null, 2) + "\n", "utf8");
  console.log(`verify: wrote ${join(outDir, "report.md")} and ${join(outDir, "research.json")}`);
}

function parseArgs(args) {
  let reportFile = null;
  let outDir = null;
  for (let i = 0; i < args.length; i += 1) {
    const arg = args[i];
    if (arg === "--out") {
      if (i + 1 >= args.length) return { error: "--out needs a directory" };
      outDir = args[i + 1];
      i += 1;
    } else if (arg.startsWith("--out=")) {
      outDir = arg.slice("--out=".length);
    } else if (arg.startsWith("--")) {
      return { error: `unknown flag '${arg}'` };
    } else if (reportFile === null) {
      reportFile = arg;
    } else {
      return { error: `unexpected extra argument '${arg}'` };
    }
  }
  if (reportFile === null) {
    return { error: "no report (usage: slimtoken-research verify <report.md> [--out DIR])" };
  }
  if (outDir !== null && outDir.length === 0) return { error: "--out needs a directory" };
  return { reportFile, outDir };
}
