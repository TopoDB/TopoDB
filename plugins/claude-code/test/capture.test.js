// post-tool-use accumulates retrievals into the state file; session-end
// builds and submits the Episode batch through a real broker and cleans up.
import { test } from "node:test";
import assert from "node:assert/strict";
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { execFileSync } from "node:child_process";
import { readState, stateFilePath } from "../recorder.js";

const HERE = path.dirname(fileURLToPath(import.meta.url));
const HOOKS = path.join(HERE, "..", "hooks");

function runHook(script, payload, env) {
  return execFileSync(process.execPath, [path.join(HOOKS, script)], {
    input: JSON.stringify(payload),
    env: { ...process.env, ...env },
    timeout: 10000,
  }).toString();
}

test("post-tool-use records search results into the state file", () => {
  const dataDir = mkdtempSync(path.join(tmpdir(), "topodb-cap-"));
  try {
    const payload = {
      session_id: "sess-A", cwd: "/tmp", hook_event_name: "PostToolUse",
      tool_name: "mcp__plugin_topodb_topodb__search_memories",
      tool_input: { query: "auth flow" },
      tool_output: { hits: [{ node: { id: "01M", props: { content: "auth uses tokens" } }, score: 0.02 }] },
    };
    assert.equal(runHook("post-tool-use.js", payload, { CLAUDE_PLUGIN_DATA: dataDir }), "");
    const state = readState(dataDir, "sess-A");
    assert.equal(state.retrievals.length, 1);
    assert.equal(state.retrievals[0].query, "auth flow");
    assert.equal(state.retrievals[0].channel, "text");
    assert.equal(state.contents["01M"], "auth uses tokens");

    // Recording off: no write.
    runHook("post-tool-use.js", { ...payload, session_id: "sess-off" }, { CLAUDE_PLUGIN_DATA: dataDir, TOPODB_RECORDING: "0" });
    assert.equal(readState(dataDir, "sess-off"), null);

    // Subagent session: no write.
    runHook("post-tool-use.js", { ...payload, session_id: "sess-sub", agent_type: "Explore" }, { CLAUDE_PLUGIN_DATA: dataDir });
    assert.equal(readState(dataDir, "sess-sub"), null);

    // Non-retrieval tool_output shape: tolerated, no write, exit 0.
    runHook("post-tool-use.js", { ...payload, session_id: "sess-x", tool_output: { weird: true } }, { CLAUDE_PLUGIN_DATA: dataDir });
    assert.equal(readState(dataDir, "sess-x"), null);
  } finally {
    rmSync(dataDir, { recursive: true, force: true });
  }
});

test("session-end flushes an episode through a real broker and deletes state", async () => {
  // Broker via a real launch.js shim (pinned server 0.0.10 is FINE here:
  // submit_batch/create_memory/get_node all exist in it).
  const { spawn } = await import("node:child_process");
  const dataDir = mkdtempSync(path.join(tmpdir(), "topodb-se-"));
  const projectDir = mkdtempSync(path.join(tmpdir(), "topodb-sep-"));
  const shim = spawn(process.execPath, [path.join(HERE, "..", "launch.js")], {
    env: { ...process.env, CLAUDE_PLUGIN_DATA: dataDir, CLAUDE_PROJECT_DIR: projectDir, TOPODB_BROKER_IDLE_MS: "5000" },
    stdio: ["pipe", "pipe", "pipe"],
  });
  let client = null;
  try {
    const { connectForProject } = await import("../broker-client.js");
    for (let i = 0; i < 50 && !client; i++) {
      await new Promise((r) => setTimeout(r, 200));
      client = await connectForProject({ projectDir, dataDir });
    }
    assert.ok(client, "broker must come up");
    // A real memory the episode will reference.
    const mem = await client.call("create_memory", { content: "alpha beta gamma delta" });

    // Seed the state file as post-tool-use would have.
    const { appendRetrieval } = await import("../recorder.js");
    appendRetrieval(dataDir, "sess-E", {
      query: "q", at: Date.now(), channel: "text",
      returned: [{ id: mem.id, rank: 0, score: 1 }],
    }, new Map([[mem.id, "alpha beta gamma delta"]]));

    // Transcript with assistant text that USES the memory (>=50% tokens).
    const transcript = path.join(dataDir, "transcript.jsonl");
    writeFileSync(transcript, [
      JSON.stringify({ type: "assistant", message: { content: [{ type: "text", text: "alpha beta gamma indeed" }] } }),
    ].join("\n"));

    assert.equal(
      runHook("session-end.js", {
        session_id: "sess-E", cwd: projectDir, hook_event_name: "SessionEnd",
        reason: "other", transcript_path: transcript,
      }, { CLAUDE_PLUGIN_DATA: dataDir, CLAUDE_PROJECT_DIR: projectDir }),
      "",
    );

    // State file is gone…
    const { readState: rs } = await import("../recorder.js");
    assert.equal(rs(dataDir, "sess-E"), null);
    // …and the episode is queryable: the memory has an incoming RETURNED
    // edge and an incoming USED edge from a RetrievalEvent. Incoming edges
    // aren't listable via get_edges(from_id) — assert via traverse from the
    // memory (both directions, 1 hop):
    const sub = await client.call("traverse", { seed_id: mem.id, max_hops: 1, direction: "both" });
    const kinds = (sub.subgraph?.edges ?? []).map((e) => e.type).sort();
    // Edge types come back normalized (lowercased, separators collapsed) by
    // the server's normalize_edge_type — see crates/topodb-json/src/lib.rs —
    // even though buildEpisodeBatch writes them as "RETURNED"/"USED".
    assert.ok(kinds.includes("returned"), `edges: ${kinds}`);
    assert.ok(kinds.includes("used"), `edges: ${kinds}`);
  } finally {
    client?.close();
    shim.kill();
    rmSync(dataDir, { recursive: true, force: true });
    rmSync(projectDir, { recursive: true, force: true });
  }
});
