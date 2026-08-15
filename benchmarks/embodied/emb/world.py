"""Deterministic seeded world + episodic-stream generator.

Pure Python, no external deps, no TopoDB. Builds a synthetic embodied world
(places with a symmetric adjacency floor-plan graph, objects, people) and a
time-ordered event stream, then emits queries whose ground-truth answers are
computed directly from that stream. See
``docs/superpowers/specs/2026-08-14-embodied-agent-memory-spike-design.md``.

Determinism: all randomness flows from ``random.Random(seed)``; there is no
wall-clock, no module-global ``random`` use, and no other entropy. Therefore
``generate_world(seed) == generate_world(seed)`` for any ``seed``.
"""

from __future__ import annotations

import random
from dataclasses import dataclass


# The seven query types the spike measures (design §4.3). Every generated world
# covers all of them.
QUERY_TYPES = (
    "belief",
    "temporal",
    "state_change",
    "dialogue",
    "multihop",
    "room_graph",
    "metric_spatial",
)

_PLACE_POOL = [
    "kitchen",
    "living_room",
    "bedroom",
    "bathroom",
    "hallway",
    "office",
    "garage",
    "dining_room",
    "garden",
    "basement",
    "attic",
    "laundry_room",
]

_OBJECT_POOL = [
    "blue_mug",
    "red_book",
    "laptop",
    "umbrella",
    "keys",
    "phone_charger",
    "water_bottle",
    "backpack",
    "toolbox",
    "remote",
    "picture_frame",
    "scissors",
    "notebook",
    "coffee_pot",
    "houseplant",
    "slippers",
]

_PERSON_POOL = ["alice", "bob", "carol", "dave"]

# (template, uses_place) — every instruction references exactly one object and
# optionally a place, so dialogue + multihop always have computable ground-truth.
_INSTRUCTION_TEMPLATES = [
    ("please bring me the {object}", False),
    ("put the {object} in the {place}", True),
    ("check whether the {object} is in the {place}", True),
    ("fetch the {object} from the {place}", True),
    ("remember where the {object} is", False),
]

_ACTIONS = ["cleaned", "vacuumed", "tidied", "mopped", "dusted"]


@dataclass
class World:
    """A generated world. Two Worlds are equal iff every field is equal."""

    places: list      # [{"id": str, "adjacent": [str, ...]}]
    objects: list     # [{"id": str}]
    people: list      # [{"id": str}]
    events: list      # time-ordered Event dicts (see module docstring)
    queries: list     # Query dicts with generator-computed ground-truth


