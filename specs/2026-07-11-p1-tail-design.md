# Design — closing P1's tail (D1, D2, D4, D5)

- **Date:** 2026-07-11
- **Status:** Approved, not yet planned or built.
- **Scope:** Make `topodb-mcp` 0.0.4 a coherent, releasable artifact by closing four
  of the five parity debts in `specs/2026-07-10-claude-code-plugin-design.md` §2.5.
  **D3 is deliberately excluded** — see §6.
- **Precedes:** P2 (`@topodb/claude-code`), which then inherits only D3.

> `docs/` is gitignored in this repo. Specs live under `specs/`.

---

## 0. Why this exists

P1 (multi-scope MCP reads) merged as `0db2d5f` and bumped `topodb-mcp` to 0.0.4.
It is **not tagged and not published**. Its final review surfaced five debts. Four
of them are cheap, and two of those are the difference between a release that is
coherent and one that ships a self-contradicting CLI plus an unannounced breaking
change. Those four are closed here, before the release is cut.

### Two claims in §2.5 are false, and are corrected here

Both were checked against the code, not taken on faith. `specs/2026-07-10-claude-code-plugin-design.md`
§2.5 is amended in place to match.

**D1's provenance was wrong.** §2.5 states: *"That divergence did not exist before
P1 — P1 created it."* It did exist. Before P1, `topodb-json/src/batch.rs`'s
`scope_of` was documented as *"Resolves a command's `scope` field (**only
create_memory/create_entity carry one**)"*, and `topodb-cli`'s `create-memory` /
`create-entity` subcommands have **never** had a `--scope` flag. So the batch-DSL
vs. subcommand divergence predates P1 by two ops. P1 added `scope` to the DSL's
`link` op and thereby **widened an inherited divergence from two ops to three** —
it did not create one.

This is a less damning story but a **larger fix**: patching only `link` would leave
`create-memory` and `create-entity` divergent, i.e. would knowingly preserve the
exact inconsistency we are here to remove.

**D4's urgency was wrong.** §2.5 states: *"The plugin's launcher will be a second
constructor of these args."* It will not. The launcher execs
`topodb-mcp --db … --scope … --read-scopes …` — it produces an **argv**, which
still flows through `Config::from_args` and is validated there. It is not a second
constructor of `Config`. `Config::from_args` remains the sole constructor
(`crates/topodb-mcp/src/main.rs:26`); no test hand-builds a `Config` either. **P2
does not make D4 live.** D4 is fixed below anyway, but on its own merits (§3), not
on a false deadline.

---

## 1. D1 — per-command `--scope` on the CLI's three scope-stamping writes

### The change

Add an optional `--scope <shared|ULID>` to three `topodb-cli` subcommands:

| Subcommand | Today | After |
|---|---|---|
| `create-memory` | global `--scope` only | `--scope` overrides it |
| `create-entity` | global `--scope` only | `--scope` overrides it |
| `link` | global `--scope` only | `--scope` overrides it |

Semantics are exactly those an MCP tool's `scope` param already has against the
server's default write scope: **present → use it; absent → fall back to the global
`--scope`.** A write lands in exactly one scope; that is unchanged.

Resolution goes through the existing `topodb_json::resolve_scope(Option<&str>,
default)` that `batch.rs` already calls — one implementation, not a second one that
can drift.

### What deliberately gets nothing

`set-props`, `remove-node`, `close-edge`, and `set-embedding` address an existing
node or edge **by id** and stamp no scope of their own. They take no `--scope`.
Adding one would imply a scope-move operation that does not exist.

### Docs

`crates/topodb-cli/README.md:84` claims *"There's no per-command `--scope` override
in v1."* It is already false for `submit`'s three ops. Rewrite it to describe the
real rule: a global default, overridable per command on the three ops that stamp a
scope.

### Tests

For each of the three subcommands:

- explicit `--scope <ulid>` stamps that scope, **not** the global `--scope`
- `--scope` omitted falls back to the global `--scope`
- a malformed `--scope` value exits 2

Plus the regression test that carries the actual risk, mirroring the MCP one P1
already has:

