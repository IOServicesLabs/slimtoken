// Always-failing node:test fixture for the slimtoken-flake selftest.
// Every run fails, so the probe must call this BROKEN, not FLAKY.
import assert from "node:assert/strict";
import { test } from "node:test";

test("broken always fails", () => {
  assert.equal(1, 2, "this test fails in every run");
});
