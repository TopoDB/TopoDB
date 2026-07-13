import { test } from "node:test";
import assert from "node:assert/strict";
import { encodeUlid, projectScopeId } from "../scope-id.js";

// Crockford base32, 26 chars, big-endian 128-bit — the encoding `ulid::Ulid`'s
// Display produces and `ScopeId::from_str` parses.
test("encodeUlid: all-zero bytes", () => {
  assert.equal(encodeUlid(new Uint8Array(16)), "00000000000000000000000000");
});

test("encodeUlid: all-ones bytes is the max ULID", () => {
  assert.equal(encodeUlid(new Uint8Array(16).fill(0xff)), "7ZZZZZZZZZZZZZZZZZZZZZZZZZ");
});

test("encodeUlid: is 26 chars of the Crockford alphabet", () => {
  const s = encodeUlid(new Uint8Array([1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16]));
  assert.equal(s.length, 26);
  assert.match(s, /^[0-9ABCDEFGHJKMNPQRSTVWXYZ]{26}$/);
});

test("encodeUlid rejects a wrong-length input", () => {
  assert.throws(() => encodeUlid(new Uint8Array(15)));
});

test("projectScopeId is deterministic and path-dependent", () => {
  const a = projectScopeId(process.cwd());
  assert.equal(a, projectScopeId(process.cwd()), "same path must give the same scope");
  assert.match(a, /^[0-9ABCDEFGHJKMNPQRSTVWXYZ]{26}$/);
  assert.notEqual(a, projectScopeId(process.cwd() + "/sub"), "different path, different scope");
});
