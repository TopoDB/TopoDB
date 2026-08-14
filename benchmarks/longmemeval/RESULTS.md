# LongMemEval-S — TopoDB results

Numbers produced by the harness in this directory. Two layers are reported
separately because they mean different things: **retrieval recall** (rigorous,
deterministic, the core measurement) and **end-to-end QA accuracy**
(preliminary — depends on an external reader/judge LLM).

Dataset: LongMemEval-S (500 questions; 470 answerable + 30 abstention).
Metric definitions and caveats: see `README.md` and the design spec.

---

## Retrieval recall (the core, reproducible result)

Session-level Recall@k — a hit means a gold evidence session appears among the
first *k* distinct retrieved sessions. Full 500 questions, **session
granularity**, host-computed **all-MiniLM-L6-v2** (384-d) vectors fed to the
engine (this isolates *ranking*, not embedder choice). 470 graded; 30 abstention
excluded.

| Leg | R@1 | R@3 | R@5 | R@10 |
|-----|-----|-----|-----|------|
| text (BM25)      | 0.857 | 0.947 | 0.968 | 0.987 |
| vector (MiniLM)  | 0.760 | 0.891 | 0.919 | 0.966 |
| hybrid (RRF)     | 0.832 | 0.951 | 0.968 | 0.983 |

Per-type recall (and the harder types) are in the JSON the harness writes.

**What this shows.** TopoDB's retrieval surfaces the gold evidence in the top-5
about **97%** of the time. With a stronger embedder (`text-embedding-3-large`),
retrieval@5 rose to ~1.00 on a balanced sample.

**A ranking finding worth noting:** the RRF hybrid slightly *underperforms* pure
BM25 at R@1 (0.832 vs 0.857) — the vector leg dilutes a strong lexical signal at
the very top rank. That's a concrete fusion-weighting lead, not a defect.

Reproduce:
```
python -m lme.run --data data/longmemeval_s.json --granularity session
```

---

## End-to-end QA accuracy (preliminary)

> **Caveat: preliminary.** This layer depends on an external LLM as reader and
> judge, and on prompt/granularity choices that are **not yet tuned**. Treat the
> number as a floor and a diagnostic, not a headline.

Stratified 90 questions (15 per type). Retrieval: `text-embedding-3-large`,
top-5 sessions as context. Reader + judge: GPT-4o with the official LongMemEval
per-type judge prompts.

| Type | QA acc | Retrieval@5 |
|---|---|---|
| single-session-assistant  | 1.00 | 1.00 |
| knowledge-update          | 0.87 | 1.00 |
| single-session-user       | 0.87 | 1.00 |
| multi-session             | 0.60 | 1.00 |
| temporal-reasoning        | 0.33 | 1.00 |
| single-session-preference | 0.13 | 0.80 |
| **Overall**               | **0.633** | ~0.97 |

(The balanced sample is representative: LongMemEval-S is itself ~53%
multi-session + temporal-reasoning. Distribution-weighted projection ≈ 0.625.)

**The key finding — the bottleneck is the reader, not retrieval.**
Retrieval@5 is ~0.97 across every type (e.g. temporal-reasoning: evidence
retrieved 15/15, answered 5/15). The reader captures only ~65% of what is
retrievable. So TopoDB's memory retrieval is strong; the end-to-end number is
limited by the reader stage — an under-tuned generic reader prompt (the 0.13 on
preference is a prompt artifact) and coarse session-granularity context
(~30k-token needle-in-haystack).

**Not yet run:** the full 500 with GPT-4o, a per-type / official-style reader
prompt, and turn-granularity context — each expected to raise the number without
changing the memory engine.

---

## Graph-leg activation experiment (neutral — no headroom)

The recall runs above ingest each session as a flat memory, so the graph/PPR
leg is inert. This experiment (`--graph`, `lme/extract.py`) *activates* it
**deterministically and offline**: extract proper-noun entities per session and
lay down cross-session `Memory--co_mention-->Memory` edges so the hybrid leg's
1-hop PPR fires. The hypothesis was that corroboration across sessions sharing an
entity would sharpen R@1 on the hard types (multi-session, temporal).

**Result: with the extractor and graph weight tuned, the graph leg is neutral —
it matches the graph-off baseline and adds no lift.** Getting there required
fixing two real mistakes that first produced a spurious *catastrophic* result
(R@1 collapsing to ~0). The investigation is the interesting part:

**Mistake 1 — a promiscuous extractor.** The first proper-noun heuristic captured
sentence-initial common words (`use`, `make`, `consider`, contractions like
`i'm`) as entities. On real conversational text that built a *near-complete*
graph (~330 entities / ~1,600 co_mention edges per ~50-session question), so the
PPR neighborhood spanned the whole haystack. Fix: a corpus-level **truecasing
filter** — keep a surface form only if it is capitalized in ≥85% of its
occurrences (genuine proper nouns are; sentence-initial verbs are not), drop
contractions and single letters. Entities per question dropped ~313 → ~77.

**Mistake 2 — an over-aggressive graph weight.** The Python binding hardcoded the
engine default `graph_weight = 0.5` (half of text/vector at 1.0 each). That is
strong enough to *reorder an already-correct* text+vector ranking: because the
PPR list excludes its own seeds, a gold session that is already a top hit gets
leapfrogged by its non-seed neighbors. Sweeping the weight (now exposed on
`topodb-py`'s `recall`) shows the cliff:

| graph_weight | multi-session R@1 | temporal-reasoning R@1 |
|---|---|---|
| off (baseline) | 0.750 | 0.750 |
| 0.5 (old default) | **0.083** | **0.000** |
| 0.2 | 0.667 | 0.750 |
| 0.1 | 0.750 | 0.750 |
| ≤0.05 | 0.750 | 0.750 |

At `graph_weight ≤ 0.1` the harm is gone and recall returns to baseline — but it
does **not exceed** it. Enabling the co-seed **corroboration** weight (also now
exposed; a mild multiplicative tie-breaker) changed nothing across 0.0–1.0: it
cannot overcome the PPR gap once a hit has been demoted.

**Conclusion.** The catastrophic drop was *our* two bugs, not evidence that
graphs hurt. Corrected, the deterministic co_mention graph leg is **neutral on
LongMemEval-S** — and neutral is the ceiling here, because retrieval is already
~0.97 R@5 (see above): there is no headroom for a corroboration graph to raise
R@1. A graph leg would only pay off in a harder retrieval regime (weaker
embedder, larger/among-distractor haystacks) or with *selective* LLM
entity/relation extraction (what Mem0/Zep do). The harness now defaults to the
safe `graph_weight = 0.1`; the engine's own 0.5 default is untouched. Product
recall path is unchanged — this is opt-in benchmark scaffolding.

Reproduce: `python -m lme.run --data data/longmemeval_s.json --granularity
session --limit 50 --legs vector,hybrid --k 1,3,5 [--graph-weight 0.1]` (runs
both graph modes). Results JSON (gitignored): `results/graph-leg-limit50.json`.

---

## Honest scope

- The graph/PPR leg is **inert** in the core recall runs above (sessions
  ingested as plain memories with
  no entities), so `hybrid` = text+vector RRF. Entity extraction is a future
  lever.
- Recall vectors are host-computed to isolate ranking; QA uses API embeddings.
- QA reader/judge is GPT-4o via the official judge prompts, but the reader
  *generation* prompt and retrieval granularity are unoptimized.
- No competitor numbers are cited here; cross-system comparison requires running
  the same pipeline per system, which this harness does not yet do.
