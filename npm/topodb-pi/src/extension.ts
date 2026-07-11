// src/extension.ts
import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";
import { Type } from "typebox";
import { TopodbServer } from "./server-handle.ts";

export default function (pi: ExtensionAPI): void {
  const server = new TopodbServer();

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
        return { content: [{ type: "text", text: JSON.stringify(result) }], details: { tool: params.tool } };
      } catch (e) {
        return { content: [{ type: "text", text: `topodb error: ${(e as Error).message}` }], details: { error: true } };
      }
    },
  });

  pi.on("session_shutdown", async () => {
    server.shutdown();
  });
}
