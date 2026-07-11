# Design — Multi-scope MCP reads + the Claude Code plugin

- **Date:** 2026-07-10 (updated 2026-07-11)
- **Status:** **P1 is DONE** — merged to `main` as `0db2d5f`, `topodb-mcp` 0.0.4,
  not yet published/tagged. **P2 is designed, not yet planned or built.**
- **Scope:** Two sub-projects, sequential. P1 shipped first and stands alone.
  P2 now also carries five parity debts inherited from P1's review — see §2.5.

> Note: `docs/` is gitignored in this repo, so specs live under `specs/`.

---

## 0. Context and the decision that shapes this

Claude Code already speaks MCP natively — `claude mcp add topodb -- npx @topodb/topodb-mcp`
works today with no code from us. So a Claude Code integration only earns its
existence on what raw MCP cannot do: a one-command install, a skill that teaches
the agent *when* to use memory, and a sane default memory model.

**Product identity (decided):** TopoDB is **infrastructure**. Pi and Claude Code
are **thin host adapters**. The "self-improving harness" is the demo, not the
business. This retires the blocker recorded in `topodb-pro/strategy/VISION.md`
§"Gaps to close" #3.

Two consequences follow directly, and both are load-bearing:

1. **The Episode Recorder does not live in this plugin.** If adapters are thin,
   a recorder built inside the Claude Code plugin would have to be rebuilt for
   Pi, and for every host after it. When it is built, it is a **host-agnostic
   core** with a thin per-host feeder; Claude Code's hooks become that feeder.
   Hooks are therefore **explicitly out of scope for v0.1** — see §3.
2. **Capability gaps belong in the engine, not the adapter.** P1 exists because
   of this.

---

## 1. P1 — Multi-scope reads in `topodb-mcp`

### The gap

A scope is either `shared` or a ULID (`topodb-mcp/src/config.rs:56`). Scope
resolution is single-valued:

```rust
// crates/topodb-json/src/lib.rs:311
pub fn scope_to_scope_set(scope: Scope) -> ScopeSet {
    match scope {
        Scope::Shared => ScopeSet::default().with_shared(),
        Scope::Id(id) => ScopeSet::of(&[id]),   // include_shared = false
    }
}
```

So launching with `--scope <ulid>` makes every read see **only** that scope —
`shared` becomes invisible. The per-call `scope` argument is also a single
`Scope`, so a caller cannot request "this project **plus** shared" either.

**Through the MCP server as it exists today, there is no way to read across two
scopes** — even though the engine's central read type, `ScopeSet`, is precisely
a `bool` (include shared) plus a `BTreeSet<ScopeId>`. The capability exists in
the engine and is not exposed by any client. This is a gap in the shipped
product, not a Claude Code problem.

### The change

The server already separates the two paths, so this is surgical rather than
architectural (`topodb-mcp/src/server.rs:67`, `:87`):

- `resolve_scopes(Option<&str>) -> ScopeSet` — **reads**
- `resolve_scope(Option<&str>) -> Scope` — **writes**

**CLI — two flags, two concerns.**

- `--scope <shared|ULID>` — **unchanged**. The default **write** scope.
- `--read-scopes <list>` — **new**. Comma-separated `shared | <ULID>`, defining
  the default read `ScopeSet`. Defaults to the single value of `--scope`.

An earlier draft overloaded `--scope` with a list whose *first element silently
became the write scope*. That was rejected: one flag carrying two meanings, with
order-dependent semantics nobody would predict (`--scope shared,<ulid>` vs
`--scope <ulid>,shared` would differ invisibly). Two flags cost nothing and are
backwards compatible by definition.

**MCP read tools.** Add an optional `scopes: string[]` alongside the existing
`scope: string`. Precedence:

