// test/recorder-integration.test.ts
//
// End-to-end proof against the REAL branch-built Rust server (not the
// published npm binary, which lacks `create_node`): one episode, recorded
// via the recorder core, submitted as a single atomic batch, then read back
// through `traverse` and `find_by_prop` to confirm the on-disk graph shape
// and that the index spec was actually applied.
//
// Gated on `target/debug/topodb-mcp(.exe)` existing (prerequisite: run
// `cargo build -p topodb-mcp` from the repo root). Skips rather than fails
// when the binary hasn't been built, mirroring how the rest of this suite
// skips/gates on optional preconditions instead of hard-failing.
import { test } from "node:test";
import assert from "node:assert/strict";
import { existsSync, mkdtempSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, dirname } from "node:path";
import { fileURLToPath } from "node:url";
import { McpStdioClient } from "../src/mcp-client.ts";
import { EpisodeBuffer, buildEpisodeBatch, toRetrievalRecord } from "../src/recorder.ts";

const here = dirname(fileURLToPath(import.meta.url)); // .../npm/topodb-pi/test
const packageRoot = join(here, ".."); // .../npm/topodb-pi
const repoRoot = join(packageRoot, "..", ".."); // repo root
const binaryName = process.platform === "win32" ? "topodb-mcp.exe" : "topodb-mcp";
const binaryPath = join(repoRoot, "target", "debug", binaryName);
const specPath = join(packageRoot, "spec", "episode-index-spec.json");

const db = () => join(mkdtempSync(join(tmpdir(), "topodb-pi-recorder-it-")), "e.redb");

test(
  "episode recorder: create_memory -> search -> submit_batch -> traverse/find_by_prop against the real branch server",
  { skip: existsSync(binaryPath) ? false : `binary not built: ${binaryPath} (run: cargo build -p topodb-mcp)` },
  async () => {
    const c = new McpStdioClient(["--db", db(), "--spec", specPath], { command: binaryPath });
    await c.start();
    try {
      // 1. Seed the one memory this episode will retrieve.
      const created: any = await c.callTool("create_memory", { content: "rust ownership rules" });
      const memoryId: string = created.id;
      assert.equal(typeof memoryId, "string");

      // 2. Drive a real search_memories call and let the recorder's own wire
      // adapter turn the result into a RetrievalRecord — no fabricated shape.
      const searchResult = await c.callTool("search_memories", { query: "ownership", k: 5 });
      const captured = toRetrievalRecord("search_memories", { query: "ownership" }, searchResult);
      assert.ok(captured, "toRetrievalRecord should recognize a search_memories result");
      assert.deepEqual(
        captured.record.returned.map((r) => ({ id: r.id, rank: r.rank })),
        [{ id: memoryId, rank: 0 }],
        "the single seeded memory should come back at rank 0",
      );

      // 3. Assemble the episode buffer and submit the whole thing atomically.
      const buffer = new EpisodeBuffer();
      buffer.start(1_000);
      buffer.bumpTurns();
      buffer.addRetrieval(captured.record);
      buffer.close();

      const commands = buildEpisodeBatch({
        buffer,
        goal: "learn rust ownership",
        outcome: "success",
        failure: "",
        endedAt: 2_000,
        tokens: 42,
        used: new Map([[0, new Set([memoryId])]]),
      });

      const batchResult: any = await c.callTool("submit_batch", { commands });
      const episodeId: string = batchResult.ids[0];
      assert.equal(typeof episodeId, "string", "submit_batch should return the Episode id first");

      // 4. Read back the graph shape via traverse: Episode -ISSUED-> RetrievalEvent,
      // RetrievalEvent -RETURNED(rank:0)-> Memory, RetrievalEvent -USED-> Memory.
      const traverseResult: any = await c.callTool("traverse", {
        seed_id: episodeId,
        max_hops: 2,
        direction: "out",
      });
      const { nodes, edges } = traverseResult.subgraph;

      const retrievalEvent = nodes.find((n: any) => n.label === "RetrievalEvent");
      assert.ok(retrievalEvent, "subgraph should contain the RetrievalEvent node");

      const issued = edges.find(
        (e: any) => e.from === episodeId && e.to === retrievalEvent.id && e.type === "ISSUED",
      );
      assert.ok(issued, "Episode should be ISSUED-linked to the RetrievalEvent");

      const returned = edges.find(
        (e: any) => e.from === retrievalEvent.id && e.to === memoryId && e.type === "RETURNED",
      );
      assert.ok(returned, "RetrievalEvent should be RETURNED-linked to the memory");
      assert.equal(returned.props.rank, 0, "RETURNED edge should carry rank:0");

      const used = edges.find(
        (e: any) => e.from === retrievalEvent.id && e.to === memoryId && e.type === "USED",
      );
      assert.ok(used, "RetrievalEvent should be USED-linked to the memory");

      // 5. find_by_prop on Episode.outcome proves the index spec (equality on
      // Episode.outcome) was actually applied by the server, not just that
      // submit_batch happened to succeed.
      const byOutcome: any = await c.callTool("find_by_prop", {
        label: "Episode",
        prop: "outcome",
        value: "success",
      });
      assert.ok(
        byOutcome.nodes.some((n: any) => n.id === episodeId),
        "find_by_prop(Episode.outcome=success) should find the episode we just wrote",
      );
    } finally {
      c.stop();
    }
  },
);
