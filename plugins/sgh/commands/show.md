---
description: Inspect sgh runs — list them, or show one run's event log. Works while a run is in progress (reads never touch the locked database).
---

Show the state of sgh runs for this project. This is the safe way to look at
a run **while it is still executing**: `sgh show` reads the event sidecar and
falls back gracefully when the database is locked by the running process.

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
`running`, tell the user they can tail it live from a terminal with:
`sgh --db "$SGH_DB" show <run-id> --follow` — do NOT run `--follow`
yourself; it blocks until Ctrl-C.

If the command exits 2, show its stderr to the user verbatim (it explains
the misuse, e.g. an invalid run id) and stop.
