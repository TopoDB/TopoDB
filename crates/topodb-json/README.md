# topodb-json

Pure, `Db`-free conversions between [TopoDB](https://crates.io/crates/topodb) engine types
(`PropValue`, `Props`, `Scope`, `NodeRecord`, `EdgeRecord`, `Subgraph`, ...) and
`serde_json::Value`, plus the shared default-index-spec constants (`Entity`/`name`,
`Memory`/`content`). It's the single JSON↔engine conversion layer used by both
[`topodb-mcp`](https://crates.io/crates/topodb-mcp) and [`topodb-cli`](https://crates.io/crates/topodb-cli),
factored out so any future JSON-speaking front end (an HTTP server, another tool) can reuse the
same rules without depending on `rmcp` or `clap`. Status: **v0**, a young extraction whose
function set and error shapes track whatever the current TopoDB front ends need, not a settled
public API — this is **not a stability promise**; expect breaking changes before 1.0.
