"""Deterministic, no-LLM query answerers for the embodied-agent memory spike.

Every embodied-memory query is answered through the existing TopoDB engine
primitives, reached ONLY via the ``EmbodiedStore`` handed to :func:`answer`.
This module never imports ``topodb`` and never reads a query's ground-truth
answer — it dispatches on ``query["type"]`` and resolves everything from the
graph, exactly as §4.3 of the spike design prescribes.

The generator embeds the referenced entities in the query's natural-language
``text`` (e.g. "where is the red_book?"), so we recover them by scanning the
text for known world ids classified by ``world`` — no NLP, just membership.

Return shape mirrors the generator's own answers:
  belief / temporal / metric_spatial → scalar place name (metric_spatial: a
    *list* of nearby objects via the documented same-room proxy — the gap)
  state_change → the ordered place trajectory (list)
  dialogue → the instruction text(s) about a person (list)
  multihop → "yes" / "no"
  room_graph → the objects in adjacent rooms (list)
"""

import re

QUERY_TYPES = (
    "belief",
    "temporal",
    "state_change",
    "dialogue",
    "multihop",
    "room_graph",
    "metric_spatial",
)


def answer(store, world, id_map, query):
    rev = {v: k for k, v in (id_map or {}).items()}
    sets = _entity_sets(world)
    text = query.get("text", "") or ""
    qtype = query.get("type")

    if qtype == "belief":
        obj = _node(text, sets["object"], id_map)
        return _place_name(store, obj, rev) if obj else ""

    if qtype == "temporal":
        as_of = query.get("as_of_ms")
        if as_of is not None:
            # "where was the X at t=..." — the belief edge live at that instant.
            obj = _node(text, sets["object"], id_map)
            return _place_name(store, obj, rev, as_of=as_of) if obj else ""
        # "when did I last <verb> the <entity>?" — the world-time of the most
        # recent episodic memory about the referenced place/object that matches
        # the action verb (a later, unrelated memory about the same place must
        # not shadow it).
        ent = _node(text, sets["object"] | sets["place"], id_map)
        if not ent:
            return ""
        mems = store.recent_about(ent)  # most-recent first
        verb = _verb_after_last(text)
        if verb:
            for m in mems:
                if verb in (m.get("content") or "").lower():
                    return str(m.get("ts_ms"))
        for m in mems:
            if m.get("ts_ms") is not None:
                return str(m["ts_ms"])
        return ""

    if qtype == "state_change":
        obj = _node(text, sets["object"], id_map)
        if not obj:
            return []
        trail = [rev.get(h["place"], h["place"]) for h in store.location_history(obj)]
        return _dedup_consecutive(trail)

    if qtype == "dialogue":
        person = _node(text, sets["person"], id_map)
        if not person:
            return []
        seen, out = set(), []
        for m in store.recent_about(person):
            c = m.get("content")
            if c and c not in seen:
                seen.add(c)
                out.append(c)
        return out

    if qtype == "multihop":
        obj = _node(text, sets["object"], id_map)
        if not obj:
            return "no"
        ts = _instruction_ts(store, obj, sets["person"], rev)
        then = store.located_in(obj, as_of_ms=ts) if ts is not None else None
        now = store.located_in(obj)
        return "yes" if then is not None and then == now else "no"

    if qtype == "room_graph":
        room = _node(text, sets["place"], id_map)
        if not room:
            return []
        out, seen = [], set()
        for nb in store.adjacent(room):
            for obj in store.objects_in(nb):
                name = rev.get(obj, obj)
                if name and name not in seen:
                    seen.add(name)
                    out.append(name)
        return out

    if qtype == "metric_spatial":
        # DOCUMENTED semantic proxy (§4.3 #7, the expected gap): the object's
        # own room. No metric coordinates exist, so true nearest-in-R^3 is out
        # of reach — we return the room (a plausible-but-coarse answer) rather
        # than special-casing toward the generator's exact object list. `run.py`
        # scores this against the true answer and *that miss is the finding*.
        obj = _node(text, sets["object"], id_map)
        if not obj:
            return ""
        room = store.located_in(obj)
        return rev.get(room, room) if room is not None else ""

    raise ValueError(f"unknown query type: {qtype!r}")


# --------------------------------------------------------------------------- #
# helpers
# --------------------------------------------------------------------------- #

def _verb_after_last(text):
    """The action word in a "when did I last <verb> the ..." query — the token
    immediately following "last". None if absent."""
    toks = re.findall(r"[a-z_]+", (text or "").lower())
    if "last" in toks:
        i = toks.index("last")
        if i + 1 < len(toks):
            return toks[i + 1]
    return None


def _entity_sets(world):
    def ids(coll):
        out = set()
        for x in coll or []:
            out.add(x if isinstance(x, str) else (x.get("id") if isinstance(x, dict) else x))
        return out

    return {
        "place": ids(getattr(world, "places", None)),
        "object": ids(getattr(world, "objects", None)),
        "person": ids(getattr(world, "people", None)),
    }


def _node(text, candidates, id_map):
    """The world id from `candidates` that appears in `text` (longest match
    first, so a specific id beats an incidental substring), mapped to its node
    id. None if nothing matches."""
    hits = sorted((c for c in candidates if c and c in text), key=len, reverse=True)
    if not hits:
        return None
    return (id_map or {}).get(hits[0])


def _place_name(store, obj_node, rev, as_of=None):
    place = store.located_in(obj_node, as_of_ms=as_of)
    return rev.get(place, place) if place is not None else ""


def _dedup_consecutive(seq):
    out = []
    for x in seq:
        if not out or out[-1] != x:
            out.append(x)
    return out


def _instruction_ts(store, obj_node, person_ids, rev):
    """The world-time of the most recent instruction about `obj_node` — the
    memory that is `about` both the object and a person carries `ts_ms`."""
    for m in store.recent_about(obj_node):
        about = m.get("about") or []
        if any(rev.get(a, a) in person_ids for a in about):
            return m.get("ts_ms")
    return None
