from lme.embed import CachedEmbedder


class FakeEmbedder:
    """Deterministic toy encoder that counts how many texts it actually embeds."""
    def __init__(self):
        self.calls: list[str] = []

    def encode(self, texts):
        self.calls.extend(texts)
        # 2-dim vector derived from length so it's deterministic and inspectable
        return [[float(len(t)), 1.0] for t in texts]


def test_cache_returns_same_vectors_and_skips_recompute(tmp_path):
    inner = FakeEmbedder()
    cached = CachedEmbedder(inner, cache_dir=tmp_path, model_tag="toy")

    v1 = cached.encode(["hello", "world"])
    assert v1 == [[5.0, 1.0], [5.0, 1.0]]
    assert inner.calls == ["hello", "world"]

    # second call: "hello" is cached, only "new" is computed
    v2 = cached.encode(["hello", "new"])
    assert v2 == [[5.0, 1.0], [3.0, 1.0]]
    assert inner.calls == ["hello", "world", "new"]  # "hello" not recomputed


def test_cache_persists_across_instances(tmp_path):
    a = CachedEmbedder(FakeEmbedder(), cache_dir=tmp_path, model_tag="toy")
    a.encode(["persisted"])
    inner_b = FakeEmbedder()
    b = CachedEmbedder(inner_b, cache_dir=tmp_path, model_tag="toy")
    b.encode(["persisted"])
    assert inner_b.calls == []  # served from disk


def test_model_tag_separates_caches(tmp_path):
    inner = FakeEmbedder()
    CachedEmbedder(inner, cache_dir=tmp_path, model_tag="toy").encode(["x"])
    inner2 = FakeEmbedder()
    CachedEmbedder(inner2, cache_dir=tmp_path, model_tag="other").encode(["x"])
    assert inner2.calls == ["x"]  # different tag -> different cache key
