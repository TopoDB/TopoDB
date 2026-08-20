import { test } from "node:test";
import assert from "node:assert/strict";
import { mkdtempSync, rmSync, readFileSync, existsSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";
import { offSwitch, recordingDisabled, parseJson, debugDump } from "../hook-io.js";

test("offSwitch and recordingDisabled", () => {
  assert.equal(offSwitch("0"), true); assert.equal(offSwitch("OFF"), true);
  assert.equal(offSwitch("1"), false); assert.equal(offSwitch(undefined), false);
  assert.equal(recordingDisabled({ TOPODB_RECORDING: "off" }), true);
  assert.equal(recordingDisabled({}), false);
});
test("parseJson returns null on garbage", () => {
  assert.deepEqual(parseJson('{"a":1}'), { a: 1 });
  assert.equal(parseJson("nope"), null);
  assert.equal(parseJson(""), null);
  assert.equal(parseJson("[1]"), null);
});
test("debugDump writes only when TOPODB_HOOK_DEBUG is set, sanitizes the name, never throws", () => {
  const dir = mkdtempSync(path.join(tmpdir(), "hookio-"));
  try {
    debugDump({ dataDir: dir, env: {}, eventName: "stop", raw: "{}" });
    assert.ok(!existsSync(path.join(dir, "episodes", "debug-stop.json")));
    debugDump({ dataDir: dir, env: { TOPODB_HOOK_DEBUG: "1" }, eventName: "after/MCP Execution", raw: '{"x":1}' });
    assert.equal(readFileSync(path.join(dir, "episodes", "debug-after_MCP_Execution.json"), "utf8"), '{"x":1}');
    debugDump({ dataDir: path.join(dir, "nope", "deeper"), env: { TOPODB_HOOK_DEBUG: "1" }, eventName: "x", raw: "" }); // creates dirs, no throw
    debugDump({ dataDir: null, env: { TOPODB_HOOK_DEBUG: "1" }, eventName: "x", raw: "" }); // no dataDir, no throw
  } finally { rmSync(dir, { recursive: true, force: true }); }
});
test("debugDump also triggers on a HOOK_DEBUG marker file in the data dir (no env/relaunch needed)", () => {
  const dir = mkdtempSync(path.join(tmpdir(), "hookio-"));
  try {
    debugDump({ dataDir: dir, env: {}, eventName: "stop", raw: "{}" });
    assert.ok(!existsSync(path.join(dir, "episodes", "debug-stop.json")), "no marker, no env → no dump");
    writeFileSync(path.join(dir, "HOOK_DEBUG"), "");
    debugDump({ dataDir: dir, env: {}, eventName: "stop", raw: '{"s":1}' });
    assert.equal(readFileSync(path.join(dir, "episodes", "debug-stop.json"), "utf8"), '{"s":1}');
  } finally { rmSync(dir, { recursive: true, force: true }); }
});
