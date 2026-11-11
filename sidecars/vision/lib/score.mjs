// score command: judge every screenshot in a directory tree against a
// host rubric, one vision call per image, budget-capped.
//
// Exit codes:
//   0 at least one image scored
//   1 zero images scored (the run judged nothing)
//   2 usage error, missing env, rubric unreadable, no images found
import { readFileSync, statSync } from "node:fs";
import { collectPngs, isPng } from "./collect.mjs";
import { judgeImage } from "./judge.mjs";
import { writeReport } from "./report.mjs";

const USAGE =
  "usage: slimtoken-vision score <shots-dir> --rubric <file> " +
  "[--out DIR] [--max-images N] [--max-image-bytes N]";

function fail(message) {
  console.error(`slimtoken-vision: ${message}`);
  console.error(USAGE);
  return 2;
}

function parseCap(value, flag) {
  const n = Number.parseInt(value, 10);
  if (!Number.isFinite(n) || n < 1) {
    console.error(`slimtoken-vision: ${flag} needs a positive integer (got '${value}')`);
    return null;
  }
  return n;
}

export async function runScore(args) {
  let shotsDir = null;
  let rubricPath = null;
  let outDir = null;
  let maxImages = 20;
  let maxImageBytes = 1500000;

  for (let i = 0; i < args.length; i += 1) {
    const a = args[i];
    const eq = a.indexOf("=");
    const flag = eq === -1 ? a : a.slice(0, eq);
    const inline = eq === -1 ? null : a.slice(eq + 1);
    const next = () => (inline !== null ? inline : args[(i += 1)]);
    switch (flag) {
      case "--rubric":
        rubricPath = next();
        break;
      case "--out":
        outDir = next();
        break;
      case "--max-images":
        maxImages = parseCap(next(), "--max-images");
        if (maxImages === null) return 2;
        break;
      case "--max-image-bytes":
        maxImageBytes = parseCap(next(), "--max-image-bytes");
        if (maxImageBytes === null) return 2;
        break;
      default:
        if (flag.startsWith("--")) return fail(`unknown option '${a}'`);
        if (shotsDir !== null) return fail(`unexpected argument '${a}'`);
        shotsDir = a;
    }
  }
  if (shotsDir === null) return fail("missing <shots-dir>");
  if (rubricPath === null || rubricPath === undefined) return fail("missing --rubric <file>");
  if (outDir === undefined) outDir = null;

  // Keys come only from the environment. Missing any is a usage error
  // that names the variable.
  for (const name of ["VISION_BASE_URL", "VISION_API_KEY", "VISION_MODEL"]) {
    if (!process.env[name] || String(process.env[name]).trim() === "") {
      console.error(`slimtoken-vision: missing environment variable ${name}`);
      console.error(USAGE);
      return 2;
    }
  }

  let rubric;
  try {
    rubric = readFileSync(rubricPath, "utf8");
  } catch {
    console.error(`slimtoken-vision: rubric unreadable: ${rubricPath}`);
    console.error(USAGE);
    return 2;
  }
  if (rubric.trim().length === 0) {
    console.error(`slimtoken-vision: rubric is empty: ${rubricPath}`);
    console.error(USAGE);
    return 2;
  }

  let files;
  try {
    if (!statSync(shotsDir).isDirectory()) throw new Error("not a directory");
    files = collectPngs(shotsDir);
  } catch {
    console.error(`slimtoken-vision: cannot read shots dir: ${shotsDir}`);
    console.error(USAGE);
    return 2;
  }
  if (files.length === 0) {
    console.error(`slimtoken-vision: no PNG images found in ${shotsDir}`);
    console.error(USAGE);
    return 2;
  }

  // Budget cap: judge at most --max-images, and say so out loud.
  let skipped = 0;
  if (files.length > maxImages) {
    const over = files.length - maxImages;
    console.log(`WARN cap: ${over} image(s) over --max-images ${maxImages} not judged`);
    files = files.slice(0, maxImages);
    skipped += over;
  }

  const images = [];
  for (const file of files) {
    let buf;
    try {
      buf = readFileSync(file);
    } catch (err) {
      console.log(`WARN SKIP ${file}: unreadable (${err.message || err})`);
      skipped += 1;
      continue;
    }
    if (buf.length > maxImageBytes) {
      console.log(`WARN SKIP ${file}: ${buf.length} bytes over --max-image-bytes ${maxImageBytes}`);
      skipped += 1;
      continue;
    }
    if (!isPng(buf)) {
      console.log(`WARN SKIP ${file}: not a valid PNG`);
      skipped += 1;
      continue;
    }
    const verdict = await judgeImage({
      baseUrl: process.env.VISION_BASE_URL,
      apiKey: process.env.VISION_API_KEY,
      model: process.env.VISION_MODEL,
      rubric,
      imageB64: buf.toString("base64"),
    });
    if (!verdict.ok) {
      console.log(`WARN SKIP ${file}: ${verdict.why}`);
      skipped += 1;
      continue;
    }
    console.log(`OK ${file} score=${verdict.score} flags=${verdict.flags.length}`);
    const entry = { file, score: verdict.score, flags: verdict.flags };
    if (verdict.note !== undefined) entry.note = verdict.note;
    images.push(entry);
  }

  const scored = images.length;
  const meanScore =
    scored > 0
      ? Math.round((images.reduce((sum, img) => sum + img.score, 0) / scored) * 100) / 100
      : null;
  console.log(
    `summary: scored ${scored}, skipped ${skipped}, mean score ${meanScore === null ? "n/a" : meanScore}`,
  );

  if (outDir !== null) {
    writeReport(outDir, { ok: scored > 0, images, summary: { scored, skipped, meanScore } });
    console.log(`report: ${outDir}`);
  }

  return scored > 0 ? 0 : 1;
}
