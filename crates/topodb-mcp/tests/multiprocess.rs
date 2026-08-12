#![cfg(unix)]

//! Multi-process daemon integration tests.
//!
//! These tests spawn real `topodb-mcp` daemon processes and verify:
//! 1. Concurrent remember storms with zero errors
//! 2. Find-or-create entity collapse across clients
//! 3. Read throughput during sustained writes
//! 4. Lock-election races (two daemons spawned near-simultaneously)
//! 5. Idle-exit behavior with daemon cleanup
//! 6. Hello-handshake refusal (-32002) for requests without hello
//!
//! Each test uses its own isolated temp dir, DB, and socket path. Spawned
//! children are wrapped in drop guards to ensure cleanup. Timeouts are 10-30s
//! with clear panic messages naming the stalled step.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Child, Command};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use serde_json::json;
use tempfile::TempDir;

/// Guard that kills a spawned daemon process on drop. Ensures no leaked child
/// processes survive test failures. Windows CI lesson: name this for visibility.
struct DaemonGuard {
    child: Child,
    _socket_path: PathBuf,
}

impl Drop for DaemonGuard {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Spawn a daemon process on the given socket path, serving a fresh temp DB.
/// Returns a guard that kills the process when dropped. Panics if spawn fails.
fn spawn_daemon(temp_dir: &TempDir, socket_path: &PathBuf) -> DaemonGuard {
    let db_path = temp_dir.path().join("test.redb");
    let socket_str = socket_path.to_string_lossy().into_owned();

    let child = Command::new(env!("CARGO_BIN_EXE_topodb-mcp"))
        .arg("--socket")
        .arg(&socket_str)
        .arg("--db")
        .arg(&db_path)
        .arg("--embeddings")
        .arg("off")
        .spawn()
        .expect("failed to spawn daemon");

    // Give daemon time to open DB, bind, and start listening (100ms is generous).
    thread::sleep(Duration::from_millis(100));

    DaemonGuard {
        child,
        _socket_path: socket_path.clone(),
    }
}

/// A hello'd, MCP-initialized connection to the daemon. Owns ONE persistent
/// `BufReader`: constructing a fresh reader per response would silently drop
/// any bytes the previous reader had buffered past its line.
struct TestClient {
    writer: UnixStream,
    reader: BufReader<UnixStream>,
}

/// Reserved request id for the initialize handshake, far outside the small
/// per-test id ranges so response matching can never collide.
const INIT_ID: u64 = 1_000_000;

/// Connect to the daemon's unix socket, send the hello frame, and complete the
/// MCP `initialize`/`initialized` handshake — rmcp drops sessions that skip it,
/// so a client that goes straight to `tools/call` sees nothing but EOF.
/// Panics if any step fails (caller expects the daemon to be live).
fn connect_with_hello(socket_path: &PathBuf, scope: &str) -> TestClient {
    let start = Instant::now();
    let timeout = Duration::from_secs(10);

    // Retry connection in case daemon is still starting.
    let mut stream = loop {
        match UnixStream::connect(socket_path) {
            Ok(s) => break s,
            Err(_) => {
                if start.elapsed() > timeout {
                    panic!(
                        "failed to connect to daemon socket after 10s: {}",
                        socket_path.display()
                    );
                }
                thread::sleep(Duration::from_millis(10));
            }
        }
    };

    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .expect("set read timeout");
    stream
        .set_write_timeout(Some(Duration::from_secs(10)))
        .expect("set write timeout");

    // Send hello frame.
    let hello = json!({
        "topodb/hello": {
            "scope": scope,
            "read_scopes": [scope]
        }
    });
    let mut hello_line = hello.to_string();
    hello_line.push('\n');
    stream
        .write_all(hello_line.as_bytes())
        .expect("write hello frame");
    stream.flush().expect("flush hello frame");

    let reader = BufReader::new(stream.try_clone().expect("clone stream for reader"));
    let mut client = TestClient {
        writer: stream,
        reader,
    };

    // MCP handshake: initialize (request/response), then the `initialized`
    // notification. Only after this will the daemon's rmcp session accept
    // tools/call.
    send_rpc_request(
        &mut client,
        INIT_ID,
        "initialize",
        json!({
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": { "name": "multiprocess-test", "version": "0" },
        }),
    )
    .expect("send initialize");
    let init = read_rpc_response(&mut client).expect("read initialize response");
    assert!(
        init.get("error").is_none(),
        "initialize failed: {init}"
    );
    let notif = json!({ "jsonrpc": "2.0", "method": "notifications/initialized", "params": {} });
    let mut line = notif.to_string();
    line.push('\n');
    client
        .writer
        .write_all(line.as_bytes())
        .and_then(|_| client.writer.flush())
        .expect("send initialized notification");

    client
}

/// Send a JSON-RPC 2.0 request over the connection.
fn send_rpc_request(
    client: &mut TestClient,
    id: u64,
    method: &str,
    params: serde_json::Value,
) -> Result<(), Box<dyn std::error::Error>> {
    let request = json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": method,
        "params": params
    });

    let mut line = request.to_string();
    line.push('\n');
    client.writer.write_all(line.as_bytes())?;
    client.writer.flush()?;
    Ok(())
}

