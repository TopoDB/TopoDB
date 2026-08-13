import tempfile
from pathlib import Path
from lme.run import run, RunConfig

FIX = Path(__file__).parent / "fixtures" / "tiny_longmemeval.json"


class FakeEmbedder:
    """Maps a few keywords to 2-dim vectors so retrieval is predictable."""
    def encode(self, texts):
        out = []
        for t in texts:
            low = t.lower()
            if "rex" in low or "dog" in low:
                out.append([1.0, 0.0])
            elif "hiking" in low:
                out.append([0.0, 1.0])
            else:
                out.append([0.5, 0.5])
        return out


def test_run_produces_manifest_and_recall_for_each_config():
    with tempfile.TemporaryDirectory() as d:
        cfg = RunConfig(
            data_path=str(FIX),
            granularities=["session"],
            ks=[1, 3],
            legs=["text", "vector", "hybrid"],
            limit=None,
            model_tag="toy",
            seed=0,
            out=Path(d) / "out.json",
        )
        results = run(cfg, embedder=FakeEmbedder())

        m = results["manifest"]
        assert m["model_tag"] == "toy"
        assert m["ks"] == [1, 3]
        assert m["granularities"] == ["session"]
        assert len(m["dataset_sha256"]) == 64
        assert m["n_questions"] == 2
        assert m["n_graded"] == 1  # q2_abs excluded

        # one block per granularity:leg
        assert set(results["results"].keys()) == {
            "session:text", "session:vector", "session:hybrid",
        }
        # q1's gold session sess_a is recoverable -> recall@3 == 1.0 on the vector leg
        assert results["results"]["session:vector"]["overall"]["recall@3"] == 1.0
        # results file was written
        assert (Path(d) / "out.json").exists()
