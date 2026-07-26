#!/usr/bin/env bash
# Resolves the two things a slash command must not guess: which `sgh` binary
# to run, and which database to run it against.
#
# Sourced, not executed. On failure it prints a build instruction and returns
# non-zero so the caller stops rather than proceeding with a broken path.

# --- binary -----------------------------------------------------------------
# Resolution order, most specific first:
#   1. $SGH_BIN            — an explicit override always wins.
#   2. in-repo release build — when developing in the TopoDB checkout, the
#      build you just made is the one you mean, so it beats anything on PATH.
#   3. `sgh` on PATH        — the installed-plugin case. A plugin installed from
#      the marketplace lives in a cache directory with no repo above it, so
#      step 2 finds nothing and this is what makes it work at all.
#   4. cargo's bin dir      — `cargo install`ed but PATH not yet reloaded.
# Never build automatically: a slash command that silently starts a
# multi-minute cargo build is a bad surprise.
if [ -z "${SGH_BIN:-}" ]; then
  _sgh_repo="${CLAUDE_PLUGIN_ROOT:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)}"
  # plugins/sgh -> repo root
  _sgh_repo="$(cd "$_sgh_repo/../.." 2>/dev/null && pwd || printf '%s' "$PWD")"
  _sgh_in_repo="$_sgh_repo/target/release/sgh"
  _sgh_cargo="${CARGO_HOME:-$HOME/.cargo}/bin/sgh"
  if [ -x "$_sgh_in_repo" ]; then
    SGH_BIN="$_sgh_in_repo"
  elif command -v sgh >/dev/null 2>&1; then
    SGH_BIN="$(command -v sgh)"
  elif [ -x "$_sgh_cargo" ]; then
    SGH_BIN="$_sgh_cargo"
  else
    # Nothing found. Keep the in-repo path so the error names a concrete
    # location rather than an empty string.
    SGH_BIN="$_sgh_in_repo"
  fi
fi

if [ ! -x "$SGH_BIN" ]; then
  echo "sgh: no usable binary found." >&2
  echo "  looked at: $SGH_BIN" >&2
  echo "             sgh on PATH" >&2
  echo "             ${CARGO_HOME:-$HOME/.cargo}/bin/sgh" >&2
  echo "Build it first:  cargo build --release -p topodb-sgh" >&2
  echo "Or point SGH_BIN at an existing sgh binary." >&2
  return 1 2>/dev/null || exit 1
fi
export SGH_BIN

# --- database ---------------------------------------------------------------
# The CLI defaults --db to ./sgh.redb, which drops a database into whatever
# directory you happened to run from. Derive a stable per-project path under
# the plugin data directory instead, keyed by a hash of the project path.
if [ -z "${SGH_DB:-}" ]; then
  if command -v sha256sum >/dev/null 2>&1; then
    _sgh_hash="$(printf '%s' "$PWD" | sha256sum | cut -c1-16)"
  else
    _sgh_hash="$(printf '%s' "$PWD" | shasum -a 256 | cut -c1-16)"
  fi
  SGH_DB="${HOME}/.claude/plugins/data/sgh/${_sgh_hash}.redb"
fi
mkdir -p "$(dirname "$SGH_DB")"
export SGH_DB

# --- agent memory database ---------------------------------------------------
# The per-project memory db the graphs read and write: agent nodes through
# mcp__topodb (composed into $SGH_MCP below), command nodes through the
# topodb CLI ($SGH_TOPODB below). NOT $SGH_DB — that is the sgh run store.
# Deriving it from SGH_DB keeps the pairing stable under an SGH_DB override.
if [ -z "${SGH_MEMORY_DB:-}" ]; then
  SGH_MEMORY_DB="${SGH_DB%.redb}-memory.redb"
fi
export SGH_MEMORY_DB

