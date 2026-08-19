# Using TopoDB memory with your agent client

TopoDB's memory is a plain [MCP](https://modelcontextprotocol.io) server —
`topodb-mcp`, speaking stdio — so **any MCP client can use it**. This page gets
you from "I use $CLIENT" to "TopoDB tools are live in $CLIENT" for six popular
clients.

Install the server once:

```bash
cargo install topodb-mcp        # installs the `topodb-mcp` binary
# on Pi: pi install npm:@topodb/pi
```

Every client below registers the **same** stdio command — only the wrapper
format differs:

```
topodb-mcp --db /absolute/path/to/agent.redb [--scope <ULID>]
```

## How seamless is it? Three tiers

The *tools* (`search_memories`, `remember`, `traverse`, …) work everywhere. How
*automatic* memory feels depends on what lifecycle surface the client exposes:

- **Seamless** — the client has lifecycle hooks, so memory is automatic: recall
  is injected at session start and durable facts are captured at the end. The
  **Claude Code** and **Cursor** plugins do this today (see
  [`plugins/claude-code/`](../plugins/claude-code/README.md) and
  [`plugins/cursor/`](../plugins/cursor/README.md)); **OpenCode** is
  the natural next one (its plugin/hook system can host the same flow — a future
  effort, not shipped here).
