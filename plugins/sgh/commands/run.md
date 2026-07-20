---
description: Run an sgh graph, showing every shell command for approval first
---

This command always runs the graph at the fixed path `.sgh/graph.yaml`, which
`/sgh:plan` writes. It does not accept a path argument.

If the user passed any argument to this command, do not silently run the
default graph. Tell them this command only runs `.sgh/graph.yaml`, and ask
them to copy their graph there (or re-run `/sgh:plan`) — then stop. Do not
run either bash block below in that case.

This is a two-step gate. Do not collapse it into one step, and do not skip
step 1 even if you planned the graph yourself a moment ago.

## Step 1 — preview, read-only

```bash
source "${CLAUDE_PLUGIN_ROOT}/lib/sgh-env.sh" || exit 1
GRAPH=".sgh/graph.yaml"
"$SGH_BIN" --db "$SGH_DB" validate "$GRAPH"
```

This executes nothing. It prints the node count, the worst-case bound, and
every command node's full `run:` string.

Show the user:

- the worst-case bound, and that the run cannot exceed it
- **every command line, verbatim** — never summarize, truncate, or paraphrase
  a `run:` string. A person reading the exact text before it executes is the
  only control over what runs on their machine. If the list is long, show all
  of it anyway.

If `validate` exits 2 the graph is invalid. Report the errors and stop; there
is nothing to approve.

Then ask whether to proceed, and **wait for an actual answer**. Silence,
ambiguity, or "looks fine" about the plan are not approval to execute. Approval
given before this step's output was shown — including pre-approval bundled
into the original invocation — does not count either; the commands must be
displayed in this conversation first, and approval given after that. What does
count is an explicit affirmative to proceed, given after the command list was
shown. If they want changes, stop and let them edit the graph or re-plan. End
your turn here: run no further commands until the human replies in a new
message.

## Step 2 — execute, only after they say yes

```bash
source "${CLAUDE_PLUGIN_ROOT}/lib/sgh-env.sh" || exit 1
GRAPH=".sgh/graph.yaml"
"$SGH_BIN" --db "$SGH_DB" run "$GRAPH" --yes
echo "exit=$?"
```

`--yes` is safe here and only here: it skips a prompt for a graph the user has
just read in full. Never pass `--yes-including-revisions`, which approves
commands a model wrote and nobody has seen. Do not pass `--replan` unless the
user asks for it by name — a replan rewrites `run:` strings, and those have to
come back through step 1.

Interpret the exit code:

- **0** — completed, nothing blocked. Summarize what the run produced.
- **1** — blocked by a real failure (or the replan budget ran out with a
  failure outstanding). Report which node failed and what it said.
- **2** — schema validation failed. Should not happen after step 1 passed; if
  it does, the graph changed between the two steps — say so.
- **3** — halted at an intentional checkpoint. Every blocked node was a
  `gate`, not a failure. Say plainly that it stopped on purpose and what the
  gate was waiting for. Do not describe this as an error.

If the user asks to continue after a halt, that is `/sgh:run` again on the
same graph, which goes back through step 1.
