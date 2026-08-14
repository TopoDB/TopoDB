"""Orchestrate a LongMemEval-S recall run and write a results JSON."""
import argparse
import json
import tempfile
from dataclasses import dataclass, field
from pathlib import Path

from lme import data, embed as embed_mod
from lme.metrics import QScore, aggregate
from lme.store import Harness, scope_for_index

RETRIEVE_DEPTH = 100


@dataclass
class RunConfig:
    data_path: str
    granularities: list[str]
    ks: list[int]
    legs: list[str]
    limit: int | None
    model_tag: str
    seed: int
    out: Path
    # Which ingest passes to run. [False] = graph-off only (legacy default,
    # keeps result keys as "granularity:leg"); [False, True] = run both so a
    # single invocation yields the hybrid(graph on) - hybrid(graph off) delta.
    graph_modes: list[bool] = field(default_factory=lambda: [False])
    # RRF weight for the hybrid graph leg. 0.1 is the measured non-destructive
    # value; the engine default (0.5) demotes correct hits on dense real graphs.
    graph_weight: float = 0.1


def _split_ingest(ret):
    """`Harness.ingest` returns either `id2session` alone (legacy) or
    `(id2session, stats)` where `stats` carries the graph-pass counts
    (`entity_count`, `edge_count`, `dateless_edge_count`) plus the recorded
    `fanout_cap` / `extraction_params`. Normalize to `(id2session, stats)`."""
    if isinstance(ret, tuple) and len(ret) >= 2:
        stats = ret[1] if isinstance(ret[1], dict) else {}
        return ret[0], stats
    return ret, {}


def _result_key(g: str, leg: str, build_graph: bool, single_mode: bool) -> str:
    """Preserve the legacy "granularity:leg" key when only one graph mode runs;
    disambiguate with a graph/nograph suffix when both run for comparison."""
    if single_mode:
        return f"{g}:{leg}"
    return f"{g}:{leg}:{'graph' if build_graph else 'nograph'}"


def run(cfg: RunConfig, embedder) -> dict:
    questions = data.load(cfg.data_path)
    if cfg.limit is not None:
        questions = questions[: cfg.limit]

    graph_modes = cfg.graph_modes or [False]
    single_mode = len(graph_modes) == 1

    # scores[(granularity, leg, build_graph)] = list[QScore]
    scores: dict[tuple[str, str, bool], list[QScore]] = {
        (g, leg, gm): []
        for g in cfg.granularities for leg in cfg.legs for gm in graph_modes
    }
    # Per-mode ingest accounting so the run is self-describing.
    graph_stats: dict[bool, dict] = {
        gm: {
            "entity_count": 0,
            "edge_count": 0,
            "dateless_edge_count": 0,
            "fanout_cap": None,
            "extraction_params": None,
        }
        for gm in graph_modes
    }

    with tempfile.TemporaryDirectory() as tmp:
        harness = Harness(str(Path(tmp) / "lme.redb"), model_tag=cfg.model_tag,
                          graph_weight=cfg.graph_weight)
        for gi, granularity in enumerate(cfg.granularities):
            for mi, build_graph in enumerate(graph_modes):
                for qi, q in enumerate(questions):
                    # unique per (graph mode, granularity, question)
                    scope = scope_for_index(mi * 1000000 + gi * 100000 + qi)
                    pairs = data.memory_texts(q, granularity)
                    vectors = embedder.encode([c for _, c in pairs])
                    mems = [(sid, c, v) for (sid, c), v in zip(pairs, vectors)]
                    id2session, stats = _split_ingest(
                        harness.ingest(scope, mems, build_graph=build_graph)
                    )
                    # `Harness.ingest` reports the graph-pass counts on
                    # `graph_stats` (None when build_graph is False), so prefer
                    # that over the return value's stats.
                    stats = harness.graph_stats or stats
                    acc = graph_stats[build_graph]
                    acc["entity_count"] += stats.get("entity_count", 0)
                    acc["edge_count"] += stats.get("edge_count", 0)
                    acc["dateless_edge_count"] += stats.get("dateless_edge_count", 0)
                    cap = stats.get("fan_out_cap", stats.get("fanout_cap"))
                    if cap is not None:
                        acc["fanout_cap"] = cap
                    if stats.get("extraction_params") is not None:
                        acc["extraction_params"] = stats["extraction_params"]
                    qvec = embedder.encode([q.text])[0]
                    for leg in cfg.legs:
                        ranked = harness.retrieve(
                            scope, q.text, qvec, leg, RETRIEVE_DEPTH, id2session
                        )
                        scores[(granularity, leg, build_graph)].append(
                            QScore(q.type, q.is_abstention, q.answer_session_ids, ranked)
                        )

    results = {
        _result_key(g, leg, gm, single_mode): aggregate(scores[(g, leg, gm)], cfg.ks)
        for g in cfg.granularities for leg in cfg.legs for gm in graph_modes
    }
    graded = sum(1 for q in questions if not q.is_abstention)
    # The fan-out cap and extraction params are run-level constants owned by the
    # ingest graph pass; surface whichever a graph-on ingest reported.
    fanout_cap = next(
        (graph_stats[gm]["fanout_cap"] for gm in graph_modes
         if graph_stats[gm]["fanout_cap"] is not None),
        None,
    )
    extraction_params = next(
        (graph_stats[gm]["extraction_params"] for gm in graph_modes
         if graph_stats[gm]["extraction_params"] is not None),
        None,
    )
    out = {
        "manifest": {
            "model": embed_mod.DEFAULT_MODEL,
            "model_tag": cfg.model_tag,
            "ks": cfg.ks,
            "granularities": cfg.granularities,
            "legs": cfg.legs,
            "depth": RETRIEVE_DEPTH,
            "limit": cfg.limit,
            "seed": cfg.seed,
            "dataset_sha256": data.dataset_sha256(cfg.data_path),
            "n_questions": len(questions),
            "n_graded": graded,
            "graph": {
                "build_graph": list(graph_modes),
                "graph_weight": cfg.graph_weight,
                "fanout_cap": fanout_cap,
                "extraction_params": extraction_params,
                "entity_count": {
                    str(gm): graph_stats[gm]["entity_count"] for gm in graph_modes
                },
                "edge_count": {
                    str(gm): graph_stats[gm]["edge_count"] for gm in graph_modes
                },
                "dateless_edge_count": {
                    str(gm): graph_stats[gm]["dateless_edge_count"] for gm in graph_modes
                },
            },
        },
        "results": results,
    }
    cfg.out.parent.mkdir(parents=True, exist_ok=True)
    cfg.out.write_text(json.dumps(out, indent=2))
    return out


