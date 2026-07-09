# topodb-json

Pure, `Db`-free conversions between [TopoDB](https://crates.io/crates/topodb) engine types
(`PropValue`, `Props`, `Scope`, `NodeRecord`, `EdgeRecord`, `Subgraph`, ...) and
`serde_json::Value`, factored out of `topodb-mcp` so other JSON-speaking front ends (a CLI, an
HTTP server) can reuse the same conversion rules without depending on `rmcp`. Status: **v0**, a
young extraction from a single consumer's internals — its function set and error shapes track
whatever the current TopoDB front ends need, not a settled public API; expect breaking changes
before 1.0.
