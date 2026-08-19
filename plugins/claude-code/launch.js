#!/usr/bin/env node
// Claude Code entry point: env → core run(). CLAUDE_PLUGIN_DATA survives plugin
// updates (the db and the npm-installed server live there); CLAUDE_PROJECT_DIR
// is the repo root this window is working in. Everything else is core.
import { run } from "./core/launch.js";

await run({
  dataDir: process.env.CLAUDE_PLUGIN_DATA,
  projectDir: process.env.CLAUDE_PROJECT_DIR ?? process.cwd(),
  env: process.env,
});
