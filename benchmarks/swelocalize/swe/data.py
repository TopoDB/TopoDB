"""Load SWE-bench-Lite instances and derive the gold file set from a patch."""
import re

_PLUS_HEADER = re.compile(r"^\+\+\+ (?:b/)?(.+)$")

def parse_gold_files(patch: str) -> set[str]:
    """Return the repo-relative paths a unified diff modifies.

    Reads the `+++` target headers (the post-image path). New files whose
    source is /dev/null still have a real `+++ b/<path>` target, so they are
    included; deletions whose target is /dev/null are skipped."""
    gold: set[str] = set()
    for line in patch.splitlines():
        m = _PLUS_HEADER.match(line)
        if not m:
            continue
        path = m.group(1).strip()
        if path == "/dev/null":
            continue
        gold.add(path)
    return gold
