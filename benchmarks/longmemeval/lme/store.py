"""Drive the real TopoDB recall pipeline through topodb-py."""
import hashlib

import topodb
from topodb import ops
from ulid import ULID


def scope_for_index(i: int) -> str:
    """Deterministic, valid ULID per question index (scope id never affects
    a recall number — it only isolates one question's memories)."""
    digest = hashlib.sha256(f"lme-scope-{i}".encode()).digest()[:16]
    return str(ULID.from_bytes(digest))


class Harness:
    def __init__(self, db_path: str, model_tag: str = "minilm-l6-v2"):
        spec = {
            "equality": [],
            "text": [{"label": "Memory", "prop": "content"}],
        }
        self._db = topodb.TopoDB.open_with(db_path, spec)
        self._model = model_tag

    def ingest(self, scope, mems):
        """mems: list[(session_id, content, vector)] -> {memory_id: session_id}.

        Two passes so no back-reference indexing is assumed: create all memories
        (ids are positionally aligned to the commands), then set embeddings by
        real id. `create_memory` via `submit()` never dedups, so identical
        contents still produce DISTINCT nodes; `id2session` therefore maps exactly
        one entry per memory (the `setdefault` is a harmless safety net)."""
        contents = [c for (_, c, _) in mems]
        res = self._db.submit([ops.create_memory(c) for c in contents], default_scope=scope)
        ids = res["ids"]
        self._db.submit(
            [ops.set_embedding(mid, self._model, list(v)) for mid, (_, _, v) in zip(ids, mems)],
            default_scope=scope,
        )
        id2session: dict[str, str] = {}
        for mid, (sid, _, _) in zip(ids, mems):
            id2session.setdefault(mid, sid)
        return id2session

    def retrieve(self, scope, query, query_vec, leg, depth, id2session):
        scopes = [scope]
        if leg == "text":
            hits = self._db.search_text(scopes, query, depth)
        elif leg == "vector":
            hits = self._db.search_vector(scopes, self._model, list(query_vec), depth)
        elif leg == "hybrid":
            hits = self._db.recall(
                scopes, query, depth,
                vector=(self._model, list(query_vec)),
                graph_boost=True,
            )
        else:
            raise ValueError(f"unknown leg: {leg!r}")
        out: list[str] = []
        for hit in hits:
            nid = hit["node"]["id"]
            sid = id2session.get(nid)
            if sid is not None:
                out.append(sid)
        return out
