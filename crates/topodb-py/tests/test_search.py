import pytest
import topodb
from topodb import ops

NOW = 1_700_000_000_000
S = ["shared"]


@pytest.fixture()
def db(tmp_path):
    spec = {"equality": [], "text": [{"label": "Memory", "prop": "content"}]}
    with topodb.TopoDB.open_with(str(tmp_path / "t.redb"), spec) as db:
        r = db.submit(
            [
                ops.create_memory("ada wrote the first program"),
                ops.create_memory("the analytical engine computes"),
                ops.create_entity("ada"),
                ops.link("#0", "#2", "ABOUT"),
                ops.set_embedding("#0", "toy", [1.0, 0.0]),
                ops.set_embedding("#1", "toy", [0.0, 1.0]),
            ],
            now_ms=NOW,
        )
        yield db, r["ids"]


def test_search_text(db):
    db, ids = db
    hits = db.search_text(S, "first program", 5)
    assert hits[0]["node"]["id"] == ids[0]
    assert isinstance(hits[0]["score"], float)
    with pytest.raises(topodb.RejectedError):
        db.search_text(S, "x", 5, recency_weight=2.0)


def test_search_vector(db):
    db, ids = db
    hits = db.search_vector(S, "toy", [1.0, 0.0], 2)
    assert hits[0]["node"]["id"] == ids[0]
    with pytest.raises(topodb.RejectedError):
        db.search_vector(S, "toy", [], 2)
    assert db.search_vector(S, "unknown-model", [1.0, 0.0], 2) == []


def test_recall_text_and_vector_legs(db):
    db, ids = db
    hits = db.recall(S, "first program", 5, vector=("toy", [1.0, 0.0]), now_ms=NOW)
    assert hits[0]["node"]["id"] == ids[0]
    with pytest.raises(topodb.RejectedError):
        db.recall(S, "x", 5, labels=[])


def test_suggest_links(db):
    db, ids = db
    out = db.suggest_links(S, ids[1], 3, model="toy")
    assert isinstance(out, list)
    for s in out:
        assert {"node", "score", "common_neighbors", "structural", "semantic"} <= set(s)
