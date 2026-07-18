//! Golden-set retrieval-quality gate. Loads the committed corpus
//! (precomputed embeddings — no model needed here), builds a fresh db,
//! and asserts per-query expectations plus aggregate MRR across four
//! configurations. EVERY future ranking change must keep this green.
use std::collections::HashMap;
use topodb::*;

#[derive(serde::Deserialize)]
struct Corpus {
    model: String,
    nodes: Vec<CNode>,
    synonyms: Vec<CSyn>,
    queries: Vec<CQuery>,
}
#[derive(serde::Deserialize)]
struct CNode {
    key: String,
    label: String,
    text: String,
    links: Vec<String>,
    vector: Vec<f32>,
}
#[derive(serde::Deserialize)]
struct CSyn {
    term: String,
    expansion: String,
}
#[derive(serde::Deserialize)]
struct CQuery {
    query: String,
    expect_top3: Vec<String>,
    vector: Vec<f32>,
}

fn load() -> Corpus {
    let p =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/recall-corpus.json");
    serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap()
}

struct Built {
    db: Db,
    scope: ScopeId,
    ids: HashMap<String, NodeId>,
    corpus: Corpus,
    // Held for its Drop impl only (removes the backing directory when the
    // last `Built` using it goes out of scope) — never read directly. Must
    // outlive `db`, which holds an open handle into this directory.
    _dir: tempfile::TempDir,
}

fn build() -> Built {
    let corpus = load();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("q.redb");
    let spec = IndexSpec {
        equality: vec![],
        text: vec![
            PropIndex {
                label: "Memory".into(),
                prop: "content".into(),
            },
            PropIndex {
                label: "Entity".into(),
                prop: "name".into(),
            },
            PropIndex {
                label: "Alias".into(),
                prop: "name".into(),
            },
        ],
    };
    let db = Db::open_with(path, spec).unwrap();
    let scope = ScopeId::new();
    let mut ids: HashMap<String, NodeId> = HashMap::new();
    for n in &corpus.nodes {
        let id = NodeId::new();
        let mut props = Props::new();
        let prop = if n.label == "Memory" {
            "content"
        } else {
            "name"
        };
        props.insert(prop.into(), PropValue::Str(n.text.clone()));
        db.submit(vec![Op::CreateNode {
            id,
            scope: Scope::Id(scope),
            label: n.label.clone().into(),
            props,
        }])
        .unwrap();
        db.submit(vec![Op::SetEmbedding {
            id,
            model: corpus.model.clone(),
            vector: n.vector.clone(),
        }])
        .unwrap();
        ids.insert(n.key.clone(), id);
    }
    for n in &corpus.nodes {
        for target in &n.links {
            db.submit(vec![Op::CreateEdge {
                id: EdgeId::new(),
                scope: Scope::Id(scope),
                ty: "about".into(),
                from: ids[&n.key],
                to: ids[target],
                props: Props::new(),
                valid_from: None,
            }])
            .unwrap();
        }
    }
    Built {
        db,
        scope,
        ids,
        corpus,
        _dir: dir,
    }
}

#[derive(Clone, Copy)]
struct Config {
    vector: bool,
    graph: bool,
    synonyms: bool,
}

fn run_config(b: &Built, cfg: Config) -> (f64 /*MRR*/, Vec<String> /*per-query misses*/) {
    let scopes = ScopeSet::of(&[b.scope]);
    let mut rr_sum = 0.0;
    let mut misses = Vec::new();
    for q in &b.corpus.queries {
        let expansions = if cfg.synonyms {
            q.query
                .split_whitespace()
                .filter_map(|w| {
                    let terms: Vec<String> = b
                        .corpus
                        .synonyms
                        .iter()
                        .filter(|s| s.term == w.to_lowercase())
                        .map(|s| s.expansion.clone())
                        .collect();
                    (!terms.is_empty()).then(|| (w.to_string(), terms))
                })
                .collect()
        } else {
            vec![]
        };
        let hits =
            b.db.recall(&RecallQuery {
                scopes: scopes.clone(),
                query: q.query.clone(),
                k: 10,
                vector: cfg
                    .vector
                    .then(|| (b.corpus.model.clone(), q.vector.clone())),
                expansions,
                graph_boost: cfg.graph,
                options: SearchOptions::default(),
            })
            .unwrap_or_default();
        let expected: Vec<NodeId> = q.expect_top3.iter().map(|k| b.ids[k]).collect();
        let rank = hits.iter().position(|(n, _)| expected.contains(&n.id));
        match rank {
            Some(r) => rr_sum += 1.0 / (r as f64 + 1.0),
            None => misses.push(q.query.clone()),
        }
        if cfg.vector && cfg.graph && cfg.synonyms {
            assert!(
                hits.iter().take(3).any(|(n, _)| expected.contains(&n.id)),
                "full hybrid must place an expected hit in top-3 for {:?}; got {:?}",
                q.query,
                hits.iter().take(3).map(|(n, _)| n.id).collect::<Vec<_>>()
            );
        }
    }
    (rr_sum / b.corpus.queries.len() as f64, misses)
}

#[test]
fn hybrid_beats_bm25_and_meets_floor() {
    let b = build();
    let bm25 = run_config(
        &b,
        Config {
            vector: false,
            graph: false,
            synonyms: false,
        },
    )
    .0;
    let with_vec = run_config(
        &b,
        Config {
            vector: true,
            graph: false,
            synonyms: false,
        },
    )
    .0;
    let with_graph = run_config(
        &b,
        Config {
            vector: true,
            graph: true,
            synonyms: false,
        },
    )
    .0;
    let full = run_config(
        &b,
        Config {
            vector: true,
            graph: true,
            synonyms: true,
        },
    )
    .0;
    eprintln!(
        "MRR — bm25 {bm25:.3} | +vec {with_vec:.3} | +graph {with_graph:.3} | full {full:.3}"
    );
    assert!(full >= bm25, "full hybrid must not be worse than BM25-only");
    // FLOOR: measured full-hybrid MRR at corpus landing was 0.760 (bm25
    // 0.718, +vec 0.748, +graph 0.760 — see task-12-report.md for the full
    // tuning history). Set to measured - 0.02, ratchet up over time as the
    // corpus/ranking improves; a regression below this is a real quality
    // bug, not noise.
    const MRR_FLOOR: f64 = 0.740;
    assert!(
        full >= MRR_FLOOR,
        "full-hybrid MRR {full:.3} fell below floor {MRR_FLOOR:.3}"
    );
}
