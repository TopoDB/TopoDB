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

Point `SGH_BIN` at an existing binary to use one from elsewhere. npm packaging
with prebuilt platform binaries is deliberately deferred.

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
