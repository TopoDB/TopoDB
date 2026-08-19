# topodb-warehouse

Context warehouse: an append-only, content-addressed log of raw session artifacts plus mirrored engine operations under `<db>.warehouse/`. From this bronze tier, Artifact and Chunk nodes, evidence lineage, and even a whole redb are deterministically re-derivable, with no LLM involvement.

The warehouse directory layout consists of five subdirectories: `segments/` holds append-only JSONL files (sealed with lz4 compression), `archive/` stores cold segments, `blobs/` contains large artifacts stored separately, `spool/` houses unseal files pending drain, and `MANIFEST.json` at the root tracks metadata and tiering state.

## CLI

`topodb-cli` exposes the warehouse under `topodb --db <db>.redb warehouse <subcommand>`:

| Subcommand | What it does |
|---|---|
| `status` | Per-tier counts, spool backlog, mirror watermark (no db open needed). |
| `drain` | Land spooled events into segments and mirror new engine ops. |
| `derive [--rederive]` | Derive `Artifact`/`Chunk` nodes + `evidence` edges from segments; `--rederive` drops and rebuilds all derived nodes/edges (e.g. after an embedder change). |
| `tier` | Move aged artifacts/segments down the tiers (hot → warm → cold → expired). |
| `rebuild <out>` | Replay op events into a fresh db file at `<out>` (must not exist). |
| `verify` | Re-hash sealed segments against `MANIFEST.json`. |
| `show <hash>` | Print an artifact's stored text by hash (blob or segment). |

## `[warehouse]` config

`.topodb.toml` accepts a `[warehouse]` table (all keys optional; shown with their defaults):

```toml
[warehouse]
enabled = true          # false, or TOPODB_WAREHOUSE=0|off, disables the warehouse entirely
path = ""               # dir override; empty = "<db>.warehouse" sibling. TOPODB_WAREHOUSE_DIR wins over both
hot_days = 14
warm_days = 180
retention_days = 730
purge_expired = false
segment_mb = 64
max_inline_kb = 16
max_artifact_kb = 512
redact = true
evidence_k = 20
tier_batch = 500
spool_min_age_ms = 2000
```

Precedence for the warehouse directory: `TOPODB_WAREHOUSE_DIR` > `[warehouse].path` > `<db>.warehouse` (sibling of the db file). Hygiene runs three scheduled tasks under `[schedule.warehouse_drain]`, `[schedule.warehouse_derive]`, and `[schedule.warehouse_tier]` (each an `{enabled, interval_secs}` pair, same shape as `[schedule.compact]` etc.) — drain defaults to immediate (interval 0), derive to hourly, tier to daily.

**`[warehouse]` here governs the CLI/daemon side only** (`topodb warehouse …`,
the hygiene tick that runs drain/derive/tier). The Claude Code plugin's hooks
that spool raw artifacts in the first place do **not** read `.topodb.toml` at
all — they are governed solely by `TOPODB_WAREHOUSE=0|off` and
`TOPODB_WAREHOUSE_DIR` in the hook's environment (see the plugin README's
Configuration section). If you relocate (`path`) or disable (`enabled =
false`) the warehouse here for a db the plugin uses, set the matching env var
too, or the hooks keep spooling into a directory nothing drains.
