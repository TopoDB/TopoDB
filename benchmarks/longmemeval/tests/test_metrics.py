from lme.metrics import recall_at_k, coverage_at_k, QScore, aggregate


def test_recall_hit_within_k_after_dedup():
    # distinct sessions in first-seen order: s1, s2, s3
    ranked = ["s1", "s1", "s2", "s3"]
    assert recall_at_k(ranked, {"s3"}, k=3) == 1.0   # s3 is 3rd distinct
    assert recall_at_k(ranked, {"s3"}, k=2) == 0.0   # only s1,s2 in top-2 distinct


def test_recall_miss_is_zero():
    assert recall_at_k(["s1", "s2"], {"s9"}, k=5) == 0.0


def test_coverage_is_fraction_of_gold_found():
    ranked = ["s1", "s2", "s3"]
    assert coverage_at_k(ranked, {"s1", "s3"}, k=3) == 1.0
    assert coverage_at_k(ranked, {"s1", "s9"}, k=3) == 0.5


def test_aggregate_excludes_abstention_and_breaks_down_by_type():
    scores = [
        QScore("multi-session", False, {"s1"}, ["s1"]),        # hit@1
        QScore("multi-session", False, {"s2"}, ["s9", "s2"]),  # hit@3 not @1
        QScore("temporal", False, {"s5"}, ["s9"]),             # miss
        QScore("abstention", True, set(), ["s9"]),             # excluded
    ]
    agg = aggregate(scores, ks=[1, 3])
    assert agg["n"] == 3
    assert agg["n_abstention"] == 1
    assert agg["overall"]["recall@1"] == 1 / 3
    assert agg["overall"]["recall@3"] == 2 / 3
    assert agg["per_type"]["multi-session"]["recall@1"] == 0.5
    assert agg["per_type"]["multi-session"]["recall@3"] == 1.0
    assert agg["per_type"]["temporal"]["recall@3"] == 0.0
