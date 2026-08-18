# topodb-warehouse

Context warehouse: an append-only, content-addressed log of raw session artifacts plus mirrored engine operations under `<db>.warehouse/`. From this bronze tier, Artifact and Chunk nodes, evidence lineage, and even a whole redb are deterministically re-derivable, with no LLM involvement.

The warehouse directory layout consists of five subdirectories: `segments/` holds append-only JSONL files (sealed with lz4 compression), `archive/` stores cold segments, `blobs/` contains large artifacts stored separately, `spool/` houses unseal files pending drain, and `MANIFEST.json` at the root tracks metadata and tiering state.
