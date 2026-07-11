// src/extension.ts
import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";
import { Type } from "typebox";
import { TopodbServer, recordingEnabled } from "./server-handle.ts";
import { EpisodeBuffer, extractText, isUsed, buildEpisodeBatch, toRetrievalRecord } from "./recorder.ts";
import { ensurePolicyVersion } from "./policy.ts";

export default function (pi: ExtensionAPI): void {
  const server = new TopodbServer();

  const recording = recordingEnabled(process.env);
  const buffer = new EpisodeBuffer();
  const memContents = new Map<string, string>(); // memory id -> content seen at retrieval
  let policyId: string | undefined;
  let policyResolved = false;

  pi.registerTool({
    name: "topodb",
    label: "topodb memory",
    description:
      "Persistent agent memory (temporal property graph). Call with " +
      '{action:"list"} to see the available sub-tools and their schemas, then ' +
      '{tool:"<name>", args:{...}} to invoke one.',
    promptSnippet:
      "topodb: persistent memory. {action:\"list\"} to discover, then {tool,args}.",
    promptGuidelines: [
      "Before answering from prior context, recall with topodb search_memories/traverse.",
      "Store durable facts (decisions, entities, relationships) with create_memory/create_entity/link.",
    ],
    parameters: Type.Object({
      action: Type.Optional(Type.Literal("list")),
      tool: Type.Optional(Type.String()),
      args: Type.Optional(Type.Record(Type.String(), Type.Unknown())),
    }),
    async execute(_toolCallId, params, _signal, _onUpdate, _ctx) {
      try {
        if (params.action === "list" && params.tool) {
          return {
            content: [
              { type: "text", text: 'error: provide exactly one of {action:"list"} or {tool, args}' },
            ],
            details: { error: "bad-params" },
          };
        }
        if (params.action === "list") {
          const tools = await server.list();
          return { content: [{ type: "text", text: JSON.stringify(tools) }], details: { action: "list" } };
        }
        if (!params.tool) {
          return {
            content: [{ type: "text", text: 'error: provide {action:"list"} or {tool, args}' }],
            details: { error: "bad-params" },
          };
        }
        const result = await server.call(params.tool, params.args ?? {});
        if (recording && buffer.open && (params.tool === "search_memories" || params.tool === "traverse")) {
          try {
            const cap = toRetrievalRecord(params.tool, params.args ?? {}, result);
            if (cap) {
              buffer.addRetrieval(cap.record);
              for (const [id, content] of cap.contents) memContents.set(id, content);
            }
          } catch (e) {
            console.error(`topodb recorder: retrieval capture failed: ${(e as Error).message}`);
          }
        }
        return { content: [{ type: "text", text: JSON.stringify(result) }], details: { tool: params.tool } };
      } catch (e) {
        return { content: [{ type: "text", text: `topodb error: ${(e as Error).message}` }], details: { error: true } };
      }
    },
  });

  pi.on("agent_start", async () => {
    if (!recording) return;
    buffer.start(Date.now());
    memContents.clear();
    if (!policyResolved) {
      policyResolved = true;
      const paths = (process.env.TOPODB_POLICY_PATHS ?? "")
        .split(",")
        .map((s) => s.trim())
        .filter(Boolean);
      if (paths.length) {
        policyId = await ensurePolicyVersion((t, a) => server.call(t, a), paths);
      }
    }
  });

  pi.on("turn_end", async () => {
    if (recording) buffer.bumpTurns();
  });

  pi.on("tool_execution_end", async (ev) => {
    if (recording && ev.isError) buffer.noteToolError();
  });

  pi.on("agent_end", async (ev) => {
    if (!recording || !buffer.open) return;
    try {
      buffer.close();
      const msgs = ev.messages ?? [];
      const firstUser = msgs.find((m) => (m as { role?: string }).role === "user");
      const goal = extractText((firstUser as { content?: unknown } | undefined)?.content).slice(0, 2000);
      const assistants = msgs.filter((m) => (m as { role?: string }).role === "assistant") as Array<{
        content?: unknown;
        timestamp?: number;
        usage?: { input?: number; output?: number };
        stopReason?: string;
        errorMessage?: string;
      }>;
      const tokens = assistants.reduce((n, m) => n + (m.usage?.input ?? 0) + (m.usage?.output ?? 0), 0);
      const last = assistants[assistants.length - 1];
      const aborted = last?.stopReason === "aborted" || last?.stopReason === "error" || Boolean(last?.errorMessage);
      const outcome = aborted || buffer.toolErrors > 0 ? "failure" : "success";
      const failure = aborted
        ? `run ended with stopReason=${last?.stopReason ?? "?"}`
        : buffer.toolErrors > 0
          ? `${buffer.toolErrors} tool error(s)`
          : "";
      const textAfter = (atMs: number) =>
        assistants
          .filter((m) => (m.timestamp ?? 0) >= atMs)
          .map((m) => extractText(m.content))
          .join("\n");
      const used = new Map<number, Set<string>>();
      buffer.retrievals.forEach((r, i) => {
        const hay = textAfter(r.at);
        const s = new Set<string>();
        for (const m of r.returned) {
          const content = memContents.get(m.id) ?? "";
          if (isUsed(content, hay)) s.add(m.id);
        }
        if (s.size) used.set(i, s);
      });
      const cmds = buildEpisodeBatch({
        buffer,
        goal,
        outcome,
        failure,
        endedAt: Date.now(),
        tokens,
        used,
        policyVersionId: policyId,
      });
      await server.call("submit_batch", { commands: cmds });
    } catch (e) {
      console.error(`topodb recorder: episode write failed: ${(e as Error).message}`);
    }
  });

  pi.on("session_shutdown", async () => {
    // Flush a still-open run (crash/quit mid-episode) as a failure before
    // tearing down the server, per spec §2b.
    if (recording && buffer.open) {
      try {
        buffer.close();
        const cmds = buildEpisodeBatch({
          buffer,
          goal: "",
          outcome: "failure",
          failure: "session shutdown mid-run",
          endedAt: Date.now(),
          tokens: 0,
          used: new Map(),
          policyVersionId: policyId,
        });
        await server.call("submit_batch", { commands: cmds });
      } catch {
        /* dying anyway */
      }
    }
    server.shutdown();
  });
}
