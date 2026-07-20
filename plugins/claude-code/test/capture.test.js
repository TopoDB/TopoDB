// post-tool-use accumulates retrievals into the state file; session-end
// builds and submits the Episode batch through a real broker and cleans up.
import { test } from "node:test";
import assert from "node:assert/strict";
import { mkdtempSync, rmSync, writeFileSync, readFileSync, existsSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";
import { rmWithGrace } from "./fsgrace.js";
import { execFileSync, spawn } from "node:child_process";
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
    // The broker outlives the shim until its idle window elapses and holds
    // topodb-mcp.exe (and its log) inside dataDir — Windows cannot delete
    // those until the processes die. rmWithGrace retries with real sleeps
    // and, if the dir still cannot go, names every surviving entry.
    await rmWithGrace(dataDir);
    await rmWithGrace(projectDir);
  }
});

// Real MCP hook payloads may not deliver the pre-parsed structured result
// toRetrievalRecord expects — normalizeToolResult (recorder.js) has to
// unwrap structuredContent / content-block-array / envelope shapes before
// capture can happen at all. Exercise each shape through the real hook.
test("post-tool-use normalizes every documented tool_output shape", () => {
  const dataDir = mkdtempSync(path.join(tmpdir(), "topodb-norm-"));
  try {
    const basePayload = {
      cwd: "/tmp", hook_event_name: "PostToolUse",
      tool_name: "mcp__plugin_topodb_topodb__search_memories",
      tool_input: { query: "auth flow" },
    };
    const structured = { hits: [{ node: { id: "01M", props: { content: "auth uses tokens" } }, score: 0.02 }] };

    // 1. structuredContent envelope.
    runHook("post-tool-use.js", {
      ...basePayload, session_id: "sess-sc",
      tool_output: { structuredContent: structured },
    }, { CLAUDE_PLUGIN_DATA: dataDir });
    assert.equal(readState(dataDir, "sess-sc").retrievals.length, 1);

    // 2. raw content-block array.
    runHook("post-tool-use.js", {
      ...basePayload, session_id: "sess-arr",
      tool_output: [{ type: "text", text: JSON.stringify(structured) }],
    }, { CLAUDE_PLUGIN_DATA: dataDir });
    assert.equal(readState(dataDir, "sess-arr").retrievals.length, 1);

    // 3. envelope with a `content` array.
    runHook("post-tool-use.js", {
      ...basePayload, session_id: "sess-env",
      tool_output: { content: [{ type: "text", text: JSON.stringify(structured) }] },
    }, { CLAUDE_PLUGIN_DATA: dataDir });
    assert.equal(readState(dataDir, "sess-env").retrievals.length, 1);

    // 4. already-parsed object (today's shape) keeps working.
    runHook("post-tool-use.js", {
      ...basePayload, session_id: "sess-parsed",
      tool_output: structured,
    }, { CLAUDE_PLUGIN_DATA: dataDir });
    assert.equal(readState(dataDir, "sess-parsed").retrievals.length, 1);

    // Garbage text block: records nothing, exits cleanly (no throw above).
    assert.equal(runHook("post-tool-use.js", {
      ...basePayload, session_id: "sess-garbage",
      tool_output: { content: [{ type: "text", text: "not json at all" }] },
    }, { CLAUDE_PLUGIN_DATA: dataDir }), "");
    assert.equal(readState(dataDir, "sess-garbage"), null);
  } finally {
    rmSync(dataDir, { recursive: true, force: true });
  }
});

test("post-tool-use debug escape dumps the raw payload", () => {
  const dataDir = mkdtempSync(path.join(tmpdir(), "topodb-dbg-"));
  try {
    const payload = {
      session_id: "sess-dbg", cwd: "/tmp", hook_event_name: "PostToolUse",
      tool_name: "mcp__plugin_topodb_topodb__search_memories",
      tool_input: { query: "auth flow" },
      tool_output: { hits: [] },
    };
    assert.equal(
      runHook("post-tool-use.js", payload, { CLAUDE_PLUGIN_DATA: dataDir, TOPODB_HOOK_DEBUG: "1" }),
      "",
    );
    const dumped = path.join(dataDir, "episodes", "debug-last-payload.json");
    assert.ok(existsSync(dumped), "debug payload file should exist");
    assert.deepEqual(JSON.parse(readFileSync(dumped, "utf8")), payload);
  } finally {
    rmSync(dataDir, { recursive: true, force: true });
  }
});

// Concurrent PostToolUse processes are ordinary (parallel tool calls in one
// session). appendRetrieval must never leave a torn (partially-written)
// state file behind — a torn read makes readState UNLINK the whole
// session's state, destroying every writer's work, not just the racer's.
// Lost updates (last rename wins) are accepted; torn files are not.
test("concurrent appendRetrieval calls never tear the state file", async () => {
  const dataDir = mkdtempSync(path.join(tmpdir(), "topodb-race-"));
  try {
    const sessionId = "sess-race";
    const WRITERS = 4;
    const APPENDS_PER_WRITER = 8;
    // file:// URL, not a bare path: on Windows an absolute path in an ESM
    // specifier parses as a URL with protocol "d:" and the loader throws
    // ERR_UNSUPPORTED_ESM_URL_SCHEME.
    const childScript = `
      import { appendRetrieval } from ${JSON.stringify(pathToFileURL(path.join(HERE, "..", "recorder.js")).href)};
      const dataDir = process.argv[1];
      const sessionId = process.argv[2];
      const n = Number(process.argv[3]);
      for (let i = 0; i < n; i++) {
        appendRetrieval(dataDir, sessionId, { query: "q" + i, at: Date.now(), channel: "text", returned: [] }, new Map());
      }
    `;
    const runWriter = () =>
      new Promise((resolve, reject) => {
        const child = spawn(process.execPath, ["--input-type=module", "-e", childScript, "--", dataDir, sessionId, String(APPENDS_PER_WRITER)]);
        let stderr = "";
        child.stderr.on("data", (d) => (stderr += d));
        child.on("exit", (code) => (code === 0 ? resolve() : reject(new Error(`writer exited ${code}: ${stderr}`))));
        child.on("error", reject);
      });

    await Promise.all(Array.from({ length: WRITERS }, runWriter));

    // The file must parse cleanly (no torn JSON) and readState must not
    // have discarded it as corrupt.
    const state = readState(dataDir, sessionId);
    assert.ok(state, "state file must survive concurrent writers intact");
    assert.ok(Array.isArray(state.retrievals));
    // Also verify the on-disk bytes parse directly (belt and suspenders —
    // readState's own JSON.parse is the real assertion above).
    const raw = readFileSync(stateFilePath(dataDir, sessionId), "utf8");
    JSON.parse(raw); // throws (failing the test) if torn
    // No zero-loss guarantee under races — at least one writer's records
    // must have survived, but we do not assert full retention.
    assert.ok(state.retrievals.length >= 1, "at least one append must have survived");
  } finally {
    rmSync(dataDir, { recursive: true, force: true });
  }
});
