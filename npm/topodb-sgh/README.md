# @topodb/topodb-sgh

One-command launcher for [`sgh`](https://crates.io/crates/topodb-sgh), the
structured-graph-harness for [TopoDB](https://github.com/TopoDB/TopoDB) frozen-DAG agent execution.

```bash
npx -y @topodb/topodb-sgh
```

The right prebuilt binary for your platform is installed automatically via an
`optionalDependencies` sub-package — no Rust toolchain, no postinstall, no network at
launch. Prefer building from source? `cargo install topodb-sgh`.
