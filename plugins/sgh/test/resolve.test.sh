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

echo
if [ "$fails" -eq 0 ]; then echo "all passed"; else echo "$fails failed"; fi
exit "$fails"