# --- agent MCP server -------------------------------------------------------
# Composes the value for `sgh run --agent-mcp`: an absolute topodb-mcp binary
# plus a per-project agent-memory database. That db is NOT $SGH_DB — SGH_DB is
# the sgh run store; this is the memory db the graph's agent nodes read and
# write through mcp__topodb. The path lives in $SGH_MEMORY_DB (derived from
# SGH_DB as "<same path>-memory") to keep the pairing stable under an SGH_DB
# override too.
# Resolution mirrors $SGH_BIN: explicit $SGH_MCP override wins, then the
# in-repo release build, then PATH, then cargo's bin dir. Unlike SGH_BIN a
# miss is NOT fatal: MCP is per-node opt-in, and a graph with no opted-in
# nodes must still run without topodb-mcp installed — so leave SGH_MCP unset
# with a note instead of stopping the caller.
if [ -z "${SGH_MCP:-}" ]; then
  # Recomputed rather than reusing $_sgh_repo: that variable only exists when
  # the SGH_BIN block above actually ran (an exported SGH_BIN skips it).
  _sgh_mcp_root="${CLAUDE_PLUGIN_ROOT:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)}"
  _sgh_mcp_root="$(cd "$_sgh_mcp_root/../.." 2>/dev/null && pwd || printf '%s' "$PWD")"
  _sgh_mcp_bin="$_sgh_mcp_root/target/release/topodb-mcp"
  _sgh_mcp_cargo="${CARGO_HOME:-$HOME/.cargo}/bin/topodb-mcp"
  if [ -x "$_sgh_mcp_bin" ]; then
    :
  elif command -v topodb-mcp >/dev/null 2>&1; then
    _sgh_mcp_bin="$(command -v topodb-mcp)"
  elif [ -x "$_sgh_mcp_cargo" ]; then
    _sgh_mcp_bin="$_sgh_mcp_cargo"
  else
    _sgh_mcp_bin=""
  fi
  if [ -n "$_sgh_mcp_bin" ]; then
    # --scope shared --embeddings off mirrors the proven live-e2e invocation;
    # embeddings stay off because the server is a per-run helper — model
    # warmup on every sgh run would be all cost, no recall benefit.
    SGH_MCP="$_sgh_mcp_bin --db $SGH_MEMORY_DB --scope shared --embeddings off"
  else
    echo "sgh: topodb-mcp not found; SGH_MCP left unset (only graphs whose agent nodes declare tools: [topodb] need it)" >&2
  fi
fi
if [ -n "${SGH_MCP:-}" ]; then
  export SGH_MCP
fi

# --- topodb CLI --------------------------------------------------------------
# Command nodes (the lifecycle graph's sweep/verify) shell out to the topodb
# CLI against $SGH_MEMORY_DB. Resolution mirrors $SGH_MCP: explicit override,
# in-repo release build, PATH, cargo's bin dir. Like $SGH_MCP a miss is NOT
# fatal — only graphs whose command nodes run "$SGH_TOPODB" need it.
if [ -z "${SGH_TOPODB:-}" ]; then
  _sgh_cli_root="${CLAUDE_PLUGIN_ROOT:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)}"
  _sgh_cli_root="$(cd "$_sgh_cli_root/../.." 2>/dev/null && pwd || printf '%s' "$PWD")"
  _sgh_cli_bin="$_sgh_cli_root/target/release/topodb"
  _sgh_cli_cargo="${CARGO_HOME:-$HOME/.cargo}/bin/topodb"
  if [ -x "$_sgh_cli_bin" ]; then
    SGH_TOPODB="$_sgh_cli_bin"
  elif command -v topodb >/dev/null 2>&1; then
    SGH_TOPODB="$(command -v topodb)"
  elif [ -x "$_sgh_cli_cargo" ]; then
    SGH_TOPODB="$_sgh_cli_cargo"
  else
    SGH_TOPODB=""
  fi
  if [ -z "$SGH_TOPODB" ]; then
    echo "sgh: topodb CLI not found; SGH_TOPODB left unset (only graphs whose command nodes run \"\$SGH_TOPODB\" need it — build with: cargo build --release -p topodb-cli)" >&2
  fi
fi
if [ -n "${SGH_TOPODB:-}" ]; then
  export SGH_TOPODB
fi
