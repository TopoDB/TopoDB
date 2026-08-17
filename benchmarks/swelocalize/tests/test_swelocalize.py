from swe.data import parse_gold_files

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