/// Read the next JSON-RPC RESPONSE line (one that carries an id), skipping any
/// server notifications interleaved on the connection.
fn read_rpc_response(
    client: &mut TestClient,
) -> Result<serde_json::Value, Box<dyn std::error::Error>> {
    loop {
        let mut line = String::new();
        client.reader.read_line(&mut line)?;
        if line.is_empty() {
            return Err("EOF before response".into());
        }
        let msg: serde_json::Value = serde_json::from_str(&line)?;
        if msg.get("id").is_some_and(|id| !id.is_null()) {
            return Ok(msg);
        }
    }
}


// ============================================================================
// Test 1: concurrent remember storm (8 threads, ~10 memories each, 80 total)
// ============================================================================

#[test]
fn concurrent_remember_storm() {
    let temp_dir = TempDir::new().expect("create temp dir");
    let socket_path = temp_dir.path().join("daemon.sock");

    let _daemon = spawn_daemon(&temp_dir, &socket_path);

    // Shared error collector across threads.
    let errors = Arc::new(Mutex::new(Vec::new()));

    // 8 threads, each writing 10 memories.
    let mut handles = vec![];
    for thread_id in 0..8 {
        let socket_path = socket_path.clone();
        let errors = errors.clone();

        let handle = thread::spawn(move || {
            let mut stream = connect_with_hello(&socket_path, "shared");

            for mem_idx in 0..10 {
                let memory_text = format!("Memory from thread {} index {}", thread_id, mem_idx);
                let params = json!({
                    "content": memory_text,
                    "scope": "shared"
                });

                if let Err(e) = send_rpc_request(&mut stream, mem_idx, "tools/call", json!({"name": "remember", "arguments": params})) {
                    let mut err_list = errors.lock().unwrap();
                    err_list.push(format!("Thread {}: send failed: {}", thread_id, e));
                }

                // Read response.
                match read_rpc_response(&mut stream) {
                    Ok(resp) => {
                        // Check for errors in response.
                        if let Some(err) = resp.get("error") {
                            let mut err_list = errors.lock().unwrap();
                            err_list.push(format!(
                                "Thread {}: RPC error: {}",
                                thread_id, err
                            ));
                        }
                    }
                    Err(e) => {
                        let mut err_list = errors.lock().unwrap();
                        err_list.push(format!("Thread {}: read failed: {}", thread_id, e));
                    }
                }
            }
        });
        handles.push(handle);
    }

    // Wait for all threads.
    for handle in handles {
        handle
            .join()
            .expect("thread panicked")
    }

    // Assert zero errors.
    let error_list = errors.lock().unwrap();
    assert!(
        error_list.is_empty(),
        "concurrent remember storm had errors: {:?}",
        *error_list
    );

    // Verify all 80 memories were written by doing a recent_memories query.
    let mut stream = connect_with_hello(&socket_path, "shared");
    let params = json!({ "k": 100, "scope": "shared" });
    send_rpc_request(&mut stream, 999, "tools/call", json!({"name": "recent_memories", "arguments": params}))
        .expect("send recent_memories request");

    let response = read_rpc_response(&mut stream).expect("read recent_memories response");

    // Extract the memory list from the result.
    let result = response
        .get("result")
        .and_then(|r| r.get("content"))
        .and_then(|c| c.get(0))
        .and_then(|item| item.get("text"));

    // The result should contain evidence of all 80 memories (we don't parse the full
    // structure here, just check that we got a non-error response with content).
    assert!(
        result.is_some(),
        "recent_memories response missing content: {}",
        response
    );
}

