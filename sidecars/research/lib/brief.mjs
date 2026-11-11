// Plan builder for the SlimToken engine's research pack.
//
// `brief` reads a questions file (one research sub-question per line;
// blank lines and # comments are ignored) and writes an engine plan
// JSON: { tasks: [{ id: "research-<n>", prompt }] }. One researcher task
// per question. The prompt template demands citation discipline, which
// is what the zero-token verify gate later checks mechanically.
//
// Contract (see sidecars/README.md):
//   - exit 0: plan written
//   - exit 2: usage error (missing --out, missing/unreadable questions file)
import { readFileSync, writeFileSync } from "node:fs";

export function researcherPrompt(question) {
  return [
    `You are a researcher. Research this sub-question and no other:`,
    ``,
    `"${question}"`,
    ``,
    `Rules:`,
    `1. Search for and read primary sources. Open every source yourself.`,
    `2. Never cite a source you did not open.`,
    `3. Write a 10-line brief as your task output. Nothing else.`,
    `4. Every load-bearing sentence carries an inline citation in the`,
    `   form [claim text](url), where the URL is a source you opened.`,
    `5. Mark a claim (consensus) when two or more independent sources`,
    `   agree on it, and cite each of those sources.`,
    `6. Mark a citation (paraphrase) when you restate the source in your`,
    `   own words instead of quoting it.`,
    `7. If a sentence is only your opinion and has no source, mark it`,
    `   (opinion) and leave it uncited.`,
  ].join("\n");
}

export async function runBrief(args) {
  const parsed = parseArgs(args);
  if (parsed.error) {
    console.error(`brief: ${parsed.error}`);
    return 2;
  }
  const { questionsFile, outFile } = parsed;

  let text;
  try {
    text = readFileSync(questionsFile, "utf8");
  } catch (error) {
    console.error(`brief: cannot read questions file '${questionsFile}': ${error.message}`);
    return 2;
  }

  const questions = text
    .split(/\r?\n/)
    .map((line) => line.trim())
    .filter((line) => line.length > 0 && !line.startsWith("#"));
  if (questions.length === 0) {
    console.error(`brief: no research questions in '${questionsFile}' (one per line; # comments ignored)`);
    return 2;
  }

  const plan = {
    tasks: questions.map((question, i) => ({
      id: `research-${i + 1}`,
      prompt: researcherPrompt(question),
    })),
  };

  try {
    writeFileSync(outFile, JSON.stringify(plan, null, 2) + "\n", "utf8");
  } catch (error) {
    console.error(`brief: cannot write plan '${outFile}': ${error.message}`);
    return 2;
  }
  console.log(`brief: ${plan.tasks.length} researcher task(s) -> ${outFile}`);
  return 0;
}

function parseArgs(args) {
  let questionsFile = null;
  let outFile = null;
  for (let i = 0; i < args.length; i += 1) {
    const arg = args[i];
    if (arg === "--out") {
      if (i + 1 >= args.length) return { error: "--out needs a file path" };
      outFile = args[i + 1];
      i += 1;
    } else if (arg.startsWith("--out=")) {
      outFile = arg.slice("--out=".length);
    } else if (arg.startsWith("--")) {
      return { error: `unknown flag '${arg}'` };
    } else if (questionsFile === null) {
      questionsFile = arg;
    } else {
      return { error: `unexpected extra argument '${arg}'` };
    }
  }
  if (questionsFile === null) {
    return { error: "no questions file (usage: slimtoken-research brief <questions-file> --out plan.json)" };
  }
  if (outFile === null || outFile.length === 0) {
    return { error: "missing --out (usage: slimtoken-research brief <questions-file> --out plan.json)" };
  }
  return { questionsFile, outFile };
}
