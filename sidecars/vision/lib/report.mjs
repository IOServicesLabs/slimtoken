// Write the judge material: report.md (one glance for a human or LLM) and
// vision.json (machine-readable twin with stable field names).
import { mkdirSync, writeFileSync } from "node:fs";
import { join } from "node:path";

export function writeReport(outDir, { ok, images, summary }) {
  mkdirSync(outDir, { recursive: true });

  const lines = [];
  lines.push("# Vision score report");
  lines.push("");
  lines.push(
    `Scored ${summary.scored} image(s); skipped ${summary.skipped}. ` +
      `Mean score: ${summary.meanScore === null ? "n/a" : summary.meanScore}.`,
  );
  lines.push("");
  if (images.length > 0) {
    lines.push("| File | Score | Flags | Note |");
    lines.push("|---|---|---|---|");
    for (const img of images) {
      const flags = img.flags.length > 0 ? img.flags.join(", ") : "(none)";
      const note = img.note ?? "";
      lines.push(`| ${img.file} | ${img.score} | ${flags} | ${note} |`);
    }
    lines.push("");
  }
  writeFileSync(join(outDir, "report.md"), lines.join("\n") + "\n", "utf8");

  const twin = { ok, images, summary };
  writeFileSync(join(outDir, "vision.json"), JSON.stringify(twin, null, 2) + "\n", "utf8");
}