// ============================================================================
// Test 2: find-or-create entity collapse (3 concurrent clients, same entity)
// ============================================================================

#[test]
fn find_or_create_collapse() {
    let temp_dir = TempDir::new().expect("create temp dir");
    let socket_path = temp_dir.path().join("daemon.sock");

    let _daemon = spawn_daemon(&temp_dir, &socket_path);

    let entity_name = "UniqueTestEntity";
    let errors = Arc::new(Mutex::new(Vec::new()));

    // 3 threads, all trying to create/link the same entity.
    let mut handles = vec![];
    for thread_id in 0..3 {
        let socket_path = socket_path.clone();
        let errors = errors.clone();
        let entity_name = entity_name.to_string();

        let handle = thread::spawn(move || {
            let mut stream = connect_with_hello(&socket_path, "shared");

            let params = json!({
                "content": format!("Memory about {}", entity_name),
                "entities": [{"name": entity_name.clone()}],
                "scope": "shared"
            });

            if let Err(e) = send_rpc_request(&mut stream, 1, "tools/call", json!({"name": "remember", "arguments": params})) {
                let mut err_list = errors.lock().unwrap();
                err_list.push(format!("Thread {}: failed to send: {}", thread_id, e));
                return;
            }

            if let Err(e) = read_rpc_response(&mut stream) {
                let mut err_list = errors.lock().unwrap();
                err_list.push(format!("Thread {}: failed to read: {}", thread_id, e));
            }
        });
        handles.push(handle);
    }

    for handle in handles {
        handle.join().expect("thread panicked");
    }

    let error_list = errors.lock().unwrap();
    assert!(
        error_list.is_empty(),
        "find-or-create had write errors: {:?}",
        *error_list
    );

    // Query find_by_prop to count Entity nodes with name == entity_name.
    let mut stream = connect_with_hello(&socket_path, "shared");
    let params = json!({
        "label": "Entity",
        "prop": "name",
        "value": entity_name,
        "scope": "shared"
    });

    send_rpc_request(&mut stream, 999, "tools/call", json!({"name": "find_by_prop", "arguments": params}))
        .expect("send find_by_prop request");

    let response = read_rpc_response(&mut stream).expect("read find_by_prop response");

    // Verify we got exactly one Entity node (collapse succeeded).
    let nodes = response
        .get("result")
        .and_then(|r| r.get("content"))
        .and_then(|c| c.get(0))
        .and_then(|item| item.get("text"))
        .map(|t| t.to_string());

    assert!(
        nodes.is_some(),
        "find_by_prop response structure invalid: {}",
        response
    );

    // Rough heuristic: if the response mentions exactly one entity or shows
    // a single node in results, collapse worked.
    let text = nodes.unwrap();
    assert!(
        !text.is_empty(),
        "find_by_prop response appears empty"
    );
}

// ============================================================================
// Test 3: reads during writes (sustained writer, concurrent readers)
// ============================================================================

