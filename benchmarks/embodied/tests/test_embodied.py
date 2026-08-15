"""End-to-end contract test for the embodied-agent memory spike.

Exercises the four spike modules together, offline and deterministically
(design ``docs/superpowers/specs/2026-08-14-embodied-agent-memory-spike-design.md``):

  * ``emb.world``   -- seeded synthetic world + ground-truth queries
  * ``emb.store``   -- thin TopoDB adapter (edges_from / traverse / search)
  * ``emb.ingest``  -- embodied event stream -> entities + episodic memories
                        + bi-temporal ``located_in`` / ``adjacent`` / ``about`` edges
  * ``emb.queries`` -- engine-only answerers for the 7 query types

The test mirrors ``emb.run``'s harness: build one seeded world, ingest it into an
``EmbodiedStore`` over a tempfile db, then assert against the generator's own
ground-truth. No network, no LLM, no wall-clock -- everything flows from the
fixed seed, so the whole test is reproducible run to run.
"""

import os
import sys

import pytest

# Make ``import emb.*`` work regardless of the directory pytest is invoked from:
# the package root is ``benchmarks/embodied`` (the parent of this tests/ dir).
_HERE = os.path.dirname(os.path.abspath(__file__))
_PKG_ROOT = os.path.dirname(_HERE)
if _PKG_ROOT not in sys.path:
    sys.path.insert(0, _PKG_ROOT)

import emb.world  # noqa: E402
import emb.store  # noqa: E402
import emb.ingest  # noqa: E402
import emb.queries  # noqa: E402

# Fixed seed keeps world + scope derivation reproducible (design section 4.4).
SEED = 42


# --------------------------------------------------------------------------- #
# Scoring + world-state helpers (kept self-contained so the test pins the
# contract rather than importing private harness internals).
# --------------------------------------------------------------------------- #

def _score(got, expected) -> bool:
    """Match semantics identical to ``emb.run._score``: exact for scalar (str)
    answers, set-equality for list answers."""
    if isinstance(expected, list):
        got_list = got if isinstance(got, list) else ([] if got is None else [got])
        return set(got_list) == set(expected)
    return got == expected


def _world_state(world):
    """Replay the event stream to recover each object's current room and the
    place-adjacency map -- the ground-truth spatial state used to judge the
    (deliberately fuzzy) metric_spatial answer."""
    loc = {}
    for ev in world.events:
        op = ev.get("op")
        if op == "observe":
            loc[ev["object"]] = ev["place"]
        elif op == "move":
            loc[ev["object"]] = ev["to"]
    adj = {p["id"]: set(p["adjacent"]) for p in world.places}
    return loc, adj


def _node_key(node):
    """Normalise a traverse/search node record to its engine node id."""
    if isinstance(node, dict):
        for k in ("id", "node", "nid"):
            if k in node and node[k] is not None:
                return node[k]
        return None
    return node


# --------------------------------------------------------------------------- #
# Fixture: one ingested world shared across the assertions below.
# --------------------------------------------------------------------------- #

@pytest.fixture(scope="module")
def ingested(tmp_path_factory):
    world = emb.world.generate_world(SEED)
    db_path = str(tmp_path_factory.mktemp("emb-spike") / "emb.redb")
    store = emb.store.EmbodiedStore(db_path)
    store.open()
    id_map = emb.ingest.ingest_world(store, world)
    return world, store, id_map


# --------------------------------------------------------------------------- #
# (a) Determinism of the generator.
# --------------------------------------------------------------------------- #

def test_generate_world_is_deterministic():
    w1 = emb.world.generate_world(SEED)
    w2 = emb.world.generate_world(SEED)
    assert w1 == w2
    # Sanity: the fixed seed actually produced a non-trivial world.
    assert w1.places and w1.objects and w1.events and w1.queries


# --------------------------------------------------------------------------- #
# (b) Ingest builds the pinned edge types and bi-temporal supersession.
# --------------------------------------------------------------------------- #

