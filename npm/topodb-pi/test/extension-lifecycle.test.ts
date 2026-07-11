// test/extension-lifecycle.test.ts
//
// Drives extension.ts's real pi.on("agent_start"/"turn_end"/"agent_end")
// handlers and the real pi.registerTool(...) tool's execute() through a
// synthetic run, end to end: a retrieval capture during the turn, then
// agent_end assembling and submitting the episode batch. This exercises the
// actual wiring (buffer bookkeeping, retrieval capture via the registered
// tool, the isUsed heuristic, buildEpisodeBatch indexing) rather than the
// pure recorder.ts functions in isolation (see recorder.test.ts for those).
//
// extension.ts constructs its own `TopodbServer` internally (no injection
// seam), so the one thing this harness fakes below the ExtensionAPI surface
// is `TopodbServer.prototype.call` — monkey-patched for the duration of each
// test so `server.call(...)` calls made from inside the real registered
// tool's execute() and from agent_end's submit_batch land on a canned
// responder instead of spawning the real topodb-mcp subprocess.
import { test } from "node:test";
import assert from "node:assert/strict";
import { TopodbServer } from "../src/server-handle.ts";
import registerExtension from "../src/extension.ts";

type CallImpl = (tool: string, args: Record<string, unknown>) => Promise<unknown> | unknown;

interface Harness {
  handlers: Map<string, (event: unknown) => Promise<unknown> | unknown>;
  tool: { execute: (...args: unknown[]) => Promise<unknown> };
  calls: Array<{ tool: string; args: Record<string, unknown> }>;
  restore: () => void;
}

function harness(callImpl: CallImpl): Harness {
  const handlers = new Map<string, (event: unknown) => Promise<unknown> | unknown>();
  let tool: Harness["tool"] | undefined;
  const pi = {
    on(event: string, handler: (event: unknown) => Promise<unknown> | unknown) {
      handlers.set(event, handler);
    },
    registerTool(def: Harness["tool"]) {
      tool = def;
    },
  } as unknown as Parameters<typeof registerExtension>[0];
  const calls: Array<{ tool: string; args: Record<string, unknown> }> = [];
  const original = TopodbServer.prototype.call;
  TopodbServer.prototype.call = async function (t: string, a: Record<string, unknown>) {
    calls.push({ tool: t, args: a });
    return callImpl(t, a);
  };
  registerExtension(pi);
  if (!tool) throw new Error("registerTool was never called");
  return {
    handlers,
    tool,
    calls,
    restore() {
      TopodbServer.prototype.call = original;
    },
  };
}

const noopTurnEnd = { type: "turn_end", turnIndex: 0, timestamp: Date.now(), message: {}, toolResults: [] };

test("agent_end: one retrieval whose memory is quoted back -> single submit_batch with Episode+RetrievalEvent+ISSUED+RETURNED+USED", async () => {
  const memId = "01MEMAAAAAAAAAAAAAAAAAAAAA";
  const memContent = "rust ownership rules";
  let submitted: Array<Record<string, unknown>> | undefined;
  const h = harness(async (t, a) => {
    if (t === "search_memories") {
      return {
        hits: [{ node: { id: memId, label: "Memory", props: { content: memContent } }, score: 0.9 }],
      };
    }
    if (t === "submit_batch") {
      submitted = a.commands as Array<Record<string, unknown>>;
      return { ids: submitted.map((_, i) => `01EP${i}`) };
    }
    throw new Error(`unexpected tool ${t}`);
  });
  try {
    await h.handlers.get("agent_start")!({ type: "agent_start" });

    // Real registered-tool execute path: this is what makes retrieval capture
    // run (extension.ts's execute() calls toRetrievalRecord + buffer.addRetrieval
    // itself; we don't call recorder.ts functions directly here).
    const toolResult = (await h.tool.execute(
      "call-1",
      { tool: "search_memories", args: { query: "ownership", k: 5 } },
      undefined,
      undefined,
      {},
    )) as { details?: { error?: unknown } };
    assert.equal(toolResult.details?.error, undefined, "the fake search_memories result must be accepted as a valid capture");

    await h.handlers.get("turn_end")!(noopTurnEnd);

    const afterRetrieval = Date.now() + 60_000; // safely after the retrieval's `at` capture time
    await h.handlers.get("agent_end")!({
      type: "agent_end",
      messages: [
        { role: "user", content: "learn rust ownership" },
        {
          role: "assistant",
          content: "we applied rust ownership concepts successfully",
          usage: { input: 100, output: 50 },
          timestamp: afterRetrieval,
        },
      ],
    });

    const submitCalls = h.calls.filter((c) => c.tool === "submit_batch");
    assert.equal(submitCalls.length, 1, "exactly one submit_batch call");
    assert.ok(submitted);
    const cmds = submitted!;
    assert.equal(cmds.length, 5);

    const episode = cmds[0] as { op: string; label: string; props: Record<string, unknown> };
    assert.equal(episode.op, "create_node");
    assert.equal(episode.label, "Episode");
    assert.equal(episode.props.goal, "learn rust ownership");
    assert.equal(episode.props.outcome, "success");
    assert.equal(episode.props.turns, 1);
    assert.equal(episode.props.tokens, 150); // input 100 + output 50
    assert.equal(episode.props.failure, "");

    const retrievalEvent = cmds[1] as { op: string; label: string; props: Record<string, unknown> };
    assert.equal(retrievalEvent.op, "create_node");
    assert.equal(retrievalEvent.label, "RetrievalEvent");
    assert.equal(retrievalEvent.props.query, "ownership");

    assert.deepEqual(cmds[2], { op: "link", from: "#0", to: "#1", type: "ISSUED" });

    const returned = cmds[3] as { op: string; from: string; to: string; type: string; props: Record<string, unknown> };
    assert.equal(returned.op, "link");
    assert.equal(returned.from, "#1");
    assert.equal(returned.to, memId);
    assert.equal(returned.type, "RETURNED");
    assert.equal(returned.props.rank, 0);
    assert.equal(returned.props.score, 0.9);
    assert.equal(returned.props.channel, "text");

    assert.deepEqual(cmds[4], { op: "link", from: "#1", to: memId, type: "USED" });
  } finally {
    h.restore();
  }
});

