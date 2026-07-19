//! Minigraf (bi-temporal Datalog EAV) implementation of the `Engine` trait.
//!
//! Corrections applied from
//! `docs/superpowers/notes/2026-07-18-minigraf-api-findings.md` against the
//! plan's hypotheses:
//! - The type is `minigraf::Minigraf`, not `minigraf::Db`.
//! - `execute()` returns `QueryResult::{Transacted(u64), Retracted(u64),
//!   QueryResults { vars, results }, Ok}`; row extraction reads
//!   `QueryResults.results` (`Vec<Vec<Value>>`), not some other shape.
//! - `Transacted`/`Retracted`'s `u64` is a Unix-ms `tx_id`, not the `:as-of`
//!   counter -- irrelevant here since this driver never inspects it, but
//!   noted because a naive read of the plan would have used it as one.
//! - There is no in-crate on-disk-size API: `on_disk_bytes` calls
//!   `checkpoint()` (to flush the WAL) before `fs::metadata`.
//! - The corpus is write-once (see `corpus.rs` module doc): `insert_corpus`
//!   never asserts the same `[entity attribute]` twice, so no `retract`
//!   pairing is needed anywhere in this file.
//!
//! **`k_hop` is NOT implemented as a real bounded traversal.** Extensive
//! investigation (see the task report's "k-hop" section) found that
//! minigraf 1.2.1 cannot express "distinct nodes reachable within N hops,
//! either direction" as a single reliable query against this corpus:
//! - Recursive rules are capped at 2 arguments (`predicate ?a ?b`), which
//!   rules out the crate's own documented depth-counter technique
//!   (`(reachable-5 ?a ?b ?d)` needs 3).
//! - A materialised 2-ary `(or ...)` "connected" rule parses and runs fast,
//!   but its derived facts mix `Value::Ref` and `Value::Keyword`
//!   representations for the same logical node depending on which side of
//!   the underlying edge fact matched, breaking further chaining.
//! - Even switching every entity to an explicit `#uuid` literal (removing
//!   the Ref/Keyword ambiguity entirely) did not fix it: plain
//!   value-position lookups (`[?x :attr <literal>]`, needed for the
//!   "reverse" half of an undirected hop) silently *undercounted* real
//!   matches against the actual corpus (e.g. 4 of 7 real `:edge/derived_from`
//!   edges into a hub node), while the same query shape returned complete,
//!   correct results against a synthetic dataset of comparable size. This
//!   points to a genuine correctness issue in minigraf's query executor
//!   under this corpus's specific data shape, not a query-design mistake.
//!
//! Rather than approximate bounded k-hop with N separate queries (which
//! would time a different operation and misreport), `k_hop` returns
//! `EngineError::Backend` unconditionally. This operation is out of the
//! scored set for minigraf; see the report for the full investigation.

use std::path::{Path, PathBuf};

use minigraf::{Minigraf, QueryResult};

use crate::corpus::Corpus;
use crate::engine::{AsOfSupport, Engine, EngineError, Payload};

pub struct MinigrafDriver {
    db: Minigraf,
    path: PathBuf,
}

fn err<E: std::fmt::Display>(e: E) -> EngineError {
    EngineError::Backend(e.to_string())
}

