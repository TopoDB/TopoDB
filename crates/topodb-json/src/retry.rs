//! Bounded retry for opening a `Db` whose file another process holds.
//! Retries ONLY on [`TopoError::Busy`]; every other outcome (success or a
//! different error) is returned from the first attempt that produced it.

use std::io::Write;
use std::time::{Duration, Instant};

use topodb::{Db, TopoError};

/// Calls `open` until it stops returning `Busy` or `budget_ms` elapses.
/// Backoff: 25ms doubling to a 500ms cap, with cheap deterministic-enough
/// jitter (from the clock's nanoseconds — no rand dependency) so two
/// contending processes don't retry in lockstep. `budget_ms == 0` means
/// exactly one attempt (fail fast). Exhaustion returns `Err(Busy)`.
///
/// Prints one stderr line when cumulative elapsed exceeds 500ms and another
/// retry is about to sleep, at most once per call.
pub fn open_with_busy_retry(
    budget_ms: u64,
    mut open: impl FnMut() -> Result<Db, TopoError>,
) -> Result<Db, TopoError> {
    let start = Instant::now();
    let mut delay_ms: u64 = 25;
    let mut printed_retry_note = false;
    loop {
        match open() {
            Err(TopoError::Busy) => {
                let elapsed = start.elapsed();
                let elapsed_ms = elapsed.as_millis() as u64;
                if elapsed_ms >= budget_ms {
                    return Err(TopoError::Busy);
                }
                let jitter = u64::from(elapsed.subsec_nanos()) % (delay_ms / 4 + 1);
                let remaining = budget_ms - elapsed_ms;

                // Print retry note once cumulative elapsed exceeds 500ms.
                if !printed_retry_note && elapsed_ms >= 500 {
                    eprintln!(
                        "topodb: database held by another process; retrying (budget {}ms)",
                        budget_ms
                    );
                    let _ = std::io::stderr().flush();
                    printed_retry_note = true;
                }

                std::thread::sleep(Duration::from_millis((delay_ms + jitter).min(remaining)));
                delay_ms = (delay_ms * 2).min(500);
            }
            other => return other,
        }
    }
}
