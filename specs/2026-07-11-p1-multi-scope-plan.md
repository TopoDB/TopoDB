# P1 — Multi-scope MCP reads: Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let an MCP client read across more than one scope at once (e.g. a project scope *plus* `shared`), and let `link` stamp an explicit scope — neither is possible today.

**Architecture:** The engine's read filter is already a `ScopeSet` (`include_shared: bool` + `BTreeSet<ScopeId>`), but every path that builds one collapses to a single member. We add a `scopes_to_scope_set` constructor in `topodb-json`, a `--read-scopes` CLI flag that seeds the server's default read set, and an optional `scopes: string[]` param on the six read tools. Separately we fix a correctness bug (`link` cannot write to a non-default scope) and gate the one unscoped read (`get_changes`) behind an opt-in flag.

**Tech Stack:** Rust, `rmcp` 2.2.0 (`#[tool_router]`/`#[tool]`), `schemars` for JSON Schema, `redb` engine, `serde_json`. Integration tests drive the real binary over newline-delimited JSON-RPC via `tests/common/mod.rs`.

**Spec:** `specs/2026-07-10-claude-code-plugin-design.md` §1.

## Global Constraints

- **Backwards compatible.** Every existing `topodb-mcp` and `topodb-json` test must pass **unmodified**. `--scope` keeps its exact current meaning (the default *write* scope).
- **All MCP params explicitly typed.** No `serde_json::Value` for new params. Untyped params were the `0.0.3` bug — they made several tools uncallable from any client. `Option<Vec<String>>` and `Option<String>`, never `Value`.
- **Writes take one scope; reads take a set.** This asymmetry is intentional and already modelled by `TopoServer::resolve_scope` (write) vs `resolve_scopes` (read). Do not "unify" them.
- **`cargo` is NOT on the Bash PATH on this machine.** Run all `cargo` commands with the **PowerShell** tool.
- Branch: `feat/mcp-multi-scope`. Repo: `C:\Users\Andrew\Documents\GitHub\topodb`.
- Ships as `topodb-mcp` **0.0.4**.
- Plans/specs live under `specs/` because `docs/` is gitignored (`.gitignore:6`).

## File Structure

| File | Responsibility | Change |
|---|---|---|
| `crates/topodb-json/src/lib.rs` | shared JSON↔engine conversion | **Modify** — add `scopes_to_scope_set` |
| `crates/topodb-json/src/batch.rs` | `submit_batch` DSL parser | **Modify** — `link` honours a per-op `scope` |
| `crates/topodb-mcp/src/config.rs` | CLI parse → `Config` | **Modify** — `--read-scopes`, `--allow-unscoped-changes` |
| `crates/topodb-mcp/src/server.rs` | rmcp tool surface | **Modify** — read `scopes[]` param, `link` scope, `get_changes` gate |
| `crates/topodb-mcp/tests/multi_scope.rs` | integration coverage | **Create** |
| `crates/topodb-mcp/Cargo.toml`, `npm/topodb-mcp/package.json`, `README.md` | release | **Modify** — 0.0.4 |

---

### Task 1: `scopes_to_scope_set` in `topodb-json`

The one thing that can build a genuinely multi-member `ScopeSet`. Everything else depends on it.

**Files:**
- Modify: `crates/topodb-json/src/lib.rs` (next to `scope_to_scope_set`, currently at :309-316)
- Test: same file, `mod tests`

**Interfaces:**
- Consumes: `topodb::{Scope, ScopeId, ScopeSet}` — `ScopeSet::of(&[ScopeId]) -> ScopeSet`, `.with_shared() -> ScopeSet`
- Produces: `pub fn scopes_to_scope_set(scopes: &[Scope]) -> ScopeSet` — used by Tasks 3 and 4.

- [ ] **Step 1: Write the failing tests**

Add to the `mod tests` block in `crates/topodb-json/src/lib.rs`:

```rust
#[test]
fn scopes_to_scope_set_admits_every_member() {
    let a = ScopeId::new();
    let b = ScopeId::new();
    let set = scopes_to_scope_set(&[Scope::Id(a), Scope::Shared, Scope::Id(b)]);
    assert!(set.contains(Scope::Id(a)));
    assert!(set.contains(Scope::Id(b)));
    assert!(set.contains(Scope::Shared));
}

#[test]
fn scopes_to_scope_set_without_shared_excludes_shared() {
    let a = ScopeId::new();
    let set = scopes_to_scope_set(&[Scope::Id(a)]);
    assert!(set.contains(Scope::Id(a)));
    assert!(!set.contains(Scope::Shared));
}

#[test]
fn scopes_to_scope_set_matches_singleton_for_one_member() {
    // The new multi-member constructor must agree with the existing
    // single-scope one for a one-element input — that equivalence is what
    // makes seeding the server's default read set from a 1-length list
    // backwards compatible.
    let a = ScopeId::new();
    let multi = scopes_to_scope_set(&[Scope::Id(a)]);
    let single = scope_to_scope_set(Scope::Id(a));
    assert_eq!(multi.contains(Scope::Id(a)), single.contains(Scope::Id(a)));
    assert_eq!(multi.contains(Scope::Shared), single.contains(Scope::Shared));

    let multi_shared = scopes_to_scope_set(&[Scope::Shared]);
    let single_shared = scope_to_scope_set(Scope::Shared);
    assert_eq!(
        multi_shared.contains(Scope::Shared),
        single_shared.contains(Scope::Shared)
    );
}

#[test]
fn scopes_to_scope_set_empty_admits_nothing() {
    let a = ScopeId::new();
    let set = scopes_to_scope_set(&[]);
    assert!(!set.contains(Scope::Shared));
    assert!(!set.contains(Scope::Id(a)));
}
```

