import threading
import pytest
import topodb
from topodb import ops

NOW = 1_700_000_000_000


def test_ops_since_and_current_seq(tmp_path):
    with topodb.TopoDB.open(str(tmp_path / "t.redb")) as db:
        r = db.submit([ops.create_entity("a"), ops.create_entity("b")], now_ms=NOW)
        assert db.current_seq() == r["last_seq"]
        evs = db.ops_since(0)
        assert [e["seq"] for e in evs] == [r["first_seq"], r["last_seq"]]
        assert "CreateNode" in str(evs[0]["op"])


def test_compact_then_ops_since_raises_compacted(tmp_path):
    with topodb.TopoDB.open(str(tmp_path / "t.redb")) as db:
        r = db.submit([ops.create_entity("a"), ops.create_entity("b")], now_ms=NOW)
        db.compact_ops(r["last_seq"])
        with pytest.raises(topodb.CompactedError) as ei:
            db.ops_since(0)
        assert ei.value.oldest == r["last_seq"]


def test_subscribe_delivers_and_times_out(tmp_path):
    with topodb.TopoDB.open(str(tmp_path / "t.redb")) as db:
        sub = db.subscribe(16)
        assert sub.next(timeout=0.05) is None  # nothing yet
        r = db.submit([ops.create_entity("a")], now_ms=NOW)
        ev = sub.next(timeout=5.0)
        assert ev["seq"] == r["first_seq"]
        sub.close()


def test_subscribe_iterator_releases_gil(tmp_path):
    with topodb.TopoDB.open(str(tmp_path / "t.redb")) as db:
        sub = db.subscribe(16)
        got = []

        def consume():
            got.append(sub.next(timeout=5.0))

        t = threading.Thread(target=consume)
        t.start()
        db.submit([ops.create_entity("a")], now_ms=NOW)  # must not deadlock
        t.join(timeout=5.0)
        assert not t.is_alive() and got and got[0] is not None


def test_iterator_protocol_ends_on_disconnect(tmp_path):
    db = topodb.TopoDB.open(str(tmp_path / "t.redb"))
    sub = db.subscribe(16)
    r = db.submit([ops.create_entity("a")], now_ms=NOW)
    db.close()  # drops the sender; buffered event still delivered, then StopIteration
    got = [ev for ev in sub]
    assert [ev["seq"] for ev in got] == [r["first_seq"]]
