import { test } from "node:test";
import assert from "node:assert/strict";
import { recordingEnabled, episodeSpecArgs } from "../src/server-handle.ts";

test("recording defaults on, TOPODB_RECORD=0 disables", () => {
  assert.equal(recordingEnabled({}), true);
  assert.equal(recordingEnabled({ TOPODB_RECORD: "0" }), false);
  assert.equal(recordingEnabled({ TOPODB_RECORD: "1" }), true);
});

test("episodeSpecArgs adds --spec pointing at the bundled file when recording", () => {
  const on = episodeSpecArgs({});
  assert.equal(on[0], "--spec");
  assert.match(on[1], /episode-index-spec\.json$/);
  assert.deepEqual(episodeSpecArgs({ TOPODB_RECORD: "0" }), []);
});