def generate_world(seed: int) -> World:
    """Build a deterministic World from ``seed``."""
    rng = random.Random(seed)

    # --- places + symmetric adjacency (a connected floor-plan graph) --------
    n_places = rng.randint(6, 8)
    place_ids = rng.sample(_PLACE_POOL, n_places)

    adjacency: dict[str, set[str]] = {p: set() for p in place_ids}
    # Spanning tree over a shuffled order guarantees connectivity.
    order = place_ids[:]
    rng.shuffle(order)
    for i in range(1, len(order)):
        j = rng.randint(0, i - 1)
        a, b = order[i], order[j]
        adjacency[a].add(b)
        adjacency[b].add(a)
    # A few extra edges so rooms have several neighbours (non-trivial traversal).
    for _ in range(rng.randint(1, 3)):
        a, b = rng.sample(place_ids, 2)
        adjacency[a].add(b)
        adjacency[b].add(a)

    places = [{"id": p, "adjacent": sorted(adjacency[p])} for p in place_ids]

    # --- objects + people ---------------------------------------------------
    n_objects = rng.randint(10, 14)
    object_ids = rng.sample(_OBJECT_POOL, n_objects)
    n_people = rng.randint(2, 3)
    people_ids = rng.sample(_PERSON_POOL, n_people)

    # --- state trackers -----------------------------------------------------
    events: list = []
    # object -> ordered [{"t_ms", "place"}], append-only location history.
    history: dict[str, list] = {}
    instructions: list = []  # {"t_ms", "person", "text", "refs", "object"}
    acts: list = []          # {"t_ms", "place", "action"}

    clock = {"t": rng.randint(500, 2000)}

    def tick() -> int:
        clock["t"] += rng.randint(500, 5000)
        return clock["t"]

    def loc_now(obj: str) -> str:
        return history[obj][-1]["place"]

    def loc_as_of(obj: str, t_ms: int):
        found = None
        for entry in history[obj]:
            if entry["t_ms"] <= t_ms:
                found = entry["place"]
            else:
                break
        return found

    # --- initial observation of every object -------------------------------
    for obj in object_ids:
        place = rng.choice(place_ids)
        t = tick()
        events.append({"op": "observe", "t_ms": t, "object": obj, "place": place})
        history[obj] = [{"t_ms": t, "place": place}]

    # --- interleaved timeline ----------------------------------------------
    plan = (
        ["move"] * rng.randint(8, 14)
        + ["observe"] * rng.randint(6, 10)
        + ["instruct"] * rng.randint(4, 6)
        + ["act"] * rng.randint(5, 8)
    )
    rng.shuffle(plan)

    for kind in plan:
        if kind == "move":
            obj = rng.choice(object_ids)
            current = loc_now(obj)
            candidates = [p for p in place_ids if p != current]
            dest = rng.choice(candidates)
            t = tick()
            events.append(
                {"op": "move", "t_ms": t, "object": obj, "from": current, "to": dest}
            )
            history[obj].append({"t_ms": t, "place": dest})
        elif kind == "observe":
            obj = rng.choice(object_ids)
            t = tick()
            events.append(
                {"op": "observe", "t_ms": t, "object": obj, "place": loc_now(obj)}
            )
        elif kind == "instruct":
            person = rng.choice(people_ids)
            obj = rng.choice(object_ids)
            place = rng.choice(place_ids)
            template, uses_place = rng.choice(_INSTRUCTION_TEMPLATES)
            text = template.format(object=obj, place=place)
            refs = [obj] + ([place] if uses_place else [])
            t = tick()
            events.append(
                {"op": "instruct", "t_ms": t, "person": person, "text": text, "refs": refs}
            )
            instructions.append(
                {"t_ms": t, "person": person, "text": text, "refs": refs, "object": obj}
            )
        else:  # act
            place = rng.choice(place_ids)
            action = rng.choice(_ACTIONS)
            t = tick()
            events.append({"op": "act", "t_ms": t, "place": place, "action": action})
            acts.append({"t_ms": t, "place": place, "action": action})

    # Guarantee enough distinct movers for belief/state_change/temporal/metric.
    moved = [o for o in object_ids if len(history[o]) >= 2]
    while len(moved) < 3:
        stationary = [o for o in object_ids if len(history[o]) < 2]
        if not stationary:
            break
        obj = stationary[0]
        current = loc_now(obj)
        dest = rng.choice([p for p in place_ids if p != current])
        t = tick()
        events.append(
            {"op": "move", "t_ms": t, "object": obj, "from": current, "to": dest}
        )
        history[obj].append({"t_ms": t, "place": dest})
        moved = [o for o in object_ids if len(history[o]) >= 2]

    # --- queries with generator-computed ground-truth ----------------------
    queries: list = []

    def objects_in(room: str) -> list:
        return sorted(o for o in object_ids if loc_now(o) == room)

    # 1. belief — current location of a moved object.
    for obj in moved[:2]:
        queries.append(
            {
                "type": "belief",
                "text": f"where is the {obj}?",
                "answer": loc_now(obj),
                "as_of_ms": None,
            }
        )

    # 2a. temporal — location as of a past instant (before its last move).
    hero = moved[0]
    hist = history[hero]
    i = rng.randint(0, len(hist) - 2)
    lo, hi = hist[i]["t_ms"], hist[i + 1]["t_ms"]
    as_of = rng.randint(lo, hi - 1) if hi - lo > 1 else lo
    queries.append(
        {
            "type": "temporal",
            "text": f"where was the {hero} at t={as_of}?",
            "answer": loc_as_of(hero, as_of),
            "as_of_ms": as_of,
        }
    )
    # 2b. temporal — when did I last <action> the <place>?
    if acts:
        ref = rng.choice(acts)
        last_t = max(
            a["t_ms"]
            for a in acts
            if a["place"] == ref["place"] and a["action"] == ref["action"]
        )
        queries.append(
            {
                "type": "temporal",
                "text": f"when did I last {ref['action']} the {ref['place']}?",
                "answer": str(last_t),
                "as_of_ms": None,
            }
        )

    # 3. state_change — the ordered location path of a moved object.
    subject = moved[1] if len(moved) > 1 else moved[0]
    queries.append(
        {
            "type": "state_change",
            "text": f"how did the {subject} move over time, and from where?",
            "answer": [entry["place"] for entry in history[subject]],
            "as_of_ms": None,
        }
    )

    # 4. dialogue — all instructions a person gave, in time order.
    for person in people_ids:
        texts = [ins["text"] for ins in instructions if ins["person"] == person]
        if texts:
            queries.append(
                {
                    "type": "dialogue",
                    "text": f"what did {person} ask me to do?",
                    "answer": texts,
                    "as_of_ms": None,
                }
            )

    # 5. multihop — is the object a person asked about still where it was then?
    if instructions:
        ins = instructions[rng.randint(0, len(instructions) - 1)]
        obj = ins["object"]
        then = loc_as_of(obj, ins["t_ms"])
        still = "yes" if then == loc_now(obj) else "no"
        queries.append(
            {
                "type": "multihop",
                "text": (
                    f"is the {obj} that {ins['person']} asked about "
                    "still where it was then?"
                ),
                "answer": still,
                "as_of_ms": None,
            }
        )

    # 6. room_graph — objects in rooms adjacent to a given place.
    focus = rng.choice(place_ids)
    adjacent_objs = sorted(
        o for o in object_ids if loc_now(o) in adjacency[focus]
    )
    queries.append(
        {
            "type": "room_graph",
            "text": f"which objects are in rooms adjacent to the {focus}?",
            "answer": adjacent_objs,
            "as_of_ms": None,
        }
    )

    # 7. metric_spatial — semantic proxy for "physically nearest": same room,
    #    else objects in adjacent rooms (the suspected engine gap, design §7).
    target = moved[0]
    room = loc_now(target)
    same_room = sorted(o for o in object_ids if o != target and loc_now(o) == room)
    if same_room:
        nearest = same_room
    else:
        nearest = sorted(
            o for o in object_ids if o != target and loc_now(o) in adjacency[room]
        )
    queries.append(
        {
            "type": "metric_spatial",
            "text": f"what is physically nearest the {target}?",
            "answer": nearest,
            "as_of_ms": None,
        }
    )

    return World(
        places=places,
        objects=[{"id": o} for o in object_ids],
        people=[{"id": p} for p in people_ids],
        events=events,
        queries=queries,
    )