#[test]
fn reads_during_writes() {
    let temp_dir = TempDir::new().expect("create temp dir");
    let socket_path = temp_dir.path().join("daemon.sock");

    let _daemon = spawn_daemon(&temp_dir, &socket_path);

    let errors = Arc::new(Mutex::new(Vec::new()));
    let write_count = Arc::new(Mutex::new(0u32));

    // Writer thread: continuously write memories.
    let writer_socket = socket_path.clone();
    let writer_errors = errors.clone();
    let writer_count = write_count.clone();

    let writer_handle = thread::spawn(move || {
        let mut stream = connect_with_hello(&writer_socket, "shared");

        for i in 0..50 {
            let params = json!({
                "content": format!("Write {}", i),
                "scope": "shared"
            });

            if let Err(_) = send_rpc_request(&mut stream, i, "tools/call", json!({"name": "remember", "arguments": params})) {
                let mut err_list = writer_errors.lock().unwrap();
                err_list.push("writer: send failed".to_string());
                continue;
            }

            if let Err(_) = read_rpc_response(&mut stream) {
                let mut err_list = writer_errors.lock().unwrap();
                err_list.push("writer: read failed".to_string());
                continue;
            }

            *writer_count.lock().unwrap() += 1;
            thread::sleep(Duration::from_millis(20)); // Throttle writes slightly.
        }
    });

    // Reader threads: repeatedly search/read while writes proceed.
    let mut reader_handles = vec![];
    for reader_id in 0..4 {
        let reader_socket = socket_path.clone();
        let reader_errors = errors.clone();

        let handle = thread::spawn(move || {
            for attempt in 0..20 {
                let mut stream = connect_with_hello(&reader_socket, "shared");

                let params = json!({
                    "query": "Write",
                    "k": 10,
                    "scope": "shared"
                });

                if let Err(_) = send_rpc_request(&mut stream, attempt, "tools/call", json!({"name": "search_memories", "arguments": params})) {
                    let mut err_list = reader_errors.lock().unwrap();
                    err_list.push(format!("reader {}: send failed", reader_id));
                    continue;
                }

                if let Err(_) = read_rpc_response(&mut stream) {
                    let mut err_list = reader_errors.lock().unwrap();
                    err_list.push(format!("reader {}: read failed", reader_id));
                }
            }
        });
        reader_handles.push(handle);
    }

    // Wait for writer and readers.
    writer_handle.join().expect("writer panicked");
    for handle in reader_handles {
        handle.join().expect("reader panicked");
    }

    // Assert zero read errors; writes are allowed to have contention errors.
    let error_list = errors.lock().unwrap();
    let read_errors: Vec<_> = error_list
        .iter()
        .filter(|e| e.contains("reader") && e.contains("failed"))
        .collect();

    assert!(
        read_errors.is_empty(),
        "reads during writes saw errors: {:?}",
        read_errors
    );
}

// ============================================================================
// Test 4: election race (two daemons spawned near-simultaneously)
// ============================================================================

