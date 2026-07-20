#!/usr/bin/env bash
# Tests for lib/sgh-env.sh. Plain shell: no bats, no node, nothing to install.
set -uo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
LIB="$HERE/../lib/sgh-env.sh"
fails=0

check() {
  local name="$1" expected="$2" actual="$3"
  if [ "$expected" = "$actual" ]; then
    echo "ok   - $name"
  else
    echo "FAIL - $name"
    echo "       expected: $expected"
    echo "       actual:   $actual"
    fails=$((fails + 1))
  fi
}

# 1. An explicit SGH_BIN is honored verbatim.
fake="$(mktemp -d)/sgh"
mkdir -p "$(dirname "$fake")" && touch "$fake" && chmod +x "$fake"
actual="$(SGH_BIN="$fake" bash -c "source '$LIB' >/dev/null && printf '%s' \"\$SGH_BIN\"")"
check "explicit SGH_BIN is honored" "$fake" "$actual"

# 2. A missing binary fails loudly, with the build command in the message.
out="$(SGH_BIN=/nonexistent/sgh bash -c "source '$LIB' 2>&1"; printf 'rc=%s' "$?")"
case "$out" in
  *"cargo build --release -p topodb-sgh"*rc=1*) echo "ok   - missing binary errors with build hint" ;;
  *) echo "FAIL - missing binary errors with build hint"; echo "       got: $out"; fails=$((fails + 1)) ;;
esac

# 3. SGH_DB is stable for one project directory.
a="$(cd / && SGH_BIN="$fake" bash -c "source '$LIB' >/dev/null && printf '%s' \"\$SGH_DB\"")"
b="$(cd / && SGH_BIN="$fake" bash -c "source '$LIB' >/dev/null && printf '%s' \"\$SGH_DB\"")"
check "SGH_DB is stable for one project" "$a" "$b"

# 4. Different projects get different databases — no cross-project bleed.
d1="$(mktemp -d)"; d2="$(mktemp -d)"
p1="$(cd "$d1" && SGH_BIN="$fake" bash -c "source '$LIB' >/dev/null && printf '%s' \"\$SGH_DB\"")"
p2="$(cd "$d2" && SGH_BIN="$fake" bash -c "source '$LIB' >/dev/null && printf '%s' \"\$SGH_DB\"")"
if [ "$p1" != "$p2" ]; then echo "ok   - distinct projects get distinct databases"; else
  echo "FAIL - distinct projects get distinct databases"; fails=$((fails + 1)); fi

# 5. An explicit SGH_DB overrides the derived path.
actual="$(SGH_BIN="$fake" SGH_DB=/tmp/custom.redb bash -c "source '$LIB' >/dev/null && printf '%s' \"\$SGH_DB\"")"
check "explicit SGH_DB is honored" "/tmp/custom.redb" "$actual"

