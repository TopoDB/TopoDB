# @topodb/pi

One-command [TopoDB](https://github.com/TopoDB/TopoDB) memory for the
[Pi](https://pi.dev) coding agent.

    pi install npm:@topodb/pi

Registers a single `topodb` tool that lazily spawns the `topodb-mcp` server and
proxies its 16 memory tools. Call `{action:"list"}` to discover them, then
`{tool, args}` to use one. Config via env: `TOPODB_DB` (default
`.topodb/memory.redb`), `TOPODB_SCOPE` (default `shared`).

No Rust toolchain and no separate MCP adapter required — the prebuilt
`topodb-mcp` binary is pulled in automatically for your platform.
