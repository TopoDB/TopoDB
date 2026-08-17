"""Per-instance orchestration: checkout -> corpus -> embed -> graph -> index ->
score each leg. All network/git/model I/O is injected (workspace, encoder) so
this module is pure and testable."""
import hashlib
import os
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
