#!/usr/bin/env node
// afterMCPExecution: for topodb's own tools only — record what retrieval
// returned (episode state) and flag memory writes (so the stop nudge stays
// quiet and a memory_write marker lands in the spool). No stdout, exit 0.
import { readStdin, parseJson, debugDump, recordingDisabled, runHook } from "../core/hook-io.js";
import { bareToolName, isTopodbTool, RETRIEVAL_TOOLS, WRITE_TOOLS, recordRetrieval, recordMemoryWrite } from "../core/hooks/retrieval.js";
import { hookContext, HARNESS } from "./_env.js";

function parseResult(r) { if (typeof r !== "string") return r; try { return JSON.parse(r); } catch { return r; } }

async function main() {
  const raw = await readStdin();
  const p = parseJson(raw);
  const { dataDir, projectDir, sessionId } = hookContext(p ?? {}, process.env);
  debugDump({ dataDir, env: process.env, eventName: "afterMCPExecution", raw });
  if (!p || recordingDisabled(process.env)) return;
  if (!isTopodbTool(p.tool_name)) return;
  const tool = bareToolName(p.tool_name);
  const result = parseResult(p.result_json ?? p.tool_output ?? p.result);
  if (RETRIEVAL_TOOLS.includes(tool)) recordRetrieval({ dataDir, sessionId, toolName: tool, toolInput: p.tool_input, toolResult: result });
  else if (WRITE_TOOLS.includes(tool)) recordMemoryWrite({ dataDir, env: process.env, projectDir, sessionId, toolResult: result, harness: HARNESS });
}
runHook(main, { deadlineMs: 4000 });
