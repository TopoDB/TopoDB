// scripts/test/sync-plugin-core.test.mjs
import { test } from "node:test";
import assert from "node:assert/strict";
import { mkdtempSync, mkdirSync, writeFileSync, readFileSync, rmSync, existsSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";
import { syncCore, GENERATED_HEADER } from "../sync-plugin-core.mjs";

function fixture() {
  const root = mkdtempSync(path.join(tmpdir(), "sync-core-"));
  const src = path.join(root, "core");
  mkdirSync(path.join(src, "hooks"), { recursive: true });
  mkdirSync(path.join(src, "test"), { recursive: true });
  mkdirSync(path.join(src, "node_modules", "x"), { recursive: true });
  writeFileSync(path.join(src, "a.js"), "export const a = 1;\n");
  writeFileSync(path.join(src, "hooks", "b.js"), "export const b = 2;\n");
  writeFileSync(path.join(src, "test", "a.test.js"), "// not copied\n");
  writeFileSync(path.join(src, "node_modules", "x", "i.js"), "// not copied\n");
  writeFileSync(path.join(src, "package.json"), "{}\n");
  const plugin = path.join(root, "plug");
  mkdirSync(plugin);
  return { root, src, target: path.join(plugin, "core"), missingTarget: path.join(root, "nope", "core") };
}

test("copies js with header, excludes test/node_modules/package.json, writes VENDORED.json", () => {
  const f = fixture();
  try {
    const res = syncCore({ source: f.src, targets: [f.target, f.missingTarget], check: false });
    assert.deepEqual(res.drift, []);
    assert.equal(readFileSync(path.join(f.target, "a.js"), "utf8"), GENERATED_HEADER + "export const a = 1;\n");
    assert.equal(readFileSync(path.join(f.target, "hooks", "b.js"), "utf8"), GENERATED_HEADER + "export const b = 2;\n");
    assert.ok(!existsSync(path.join(f.target, "test")));
    assert.ok(!existsSync(path.join(f.target, "node_modules")));
    assert.ok(!existsSync(path.join(f.target, "package.json")));
    assert.ok(!existsSync(f.missingTarget), "targets whose plugin dir is absent are skipped");
    const v = JSON.parse(readFileSync(path.join(f.target, "VENDORED.json"), "utf8"));
    assert.equal(v.source, "plugins/core");
    assert.deepEqual(Object.keys(v.files).sort(), ["a.js", "hooks/b.js"]);
    assert.match(v.files["a.js"], /^[0-9a-f]{64}$/);
  } finally { rmSync(f.root, { recursive: true, force: true }); }
});

test("--check passes when in sync, names drifted, extra and missing files", () => {
  const f = fixture();
  try {
    syncCore({ source: f.src, targets: [f.target], check: false });
    assert.deepEqual(syncCore({ source: f.src, targets: [f.target], check: true }).drift, []);
    // edit vendored copy directly → drift
    writeFileSync(path.join(f.target, "a.js"), GENERATED_HEADER + "export const a = 99;\n");
    let d = syncCore({ source: f.src, targets: [f.target], check: true }).drift;
    assert.ok(d.some((s) => s.includes("a.js") && s.includes("drift")), d.join("\n"));
    // re-sync, then add a new source file without syncing → missing
    syncCore({ source: f.src, targets: [f.target], check: false });
    writeFileSync(path.join(f.src, "c.js"), "export const c = 3;\n");
    d = syncCore({ source: f.src, targets: [f.target], check: true }).drift;
    assert.ok(d.some((s) => s.includes("c.js") && s.includes("missing")), d.join("\n"));
    // re-sync, then leave a stray file in the target → extra
    syncCore({ source: f.src, targets: [f.target], check: false });
    writeFileSync(path.join(f.target, "stray.js"), "// stray\n");
    d = syncCore({ source: f.src, targets: [f.target], check: true }).drift;
    assert.ok(d.some((s) => s.includes("stray.js") && s.includes("extra")), d.join("\n"));
  } finally { rmSync(f.root, { recursive: true, force: true }); }
});

test("keeps a shebang on line 1 and puts the header on line 2; --check round-trips it", () => {
  const f = fixture();
  try {
    // Create a source file with a shebang
    writeFileSync(path.join(f.src, "bin.js"), "#!/usr/bin/env node\nexport const b = 1;\n");

    // Sync without --check to vendor the file
    syncCore({ source: f.src, targets: [f.target], check: false });

    // Verify vendored file has shebang on line 1, header on line 2
    const vendored = readFileSync(path.join(f.target, "bin.js"), "utf8");
    assert.equal(vendored, "#!/usr/bin/env node\n" + GENERATED_HEADER + "export const b = 1;\n");

    // Verify --check round-trips it (no drift)
    const checkResult = syncCore({ source: f.src, targets: [f.target], check: true });
    assert.deepEqual(checkResult.drift, []);
  } finally { rmSync(f.root, { recursive: true, force: true }); }
});

test("CRLF checkouts (Windows autocrlf) on either side do not read as drift", () => {
  const f = fixture();
  try {
    writeFileSync(path.join(f.src, "bin.js"), "#!/usr/bin/env node\nexport const b = 1;\n");
    syncCore({ source: f.src, targets: [f.target], check: false });
    // Simulate git rewriting BOTH trees to CRLF on checkout.
    for (const rel of ["a.js", "hooks/b.js", "bin.js"]) {
      for (const root of [f.src, f.target]) {
        const p = path.join(root, rel);
        writeFileSync(p, readFileSync(p, "utf8").replace(/\n/g, "\r\n"));
      }
    }
    assert.deepEqual(syncCore({ source: f.src, targets: [f.target], check: true }).drift, []);
    // A real content change is still caught under CRLF.
    writeFileSync(path.join(f.src, "a.js"), "export const a = 2;\r\n");
    const d = syncCore({ source: f.src, targets: [f.target], check: true }).drift;
    assert.ok(d.some((s) => s.includes("a.js") && s.includes("drift")), d.join("\n"));
  } finally { rmSync(f.root, { recursive: true, force: true }); }
});
