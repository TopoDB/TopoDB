#!/usr/bin/env node
// Cursor entry point: env → core run(). Cursor has no per-plugin data dir, so
// the location is resolved by core/data-dir.js (TOPODB_PLUGIN_DATA, else the
// Claude Code plugin's dir when present — one db and one daemon across both
// editors — else ~/.topodb/plugin-data). CURSOR_PROJECT_DIR is the workspace
// root; CLAUDE_PROJECT_DIR is Cursor's compatibility alias for it.
import { run } from "./core/launch.js";
import { resolveDataDir } from "./core/data-dir.js";

const { dir, reason } = resolveDataDir(process.env);
process.stderr.write(`topodb: data dir ${dir} (${reason})\n`);
await run({
  dataDir: dir,
  projectDir: process.env.CURSOR_PROJECT_DIR ?? process.env.CLAUDE_PROJECT_DIR ?? process.cwd(),
  env: process.env,
});
