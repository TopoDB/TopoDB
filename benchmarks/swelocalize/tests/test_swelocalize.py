import os
from swe.data import parse_gold_files, load_instances, Instance
from swe.corpus import iter_py_files, chunk_file
from swe.graph import build_import_graph
from swe.metrics import hit_at_k, reciprocal_rank, aggregate

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

def test_build_import_graph_resolves_absolute_and_relative():
    files = [
        ("pkg/__init__.py", ""),
        ("pkg/core.py", "from pkg.util import helper\nimport os\n"),
        ("pkg/util.py", "from . import base\n"),
        ("pkg/base.py", "x = 1\n"),
    ]
    g = build_import_graph(files)
    assert g["pkg/core.py"] == {"pkg/util.py"}     # `import os` dropped (stdlib)
    assert g["pkg/util.py"] == {"pkg/base.py"}      # relative `from . import base`
    assert g["pkg/base.py"] == set()

def test_build_import_graph_relative_import_with_module():
    files = [
        ("pkg/__init__.py", ""),
        ("pkg/core.py", "from .util import helper\n"),
        ("pkg/util.py", "def helper():\n    return 1\n"),
    ]
    g = build_import_graph(files)
    assert g["pkg/core.py"] == {"pkg/util.py"}   # `.util` module edge kept

def test_build_import_graph_ignores_unresolved():
    files = [("a.py", "import totally_not_here\n")]
    assert build_import_graph(files) == {"a.py": set()}

def test_hit_at_k_any_and_all():
    retrieved = ["a.py", "b.py", "c.py"]
    assert hit_at_k(retrieved, {"c.py"}, 3, "any") == 1
    assert hit_at_k(retrieved, {"c.py"}, 2, "any") == 0
    assert hit_at_k(retrieved, {"a.py", "c.py"}, 3, "all") == 1
    assert hit_at_k(retrieved, {"a.py", "c.py"}, 2, "all") == 0

def test_reciprocal_rank():
    assert reciprocal_rank(["a.py", "b.py"], {"b.py"}) == 0.5
    assert reciprocal_rank(["a.py"], {"z.py"}) == 0.0

def test_aggregate_means():
    per = [
        {"retrieved": ["a.py"], "gold": {"a.py"}},
        {"retrieved": ["x.py", "b.py"], "gold": {"b.py"}},
    ]
    agg = aggregate(per, ks=[1])
    assert agg["any@1"] == 0.5
    assert agg["mrr"] == (1.0 + 0.5) / 2

def test_harness_indexes_and_retrieves_by_path(tmp_path):
    from swe.store import Harness
    db = str(tmp_path / "swe.redb")
    h = Harness(db)
    # 3 files; toy 2-d vectors; core imports util.
    files = [
        ("pkg/core.py", "def crash_on_empty(x):\n    return x[0]\n", [1.0, 0.0]),
        ("pkg/util.py", "def helper():\n    return 1\n", [0.0, 1.0]),
        ("pkg/base.py", "VALUE = 7\n", [0.5, 0.5]),
    ]
    graph = {"pkg/core.py": {"pkg/util.py"}, "pkg/util.py": set(), "pkg/base.py": set()}
    id2path = h.index("00000000000000000000000000", files, graph)
    assert set(id2path.values()) == {"pkg/core.py", "pkg/util.py", "pkg/base.py"}
    hits = h.retrieve("00000000000000000000000000",
                      "crash on empty input", [1.0, 0.0], "text", 3, id2path)
    assert "pkg/core.py" in hits

def test_format_table_has_row_per_leg_and_header():
    from swe.report import format_table
    results = {
        "text":   {"any@1": 0.5, "any@5": 0.9, "all@1": 0.4, "all@5": 0.8, "mrr": 0.6},
        "graph":  {"any@1": 0.6, "any@5": 0.95, "all@1": 0.5, "all@5": 0.85, "mrr": 0.7},
    }
    table = format_table(results, ks=[1, 5])
    lines = table.splitlines()
    assert lines[0].split()[:1] == ["leg"]
    assert any(l.startswith("text") for l in lines)
    assert any(l.startswith("graph") for l in lines)
    assert "any@5" in lines[0] and "mrr" in lines[0]

def test_evaluate_scores_legs_with_injected_workspace_and_encoder(tmp_path):
    from swe.run import evaluate
    from swe.data import Instance

    # Build a toy repo on disk; the injected workspace returns its root.
    repo = tmp_path / "repo"
    (repo / "pkg").mkdir(parents=True)
    (repo / "pkg" / "core.py").write_text("def crash_on_empty(x):\n    return x[0]\n")
    (repo / "pkg" / "util.py").write_text("def helper():\n    return 1\n")

    inst = Instance("t-1", "org/pkg", "deadbeef",
                    "crash on empty input in core", gold_files={"pkg/core.py"})

    # Deterministic fake encoder: 2-d, keys off a substring so 'core' ranks itself.
    def encoder(texts):
        return [[1.0, 0.0] if "crash_on_empty" in t or "crash on empty" in t
                else [0.0, 1.0] for t in texts]

    out = evaluate([inst],
                   workspace=lambda i: str(repo),
                   encoder=encoder,
                   ks=(1,), depth=5, legs=("text",),
                   db_dir=str(tmp_path / "dbs"))
    assert out["results"]["text"]["any@1"] == 1.0
    assert out["manifest"]["n_instances"] == 1
    assert out["manifest"]["instance_ids"] == ["t-1"]
    assert out["gold_dist"] == {1: 1}   # one instance with exactly 1 gold file
    assert out["unretrievable"] == {"full": 0, "any": 0}

