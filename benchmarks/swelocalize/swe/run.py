"""Per-instance orchestration: checkout -> corpus -> embed -> graph -> index ->
score each leg. All network/git/model I/O is injected (workspace, encoder) so
this module is pure and testable."""
import argparse
import hashlib
import json
import os
import shutil
import subprocess
from collections import OrderedDict
from ulid import ULID

from swe.corpus import iter_py_files, chunk_file
from swe.graph import build_import_graph
from swe.metrics import aggregate
from swe.store import Harness

def _scope_for(instance_id: str) -> str:
    digest = hashlib.sha256(instance_id.encode()).digest()[:16]
    return str(ULID.from_bytes(digest))

def _embed_files(files_raw, encoder, cache) -> list:
    """Turn [(rel, content)] into [(rel, content, vector)], mean-pooling chunk
    embeddings into one per-file vector (keeps nodes == files == graph nodes;
    max-pool-via-chunk-nodes is a documented future refinement).

    Two throughput levers, both load-bearing for a real multi-repo run:
      * content-hash cache: each unique file content is embedded ONCE and
        reused across instances/repos. SWE-bench-Lite's ~300 instances span
        only ~12 repos and most files are byte-identical across a repo's
        commits, so without this every instance re-embeds its whole repo
        (measured ~10 min/instance on the Intel dev Mac -> ~5 h for 30).
      * single batched encode: all uncached chunks in a repo go through ONE
        encoder() call so sentence-transformers batches them, instead of one
        small call per file."""
    hashes = [hashlib.sha256(c.encode("utf-8")).hexdigest() for (_r, c) in files_raw]
    all_chunks = []
    todo = []  # (file_index, chunk_start, chunk_count) for uncached files
    for i, (_rel, content) in enumerate(files_raw):
        if hashes[i] in cache:
            continue
        chunks = chunk_file(content) or [content or ""]
        todo.append((i, len(all_chunks), len(chunks)))
        all_chunks.extend(chunks)
    if all_chunks:
        vecs = encoder(all_chunks)  # one batched call for the whole repo
        for (i, start, n) in todo:
            seg = vecs[start:start + n]
            dim = len(seg[0])
            cache[hashes[i]] = [sum(v[j] for v in seg) / len(seg) for j in range(dim)]
    return [(rel, content, cache[hashes[i]])
            for i, (rel, content) in enumerate(files_raw)]

def build_manifest(model_tag, ks, depth, graph_weight, legs, n_instances,
                   instance_ids, reference_commits=None):
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
        # Index-once-per-repo approximation: a repo is indexed ONCE at a
        # reference checkout (the first instance's base_commit, recorded here)
        # and every instance of that repo is scored against it. Instances whose
        # own base_commit differs see slightly drifted file contents; a gold
        # file absent from the reference corpus is counted in `unretrievable`.
        "reference_strategy": "index-once-per-repo; reference = first instance base_commit",
        "reference_commits": dict(reference_commits or {}),
    }

def evaluate(instances, workspace, encoder, *, ks=(1, 3, 5, 10), depth=10,
             graph_weight=0.1, legs=("text", "vector", "hybrid", "graph"),
             db_dir, model_tag="minilm-l6-v2"):
    """Index each repo ONCE (at its first instance's base_commit) and score every
    instance of that repo against the shared store. This trades a small, disclosed
    fidelity loss (content drift between an instance's own commit and the
    reference) for an ~N-instances-per-repo speedup, since the whole text+vector
    index build is the per-instance floor."""
    os.makedirs(db_dir, exist_ok=True)
    # Group instances by repo, preserving first-seen (dataset) order.
    by_repo = OrderedDict()
    for inst in instances:
        by_repo.setdefault(inst.repo, []).append(inst)

    per_leg = {leg: [] for leg in legs}
    gold_dist = {}
    unretrievable_full = 0   # every gold file absent from the reference corpus -> guaranteed 0
    unretrievable_any = 0    # >=1 gold file absent (patch-created OR dropped by reference drift)
    embed_cache = {}         # content sha256 -> file vector, reused across repos
    reference_commits = {}   # repo -> the base_commit used as its reference index
    for repo, insts in by_repo.items():
        ref = insts[0]                       # reference instance for this repo
        reference_commits[repo] = ref.base_commit
        root = workspace(ref)                # checkout the reference commit ONCE
        files_raw = iter_py_files(root)      # [(rel, content)]
        corpus_paths = {rel for (rel, _c) in files_raw}
        graph = build_import_graph(files_raw)
        files = _embed_files(files_raw, encoder, embed_cache)
        scope = _scope_for(repo)
        db_path = os.path.join(db_dir, repo.replace("/", "_") + ".redb")
        h = Harness(db_path, model_tag=model_tag, graph_weight=graph_weight)
        id2path = h.index(scope, files, graph)   # index ONCE per repo
        for inst in insts:
            query_vec = encoder([inst.problem_statement])[0]
            gold = set(inst.gold_files)
            gold_dist[len(gold)] = gold_dist.get(len(gold), 0) + 1
            missing = gold - corpus_paths
            if missing:
                unretrievable_any += 1
                if missing == gold:
                    unretrievable_full += 1
            for leg in legs:
                retrieved = h.retrieve(scope, inst.problem_statement, query_vec,
                                       leg, depth, id2path)
                per_leg[leg].append({"retrieved": retrieved, "gold": gold})
    results = {leg: aggregate(rows, list(ks)) for leg, rows in per_leg.items()}
    manifest = build_manifest(model_tag, ks, depth, graph_weight, legs,
                              len(instances), [i.instance_id for i in instances],
                              reference_commits)
    return {"results": results, "manifest": manifest, "gold_dist": gold_dist,
            "unretrievable": {"full": unretrievable_full, "any": unretrievable_any}}

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
    print("unretrievable gold (files absent at base_commit):", out["unretrievable"])

if __name__ == "__main__":
    main()
