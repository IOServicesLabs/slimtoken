// Plain passing node:test fixture for the slimtoken-flake selftest.
import assert from "node:assert/strict";
import { test } from "node:test";

test("stable always passes", () => {
  assert.equal(1 + 1, 2);
});