/// Escape a value for embedding in a Datalog string literal.
fn q(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

/// The Datalog keyword identifying a corpus node's entity.
fn node_kw(id: usize) -> String {
    format!(":n{id}")
}

impl Engine for MinigrafDriver {
    fn open(path: &Path) -> Result<Self, EngineError> {
        let db = minigraf::OpenOptions::new()
            .path(path)
            .open()
            .map_err(err)?;
        Ok(MinigrafDriver {
            db,
            path: path.to_path_buf(),
        })
    }

    fn insert_corpus(&mut self, corpus: &Corpus) -> Result<(), EngineError> {
        // Exactly PROP_NAMES.len() (5) facts per node, one fact per edge --
        // no entity-existence marker, no type/label triple, no bookkeeping
        // facts of any kind. See the task report's fact-count section for
        // the arithmetic proving this matches `Corpus::translation_ratio().facts`.
        let mut node_facts = String::new();
        for n in &corpus.nodes {
            let e = node_kw(n.id);
            node_facts.push_str(&format!(
                r#"[{e} :node/name "{}"] [{e} :node/kind "{}"] [{e} :node/note "{}"] [{e} :node/rank {}] [{e} :node/active {}] "#,
                q(&n.name),
                q(&n.kind),
                q(&n.note),
                n.rank,
                n.active,
            ));
        }
        self.db
            .execute(&format!("(transact [{node_facts}])"))
            .map_err(err)?;

        let mut edge_facts = String::new();
        for e in &corpus.edges {
            edge_facts.push_str(&format!(
                "[{} :edge/{} {}] ",
                node_kw(e.from),
                e.ty.to_lowercase(),
                node_kw(e.to),
            ));
        }
        self.db
            .execute(&format!("(transact [{edge_facts}])"))
            .map_err(err)?;

        Ok(())
    }

    fn point_lookup(&self, id: usize) -> Result<Option<Payload>, EngineError> {
        let e = node_kw(id);
        let res = self
            .db
            .execute(&format!(
                "(query [:find ?name ?rank :where [{e} :node/name ?name] [{e} :node/rank ?rank]])"
            ))
            .map_err(err)?;

        Ok(extract_payload(&res))
    }

    fn k_hop(&self, _seed: usize, _depth: u8) -> Result<usize, EngineError> {
        // See the module doc comment: not reliably expressible as a single
        // correct query against this corpus in minigraf 1.2.1. Reported as
        // an honest failure rather than an approximated or silently wrong
        // count -- this operation is out of the scored set for minigraf.
        Err(EngineError::Backend(
            "bounded k-hop is not reliably expressible in minigraf 1.2.1 Datalog against this \
             corpus (see docs/superpowers/sdd/task-4-report.md); not in the scored set"
                .to_string(),
        ))
    }

    fn on_disk_bytes(&self) -> Result<u64, EngineError> {
        // Writes may sit unflushed in the sidecar WAL until an automatic or
        // manual checkpoint; measuring without one would make the reported
        // size depend on incidental WAL flush timing rather than the data
        // actually stored.
        self.db.checkpoint().map_err(err)?;
        Ok(std::fs::metadata(&self.path).map_err(err)?.len())
    }

    fn as_of_support() -> AsOfSupport {
        AsOfSupport::Supported
    }
}

impl MinigrafDriver {
    /// Total number of `[entity attribute value]` triples currently in the
    /// database, via an unconstrained wildcard scan. Used to verify, against
    /// a real database rather than by inspecting `insert_corpus`'s source,
    /// that what gets asserted matches `Corpus::translation_ratio().facts`
    /// exactly -- see the task report's fact-count section.
    pub fn fact_count(&self) -> Result<usize, EngineError> {
        let res = self
            .db
            .execute("(query [:find (count ?e) :where [?e ?a ?v]])")
            .map_err(err)?;
        let QueryResult::QueryResults { results, .. } = res else {
            return Err(err("expected QueryResults counting facts"));
        };
        results
            .first()
            .and_then(|row| row.first())
            .and_then(|v| v.as_integer())
            .map(|n| n as usize)
            .ok_or_else(|| err("count query returned no rows"))
    }
}

/// Pull `(?name, ?rank)` out of a point-lookup `QueryResult`. Empty results
/// (unknown id) and any shape mismatch both come back as `None`, never an
/// error -- a miss is not a backend failure.
fn extract_payload(res: &QueryResult) -> Option<Payload> {
    let QueryResult::QueryResults { results, .. } = res else {
        return None;
    };
    let row = results.first()?;
    let name = row.first()?.as_string()?.to_string();
    let rank = row.get(1)?.as_integer()?;
    Some(Payload { name, rank })
}