- **Rules-nudge** — no hooks, but the client reads a rules / `AGENTS.md` file, so
  you can *instruct the agent* to recall at the start of a task and remember what
  matters. Best-effort and model-driven — see the [snippet](#rules-nudge-snippet)
  below.
- **Tool-only** — the tools are present; you or the agent invoke them explicitly.

## Compatibility matrix

| Client | Config location | Best tier today | Snippet source |
|---|---|---|---|
| Claude Code | (use the plugin) | **Seamless** | [plugin README](../plugins/claude-code/README.md) |
| OpenCode | `opencode.json` (project) or `~/.config/opencode/opencode.json` | Rules-nudge *(seamless-capable — future)* | Per vendor docs (2026-08) |
| Codex CLI | `~/.codex/config.toml` | Rules-nudge (`AGENTS.md`) | Per vendor docs (2026-08) |
| Cursor | (use the plugin) | **Seamless** | [plugin README](../plugins/cursor/README.md) |
| Windsurf | `~/.codeium/windsurf/mcp_config.json` | Rules-nudge (`.windsurfrules`) | Per vendor docs (2026-08) |
| Zed | `settings.json` → `context_servers` | Tool-only | Per vendor docs (2026-08) |
| Cline | `cline_mcp_settings.json` | Rules-nudge (`.clinerules`) | Per vendor docs (2026-08) |

> Every snippet below is **Per vendor docs (as of 2026-08)** — the format is
> taken from each vendor's current MCP documentation (linked), not asserted from
> memory. None were run end-to-end in this repo; if a vendor changes their config
> shape, trust their docs over this page.

Replace `/absolute/path/to/agent.redb` with a real path, and read
[Scoping](#scoping-one-db-many-projects) before you pick it.

## Per-client setup

### OpenCode

`opencode.json` (project root) or `~/.config/opencode/opencode.json`:

```json
{
  "$schema": "https://opencode.ai/config.json",
  "mcp": {
    "topodb": {
      "type": "local",
      "command": ["topodb-mcp", "--db", "/absolute/path/to/agent.redb"],
      "enabled": true
    }
  }
}
```

Source: [OpenCode — MCP servers](https://opencode.ai/docs/mcp-servers/). OpenCode's
plugin/hook system makes it the best candidate for a future *seamless*
integration (auto-recall/auto-capture, mirroring the Claude Code plugin).

### Codex CLI

`~/.codex/config.toml` — add an `[mcp_servers.NAME]` table (or run
`codex mcp add topodb -- topodb-mcp --db /absolute/path/to/agent.redb`):

```toml
[mcp_servers.topodb]
command = "topodb-mcp"
args = ["--db", "/absolute/path/to/agent.redb"]
```

Source: [Codex — Model Context Protocol](https://developers.openai.com/codex/mcp).
A **project-local** `.codex/config.toml` is only read for *trusted* projects; for
an untrusted project put the server in the global `~/.codex/config.toml`. Codex
reads `AGENTS.md`, so the [rules-nudge snippet](#rules-nudge-snippet) applies.

### Cursor

Install the [Cursor plugin](../plugins/cursor/README.md) for the seamless tier
(auto-recall, episode + warehouse capture, shared db with Claude Code). Manual
setup, if you prefer:

`.cursor/mcp.json` (project) or `~/.cursor/mcp.json` (global):

```json
{
  "mcpServers": {
    "topodb": {
      "command": "topodb-mcp",
      "args": ["--db", "/absolute/path/to/agent.redb"]
    }
  }
}
```

Source: [Cursor — Model Context Protocol](https://cursor.com/docs/mcp). Put a
recall/capture instruction in `.cursor/rules` (or `AGENTS.md`) — see the
[snippet](#rules-nudge-snippet).

### Windsurf

`~/.codeium/windsurf/mcp_config.json` (create it if absent; on Windows
`%USERPROFILE%\.codeium\windsurf\mcp_config.json`):

```json
{
  "mcpServers": {
    "topodb": {
      "command": "topodb-mcp",
      "args": ["--db", "/absolute/path/to/agent.redb"]
    }
  }
}
```

Source: [Windsurf — Cascade MCP](https://docs.windsurf.com/plugins/cascade/mcp).
Press the **refresh** button in Cascade → MCP after editing. Add the
recall/capture instruction to `.windsurfrules`.

### Zed

`settings.json` → `context_servers` (Zed spawns it over stdio and restarts it on
save):

```json
{
  "context_servers": {
    "topodb": {
      "source": "custom",
      "command": "topodb-mcp",
      "args": ["--db", "/absolute/path/to/agent.redb"],
      "env": {}
    }
  }
}
```

Source: [Zed — Model Context Protocol](https://zed.dev/docs/ai/mcp). Check
Settings → AI → MCP Servers for a green indicator.

### Cline

`cline_mcp_settings.json` (macOS/Linux:
`~/Library/Application Support/Code/User/globalStorage/saoudrizwan.claude-dev/settings/cline_mcp_settings.json`;
edit it from Cline's **MCP Servers → Configure** panel):

```json
{
  "mcpServers": {
    "topodb": {
      "command": "topodb-mcp",
      "args": ["--db", "/absolute/path/to/agent.redb"],
      "disabled": false,
      "autoApprove": []
    }
  }
}
```

Source: [Cline — MCP overview](https://docs.cline.bot/mcp/mcp-overview). Add the
recall/capture instruction to `.clinerules`.

## Scoping: one db, many projects

This is the one real footgun. The Claude Code plugin automatically keys the
**scope** to the absolute project path, so each project gets isolated memory with
a shared layer for lessons that generalize. Manual MCP clients **do not** get
that — you choose. Two supported patterns:

1. **Per-project db (simplest).** Point `--db` at a file inside the project
   (e.g. `--db ./.topodb/memory.redb`). Isolation for free; no cross-project
   shared layer.

2. **Shared db + explicit scope.** One db, a stable per-project `--scope <ULID>`
   for writes, and optionally a wider read set:

   ```
   topodb-mcp --db ~/.topodb/memory.redb --scope <PROJECT_ULID> \
     --read-scopes <PROJECT_ULID>,<SHARED_ULID>
   ```

   Writes land in the project scope; reads span the project plus a shared scope —
   the same shape the plugin manages for you. A scope id is any
   [ULID](https://github.com/ulid/spec); generate one per project and keep it
   stable.

Without a `--scope`, a shared db mixes every project's memory into one scope —
usually not what you want. Pick per-project dbs unless you specifically want a
shared-lessons layer.

## Rules-nudge snippet

For clients without lifecycle hooks, drop this into the rules file the client
reads (`AGENTS.md`, `.cursor/rules`, `.windsurfrules`, `.clinerules`, …). It is
**best-effort and model-driven** — a prompt to the agent, not a guarantee like
the Claude Code hooks:

```markdown
## Memory (TopoDB)

You have a persistent memory via the `topodb` MCP tools.

- **At the start of a task**, call `search_memories` (and `traverse` from the
  best hit) for anything relevant to what you're about to do — past decisions,
  who owns what, prior gotchas. Prefer recalling over re-deriving.
- **When you learn something durable** — a decision and its rationale, a
  non-obvious constraint, a fix and its cause, a user preference — call
  `remember` to store it. Skip the ephemeral (this task's scratch work) and
  anything already obvious from the code or git history.
```

## Notes

- **Embeddings.** `topodb-mcp` runs local embeddings by default (ONNX Runtime is
  auto-downloaded and sha256-pinned). On an Intel Mac with no system runtime it
  falls back to text + graph only — see
  [`crates/topodb-mcp/README.md`](../crates/topodb-mcp/README.md).
- **One process per db file.** `topodb-mcp` opens the db in-process; don't point
  two clients at the same `.redb` file at once.
- **Full tool list and flags:** [`crates/topodb-mcp/README.md`](../crates/topodb-mcp/README.md).
