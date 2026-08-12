#![cfg(unix)]

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Child, Command};
use std::time::{Duration, Instant};
use tempfile::TempDir;

/// Guard to ensure daemon is killed on exit, even if test panics.
struct DaemonGuard {
    child: Child,
}

impl DaemonGuard {
    fn new(child: Child) -> Self {
        DaemonGuard { child }
    }
}

impl Drop for DaemonGuard {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Spawn the daemon with a temp DB and socket.
fn spawn_daemon(db_path: &PathBuf, socket_path: &PathBuf) -> std::io::Result<DaemonGuard> {
    let binary = env!("CARGO_BIN_EXE_topodb-mcp");
    let child = Command::new(binary)
        .arg("--db")
        .arg(db_path)
        .arg("--socket")
        .arg(socket_path)
        .arg("--embeddings")
        .arg("off")
        .spawn()?;

    Ok(DaemonGuard::new(child))
}

/// A single client connection to the daemon.
struct Client {
    stream: UnixStream,
    reader: BufReader<UnixStream>,
}

impl Client {
    fn new(socket_path: &PathBuf) -> std::io::Result<Self> {
        let stream = UnixStream::connect(socket_path)?;
        stream.set_read_timeout(Some(Duration::from_secs(5)))?;
        stream.set_write_timeout(Some(Duration::from_secs(5)))?;

        let reader_stream = stream.try_clone()?;
        let reader = BufReader::new(reader_stream);

        Ok(Client { stream, reader })
    }

    /// Send hello frame to establish scope.
    fn send_hello(&mut self, scope: &str) -> std::io::Result<()> {
        let hello = serde_json::json!({
            "topodb/hello": {
                "scope": scope,
                "read_scopes": [scope]
            }
        });

        let mut line = hello.to_string();
        line.push('\n');
        self.stream.write_all(line.as_bytes())?;
        self.stream.flush()?;
        Ok(())
    }

    /// Send an MCP initialize request.
    fn send_initialize(&mut self) -> std::io::Result<()> {
        let init = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": {
                    "name": "bench-client",
                    "version": "0.0.1"
                }
            }
        });

        let mut line = init.to_string();
        line.push('\n');
        self.stream.write_all(line.as_bytes())?;
        self.stream.flush()?;
        Ok(())
    }

    /// Send a search_memories tool call.
    fn send_search_memories(&mut self, query: &str, req_id: i32) -> std::io::Result<()> {
        let search = serde_json::json!({
            "jsonrpc": "2.0",
            "id": req_id,
            "method": "tools/call",
            "params": {
                "name": "search_memories",
                "arguments": {
                    "query": query
                }
            }
        });

        let mut line = search.to_string();
        line.push('\n');
        self.stream.write_all(line.as_bytes())?;
        self.stream.flush()?;
        Ok(())
    }

    /// Send a create_memory tool call.
    fn send_create_memory(&mut self, content: &str, req_id: i32) -> std::io::Result<()> {
        let create = serde_json::json!({
            "jsonrpc": "2.0",
            "id": req_id,
            "method": "tools/call",
            "params": {
                "name": "create_memory",
                "arguments": {
                    "content": content,
                    "labels": []
                }
            }
        });

        let mut line = create.to_string();
        line.push('\n');
        self.stream.write_all(line.as_bytes())?;
        self.stream.flush()?;
        Ok(())
    }

    /// Read one response line from the daemon.
    fn read_response(&mut self) -> std::io::Result<String> {
        let mut line = String::new();
        self.reader.read_line(&mut line)?;
        if line.is_empty() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "daemon closed connection",
            ));
        }
        Ok(line)
    }
}

