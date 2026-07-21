import time
import topodb
from topodb import ops

NOW = 1_700_000_000_000
S = ["shared"]


def wait_for_stats(db, scopes, node_id, deadline_s=5.0):
    """Poll until access count becomes positive (async bumper thread behavior)."""
    end = time.monotonic() + deadline_s
    while time.monotonic() < end:
        st = db.access_stats(scopes, node_id)
        if st is not None and st["access_count"] >= 1:
            return st
        time.sleep(0.01)
    raise AssertionError("access count never became positive within deadline")


def test_admin_surface(tmp_path):
    spec = {"equality": [{"label": "Entity", "prop": "name"}], "text": []}
    with topodb.TopoDB.open_with(str(tmp_path / "t.redb"), spec) as db:
        r = db.submit([ops.create_entity("ada")], now_ms=NOW)
        assert db.index_spec() == spec
        report = db.storage_report()
        assert isinstance(report, list) and report
        # Node exists but never accessed: stats exist with access_count=0
        st0 = db.access_stats(S, r["ids"][0])
        assert st0 is not None and st0["access_count"] == 0
        # Absent node: returns None
        assert db.access_stats(S, "01ARZ3NDEKTSV4RRFFQ69G5FAV") is None
        db.nodes_by_label(S, "Entity")  # bumps (async, so poll for result)
        st = wait_for_stats(db, S, r["ids"][0])
        assert st["access_count"] >= 1
        db.rebuild_state_from_ops()
        assert db.node(S, r["ids"][0]) is not None
        assert len(db.debug_dump_nodes()) == 1
        assert db.debug_dump_edges() == []


def test_open_stored_reopens(tmp_path):
    p = str(tmp_path / "t.redb")
    with topodb.TopoDB.open(p) as db:
        db.submit([ops.create_entity("ada")], now_ms=NOW)
    with topodb.TopoDB.open_stored(p) as db:
        assert len(db.nodes_by_label(S, "Entity")) == 1
