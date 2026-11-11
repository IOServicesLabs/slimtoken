// Clean fuzz target for the slimtoken-fuzz selftest: never throws. It
// exercises the input (JSON.parse) but swallows every failure.
export function fuzz(input) {
  try {
    return JSON.parse(input);
  } catch {
    return null;
  }
}
