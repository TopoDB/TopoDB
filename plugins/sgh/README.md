# sgh — Claude Code plugin

Plan and run agent work as a **validated graph with a computable worst-case
bound**, instead of an open-ended loop. The bound is the point: before anything
runs, you know the maximum number of agent calls the graph can make.

## Install

```
/plugin marketplace add TopoDB/TopoDB
/plugin install sgh
```

## Requires the `sgh` binary

Unlike the `topodb` memory plugin, this one does **not** download anything. It
shells out to a locally built binary:

```
cargo build --release -p topodb-sgh
```

or `cargo install --path crates/topodb-sgh` to put `sgh` on your `PATH`.

The plugin looks for the binary in this order:

1. `$SGH_BIN`, if you set it — an explicit override always wins.
2. `target/release/sgh` in the TopoDB checkout the plugin is running from. When
   you are developing in the repo, the build you just made beats anything on
   `PATH`.
3. `sgh` on your `PATH`. This is the case that matters for an installed
   plugin: installed from the marketplace, the plugin lives in a cache
   directory with no repo above it, so step 2 finds nothing.
4. `$CARGO_HOME/bin/sgh` (default `~/.cargo/bin/sgh`), for a `cargo install`
   done in a shell whose `PATH` you have not reloaded.

If none of those exist it tells you where it looked and stops — it never
builds anything for you, because a slash command that silently starts a
multi-minute compile is a bad surprise.

npm packaging with prebuilt platform binaries is deliberately deferred.

## Providers

