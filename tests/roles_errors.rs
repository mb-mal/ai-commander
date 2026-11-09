//! v5 #6: role files with errors are VISIBLE (path + message), never
//! silently dropped; the rest of the roles still load. serde_yaml is
//! replaced by the maintained serde_norway fork.

use ai_commander::kanban::roles::RoleSet;

fn dir_with(tag: &str, files: &[(&str, &str)]) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("aic-roles-err-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    for (name, body) in files {
        std::fs::write(d.join(name), body).unwrap();
    }
    d
}

#[test]
fn role_typo_is_reported_with_field_and_path() {
    let d = dir_with(
        "typo",
        &[(
            "typo.md",
            "---\nname: Coder\nharness: oc\nmodle: deepseek/deepseek-flash\n---\nWrite code.",
        )],
    );
    let mut rs = RoleSet::default();
    ai_commander::kanban::roles::load_roles_dir_into(&d, &mut rs);
    assert!(rs.roles.is_empty(), "the broken role must not load");
    assert_eq!(rs.errors.len(), 1);
    let e = &rs.errors[0];
    assert!(
        e.path.to_string_lossy().contains("typo.md"),
        "the error names the file: {}",
        e.path.display()
    );
    assert!(
        e.message.contains("modle"),
        "the error names the unknown field: {}",
        e.message
    );
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn syntax_error_reports_line_and_other_roles_load() {
    let d = dir_with(
        "syntax",
        &[
            ("broken.md", "---\nname: [unclosed\nharness: oc\n---\nbody"),
            ("good.md", "---\nname: Good\nharness: oc\n---\nWorks."),
        ],
    );
    let mut rs = RoleSet::default();
    ai_commander::kanban::roles::load_roles_dir_into(&d, &mut rs);
    assert!(rs.roles.contains_key("good"), "the healthy role loads");
    assert_eq!(rs.errors.len(), 1);
    let e = &rs.errors[0];
    assert!(
        e.path.to_string_lossy().contains("broken.md"),
        "names the file"
    );
    assert!(
        e.message.contains(':'),
        "carries a position (line/column): {}",
        e.message
    );
    let _ = std::fs::remove_dir_all(&d);
}
