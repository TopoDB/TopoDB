#!/usr/bin/env node
// Claude Code entry point: env → core run(). CLAUDE_PLUGIN_DATA survives plugin
// updates (the db and the npm-installed server live there); CLAUDE_PROJECT_DIR
// is the repo root this window is working in.
//
// Cursor auto-imports this plugin but does not set CLAUDE_PLUGIN_DATA. Use the
// same data-dir resolution as the Cursor plugin so both editors share one store
// instead of advertising a 0-tool degraded server.
import { run } from "./core/launch.js";
import { resolveDataDir } from "./core/data-dir.js";

const { dir, reason } = resolveDataDir(process.env);
process.stderr.write(`topodb: data dir ${dir} (${reason})\n`);
await run({
  dataDir: dir,
  projectDir:
    process.env.CLAUDE_PROJECT_DIR ?? process.env.CURSOR_PROJECT_DIR ?? process.cwd(),
  env: process.env,
});
