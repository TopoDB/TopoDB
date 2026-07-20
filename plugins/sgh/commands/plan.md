---
description: Compile a goal into a validated sgh graph with a worst-case bound
---

Compile this goal into an sgh graph: **$ARGUMENTS**

If the goal is empty, ask what they want done rather than invoking the planner
with nothing.

First create the `.sgh` directory if it does not exist, then write the goal
above verbatim to `.sgh/goal.txt` using your file-writing tool (the Write
tool). Do not write it via bash — no heredoc, no `echo`, no other shell
command — since the goal can contain characters that would be interpreted as
shell syntax if it ever passed through a shell command line. Writing it to a
file first means the goal is never interpolated into a shell command.

Then run:

```bash
source "${CLAUDE_PLUGIN_ROOT}/lib/sgh-env.sh" || exit 1
"$SGH_BIN" --db "$SGH_DB" plan "$(cat .sgh/goal.txt)" --out .sgh/graph.yaml
```

`sgh plan` prints the worst-case bound — the maximum number of agent calls the
graph can ever make. Show that number and say plainly what it means: the run
cannot exceed it, which is the property that distinguishes this from a loop.

Then show the graph's shape — the nodes and what depends on what — so the
person can see the plan before committing to it. Read `.sgh/graph.yaml` for
this; do not re-derive it from the goal.

If the planner exits non-zero it failed to produce a valid graph within
`--max-attempts` (default 3). Report what it said rather than retrying blindly;
a goal that will not compile usually needs to be narrowed, not repeated.

Finish by telling them the graph is at `.sgh/graph.yaml` and that `/sgh:run`
(no argument) will execute it after showing every command for approval.
Do not run it yourself — planning and executing are deliberately separate.
