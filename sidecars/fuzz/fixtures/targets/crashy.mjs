// Crashy fuzz target for the slimtoken-fuzz selftest. Plants its crash on
// the "💥" boundary token, which the mutator's alphabet always includes.
export function fuzz(input) {
  if (input.includes("💥")) throw new Error("boom at " + input.length);
}
