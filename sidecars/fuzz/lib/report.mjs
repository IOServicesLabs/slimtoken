// Report writer for slimtoken-fuzz: report.md + fuzz.json + finding-N.txt.
// Judge output per the sidecar contract: one-glance markdown plus a
// machine-readable JSON twin with stable field names.
import { mkdirSync, writeFileSync } from "node:fs";
import { join, resolve } from "node:path";

// Display-safe truncation at 200 chars (codepoint-safe so 💥 never splits).
export function displayInput(input) {
  const chars = Array.from(input);
  if (chars.length <= 200) return JSON.stringify(input);
  return JSON.stringify(chars.slice(0, 200).join("")) + `…(${chars.length} chars)`;
}

export function writeReport(outDir, { target, seed, cases, seconds, keepGoing, findings }) {
  const abs = resolve(outDir);
  mkdirSync(abs, { recursive: true });

  const rows = findings.map((f, i) => {
    writeFileSync(join(abs, `finding-${i}.txt`), f.input, "utf8");
    return { error: f.error, case: f.case, inputFile: `finding-${i}.txt`, input: f.input };
  });

  const summary =
    findings.length === 0
      ? `no findings in ${cases} cases`
      : `${findings.length} finding(s) in ${cases} cases`;

  const json = {
    ok: findings.length === 0,
    cases,
    seconds,
    findings: rows.map(({ input, ...rest }) => ({
      ...rest,
      input: displayInput(input),
    })),
    summary,
  };
  writeFileSync(join(abs, "fuzz.json"), JSON.stringify(json, null, 2) + "\n", "utf8");

  const lines = [
    "# slimtoken-fuzz report",
    "",
    `- target: ${target}`,
    `- prng seed: ${seed}`,
    `- keep-going: ${keepGoing}`,
    `- cases: ${cases}`,
    `- wall seconds: ${seconds}`,
    `- findings: ${findings.length}`,
    "",
    summary,
    "",
  ];
  findings.forEach((f, i) => {
    lines.push(`## Finding ${i} (case ${f.case})`, "", `- error: ${f.error}`, `- input: ${displayInput(f.input)}`, `- full input: finding-${i}.txt`, "");
  });
  writeFileSync(join(abs, "report.md"), lines.join("\n") + "\n", "utf8");

  return abs;
}