def main(argv=None) -> None:
    p = argparse.ArgumentParser(description="LongMemEval-S recall harness")
    p.add_argument("--data", required=True)
    p.add_argument("--granularity", action="append", choices=["session", "turn"], default=None)
    p.add_argument("--k", default="1,3,5,10")
    p.add_argument("--legs", default="text,vector,hybrid")
    p.add_argument("--limit", type=int, default=None)
    p.add_argument("--model", default=embed_mod.DEFAULT_MODEL)
    p.add_argument("--model-tag", default=embed_mod.MODEL_TAG)
    p.add_argument("--seed", type=int, default=0)
    p.add_argument("--out", default=None)
    p.add_argument("--cache-dir", default=".cache")
    graph = p.add_mutually_exclusive_group()
    graph.add_argument(
        "--graph", dest="build_graph", action="store_true",
        help="Run only the graph-on (co_mention) ingest pass.",
    )
    graph.add_argument(
        "--no-graph", dest="build_graph", action="store_false",
        help="Run only the graph-off ingest pass.",
    )
    p.set_defaults(build_graph=None)
    p.add_argument(
        "--graph-weight", type=float, default=0.1,
        help="RRF weight for the hybrid graph leg (default 0.1, the measured "
             "non-destructive value; the engine default 0.5 demotes correct "
             "hits on dense real graphs).",
    )
    args = p.parse_args(argv)

    granularities = args.granularity or ["session", "turn"]
    ks = [int(x) for x in args.k.split(",")]
    legs = args.legs.split(",")
    # Default (neither flag): run BOTH so one invocation yields the comparison.
    graph_modes = [False, True] if args.build_graph is None else [args.build_graph]
    out = Path(args.out) if args.out else Path("results") / f"lme-{'-'.join(granularities)}.json"

    embedder = embed_mod.CachedEmbedder(
        embed_mod.MiniLMEmbedder(args.model), cache_dir=args.cache_dir, model_tag=args.model_tag
    )
    cfg = RunConfig(
        data_path=args.data, granularities=granularities, ks=ks, legs=legs,
        limit=args.limit, model_tag=args.model_tag, seed=args.seed, out=out,
        graph_modes=graph_modes, graph_weight=args.graph_weight,
    )
    result = run(cfg, embedder=embedder)
    from lme.report import render
    print(render(result))


if __name__ == "__main__":
    main()