1. `scopes` present → build a real multi-member `ScopeSet`
2. else `scope` present → one-member set (today's behavior)
3. else → the server's configured default read set

Read tools with a `scope` param today: `get_node`, `find_by_prop`,
`search_memories`, `traverse`, `access_stats`, `search_vectors`.

**MCP write tools.** A write lands in exactly one scope; that asymmetry with
reads is intentional and already modelled in the code. But one write tool is
**missing** its scope, and it is a correctness bug for §2:

> **`link` has no `scope` param on the wire** (`server.rs:400`, `LinkParams`),
> and neither does the `link` op in the `submit_batch` DSL. Every edge is
> therefore stamped with the server's default write scope. Under §2 that is the
> *project* scope — so an edge attached to a `shared` node would be
> project-scoped and **invisible from every other project**. Shared memories
> would become disconnected islands: `search_memories` would still surface the
> node's text, but `traverse` would not cross projects. The feature would appear
> to work and quietly not.

**Fix: add `scope: Option<String>` to `LinkParams` and a `scope?` field to the
batch DSL's `link` op**, resolving through the existing `resolve_scope` write
path. Without this, §2's cross-project memory model does not function.

**`get_changes` — gate the unscoped read.** Its own description states it is
*"the ONE unscoped read; the log spans all scopes."* Correct for a per-project
db. Under §2's **global** db it means any project's agent can call
`get_changes(since_seq: 0)` and replay every other project's writes verbatim —
cross-project context contamination and a token bomb, arrived at by accident
rather than by choice.

**Fix: add `--allow-unscoped-changes` (default OFF).** When off, `get_changes`
returns `invalid_params` explaining the flag. Sync/consolidation hosts that
genuinely need the op log opt in explicitly; the Claude Code plugin does not set
it. The primitive is preserved, the accident is removed. Scope-*filtering* the
op log was considered and rejected — a partial log cannot be replayed
deterministically, which would break the tool's actual contract.

**Typing.** Every param is explicitly typed. No `serde_json::Value` — that was
the bug fixed in `topodb-mcp@0.0.3` (it made several tools uncallable from any
client) and it will not be reintroduced in a new form.

**Errors.** An unparseable ULID anywhere in a list → `invalid_params`.

### Tests

- config parse: `--scope` alone (back-compat), `--read-scopes` list, list
  containing `shared`, empty string, malformed ULID, `--read-scopes` absent
  (defaults to `--scope`)
- `ScopeSet` construction from a list, including the `shared` flag
- read-tool precedence: `scopes` > `scope` > server default read set
- writes land in `--scope` regardless of `--read-scopes`
- **`link` with an explicit `scope` stamps that scope**, and a `shared` edge is
  visible from a read set of `{other-project, shared}` — the regression test for
  the "disconnected islands" bug above
- `link` with `scope` omitted still resolves to the default write scope
- batch DSL `link { scope }` behaves identically to the `link` tool
- `get_changes` returns `invalid_params` without `--allow-unscoped-changes`, and
  replays the log with it
- existing `topodb-mcp` tests pass unmodified

### Ships as

`topodb-mcp` 0.0.4 → npm (5 platform binaries).

**Related loose end, worth doing in the same pass:** crates.io is stale. Engine
`topodb` 0.0.3 there predates the shipped fix (the fix went out under the *same*
already-published version), so it needs a bump to 0.0.4 and a `cargo publish`.

---

## 2. P2 — `@topodb/claude-code`

Depends on P1.

### Memory model

One database at `${CLAUDE_PLUGIN_DATA}/memory.redb` (survives plugin updates).
Two scopes are live in every session:

| Scope | Holds |
|---|---|
| **project** — a ScopeId derived from the project path | facts about *this* repo |
| **`shared`** | durable, transferable, cross-project lessons |

The server launches with:

```
--scope <project-ulid> --read-scopes <project-ulid>,shared
```

**Reads see both. Writes default to the project scope.** The bundled skill
instructs the agent to pass `scope: "shared"` explicitly when a lesson
generalizes beyond the current repo — on `create_memory`, `create_entity`, **and
`link`** (see §1: `link` gains a `scope` param precisely so shared lessons can
carry their relationships across projects).

`--allow-unscoped-changes` is **not** passed, so `get_changes` — the one unscoped
read — is unavailable to the agent and cannot leak other projects' op logs into
this session's context.

This cross-project recall is the entire reason to prefer a global database over
Pi's per-project `.topodb/`, and **P1 is what makes it possible** — without it,
this model silently degrades to per-project isolation with a larger blast radius
and no upside.

### Scope derivation

```
ScopeId = ULID(first 16 bytes of sha256(canonical absolute project path))
```

No registry file, therefore: no state to corrupt, no write race between
concurrent sessions in different repos, reproducible across machines and
reinstalls, and recomputed cheaply on every launch.

This is a sanctioned pattern, not a hack. `ScopeId::from_u128` exists for it —
`crates/topodb/src/ids.rs:22`:

> *"Deterministic constructor for tests/fixtures and hosts that need a stable,
> reproducible id (e.g. derived from an external key) rather than `Ulid::new()`'s
> wall-clock-derived randomness."*

**Cross-language golden test (required).** The plugin computes the ULID string in
TypeScript; the engine parses it in Rust. A fixture pins the TS Crockford-base32
encoder against the Rust `Display` output for known 128-bit values. "The two
implementations disagree about one byte" is exactly the failure this prevents.

**To verify during implementation, not assume:** nothing in topodb orders or
interprets `ScopeId` by its ULID timestamp prefix (a hash-derived id has a
meaningless one). `BTreeSet<ScopeId>` ordering is set membership only, but this
must be confirmed rather than believed.

### The launcher

`.mcp.json` is static and cannot compute a hash, so the plugin ships a shim:

```json
{
  "mcpServers": {
    "topodb": {
      "command": "node",
      "args": ["${CLAUDE_PLUGIN_ROOT}/dist/launch.js"]
    }
  }
}
```

**Exec form with `node`, never a `.cmd`/`.bat` shim** — the documented Windows
failure mode for plugin hooks and MCP entries.

`launch.js`:
1. derives the project ScopeId from `cwd`,
2. **creates the db's parent directory** — the exact bug that shipped in
   `@topodb/pi` 0.0.1 and made the db fail to come up in a fresh project,
3. execs `topodb-mcp --db <plugin-data>/memory.redb --scope <ulid>
   --read-scopes <ulid>,shared`, passing stdio straight through.

It is a **shim, not a proxy.** Unlike the Pi extension — which had to bridge MCP
to Pi's tool API and re-implement protocol plumbing — Claude Code speaks MCP
natively, so no protocol code exists here.

### Contents

```
plugins/claude-code/
  .claude-plugin/plugin.json
  .mcp.json
  skills/topodb-memory/SKILL.md    # when to recall; when to store; project vs shared
  commands/recall.md               # /recall <query>
  commands/remember.md             # /remember <fact>
  src/launch.ts
  src/scope-id.ts
  test/
.claude-plugin/marketplace.json    # repo root
```

Install:

```
/plugin marketplace add TopoDB/TopoDB
/plugin install topodb
```

### Out of scope for v0.1 (deliberate)

- **Hooks / episode recording.** See §0. Building it here builds it in the wrong
  place.
- Vector search / embeddings config.
- Any change to the Pi adapter's per-project `.topodb/` default. The two adapters
  will differ in memory model until there is a reason to unify them; that
  divergence is noted, accepted, and revisited when the recorder lands.

### Accepted risks

- Requires `node` at runtime. Same constraint as `@topodb/pi`. Acceptable.
- Global db = a single file across all projects. Mitigated by per-project scopes;
  the blast radius is real and accepted in exchange for cross-project recall.

---

## 2.5 Folded into P2 from P1 — the parity debts

P1 shipped (merged `0db2d5f`, `topodb-mcp` 0.0.4). Its final whole-branch review
surfaced five items that were deliberately **not** fixed there because P1 was
scoped MCP-only. They are folded into P2 rather than left as loose ends, because
three of them are the *same quiet-failure family* as the `link` bug P1 exists to
fix, and shipping the Claude Code plugin on top of them would bake them in.

**The through-line: P1 made the MCP surface scope-correct. These are the places
that surface's guarantees stop being true.** Each must be either fixed or stated
as a limitation — an accident is not acceptable.

### D1 — `topodb-cli link` still has the bug (the sharpest one)

The batch DSL fix lives in shared code (`topodb-json/src/batch.rs`), so
`topodb-cli submit '[{"op":"link", …, "scope":"…"}]'` **can** now stamp an edge
scope. But `topodb-cli link` (`crates/topodb-cli/src/cli.rs`) has **no `--scope`
flag**, so it still stamps every edge with the process-wide `--scope`.

**The CLI's two ways to create an edge now disagree with each other.** One can
cross a scope boundary; the other silently cannot. That divergence did not exist
before P1 — P1 created it.

Fix: add `--scope` to `topodb-cli link`, matching the MCP tool's semantics.
Also update `crates/topodb-cli/README.md:84`, which claims *"There's no
per-command `--scope` override in v1"* — already false for `submit`'s three ops.

### D2 — `topodb-cli changes` is an ungated unscoped read

P1 gated `get_changes` behind `--allow-unscoped-changes` because, in a database
shared across projects, replaying the op log hands one project every other
project's writes. **That rationale does not stop at the MCP boundary.**
`topodb-cli changes` reads the same log, unscoped, with no gate.

Decide explicitly: gate it to match, or state why the CLI is trusted differently
(a human at a terminal is not an LLM with a token budget — that is a real
argument, but it must be *made*, not assumed).

### D3 — an edge's scope is never validated against its endpoints'

`crates/topodb/src/storage.rs:941-955` validates only that the *endpoints*
satisfy the cross-scope rule (at least one `Shared`). The **edge's own** scope is
stamped as given, unconstrained. So this commits happily:

```
link(from=<shared A>, to=<shared B>, scope=<some unrelated project ULID>)
```

…and produces an edge **no reader can ever traverse** — invisible to anyone who
can see A and B. P1 handed clients the ability to set an edge scope; it did not
give them a way to get it wrong loudly. Same quiet-failure family as the bug P1
fixed.

Engine-level fix: reject an edge scope that is neither `Shared` nor equal to one
of its endpoints' scopes. This is a behaviour change to the engine — it needs its
own think, and it may belong in a `topodb` release rather than the plugin's.

### D4 — the non-empty read-set invariant lives in the parser, not the type

`Config.default_read_scopes: Vec<Scope>` is `pub`, and its non-empty invariant is
enforced only in `Config::from_args`. A hand-built `Config { default_read_scopes:
vec![], .. }` yields a `ScopeSet` admitting nothing, and every default read
silently returns empty. It **fails closed** (returns nothing, never everything),
and `main.rs` is today the only constructor — so this is latent, not live. But
"there is no unscoped read" is load-bearing enough to deserve a type, not a
convention. The plugin's launcher will be a second constructor of these args.

### D5 — the `get_changes` gate is a breaking change with no semver signal

`topodb-mcp` 0.0.3 → 0.0.4 silently breaks any existing client that calls
`get_changes`. There is **no CHANGELOG in the repo**. A sync/consolidation host
upgrading will fail at runtime with no warning.

Before the 0.0.4 release is cut: add a CHANGELOG, and make the `get_changes` gate
loud in the release notes. This is release hygiene, not code — but it is the one
item that bites a real user today.

---

## 3. Why hooks are excluded, stated once, plainly

Claude Code's hook surface is genuinely strong — `PostToolUse` receives
`tool_name`, `tool_input`, **and** `tool_output`; `UserPromptSubmit` can inject
context; `Stop` sees the final message and stop reason; hooks support
`"async": true` so they need not tax every tool call. That is a complete,
structured observation channel over an agent's task loop, requiring **no LLM
inside the engine** — the founding principle survives intact.

It is, in other words, an excellent Episode Recorder substrate. **That is why it
is not being built here.** It is the keystone item on both roadmaps
(`topodb-pro/strategy/VISION.md` §"The keystone"), it needs the
episode/procedure/policy graph schema designed first, and under the
infrastructure identity it must be host-agnostic. Bolting it onto this plugin
would produce a Claude-Code-shaped recorder that Pi cannot use.

**Next design work, in order:** episode/procedure/policy graph schema → the
host-agnostic Episode Recorder core → per-host feeders (Claude Code hooks, Pi
events) → benchmarks and the promotion gate, scoped to hard coding signals.