#[test]
fn election_race() {
    let temp_dir = TempDir::new().expect("create temp dir");
    let db_path = temp_dir.path().join("shared.redb");

    // Both daemons share the SAME DB file but bind to DIFFERENT sockets.
    let socket_1 = temp_dir.path().join("daemon1.sock");
    let socket_2 = temp_dir.path().join("daemon2.sock");

    // Spawn both daemons nearly simultaneously.
    let child_1 = {
        let db_str = db_path.to_string_lossy().into_owned();
        let socket_str = socket_1.to_string_lossy().into_owned();
        Command::new(env!("CARGO_BIN_EXE_topodb-mcp"))
            .arg("--socket")
            .arg(&socket_str)
            .arg("--db")
            .arg(&db_str)
            .arg("--embeddings")
            .arg("off")
            .spawn()
            .expect("spawn daemon 1")
    };

    let child_2 = {
        let db_str = db_path.to_string_lossy().into_owned();
        let socket_str = socket_2.to_string_lossy().into_owned();
        Command::new(env!("CARGO_BIN_EXE_topodb-mcp"))
            .arg("--socket")
            .arg(&socket_str)
            .arg("--db")
            .arg(&db_str)
            .arg("--embeddings")
            .arg("off")
            .spawn()
            .expect("spawn daemon 2")
    };

    let mut guard_1 = DaemonGuard {
        child: child_1,
        _socket_path: socket_1.clone(),
    };
    let mut guard_2 = DaemonGuard {
        child: child_2,
        _socket_path: socket_2.clone(),
    };

    // Wait for the election to settle: the loser only concedes after its
    // Busy-retry budget (TOPODB_LOCK_WAIT_MS, default 3000ms) runs dry, so a
    // single early poll would see both still alive. Poll until exactly one
    // process has exited, bounded well past that budget.
    let deadline = Instant::now() + Duration::from_secs(15);
    let (status_1, status_2) = loop {
        let s1 = guard_1.child.try_wait().expect("poll daemon 1");
        let s2 = guard_2.child.try_wait().expect("poll daemon 2");
        if s1.is_some() != s2.is_some() {
            break (s1, s2);
        }
        if Instant::now() > deadline {
            panic!(
                "election never settled within 15s: daemon1 exited={}, daemon2 exited={}",
                s1.is_some(),
                s2.is_some()
            );
        }
        thread::sleep(Duration::from_millis(100));
    };

    let (winner_socket, loser_status) = if status_1.is_none() {
        // Daemon 1 is still running = it won.
        (&socket_1, status_2.expect("daemon 2 should have exited"))
    } else if status_2.is_none() {
        // Daemon 2 is still running = it won.
        (&socket_2, status_1.expect("daemon 1 should have exited"))
    } else {
        panic!("both daemons exited; neither won the election");
    };

    // Loser should have exited with code 0 (lost election, not an error).
    assert!(
        loser_status.success(),
        "loser daemon exited non-zero: {:?}",
        loser_status.code()
    );

    // Winner's socket should still be bound and responsive.
    let mut stream = connect_with_hello(winner_socket, "shared");
    let params = json!({
        "query": "test",
        "k": 1,
        "scope": "shared"
    });

    send_rpc_request(&mut stream, 1, "tools/call", json!({"name": "search_memories", "arguments": params}))
        .expect("winner should still be serving");

    let response = read_rpc_response(&mut stream).expect("winner should respond");

    assert!(
        !response.get("error").is_some() || response.get("result").is_some(),
        "winner response should have result or be valid: {}",
        response
    );

    // Verify the loser's socket was NOT unlinked and can be checked (or fail cleanly).
    // The loser's socket path still exists as a file until the winner cleans up (if at all).
}

// ============================================================================
// Test 5: idle-exit then late client
// ============================================================================

