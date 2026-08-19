// _env.js — Cursor payload/env → the ids every hook needs. Cursor puts
// conversation_id on every hook and session_id only on sessionStart/sessionEnd;
// sessionStart returns TOPODB_SESSION_ID in its `env` output so later hooks in
// the same session key their state identically (spec §4.3, dogfood D2).
import { resolveDataDir } from "../core/data-dir.js";
export const HARNESS = "cursor";
export function hookContext(payload, env) {
  const p = payload ?? {};
  return {
    dataDir: resolveDataDir(env).dir,
    projectDir: env.CURSOR_PROJECT_DIR ?? (Array.isArray(p.workspace_roots) ? p.workspace_roots[0] : undefined) ?? p.cwd ?? null,
    sessionId: p.session_id ?? env.TOPODB_SESSION_ID ?? p.conversation_id ?? null,
  };
}
