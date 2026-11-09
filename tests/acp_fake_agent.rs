//! Б2: the ACP backend must speak schema-canonical Agent Client Protocol.
//! This conformance test drives AcpBackend against the fake-acp-agent
//! binary, which validates EVERY client message against the schema types
//! (deserialize + exact round-trip) and logs OK/FAIL per method. The test
//! fails if any message is off-schema — e.g. the old hand-rolled
//! `session/cancel {sessionId, reason}` (reason is not in the schema).

use std::time::Duration;

use ai_commander::backend::{BackendEvent, SendOpts, create_backend};
use ai_commander::config::HarnessCfg;

/// Both tests set FAKE_ACP_LOG (process-global env) — serialize them.
static ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

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
    let _env_guard = ENV_LOCK.lock().await;
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

    // collect events until StreamDone (bounded wait); answer the agent's
    // permission request (string id "perm-1") with "once"
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
                BackendEvent::PermissionRequest {
                    session_id,
                    request_id,
                    ..
                } => {
                    let b = b.clone();
                    tokio::spawn(async move {
                        let _ = b.permission_reply(&session_id, &request_id, "once").await;
                    });
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

/// v4 Б2 permission scenarios: the user's answer must map to the option
/// with the matching schema `kind` (never a blind first-option pick), and
/// cancelling the turn answers every pending request with `cancelled`.
/// The fake agent uses a STRING jsonrpc id ("perm-1") — routing the reply
/// proves string ids are supported.
async fn permission_scenario(tag: &str, user_reply: Option<String>, expected_log: &str) {
    let exe = env!("CARGO_BIN_EXE_fake-acp-agent");
    let dir = std::env::temp_dir().join(format!("aic-perm-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let log = dir.join("perm.log");
    // SAFETY: per-scenario env, set before the child spawns; scenarios run
    // sequentially within this one test binary
    unsafe { std::env::set_var("FAKE_ACP_LOG", &log) };

    let cfg = HarnessCfg {
        id: format!("fakeacp-{tag}"),
        name: "fake acp".into(),
        kind: "opencode-acp".into(),
        enabled: true,
        server_url: exe.into(),
        server_password: String::new(),
        workdir: dir.to_string_lossy().into_owned(),
        agent: String::new(),
        auto_approve: false,
        default_model: None,
    };
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<BackendEvent>();
    let b = create_backend(&cfg, tx).unwrap();
    b.connect().await.expect("initialize");

    b.send(
        "pending-fake",
        "probe",
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

    let mut done = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    while tokio::time::Instant::now() < deadline && !done {
        match tokio::time::timeout(Duration::from_millis(300), rx.recv()).await {
            Ok(Some(ev)) => match ev {
                BackendEvent::PermissionRequest {
                    session_id,
                    request_id,
                    ..
                } => match user_reply.clone() {
                    // answer as the user would (errors surface the panic)
                    Some(reply) => {
                        b.permission_reply(&session_id, &request_id, &reply)
                            .await
                            .expect("permission_reply failed");
                    }
                    // cancel-during-request: abort the turn instead
                    None => {
                        b.abort(&session_id).await.expect("abort failed");
                    }
                },
                BackendEvent::StreamDone { .. } => done = true,
                BackendEvent::StreamError { error, .. } => {
                    panic!("[{tag}] unexpected StreamError: {error}")
                }
                _ => {}
            },
            Ok(None) => break,
            Err(_) => continue,
        }
    }
    assert!(done, "[{tag}] the turn must finish");
    b.shutdown();
    tokio::time::sleep(Duration::from_millis(200)).await;

    let log = std::fs::read_to_string(&log).unwrap_or_default();
    assert!(
        log.lines().any(|l| l.contains(expected_log)),
        "[{tag}] expected `{expected_log}` in the agent log:\n{log}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn acp_permission_replies_match_schema_kinds() {
    let _env_guard = ENV_LOCK.lock().await;
    permission_scenario("once", Some("once".into()), "PERM selected allow_once").await;
    permission_scenario(
        "always",
        Some("always".into()),
        "PERM selected allow_always",
    )
    .await;
    permission_scenario("reject", Some("reject".into()), "PERM selected reject_once").await;
    // cancel during the open request → the client MUST answer `cancelled`
    permission_scenario("cancel", None, "PERM cancelled").await;
}

/// v5 М1: abort cancels only ITS session's pending permission — another
/// session's open question survives and is answered normally afterwards.
#[tokio::test]
async fn abort_cancels_only_its_own_session_permission() {
    let _env_guard = ENV_LOCK.lock().await;
    let exe = env!("CARGO_BIN_EXE_fake-acp-agent");
    let dir = std::env::temp_dir().join(format!("aic-2sess-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let log = dir.join("perm.log");
    // SAFETY: env for the child; ENV_LOCK serializes env-dependent tests
    unsafe { std::env::set_var("FAKE_ACP_LOG", &log) };

    let cfg = HarnessCfg {
        id: "fakeacp2s".into(),
        name: "fake acp".into(),
        kind: "opencode-acp".into(),
        enabled: true,
        server_url: exe.into(),
        server_password: String::new(),
        workdir: dir.to_string_lossy().into_owned(),
        agent: String::new(),
        auto_approve: false,
        default_model: None,
    };
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<BackendEvent>();
    let b = create_backend(&cfg, tx).unwrap();
    b.connect().await.expect("initialize");

    // two concurrent turns; the titles tell the sent ids apart
    b.send(
        "pending-a",
        "alpha probe",
        &[],
        SendOpts {
            model: None,
            agent: None,
            mcp_role: None,
            cwd: None,
        },
    )
    .await
    .expect("send a");
    b.send(
        "pending-b",
        "beta probe",
        &[],
        SendOpts {
            model: None,
            agent: None,
            mcp_role: None,
            cwd: None,
        },
    )
    .await
    .expect("send b");

    // map real session ids via SessionCreated titles, collect both perms
    let mut real_of: std::collections::HashMap<String, String> = Default::default();
    let mut perms: Vec<(String, String)> = Vec::new(); // (session, request_id)
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    while tokio::time::Instant::now() < deadline && perms.len() < 2 {
        match tokio::time::timeout(Duration::from_millis(300), rx.recv()).await {
            Ok(Some(ev)) => match ev {
                BackendEvent::SessionCreated {
                    native_id, title, ..
                } => {
                    real_of.insert(title, native_id);
                }
                BackendEvent::PermissionRequest {
                    session_id,
                    request_id,
                    ..
                } if !perms.iter().any(|(s, _)| *s == session_id) => {
                    perms.push((session_id, request_id));
                }
                _ => {}
            },
            Ok(None) => break,
            Err(_) => continue,
        }
    }
    let real_a = real_of.get("alpha probe").expect("session a created");
    let real_b = real_of.get("beta probe").expect("session b created");
    assert_eq!(perms.len(), 2, "both permission requests must arrive");

    // abort A while BOTH questions are open — B's must survive
    b.abort("pending-a").await.expect("abort a");
    // now answer B normally
    let (sess_b, req_b) = perms
        .iter()
        .find(|(s, _)| s == real_b)
        .cloned()
        .expect("perm of b");
    assert_eq!(&sess_b, real_b);
    b.permission_reply(&sess_b, &req_b, "once")
        .await
        .expect("b's permission is still answerable after a's abort");

    // wait for both turns to finish
    let mut finished = std::collections::HashSet::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    while tokio::time::Instant::now() < deadline && finished.len() < 2 {
        match tokio::time::timeout(Duration::from_millis(300), rx.recv()).await {
            Ok(Some(BackendEvent::StreamDone { native_id, .. })) => {
                finished.insert(native_id);
            }
            Ok(Some(_)) => {}
            Ok(None) => break,
            Err(_) => continue,
        }
    }
    b.shutdown();
    tokio::time::sleep(Duration::from_millis(200)).await;

    let log = std::fs::read_to_string(&log).unwrap_or_default();
    assert!(
        log.lines()
            .any(|l| l.contains("cancelled") && l.contains(real_a)),
        "a's permission must be cancelled:\n{log}"
    );
    assert!(
        log.lines()
            .any(|l| l.contains("PERM selected allow_once") && l.contains(real_b)),
        "b's permission must survive a's abort and be answered:\n{log}"
    );
    assert!(
        !log.lines()
            .any(|l| l.contains("cancelled") && l.contains(real_b)),
        "b's permission must NOT be cancelled by a's abort (М1):\n{log}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// v5 М2: a malformed permission request is answered with -32602, not
/// silence — the agent's turn still finishes.
#[tokio::test]
async fn malformed_permission_request_gets_32602() {
    let _env_guard = ENV_LOCK.lock().await;
    let exe = env!("CARGO_BIN_EXE_fake-acp-agent");
    let dir = std::env::temp_dir().join(format!("aic-badperm-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let log = dir.join("perm.log");
    // SAFETY: env for the child; ENV_LOCK serializes env-dependent tests
    unsafe { std::env::set_var("FAKE_ACP_LOG", &log) };
    unsafe { std::env::set_var("FAKE_ACP_MALFORMED_PERM", "1") };

    let cfg = HarnessCfg {
        id: "fakeacpbad".into(),
        name: "fake acp".into(),
        kind: "opencode-acp".into(),
        enabled: true,
        server_url: exe.into(),
        server_password: String::new(),
        workdir: dir.to_string_lossy().into_owned(),
        agent: String::new(),
        auto_approve: false,
        default_model: None,
    };
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<BackendEvent>();
    let b = create_backend(&cfg, tx).unwrap();
    b.connect().await.expect("initialize");

    b.send(
        "pending-bad",
        "malformed probe",
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

    let mut done = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    while tokio::time::Instant::now() < deadline && !done {
        match tokio::time::timeout(Duration::from_millis(300), rx.recv()).await {
            Ok(Some(BackendEvent::StreamDone { .. })) => done = true,
            Ok(Some(_)) => {}
            Ok(None) => break,
            Err(_) => continue,
        }
    }
    b.shutdown();
    tokio::time::sleep(Duration::from_millis(200)).await;
    unsafe { std::env::remove_var("FAKE_ACP_MALFORMED_PERM") };

    assert!(done, "the turn must finish after the -32602 reply");
    let log = std::fs::read_to_string(&log).unwrap_or_default();
    assert!(
        log.lines().any(|l| l.contains("MALFORMED -32602")),
        "the malformed request must be answered -32602, not silence:\n{log}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