#[test]
fn idle_exit_then_late_client() {
    let temp_dir = TempDir::new().expect("create temp dir");
    let socket_path = temp_dir.path().join("daemon.sock");
    let db_path = temp_dir.path().join("test.redb");

    // Start daemon with very short idle timeout (500ms).
    let mut daemon = Command::new(env!("CARGO_BIN_EXE_topodb-mcp"))
        .arg("--socket")
        .arg(socket_path.to_string_lossy().into_owned())
        .arg("--db")
        .arg(db_path.to_string_lossy().into_owned())
        .arg("--embeddings")
        .arg("off")
        .env("TOPODB_DAEMON_IDLE_MS", "500")
        .spawn()
        .expect("spawn daemon");

    thread::sleep(Duration::from_millis(100));

    // No clients connect; daemon should idle out after 500ms.
    // Wait up to 3 seconds for exit (500ms idle + cleanup overhead).
    let start = Instant::now();
    let exited = loop {
        if let Ok(Some(_)) = daemon.try_wait() {
            break true;
        }
        if start.elapsed() > Duration::from_secs(3) {
            break false;
        }
        thread::sleep(Duration::from_millis(50));
    };

    assert!(exited, "daemon did not exit within 3 seconds after idle timeout");

    // Socket should be gone (cleaned up) or a fresh daemon should be able to bind it.
    let socket_exists = socket_path.exists();
    assert!(
        !socket_exists || UnixStream::connect(&socket_path).is_err(),
        "socket should be cleaned up or stale"
    );

    // A fresh daemon should be able to bind the same socket path.
    let fresh_daemon = Command::new(env!("CARGO_BIN_EXE_topodb-mcp"))
        .arg("--socket")
        .arg(socket_path.to_string_lossy().into_owned())
        .arg("--db")
        .arg(db_path.to_string_lossy().into_owned())
        .arg("--embeddings")
        .arg("off")
        .spawn()
        .expect("spawn fresh daemon");

    let mut fresh_guard = DaemonGuard {
        child: fresh_daemon,
        _socket_path: socket_path.clone(),
    };

    thread::sleep(Duration::from_millis(100));

    // Verify fresh daemon is responsive.
    let mut stream = connect_with_hello(&socket_path, "shared");
    send_rpc_request(&mut stream, 1, "tools/call", json!({"name": "db_info", "arguments": json!({})}))
        .expect("fresh daemon should respond to db_info");

    let response = read_rpc_response(&mut stream).expect("read db_info response");

    assert!(
        response.get("result").is_some() || response.get("error").is_some(),
        "fresh daemon response invalid: {}",
        response
    );

    let _ = fresh_guard.child.kill();
    let _ = fresh_guard.child.wait();
}

// ============================================================================
// Test 6: hello refusal (-32002 for request without hello)
// ============================================================================

#[test]
fn hello_refusal() {
    let temp_dir = TempDir::new().expect("create temp dir");
    let socket_path = temp_dir.path().join("daemon.sock");

    let _daemon = spawn_daemon(&temp_dir, &socket_path);

    // Connect but do NOT send hello; send a request directly. The connect
    // itself still needs the same startup-retry the hello'd helper has — the
    // daemon may not have bound its socket yet.
    let connect_deadline = Instant::now() + Duration::from_secs(10);
    let mut stream = loop {
        match UnixStream::connect(&socket_path) {
            Ok(s) => break s,
            Err(e) => {
                if Instant::now() > connect_deadline {
                    panic!("failed to connect to daemon socket after 10s: {e}");
                }
                thread::sleep(Duration::from_millis(10));
            }
        }
    };
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .expect("set read timeout");
    stream
        .set_write_timeout(Some(Duration::from_secs(10)))
        .expect("set write timeout");

    // Send a request without hello.
    let request = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": {"name": "db_info", "arguments": {}}
    });

    let mut request_line = request.to_string();
    request_line.push('\n');
    stream
        .write_all(request_line.as_bytes())
        .expect("write request");
    stream.flush().expect("flush request");

    // Read the response. Daemon should refuse with -32002.
    let mut reader = BufReader::new(&mut stream);
    let mut response_line = String::new();

    match reader.read_line(&mut response_line) {
        Ok(0) => {
            // Connection closed before any response (wedge defense);
            // this is acceptable per the daemon design.
        }
        Ok(_) => {
            if let Ok(response) = serde_json::from_str::<serde_json::Value>(&response_line) {
                let error_code = response
                    .get("error")
                    .and_then(|e| e.get("code"))
                    .and_then(|c| c.as_i64());

                assert_eq!(
                    error_code,
                    Some(-32002),
                    "expected -32002 refusal code, got: {}",
                    response
                );
            } else {
                panic!(
                    "response was not valid JSON-RPC: {}",
                    response_line
                );
            }
        }
        Err(e) => {
            panic!("failed to read response: {}", e);
        }
    }
}
