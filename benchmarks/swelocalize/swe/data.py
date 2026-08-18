"""Load SWE-bench-Lite instances and derive the gold file set from a patch."""
import re
from dataclasses import dataclass, field

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

@dataclass(frozen=True)
class Instance:
    instance_id: str
    repo: str
    base_commit: str
    problem_statement: str
    gold_files: frozenset = field(default_factory=frozenset)

def _default_reader():
    from datasets import load_dataset
    return load_dataset("princeton-nlp/SWE-bench_Lite", split="test")

def load_instances(reader=_default_reader) -> list:
    rows = reader()
    out = []
    for r in rows:
        out.append(Instance(
            instance_id=r["instance_id"],
            repo=r["repo"],
            base_commit=r["base_commit"],
            problem_statement=r["problem_statement"],
            gold_files=frozenset(parse_gold_files(r["patch"])),
        ))
    return out
