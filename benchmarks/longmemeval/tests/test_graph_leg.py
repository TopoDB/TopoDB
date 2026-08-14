"""End-to-end proof that the co_mention graph leg is NON-INERT (spec §10, §12).

These tests are deterministic and offline: fixed toy embedding vectors, the
'toy' model tag, no dataset or model download. They construct the multi-session
pattern the graph leg is meant to help — a gold session buried by pure vector
similarity but corroborated (via a shared proper-noun entity) by the sessions
that DO rank highly — and assert the graph leg lifts it into R@1.

Mechanism the fixtures rely on (see crates/topodb/src/recall.rs):
  * The graph leg materializes the 1-hop neighborhood of the top-GRAPH_SEEDS(=5)
    preliminary hits and folds it into the RRF fusion at graph_weight=0.5,
    EXCLUDING the seeds themselves. So the session the graph boosts must be a
    NON-seed (ranked outside the top 5 by the prelim legs) that many seeds
    converge on.
  * The query TEXT is chosen to match nothing ('xyzzy'), so the BM25 text leg is
    inert and the comparison isolates vector vs vector+graph.
  * The shared entity must be a CAPITALIZED token appearing MID-sentence (not
    sentence-initial) in >=2 sessions, or lme.extract's proper-noun heuristic +
    sentence-initial artifact filter will not treat it as a shared entity.
"""
import math
import tempfile
from pathlib import Path

from lme.store import Harness, scope_for_index


def _unit(v):
    n = math.sqrt(sum(x * x for x in v))
    return [x / n for x in v]


def _harness():
    d = tempfile.mkdtemp()
    return Harness(str(Path(d) / "lme.redb"), model_tag="toy")


def test_co_mention_edge_created_between_shared_entity_sessions():
    """build_graph=True creates an Entity for a cross-session proper noun and a
    load-bearing Memory--co_mention-->Memory edge between the two sessions."""
    h = _harness()
    scope = scope_for_index(0)
    # 'Rex' capitalized and mid-sentence in BOTH sessions -> a shared entity.
    mems = [
        ("sess_a", "I adopted Rex last spring", [1.0, 0.0]),
        ("sess_b", "my dog Rex loves the park", [0.9, 0.1]),
    ]
    id2session = h.ingest(scope, mems, build_graph=True)

    assert h.graph_stats is not None
    assert h.graph_stats["entity_count"] >= 1, "shared proper noun should yield an entity"
    assert h.graph_stats["edge_count"] >= 1

    # A co_mention edge is canonical (src<dst by creation order), so it shows on
    # exactly one endpoint's outgoing set; count across both to be direction-agnostic.
    mem_ids = list(id2session)
    co_mention_total = sum(
        len(h._db.edges_from([scope], mid, type="co_mention")) for mid in mem_ids
    )
    assert co_mention_total >= 1, "expected a co_mention edge between the two sessions"


def test_graph_leg_lifts_buried_gold_into_r1_where_vector_does_not():
    """The non-inert proof: a gold session ranked #6 by pure vector similarity is
    pulled to R@1 by the graph leg, because five higher-ranked sessions all share
    an entity with it and the PPR neighborhood injects it into the fusion."""
    q = _unit([1.0, 0.0, 0.0])
    # Five seeds (top-5 vector) all mention 'Zorp' mid-sentence -> all connect to
    # the gold; the gold 'B' has low vector similarity (rank 6, a NON-seed) but is
    # the convergence node of the seeds' 1-hop neighborhood.
    mems = [
        ("S1", "the Zorp project shipped on time", _unit([1.00, 0.10, 0.0])),
        ("S2", "our Zorp review went really well", _unit([0.98, 0.17, 0.0])),
        ("S3", "everyone praised Zorp in standup", _unit([0.96, 0.22, 0.0])),
        ("S4", "we scoped Zorp for next quarter", _unit([0.94, 0.28, 0.0])),
        ("S5", "the Zorp budget was approved fast", _unit([0.92, 0.33, 0.0])),
        ("B", "honestly Zorp changed my whole workflow", _unit([0.55, 0.83, 0.0])),
        ("C", "unrelated lunch plans for friday", _unit([0.1, 0.2, 0.97])),
    ]
    gold = "B"

    def top1(build_graph, leg):
        h = _harness()
        scope = scope_for_index(0)
        id2 = h.ingest(scope, mems, build_graph=build_graph)
        ranked = h.retrieve(scope, "xyzzy", q, leg, 10, id2)
        return ranked[0], h.graph_stats

    # Pure vector misses: gold is not rank 1.
    vec_r1, _ = top1(build_graph=False, leg="vector")
    assert vec_r1 != gold

    # Hybrid WITHOUT the graph also misses (text leg is inert here, so hybrid
    # degrades to vector) — isolates that the lift below is the graph, not text.
    nograph_r1, _ = top1(build_graph=False, leg="hybrid")
    assert nograph_r1 != gold

    # Hybrid WITH the graph lifts the buried gold to R@1.
    graph_r1, stats = top1(build_graph=True, leg="hybrid")
    assert stats["entity_count"] >= 1 and stats["edge_count"] >= 1
    assert graph_r1 == gold, f"graph leg should lift {gold} to R@1, got {graph_r1}"


def test_no_shared_entity_is_a_noop():
    """With no cross-session entity, the graph pass builds nothing and graph-on
    ranking is identical to graph-off — the leg is correctly inert, not harmful."""
    q = _unit([1.0, 0.0])
    # Distinct proper nouns per session, none shared -> no co_mention edges.
    mems = [
        ("s1", "the Apollo report is ready", _unit([1.0, 0.2])),
        ("s2", "we love Brussels in spring", _unit([0.6, 0.9])),
        ("s3", "Cairo traffic was terrible", _unit([0.2, 1.0])),
    ]

    def ranking(build_graph):
        h = _harness()
        scope = scope_for_index(0)
        id2 = h.ingest(scope, mems, build_graph=build_graph)
        return h.retrieve(scope, "xyzzy", q, "hybrid", 10, id2), h.graph_stats

    off_rank, _ = ranking(False)
    on_rank, on_stats = ranking(True)

    assert on_stats["entity_count"] == 0
    assert on_stats["edge_count"] == 0
    assert on_rank == off_rank, "no shared entity => graph-on must equal graph-off"
