//! v5 #7: the dsh backend must NOT read another app's credential store
//! (opencode auth.json) without explicit consent (reuse_opencode_key).

use ai_commander::backend::create_backend;
use ai_commander::config::HarnessCfg;

fn cfg(reuse: bool) -> HarnessCfg {
    HarnessCfg {
        id: "dsh-test".into(),
        name: "dsh".into(),
        kind: "deepseek-harness".into(),
        enabled: true,
        server_url: String::new(),
        server_password: String::new(),
        workdir: String::new(),
        agent: String::new(),
        auto_approve: false,
        reuse_opencode_key: reuse,
        default_model: None,
    }
}

#[tokio::test]
async fn opencode_key_store_is_not_read_without_consent() {
    // SAFETY: env overrides; HOME/USERPROFILE point at a fake home that
    // contains a juicy opencode auth.json
    let home = std::env::temp_dir().join(format!("aic-dsh-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    let oc = home.join(".local").join("share").join("opencode");
    std::fs::create_dir_all(&oc).unwrap();
    std::fs::write(
        oc.join("auth.json"),
        r#"{"deepseek": {"key": "sk-secret-from-opencode"}}"#,
    )
    .unwrap();
    unsafe {
        std::env::set_var("HOME", &home);
        std::env::set_var("USERPROFILE", &home);
        std::env::remove_var("DEEPSEEK_API_KEY");
    }

    // no consent → connect must fail with the hint, NOT silently use the key
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let b = create_backend(&cfg(false), tx).unwrap();
    let err = b.connect().await.unwrap_err().to_string();
    assert!(
        err.contains("DEEPSEEK_API_KEY") && err.contains("reuse_opencode_key"),
        "the error must tell how to fix it: {err}"
    );

    // explicit consent → the reused key works (connect succeeds)
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let b = create_backend(&cfg(true), tx).unwrap();
    b.connect().await.expect("consent reuses the opencode key");

    // env var wins over everything
    unsafe { std::env::set_var("DEEPSEEK_API_KEY", "sk-from-env") };
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let b = create_backend(&cfg(false), tx).unwrap();
    b.connect().await.expect("env key works without consent");

    unsafe { std::env::remove_var("DEEPSEEK_API_KEY") };
    let _ = std::fs::remove_dir_all(&home);
}