def test_ingest_builds_located_in_adjacent_and_about_edges(ingested):
    world, store, id_map = ingested

    # -- located_in + bi-temporal move: a moved object ends with exactly one
    #    OPEN located_in edge and at least one CLOSED (superseded) one. --------
    moved_objs = [ev["object"] for ev in world.events if ev.get("op") == "move"]
    assert moved_objs, "fixture world should contain at least one move"

    obj = moved_objs[0]
    obj_node = id_map[obj]
    locs = store.edges_from(obj_node, type="located_in")
    assert locs, "a moved object must have located_in edges"

    open_locs = [e for e in locs if e.get("valid_to") is None]
    closed_locs = [e for e in locs if e.get("valid_to") is not None]
    assert len(open_locs) == 1, "exactly one located_in edge stays open (current belief)"
    assert closed_locs, "a move must CLOSE the prior located_in edge (supersession)"

    # Bi-temporal fields are present and coherent.
    assert open_locs[0].get("valid_from") is not None
    for e in closed_locs:
        assert e.get("valid_from") is not None
        assert e.get("valid_to") is not None

    # -- adjacent: place<->place edges exist, and a traversal over them recovers
    #    a room's known neighbours (direction-agnostic graph reachability). ----
    total_adjacent = sum(
        len(store.edges_from(id_map[p["id"]], type="adjacent")) for p in world.places
    )
    assert total_adjacent > 0, "ingest must create adjacent edges"

    place = next(p for p in world.places if p["adjacent"])
    rev = {nid: wid for wid, nid in id_map.items()}
    reached = {
        rev[k]
        for k in (_node_key(n) for n in store.traverse(
            [id_map[place["id"]]], 1, edge_types=["adjacent"]
        ))
        if k in rev
    }
    for neighbour in place["adjacent"]:
        assert neighbour in reached, (
            f"{neighbour} adjacent to {place['id']} should be reachable via traverse"
        )

    # -- about: episodic memories carry an about edge to an entity. -----------
    hits = []
    for term in ("Saw", world.objects[0]["id"]):
        hits = store.search(term, 10)
        if hits:
            break
    assert hits, "text search should surface episodic memories"

    about_seen = False
    for hit in hits:
        mem_node = hit["node"] if isinstance(hit, dict) else hit
        if store.edges_from(mem_node, type="about"):
            about_seen = True
            break
    assert about_seen, "episodic memories must carry about edges (memory -> entity)"


# --------------------------------------------------------------------------- #
# (c) Each of the 7 query types answers ground-truth -- except metric_spatial,
#     which is only required to RUN and return a plausible same/adjacent room.
# --------------------------------------------------------------------------- #

def test_query_types_answer_ground_truth(ingested):
    world, store, id_map = ingested

    by_type = {}
    for q in world.queries:
        by_type.setdefault(q["type"], []).append(q)

    # The fixture exercises the full taxonomy.
    assert set(by_type) == set(emb.world.QUERY_TYPES)

    # The six graph-answerable types must match the generator ground-truth.
    for q in world.queries:
        if q["type"] == "metric_spatial":
            continue
        got = emb.queries.answer(store, world, id_map, q)
        assert _score(got, q["answer"]), (
            f"{q['type']} query {q['text']!r} -> {got!r}, expected {q['answer']!r}"
        )

    # metric_spatial is the documented semantic-spatial gap (design section 7):
    # assert only that it RUNS and returns a plausible room (the target's own
    # room or one adjacent to it) -- NOT that it nails the exact answer.
    current_loc, adjacency = _world_state(world)
    for q in by_type["metric_spatial"]:
        got = emb.queries.answer(store, world, id_map, q)
        target = next(o["id"] for o in world.objects if o["id"] in q["text"])
        room = current_loc[target]
        plausible = {room} | adjacency.get(room, set())
        assert got in plausible, (
            f"metric_spatial -> {got!r} is not the room of {target} nor adjacent "
            f"to it (room={room}, plausible={sorted(plausible)})"
        )
