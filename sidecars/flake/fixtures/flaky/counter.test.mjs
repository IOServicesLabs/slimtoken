// counter.test.mjs — fixture for the slimtoken-flake selftest.
//
// The test reads and writes a counter FILE next to this file. It fails
// when the counter is 0 and passes when the counter is 1 or more. So
// the first probe run fails and the rest pass: a known flake.
//
// The selftest must reset the counter (delete .counter) before each
// scenario that uses this fixture. Without a reset, later runs see a
// high counter and the test passes every run.
import assert from "node:assert/strict";
import { existsSync, readFileSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { test } from "node:test";
import { fileURLToPath } from "node:url";

const counterFile = join(dirname(fileURLToPath(import.meta.url)), ".counter");

test("flaky counter", () => {
  const count = existsSync(counterFile) ? Number(readFileSync(counterFile, "utf8")) || 0 : 0;
  writeFileSync(counterFile, String(count + 1));
  assert.notEqual(count, 0, "counter is 0 on the first run: simulated flake");
});
