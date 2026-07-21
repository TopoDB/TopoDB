import pytest
import topodb
from topodb import ops

NOW = 1_700_000_000_000
S = ["shared"]


@pytest.fixture()
def db(tmp_path):
    spec = {"equality": [{"label": "Entity", "prop": "name"}], "text": []}
    with topodb.TopoDB.open_with(str(tmp_path / "t.redb"), spec) as db:
        r = db.submit(
            [
                ops.create_entity("ada"),
                ops.create_memory("ada wrote the first program"),
                ops.link("#1", "#0", "ABOUT"),
            ],
            now_ms=NOW,
        )
        yield db, r["ids"]


def test_node_and_missing(db):
    db, ids = db
    n = db.node(S, ids[0])
    assert n["label"] == "Entity"
    assert n["props"]["name"] == "ada"
    assert db.node(S, "01ARZ3NDEKTSV4RRFFQ69G5FAV") is None


def test_nodes_by_label_and_newest(db):
    db, _ = db
    assert len(db.nodes_by_label(S, "Entity")) == 1
    assert len(db.nodes_by_label_newest(S, "Memory", 10)) == 1
    assert db.nodes_by_label(S, "nope") == []


def test_nodes_by_prop_exact_normalized_and_unindexed(db):
    db, ids = db
    assert db.nodes_by_prop(S, "Entity", "name", "ada")[0]["id"] == ids[0]
    assert db.nodes_by_prop(S, "Entity", "name", "ADA") == []
    assert db.nodes_by_prop_normalized(S, "Entity", "name", "  ADA ")[0]["id"] == ids[0]
    with pytest.raises(topodb.RejectedError):
        db.nodes_by_prop(S, "Entity", "unindexed", "x")


def test_edges(db):
    db, ids = db
    edges = db.edges_from(S, ids[1])
    assert len(edges) == 1
    assert edges[0]["type"] == "about"
    assert len(db.all_edges_between(ids[1], ids[0])) == 1
    assert db.open_edges_between(ids[1], ids[0]) == [ids[2]]
    assert db.edges_from(S, ids[1], type="OTHER") == []


def test_traverse(db):
    db, ids = db
    sg = db.traverse(S, seeds=[ids[1]], max_hops=2)
    assert {n["id"] for n in sg["nodes"]} == {ids[0], ids[1]}
    assert len(sg["edges"]) == 1
    with pytest.raises(topodb.RejectedError):
        db.traverse(S, seeds=[ids[1]], max_hops=0)


def test_float_range(db):
    db, ids = db
    db.submit([ops.set_node_props(ids[0], {"score": 0.7})], now_ms=NOW + 1)
    assert len(db.nodes_by_float_range(S, "score", 0.0, 1.0)) == 1