- [ ] **Step 2: Run tests to verify they fail**

PowerShell: `cargo test -p topodb-json scopes_to_scope_set`
Expected: FAIL — `cannot find function 'scopes_to_scope_set' in this scope`

- [ ] **Step 3: Implement**

Add directly below `scope_to_scope_set` in `crates/topodb-json/src/lib.rs`:

```rust
/// Several resolved `Scope`s → the `ScopeSet` a multi-scope read runs against.
/// `Scope::Shared` sets the set's `include_shared` flag; each `Scope::Id`
/// becomes a member id. This is the only constructor that can produce a
/// genuinely multi-member `ScopeSet` — [`scope_to_scope_set`] always collapses
/// to a singleton, which is why "this project *plus* shared" was previously
/// unexpressible from any client.
///
/// An empty slice yields a set that admits nothing. Callers must not hand a
/// read an empty set expecting "everything" — there is no unscoped read.
pub fn scopes_to_scope_set(scopes: &[Scope]) -> ScopeSet {
    let ids: Vec<ScopeId> = scopes
        .iter()
        .filter_map(|s| match s {
            Scope::Id(id) => Some(*id),
            Scope::Shared => None,
        })
        .collect();
    let set = ScopeSet::of(&ids);
    if scopes.iter().any(|s| matches!(s, Scope::Shared)) {
        set.with_shared()
    } else {
        set
    }
}
```

- [ ] **Step 4: Run tests to verify they pass**

PowerShell: `cargo test -p topodb-json`
Expected: PASS, including all pre-existing tests.

- [ ] **Step 5: Commit**

```bash
git add crates/topodb-json/src/lib.rs
git commit -m "feat(json): add scopes_to_scope_set for multi-member read sets"
```

---

### Task 2: `link` gains an explicit `scope` (correctness fix)

**This is a bug fix, not a feature.** `LinkParams` has no `scope` field and the batch DSL's `link` op ignores `scope`, so **every edge is stamped with the server's default write scope**. An edge attached to a `shared` node therefore lands in the *default* scope and is invisible to any reader whose set doesn't include it — shared memories become disconnected islands. `search_memories` still returns the node's text, so this fails *silently*: the graph simply never crosses a scope boundary.

**Files:**
- Modify: `crates/topodb-mcp/src/server.rs` — `LinkParams` (:399-416), `fn link` (:758-779), `resolve_scope` doc comment (:78-89), `submit_batch` tool description (:884)
- Modify: `crates/topodb-json/src/batch.rs` — the `"link"` arm (:163-193)
- Test: `crates/topodb-json/src/batch.rs` `mod tests`; `crates/topodb-mcp/tests/multi_scope.rs` (created in Task 4 — the cross-scope integration assertion lives there; the unit-level proof lives here)

**Interfaces:**
- Consumes: `scope_of(obj, default_scope, idx) -> Result<Scope, String>` — the existing private helper in `batch.rs:54`, already used by `create_memory` and `create_entity`.
- Consumes: `TopoServer::resolve_scope(&self, scope: Option<&str>) -> Result<Scope, ErrorData>` (`server.rs:86`) — unchanged signature.
- Produces: `link` and batch-`link` accept an optional `scope` string.

- [ ] **Step 1: Write the failing batch test**

Add to `mod tests` in `crates/topodb-json/src/batch.rs`:

The batch entry point is `resolve_batch(batch: &Value, default_scope: Scope) -> Result<(Vec<Op>, Vec<Option<String>>), String>` (`batch.rs:110`), which the existing tests already call as `resolve_batch(&batch, Scope::Shared).unwrap()`.

```rust
#[test]
fn link_honours_an_explicit_scope() {
    let other = ScopeId::new();
    let batch = serde_json::json!([
        { "op": "create_entity", "name": "a" },
        { "op": "create_entity", "name": "b" },
        { "op": "link", "from": "#0", "to": "#1", "type": "x", "scope": other.to_string() }
    ]);
    // Default scope is Shared; the link must land in `other`, NOT the default.
    let (ops, _produced) = resolve_batch(&batch, Scope::Shared).unwrap();
    match &ops[2] {
        Op::CreateEdge { scope, .. } => assert_eq!(*scope, Scope::Id(other)),
        other_op => panic!("expected CreateEdge, got {other_op:?}"),
    }
}

#[test]
fn link_without_scope_falls_back_to_the_default() {
    let default_id = ScopeId::new();
    let batch = serde_json::json!([
        { "op": "create_entity", "name": "a" },
        { "op": "create_entity", "name": "b" },
        { "op": "link", "from": "#0", "to": "#1", "type": "x" }
    ]);
    let (ops, _produced) = resolve_batch(&batch, Scope::Id(default_id)).unwrap();
    match &ops[2] {
        Op::CreateEdge { scope, .. } => assert_eq!(*scope, Scope::Id(default_id)),
        other_op => panic!("expected CreateEdge, got {other_op:?}"),
    }
}
```

