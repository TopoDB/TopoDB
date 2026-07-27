//! Shared subprocess engine: spawn in own process group (unix), drain stdout/
//! stderr concurrently, enforce a deadline, kill the WHOLE group on deadline
//! or cancellation. Both ShellCommandRunner and CliPrintRunner build on this;
//! neither reimplements spawn/drain/kill.
use super::cancel::CancelToken;
use std::io::Read;
use std::process::{Child, Command, ExitStatus};
use std::time::{Duration, Instant};

#[derive(Debug)]
pub struct ProcOutput {
    pub status: ExitStatus,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum ProcEnd {
    /// Child exited on its own (status meaningful).
    Exited,
    DeadlineKilled,
    Cancelled,
}

/// Spawn `cmd` (stdout/stderr MUST already be Stdio::piped() by the caller;
/// this fn sets the process group on unix), then wait with drain threads and
/// a 10ms poll loop until exit, deadline, or cancellation. On deadline or
/// cancel: kill the process GROUP (unix) / the child (elsewhere), wait, give
/// the drain threads a 50ms grace, and return what was captured. On normal
/// exit: 2s drain grace per stream (grandchild may hold the pipe open —
/// captured bytes are still returned; a stuck drain thread is abandoned).
pub fn run_with_deadline(
    cmd: &mut Command,
    deadline: Duration,
    cancel: Option<&CancelToken>,
) -> std::io::Result<(ProcOutput, ProcEnd)> {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }

    let mut child = cmd.spawn()?;

    // Take the pipes and drain them on their own threads *before* polling
    // for exit. A child that writes more than the OS pipe buffer (~64KB)
    // blocks in write() until someone reads — if we only read after the
    // process exits, try_wait() never returns Some and a large-but-legitimate
    // command is misreported as a timeout.
    //
    // The reader threads report completion over an mpsc channel instead of a
    // JoinHandle, because the spawned command can background a grandchild
    // that inherits these pipe write ends and outlives the immediate child.
    // Killing (or even just waiting on) the immediate child does not close
    // that inherited fd, so a `read_to_end` never sees EOF and a thread join
    // would block forever. `recv_timeout` lets us bound the wait and abandon
    // the thread instead of hanging the whole run.
    //
    // Each thread accumulates into a shared buffer via incremental `read()`
    // calls rather than a single `read_to_end`, so that if the grace period
    // expires before EOF (an orphan is still holding the pipe open), whatever
    // bytes the *legitimate* process already wrote are still visible in the
    // buffer instead of being discarded along with the abandoned thread.
    let out_buf = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let mut out_pipe = child.stdout.take().expect("piped");
    let (out_tx, out_rx) = std::sync::mpsc::channel::<std::io::Result<()>>();
    {
        let out_buf = std::sync::Arc::clone(&out_buf);
        std::thread::spawn(move || {
            let mut chunk = [0u8; 8192];
            loop {
                match out_pipe.read(&mut chunk) {
                    Ok(0) => {
                        let _ = out_tx.send(Ok(()));
                        break;
                    }
                    Ok(n) => out_buf.lock().unwrap().extend_from_slice(&chunk[..n]),
                    Err(e) => {
                        let _ = out_tx.send(Err(e));
                        break;
                    }
                }
            }
        });
    }
    let err_buf = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let mut err_pipe = child.stderr.take().expect("piped");
    let (err_tx, err_rx) = std::sync::mpsc::channel::<std::io::Result<()>>();
    {
        let err_buf = std::sync::Arc::clone(&err_buf);
        std::thread::spawn(move || {
            let mut chunk = [0u8; 8192];
            loop {
                match err_pipe.read(&mut chunk) {
                    Ok(0) => {
                        let _ = err_tx.send(Ok(()));
                        break;
                    }
                    Ok(n) => err_buf.lock().unwrap().extend_from_slice(&chunk[..n]),
                    Err(e) => {
                        let _ = err_tx.send(Err(e));
                        break;
                    }
                }
            }
        });
    }

    // Poll for completion so a hung command cannot stall the run. Cancel is
    // checked before the deadline each iteration, so a cancellation that
    // races a simultaneous deadline is reported as Cancelled.
    let started = Instant::now();
    loop {
        match child.try_wait()? {
            Some(status) => {
                // Normal-exit grace: the process has already exited, so
                // under ordinary circumstances the pipe is closed and the
                // reader thread's EOF signal has already landed (or is
                // about to, essentially immediately) — a couple of seconds
                // is generous slack for a slow scheduler, while still being
                // bounded if an orphaned grandchild is holding the write end
                // open. In that orphan case we don't fail the command solely
                // because a stream couldn't be fully drained; we take
                // whatever the buffer holds so far and abandon that reader
                // thread.
                let normal_grace = Duration::from_secs(2);
                if let Ok(result) = out_rx.recv_timeout(normal_grace) {
                    result?;
                }
                if let Ok(result) = err_rx.recv_timeout(normal_grace) {
                    result?;
                }
                let stdout = out_buf.lock().unwrap().clone();
                let stderr = err_buf.lock().unwrap().clone();
                return Ok((
                    ProcOutput {
                        status,
                        stdout,
                        stderr,
                    },
                    ProcEnd::Exited,
                ));
            }
            None => {
                let cancelled = cancel.is_some_and(CancelToken::is_cancelled);
                if cancelled || started.elapsed() >= deadline {
                    kill_group(&mut child);
                    let status = child.wait()?;
                    // Already declared timed-out/cancelled, so this grace
                    // must not add meaningful latency — it only exists to
                    // pick up output that was already fully buffered before
                    // the kill landed. If an orphaned grandchild still holds
                    // the pipe open, the recv simply times out and that
                    // reader thread is abandoned (leaked, blocked forever,
                    // but bounded to one thread per affected run).
                    let kill_grace = Duration::from_millis(50);
                    let _ = out_rx.recv_timeout(kill_grace);
                    let _ = err_rx.recv_timeout(kill_grace);
                    let stdout = out_buf.lock().unwrap().clone();
                    let stderr = err_buf.lock().unwrap().clone();
                    let end = if cancelled {
                        ProcEnd::Cancelled
                    } else {
                        ProcEnd::DeadlineKilled
                    };
                    return Ok((
                        ProcOutput {
                            status,
                            stdout,
                            stderr,
                        },
                        end,
                    ));
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    }
}

/// Kill child's process group on unix (killpg(child_pid, SIGKILL) — valid
/// because spawn placed the child in its own group with pgid == its pid),
/// plain child.kill() elsewhere. Always followed by wait() by the caller.
pub fn kill_group(child: &mut Child) {
    #[cfg(unix)]
    {
        unsafe {
            libc::killpg(child.id() as i32, libc::SIGKILL);
        }
    }
    #[cfg(not(unix))]
    {
        let _ = child.kill();
    }
}
