//! HTTP-mock tests for the opencode backend: 401 on health, 500 on REST,
//! and a broken SSE stream. Uses a tiny hand-rolled mock server (no wiremock
//! dependency needed) speaking just enough HTTP for the backend.

use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;

use ai_commander::backend::{BackendEvent, create_backend};
use ai_commander::config::HarnessCfg;

fn cfg_for(port: u16) -> HarnessCfg {
    HarnessCfg {
        id: "ocmock".into(),
        name: "mock".into(),
        kind: "opencode".into(),
        enabled: true,
        server_url: format!("http://127.0.0.1:{port}"),
        server_password: "pw".into(),
        workdir: String::new(),
        agent: String::new(),
        auto_approve: false,
        default_model: None,
    }
}

/// Respond to every request with `status` and `body`.
fn spawn_mock(status: &'static str, body: &'static str) -> u16 {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let mut reader = BufReader::new(match stream.try_clone() {
                Ok(s) => s,
                Err(_) => continue,
            });
            let mut line = String::new();
            if reader.read_line(&mut line).is_err() {
                continue;
            }
            // drain headers
            loop {
                let mut h = String::new();
                if reader.read_line(&mut h).is_err() || h.trim().is_empty() {
                    break;
                }
            }
            let mut stream = stream;
            let _ = write!(
                stream,
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.flush();
        }
    });
    port
}

/// SSE mock: health ok, then the event stream breaks after one event.
fn spawn_sse_breaker() -> u16 {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let mut reader = BufReader::new(match stream.try_clone() {
                Ok(s) => s,
                Err(_) => continue,
            });
            let mut line = String::new();
            if reader.read_line(&mut line).is_err() {
                continue;
            }
            let is_event = line.contains("/api/event");
            loop {
                let mut h = String::new();
                if reader.read_line(&mut h).is_err() || h.trim().is_empty() {
                    break;
                }
            }
            let mut stream = stream;
            if is_event {
                // one valid SSE event, then the connection dies mid-stream
                let body = "data: {\"id\":\"e1\",\"type\":\"session.updated\",\"data\":{}}\n\n";
                let _ = write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\r\n{body}"
                );
                let _ = stream.flush();
                let _ = stream.shutdown(std::net::Shutdown::Both); // break
            } else {
                let body = "{\"data\":[]}\n";
                let _ = write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{body}"
                );
                let _ = stream.flush();
            }
        }
    });
    port
}

#[tokio::test]
async fn health_401_fails_connect_with_clear_error() {
    let port = spawn_mock("401 Unauthorized", r#"{"_tag":"UnauthorizedError"}"#);
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let b = create_backend(&cfg_for(port), tx).unwrap();
    let err = b.connect().await.unwrap_err().to_string();
    assert!(err.contains("not healthy"), "got: {err}");
}

#[tokio::test]
async fn list_models_500_surfaces_error() {
    // health OK (data array), models endpoint 500
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let mut reader = BufReader::new(match stream.try_clone() {
                Ok(s) => s,
                Err(_) => continue,
            });
            let mut line = String::new();
            if reader.read_line(&mut line).is_err() {
                continue;
            }
            let is_models = line.contains("/api/model");
            loop {
                let mut h = String::new();
                if reader.read_line(&mut h).is_err() || h.trim().is_empty() {
                    break;
                }
            }
            let mut stream = stream;
            if is_models {
                let _ = write!(
                    stream,
                    "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 6\r\nConnection: close\r\n\r\nboom!\n"
                );
            } else {
                let _ = write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 10\r\nConnection: close\r\n\r\n{{\"data\":[]}}"
                );
            }
            let _ = stream.flush();
        }
    });

    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let b = create_backend(&cfg_for(port), tx).unwrap();
    b.connect().await.expect("connect ok");
    let err = b.list_models().await.unwrap_err().to_string();
    assert!(err.contains("500"), "got: {err}");
}

#[tokio::test]
async fn broken_see_stream_does_not_crash_backend() {
    let port = spawn_sse_breaker();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<BackendEvent>();
    let b = create_backend(&cfg_for(port), tx).unwrap();
    b.connect().await.expect("connect ok");
    // give the SSE reader a moment to hit the broken stream
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    // the backend must still be usable (no panic, no hang): list_agents works
    let agents = b.list_agents().await.unwrap();
    assert!(agents.is_empty(), "mock returns [] for agents");
    // drain: nothing fatal arrived
    while let Ok(_ev) = rx.try_recv() {}
    b.shutdown();
}
