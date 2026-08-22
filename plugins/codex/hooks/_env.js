// _env.js — Codex payload/env → the ids every hook needs. Codex sets
// PLUGIN_DATA plus CLAUDE_PLUGIN_DATA "for compatibility"; the pair is
// normalized ONCE here (TOPODB_PLUGIN_DATA → PLUGIN_DATA → CLAUDE_PLUGIN_DATA)
// by feeding the winner through core/data-dir.js, so every hook and launch.js
// land on the same directory — and the same db and daemon — as the Claude
// Code plugin when both are installed.
import { resolveDataDir } from "../core/data-dir.js";
export const HARNESS = "codex";
export function hookContext(payload, env) {
  const p = payload ?? {};
  const plug = typeof env.PLUGIN_DATA === "string" && env.PLUGIN_DATA.trim() !== "" ? env.PLUGIN_DATA : env.CLAUDE_PLUGIN_DATA;
  return {
    dataDir: resolveDataDir({ ...env, CLAUDE_PLUGIN_DATA: plug }).dir,
    projectDir: env.CLAUDE_PROJECT_DIR ?? p.cwd ?? null,
    sessionId: env.TOPODB_SESSION_ID ?? p.session_id ?? null,
  };
}