`ScopeId` may need adding to the test module's `use` (the file already imports `Op`, `Scope`).

- [ ] **Step 2: Run tests to verify they fail**

PowerShell: `cargo test -p topodb-json link_honours`
Expected: FAIL — the edge's scope is the default, not `other`.

- [ ] **Step 3: Fix the batch DSL**

In `crates/topodb-json/src/batch.rs`, in the `"link"` arm, replace the hardcoded `scope: default_scope` (:186). Add this line next to the other field reads (after `let ty = req_str(obj, "type", idx)?;`):

```rust
                let scope = scope_of(obj, default_scope, idx)?;
```

and change the `Op::CreateEdge` construction from `scope: default_scope,` to:

```rust
                    scope,
```

- [ ] **Step 4: Run tests to verify they pass**

PowerShell: `cargo test -p topodb-json`
Expected: PASS (new + all pre-existing).

- [ ] **Step 5: Add `scope` to the `link` MCP tool**

In `crates/topodb-mcp/src/server.rs`, add to `LinkParams` (after `edge_type`, matching the wording of `CreateMemoryParams`'s scope doc at :370-373):

```rust
    /// Scope to create the edge in: `"shared"` or a scope ULID. Defaults to
    /// the server's configured default scope when omitted. Set this explicitly
    /// when linking nodes that live in a scope other than the default —
    /// otherwise the edge is stamped with the default scope and is invisible
    /// to readers of the nodes' own scope.
    #[serde(default)]
    scope: Option<String>,
```

In `fn link` (:758), replace the two-line comment and `let scope = self.resolve_scope(None)?;` with:

```rust
        let scope = self.resolve_scope(p.scope.as_deref())?;
```

Update the now-stale doc comment on `resolve_scope` (`server.rs:83-85`) — delete the sentence beginning *"`link` has no `scope` param on the wire (per the plan's tool table) and always calls this with `None`…"* and replace with:

```rust
    /// Every write tool (`create_memory`, `create_entity`, `link`) passes its
    /// optional `scope` param through here; `None` resolves to the server's
    /// configured default write scope.
```

Update the `submit_batch` tool description (:884): change the `link` entry from
`link { from, to, type, props?, valid_from? }` to
`link { from, to, type, scope?, props?, valid_from? }`.

- [ ] **Step 6: Verify the whole workspace still builds and passes**

PowerShell: `cargo test --workspace`
Expected: PASS. (The MCP-level cross-scope integration assertion is added in Task 4, once a multi-scope reader exists to observe the edge with.)

- [ ] **Step 7: Commit**

```bash
git add crates/topodb-json/src/batch.rs crates/topodb-mcp/src/server.rs
git commit -m "fix(mcp): let link stamp an explicit scope

LinkParams had no scope field and the batch DSL's link op ignored one, so
every edge was stamped with the server's default write scope. An edge
attached to a node in another scope was therefore invisible to readers of
that scope - shared memories became disconnected islands, and it failed
silently because search_memories still returned the node's text while
traverse never crossed the boundary."
```

---

### Task 3: `--read-scopes` CLI flag and the server's default read set

**Files:**
- Modify: `crates/topodb-mcp/src/config.rs` — module docs (:1-13), `Config` (:28-38), `from_args` (:72-126), usage string (:98)
- Modify: `crates/topodb-mcp/src/server.rs` — `TopoServer::new` (:51-60)
- Test: `crates/topodb-mcp/src/config.rs` `mod tests`

**Interfaces:**
- Consumes: `topodb_json::scopes_to_scope_set(&[Scope]) -> ScopeSet` (Task 1); `parse_scope(&str) -> Result<Scope, Box<dyn Error>>` (existing, `config.rs:56`).
- Produces: `Config.default_read_scopes: Vec<Scope>` — a **non-empty** vec; defaults to `vec![default_scope]`. Task 4 reads it. `Config.default_scope: Scope` is unchanged and remains the default **write** scope.

- [ ] **Step 1: Write the failing tests**

Add to `mod tests` in `crates/topodb-mcp/src/config.rs`:

```rust
#[test]
fn read_scopes_defaults_to_the_write_scope() {
    let id = ScopeId::new();
    let s = id.to_string();
    let cfg = Config::from_args(argv(&["--db", "t.redb", "--scope", &s])).unwrap();
    assert_eq!(cfg.default_read_scopes, vec![Scope::Id(id)]);
}

#[test]
fn read_scopes_defaults_to_shared_when_scope_omitted() {
    let cfg = Config::from_args(argv(&["--db", "t.redb"])).unwrap();
    assert_eq!(cfg.default_read_scopes, vec![Scope::Shared]);
}

#[test]
fn read_scopes_parses_a_comma_separated_list() {
    let a = ScopeId::new();
    let list = format!("{a},shared");
    let cfg = Config::from_args(argv(&[
        "--db", "t.redb", "--scope", &a.to_string(), "--read-scopes", &list,
    ]))
    .unwrap();
    assert_eq!(
        cfg.default_read_scopes,
        vec![Scope::Id(a), Scope::Shared]
    );
    // The write scope is untouched by --read-scopes.
    assert!(matches!(cfg.default_scope, Scope::Id(got) if got == a));
}

#[test]
fn read_scopes_tolerates_whitespace_around_entries() {
    let a = ScopeId::new();
    let list = format!(" {a} , shared ");
    let cfg = Config::from_args(argv(&["--db", "t.redb", "--read-scopes", &list])).unwrap();
    assert_eq!(cfg.default_read_scopes, vec![Scope::Id(a), Scope::Shared]);
}

#[test]
fn read_scopes_rejects_a_bad_ulid() {
    assert!(Config::from_args(argv(&[
        "--db", "t.redb", "--read-scopes", "shared,not-a-ulid"
    ]))
    .is_err());
}

#[test]
fn read_scopes_rejects_an_empty_list() {
    // An empty read set admits nothing — there is no unscoped read, so this is
    // a caller error, not "read everything".
    assert!(Config::from_args(argv(&["--db", "t.redb", "--read-scopes", ""])).is_err());
    assert!(Config::from_args(argv(&["--db", "t.redb", "--read-scopes", " , "])).is_err());
}
```

`Scope` must derive `PartialEq` for `assert_eq!` on a `Vec<Scope>` — it already does (`crates/topodb/src/ids.rs:56` derives `PartialEq, Eq`).

- [ ] **Step 2: Run tests to verify they fail**

PowerShell: `cargo test -p topodb-mcp --lib read_scopes`
Expected: FAIL — `no field 'default_read_scopes' on type 'Config'`

- [ ] **Step 3: Implement the config change**

In `crates/topodb-mcp/src/config.rs`, add the field to `Config`:

```rust
    /// The default read `ScopeSet`, as a non-empty list of scopes. Seeded from
    /// `--read-scopes`, or from `--scope` alone when that flag is omitted (so
    /// the single-scope behaviour every existing client relies on is preserved
    /// exactly). Distinct from `default_scope`, which is the single scope a
    /// *write* is stamped with — a read filters by a set, a write picks one.
    pub default_read_scopes: Vec<Scope>,
```

Add the parse helper below `parse_scope`:

```rust
/// Parses a `--read-scopes` value: a comma-separated list of `shared` / ULID
/// entries, whitespace around each entry ignored. Rejects an empty list — an
/// empty `ScopeSet` admits nothing, and "read nothing" is never what a caller
/// means (there is no unscoped read).
fn parse_read_scopes(s: &str) -> Result<Vec<Scope>, Box<dyn Error>> {
    let scopes = s
        .split(',')
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .map(parse_scope)
        .collect::<Result<Vec<_>, _>>()?;
    if scopes.is_empty() {
        return Err(format!(
            "--read-scopes value {s:?} is empty; expected a comma-separated list of \"shared\" or scope ULIDs"
        )
        .into());
    }
    Ok(scopes)
}
```

In `from_args`, add the local, the match arm, and the resolution. Add next to the other locals:

```rust
        let mut read_scopes: Option<String> = None;
```

Add the match arm next to `"--scope"`:

```rust
                "--read-scopes" => {
                    read_scopes = Some(
                        it.next()
                            .ok_or("--read-scopes requires a comma-separated <ulid|shared> list")?,
                    );
                }
```

Update the unknown-flag usage string to:

```rust
                        "unknown argument {other:?}; usage: topodb-mcp --db <path> [--scope <ulid|shared>] [--read-scopes <ulid|shared>[,...]] [--spec <spec.json>]"
```

After `default_scope` is resolved, add:

```rust
        let default_read_scopes = match read_scopes {
            Some(s) => parse_read_scopes(&s)?,
            None => vec![default_scope],
        };
```

and add `default_read_scopes,` to the returned `Config`.

Update the module doc block at the top of the file (:3-6) to document the new flag:

```rust
//! CLI: `topodb-mcp --db <path> [--scope <ulid|shared>]
//!      [--read-scopes <ulid|shared>[,...]] [--spec <spec.json>]`
//! - `--scope`: the default **write** scope — the scope a created node/edge is
//!   stamped with when a write tool omits `scope`. `"shared"` (case-insensitive)
//!   or omitted => [`Scope::Shared`]; any other value is parsed as a ULID.
//! - `--read-scopes`: the default **read** scope set — the scopes a read tool
//!   filters by when it omits `scope`/`scopes`. Comma-separated. Defaults to
//!   just `--scope`'s value, which is the single-scope behaviour every existing
//!   client relies on. A read filters by a *set*; a write picks *one* scope —
//!   hence two flags rather than one overloaded flag.
```

- [ ] **Step 4: Wire the server's default read set**

In `crates/topodb-mcp/src/server.rs`, in `TopoServer::new` (:52), replace

```rust
        let default_scopes = convert::scope_to_scope_set(config.default_scope);
```

with

```rust
        let default_scopes = convert::scopes_to_scope_set(&config.default_read_scopes);
```

Update the `default_scopes` field doc (:41-43) to:

```rust
    /// The configured default **read** set (from `--read-scopes`, or `--scope`
    /// alone), reused by every scoped read tool call that omits `scope`/`scopes`
    /// (see [`TopoServer::resolve_scopes`]).
```

- [ ] **Step 5: Run tests to verify they pass**

PowerShell: `cargo test --workspace`
Expected: PASS — new config tests plus every pre-existing test. If any pre-existing test fails, the change is not backwards compatible; stop and fix rather than editing the old test.

- [ ] **Step 6: Commit**

```bash
git add crates/topodb-mcp/src/config.rs crates/topodb-mcp/src/server.rs
git commit -m "feat(mcp): add --read-scopes for a multi-scope default read set

--scope keeps its exact meaning (the default WRITE scope). --read-scopes
seeds the default READ ScopeSet and defaults to --scope's single value, so
existing behaviour is unchanged. Two flags rather than one overloaded flag:
a read filters by a set, a write picks one scope."
```

---

### Task 4: `scopes: string[]` on the six read tools

**Files:**
- Modify: `crates/topodb-mcp/src/server.rs` — `resolve_scopes` (:67-76); the six read param structs `GetNodeParams` (:184), `FindByPropParams` (:205), `SearchMemoriesParams` (:233), `TraverseParams` (:287), `AccessStatsParams` (:315), `SearchVectorsParams` (:476); the six call sites (:547, :573, :597, :630, :662, :849)
- Create: `crates/topodb-mcp/tests/multi_scope.rs`

**Interfaces:**
- Consumes: `convert::scopes_to_scope_set` (Task 1); `link`'s `scope` param (Task 2); `Config.default_read_scopes` (Task 3).
- Produces: `TopoServer::resolve_scopes(&self, scope: Option<&str>, scopes: Option<&[String]>) -> Result<ScopeSet, ErrorData>` — **signature change**, all six call sites updated.

Precedence: `scopes` (if present and non-empty) → `scope` → server default read set.

- [ ] **Step 1: Write the failing integration test**

Create `crates/topodb-mcp/tests/multi_scope.rs`:

```rust
//! Multi-scope reads (P1): a client can read across a project scope *and*
//! `shared` in one call, and an edge can be stamped into an explicit scope so
//! the graph actually crosses a scope boundary.
//!
//! Shared spawn/JSON-RPC/deadline plumbing lives in `tests/common/mod.rs` —
//! see that module's docs for why every read is deadlined and the child is
//! always killed.

mod common;

use common::{structured_content, Server, DEFAULT_TIMEOUT};

/// A memory in scope A and a memory in `shared` are BOTH visible to a reader
/// whose set is {A, shared} — the capability that did not exist before P1.
#[test]
fn read_spans_project_scope_and_shared() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("multi.redb");
    let project = topodb::ScopeId::new().to_string();
    let read_list = format!("{project},shared");

    let mut server = Server::spawn(
        &db_path,
        &["--scope", project.as_str(), "--read-scopes", read_list.as_str()],
    );
    server.initialize(DEFAULT_TIMEOUT);

    // Written with no explicit scope => the default WRITE scope => project.
    server.call_tool_ok(
        "create_memory",
        serde_json::json!({ "content": "zzqqx project fact" }),
        DEFAULT_TIMEOUT,
    );
    // Written explicitly into shared.
    server.call_tool_ok(
        "create_memory",
        serde_json::json!({ "content": "zzqqx shared lesson", "scope": "shared" }),
        DEFAULT_TIMEOUT,
    );

    // The default read set is {project, shared} => both come back.
    let res = server.call_tool_ok(
        "search_memories",
        serde_json::json!({ "query": "zzqqx", "k": 10 }),
        DEFAULT_TIMEOUT,
    );
    let hits = structured_content(&res);
    let hits = hits["hits"].as_array().expect("hits should be an array");
    assert_eq!(
        hits.len(),
        2,
        "a {{project, shared}} read set must see BOTH memories, got: {hits:?}"
    );
}

