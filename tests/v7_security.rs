//! v7 #2 + #3: security (human token not in board.port; config_dir
//! always denied) and hint consistency (no Alt+Enter anywhere).

use ai_commander::policy::{Effect, Policy};

/// v7 #2: the port file does NOT contain the human token (only 2 lines:
/// port and transport token — the agent must not see the human one).
#[test]
fn board_port_has_no_human_token() {
    use std::sync::Arc;
    let board = Arc::new(parking_lot::Mutex::new(
        ai_commander::kanban::store::BoardStore::open_memory(),
    ));
    let handle: ai_commander::kanban::ipc::ToolHandler = Arc::new(move |tool, args, actor| {
        let mut b = board.lock();
        ai_commander::kanban::board_tool_call(tool, args, &actor, &mut b)
    });
    let pf = std::env::temp_dir().join(format!("aic-v7sec-{}", std::process::id()));
    let _ = std::fs::remove_file(&pf);
    ai_commander::kanban::ipc::serve_at(&pf, handle).expect("serve");

    let raw = std::fs::read_to_string(&pf).expect("port file");
    let lines: Vec<&str> = raw.lines().collect();
    assert_eq!(
        lines.len(),
        2,
        "exactly 2 lines (port + transport), got {lines:?}"
    );
    assert!(lines[0].parse::<u16>().is_ok(), "line 1 is the port");
    assert!(!lines[1].is_empty(), "line 2 is the transport token");

    let _ = std::fs::remove_file(&pf);
}

/// v7 #2: requests for the config dir are DENIED regardless of any rules.
#[test]
fn config_dir_is_always_denied() {
    // a policy that allows everything
    let p = Policy {
        rules: vec![ai_commander::policy::Rule {
            action: "*".into(),
            pattern: "".into(),
            effect: Effect::Allow,
        }],
        ..Default::default()
    };
    let config = ai_commander::config::config_dir();
    let config_str = config.to_string_lossy();
    for action in ["read", "edit", "execute", "delete", "move", "write"] {
        assert_eq!(
            p.check(action, &config_str),
            Effect::Deny,
            "{action} on config_dir must be Deny even with allow-all rules"
        );
        assert_eq!(
            p.check(action, &format!("{}/policy.yaml", config_str)),
            Effect::Deny,
            "{action} on policy.yaml must be Deny"
        );
        assert_eq!(
            p.check(action, &format!("{}/board.port", config_str)),
            Effect::Deny,
            "{action} on board.port must be Deny"
        );
    }
    // a path OUTSIDE the config dir is not affected
    assert_eq!(
        p.check("edit", "/tmp/src/lib.rs"),
        Effect::Allow,
        "the deny only covers the config dir"
    );
}

/// v7 #3: no source file or README mentions Alt+Enter (it is the
/// Windows-terminal fullscreen toggle, not a newline).
#[test]
fn no_alt_enter_in_hints() {
    let files = [
        "src/i18n.rs",
        "src/ui/modals.rs",
        "src/ui/editor.rs",
        "README.md",
    ];
    for f in files {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(f);
        let Ok(content) = std::fs::read_to_string(&path) else {
            continue;
        };
        assert!(
            !content.contains("Alt+Enter"),
            "{f} still mentions Alt+Enter (it is fullscreen in Windows)"
        );
    }
}
