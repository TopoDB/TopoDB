import os
from swe.data import parse_gold_files, load_instances, Instance
from swe.corpus import iter_py_files, chunk_file

def test_load_instances_shapes_rows_and_derives_gold():
    rows = [{
        "instance_id": "pkg__pkg-1",
        "repo": "org/pkg",
        "base_commit": "abc123",
        "problem_statement": "core crashes on empty input",
        "patch": ("diff --git a/pkg/core.py b/pkg/core.py\n"
                  "--- a/pkg/core.py\n+++ b/pkg/core.py\n@@ -1 +1 @@\n-a\n+b\n"),
    }]
    got = load_instances(reader=lambda: rows)
    assert got == [Instance(
        instance_id="pkg__pkg-1", repo="org/pkg", base_commit="abc123",
        problem_statement="core crashes on empty input",
        gold_files={"pkg/core.py"},
    )]

def test_parse_gold_files_extracts_modified_paths():
    patch = (
        "diff --git a/src/pkg/core.py b/src/pkg/core.py\n"
        "--- a/src/pkg/core.py\n"
        "+++ b/src/pkg/core.py\n"
        "@@ -1,3 +1,3 @@\n"
        "-old\n+new\n"
        "diff --git a/src/pkg/util.py b/src/pkg/util.py\n"
        "--- a/src/pkg/util.py\n"
        "+++ b/src/pkg/util.py\n"
        "@@ -1 +1 @@\n-x\n+y\n"
    )
    assert parse_gold_files(patch) == {"src/pkg/core.py", "src/pkg/util.py"}

def test_parse_gold_files_ignores_dev_null_for_new_files():
    patch = (
        "diff --git a/new.py b/new.py\n"
        "--- /dev/null\n"
        "+++ b/new.py\n"
        "@@ -0,0 +1 @@\n+hello\n"
    )
    assert parse_gold_files(patch) == {"new.py"}

def test_parse_gold_files_skips_deleted_file_target():
    patch = (
        "diff --git a/gone.py b/gone.py\n"
        "--- a/gone.py\n"
        "+++ /dev/null\n"
        "@@ -1 +0,0 @@\n-x\n"
    )
    assert parse_gold_files(patch) == set()

def test_iter_py_files_returns_sorted_relpaths(tmp_path):
    (tmp_path / "pkg").mkdir()
    (tmp_path / "pkg" / "a.py").write_text("x = 1\n")
    (tmp_path / "pkg" / "__init__.py").write_text("")
    (tmp_path / "readme.md").write_text("nope")
    got = iter_py_files(str(tmp_path))
    assert got == [("pkg/__init__.py", ""), ("pkg/a.py", "x = 1\n")]

def test_chunk_file_splits_at_top_level_and_never_empty():
    src = "\n".join(f"def f{i}():\n    return {i}" for i in range(5))
    chunks = chunk_file(src, max_lines=4)
    assert len(chunks) >= 2
    assert "".join(chunks).replace("\n", "") == src.replace("\n", "")

def test_chunk_file_falls_back_on_unparseable():
    chunks = chunk_file("def broken(:\n  pass\n", max_lines=1)
    assert len(chunks) >= 1

def test_chunk_file_preserves_leading_header_and_comments():
    src = "#!/usr/bin/env python3\n# license header\n\ndef f():\n    return 1\n"
    assert "".join(chunk_file(src, max_lines=60)) == src

def test_chunk_file_preserves_content_between_statements():
    src = "def f0():\n    return 0\n\n# between defs\ndef f1():\n    return 1\n"
    joined = "".join(chunk_file(src, max_lines=2))
    assert "# between defs" in joined
    assert joined == src