/// Per-call `scopes` overrides the server default, and beats `scope`.
#[test]
fn per_call_scopes_param_overrides_the_default_and_beats_scope() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("percall.redb");
    let project = topodb::ScopeId::new().to_string();

    // Default read set is project-only (no --read-scopes).
    let mut server = Server::spawn(&db_path, &["--scope", project.as_str()]);
    server.initialize(DEFAULT_TIMEOUT);

    server.call_tool_ok(
        "create_memory",
        serde_json::json!({ "content": "wwvvu project fact" }),
        DEFAULT_TIMEOUT,
    );
    server.call_tool_ok(
        "create_memory",
        serde_json::json!({ "content": "wwvvu shared lesson", "scope": "shared" }),
        DEFAULT_TIMEOUT,
    );

    // Default (project-only) sees 1.
    let res = server.call_tool_ok(
        "search_memories",
        serde_json::json!({ "query": "wwvvu", "k": 10 }),
        DEFAULT_TIMEOUT,
    );
    assert_eq!(structured_content(&res)["hits"].as_array().unwrap().len(), 1);

    // Explicit multi-scope read sees both. `scopes` wins over `scope`.
    let res = server.call_tool_ok(
        "search_memories",
        serde_json::json!({
            "query": "wwvvu",
            "k": 10,
            "scope": "shared",
            "scopes": [project.as_str(), "shared"]
        }),
        DEFAULT_TIMEOUT,
    );
    assert_eq!(
        structured_content(&res)["hits"].as_array().unwrap().len(),
        2,
        "`scopes` must take precedence over `scope`"
    );
}

