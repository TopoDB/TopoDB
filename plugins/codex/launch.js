#!/usr/bin/env node
// Codex entry point: env → core run(). Codex has no per-plugin data dir of its
// own, so the location is resolved by core/data-dir.js (TOPODB_PLUGIN_DATA,
// else the Claude Code plugin's dir when present — one db and one daemon
// across editors — else ~/.topodb/plugin-data). CLAUDE_PROJECT_DIR is Codex's
// compatibility alias for the workspace root.
import { run } from "./core/launch.js";
import { resolveDataDir } from "./core/data-dir.js";

const { dir, reason } = resolveDataDir(process.env);
process.stderr.write(`topodb: data dir ${dir} (${reason})\n`);
await run({
  dataDir: dir,
  projectDir: process.env.CLAUDE_PROJECT_DIR ?? process.cwd(),
  env: process.env,
});
