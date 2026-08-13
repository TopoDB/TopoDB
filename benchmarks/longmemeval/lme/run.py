"""Orchestrate a LongMemEval-S recall run and write a results JSON."""
import argparse
import json
import tempfile
from dataclasses import dataclass
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


def run(cfg: RunConfig, embedder) -> dict:
    questions = data.load(cfg.data_path)
    if cfg.limit is not None:
        questions = questions[: cfg.limit]

    # scores[(granularity, leg)] = list[QScore]
    scores: dict[tuple[str, str], list[QScore]] = {
        (g, leg): [] for g in cfg.granularities for leg in cfg.legs
    }

    with tempfile.TemporaryDirectory() as tmp:
        harness = Harness(str(Path(tmp) / "lme.redb"), model_tag=cfg.model_tag)
        for gi, granularity in enumerate(cfg.granularities):
            for qi, q in enumerate(questions):
                scope = scope_for_index(gi * 100000 + qi)  # unique per (granularity, question)
                pairs = data.memory_texts(q, granularity)
                vectors = embedder.encode([c for _, c in pairs])
                mems = [(sid, c, v) for (sid, c), v in zip(pairs, vectors)]
                id2session = harness.ingest(scope, mems)
                qvec = embedder.encode([q.text])[0]
                for leg in cfg.legs:
                    ranked = harness.retrieve(
                        scope, q.text, qvec, leg, RETRIEVE_DEPTH, id2session
                    )
                    scores[(granularity, leg)].append(
                        QScore(q.type, q.is_abstention, q.answer_session_ids, ranked)
                    )

    results = {
        (f"{g}:{leg}"): aggregate(scores[(g, leg)], cfg.ks)
        for g in cfg.granularities for leg in cfg.legs
    }
    graded = sum(1 for q in questions if not q.is_abstention)
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
    args = p.parse_args(argv)

    granularities = args.granularity or ["session", "turn"]
    ks = [int(x) for x in args.k.split(",")]
    legs = args.legs.split(",")
    out = Path(args.out) if args.out else Path("results") / f"lme-{'-'.join(granularities)}.json"

    embedder = embed_mod.CachedEmbedder(
        embed_mod.MiniLMEmbedder(args.model), cache_dir=args.cache_dir, model_tag=args.model_tag
    )
    cfg = RunConfig(
        data_path=args.data, granularities=granularities, ks=ks, legs=legs,
        limit=args.limit, model_tag=args.model_tag, seed=args.seed, out=out,
    )
    result = run(cfg, embedder=embedder)
    from lme.report import render
    print(render(result))


if __name__ == "__main__":
    main()