fn main() {
    println!("TopoDB daemon throughput benchmark");

    // Create temp DB and socket path.
    let db_dir = TempDir::new().expect("Failed to create temp dir");
    let db_path = db_dir.path().join("bench.redb");
    let socket_path = db_dir.path().join("bench.sock");

    // Spawn daemon.
    println!("Spawning daemon at {:?}", socket_path);
    let _daemon = match spawn_daemon(&db_path, &socket_path) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("Failed to spawn daemon: {}", e);
            return;
        }
    };

    // Give daemon time to bind.
    std::thread::sleep(Duration::from_millis(500));

    // Connect N reader clients.
    const NUM_READERS: usize = 8;
    let mut readers: Vec<Client> = Vec::new();

    for i in 0..NUM_READERS {
        match Client::new(&socket_path) {
            Ok(mut client) => {
                if let Err(e) = client.send_hello(&format!("bench-reader-{}", i)) {
                    eprintln!("Failed to send hello for reader {}: {}", i, e);
                    return;
                }
                // Read and discard hello response
                if let Err(e) = client.read_response() {
                    eprintln!("Failed to read hello response for reader {}: {}", i, e);
                    return;
                }
                readers.push(client);
            }
            Err(e) => {
                eprintln!("Failed to connect reader {}: {}", i, e);
                return;
            }
        }
    }

    // Initialize all readers.
    for (i, client) in readers.iter_mut().enumerate() {
        if let Err(e) = client.send_initialize() {
            eprintln!("Failed to send initialize for reader {}: {}", i, e);
            return;
        }
        // Read and discard initialize response
        if let Err(e) = client.read_response() {
            eprintln!("Failed to read initialize response for reader {}: {}", i, e);
            return;
        }
    }

    // Connect one writer client.
    let mut writer = match Client::new(&socket_path) {
        Ok(mut client) => {
            if let Err(e) = client.send_hello("bench-writer") {
                eprintln!("Failed to send hello for writer: {}", e);
                return;
            }
            // Read and discard hello response
            if let Err(e) = client.read_response() {
                eprintln!("Failed to read hello response for writer: {}", e);
                return;
            }
            if let Err(e) = client.send_initialize() {
                eprintln!("Failed to send initialize for writer: {}", e);
                return;
            }
            // Read and discard initialize response
            if let Err(e) = client.read_response() {
                eprintln!("Failed to read initialize response for writer: {}", e);
                return;
            }
            client
        }
        Err(e) => {
            eprintln!("Failed to connect writer: {}", e);
            return;
        }
    };

    // Run benchmark for ~5 seconds.
    let bench_duration = Duration::from_secs(5);
    let start = Instant::now();

    let mut latencies: Vec<Duration> = Vec::new();
    let mut read_count = 0u64;
    let mut write_count = 0u64;
    let mut req_id = 100;

    while start.elapsed() < bench_duration {
        // Send search from each reader (pipelined).
        for (reader_idx, client) in readers.iter_mut().enumerate() {
            let query = format!(
                "memory {}",
                (reader_idx as u128 + start.elapsed().as_millis()) % 10
            );
            if let Err(e) = client.send_search_memories(&query, req_id) {
                eprintln!("Failed to send search from reader {}: {}", reader_idx, e);
                return;
            }
            req_id += 1;
        }

        // Send write from writer.
        if write_count < 100 {
            let content = format!("bench memory {}", write_count);
            if let Err(e) = writer.send_create_memory(&content, req_id) {
                eprintln!("Failed to send create_memory: {}", e);
                return;
            }
            req_id += 1;
            write_count += 1;
        }

        // Read responses from all readers (measure latency).
        for (reader_idx, client) in readers.iter_mut().enumerate() {
            let read_start = Instant::now();
            match client.read_response() {
                Ok(_response) => {
                    let latency = read_start.elapsed();
                    latencies.push(latency);
                    read_count += 1;
                }
                Err(e) => {
                    eprintln!(
                        "Failed to read search response from reader {}: {}",
                        reader_idx, e
                    );
                    return;
                }
            }
        }

        // Read write response if pending.
        if write_count > 0 {
            match writer.read_response() {
                Ok(_response) => {
                    // Write completed
                }
                Err(e) => {
                    eprintln!("Failed to read write response: {}", e);
                    return;
                }
            }
        }
    }

    let elapsed = start.elapsed();

    // Compute statistics.
    latencies.sort();
    let p50 = if !latencies.is_empty() {
        latencies[latencies.len() / 2]
    } else {
        Duration::from_secs(0)
    };
    let p95 = if !latencies.is_empty() {
        latencies[(latencies.len() * 95) / 100]
    } else {
        Duration::from_secs(0)
    };

    let throughput = if elapsed.as_secs_f64() > 0.0 {
        read_count as f64 / elapsed.as_secs_f64()
    } else {
        0.0
    };

    println!("\n=== Throughput Benchmark Results ===");
    println!("Read latency p50: {:.2}ms", p50.as_secs_f64() * 1000.0);
    println!("Read latency p95: {:.2}ms", p95.as_secs_f64() * 1000.0);
    println!("Total reads: {}", read_count);
    println!("Total throughput: {:.0} reads/sec", throughput);
    println!("Benchmark duration: {:.2}s", elapsed.as_secs_f64());
    println!("Concurrent readers: {}", NUM_READERS);
    println!("Concurrent writers: 1");
}

#[cfg(not(unix))]
fn main() {
    println!("This benchmark is unix-only");
}
