from swe.data import parse_gold_files, load_instances, Instance

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
