// Report writer for the slimtoken-trace pack.
//
// Emits the judge material: a report.md a human or LLM reads in one
// glance, plus a machine-readable trace.json twin with stable field
// names (new fields append at the end). The tuning brief names the
// SchedulerConfig knob to look at per finding; it is judge material,
// not doctrine — the knobs interact and the host measures before
// trusting.
import { mkdirSync, writeFileSync } from "node:fs";
import { join } from "node:path";

// One brief line per finding rule: the SchedulerConfig knob (or prompt
// surface) the host should look at first.
export const TUNING_BRIEF = {
  "retry-storm":
    "retry-storm -> lower `reasoning_effort` or fix the task prompt: the task keeps failing its attempts.",
  "repair-queue-depth":
    "repair-queue-depth -> raise `max_concurrent_repairs` or cut the failure source that fills the queue.",
  "rate-churn":
    "rate-churn -> revisit `rate_auto_tune`: the tuner changed pace too often for a steady wave.",
  "low-cache-hit":
    "low-cache-hit -> add `shared_context_extra` and set a stable `prompt_cache_key` so prefixes cache.",
  "concurrency-collapse":
    "concurrency-collapse -> enable the concurrency probe or raise the auto width settings (`probe_width` / `max_concurrent`).",
  "failures-present":
    "failures-present -> inspect the failed tasks; fix prompts or gate inputs before the next wave."
};

export function buildReportMarkdown(result) {
  const lines = [];
  lines.push("# slimtoken-trace report");
  lines.push("");
  lines.push(
    result.ok
      ? "Verdict: no findings (exit 0)."
      : `Verdict: ${result.findings.length} finding(s) (exit 1).`
  );
  lines.push("");
  lines.push("## Findings");
  lines.push("");
  if (result.findings.length === 0) {
    lines.push("None.");
  } else {
    for (const finding of result.findings) {
      lines.push(`- ${finding.detail}`);
      lines.push(`  evidence: ${JSON.stringify(finding.evidence)}`);
    }
  }
  lines.push("");
  lines.push("## Tuning brief");
  lines.push("");
  if (result.findings.length === 0) {
    lines.push("No anomalies; no knobs to look at.");
  } else {
    for (const finding of result.findings) {
      const brief = TUNING_BRIEF[finding.rule];
      lines.push(`- ${brief || `${finding.rule} -> no brief entry yet.`}`);
    }
  }
  lines.push("");
  lines.push(
    "This brief is judge material, not doctrine. The knobs interact " +
      "(width feeds retry pressure, cache layout feeds width), so the " +
      "host measures with an A/B run before trusting any single line."
  );
  lines.push("");
  lines.push("## Stats");
  lines.push("");
  lines.push("```");
  lines.push(JSON.stringify(result.stats, null, 2));
  lines.push("```");
  lines.push("");
  return lines.join("\n");
}

export function buildTraceJson(result) {
  return {
    ok: result.ok,
    findings: result.findings.map((f) => ({
      rule: f.rule,
      detail: f.detail,
      evidence: f.evidence
    })),
    stats: result.stats,
    summary:
      result.findings.length === 0
        ? "no findings"
        : `${result.findings.length} finding(s): ` +
          result.findings.map((f) => f.rule).join(", ")
  };
}

export function writeReports(outDir, result) {
  mkdirSync(outDir, { recursive: true });
  writeFileSync(join(outDir, "report.md"), buildReportMarkdown(result), "utf8");
  writeFileSync(join(outDir, "trace.json"), JSON.stringify(buildTraceJson(result), null, 2) + "\n", "utf8");
}
