import { test } from "node:test";
import assert from "node:assert/strict";
import { shout } from "./src/str.mjs";

test("shout uppercases", () => {
  assert.equal(shout("hi"), "HI!");
});