The `sgh` CLI's `run` and `plan` subcommands take `--provider
claude-code|anthropic|openai` (default `claude-code`); `openai` also accepts
`--base-url` to point at any OpenAI-compatible local endpoint (vLLM, Ollama,
etc). `anthropic` and `openai` talk to the provider's HTTP API directly and
read their key from `ANTHROPIC_API_KEY` / `OPENAI_API_KEY`. `--agent-bash` is
claude-code-only — the HTTP providers execute no shell of their own, so a
bash grant has nothing to attach to. **The plugin itself is unaffected**: its
slash commands always drive the local `claude` CLI (`--provider claude-code`,
the default), unchanged by this flag existing.

Under `--provider anthropic|openai`, `sgh` itself hosts the MCP server for
the whole run, and that server holds redb's exclusive lock on the memory db
for as long as `sgh` is running. If a `command` node shells out to `topodb`
against the *same* db mid-run, it will fail with "database held by another
process" — verify db state after the run finishes instead, or point
command-node checks at a different db. This doesn't bite under
`claude-code`, since claude spawns the MCP server per node invocation rather
than once for the whole run.

### Hardening flags

`run` (and, for `--agent-timeout`, `plan` too) take a few flags that bound
how long and how wide a run is allowed to get:

- `--agent-timeout <secs>` (default `600`) — the whole-node deadline for a
  single agent call, for every provider: the claude-code subprocess and both
  HTTP runners' tool loop alike. A node that blows through it is treated as
  failed, not left running.
- `--max-inflight <n>` (default `4`, only on `run`) — how many ready nodes the
  executor will run concurrently. `1` forces strictly sequential execution,
  which is useful when you want deterministic ordering or your provider rate-
  limits concurrent calls.
- **Ctrl-C** — a `sgh run` in progress cancels gracefully on the first
  interrupt: inflight children are killed as a process group, the run is
  marked blocked, and the process exits `1`. It does not delete anything —
  the run's state is left in the db for a future replan or resume.

## Commands

- `/sgh:plan <goal>` — compile a goal into `.sgh/graph.yaml` and print its
  worst-case bound. Executes nothing.

  `/sgh:plan` writes the goal to `.sgh/goal.txt` (via the Write tool, never
  through a shell command) before invoking the CLI, and reads it back with
  `$(cat .sgh/goal.txt)`. This means the goal text is never interpolated
  directly into a shell command line, so a goal containing shell metacharacters
  cannot be interpreted as shell syntax.

  `/sgh:plan` writes `.sgh/goal.txt` and `.sgh/graph.yaml` into your project
  directory as untracked files; you may want to add `.sgh/` to `.gitignore`.

- `/sgh:run` — show the bound and every shell command for approval, then
  execute after you say yes. It takes **no argument**: it always runs the
  fixed path `.sgh/graph.yaml`, the file `/sgh:plan` writes. If you pass it a
  path anyway, it tells you to copy your graph to `.sgh/graph.yaml` (or
  re-run `/sgh:plan`) and stops — it will not run an arbitrary path. This is
  deliberate: accepting a path as a command argument would be a shell-
  injection vector, so the path is fixed instead.

- `/sgh:lifecycle` — Runs the shipped memory-lifecycle graph (`graphs/lifecycle.yaml`) against
  this project's memory db (`$SGH_MEMORY_DB`): a read-only decay sweep
  proposes candidates, a judge agent (armed with `mcp__topodb`) decides
  keep/forget and duplicate consolidation and applies its own verdicts, and
  a verify step re-reads the database — the run fails if any claimed action
  is not actually reflected. Same two-step gate as `/sgh:run`: every command
  is shown before anything executes. Nothing is hard-deleted; forget and
  consolidation stamp soft tombstones (`forgotten_at`/`superseded_at`) that
  `--include-superseded` and `as_of` queries can still reach.

  Requires the `topodb` CLI and `topodb-mcp` binaries (build with
  `cargo build --release -p topodb-cli -p topodb-mcp`) and `jq`.

## The approval gate

`/sgh:run` runs `sgh validate` first — read-only — which prints every
**command** node's exact `run:` string, shows you all of them verbatim, and
waits for explicit approval before running anything. That covers shell
commands only. Graphs can also contain **agent** nodes, and those are not
displayed by `validate` or by the gate: an agent node spawns `claude -p` with
a model-authored prompt, and that prompt goes unread and runs under your
existing Claude Code permission settings. The worst-case bound tells you how
many agent calls can happen at most — it does not tell you what any of them
will be asked to do.

Agent prompts remain ungated and run under your existing permission settings.
The `--agent-bash` flag, available for direct CLI use, widens what an agent
node can execute by granting it `Bash(<prefix>:*)` permissions additively on
top of Read, Write, and Edit (e.g. `--agent-bash 'topodb'` grants access to
shell commands matching the prefix `topodb:*`). The rail rejects only shells
and launchers (`sh`, `bash`, `zsh`, `dash`, `ksh`, `fish`, `env`), not binary
restrictions — `npm` would pass through. As guidance, grant the narrowest
binary scoped to your task. The gate echoes every grant so you can see what
permissions an agent receives before it runs. **The grant must textually
prefix-match the exact command your prompts issue** — if a prompt invokes an
absolute path like `/abs/path/topodb`, pass `--agent-bash /abs/path/topodb`,
not `--agent-bash topodb` (relative or `PATH`-resolved prefixes work only when
the prompts use the same form). **The plugin itself never passes
`--agent-bash`** — it runs agents under the permissions you configure globally
in Claude Code settings.

Agent nodes may also carry `tools: [topodb]` to opt into the TopoDB MCP server
surface (`mcp__topodb`, the full server API). Running such a graph requires
`--agent-mcp '<absolute topodb-mcp path> --db <path> --scope <ulid|shared> …'`
(from the `sgh` CLI); the server command is whitespace-split with no shell and
echoed at the gate exactly like bash grants, subject to the same textual-honesty
rule (grant the exact binary path you use in the prompt).

### Worked examples

Agent node with bash:

```sh
sgh run graph.yaml --agent-bash topodb
```

Agent node with MCP (TopoDB memory tools):

```sh
sgh run graph.yaml --agent-mcp '/abs/topodb-mcp --db /Users/you/.topodb/agent.redb --scope shared --embeddings off'
```

Agent nodes over an HTTP provider instead of claude-code:

```sh
sgh run graph.yaml --provider anthropic --agent-mcp '/abs/topodb-mcp --db /Users/you/.topodb/agent.redb --scope shared --embeddings off'
```

`--yes-including-revisions` is not used anywhere in this plugin, and `--replan`
is off unless you ask for it by name. Both exist because a replan lets a model
rewrite the shell commands; anything a model authored goes back through the
gate before it runs.

## Storage

Runs are recorded in a per-project database under
`~/.claude/plugins/data/sgh/`, keyed by a hash of the project path. The CLI's
default is `./sgh.redb` in the working directory; the plugin never uses that.
Override with `SGH_DB`.

## Not included yet

- `/sgh:show` — needs an IPC layer, because redb takes an exclusive
  cross-process lock and `show` cannot read the database while a run holds it.
- Pi packaging (`npm/topodb-sgh-pi`).
