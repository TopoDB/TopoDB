# plugins/core — shared plugin modules

Source of truth for everything the TopoDB editor plugins share: the MCP
launcher bootstrap (`launch.js`), socket/IPC (`ipc.js`), scope derivation
(`scope-id.js`, `server-args.js`), episode recorder (`recorder.js`), warehouse
spool (`warehouse-spool.js`), degraded-mode server (`degraded.js`), and the
pure hook logic under `hooks/`.

Plugins must be self-contained at install time, so this directory is **copied**
into `plugins/claude-code/core/` and `plugins/cursor/core/` by
`node scripts/sync-plugin-core.mjs`. Each plugin's test suite runs the
`--check` mode and fails on drift. Edit here, sync, commit both.

Tests here are pure (no spawned servers). Integration tests that spawn
`launch.js`/hook scripts live in each plugin.
