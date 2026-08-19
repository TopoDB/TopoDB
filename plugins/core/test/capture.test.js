import { test } from "node:test";
import assert from "node:assert/strict";
import { mkdtempSync, rmSync, readdirSync, readFileSync, existsSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";
import { captureArtifact } from "../hooks/capture.js";
import { warehouseDir } from "../warehouse-spool.js";

const spooled = (dir) => { const s = path.join(warehouseDir(dir, {}), "spool"); return existsSync(s) ? readdirSync(s).flatMap((f) => readFileSync(path.join(s, f), "utf8").split("\n").filter(Boolean).map(JSON.parse)) : []; };

test("spools an artifact tagged with the harness; kill switch and missing ids return false", () => {
  const dir = mkdtempSync(path.join(tmpdir(), "cap-"));
  try {
    const base = { dataDir: dir, env: {}, projectDir: dir, sessionId: "s", toolName: "Read", toolInput: { file_path: "/x.rs" }, toolResponse: { file: { content: "hello" } }, cwd: dir, harness: "cursor" };
    assert.equal(captureArtifact(base), true);
    const evs = spooled(dir);
    assert.equal(evs.length, 1); assert.equal(evs[0].source.harness, "cursor"); assert.equal(evs[0].artifact.content, "hello");
    assert.equal(captureArtifact({ ...base, env: { TOPODB_WAREHOUSE: "0" } }), false);
    assert.equal(captureArtifact({ ...base, sessionId: undefined }), false);
    assert.equal(captureArtifact({ ...base, toolName: "Task" }), false);
    assert.equal(spooled(dir).length, 1);
  } finally { rmSync(dir, { recursive: true, force: true }); }
});
