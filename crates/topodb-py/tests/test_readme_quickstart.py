"""Execute the README's own quickstart block verbatim.

The quickstart is the first code a user runs; this keeps it honest. The bug
class is real: a shipped draft opened an unindexed store (plain open()) and
then indexed into search results that were necessarily empty.
"""

import pathlib
import re

README = pathlib.Path(__file__).parents[1] / "README.md"


def test_readme_quickstart_runs_and_search_hits(tmp_path, capsys):
    text = README.read_text(encoding="utf-8")
    m = re.search(r"## Quickstart\s+```python\n(.*?)```", text, re.S)
    assert m, "README has a ## Quickstart ```python block"
    code = m.group(1)
    # Redirect the db file into tmp_path; everything else runs as written.
    code = code.replace('"memory.redb"', repr(str(tmp_path / "memory.redb")))
    exec(compile(code, str(README), "exec"), {})  # noqa: S102 - the README is ours
    out = capsys.readouterr().out
    assert "first program" in out, f"quickstart search printed no hit: {out!r}"