def test_parse_args_limit_and_out():
    from swe.run import parse_args
    ns = parse_args(["--limit", "30", "--out", "results/run.json"])
    assert ns.limit == 30
    assert ns.out == "results/run.json"

def test_parse_args_defaults():
    from swe.run import parse_args
    ns = parse_args([])
    assert ns.limit is None
    assert ns.out == "results/swelocalize.json"

def test_evaluate_counts_unretrievable_gold_for_created_files(tmp_path):
    from swe.run import evaluate
    from swe.data import Instance
    repo = tmp_path / "repo2"
    (repo / "pkg").mkdir(parents=True)
    (repo / "pkg" / "core.py").write_text("def f(x):\n    return x\n")
    # gold names a file that does NOT exist at base_commit (patch would create it)
    inst = Instance("t-2", "org/pkg", "deadbeef",
                    "add new module", gold_files={"pkg/newmod.py"})
    out = evaluate([inst],
                   workspace=lambda i: str(repo),
                   encoder=lambda texts: [[1.0, 0.0] for _ in texts],
                   ks=(1,), depth=5, legs=("text",),
                   db_dir=str(tmp_path / "dbs2"))
    assert out["unretrievable"] == {"full": 1, "any": 1}
    assert out["results"]["text"]["any@1"] == 0.0   # unretrievable -> hard zero


def test_evaluate_caches_embeddings_across_instances(tmp_path):
    """Two instances share a repo (identical file contents). Each unique file
    content must be embedded ONCE (cache hit on the second instance), not
    re-embedded per instance -- otherwise a 30-instance run re-embeds every
    repo N times. Query strings are embedded per instance (1 each)."""
    from swe.run import evaluate
    from swe.data import Instance
    repo = tmp_path / "repo"
    (repo / "pkg").mkdir(parents=True)
    (repo / "pkg" / "core.py").write_text("def f(x):\n    return x\n")
    (repo / "pkg" / "util.py").write_text("def g():\n    return 1\n")

    calls = {"texts": 0}
    def encoder(texts):
        calls["texts"] += len(texts)
        return [[1.0, 0.0] for _ in texts]

    insts = [
        Instance("i-1", "org/pkg", "c1", "fix core", gold_files={"pkg/core.py"}),
        Instance("i-2", "org/pkg", "c2", "fix util", gold_files={"pkg/util.py"}),
    ]
    evaluate(insts, workspace=lambda i: str(repo), encoder=encoder,
             ks=(1,), depth=5, legs=("text",), db_dir=str(tmp_path / "dbs"))
    # 2 unique files (1 chunk each) embedded once = 2 texts, cached for
    # instance 2; + 1 query embedding per instance = 2. Total = 4 (not 6).
    assert calls["texts"] == 4


def test_evaluate_indexes_once_per_repo(tmp_path):
    """Two instances of the SAME repo must build ONE shared store (indexed at a
    reference checkout), not one per instance -- otherwise each instance rebuilds
    the whole repo's text+vector index (the ~350s/instance floor). Both instances
    are still scored against that shared store."""
    import os as _os
    from swe.run import evaluate
    from swe.data import Instance
    repo = tmp_path / "repo"
    (repo / "pkg").mkdir(parents=True)
    (repo / "pkg" / "core.py").write_text("def crash_on_empty(x):\n    return x[0]\n")
    (repo / "pkg" / "util.py").write_text("def helper():\n    return 1\n")

    insts = [
        Instance("org__pkg-1", "org/pkg", "c1", "crash on empty core",
                 gold_files={"pkg/core.py"}),
        Instance("org__pkg-2", "org/pkg", "c2", "helper util issue",
                 gold_files={"pkg/util.py"}),
    ]
    db_dir = tmp_path / "dbs"
    out = evaluate(insts, workspace=lambda i: str(repo),
                   encoder=lambda texts: [[1.0, 0.0] for _ in texts],
                   ks=(1,), depth=5, legs=("text",), db_dir=str(db_dir))
    # One .redb for the shared repo, not two.
    assert len([f for f in _os.listdir(db_dir) if f.endswith(".redb")]) == 1
    # Both instances still scored.
    assert out["gold_dist"] == {1: 2}
    assert out["manifest"]["n_instances"] == 2
