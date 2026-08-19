// _env.js — Cursor payload/env → the ids every hook needs. Cursor puts
// conversation_id on every hook payload, while session_id is only on
// sessionStart/sessionEnd; preferring conversation_id (and the env echo of
// it, TOPODB_SESSION_ID, set by sessionStart's `env` output) keeps spool and
// episode state on one key whether or not Cursor honors sessionStart's `env`
// output (spec §4.3, dogfood D2).
import { resolveDataDir } from "../core/data-dir.js";
export const HARNESS = "cursor";
export function hookContext(payload, env) {
  const p = payload ?? {};
  return {
    dataDir: resolveDataDir(env).dir,
    projectDir: env.CURSOR_PROJECT_DIR ?? (Array.isArray(p.workspace_roots) ? p.workspace_roots[0] : undefined) ?? p.cwd ?? null,
    sessionId: env.TOPODB_SESSION_ID ?? p.conversation_id ?? p.session_id ?? null,
  };
}