- an edge created with `topodb-cli link --scope shared` between two `shared` nodes
  is **visible from a read set of `{other-project, shared}`** — the "disconnected
  islands" bug, tested from the CLI side.

---

## 2. D2 — document the CLI/MCP asymmetry; do not gate `topodb-cli changes`

### The decision

`topodb-cli changes` **stays ungated.** No `--allow-unscoped-changes` on the CLI.

### The reasoning, which must be stated rather than assumed

The MCP gate exists because **MCP advertises `get_changes` in the model's tool
list.** An agent trips over it while doing something else; §2.5's own words are
*"arrived at by accident rather than by choice."* The gate is **accident-prevention,
not a security boundary.**

The CLI advertises nothing to a model. Reaching `topodb-cli changes` takes
deliberate intent, and its operator already holds the database file — anyone who can
run `topodb-cli --db X changes` can equally `cat X`.

Gating the CLI would therefore **imply a security property TopoDB cannot deliver**,
and an implied-but-false boundary is worse than a stated limitation.

### The accepted risk, written down

> **An agent with shell access bypasses the MCP gate entirely** — by invoking
> `topodb-cli changes` against the same database file, or by reading the `.redb`
> directly. `--allow-unscoped-changes` stops accidents, not attackers. If a future
> host drives `topodb-cli` from an agent loop, this decision must be revisited.

### Where it goes

- `crates/topodb-cli/README.md` — a short subsection under `changes`.
- The `Changes` doc comment in `crates/topodb-cli/src/cli.rs` (currently lines
  119–124), which already says *"Unscoped host-level primitive — spans every
  scope"* and should say **why that is safe here and gated there.**

No code change.

---

## 3. D4 — a `ReadScopes` newtype

### The change

In `crates/topodb-mcp/src/config.rs`, introduce:

```rust
/// A non-empty set of scopes to read under. The invariant is structural:
/// there is no unscoped read, and an empty set admits nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadScopes(Vec<Scope>);

impl ReadScopes {
    /// Rejects an empty list — "read nothing" is never what a caller means.
    pub fn new(scopes: Vec<Scope>) -> Result<Self, Box<dyn Error>> { … }
    pub fn as_slice(&self) -> &[Scope] { &self.0 }
}
```

`Config.default_read_scopes` becomes `ReadScopes`. The **second, untyped copy** at
`crates/topodb-mcp/src/server.rs:51` becomes `ReadScopes` too.

### Why, honestly

This is the weakest of the four on urgency, and the design should say so rather than
inflate it:

- It is **latent** — `Config::from_args` is the only constructor.
- It **fails closed** — an empty read set returns nothing, loudly and immediately.
  That is the *opposite* of this codebase's characteristic quiet failure. §2.5 files
  D4 next to D1 and D3 as if it were the same species. It is not.
- `topodb-mcp` is **bin-only**, so no external crate can construct a `Config` at all.

It is fixed anyway because the invariant is **currently assumed in two places
(`config.rs`, `server.rs`) and enforced in zero past the parser**, and *"there is no
unscoped read"* is load-bearing for the entire scoping story. A newtype single-sources
it for about thirty lines. That is the whole argument — cheap insurance on a
load-bearing property, not an emergency.

### Impact

Purely internal. No wire change, no flag change, **no version bump.**

### Tests

- `ReadScopes::new(vec![])` is an error.
- The existing `--read-scopes` parse tests keep passing through the new type
  (including the empty-list rejection at `config.rs:321`, which now has a second
  line of defence rather than being the only one).

---

## 4. D5 — a `CHANGELOG.md`

### Placement

**One `CHANGELOG.md` at the repo root**, sections keyed by package. The repo already
tags per-package (`topodb-mcp-v0.0.3`, `topodb-pi-v0.0.2`), so per-package sections
match how releases are actually cut, while a single file stays discoverable — npm and
crates.io readers land on the repo anyway. Keep-a-Changelog format.

### Where it starts, and why there is a gap on purpose