/// THE REGRESSION TEST for the Task 2 bug: an edge explicitly stamped `shared`
/// must be traversable by a reader whose set includes `shared` — i.e. the graph
/// crosses a scope boundary. Before the `link` fix this edge would have been
/// stamped with the project scope and been invisible from any other project.
#[test]
fn a_shared_edge_is_traversable_from_a_multi_scope_reader() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("edge.redb");
    let project = topodb::ScopeId::new().to_string();
    let read_list = format!("{project},shared");

    let mut server = Server::spawn(
        &db_path,
        &["--scope", project.as_str(), "--read-scopes", read_list.as_str()],
    );
    server.initialize(DEFAULT_TIMEOUT);

    // Two nodes in `shared`, linked by an edge explicitly stamped `shared`.
    let a = server.call_tool_ok(
        "create_entity",
        serde_json::json!({ "name": "shared-a", "scope": "shared" }),
        DEFAULT_TIMEOUT,
    );
    let b = server.call_tool_ok(
        "create_entity",
        serde_json::json!({ "name": "shared-b", "scope": "shared" }),
        DEFAULT_TIMEOUT,
    );
    let a_id = structured_content(&a)["id"].as_str().unwrap().to_string();
    let b_id = structured_content(&b)["id"].as_str().unwrap().to_string();

    server.call_tool_ok(
        "link",
        serde_json::json!({
            "from_id": a_id, "to_id": b_id, "edge_type": "about", "scope": "shared"
        }),
        DEFAULT_TIMEOUT,
    );

    // Traverse from A across `shared` — the edge must be visible.
    // NB: traverse's params are `seed_id` / `max_hops` (see TraverseParams,
    // server.rs:287), and its result is `{ "subgraph": {...} }`.
    let res = server.call_tool_ok(
        "traverse",
        serde_json::json!({ "seed_id": a_id, "max_hops": 1, "scopes": ["shared"] }),
        DEFAULT_TIMEOUT,
    );
    let body = structured_content(&res)["subgraph"].to_string();
    assert!(
        body.contains(&b_id),
        "a `shared`-scoped edge must be traversable by a reader of `shared`; \
         got subgraph: {body}"
    );
}
```

- [ ] **Step 2: Run to verify it fails**

PowerShell: `cargo test -p topodb-mcp --test multi_scope`
Expected: FAIL — the server rejects `--read-scopes` (Task 3 must be merged first) or rejects the unknown `scopes` param.

- [ ] **Step 3: Change `resolve_scopes` to accept a list**

In `crates/topodb-mcp/src/server.rs`, replace `resolve_scopes` (:62-76) with:

```rust
    /// Resolves a read tool's optional `scope` / `scopes` params to the
    /// [`ScopeSet`] the read runs against. Precedence:
    ///
    /// 1. `scopes` (non-empty) → a genuine multi-member set. This is the only
    ///    way a client can read across e.g. a project scope *and* `shared`.
    /// 2. `scope` → a one-member set (the pre-P1 behaviour).
    /// 3. neither → the server's configured default read set (`--read-scopes`,
    ///    or `--scope` alone), pre-resolved once in `new` rather than re-derived
    ///    on every call — the common case.
    ///
    /// An explicitly empty `scopes: []` is rejected: an empty set admits
    /// nothing, so it is a caller error, never "read everything" (there is no
    /// unscoped read).
    fn resolve_scopes(
        &self,
        scope: Option<&str>,
        scopes: Option<&[String]>,
    ) -> Result<ScopeSet, ErrorData> {
        match scopes {
            Some(list) if list.is_empty() => Err(ErrorData::invalid_params(
                "`scopes` must not be empty (an empty scope set admits nothing); \
                 omit it to use the server's default read scopes"
                    .to_string(),
                None,
            )),
            Some(list) => {
                let resolved = list
                    .iter()
                    .map(|s| convert::resolve_scope(Some(s), self.default_scope))
                    .collect::<Result<Vec<Scope>, String>>()
                    .map_err(|e| ErrorData::invalid_params(e, None))?;
                Ok(convert::scopes_to_scope_set(&resolved))
            }
            None => match scope {
                None => Ok(self.default_scopes.clone()),
                Some(_) => {
                    let resolved = convert::resolve_scope(scope, self.default_scope)
                        .map_err(|e| ErrorData::invalid_params(e, None))?;
                    Ok(convert::scope_to_scope_set(resolved))
                }
            },
        }
    }
