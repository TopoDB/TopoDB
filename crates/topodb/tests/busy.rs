//! A second open of a live database must be a typed Busy, not an opaque
//! storage error — front ends retry/back off on this variant.

use topodb::{Db, TopoError};

#[test]
fn second_open_of_live_db_is_busy() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.redb");
    let _held = Db::open(&path).unwrap();
    match Db::open(&path) {
        Err(TopoError::Busy) => {}
        Ok(_) => panic!("second open unexpectedly succeeded"),
        Err(other) => panic!("expected TopoError::Busy, got: {other}"),
    }
}

#[test]
fn busy_clears_when_holder_drops() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.redb");
    {
        let _held = Db::open(&path).unwrap();
    }
    Db::open(&path).expect("open after drop must succeed");
}

#[test]
fn open_stored_on_held_db_is_busy() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.redb");
    // Create the file first so open_stored takes the read-persisted-spec path.
    drop(Db::open(&path).unwrap());
    let _held = Db::open(&path).unwrap();
    match Db::open_stored(&path) {
        Err(TopoError::Busy) => {}
        Ok(_) => panic!("open_stored of a held db unexpectedly succeeded"),
        Err(other) => panic!("expected TopoError::Busy, got: {other}"),
    }
}
