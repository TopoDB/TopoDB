"""Drive the real TopoDB recall pipeline through topodb-py."""
import hashlib
from datetime import datetime, timezone

import topodb
from topodb import ops
from ulid import ULID

# Default per-node cap on outgoing co_mention edges. Surfaced as an ingest
# parameter so run.py can record the value it actually ran with in the manifest.
DEFAULT_FAN_OUT_CAP = 32


def scope_for_index(i: int) -> str:
    """Deterministic, valid ULID per question index (scope id never affects
    a recall number — it only isolates one question's memories)."""
    digest = hashlib.sha256(f"lme-scope-{i}".encode()).digest()[:16]
    return str(ULID.from_bytes(digest))


def _session_date_to_unix_ms(date_str):
    """Parse a LongMemEval session date string to Unix ms (naive dates are read
    as UTC). Returns None for missing/unparseable dates so the caller leaves the
    edge's valid_from open — the edge still exists and still counts."""
    if not date_str:
        return None
    s = date_str.strip()
    if not s:
        return None
    for fmt in (
        "%Y/%m/%d (%a) %H:%M",
        "%Y/%m/%d %H:%M",
        "%Y-%m-%d %H:%M:%S",
        "%Y-%m-%d %H:%M",
        "%Y-%m-%d",
    ):
        try:
            dt = datetime.strptime(s, fmt)
            return int(dt.replace(tzinfo=timezone.utc).timestamp() * 1000)
        except ValueError:
            continue
    try:
        dt = datetime.fromisoformat(s)
    except ValueError:
        return None
    if dt.tzinfo is None:
        dt = dt.replace(tzinfo=timezone.utc)
    return int(dt.timestamp() * 1000)


