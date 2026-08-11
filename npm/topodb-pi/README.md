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

No Rust toolchain and no separate MCP adapter required — the prebuilt
`topodb-mcp` binary is pulled in automatically for your platform.
