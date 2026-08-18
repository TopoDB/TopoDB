"""Drive the real TopoDB recall pipeline over a single repo's files+graph.

Mirrors benchmarks/longmemeval/lme/store.py: files are Memory nodes, imports
are edges, and the four legs map onto search_text / search_vector / recall."""
import topodb
from topodb import ops

class Harness:
    def __init__(self, db_path: str, model_tag: str = "minilm-l6-v2",
                 graph_weight: float = 0.1):
        spec = {"equality": [], "text": [{"label": "Memory", "prop": "content"}]}
        self._db = topodb.TopoDB.open_with(db_path, spec)
        self._model = model_tag
        self._graph_weight = graph_weight

    def index(self, scope: str, files: list, graph: dict) -> dict:
        # Pass 1: create one Memory node per file (path prepended so path tokens
        # are lexically searchable). create_memory never dedups, so positional
        # alignment of ids -> files is exact.
        docs = [f"{rel}\n{content}" for (rel, content, _v) in files]
        res = self._db.submit([ops.create_memory(d) for d in docs], default_scope=scope)
        ids = res["ids"]
        path2id = {rel: mid for mid, (rel, _c, _v) in zip(ids, files)}
        id2path = {mid: rel for mid, (rel, _c, _v) in zip(ids, files)}
        # Pass 2: embeddings.
        self._db.submit(
            [ops.set_embedding(mid, self._model, list(v))
             for mid, (_r, _c, v) in zip(ids, files)],
            default_scope=scope,
        )
        # Pass 3: one directed `imports` edge per resolved import (recall
        # traverses direction=Both, so a single direction suffices).
        edges = []
        for src, dsts in graph.items():
            if src not in path2id:
                continue
            for dst in sorted(dsts):
                if dst in path2id:
                    edges.append(ops.link(path2id[src], path2id[dst], "imports"))
        if edges:
            self._db.submit(edges, default_scope=scope)
        return id2path

    def retrieve(self, scope: str, query: str, query_vec: list, leg: str,
                 depth: int, id2path: dict) -> list:
        scopes = [scope]
        if leg == "text":
            hits = self._db.search_text(scopes, query, depth)
        elif leg == "vector":
            hits = self._db.search_vector(scopes, self._model, list(query_vec), depth)
        elif leg == "hybrid":
            hits = self._db.recall(scopes, query, depth,
                                   vector=(self._model, list(query_vec)),
                                   graph_boost=False)
        elif leg == "graph":
            hits = self._db.recall(scopes, query, depth,
                                   vector=(self._model, list(query_vec)),
                                   graph_boost=True, graph_weight=self._graph_weight)
        else:
            raise ValueError(f"unknown leg: {leg!r}")
        out = []
        seen = set()
        for hit in hits:
            rel = id2path.get(hit["node"]["id"])
            if rel is not None and rel not in seen:
                seen.add(rel)
                out.append(rel)
        return out