**The changelog starts at the next release. Earlier versions get one honest line
saying they predate it.** Reconstructing 0.0.1–0.0.3 from git log invites guessing,
and a changelog that quietly fabricates history is worse than one that admits where
it begins.

### `topodb-mcp` 0.0.4 — lead with the breaking change

This is the item that bites a real user today.

- **BREAKING — `get_changes` now requires `--allow-unscoped-changes`.** Without the
  flag the server returns `invalid_params`. Any existing client calling `get_changes`
  **breaks on upgrade from 0.0.3.** Rationale: in a database shared across projects,
  replaying the op log hands one project every other project's writes. Migration:
  start the server with `--allow-unscoped-changes`.
- **Added** — `--read-scopes <list>`, defining the default read `ScopeSet`
  (defaults to `--scope`, so existing single-scope behaviour is preserved exactly).
- **Added** — a `scopes: string[]` param on the six read tools (`get_node`,
  `find_by_prop`, `search_memories`, `traverse`, `access_stats`, `search_vectors`).
  Precedence: `scopes` > `scope` > server default read set.
- **Added** — `scope` on the `link` tool and on the batch DSL's `link` op, so an
  edge can cross a scope boundary.
- **Fixed** — write tools **silently accepted and ignored** a `scopes` param;
  `create_memory {scopes:["shared"]}` returned success and wrote to the *project*
  scope. All 15 param structs now `deny_unknown_fields`.
- **Fixed** — `db_info` reported only the write scope, so an agent following the
  server's own instructions would pass `scope:"shared"` and silently **narrow** its
  reads. It now reports the default read set.

### `topodb-cli` 0.0.2

- **Added** — `--scope` on `create-memory`, `create-entity`, and `link` (§1).
- **Docs** — the `changes` command's unscoped read is documented as deliberate (§2).

---

## 5. Version impact

| Package | Now | After | Why |
|---|---|---|---|
| `topodb-cli` | 0.0.1 | **0.0.2** | D1 adds flags |
| `topodb-mcp` | 0.0.4 | **0.0.4** | D4 is internal; 0.0.4 is not yet released |
| `topodb` | 0.0.3 | 0.0.3 | untouched |
| `topodb-json` | 0.0.1 | 0.0.1 | untouched |

**Separate, still-open loose end (not fixed here):** engine `topodb` 0.0.3 on
crates.io is **stale** — it was published under a version number whose fix landed
later. It needs a bump and its own `cargo publish`. Recorded so it is not lost.

---

## 6. Explicitly not in scope

**D3 — an edge's scope is never validated against its endpoints'.**
`crates/topodb/src/storage.rs:941-955` validates the endpoints' cross-scope rule but
stamps the edge's own scope unconstrained, so `link(from=<shared A>, to=<shared B>,
scope=<unrelated project>)` commits happily and produces an edge **no reader can ever
traverse.** This is a **behaviour change to the `topodb` engine**, belongs in an
engine release rather than riding along with CLI flags and a changelog, and needs its
own brainstorm. It stays open and is P2's — or an engine release's — to carry.

**Everything in §2 of the plugin design** (the `@topodb/claude-code` plugin itself).

---

## 7. Verification

The CI gate is `.github/workflows/ci.yml`. Run all three, and quote the output:

```powershell
cargo fmt --all --check;                                  $LASTEXITCODE
cargo clippy --workspace --all-targets -- -D warnings;    $LASTEXITCODE
cargo test --workspace | Out-Null;                        $LASTEXITCODE
```

Two traps this repo has already sprung, recorded so they are not re-learned:

- **`cargo` is not on the Bash PATH** on this machine. Use the PowerShell tool.
- **`2>&1` on a native exe fakes a nonzero exit** in PowerShell 5.1 — `cargo test`
  printed `0 failed` everywhere and still exited 255. Pipe to `Out-Null` and read
  `$LASTEXITCODE`.
- `topodb-mcp` is **bin-only**: `--lib` does not exist; use `--bins`.

---

## 8. Follow-on

After this lands and the gate is green: **push** (22 commits currently sit only on
the local `main`), then cut the 0.0.4 release, then plan P2 — which by then carries
only D3.
