import { test } from "node:test";
import assert from "node:assert/strict";
import { mkdtempSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { hashArtifacts, ensurePolicyVersion } from "../src/policy.ts";

test("hashArtifacts: stable sha256 per file, sorted by path", () => {
  const dir = mkdtempSync(join(tmpdir(), "pol-"));
  writeFileSync(join(dir, "b.md"), "skill b");
  writeFileSync(join(dir, "a.md"), "prompt a");
  const arts = hashArtifacts([join(dir, "b.md"), join(dir, "a.md")]);
  assert.equal(arts.length, 2);
  assert.ok(arts[0].path.endsWith("a.md")); // sorted
  assert.match(arts[0].sha256, /^[0-9a-f]{64}$/);
  const again = hashArtifacts([join(dir, "a.md"), join(dir, "b.md")]);
  assert.deepEqual(arts, again); // order-insensitive input, stable output
});

test("ensurePolicyVersion: existing matching version is reused, nothing written", () => {
  const dir = mkdtempSync(join(tmpdir(), "pol-"));
  writeFileSync(join(dir, "a.md"), "prompt a");
  const arts = hashArtifacts([join(dir, "a.md")]);
  const calls: string[] = [];
  const fake = async (tool: string, args: Record<string, unknown>) => {
    calls.push(tool);
    if (tool === "find_by_prop" && args.label === "Artifact")
      return { nodes: [{ id: "01ART" }] };
    if (tool === "traverse")
      // inbound INCLUDES from the artifact: one candidate version whose
      // artifact set matches exactly
      return { subgraph: { nodes: [
        { id: "01VER", label: "PolicyVersion", props: { version: 1 } },
        { id: "01ART", label: "Artifact", props: { sha256: arts[0].sha256 } },
      ], edges: [ { from: "01VER", to: "01ART", ty: "INCLUDES" } ] } };
    throw new Error(`unexpected tool ${tool}`);
  };
  return ensurePolicyVersion(fake, [join(dir, "a.md")]).then((id) => {
    assert.equal(id, "01VER");
    assert.ok(!calls.includes("submit_batch")); // reuse, no writes
  });
});

test("ensurePolicyVersion: returns undefined instead of throwing", async () => {
  const boom = async () => { throw new Error("db down"); };
  assert.equal(await ensurePolicyVersion(boom, ["/nope/missing.md"]), undefined);
});

test("ensurePolicyVersion: rejected call after successful hashing returns undefined", async () => {
  const dir = mkdtempSync(join(tmpdir(), "pol-"));
  writeFileSync(join(dir, "a.md"), "prompt a");
  const boom = async () => { throw new Error("db down"); };
  assert.equal(await ensurePolicyVersion(boom, [join(dir, "a.md")]), undefined);
});

test("ensurePolicyVersion: duplicate-content files are deduped in the create batch", async () => {
  const dir = mkdtempSync(join(tmpdir(), "pol-"));
  writeFileSync(join(dir, "a.md"), "same content");
  writeFileSync(join(dir, "b.md"), "same content");
  let batch: Array<Record<string, unknown>> | undefined;
  const fake = async (tool: string, args: Record<string, unknown>) => {
    if (tool === "find_by_prop") return { nodes: [] }; // nothing exists yet
    if (tool === "submit_batch") {
      batch = args.commands as Array<Record<string, unknown>>;
      return { ids: batch.map((_, i) => `01ID${i}`) };
    }
    throw new Error(`unexpected tool ${tool}`);
  };
  const id = await ensurePolicyVersion(fake, [join(dir, "a.md"), join(dir, "b.md")]);
  assert.ok(batch, "submit_batch was called");
  const artCreates = batch!.filter(
    (c) => c.op === "create_node" && c.label === "Artifact",
  );
  const includes = batch!.filter((c) => c.op === "link" && c.type === "INCLUDES");
  assert.equal(artCreates.length, 1); // ONE Artifact for identical content
  assert.equal(includes.length, 1); // ONE INCLUDES link
  assert.equal(id, "01ID1"); // PolicyVersion is the second command
});

test("ensurePolicyVersion: duplicate-content files still reuse an existing version", () => {
  const dir = mkdtempSync(join(tmpdir(), "pol-"));
  writeFileSync(join(dir, "a.md"), "same content");
  writeFileSync(join(dir, "b.md"), "same content");
  const arts = hashArtifacts([join(dir, "a.md")]);
  const calls: string[] = [];
  const fake = async (tool: string, args: Record<string, unknown>) => {
    calls.push(tool);
    if (tool === "find_by_prop" && args.label === "Artifact")
      return { nodes: [{ id: "01ART" }] };
    if (tool === "traverse")
      return { subgraph: { nodes: [
        { id: "01VER", label: "PolicyVersion", props: { version: 1 } },
        { id: "01ART", label: "Artifact", props: { sha256: arts[0].sha256 } },
      ], edges: [ { from: "01VER", to: "01ART", ty: "INCLUDES" } ] } };
    throw new Error(`unexpected tool ${tool}`);
  };
  return ensurePolicyVersion(fake, [join(dir, "a.md"), join(dir, "b.md")]).then((id) => {
    assert.equal(id, "01VER");
    assert.ok(!calls.includes("submit_batch")); // reuse, no writes
  });
});
