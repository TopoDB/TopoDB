---
description: Inspect sgh runs — list them, or show one run's event log. Works while a run is in progress (reads never touch the locked database).
---

Show the state of sgh runs for this project. This is the safe way to look at
a run **while it is still executing**: `show <run-id>` reads only the event
sidecar (it never opens the database), and `show --list` falls back to
scanning the sidecar directory when the database is locked by the running
process.

**$ARGUMENTS** is an optional run id. Two shapes:

## No argument — list runs

```bash
source "${CLAUDE_PLUGIN_ROOT}/lib/sgh-env.sh" || exit 1
"$SGH_BIN" --db "$SGH_DB" show --list
```

Present the table as-is (columns: RUN_ID, STATUS, CREATED_MS, GOAL). If the
list is empty, say so — do not invent runs. If exactly one run is `running`,
mention the user can watch it live with `/sgh:show <that-run-id>`.

## Run id given — show that run's event log

```bash
source "${CLAUDE_PLUGIN_ROOT}/lib/sgh-env.sh" || exit 1
"$SGH_BIN" --db "$SGH_DB" show "$ARGUMENTS"
```

Summarize the event log briefly (started, per-node outcomes, current
status); show the raw tail only if the user asks. If the run's status is
`running`, tell the user they can tail it live from a terminal with
`sgh --db <db-path> show <run-id> --follow`, substituting the actual
`$SGH_DB` path you used above (the variable won't expand in their shell) —
and do NOT run `--follow` yourself; it blocks until Ctrl-C.

If the command exits 2, show its stderr to the user verbatim (it explains
the misuse, e.g. an invalid run id) and stop.
