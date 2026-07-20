#!/usr/bin/env bash
# Resolves the two things a slash command must not guess: which `sgh` binary
# to run, and which database to run it against.
#
# Sourced, not executed. On failure it prints a build instruction and returns
# non-zero so the caller stops rather than proceeding with a broken path.

# --- binary -----------------------------------------------------------------
# $SGH_BIN wins. Otherwise look for a release build in the repo this plugin
# ships from. Never build automatically: a slash command that silently starts
# a multi-minute cargo build is a bad surprise.
if [ -z "${SGH_BIN:-}" ]; then
  _sgh_repo="${CLAUDE_PLUGIN_ROOT:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)}"
  # plugins/sgh -> repo root
  _sgh_repo="$(cd "$_sgh_repo/../.." 2>/dev/null && pwd || printf '%s' "$PWD")"
  SGH_BIN="$_sgh_repo/target/release/sgh"
fi

if [ ! -x "$SGH_BIN" ]; then
  echo "sgh: no binary at $SGH_BIN" >&2
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
