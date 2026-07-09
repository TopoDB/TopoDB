# @topodb/topodb-mcp

Zero-toolchain launcher for [`topodb-mcp`](https://crates.io/crates/topodb-mcp), the
stdio MCP server for the [TopoDB](https://github.com/TopoDB/TopoDB) agent-memory engine.

```bash
npx -y @topodb/topodb-mcp --db .topodb/memory.redb
```

The right prebuilt binary for your platform is installed automatically via an
`optionalDependencies` sub-package — no Rust toolchain, no postinstall, no network at
launch. Prefer building from source? `cargo install topodb-mcp`.
