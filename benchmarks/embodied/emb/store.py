"""Thin TopoDB wrapper for the embodied-agent memory spike.

Maps the embodied world (§4.2 of the design) onto TopoDB's generic primitives —
entities, episodic memories, bi-temporal edges — and exposes exactly the query
handles the taxonomy layer (§4.3) needs. No embodiment-special engine features:
if these methods answer the queries, the *existing* engine suffices.

Contract (pinned — the query layer depends only on these):
    EmbodiedStore(db_path).open()
    .submit(commands)                              batch of `topodb.ops` builders
    .edges_from(node, type, open_only, as_of)      outgoing edges, bi-temporal
    .traverse(seeds, max_hops, edge_types, as_of)  reachable nodes
    .search(query, k)                              top-k memory hits
    .node_id(name)                                 entity node id by world name

Edge-type vocabulary the higher layers rely on:
    "located_in"  object -> place   (bi-temporal belief; superseded on a move)
    "adjacent"    place  <-> place   (paired, open — the floor-plan graph)
    "about"       memory -> entity   (episodic provenance)
Entities carry an `etype` prop in {place, object, person}; an entity's `name`
prop is its world id (what `node_id` resolves).
"""

import hashlib

import topodb
from topodb import ops  # noqa: F401  (re-exported for the ingest layer's convenience)
from ulid import ULID

# Node labels TopoDB stamps for its built-in write shapes (create_entity /
# create_memory). Mirrored here so `node_id`'s equality lookup and the index
# spec name the same (label, prop) pairs the engine uses.
ENTITY_LABEL = "Entity"
ENTITY_NAME_PROP = "name"
MEMORY_LABEL = "Memory"
MEMORY_CONTENT_PROP = "content"


def scope_for(db_path: str) -> str:
    """Deterministic, valid ULID scope derived from the db path.

    One agent = one scope (§4.2). Derivation is stable across runs so a rerun
    against the same path reads its own prior memories — the scope id never
    affects a score, it only isolates this robot's memory."""
    digest = hashlib.sha256(f"emb-scope-{db_path}".encode()).digest()[:16]
    return str(ULID.from_bytes(digest))


def _as_id(x):
    """Accept either a bare node-id string or a node/record dict (callers and
    tests pass both) and return the id string."""
    if isinstance(x, dict):
        return x.get("id") or x.get("node")
    return x


def _live_at(edge: dict, t: int) -> bool:
    """Mirror the engine's `edge_live_at`: valid on the world-time axis at `t`
    when valid_from <= t and (open OR valid_to > t). Inclusive lower, exclusive
    upper — same semantics the CLI/MCP `--as-of` filters use."""
    if edge["valid_from"] > t:
        return False
    vt = edge["valid_to"]
    return vt is None or vt > t


