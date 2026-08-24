import { test } from "node:test";
import assert from "node:assert/strict";
import { mkdtempSync, rmSync, readdirSync, readFileSync, existsSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";
import { nearestTopodbToml, parseWarehouseToml, resolveWarehouse, warehouseDir, warehouseOff, tryMarker } from "../warehouse-spool.js";
import { captureArtifact } from "../hooks/capture.js";
import { sessionScopes } from "../server-args.js";

// Keys are written POSIX-style; the code under test builds candidates with
// path.join, so compare normalized forms (Windows CI).
const fakeIo = (files) => {
  const norm = (p) => path.normalize(p);
  const map = new Map(Object.entries(files).map(([k, v]) => [norm(k), v]));
  return { existsFile: (p) => map.has(norm(p)), readFile: (p) => { if (!map.has(norm(p))) throw new Error("ENOENT"); return map.get(norm(p)); } };
};
const spooled = (dir) => { const s = path.join(dir, "spool"); return existsSync(s) ? readdirSync(s).flatMap((f) => readFileSync(path.join(s, f), "utf8").split("\n").filter(Boolean).map(JSON.parse)) : []; };

test("nearestTopodbToml walks up from the db's directory without resolving relative paths", () => {
  const io = fakeIo({ "/r/.topodb.toml": "", "/r/a/b/.topodb.toml": "" });
  assert.equal(nearestTopodbToml("/r/a/b/c/memory.redb", io), path.join("/r/a/b", ".topodb.toml"));
  assert.equal(nearestTopodbToml("/r/a/memory.redb", io), path.join("/r", ".topodb.toml"));
  assert.equal(nearestTopodbToml("/elsewhere/memory.redb", io), undefined);
  const rel = fakeIo({ ".topodb.toml": "" });
  assert.equal(nearestTopodbToml(".topodb/memory.redb", rel), ".topodb.toml");
  assert.equal(nearestTopodbToml("memory.redb", rel), ".topodb.toml");
});

test("parseWarehouseToml reads only [warehouse] enabled/path and tolerates the rest", () => {
  assert.deepEqual(parseWarehouseToml(""), { enabled: true });
  assert.deepEqual(parseWarehouseToml("[schedule.warehouse_drain]\nenabled = false\n"), { enabled: true });
  assert.deepEqual(parseWarehouseToml("# c\n[warehouse]\nenabled = false # trailing\npath = \"wh\"\nsegment_mb = 64\n[[reingest.source]]\npath = \"nope\"\n"), { enabled: false, path: "wh" });
  assert.deepEqual(parseWarehouseToml("[warehouse]\npath = ''\n"), { enabled: true });
  assert.deepEqual(parseWarehouseToml("[warehouse]\npath = '~/w'\nenabled = maybe\n"), { enabled: true, path: "~/w" });
  assert.deepEqual(parseWarehouseToml("[ warehouse ]\r\nenabled=true\r\n"), { enabled: true });
  assert.deepEqual(parseWarehouseToml("[warehouse]\npath = \"a # not a comment\"\n"), { enabled: true, path: "a # not a comment" });
  assert.deepEqual(parseWarehouseToml("[warehouse]\npath = \"wh\" # trailing comment\n"), { enabled: true, path: "wh" });
});

test("resolveWarehouse mirrors the Rust precedence", () => {
  const db = "/p/sub/memory.redb";
  const none = fakeIo({});
  assert.deepEqual(resolveWarehouse(db, {}, none), { enabled: true, dir: "/p/sub/memory.warehouse", source: "default" });
  assert.deepEqual(resolveWarehouse(db, { TOPODB_WAREHOUSE_DIR: "/w " }, none), { enabled: true, dir: "/w ", source: "env" });
  assert.equal(resolveWarehouse(db, { TOPODB_WAREHOUSE: " OFF " }, none).enabled, false);
  const rel = fakeIo({ "/p/.topodb.toml": "[warehouse]\npath = \"wh\"\n" });
  assert.deepEqual(resolveWarehouse(db, {}, rel), { enabled: true, dir: path.join("/p", "wh"), source: "toml" });
  assert.deepEqual(resolveWarehouse(db, { TOPODB_WAREHOUSE_DIR: "/env" }, rel), { enabled: true, dir: "/env", source: "env" });
  const abs = fakeIo({ "/p/.topodb.toml": "[warehouse]\npath = \"/abs/wh\"\n" });
  assert.equal(resolveWarehouse(db, {}, abs).dir, "/abs/wh");
  const home = fakeIo({ "/p/.topodb.toml": "[warehouse]\npath = \"~/wh\"\n" });
  const homeKey = process.platform === "win32" ? "USERPROFILE" : "HOME";
  assert.equal(resolveWarehouse(db, { [homeKey]: "/home/u" }, home).dir, path.join("/home/u", "wh"));
  assert.equal(resolveWarehouse(db, { [homeKey]: "" }, home).dir, path.join("/p", "wh"));
  assert.equal(resolveWarehouse(db, {}, home).dir, path.join("/p", "~/wh"));
  const off = fakeIo({ "/p/.topodb.toml": "[warehouse]\nenabled = false\n" });
  assert.deepEqual(resolveWarehouse(db, { TOPODB_WAREHOUSE: "1", TOPODB_WAREHOUSE_DIR: "/env" }, off), { enabled: false, dir: "/env", source: "env" });
  const broken = { existsFile: () => true, readFile: () => { throw new Error("EACCES"); } };
  assert.deepEqual(resolveWarehouse(db, {}, broken), { enabled: true, dir: "/p/sub/memory.warehouse", source: "default" });
});

test("warehouseDir/warehouseOff resolve from <dataDir>/memory.redb: a toml in the data dir redirects or disables the hooks", () => {
  const dataDir = mkdtempSync(path.join(tmpdir(), "wh-toml-"));
  try {
    assert.equal(warehouseDir(dataDir, {}), path.join(dataDir, "memory.warehouse"));
    writeFileSync(path.join(dataDir, ".topodb.toml"), "[warehouse]\npath = \"wh\"\n");
    assert.equal(warehouseDir(dataDir, {}), path.join(dataDir, "wh"));
    assert.equal(warehouseDir(dataDir, { TOPODB_WAREHOUSE_DIR: "/env" }), "/env");
    assert.equal(warehouseOff(dataDir, {}), false);
    const base = { dataDir, env: {}, projectDir: dataDir, sessionId: "s", toolName: "Read", toolInput: { file_path: "/x.rs" }, toolResponse: "hello", cwd: dataDir, harness: "claude-code" };
    assert.equal(captureArtifact(base), true);
    tryMarker({ dataDir, env: {}, projectDir: dataDir, sessionId: "s", type: "session_end", sessionScopes, harness: "claude-code" });
    assert.deepEqual(spooled(path.join(dataDir, "wh")).map((e) => e.kind), ["artifact", "marker"]);
    assert.equal(existsSync(path.join(dataDir, "memory.warehouse")), false);
    writeFileSync(path.join(dataDir, ".topodb.toml"), "[warehouse]\nenabled = false\n");
    assert.equal(warehouseOff(dataDir, {}), true);
    assert.equal(warehouseOff(dataDir, { TOPODB_WAREHOUSE: "1" }), true); // env cannot re-enable
    assert.equal(captureArtifact({ ...base, sessionId: "s2" }), false);
    tryMarker({ dataDir, env: {}, projectDir: dataDir, sessionId: "s2", type: "session_end", sessionScopes, harness: "claude-code" });
    assert.equal(spooled(path.join(dataDir, "memory.warehouse")).length, 0);
    assert.equal(spooled(path.join(dataDir, "wh")).filter((e) => (e.source?.session ?? e.marker?.session) === "s2").length, 0);
  } finally { rmSync(dataDir, { recursive: true, force: true }); }
});
