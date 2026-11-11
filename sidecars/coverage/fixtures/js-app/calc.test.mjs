import { test } from "node:test";
import assert from "node:assert/strict";
import { add } from "./src/calc.mjs";

test("add small", () => {
  assert.equal(add(2, 2), 4);
});

test("add negative", () => {
  assert.equal(add(-1, 1), 0);
});
