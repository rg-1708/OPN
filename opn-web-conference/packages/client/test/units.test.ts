import assert from "node:assert/strict";
import { test } from "node:test";
import { backoffMs, DedupeRing } from "../src/index.ts";

test("DedupeRing admits first sight, rejects repeats", () => {
  const ring = new DedupeRing(4);
  assert.equal(ring.admit("a"), true);
  assert.equal(ring.admit("a"), false);
  assert.equal(ring.admit("b"), true);
});

test("DedupeRing evicts oldest past capacity, re-admitting it", () => {
  const ring = new DedupeRing(2);
  ring.admit("a");
  ring.admit("b");
  ring.admit("c"); // evicts "a"
  assert.equal(ring.admit("a"), true, "evicted id is seen as new again");
  assert.equal(ring.admit("c"), false, "recent id still deduped");
});

test("backoffMs stays within [0, max)", () => {
  assert.equal(backoffMs(3000, () => 0), 0);
  assert.equal(backoffMs(3000, () => 0.999999), 2999);
  assert.equal(backoffMs(0, () => 0.5), 0);
});
