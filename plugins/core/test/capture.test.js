import { test } from "node:test";
import assert from "node:assert/strict";
import { mkdtempSync, rmSync, readdirSync, readFileSync, existsSync, statSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";
import { captureArtifact } from "../hooks/capture.js";
import { warehouseDir, spoolCapBytes, spoolBytes, DEFAULT_SPOOL_MAX_MB, appendSpool } from "../warehouse-spool.js";

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

test("spoolCapBytes: default 64 MB, 0 = unlimited, invalid → default; spoolBytes totals every file under spool/", () => {
  const MB = 1024 * 1024;
  assert.equal(DEFAULT_SPOOL_MAX_MB, 64);
  assert.equal(spoolCapBytes({}), 64 * MB);
  assert.equal(spoolCapBytes({ TOPODB_WAREHOUSE_SPOOL_MAX_MB: " " }), 64 * MB);
  assert.equal(spoolCapBytes({ TOPODB_WAREHOUSE_SPOOL_MAX_MB: "0" }), 0);
  assert.equal(spoolCapBytes({ TOPODB_WAREHOUSE_SPOOL_MAX_MB: "8" }), 8 * MB);
  assert.equal(spoolCapBytes({ TOPODB_WAREHOUSE_SPOOL_MAX_MB: "lots" }), 64 * MB);
  assert.equal(spoolCapBytes({ TOPODB_WAREHOUSE_SPOOL_MAX_MB: "-1" }), 64 * MB);
  const dir = mkdtempSync(path.join(tmpdir(), "cap-"));
  try {
    assert.equal(spoolBytes(dir, {}), 0);
    appendSpool(dir, "s", { a: 1 }, {});
    appendSpool(dir, "t", { b: 22 }, {});
    const line = (o) => Buffer.byteLength(JSON.stringify(o) + "\n");
    assert.equal(spoolBytes(dir, {}), line({ a: 1 }) + line({ b: 22 }));

    appendSpool(dir, "u", { c: 333 }, {});
    const spoolDir = path.join(warehouseDir(dir, {}), "spool");
    const files = readdirSync(spoolDir).sort();
    const firstSize = statSync(path.join(spoolDir, files[0])).size;
    const trueTotal = spoolBytes(dir, {});
    const limited = spoolBytes(dir, {}, firstSize);
    assert.ok(limited >= firstSize);
    assert.ok(limited <= trueTotal);
  } finally { rmSync(dir, { recursive: true, force: true }); }
});

test("spool cap: artifacts are dropped at/over cap, one stderr line per episode via the spool-capped sentinel, a drain resets", () => {
  const dir = mkdtempSync(path.join(tmpdir(), "cap-"));
  const errors = []; const orig = console.error; console.error = (m) => errors.push(String(m));
  try {
    const env = { TOPODB_WAREHOUSE_SPOOL_MAX_MB: "0.0001" }; // 104 bytes: any one event is over
    const base = { dataDir: dir, env, projectDir: dir, sessionId: "s", toolName: "Read", toolInput: { file_path: "/x.rs" }, toolResponse: "hello world, comfortably longer than the cap once wrapped in an event", cwd: dir, harness: "cursor" };
    const sentinel = path.join(warehouseDir(dir, {}), "spool-capped");
    const capLogs = () => errors.filter((e) => e.startsWith("topodb warehouse: spool cap")).length;
    assert.equal(captureArtifact(base), true); // empty spool: lands
    assert.equal(captureArtifact(base), false); // over cap: dropped
    assert.equal(captureArtifact(base), false);
    assert.equal(spooled(dir).length, 1);
    assert.equal(capLogs(), 1);
    assert.ok(existsSync(sentinel));
    rmSync(path.join(warehouseDir(dir, {}), "spool"), { recursive: true, force: true }); // a drain emptied it
    assert.equal(captureArtifact(base), true);
    assert.equal(existsSync(sentinel), false);
    assert.equal(captureArtifact(base), false);
    assert.equal(capLogs(), 2);
    assert.equal(captureArtifact({ ...base, env: { TOPODB_WAREHOUSE_SPOOL_MAX_MB: "0" } }), true); // unlimited
  } finally { console.error = orig; rmSync(dir, { recursive: true, force: true }); }
});
