import { test } from "node:test";
import assert from "node:assert/strict";
import { socketPathFor, lineReader } from "../ipc.js";

test("socketPathFor is deterministic and db-specific", () => {
  const a = socketPathFor("/tmp/one/memory.redb");
  assert.equal(a, socketPathFor("/tmp/one/memory.redb"));
  assert.notEqual(a, socketPathFor("/tmp/two/memory.redb"));
});

test("socketPathFor uses the platform's IPC convention", () => {
  const p = socketPathFor("/tmp/x/memory.redb");
  if (process.platform === "win32") assert.match(p, /^\\\\[.]\\pipe\\topodb-[0-9a-f]{12}$/);
  else assert.match(p, /topodb-[0-9a-f]{12}\.sock$/);
});

test("socketPathFor stays under the unix 104-byte sun_path limit", () => {
  // A long dataDir must not produce an unbindable socket path. This is why the
  // socket lives in the OS temp dir rather than beside the database.
  const deep = "/" + "verylongdirectory/".repeat(20) + "memory.redb";
  if (process.platform !== "win32") assert.ok(Buffer.byteLength(socketPathFor(deep)) < 104);
});

test("lineReader emits complete lines and buffers partials", () => {
  const got = [];
  const feed = lineReader((l) => got.push(l));
  feed(Buffer.from('{"a":1}\n{"b":'));
  assert.deepEqual(got, ['{"a":1}']);
  feed(Buffer.from('2}\n'));
  assert.deepEqual(got, ['{"a":1}', '{"b":2}']);
});

test("lineReader handles several lines in one chunk and ignores blanks", () => {
  const got = [];
  const feed = lineReader((l) => got.push(l));
  feed(Buffer.from('{"a":1}\n\n{"b":2}\n'));
  assert.deepEqual(got, ['{"a":1}', '{"b":2}']);
});
