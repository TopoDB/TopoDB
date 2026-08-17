"""Per-instance orchestration: checkout -> corpus -> embed -> graph -> index ->
score each leg. All network/git/model I/O is injected (workspace, encoder) so
this module is pure and testable."""
import argparse
import hashlib
import json
import os
import shutil
import subprocess
from ulid import ULID

from swe.corpus import iter_py_files, chunk_file
from swe.graph import build_import_graph
from swe.metrics import aggregate
from swe.store import Harness

def _scope_for(instance_id: str) -> str:
    digest = hashlib.sha256(instance_id.encode()).digest()[:16]
    return str(ULID.from_bytes(digest))

def _file_vector(content: str, encoder) -> list:
    """Mean-pool chunk embeddings into one per-file vector. Phase-1 choice:
    keeps nodes == files == graph nodes. max-pool-via-chunk-nodes is a
    documented future refinement (README)."""
    chunks = chunk_file(content) or [content or ""]
    vecs = encoder(chunks)
    dim = len(vecs[0])
    return [sum(v[j] for v in vecs) / len(vecs) for j in range(dim)]

def build_manifest(model_tag, ks, depth, graph_weight, legs, n_instances, instance_ids):
    return {
        "model_tag": model_tag,
        "ks": list(ks),
        "depth": depth,
        "graph_weight": graph_weight,
        "legs": list(legs),
        "granularity": "file",
        "n_instances": n_instances,
        "instance_ids": list(instance_ids),
        "import_resolution": "ast best-effort; stdlib/third-party/dynamic dropped",
    }

def evaluate(instances, workspace, encoder, *, ks=(1, 3, 5, 10), depth=10,
             graph_weight=0.1, legs=("text", "vector", "hybrid", "graph"),
             db_dir, model_tag="minilm-l6-v2"):
    os.makedirs(db_dir, exist_ok=True)
    per_leg = {leg: [] for leg in legs}
    gold_dist = {}
    for inst in instances:
        root = workspace(inst)
        files_raw = iter_py_files(root)               # [(rel, content)]
        graph = build_import_graph(files_raw)
        files = [(rel, content, _file_vector(content, encoder))
                 for (rel, content) in files_raw]
        query_vec = encoder([inst.problem_statement])[0]
        scope = _scope_for(inst.instance_id)
        db_path = os.path.join(db_dir, inst.instance_id.replace("/", "_") + ".redb")
        h = Harness(db_path, model_tag=model_tag, graph_weight=graph_weight)
        id2path = h.index(scope, files, graph)
        gold = set(inst.gold_files)
        gold_dist[len(gold)] = gold_dist.get(len(gold), 0) + 1
        for leg in legs:
            retrieved = h.retrieve(scope, inst.problem_statement, query_vec,
                                   leg, depth, id2path)
            per_leg[leg].append({"retrieved": retrieved, "gold": gold})
    results = {leg: aggregate(rows, list(ks)) for leg, rows in per_leg.items()}
    manifest = build_manifest(model_tag, ks, depth, graph_weight, legs,
                              len(instances), [i.instance_id for i in instances])
    return {"results": results, "manifest": manifest, "gold_dist": gold_dist}

def parse_args(argv=None):
    p = argparse.ArgumentParser(prog="swe.run")
    p.add_argument("--limit", type=int, default=None,
                   help="evaluate only the first N instances (subset-first)")
    p.add_argument("--out", type=str, default="results/swelocalize.json")
    p.add_argument("--graph-weight", type=float, default=0.1)
    return p.parse_args(argv)

def git_workspace(cache_dir):
    os.makedirs(cache_dir, exist_ok=True)
    def workspace(inst):
        dest = os.path.join(cache_dir, inst.repo.replace("/", "_"))
        if not os.path.isdir(os.path.join(dest, ".git")):
            subprocess.run(["git", "clone", "--quiet",
                            f"https://github.com/{inst.repo}.git", dest], check=True)
        subprocess.run(["git", "-C", dest, "checkout", "--quiet", inst.base_commit],
                       check=True)
        return dest
    return workspace

def minilm_encoder():
    from sentence_transformers import SentenceTransformer
    model = SentenceTransformer("all-MiniLM-L6-v2")
    return lambda texts: [list(map(float, v))
                          for v in model.encode(texts, show_progress_bar=False)]

def main(argv=None):
    from swe.data import load_instances
    from swe.report import format_table
    ns = parse_args(argv)
    instances = load_instances()
    if ns.limit is not None:
        instances = instances[:ns.limit]
    db_dir = ".cache/dbs"
    # Clear db_dir to ensure each run starts clean (prevents double-indexing when re-running)
    shutil.rmtree(db_dir, ignore_errors=True)
    os.makedirs(db_dir, exist_ok=True)
    out = evaluate(instances,
                   workspace=git_workspace(".cache/repos"),
                   encoder=minilm_encoder(),
                   graph_weight=ns.graph_weight,
                   db_dir=db_dir)
    os.makedirs(os.path.dirname(ns.out) or ".", exist_ok=True)
    with open(ns.out, "w") as f:
        json.dump(out, f, indent=2, default=lambda o: sorted(o) if isinstance(o, set) else str(o))
    print(format_table(out["results"], out["manifest"]["ks"]))
    print("\nmanifest:", json.dumps(out["manifest"], indent=2))
    print("gold-file distribution:", out["gold_dist"])

if __name__ == "__main__":
    main()
