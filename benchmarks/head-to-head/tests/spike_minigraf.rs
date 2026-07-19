//! Spike: verify minigraf's real API against what its README claims.
//! Every assertion here is a hypothesis until it compiles and passes.

#[test]
fn open_transact_and_query_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("spike.graph");

    // HYPOTHESIS: OpenOptions::new().path(..).open()
    let db = minigraf::OpenOptions::new()
        .path(&path)
        .open()
        .expect("open a fresh database");

    // HYPOTHESIS: execute() takes a Datalog string and transact takes EAV triples
    db.execute(r#"(transact [[:alice :person/name "Alice"]])"#)
        .expect("transact one fact");

    // HYPOTHESIS: query returns something we can count and read
    let results = db
        .execute(r#"(query [:find ?name :where [?e :person/name ?name]])"#)
        .expect("query the fact back");

    // Print the shape so we learn what execute() actually returns.
    println!("QUERY RESULT DEBUG: {results:?}");
}

#[test]
fn as_of_query_shape() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("asof.graph");
    let db = minigraf::OpenOptions::new().path(&path).open().unwrap();

    db.execute(r#"(transact [[:alice :person/age 30]])"#).unwrap();
    db.execute(r#"(transact [[:alice :person/age 31]])"#).unwrap();

    // HYPOTHESIS: :as-of takes a transaction counter and reconstructs past state.
    let past = db
        .execute(r#"(query [:find ?age :as-of 1 :where [:alice :person/age ?age]])"#)
        .expect("as-of query");
    println!("AS-OF RESULT DEBUG: {past:?}");

    let now = db
        .execute(r#"(query [:find ?age :where [:alice :person/age ?age]])"#)
        .expect("current query");
    println!("CURRENT RESULT DEBUG: {now:?}");
}

/// HYPOTHESIS (revised after reading minigraf's own bitemporal_test.rs and
/// retraction_test.rs on crates.io source): (transact) does NOT overwrite a
/// prior value for the same (entity, attribute) pair. Both old and new
/// values remain simultaneously "currently valid" until the old one is
/// explicitly (retract)ed. Confirm this empirically, and confirm that a
/// retract-then-transact (point-lookup "update") leaves exactly one row.
#[test]
fn point_lookup_requires_explicit_retract_for_last_write_wins() {
    let db = minigraf::Minigraf::in_memory().unwrap();
    db.execute(r#"(transact [[:alice :person/age 30]])"#).unwrap();
    db.execute(r#"(transact [[:alice :person/age 31]])"#).unwrap();

    let both = db
        .execute(r#"(query [:find ?age :where [:alice :person/age ?age]])"#)
        .unwrap();
    println!("WITHOUT RETRACT (expect 2 rows): {both:?}");

    // Now do it the way a "point lookup after update" benchmark op must:
    // retract the old value explicitly before asserting the new one.
    db.execute(r#"(retract [[:alice :person/age 30]])"#).unwrap();
    db.execute(r#"(transact [[:alice :person/age 32]])"#).unwrap();

    let one = db
        .execute(r#"(query [:find ?age :where [:alice :person/age ?age]])"#)
        .unwrap();
    println!("AFTER RETRACT+TRANSACT (expect 1 row, 31 and 32 both asserted so still 2 unless 31 retracted too): {one:?}");
}

/// HYPOTHESIS: bounded k-hop traversal is expressible as k chained EAV
/// patterns in a single (query ... :where ...) clause — no recursive (rule)
/// needed for a *bounded* depth, since recursive rules give unbounded
/// transitive closure instead.
#[test]
fn bounded_k_hop_traversal_via_chained_patterns() {
    let db = minigraf::Minigraf::in_memory().unwrap();
    db.execute("(transact [[:a :next :b] [:b :next :c] [:c :next :d]])")
        .unwrap();

    // 2-hop: a -> ?mid -> ?end
    let two_hop = db
        .execute("(query [:find ?end :where [:a :next ?mid] [?mid :next ?end]])")
        .unwrap();
    println!("2-HOP FROM :a DEBUG (expect :c): {two_hop:?}");

    // 3-hop: a -> ?m1 -> ?m2 -> ?end
    let three_hop = db
        .execute("(query [:find ?end :where [:a :next ?m1] [?m1 :next ?m2] [?m2 :next ?end]])")
        .unwrap();
    println!("3-HOP FROM :a DEBUG (expect :d): {three_hop:?}");
}

/// HYPOTHESIS: there is no in-crate "size on disk" API (confirmed absent
/// from db.rs's public surface); on-disk size must be measured via
/// std::fs::metadata on the .graph file, after an explicit checkpoint() so
/// WAL-buffered facts are flushed into the main file first.
#[test]
fn on_disk_size_via_filesystem_metadata_after_checkpoint() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("size.graph");
    let db = minigraf::OpenOptions::new().path(&path).open().unwrap();
    db.execute(r#"(transact [[:alice :person/name "Alice"]])"#)
        .unwrap();
    db.checkpoint().unwrap();
    let size = std::fs::metadata(&path).unwrap().len();
    println!("ON-DISK SIZE AFTER CHECKPOINT: {size} bytes");
    assert!(size > 0);
    println!("current_tx_count() = {}", db.current_tx_count());
}
