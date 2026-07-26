---
description: Run the memory-lifecycle graph — sweep decay candidates, judge, verify
---

This command always runs the plugin's shipped graph at
`${CLAUDE_PLUGIN_ROOT}/graphs/lifecycle.yaml` against this project's memory
database (`$SGH_MEMORY_DB`). It does not accept a path argument. If the user
passed any argument, tell them this command only runs the shipped lifecycle
graph — for arbitrary graphs use `/sgh:plan` + `/sgh:run` — then stop. Do
not run either bash block below in that case.

What the graph does: a deterministic sweep proposes decay candidates
(`topodb lifecycle-candidates` — read-only, no model call), a judge agent
reviews them plus `find_duplicate_memories` pairs and applies its verdicts
through mcp__topodb (`forget`, `consolidate_memories`), and a verify step
re-reads the database to prove every claimed action actually happened. The
judge writes to your project memory db; nothing is hard-deleted — forget
and consolidate stamp soft tombstones that history queries can still reach.

This is a two-step gate. Do not collapse it into one step, and do not skip
step 1.

## Step 1 — preview, read-only

```bash
source "${CLAUDE_PLUGIN_ROOT}/lib/sgh-env.sh" || exit 1
if [ -z "${SGH_MCP:-}" ]; then
  echo "lifecycle: SGH_MCP is unset — the judge node needs topodb-mcp." >&2
  echo "Build it first:  cargo build --release -p topodb-mcp" >&2
  exit 1
fi
if [ -z "${SGH_TOPODB:-}" ]; then
  echo "lifecycle: SGH_TOPODB is unset — the sweep/verify nodes need the topodb CLI." >&2
  echo "Build it first:  cargo build --release -p topodb-cli" >&2
  exit 1
fi
if ! command -v jq >/dev/null 2>&1; then
  echo "lifecycle: jq is required by the verify step — install jq first." >&2
  exit 1
fi
GRAPH="${CLAUDE_PLUGIN_ROOT}/graphs/lifecycle.yaml"
"$SGH_BIN" --db "$SGH_DB" validate "$GRAPH" --agent-mcp "$SGH_MCP"
```

This executes nothing. It prints the node count, the worst-case bound, and
every command node's full `run:` string.

Show the user:

- the worst-case bound, and that the run cannot exceed it
- **every command line, verbatim** — never summarize, truncate, or
  paraphrase a `run:` string
- which database the run will modify: `$SGH_MEMORY_DB` (echo its value)

If `validate` exits 2 the graph is invalid. Report the errors and stop.

Then ask whether to proceed, and **wait for an actual answer**. Silence,
ambiguity, or approval given before this step's output was shown do not
count. If they want changes, stop. End your turn here: run no further
commands until the human replies in a new message.

## Step 2 — execute, only after they say yes

```bash
source "${CLAUDE_PLUGIN_ROOT}/lib/sgh-env.sh" || exit 1
GRAPH="${CLAUDE_PLUGIN_ROOT}/graphs/lifecycle.yaml"
"$SGH_BIN" --db "$SGH_DB" run "$GRAPH" --yes --agent-mcp "$SGH_MCP"
echo "exit=$?"
```

`--yes` is safe here and only here: it skips a prompt for a graph the user
has just read in full. Never pass `--yes-including-revisions`. Do not pass
`--replan` unless the user asks for it by name — a replanned graph's `run:`
strings have to come back through step 1.

Interpret the exit code:

- **0** — completed. Summarize the verify node's report: candidates before
  and after, how many memories were acted on, and each verdict with its
  rationale. If `acted` is 0, say plainly that the judge kept everything.
- **1** — blocked by a real failure. If the blocked node is `verify`, treat
  it seriously: the judge claimed an action the database does not show —
  report which id was still live. Otherwise report which node failed and
  what it said.
- **2** — schema validation failed. Should not happen after step 1 passed;
  if it does, the graph changed between the two steps — say so.
- **3** — halted at an intentional checkpoint (no gates exist in this
  graph today; if you see 3, report it as unexpected).
