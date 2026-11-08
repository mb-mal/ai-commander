//! Б2: the ACP backend must speak schema-canonical Agent Client Protocol.
//! This conformance test drives AcpBackend against the fake-acp-agent
//! binary, which validates EVERY client message against the schema types
//! (deserialize + exact round-trip) and logs OK/FAIL per method. The test
//! fails if any message is off-schema — e.g. the old hand-rolled
//! `session/cancel {sessionId, reason}` (reason is not in the schema).

use std::time::Duration;

use ai_commander::backend::{BackendEvent, SendOpts, create_backend};
use ai_commander::config::HarnessCfg;

fn fake_cfg(exe: &str, workdir: &str) -> HarnessCfg {
    HarnessCfg {
        id: "fakeacp".into(),
        name: "fake acp".into(),
        kind: "opencode-acp".into(),
        enabled: true,
        server_url: exe.into(),
        server_password: String::new(),
        workdir: workdir.into(),
        agent: String::new(),
        auto_approve: false,
        default_model: None,
    }
}

#[tokio::test]
async fn acp_backend_speaks_schema_canonical_protocol() {
    let exe = env!("CARGO_BIN_EXE_fake-acp-agent");
    let workdir = std::env::temp_dir()
        .join(format!("aic-fakeacp-{}", std::process::id()))
        .to_string_lossy()
        .into_owned();
    std::fs::create_dir_all(&workdir).unwrap();
    let log = std::env::temp_dir().join(format!("aic-fakeacp-{}.log", std::process::id()));
    let _ = std::fs::remove_file(&log);
    // the fake agent inherits our env and writes its validation log there
    // (set_var before any thread spawns; single-test binary)
    // SAFETY: no other threads reference the environment at this point.
    unsafe { std::env::set_var("FAKE_ACP_LOG", &log) };

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<BackendEvent>();
    let b = create_backend(&fake_cfg(exe, &workdir), tx).unwrap();
    b.connect().await.expect("initialize against fake agent");

    // send a first message: session/new + session/prompt
    b.send(
        "pending-fake",
        "hello protocol",
        &[],
        SendOpts {
            model: None,
            agent: None,
            mcp_role: None,
            cwd: None,
        },
    )
    .await
    .expect("send");

    // collect events until StreamDone (bounded wait)
    let mut got_echo = false;
    let mut done = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    while tokio::time::Instant::now() < deadline && !done {
        match tokio::time::timeout(Duration::from_millis(500), rx.recv()).await {
            Ok(Some(ev)) => match ev {
                BackendEvent::PartDelta { delta, .. } => {
                    if delta.contains("echo: hello protocol") {
                        got_echo = true;
                    }
                }
                BackendEvent::StreamError { error, .. } => {
                    panic!("unexpected StreamError: {error}");
                }
                BackendEvent::StreamDone { .. } => done = true,
                _ => {}
            },
            Ok(None) => break,
            Err(_) => continue,
        }
    }
    assert!(done, "the turn must finish (StreamDone)");
    assert!(got_echo, "the agent_message_chunk echo must arrive");

    // abort → session/cancel notification must also be schema-canonical
    b.abort("pending-fake").await.expect("abort");
    tokio::time::sleep(Duration::from_millis(300)).await;
    b.shutdown();

    // the fake agent must have validated every message as OK
    let log = std::fs::read_to_string(&log).unwrap_or_default();
    assert!(
        !log.is_empty(),
        "fake agent wrote no validation log — was FAKE_ACP_LOG set?"
    );
    let fails: Vec<&str> = log.lines().filter(|l| l.starts_with("FAIL")).collect();
    assert!(
        fails.is_empty(),
        "schema violations in client messages:\n{}",
        fails.join("\n")
    );
    for method in ["initialize", "session/new", "session/prompt"] {
        assert!(
            log.lines().any(|l| l == format!("OK {method}")),
            "{method} was not validated OK; log:\n{log}"
        );
    }

    let _ = std::fs::remove_dir_all(&workdir);
    let _ = std::fs::remove_file(&log);
}
