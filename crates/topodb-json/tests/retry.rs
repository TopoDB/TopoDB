use std::sync::atomic::{AtomicU32, Ordering};
use topodb::{Db, TopoError};
use topodb_json::open_with_busy_retry;

#[test]
fn zero_budget_fails_fast_on_busy() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.redb");
    let _held = Db::open(&path).unwrap();
    let attempts = AtomicU32::new(0);
    let res = open_with_busy_retry(0, || {
        attempts.fetch_add(1, Ordering::SeqCst);
        Db::open(&path)
    });
    assert!(matches!(res, Err(TopoError::Busy)));
    assert_eq!(
        attempts.load(Ordering::SeqCst),
        1,
        "budget 0 = exactly one try"
    );
}

#[test]
fn retries_until_the_holder_releases() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.redb");
    // Create the file, then hold it from another thread for ~200ms.
    drop(Db::open(&path).unwrap());
    let held = Db::open(&path).unwrap();
    let handle = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(200));
        drop(held);
    });
    let res = open_with_busy_retry(5_000, || Db::open(&path));
    handle.join().unwrap();
    assert!(
        res.is_ok(),
        "must succeed once the holder drops: {:?}",
        res.err().map(|e| e.to_string())
    );
}

#[test]
fn non_busy_errors_are_not_retried() {
    // A path whose PARENT does not exist fails immediately with a non-Busy
    // error; the helper must pass it straight through on the first attempt.
    let attempts = AtomicU32::new(0);
    let res = open_with_busy_retry(5_000, || {
        attempts.fetch_add(1, Ordering::SeqCst);
        Db::open_stored("/nonexistent-dir-xyz/t.redb")
    });
    assert!(res.is_err());
    assert_eq!(attempts.load(Ordering::SeqCst), 1);
}
