"""Deterministic ingestion: synthetic embodied world -> TopoDB.

This is the core contribution of the embodied-agent memory spike (see
``docs/superpowers/specs/2026-08-14-embodied-agent-memory-spike-design.md``,
§4.2).  It translates a fully-known synthetic world into the existing engine
primitives so the query taxonomy (§4.3) can be answered and scored against
generator ground-truth.

All TopoDB interaction goes through the passed ``store`` (an
``emb.store.EmbodiedStore``).  This module never imports ``topodb`` directly and
consumes ``world`` (an ``emb.world.World``) purely by attribute.

Assumed collaborator contract
-----------------------------
``store`` provides (batch-oriented; ids are opaque node identifiers):
  * ``entity(name, etype) -> node_id`` — find-or-create an entity node with
    props ``etype`` and ``name``.
  * ``memory(content) -> node_id`` — create an episodic memory node.
  * ``link(src, dst, rel, valid_from=None) -> edge_id`` — open a (directed)
    edge; ``valid_from`` stamps bi-temporal validity when given.
  * ``close_edge(src, dst, rel, valid_to) -> None`` — close the currently-open
    edge matching ``src -> dst`` with relation ``rel``.
  * ``submit() -> None`` — flush the pending batch (optional; called if present).

``world`` exposes ``places``, ``objects``, ``people`` (each element carrying a
world id), an ``adjacency`` of place-id pairs, and an ordered ``events`` stream.
Each event carries a ``kind`` in {observe, move, instruction, action} plus a
time and kind-specific references.

The mapping is deterministic: collections are consumed in their given order and
adjacency pairs are de-duplicated by unordered identity, so repeated runs over
the same seeded world yield an identical graph and an identical id-map.
"""


# --- small, documented tolerance for collaborator attribute naming ----------
# The world/store are built by sibling modules to the same spec; these helpers
# absorb the couple of high-risk naming choices without changing the contract.

def _values(coll):
    """Iterate a collection that may be a list/tuple/set or an id->obj dict."""
    if coll is None:
        return []
    if isinstance(coll, dict):
        return list(coll.values())
    return list(coll)


def _get(obj, *names, default=None):
    """First present, non-None attribute (or dict key) among ``names``."""
    for name in names:
        if isinstance(obj, dict):
            if name in obj and obj[name] is not None:
                return obj[name]
        else:
            val = getattr(obj, name, None)
            if val is not None:
                return val
    return default


def _wid(x):
    """The world id of an element (a bare id string, or its ``id``/``name``)."""
    if isinstance(x, str):
        return x
    return _get(x, "id", "world_id", "name", default=x)


def _submit(store):
    submit = getattr(store, "submit", None)
    if callable(submit):
        submit()


def ingest_world(store, world):
    """Ingest ``world`` into ``store`` and return {world-id -> node-id}.

    The returned id-map covers every place, object, and person.  Alongside the
    entities it lays down: paired-but-open ``adjacent`` edges between places,
    episodic ``Memory`` nodes with ``about`` provenance edges, and bi-temporal
    ``located_in`` belief edges (opened on observation, superseded on a move).
    """
    id_map = {}

    # --- 1. Entities: every place / object / person -------------------------
    # name = world id, prop etype in {place, object, person}.
    for place in _values(getattr(world, "places", None)):
        wid = _wid(place)
        id_map[wid] = store.entity(wid, "place")
    for obj in _values(getattr(world, "objects", None)):
        wid = _wid(obj)
        id_map[wid] = store.entity(wid, "object")
    for person in _values(getattr(world, "people", None)):
        wid = _wid(person)
        id_map[wid] = store.entity(wid, "person")
    _submit(store)

    # --- 2. Adjacency: ONE directed edge per unordered pair, left open ------
    # The floor plan lives on each place as an `adjacent` neighbour-id list.
    seen_pairs = set()
    for place in _values(getattr(world, "places", None)):
        a = _wid(place)
        for nb in _values(_get(place, "adjacent", "neighbors", "adj", default=[])):
            b = _wid(nb)
            if a == b or a not in id_map or b not in id_map:
                continue
            key = frozenset((a, b))
            if key in seen_pairs:
                continue
            seen_pairs.add(key)
            # canonical, deterministic direction for the single directed edge
            src, dst = sorted((a, b))
            store.link(id_map[src], id_map[dst], "adjacent")
    _submit(store)

    # --- 3. Event stream: episodic memories + bi-temporal located_in --------
    # Track the currently-open located_in place per object so a move (or a
    # relocating observation) can close it before opening the successor.
    open_loc = {}

    def set_location(obj_wid, place_wid, t):
        """Ensure an open ``located_in`` edge obj->place, superseding any prior."""
        cur = open_loc.get(obj_wid)
        if cur == place_wid:
            return  # belief unchanged; leave the open edge as-is
        if cur is not None:
            store.close_edge(id_map[obj_wid], id_map[cur], "located_in", t)
        store.link(id_map[obj_wid], id_map[place_wid], "located_in", valid_from=t)
        open_loc[obj_wid] = place_wid

    def about(mem_id, *world_ids):
        for wid in world_ids:
            if wid is not None and wid in id_map:
                store.link(mem_id, id_map[wid], "about")

    for ev in _values(getattr(world, "events", None)):
        # World op vocabulary is {observe, move, instruct, act}; tolerate the
        # a couple of alias spellings the sibling modules might use.
        kind = _get(ev, "op", "kind", "type", "event", default="")
        t = _get(ev, "t_ms", "t", "time", "at", "when")

        obj = _wid_or_none(_get(ev, "object", "obj"))
        place = _wid_or_none(_get(ev, "place", "room", "location"))
        dest = _wid_or_none(_get(ev, "to", "dest", "destination"))
        src = _wid_or_none(_get(ev, "from", "src", "from_", "frm", "source"))
        person = _wid_or_none(_get(ev, "person", "who", "speaker"))
        refs = [_wid(r) for r in _values(_get(ev, "refs", default=[]))]
        text = _get(ev, "text", "content", "utterance", "instruction")

        if kind == "observe":
            content = "Saw {} in the {}.".format(obj, place)
            mem = store.memory(content, ts_ms=t)
            about(mem, obj, place)
            if obj is not None and place is not None:
                set_location(obj, place, t)

        elif kind == "move":
            frm = src if src is not None else open_loc.get(obj)
            content = "Moved {} from the {} to the {}.".format(obj, frm, dest)
            mem = store.memory(content, ts_ms=t)
            about(mem, obj, frm, dest)
            if obj is not None and dest is not None:
                set_location(obj, dest, t)

        elif kind in ("instruct", "instruction"):
            content = text or "{} gave an instruction.".format(person)
            mem = store.memory(content, ts_ms=t)
            about(mem, person, *refs)

        elif kind in ("act", "action"):
            verb = _get(ev, "action", "verb", "action_verb", default="did")
            target = place if place is not None else obj
            content = text or "{} the {}.".format(str(verb).capitalize(), target)
            mem = store.memory(content, ts_ms=t)
            about(mem, place, obj, person)

        else:
            # Unknown event kind: record an episodic note about any references
            # it names so provenance is never silently dropped.
            content = text or "Event: {}.".format(kind or "unknown")
            mem = store.memory(content, ts_ms=t)
            about(mem, obj, place, dest, src, person, *refs)

    _submit(store)

    return id_map


def _wid_or_none(x):
    return None if x is None else _wid(x)