class Harness:
    def __init__(self, db_path: str, model_tag: str = "minilm-l6-v2",
                 graph_weight: float = 0.1):
        spec = {
            "equality": [],
            "text": [{"label": "Memory", "prop": "content"}],
        }
        self._db = topodb.TopoDB.open_with(db_path, spec)
        self._model = model_tag
        # RRF weight for the hybrid leg's graph contribution. The engine default
        # is 0.5, which on a dense real-data co_mention graph REORDERS an
        # already-good text+vector ranking and demotes correct top hits (see
        # RESULTS.md "Graph-leg activation"). 0.1 is the measured non-destructive
        # value: it matches the graph-off baseline on the hard types while still
        # surfacing buried gold on sparse structure.
        self._graph_weight = graph_weight
        # Set by ingest(build_graph=True); None otherwise. run.py reads this to
        # populate the manifest.
        self.graph_stats = None

    def ingest(self, scope, mems, *, build_graph: bool = False,
               fan_out_cap: int = DEFAULT_FAN_OUT_CAP, session_dates=None):
        """mems: list[(session_id, content, vector)] -> {memory_id: session_id}.

        Two passes so no back-reference indexing is assumed: create all memories
        (ids are positionally aligned to the commands), then set embeddings by
        real id. `create_memory` via `submit()` never dedups, so identical
        contents still produce DISTINCT nodes; `id2session` therefore maps exactly
        one entry per memory (the `setdefault` is a harmless safety net).

        When `build_graph` is True, a THIRD (single) submit lays down a graph on
        top of the memories: an Entity per cross-session shared surface form, a
        Memory --about--> Entity provenance edge, and the load-bearing
        cross-session Memory --co_mention--> Memory edges the hybrid leg's
        graph_boost rides on. The retrieval path is untouched. Graph counts are
        exposed on `self.graph_stats` (and returned unchanged: `id2session`).

        `session_dates`: optional {session_id: date_str} supplying valid_from for
        the edges; anything missing/unparseable leaves that edge open."""
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

        self.graph_stats = None
        if build_graph:
            self.graph_stats = self._build_graph(
                scope, ids, mems, fan_out_cap, session_dates or {}
            )
        return id2session

    def _build_graph(self, scope, ids, mems, fan_out_cap, session_dates):
        """Lay down the entity/co_mention graph in ONE submit() batch.

        Adapts to lme.extract's ACTUAL exported contract (the harness spec's
        assumed `shared_entities_and_pairs` does not exist):
          * extract_shared_entities(pairs) -> sorted {surface: set[session_id]}
            keyed only by surface forms shared across >=2 DISTINCT sessions;
          * cross_session_pairs(shared) -> sorted [(surface, session_a, session_b)]
            at SESSION granularity.
        The co_mention edges the hybrid leg's graph_boost rides on are
        Memory->Memory, so we induce them at MEMORY granularity: per-memory
        surface forms come from extract's `_content_surface_forms`, intersected
        with the cross-session shared set. `pairs` is [(session_id, content)] in
        ingest order, so memory index `i` maps positionally onto `ids[i]`, the
        real node id.

        Determinism: entities are created in extract's key-sorted order;
        `about` and `co_mention` edges are emitted in sorted index order. The
        retrieval path is untouched."""
        from lme.extract import extract_shared_entities, _iter_surface_forms

        pairs = [(sid, content) for (sid, content, _) in mems]
        shared = extract_shared_entities(pairs)    # sorted {surface: {session_id}}
        shared_set = set(shared)
        sid_of = [sid for (sid, _, _) in mems]     # memory index -> session id

        def date_ms(sid):
            return _session_date_to_unix_ms(session_dates.get(sid))

        entity_count = len(shared)
        edge_count = 0
        dateless = 0

        # A None valid_from means the source date was missing/unparseable: omit
        # the field entirely so the edge is left open (it still exists and still
        # counts). No silent truncation — every omission is tallied.
        def add_edge(src, dst, typ, vf):
            nonlocal edge_count, dateless
            if vf is None:
                batch.append(ops.link(src, dst, typ))
                dateless += 1
            else:
                batch.append(ops.link(src, dst, typ, valid_from=vf))
            edge_count += 1

        # (1) Entities first so `about` links can back-reference them by batch
        #     index (`#i` == id created by the i-th op in this submit).
        entity_surfaces = list(shared)             # extract returns key-sorted
        entity_index = {s: i for i, s in enumerate(entity_surfaces)}
        batch = [ops.create_entity(s) for s in entity_surfaces]

        # Per-memory shared surface forms: only forms that actually cross
        # sessions carry signal, so intersect with `shared_set`.
        mem_surfaces = [
            set(_iter_surface_forms(content)) & shared_set
            for (_, content, _) in mems
        ]

        # (2) about edges: Memory --about--> Entity provenance, deterministic
        #     order; valid_from = that memory's own session date.
        about = sorted(
            (mem_idx, surface)
            for mem_idx, surfaces in enumerate(mem_surfaces)
            for surface in surfaces
        )
        for mem_idx, surface in about:
            add_edge(ids[mem_idx], f"#{entity_index[surface]}", "about",
                     date_ms(sid_of[mem_idx]))

        # (3) co_mention edges: induce canonical undirected memory pairs from
        #     memories that share a surface form, keeping only DIFFERENT-session
        #     pairs (same-session pairs are skipped).
        surface_to_mems: dict = {}
        for mem_idx, surfaces in enumerate(mem_surfaces):
            for surface in surfaces:
                surface_to_mems.setdefault(surface, []).append(mem_idx)

        edges: set = set()        # {(src_idx, dst_idx)}  src_idx < dst_idx
        for surface in sorted(surface_to_mems):
            members = sorted(surface_to_mems[surface])
            for x in range(len(members)):
                for y in range(x + 1, len(members)):
                    a, b = members[x], members[y]          # a < b (members sorted)
                    if sid_of[a] == sid_of[b]:
                        continue                            # skip same-session
                    edges.add((a, b))

        # Deterministic per-source fan-out cap that keeps the lowest session-id
        # partners (ties broken by node index). Truncation is counted, never
        # silent.
        by_src: dict = {}
        for (src, dst) in edges:
            by_src.setdefault(src, []).append((sid_of[dst], dst))
        truncated = 0
        for src in sorted(by_src):
            partners = sorted(by_src[src])         # lowest partner session-id first
            kept = partners[:fan_out_cap]
            truncated += len(partners) - len(kept)
            vf = date_ms(sid_of[src])              # co_mention valid_from = source session
            for _dst_sid, dst in kept:
                add_edge(ids[src], ids[dst], "co_mention", vf)

        if batch:
            self._db.submit(batch, default_scope=scope)

        return {
            "entity_count": entity_count,
            "edge_count": edge_count,
            "dateless_edge_count": int(dateless),
            "fan_out_cap": fan_out_cap,
            "co_mention_truncated": truncated,
        }

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
                graph_weight=self._graph_weight,
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
