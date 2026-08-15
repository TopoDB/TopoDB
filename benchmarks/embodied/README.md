# Embodied memory benchmark (spike)

A small benchmark that probes TopoDB recall over an *embodied* transcript — an
agent moving through rooms, changing state, and holding dialogue — across seven
kinds of query. It is a spike: the point is to find where semantic recall alone
is enough and where it is not.

## How to run

This benchmark **reuses the existing `benchmarks/longmemeval/.venv`**. TopoDB is
already installed into that venv. **Do not create a second venv and do not
rebuild** (no `maturin`, no fresh `pip install topodb`).

Run the tests:

```
cd benchmarks/embodied && PYTHONPATH=. ../longmemeval/.venv/bin/python -m pytest tests/ -q -p no:cacheprovider
```

Run the benchmark:

```
cd benchmarks/embodied && PYTHONPATH=. ../longmemeval/.venv/bin/python -m emb.run
```

`PYTHONPATH=.` makes the local `emb` package importable, and pointing at
`../longmemeval/.venv/bin/python` borrows the sibling benchmark's environment so
there is nothing extra to install.

## Query taxonomy

Every query is tagged with exactly one of seven types:

| Type | What it asks |
|----------------|--------------|
| `belief` | What the agent believes/knows at a point in time |
| `temporal` | Ordering and timing of events ("before/after", "when") |
| `state_change` | How some fact changed over the transcript |
| `dialogue` | What was said in conversation |
| `multihop` | Answers that require chaining several facts together |
| `room_graph` | Connectivity of the room graph (what connects to what) |
| `metric_spatial` | Distance/direction/layout — quantitative spatial questions |

## The expected gap

`metric_spatial` is answered with a **same/adjacent-room semantic proxy**: rather
than reasoning over real coordinates or path distances, it approximates spatial
relations by whether two things are in the same room or in adjacent rooms.

This proxy is **the expected gap, and it is the finding of the spike**. Semantic
recall handles the other six types well, but true metric-spatial reasoning
(distances, directions, geometric layout) is not something a same/adjacent-room
approximation can capture. The benchmark surfaces that limit rather than hiding
it.
