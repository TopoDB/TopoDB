# @topodb/pi

One-command [TopoDB](https://github.com/TopoDB/TopoDB) memory for the
[Pi](https://pi.dev) coding agent.

    pi install npm:@topodb/pi

Registers a single `topodb` tool that lazily spawns the `topodb-mcp` server and
proxies its memory tools. Call `{action:"list"}` to discover them, then
`{tool, args}` to use one. Config via env: `TOPODB_DB` (default
`.topodb/memory.redb`), `TOPODB_SCOPE` (default `shared`), `TOPODB_IDLE_MS`
(reap the idle server after this many ms so other processes can use the same
db; default `30000`, `0` keeps it always resident).

Note: on first use on an embedding-capable platform the server downloads the
ONNX runtime and embedding model (~50MB, one-time, cached). On a slow link the
idle reaper can interrupt that download before it completes — set
`TOPODB_IDLE_MS=0` for the first session if semantic search stays in
text-fallback mode.

**Context warehouse capture.** Every successful `bash`/`read`/`edit`/`write`/
`grep`/`find` result, plus session start/end and memory-write markers, is
appended raw to a spool under `<db>.warehouse/` next to the db
(`.topodb/memory.warehouse/` by default; `TOPODB_WAREHOUSE_DIR` relocates it).
The `topodb-mcp` child this extension already spawns drains the spool at boot
into content-addressed, redacted segments and derives `Artifact`/`Chunk` nodes
with `evidence` links to the memories the session wrote — deterministic, no
model calls. `topodb warehouse status --db .topodb/memory.redb` shows the tiers.
`TOPODB_WAREHOUSE=0` turns just this off; `TOPODB_RECORD=0` turns all recording
off. `ls`, custom tools, MCP tools, and failed tool calls are never captured.

No Rust toolchain and no separate MCP adapter required — the prebuilt
`topodb-mcp` binary is pulled in automatically for your platform.
