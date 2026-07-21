import pytest
import topodb
from topodb import ops

NOW = 1_700_000_000_000


def test_submit_batch_with_backrefs(tmp_path):
    with topodb.TopoDB.open(str(tmp_path / "t.redb")) as db:
        r = db.submit(
            [
                ops.create_entity("ada"),
                ops.create_memory("ada wrote the first program"),
                ops.link("#1", "#0", "ABOUT"),
            ],
            now_ms=NOW,
        )
        assert set(r) == {"first_seq", "last_seq", "ids"}
        assert r["last_seq"] - r["first_seq"] == 2
        assert len(r["ids"]) == 3
        assert all(isinstance(i, str) for i in r["ids"])


def test_submit_rejects_bad_batch(tmp_path):
    with topodb.TopoDB.open(str(tmp_path / "t.redb")) as db:
        with pytest.raises(topodb.RejectedError):
            db.submit([{"op": "no_such_op"}])
        with pytest.raises(topodb.RejectedError):
            db.submit({"not": "an array"})


def test_submit_default_scope_and_explicit_scope(tmp_path):
    with topodb.TopoDB.open(str(tmp_path / "t.redb")) as db:
        with pytest.raises(topodb.RejectedError):
            db.submit([ops.create_entity("x")], default_scope="not-a-ulid")
        r = db.submit([ops.create_entity("x")], default_scope="shared", now_ms=NOW)
        assert len(r["ids"]) == 1


def test_ops_builders_shapes():
    assert ops.create_entity("ada") == {"op": "create_entity", "name": "ada"}
    assert ops.link("#1", "#0", "ABOUT") == {
        "op": "link", "from": "#1", "to": "#0", "type": "ABOUT"}
    assert ops.set_node_props("id1", {"k": None}) == {
        "op": "set_node_props", "id": "id1", "props": {"k": None}}