```

- [ ] **Step 4: Add the `scopes` param to all six read structs**

Add this field to **each** of `GetNodeParams` (:184), `FindByPropParams` (:205), `SearchMemoriesParams` (:233), `TraverseParams` (:287), `AccessStatsParams` (:315), `SearchVectorsParams` (:476), directly below each struct's existing `scope` field:

```rust
    /// Read across SEVERAL scopes at once: a list of `"shared"` / scope ULIDs
    /// (e.g. a project scope plus `"shared"`). Takes precedence over `scope`.
    /// Omit both to use the server's configured default read scopes.
    #[serde(default)]
    scopes: Option<Vec<String>>,
```

Then update all six call sites (:547, :573, :597, :630, :662, :849) from

```rust
        let scopes = self.resolve_scopes(p.scope.as_deref())?;
```

to

```rust
        let scopes = self.resolve_scopes(p.scope.as_deref(), p.scopes.as_deref())?;
```

> The local binding is already named `scopes`, which now shadows the param name. That compiles, but rename the local to `scope_set` at each site for readability, and update its uses in that function body.

- [ ] **Step 5: Run tests to verify they pass**

PowerShell: `cargo test --workspace`
Expected: PASS — `multi_scope.rs`'s three tests plus every pre-existing test.

- [ ] **Step 6: Commit**

```bash
git add crates/topodb-mcp/src/server.rs crates/topodb-mcp/tests/multi_scope.rs
git commit -m "feat(mcp): add scopes[] param to the six read tools

