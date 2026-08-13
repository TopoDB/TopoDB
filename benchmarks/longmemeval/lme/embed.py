"""Embedding with a persistent per-text cache and an injectable encoder."""
import hashlib
import json
from pathlib import Path
from typing import Protocol

DEFAULT_MODEL = "sentence-transformers/all-MiniLM-L6-v2"
MODEL_TAG = "minilm-l6-v2"


class Embedder(Protocol):
    def encode(self, texts: list[str]) -> list[list[float]]: ...


class MiniLMEmbedder:
    """Wraps sentence-transformers; imported lazily so tests need no model."""
    def __init__(self, model_name: str = DEFAULT_MODEL):
        from sentence_transformers import SentenceTransformer
        self._model = SentenceTransformer(model_name)

    def encode(self, texts: list[str]) -> list[list[float]]:
        vecs = self._model.encode(texts, normalize_embeddings=True)
        return [[float(x) for x in row] for row in vecs]


class CachedEmbedder:
    """Caches vectors on disk keyed by sha256(model_tag \\x00 text)."""
    def __init__(self, inner: Embedder, cache_dir, model_tag: str = MODEL_TAG):
        self._inner = inner
        self._dir = Path(cache_dir)
        self._dir.mkdir(parents=True, exist_ok=True)
        self._tag = model_tag

    def _path(self, text: str) -> Path:
        h = hashlib.sha256(f"{self._tag}\x00{text}".encode()).hexdigest()
        return self._dir / f"{h}.json"

    def encode(self, texts: list[str]) -> list[list[float]]:
        result: list[list[float] | None] = [None] * len(texts)
        missing_idx: list[int] = []
        missing_txt: list[str] = []
        for i, t in enumerate(texts):
            p = self._path(t)
            if p.exists():
                result[i] = json.loads(p.read_text())
            else:
                missing_idx.append(i)
                missing_txt.append(t)
        if missing_txt:
            computed = self._inner.encode(missing_txt)
            for i, t, v in zip(missing_idx, missing_txt, computed):
                self._path(t).write_text(json.dumps(v))
                result[i] = v
        return [r for r in result]  # type: ignore[misc]
