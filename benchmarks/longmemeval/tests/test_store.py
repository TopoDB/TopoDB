import tempfile
from pathlib import Path
from lme.store import Harness, scope_for_index


def test_scope_for_index_is_deterministic_and_valid_ulid():
    a = scope_for_index(7)
    b = scope_for_index(7)
    assert a == b
    assert len(a) == 26  # ULID Crockford base32
    assert scope_for_index(7) != scope_for_index(8)


def test_ingest_then_retrieve_ranks_and_maps_sessions():
    with tempfile.TemporaryDirectory() as d:
        db_path = str(Path(d) / "lme.redb")
        h = Harness(db_path, model_tag="toy")
        scope = scope_for_index(0)
        # Two memories in two sessions; orthogonal 2-dim vectors.
        mems = [
            ("sess_a", "the dog is named rex", [1.0, 0.0]),
            ("sess_b", "weekend hiking trails", [0.0, 1.0]),
        ]
        id2session = h.ingest(scope, mems)
        assert set(id2session.values()) == {"sess_a", "sess_b"}

        # vector leg: query aligned with sess_a's vector -> sess_a first
        vec_hits = h.retrieve(scope, "dog", [1.0, 0.0], "vector", 10, id2session)
        assert vec_hits[0] == "sess_a"

        # text leg: query term "hiking" -> sess_b
        txt_hits = h.retrieve(scope, "hiking", [0.0, 0.0], "text", 10, id2session)
        assert txt_hits and txt_hits[0] == "sess_b"

        # hybrid leg: returns mapped sessions, does not error, finds sess_a
        hyb_hits = h.retrieve(scope, "dog", [1.0, 0.0], "hybrid", 10, id2session)
        assert "sess_a" in hyb_hits
