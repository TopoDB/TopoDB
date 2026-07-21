import json
import pathlib
import sys
import pytest
import topodb

ROOT = pathlib.Path(__file__).parents[3]
FIXTURES = sorted((ROOT / "fixtures" / "parity").glob("*.json"))
sys.path.insert(0, str(ROOT / "scripts"))
from parity_lib import resolve  # noqa: E402


@pytest.mark.parametrize("path", FIXTURES, ids=lambda p: p.stem)
def test_parity(path, tmp_path):
    fx = json.loads(path.read_text())
    spec = fx.get("index_spec", {"equality": [], "text": []})
    with topodb.TopoDB.open_with(str(tmp_path / "t.redb"), spec) as db:
        ids = db.submit(fx["batch"], now_ms=fx["now_ms"])["ids"]
        # Handle optional second batch (for temporal tests)
        if "batch2" in fx:
            batch2 = resolve(fx["batch2"], ids)
            db.submit(batch2, now_ms=fx["now_ms2"])
        # Run checks
        for chk in fx["checks"]:
            args = resolve(chk["args"], ids)
            call, out = chk["call"], None
            if call == "node":
                out = db.node(args["scopes"], args["id"])
                assert out["label"] == chk["expect_label"]
            elif call == "nodes_by_label":
                out = db.nodes_by_label(args["scopes"], args["label"])
                assert [n["id"] for n in out] == resolve(chk["expect_ids"], ids)
            elif call == "search_text":
                out = db.search_text(args["scopes"], args["query"], args["k"])
                assert [h["node"]["id"] for h in out] == resolve(chk["expect_ids"], ids)
            elif call == "traverse":
                out = db.traverse(args["scopes"], seeds=args["seeds"],
                                  max_hops=args["max_hops"], as_of=args.get("as_of"))
                assert sorted(n["id"] for n in out["nodes"]) == sorted(resolve(chk["expect_node_ids"], ids))
                if "expect_edge_count" in chk:
                    assert len(out["edges"]) == chk["expect_edge_count"]
            else:
                pytest.fail(f"unknown check call {call!r}")