# 6. The database never lands in the project directory (the CLI's bad default).
case "$p1" in
  "$d1"/*) echo "FAIL - SGH_DB must not live in the project dir"; fails=$((fails + 1)) ;;
  *) echo "ok   - SGH_DB lives outside the project dir" ;;
esac

# 7. The default binary-resolution path works when SGH_BIN is unset.
# Users who don't set SGH_BIN explicitly should get the fallback behavior:
# derive from CLAUDE_PLUGIN_ROOT and resolve to <repo>/target/release/sgh.
tmp_repo="$(mktemp -d)"
mkdir -p "$tmp_repo/plugins/sgh" "$tmp_repo/target/release"
touch "$tmp_repo/target/release/sgh" && chmod +x "$tmp_repo/target/release/sgh"
expected="$tmp_repo/target/release/sgh"
actual="$(env -u SGH_BIN CLAUDE_PLUGIN_ROOT="$tmp_repo/plugins/sgh" bash -c "source '$LIB' >/dev/null && printf '%s' \"\$SGH_BIN\"")"
check "default binary resolution from CLAUDE_PLUGIN_ROOT" "$expected" "$actual"

# 8. Both SGH_BIN and SGH_DB are genuinely exported to child processes.
# A previous reviewer deleted both export lines and all existing tests still passed
# because they only read the variables in the same shell. This test ensures
# the exports actually work — the variables must be visible to a child process.
tmp_derive="$(mktemp -d)"
mkdir -p "$tmp_derive/plugins/sgh" "$tmp_derive/target/release"
touch "$tmp_derive/target/release/sgh" && chmod +x "$tmp_derive/target/release/sgh"
expected_bin="$tmp_derive/target/release/sgh"
# Test SGH_BIN export: derive it via CLAUDE_PLUGIN_ROOT, verify child sees it
sgb_child="$(env -u SGH_BIN CLAUDE_PLUGIN_ROOT="$tmp_derive/plugins/sgh" bash -c "source '$LIB' >/dev/null && bash -c 'printf \"%s\" \"\$SGH_BIN\"'")"
# Test SGH_DB export: derived from pwd, verify child sees it
sgd_child="$(env -u SGH_BIN CLAUDE_PLUGIN_ROOT="$tmp_derive/plugins/sgh" bash -c "source '$LIB' >/dev/null && bash -c 'printf \"%s\" \"\$SGH_DB\"'")"
if [ -n "$sgb_child" ] && [ "$sgb_child" = "$expected_bin" ]; then
  echo "ok   - SGH_BIN is exported to child processes"
else
  echo "FAIL - SGH_BIN is exported to child processes"
  echo "       expected: $expected_bin"
  echo "       actual:   $sgb_child"
  fails=$((fails + 1))
fi
if [ -n "$sgd_child" ]; then
  echo "ok   - SGH_DB is exported to child processes"
else
  echo "FAIL - SGH_DB is exported to child processes"
  echo "       expected: non-empty"
  echo "       actual:   (empty)"
  fails=$((fails + 1))
fi

# 10. An in-repo release build is preferred over one on PATH: when you are
# developing in the repo, the build you just made is the one you mean.
repo="$(mktemp -d)"; mkdir -p "$repo/plugins/sgh" "$repo/target/release" "$repo/binstub"
printf '#!/bin/sh\n' > "$repo/target/release/sgh"; chmod +x "$repo/target/release/sgh"
printf '#!/bin/sh\n' > "$repo/binstub/sgh"; chmod +x "$repo/binstub/sgh"
actual="$(env -u SGH_BIN PATH="$repo/binstub:$PATH" CLAUDE_PLUGIN_ROOT="$repo/plugins/sgh" \
  bash -c "source '$LIB' >/dev/null && printf '%s' \"\$SGH_BIN\"")"
check "in-repo build beats PATH" "$repo/target/release/sgh" "$actual"

# 11. With no in-repo build, an `sgh` on PATH is used. This is the installed-
# plugin case: the plugin lives in a cache dir with no repo above it.
cache="$(mktemp -d)"; mkdir -p "$cache/plugins/sgh" "$cache/binstub"
printf '#!/bin/sh\n' > "$cache/binstub/sgh"; chmod +x "$cache/binstub/sgh"
actual="$(env -u SGH_BIN PATH="$cache/binstub:$PATH" CLAUDE_PLUGIN_ROOT="$cache/plugins/sgh" \
  bash -c "source '$LIB' >/dev/null && printf '%s' \"\$SGH_BIN\"")"
check "PATH is used when no in-repo build exists" "$cache/binstub/sgh" "$actual"

# 12. Nothing anywhere still fails loudly with the build hint — the error path
# must not be lost while adding fallbacks.
empty="$(mktemp -d)"; mkdir -p "$empty/plugins/sgh"
# PATH points at an empty (but real) dir so `sgh` is unfindable; bash is
# invoked by absolute path so the shell itself stays reachable.
nobin="$(mktemp -d)"
out="$(env -u SGH_BIN PATH="$nobin" CLAUDE_PLUGIN_ROOT="$empty/plugins/sgh" \
  /bin/bash -c "source '$LIB' 2>&1"; printf 'rc=%s' "$?")"
case "$out" in
  *"cargo build --release -p topodb-sgh"*rc=1*) echo "ok   - still errors with build hint when nothing is found" ;;
  *) echo "FAIL - still errors with build hint when nothing is found"; echo "       got: $out"; fails=$((fails + 1)) ;;
esac

echo
if [ "$fails" -eq 0 ]; then echo "all passed"; else echo "$fails failed"; fi
exit "$fails"
