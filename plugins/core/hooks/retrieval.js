import { appendRetrieval, markCaptured, normalizeToolResult, toRetrievalRecord } from "../recorder.js";
import { sessionScopes } from "../server-args.js";
import { tryMarker, HARNESS } from "../warehouse-spool.js";

export const RETRIEVAL_TOOLS = ["search_memories", "traverse", "recent_memories"];
export const WRITE_TOOLS = ["remember", "create_memory"];
/** mcp__plugin_topodb_topodb__search_memories | topodb/search_memories | topodb:search_memories → search_memories */
export function bareToolName(name) {
  const s = String(name ?? "");
  return s.split(/__|\/|:/).pop();
}
export function recordRetrieval({ dataDir, sessionId, toolName, toolInput, toolResult }) {
  if (!dataDir || !sessionId) return false;
  const tool = bareToolName(toolName);
  if (!RETRIEVAL_TOOLS.includes(tool)) return false;
  const rec = toRetrievalRecord(tool, toolInput ?? {}, normalizeToolResult(toolResult));
  if (!rec) return false;
  appendRetrieval(dataDir, sessionId, rec.record, rec.contents);
  return true;
}
export function recordMemoryWrite({ dataDir, env, projectDir, sessionId, toolResult, harness = HARNESS }) {
  if (!dataDir || !sessionId) return [];
  markCaptured(dataDir, sessionId);
  const r = normalizeToolResult(toolResult) ?? {};
  const ids = [...new Set([r.memory_id, r.id, r.node?.id, r.memory?.id].filter((s) => typeof s === "string" && s.length === 26))];
  if (ids.length) tryMarker({ dataDir, env, projectDir, sessionId, type: "memory_write", nodeIds: ids, sessionScopes, harness });
  return ids;
}
