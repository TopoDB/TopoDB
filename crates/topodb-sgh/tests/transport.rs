#![cfg(feature = "http")]
//! Loopback contract tests for UreqTransport. These pin the HttpTransport
//! contract (provider/mod.rs): HTTP error statuses are Ok((status, body))
//! with the body intact; only transport-level failures are Err.
//! Written against ureq 2 and kept byte-identical across the ureq 3
//! migration — they are the migration's guard.
use std::io::{Read, Write};
use std::net::TcpListener;
use std::time::Duration;
use topodb_sgh::provider::{HttpPayload, HttpTransport, UreqTransport};

/// One-shot HTTP server: accepts a single connection, reads the request
/// until the end of its body, replies with `status` and `body`, records
/// what it saw. Returns (url, join-handle yielding the raw request bytes).
fn one_shot_server(status: u16, body: &'static [u8]) -> (String, std::thread::JoinHandle<Vec<u8>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = std::thread::spawn(move || {
        let (mut sock, _) = listener.accept().unwrap();
        sock.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let mut req = Vec::new();
        let mut buf = [0u8; 4096];
        // Read headers, then exactly content-length body bytes.
        let (mut header_end, mut content_len) = (None, 0usize);
        loop {
            let n = sock.read(&mut buf).unwrap();
            if n == 0 {
                break;
            }
            req.extend_from_slice(&buf[..n]);
            if header_end.is_none() {
                if let Some(pos) = req.windows(4).position(|w| w == b"\r\n\r\n") {
                    header_end = Some(pos + 4);
                    let headers = String::from_utf8_lossy(&req[..pos]);
                    for line in headers.lines() {
                        if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                            content_len = v.trim().parse().unwrap();
                        }
                    }
                }
            }
            if let Some(he) = header_end {
                if req.len() >= he + content_len {
                    break;
                }
            }
        }
        let resp = format!(
            "HTTP/1.1 {status} X\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
            body.len()
        );
        sock.write_all(resp.as_bytes()).unwrap();
        sock.write_all(body).unwrap();
        req
    });
    (format!("http://{addr}/t"), handle)
}

fn payload(url: String) -> HttpPayload {
    HttpPayload {
        url,
        headers: vec![("x-test-header".to_string(), "tv".to_string())],
        body: b"req-body".to_vec(),
    }
}

#[test]
fn ok_status_returns_ok_with_body() {
    let (url, srv) = one_shot_server(200, b"resp-ok");
    let (status, body) = UreqTransport
        .post(&payload(url), Duration::from_secs(5))
        .expect("200 is Ok");
    assert_eq!(status, 200);
    assert_eq!(body, b"resp-ok");
    let raw = String::from_utf8_lossy(&srv.join().unwrap()).to_string();
    assert!(raw.starts_with("POST /t"), "method+path: {raw}");
    assert!(
        raw.to_ascii_lowercase().contains("x-test-header: tv"),
        "{raw}"
    );
    assert!(raw.ends_with("req-body"), "body forwarded: {raw}");
}

#[test]
fn error_status_returns_ok_with_body_intact() {
    // THE load-bearing case: send_with_retries and the codecs read error
    // bodies; a 500 must arrive as Ok((500, body)), never Err.
    let (url, srv) = one_shot_server(500, b"error-detail");
    let (status, body) = UreqTransport
        .post(&payload(url), Duration::from_secs(5))
        .expect("HTTP 500 is Ok((500, body)), not Err");
    assert_eq!(status, 500);
    assert_eq!(body, b"error-detail");
    srv.join().unwrap();
}

#[test]
fn four_xx_returns_ok_with_body_intact() {
    let (url, srv) = one_shot_server(401, b"denied");
    let (status, body) = UreqTransport
        .post(&payload(url), Duration::from_secs(5))
        .expect("HTTP 401 is Ok((401, body))");
    assert_eq!(status, 401);
    assert_eq!(body, b"denied");
    srv.join().unwrap();
}

#[test]
fn connect_refused_is_err() {
    // Bind-then-drop: the port existed a moment ago and is now closed,
    // so connecting fails fast with a refusal, not a timeout.
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = l.local_addr().unwrap();
    drop(l);
    let err = UreqTransport
        .post(&payload(format!("http://{addr}/t")), Duration::from_secs(5))
        .expect_err("connect failure is transport-level Err");
    let _ = err; // io::Error — kind varies by platform; Err-ness is the contract
}

#[test]
fn stalled_server_times_out_as_err() {
    // Pins the whole-call timeout leg of the contract: a server that
    // accepts and then goes silent must yield Err within (roughly) the
    // passed timeout, not hang. The server thread wakes and exits on its
    // own after 3s; it is deliberately not joined — the client must have
    // returned long before that, and the assertion below proves it.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        let (sock, _) = listener.accept().unwrap();
        std::thread::sleep(Duration::from_secs(3));
        drop(sock);
    });
    let start = std::time::Instant::now();
    let err = UreqTransport
        .post(
            &payload(format!("http://{addr}/t")),
            Duration::from_millis(200),
        )
        .expect_err("a stalled server must time out as a transport-level Err");
    let _ = err;
    // Generous CI margin above the 200ms timeout, but well below the 3s
    // server sleep — if the timeout didn't bind the call, the post would
    // only return when the server closes the socket and this fails.
    assert!(
        start.elapsed() < Duration::from_secs(2),
        "timed out in {:?}, expected ~200ms",
        start.elapsed()
    );
}
