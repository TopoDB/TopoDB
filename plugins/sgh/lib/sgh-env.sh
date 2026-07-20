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