Precedence: scopes > scope > server default read set. This is the only way
a client can read across a project scope AND shared in one call - the engine
has always had a multi-member ScopeSet, but nothing could construct one.

Includes the regression test for the link-scope bug: a shared-scoped edge
must be traversable by a reader of shared."
```

---

### Task 5: gate `get_changes` behind `--allow-unscoped-changes`

`get_changes`'s own description says it is *"the ONE unscoped read; the log spans all scopes."* That is correct and useful for a sync/consolidation host with a per-project db. But the Claude Code plugin (spec §2) puts **every project in one db**, where an agent calling `get_changes(since_seq: 0)` would replay every other project's writes into its context. Default it off; let real sync hosts opt in.

**Files:**
- Modify: `crates/topodb-mcp/src/config.rs` — `Config`, `from_args`, usage string, module docs
- Modify: `crates/topodb-mcp/src/server.rs` — `TopoServer` struct + `new`, `fn get_changes` (:684), its `#[tool(description)]` (:681-683)
- Test: `crates/topodb-mcp/src/config.rs` `mod tests`; `crates/topodb-mcp/tests/multi_scope.rs`

**Interfaces:**
- Produces: `Config.allow_unscoped_changes: bool` (default `false`); `TopoServer.allow_unscoped_changes: bool`.

- [ ] **Step 1: Write the failing tests**

Add to `mod tests` in `crates/topodb-mcp/src/config.rs`:

```rust
#[test]
fn unscoped_changes_is_off_by_default() {
    let cfg = Config::from_args(argv(&["--db", "t.redb"])).unwrap();
    assert!(!cfg.allow_unscoped_changes);
}

#[test]
fn unscoped_changes_flag_is_a_bare_toggle() {
    let cfg =
        Config::from_args(argv(&["--db", "t.redb", "--allow-unscoped-changes"])).unwrap();
    assert!(cfg.allow_unscoped_changes);
}
```

Add to `crates/topodb-mcp/tests/multi_scope.rs`:

`expect_tool_error(resp: &serde_json::Value)` (`tests/common/mod.rs:287`) **asserts and returns `()`** — it does not hand back the message. So assert the flag is named by stringifying the response itself.

```rust
/// `get_changes` is the one unscoped read — it spans every scope in the db.
/// In a db shared across projects that is a cross-project leak, so it is off
/// unless the host explicitly opts in.
#[test]
fn get_changes_is_gated_unless_explicitly_allowed() {
    use common::expect_tool_error;

    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("gate.redb");

    // Without the flag: the call is a tool error naming the flag.
    let mut server = Server::spawn(&db_path, &["--scope", "shared"]);
    server.initialize(DEFAULT_TIMEOUT);
    let resp = server.call_tool(
        "get_changes",
        serde_json::json!({ "since_seq": 0 }),
        DEFAULT_TIMEOUT,
    );
    expect_tool_error(&resp);
    let body = resp.to_string();
    assert!(
        body.contains("--allow-unscoped-changes"),
        "the error must name the flag that enables it; got: {body}"
    );
    drop(server); // release the db file before reopening it below

    // With the flag: it works.
    let mut server = Server::spawn(
        &db_path,
        &["--scope", "shared", "--allow-unscoped-changes"],
    );
    server.initialize(DEFAULT_TIMEOUT);
    let res = server.call_tool_ok(
        "get_changes",
        serde_json::json!({ "since_seq": 0 }),
        DEFAULT_TIMEOUT,
    );
    assert!(structured_content(&res).get("ops").is_some());
}
```

`Server::call_tool(name, arguments, timeout) -> serde_json::Value` (`tests/common/mod.rs:204`) returns the raw response without asserting success — that is the one to use when you expect an error.

- [ ] **Step 2: Run to verify they fail**

PowerShell: `cargo test -p topodb-mcp unscoped`
Expected: FAIL — `no field 'allow_unscoped_changes' on type 'Config'`

- [ ] **Step 3: Implement the config flag**

In `crates/topodb-mcp/src/config.rs`, add to `Config`:

