//! К1 + К2 acceptance: the ACP backend surfaces usage_update as `Usage`
//! with the same numbers, and the RFD compaction stream (in_progress →
//! chunks → completed) arrives as `Compaction` events with ONE stable
//! compaction id.

use std::time::Duration;

use ai_commander::backend::{
    BackendEvent, CompactionSource, CompactionStatus, SendOpts, create_backend,
};
use ai_commander::config::HarnessCfg;

#[tokio::test]
async fn acp_usage_and_compaction_stream_through() {
    let exe = env!("CARGO_BIN_EXE_fake-acp-agent");
    let dir = std::env::temp_dir().join(format!("aic-uc-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let log = dir.join("uc.log");
    // SAFETY: env for the child; serialized with the other env tests by
    // running this as the only test in the binary
    unsafe {
        std::env::set_var("FAKE_ACP_LOG", &log);
        std::env::set_var("FAKE_ACP_COMPACTION", "1");
    }

    let cfg = HarnessCfg {
        id: "fakeuc".into(),
        name: "fake acp".into(),
        kind: "opencode-acp".into(),
        enabled: true,
        server_url: exe.into(),
        server_password: String::new(),
        workdir: dir.to_string_lossy().into_owned(),
        agent: String::new(),
        auto_approve: false,
        reuse_opencode_key: false,
        default_model: None,
    };
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<BackendEvent>();
    let b = create_backend(&cfg, tx).unwrap();
    b.connect().await.expect("initialize");

    b.send(
        "pending-uc",
        "hello context",
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

    let mut usages: Vec<(u64, u64)> = Vec::new();
    let mut compactions: Vec<(String, CompactionStatus, Option<String>)> = Vec::new();
    let mut done = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    while tokio::time::Instant::now() < deadline && !done {
        match tokio::time::timeout(Duration::from_millis(300), rx.recv()).await {
            Ok(Some(ev)) => match ev {
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
                BackendEvent::Usage {
                    used,
                    size,
                    estimated,
                    ..
                } => {
                    assert!(!estimated, "ACP usage is reported, not estimated");
                    usages.push((used, size));
                }
                BackendEvent::Compaction {
                    compaction_id,
                    status,
                    summary_delta,
                    source,
                    ..
                } => {
                    assert_eq!(source, CompactionSource::Reported);
                    compactions.push((compaction_id, status, summary_delta));
                }
                BackendEvent::StreamDone { .. } => {
                    // the compaction stream follows the turn; keep draining
                    // a little longer
                    if !compactions.is_empty() {
                        done = true;
                    }
                }
                BackendEvent::StreamError { error, .. } => panic!("StreamError: {error}"),
                _ => {}
            },
            Ok(None) => break,
            Err(_) => continue,
        }
    }
    b.shutdown();
    unsafe { std::env::remove_var("FAKE_ACP_COMPACTION") };

    // К1: usage with the exact reported numbers (before and after)
    assert!(usages.contains(&(53_000, 200_000)), "usages: {usages:?}");
    assert!(usages.contains(&(41_000, 200_000)), "usages: {usages:?}");

    // К2: in_progress → 3 chunks → completed, ONE compaction id
    assert_eq!(compactions.len(), 6, "events: {compactions:?}");
    assert!(
        compactions.iter().all(|(id, ..)| id == "c-0042"),
        "one stable compaction id: {compactions:?}"
    );
    assert_eq!(compactions[0].1, CompactionStatus::InProgress);
    let chunks: Vec<&str> = compactions[1..5]
        .iter()
        .map(|(_, _, d)| d.as_deref().unwrap_or(""))
        .collect();
    assert_eq!(
        chunks.concat(),
        "Summarizing the old context...",
        "summary chunks stream in order"
    );
    assert_eq!(compactions[5].1, CompactionStatus::Completed);
    let _ = std::fs::remove_dir_all(&dir);
}