class EmbodiedStore:
    def __init__(self, db_path: str):
        self._db_path = db_path
        self._scope = scope_for(db_path)
        self._scopes = [self._scope]
        self._db = None

    @property
    def scope(self) -> str:
        return self._scope

    def open(self) -> "EmbodiedStore":
        """Open (or create) the db with the equality/text indexes the queries
        need: equality on (Entity, name) so `node_id` resolves a world name,
        text on (Memory, content) so `search` ranks episodic memories."""
        spec = {
            "equality": [{"label": ENTITY_LABEL, "prop": ENTITY_NAME_PROP}],
            "text": [{"label": MEMORY_LABEL, "prop": MEMORY_CONTENT_PROP}],
        }
        self._db = topodb.TopoDB.open_with(self._db_path, spec)
        return self

    def submit(self, commands=None, now_ms=None):
        """Apply a batch of `topodb.ops` command dicts under this run's scope.

        `now_ms` sets the belief-axis timestamp (recorded_at) for the batch —
        pass the event instant so bi-temporal supersession (close old
        `located_in`, open new) is deterministic. Returns the engine result
        (incl. `ids`, positionally aligned to `commands`) so the ingest layer
        can back-reference freshly created nodes.

        `commands` is optional: the convenience methods below submit
        immediately (one transaction each), so the ingest layer's per-phase
        `submit()` flush is a harmless no-op here."""
        if not commands:
            return {"ids": []}
        return self._db.submit(commands, default_scope=self._scope, now_ms=now_ms)

    def node_id(self, name: str):
        """Resolve an entity's node id by its world `name`, or None if absent."""
        hits = self._db.nodes_by_prop(
            self._scopes, ENTITY_LABEL, ENTITY_NAME_PROP, name
        )
        return hits[0]["id"] if hits else None

    def edges_from(self, node, type, open_only=False, as_of=None):
        """Outgoing edges of `type` from `node` (a node id), filtered on the
        world-time (valid) axis.

        - `open_only`: keep only edges still open (valid_to is None).
        - `as_of` (Unix ms): keep only edges live at that instant. Closed edges
          that were live then are retained — this is the bi-temporal read.
        When `as_of` is set the engine's own open-only prefilter is bypassed
        (it would drop history), and both filters are applied in Python."""
        node = _as_id(node)
        prefilter = open_only and as_of is None
        edges = self._db.edges_from(self._scopes, node, None, type, prefilter)
        if as_of is None and open_only is not True:
            return edges  # engine already applied open_only (or none requested)

        out = []
        for e in edges:
            if as_of is not None and not _live_at(e, as_of):
                continue
            if open_only and e["valid_to"] is not None:
                continue
            out.append(e)
        return out

    def traverse(self, seeds, max_hops, edge_types=None, as_of=None):
        """Nodes reachable from `seeds` (node ids) within `max_hops`, following
        `edge_types` in either direction. Bi-temporal: `as_of` (Unix ms) walks
        the graph as it stood at that instant. Returns the reached node records
        (list of dicts)."""
        sg = self._db.traverse(
            self._scopes,
            [_as_id(s) for s in seeds],
            max_hops,
            edge_types,
            "both",
            as_of,
        )
        return sg["nodes"]

    def search(self, query, k):
        """Top-`k` episodic memory hits for `query` (text ranking over Memory
        content). Returns a list of {node, score} rows."""
        return self._db.search_text(self._scopes, query, k)

    # ------------------------------------------------------------------ #
    # Convenience surface the ingest (§4.2) and query (§4.3) layers call. #
    # Each is a thin composition of the primitives above — no            #
    # embodiment-special engine feature — so a passing taxonomy means the #
    # existing engine suffices.                                          #
    # ------------------------------------------------------------------ #

    def entity(self, name, etype):
        """Find-or-create the `Entity` named `name`, stamped with its `etype`
        ({place,object,person}); returns the node id."""
        nid = self.node_id(name)
        if nid is None:
            nid = self.submit([ops.create_entity(name)])["ids"][0]
        self.submit([ops.set_node_props(nid, {"etype": etype})])
        return nid

    def memory(self, content, now_ms=None, ts_ms=None):
        """Create an episodic `Memory`. Memories are created in event order, so
        ULID id order *is* recency order (what `recent_about` sorts on). `ts_ms`
        records the world-time of the underlying event as a prop, so a
        multi-hop query can recover "then" (the instruction instant). Returns
        the node id."""
        mid = self.submit([ops.create_memory(content)], now_ms=now_ms)["ids"][0]
        props = {"kind": "episodic"}
        if ts_ms is not None:
            props["ts_ms"] = ts_ms
        self.submit([ops.set_node_props(mid, props)])
        return mid

    def link(self, from_node, to_node, type, valid_from=None):
        """Open a typed edge and return its id. `valid_from` sets the world-time
        the belief begins (the bi-temporal half of `located_in`)."""
        return self.submit(
            [ops.link(from_node, to_node, type, valid_from=valid_from)]
        )["ids"][0]

    def close_edge(self, from_node, to_node, type, valid_to):
        """Close the currently-open `type` edge from `from_node` to `to_node` at
        `valid_to` — the supersession half of a move. Returns the closed edge
        id, or None if no such open edge existed."""
        for e in self.edges_from(from_node, type, open_only=True):
            if e["to"] == to_node:
                self.submit([ops.close_edge(e["id"], valid_to=valid_to)])
                return e["id"]
        return None

    # --- query handles (§4.3), shapes matched to the `queries` layer ------ #

    def located_in(self, obj_node, as_of_ms=None):
        """The place an object is in: the OPEN `located_in` target now, or the
        one live at `as_of_ms`. Returns a place node id (str) or None."""
        if as_of_ms is None:
            edges = self.edges_from(obj_node, "located_in", open_only=True)
        else:
            edges = self.edges_from(obj_node, "located_in", as_of=as_of_ms)
        return edges[0]["to"] if edges else None

    def location_history(self, obj_node):
        """Every `located_in` interval (closed and open), chronological
        (oldest first) so `[-1]` is the current place and `[-2]` is where it
        last moved from. Each row: `{"place","valid_from","valid_to"}`."""
        edges = self.edges_from(obj_node, "located_in", open_only=False)
        edges.sort(key=lambda e: e["valid_from"])
        return [
            {"place": e["to"], "valid_from": e["valid_from"], "valid_to": e["valid_to"]}
            for e in edges
        ]

    def adjacent(self, room_node):
        """Place node ids one `adjacent` hop from `room_node` (either direction
        — the edge is stored once per unordered pair)."""
        return [
            n["id"]
            for n in self.traverse([room_node], 1, ["adjacent"])
            if n["id"] != room_node
        ]

    def objects_in(self, room_node, as_of_ms=None):
        """Object node ids whose current (or `as_of_ms`) `located_in` resolves
        to `room_node`. Reverse lookup: reach candidates by traversal (which
        walks closed edges too), then keep only those whose belief actually
        points at this room now/as-of."""
        out = []
        for n in self.traverse([room_node], 1, ["located_in"], as_of=as_of_ms):
            nid = n["id"]
            if nid == room_node:
                continue
            if self.located_in(nid, as_of_ms=as_of_ms) == room_node:
                out.append(nid)
        return out

    def recent_about(self, entity_node, limit=None):
        """Episodic memories with an `about` edge to `entity_node`, most-recent
        first (ULID id descending == event order). Each row:
        `{"id","content","ts_ms","about"}`, where `about` is the list of node
        ids the memory points at (so a multi-hop query can find its object)."""
        mems = [
            n
            for n in self.traverse([entity_node], 1, ["about"])
            if n.get("label") == MEMORY_LABEL and n["id"] != entity_node
        ]
        mems.sort(key=lambda n: n["id"], reverse=True)
        chosen = mems[:limit] if limit else mems
        rows = []
        for n in chosen:
            props = n.get("props") or {}
            about = [e["to"] for e in self.edges_from(n["id"], "about", open_only=False)]
            rows.append(
                {
                    "id": n["id"],
                    "content": props.get(MEMORY_CONTENT_PROP, ""),
                    "ts_ms": props.get("ts_ms"),
                    "about": about,
                }
            )
        return rows