```rust
    /// Opt-in for `get_changes`, the one unscoped read — the op log spans every
    /// scope in the db, so in a db shared across projects it is a cross-project
    /// read of everything. Off unless the host explicitly asks for it. Sync and
    /// consolidation hosts, which legitimately need the whole log, pass
    /// `--allow-unscoped-changes`.
    pub allow_unscoped_changes: bool,
```

Add the local `let mut allow_unscoped_changes = false;`, the match arm:

```rust
                "--allow-unscoped-changes" => {
                    allow_unscoped_changes = true;
                }
```

add `allow_unscoped_changes,` to the returned `Config`, and extend the usage string to end with `[--allow-unscoped-changes]`. Add a bullet to the module docs describing it.

- [ ] **Step 4: Gate the tool**

In `crates/topodb-mcp/src/server.rs`, add the field to `TopoServer`:

```rust
    /// See `Config::allow_unscoped_changes`.
    allow_unscoped_changes: bool,
```

set it in `new` from `config.allow_unscoped_changes`, and add this as the first statement in `fn get_changes` (:684, before `self.db.ops_since(...)`):

```rust
        if !self.allow_unscoped_changes {
            return Err(ErrorData::invalid_params(
                "get_changes is disabled: it is the one unscoped read (the op log \
                 spans every scope in the db), so it is off by default. Restart \
                 topodb-mcp with --allow-unscoped-changes to enable it."
                    .to_string(),
                None,
            ));
        }
```

Amend the tool description (:682) so a client isn't told it can call something it can't — append to the existing string:

```
 Disabled unless the server was started with --allow-unscoped-changes.
```

- [ ] **Step 5: Run tests to verify they pass**

PowerShell: `cargo test --workspace`
Expected: PASS.

> If a pre-existing test in `e2e.rs` or `plan6.rs` calls `get_changes`, it will now fail. That is expected and is **the one sanctioned exception** to "existing tests pass unmodified": add `--allow-unscoped-changes` to that test's spawn args (it is a sync-host-style test and legitimately wants the log). Do not weaken the gate.

- [ ] **Step 6: Commit**

```bash
git add crates/topodb-mcp/src/config.rs crates/topodb-mcp/src/server.rs crates/topodb-mcp/tests/multi_scope.rs
git commit -m "feat(mcp): gate get_changes behind --allow-unscoped-changes

get_changes is the one unscoped read - the op log spans every scope. Fine
for a per-project db; in a db shared across projects it lets any agent
replay every other project's writes. Off by default; sync/consolidation
hosts opt in explicitly. Scope-filtering the log was rejected: a partial
log can't be replayed deterministically, which breaks the tool's contract."
```

---

### Task 6: release `topodb-mcp` 0.0.4

**Files:**
- Modify: `crates/topodb-mcp/Cargo.toml` (`version = "0.0.3"` → `"0.0.4"`)
- Modify: `npm/topodb-mcp/package.json` (`"version": "0.0.2"` → `"0.0.4"` — align it with the crate; it has drifted)
- Modify: `README.md` — document `--read-scopes`, `--allow-unscoped-changes`, `scopes[]`, and `link`'s `scope`

**Interfaces:** none — release mechanics only.

- [ ] **Step 1: Confirm the npm version drift before changing it**

PowerShell: `Select-String -Path npm/topodb-mcp/package.json -Pattern '"version"'`
The crate is `0.0.3` and the npm package reads `0.0.2`. **Do not assume they should match** — read `npm/topodb-mcp/bin/` and any release workflow in `.github/workflows/` first to see whether the npm version is set independently or derived from a tag. Follow whatever the release workflow actually does; if the workflow derives it, leave `package.json` alone and only bump the crate.

- [ ] **Step 2: Bump the crate version**

In `crates/topodb-mcp/Cargo.toml`: `version = "0.0.4"`.

- [ ] **Step 3: Document the new surface in `README.md`**

Add to the MCP section: the two new flags with one line each, the `scopes[]` read param, and `link`'s new `scope` param. State plainly that reads filter by a *set* of scopes and writes are stamped with *one* — that asymmetry is the thing users will otherwise get wrong.

- [ ] **Step 4: Verify the whole workspace is green**

PowerShell: `cargo test --workspace`
Expected: PASS.

PowerShell: `cargo clippy --workspace --all-targets -- -D warnings`
Expected: no warnings.

PowerShell: `cargo fmt --all --check`
Expected: no diff.

- [ ] **Step 5: Commit**

```bash
git add crates/topodb-mcp/Cargo.toml npm/topodb-mcp/package.json README.md
git commit -m "chore(mcp): release 0.0.4 - multi-scope reads, link scope, get_changes gate"
```

- [ ] **Step 6: Stop. Do not publish.**

Publishing to npm/crates.io and tagging are **not** part of this plan. Report the branch state and hand back — the human decides when to cut the release, and the `NPM_TOKEN` handling is theirs.

---

## Out of scope for P1

- The `@topodb/claude-code` plugin itself (spec §2) — a separate plan, depends on this one.
- Hooks / the Episode Recorder (spec §3).
- The stale crates.io `topodb` engine version noted in spec §1 — flag it to the human; it is a separate release decision.