test("agent_end: two retrievals in one run -> back-reference (#N) indices for both RetrievalEvents are correct", async () => {
  const memA = "01MEMAAAAAAAAAAAAAAAAAAAAA";
  const memB = "01MEMBBBBBBBBBBBBBBBBBBBBB";
  let submitted: Array<Record<string, unknown>> | undefined;
  const h = harness(async (t, a) => {
    if (t === "search_memories") {
      const query = (a as { query?: unknown }).query;
      if (query === "ownership") {
        return { hits: [{ node: { id: memA, label: "Memory", props: { content: "rust ownership rules" } }, score: 0.9 }] };
      }
      if (query === "async") {
        return { hits: [{ node: { id: memB, label: "Memory", props: { content: "async tokio runtime" } }, score: 0.7 }] };
      }
      throw new Error(`unexpected query ${String(query)}`);
    }
    if (t === "submit_batch") {
      submitted = a.commands as Array<Record<string, unknown>>;
      return { ids: submitted.map((_, i) => `01EP${i}`) };
    }
    throw new Error(`unexpected tool ${t}`);
  });
  try {
    await h.handlers.get("agent_start")!({ type: "agent_start" });

    await h.tool.execute("call-1", { tool: "search_memories", args: { query: "ownership", k: 5 } }, undefined, undefined, {});
    await h.tool.execute("call-2", { tool: "search_memories", args: { query: "async", k: 5 } }, undefined, undefined, {});

    await h.handlers.get("turn_end")!(noopTurnEnd);

    const afterRetrievals = Date.now() + 60_000;
    await h.handlers.get("agent_end")!({
      type: "agent_end",
      messages: [
        { role: "user", content: "learn rust ownership and async rust" },
        {
          role: "assistant",
          content: "we used rust ownership concepts and tokio runtime patterns",
          usage: { input: 10, output: 5 },
          timestamp: afterRetrievals,
        },
      ],
    });

    assert.ok(submitted);
    const cmds = submitted!;
    // Expected layout (indices pin the #N back-reference arithmetic):
    // 0: Episode
    // 1: RetrievalEvent (r0)  -> referenced as "#1"
    // 2: ISSUED   #0 -> #1
    // 3: RETURNED #1 -> memA
    // 4: USED     #1 -> memA
    // 5: RetrievalEvent (r1)  -> referenced as "#5"
    // 6: ISSUED   #0 -> #5
    // 7: RETURNED #5 -> memB
    // 8: USED     #5 -> memB
    assert.equal(cmds.length, 9);

    assert.equal((cmds[1] as { label: string }).label, "RetrievalEvent");
    assert.deepEqual(cmds[2], { op: "link", from: "#0", to: "#1", type: "ISSUED" });
    assert.equal((cmds[3] as { to: string }).to, memA);
    assert.equal((cmds[3] as { from: string }).from, "#1");
    assert.deepEqual(cmds[4], { op: "link", from: "#1", to: memA, type: "USED" });

    assert.equal((cmds[5] as { label: string }).label, "RetrievalEvent");
    assert.deepEqual(cmds[6], { op: "link", from: "#0", to: "#5", type: "ISSUED" });
    assert.equal((cmds[7] as { to: string }).to, memB);
    assert.equal((cmds[7] as { from: string }).from, "#5");
    assert.deepEqual(cmds[8], { op: "link", from: "#5", to: memB, type: "USED" });
  } finally {
    h.restore();
  }
});

test("agent_end: submit_batch rejecting resolves without throwing (never-break-the-agent)", async () => {
  const h = harness(async (t) => {
    if (t === "submit_batch") throw new Error("db down");
    throw new Error(`unexpected tool ${t}`);
  });
  const originalError = console.error;
  const errors: string[] = [];
  console.error = (msg?: unknown) => {
    errors.push(String(msg));
  };
  try {
    await h.handlers.get("agent_start")!({ type: "agent_start" });
    await h.handlers.get("turn_end")!(noopTurnEnd);

    await assert.doesNotReject(
      Promise.resolve(
        h.handlers.get("agent_end")!({
          type: "agent_end",
          messages: [
            { role: "user", content: "goal" },
            { role: "assistant", content: "done", usage: { input: 1, output: 1 }, timestamp: Date.now() },
          ],
        }),
      ),
    );

    assert.ok(
      errors.some((e) => e.includes("episode write failed")),
      "the rejection should be logged, not swallowed silently",
    );
    assert.equal(h.calls.filter((c) => c.tool === "submit_batch").length, 1);
  } finally {
    console.error = originalError;
    h.restore();
  }
});
