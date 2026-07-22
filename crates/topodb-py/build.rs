fn main() {
    // pyo3's `extension-module` feature deliberately does NOT link libpython:
    // Python symbols are resolved at load time by the interpreter that imports
    // the module. On macOS the linker must be told to permit those undefined
    // symbols. maturin (how the `bindings` workflow builds this crate) adds the
    // flag automatically, but a plain `cargo build` — e.g. cargo-dist's
    // `--workspace` release build for topodb-mcp/-cli — does not, so the crate
    // fails to link with "symbol(s) not found for architecture" and takes the
    // whole release build down with it. Add the flag ourselves, scoped to the
    // cdylib so nothing else in the workspace has its linking loosened.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        println!("cargo:rustc-cdylib-link-arg=-undefined");
        println!("cargo:rustc-cdylib-link-arg=dynamic_lookup");
    }
}
