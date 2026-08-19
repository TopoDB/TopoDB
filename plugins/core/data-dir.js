// data-dir.js — where a plugin keeps memory.redb, the npm-installed server,
// episodes/ and memory.warehouse/. One rule for every client so that, when two
// editors are installed, they land on the SAME directory and therefore the
// same db and the same daemon (the socket is keyed by db path — ipc.js).
import { existsSync } from "node:fs";
import { homedir as osHomedir } from "node:os";
import path from "node:path";

/** Claude Code's plugin data dir for this plugin (CLAUDE_PLUGIN_DATA), as
 *  Claude Code lays it out: ~/.claude/plugins/data/<marketplace>-<plugin>/. */
export const CLAUDE_SHARED_REL = [".claude", "plugins", "data", "topodb-topodb"];
export const DEFAULT_REL = [".topodb", "plugin-data"];

const nonBlank = (v) => typeof v === "string" && v.trim() !== "" ? v : null;

/**
 * @returns {{ dir: string, reason: "override"|"claude-env"|"claude-shared"|"default" }}
 * Does not create the directory — the launcher does (mkdirSync recursive).
 */
export function resolveDataDir(env, { homedir = osHomedir(), exists = existsSync } = {}) {
  const override = nonBlank(env.TOPODB_PLUGIN_DATA);
  if (override) return { dir: override, reason: "override" };
  const claudeEnv = nonBlank(env.CLAUDE_PLUGIN_DATA);
  if (claudeEnv) return { dir: claudeEnv, reason: "claude-env" };
  const shared = path.join(homedir, ...CLAUDE_SHARED_REL);
  if (exists(shared)) return { dir: shared, reason: "claude-shared" };
  return { dir: path.join(homedir, ...DEFAULT_REL), reason: "default" };
}
