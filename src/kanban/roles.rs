//! Agent roles and teams, declared as files (git-friendly, Claude-Code-style):
//!
//!   .ai-commander/agents/<id>.md   — YAML frontmatter + markdown instructions
//!   .ai-commander/teams/<id>.yaml  — a squad of roles
//!
//! A role binds a harness + model + system instructions to a task; the task
//! prompt is prefixed with the role's instruction body. User level
//! (`~/.ai-commander`) and project level (`.<cwd>/.ai-commander`) are merged,
//! project wins on id conflicts.

use anyhow::{Result, anyhow, bail};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Role {
    /// display name
    pub name: String,
    /// harness id from config (oc / hermes / dsh / ocacp …)
    pub harness: String,
    /// provider/model like "deepseek/deepseek-flash" (optional: harness default)
    pub model: Option<String>,
    /// autonomy hint surfaced in the prompt
    pub autonomy: Option<String>,
    /// markdown instructions (the .md body)
    #[serde(skip)]
    pub instructions: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Team {
    pub name: String,
    pub lead: String,
    pub members: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub struct RoleSet {
    pub roles: BTreeMap<String, Role>,
    pub teams: BTreeMap<String, Team>,
    /// paths that contributed (for UI/debug)
    pub sources: Vec<PathBuf>,
}

#[derive(Debug, Deserialize)]
struct Frontmatter {
    #[serde(flatten)]
    role: Role,
}

/// Split a `.md` file into (yaml frontmatter, markdown body).
fn split_frontmatter(raw: &str) -> Result<(String, String)> {
    let t = raw.trim_start();
    if let Some(rest) = t.strip_prefix("---") {
        if let Some(end) = rest.find("\n---") {
            let fm = rest[..end].to_string();
            let body = rest[end + 4..].trim_start_matches('\n').to_string();
            return Ok((fm, body));
        }
    }
    Err(anyhow!("no YAML frontmatter (--- … ---) found"))
}

fn load_roles_dir(dir: &Path, into: &mut RoleSet) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let p = e.path();
        if p.extension().and_then(|x| x.to_str()) != Some("md") {
            continue;
        }
        let Some(id) = p.file_stem().and_then(|s| s.to_str()).map(str::to_owned) else {
            continue;
        };
        let Ok(raw) = std::fs::read_to_string(&p) else {
            continue;
        };
        let Ok((fm, body)) = split_frontmatter(&raw) else {
            continue;
        };
        let Ok(parsed) = serde_yaml::from_str::<Frontmatter>(&fm) else {
            continue;
        };
        let mut role = parsed.role;
        role.instructions = body;
        into.roles.insert(id, role); // project level overwrites user level
    }
}

fn load_teams_dir(dir: &Path, into: &mut RoleSet) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let p = e.path();
        if p.extension().and_then(|x| x.to_str()) != Some("yaml") {
            continue;
        }
        let Some(id) = p.file_stem().and_then(|s| s.to_str()).map(str::to_owned) else {
            continue;
        };
        let Ok(raw) = std::fs::read_to_string(&p) else {
            continue;
        };
        if let Ok(team) = serde_yaml::from_str::<Team>(&raw) {
            into.teams.insert(id, team);
        }
    }
}

impl RoleSet {
    pub fn load(cwd: &Path) -> RoleSet {
        let mut rs = RoleSet::default();
        // user level
        if let Some(home) = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME")) {
            let base = PathBuf::from(&home).join(".ai-commander");
            load_roles_dir(&base.join("agents"), &mut rs);
            load_teams_dir(&base.join("teams"), &mut rs);
            rs.sources.push(base);
        }
        // project level (wins)
        let proj = cwd.join(".ai-commander");
        load_roles_dir(&proj.join("agents"), &mut rs);
        load_teams_dir(&proj.join("teams"), &mut rs);
        rs.sources.push(proj);
        rs
    }

    /// Prefix prepended to the task prompt when a role executes the task.
    pub fn role_prompt(&self, role_id: &str) -> Option<String> {
        let role = self.roles.get(role_id)?;
        let mut p = format!("# Role: {} ({})\n", role.name, role_id);
        if let Some(a) = &role.autonomy {
            p.push_str(&format!("Autonomy: {a}.\n"));
        }
        if !role.instructions.trim().is_empty() {
            p.push('\n');
            p.push_str(role.instructions.trim_end());
            p.push_str("\n\n---\n\n");
        }
        Some(p)
    }
}

/// Role templates for the "new role" wizard (F-key board UI).
pub fn template_body(kind: &str) -> &'static str {
    match kind {
        "coder" => {
            "You write clean, idiomatic code. Prefer small diffs, run the tests before DONE."
        }
        "reviewer" => {
            "You review code for correctness and safety. Focus on unsafe blocks, error handling and tests; approve only what you would ship."
        }
        "planner" => {
            "You break work into small tasks with acceptance criteria. Never implement yourself — create board tasks with depends_on."
        }
        "tester" => {
            "You write tests that fail before the fix and pass after. Cover edge cases and error paths."
        }
        _ => "You are a helpful project agent.",
    }
}

/// Create `.ai-commander/agents/<kind>.md` from a template (project level).
pub fn create_from_template(kind: &str) -> Result<PathBuf> {
    let cwd = std::env::current_dir().unwrap_or_default();
    let dir = cwd.join(".ai-commander").join("agents");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{kind}.md"));
    if path.exists() {
        bail!("role file already exists: {}", path.display());
    }
    let name = match kind {
        "coder" => "Coder",
        "reviewer" => "Code Reviewer",
        "planner" => "Planner",
        "tester" => "Tester",
        other => other,
    };
    let body = format!(
        "---
name: {name}
harness: oc
autonomy: ask_first
---
{}
",
        template_body(kind)
    );
    std::fs::write(&path, body)?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frontmatter_split_and_parse() {
        let raw = "---\nname: Code Reviewer\nharness: oc\nmodel: deepseek/deepseek-flash\nautonomy: ask_first\n---\nYou review Rust code.\nFocus on unsafe.";
        let (fm, body) = split_frontmatter(raw).unwrap();
        let parsed: Frontmatter = serde_yaml::from_str(&fm).unwrap();
        assert_eq!(parsed.role.name, "Code Reviewer");
        assert_eq!(parsed.role.harness, "oc");
        assert_eq!(
            parsed.role.model.as_deref(),
            Some("deepseek/deepseek-flash")
        );
        assert!(body.starts_with("You review Rust code."));
    }

    #[test]
    fn role_prompt_contains_instructions() {
        let mut rs = RoleSet::default();
        rs.roles.insert(
            "reviewer".into(),
            Role {
                name: "Reviewer".into(),
                harness: "oc".into(),
                model: None,
                autonomy: Some("ask_first".into()),
                instructions: "Check unsafe blocks.".into(),
            },
        );
        let p = rs.role_prompt("reviewer").unwrap();
        assert!(p.contains("Role: Reviewer"));
        assert!(p.contains("Check unsafe blocks."));
        assert!(p.contains("Autonomy"));
        assert!(rs.role_prompt("nope").is_none());
    }

    #[test]
    fn template_creates_loadable_role() {
        let dir = std::env::temp_dir().join(format!("aic-role-tpl-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let prev = std::env::current_dir().unwrap();
        std::env::set_current_dir(&dir).unwrap();
        let path = create_from_template("reviewer").unwrap();
        assert!(path.exists());
        assert!(
            create_from_template("reviewer").is_err(),
            "second create must fail"
        );
        let mut rs = RoleSet::default();
        load_roles_dir(&dir.join(".ai-commander").join("agents"), &mut rs);
        assert_eq!(rs.roles["reviewer"].name, "Code Reviewer");
        assert!(rs.roles["reviewer"].instructions.contains("unsafe"));
        std::env::set_current_dir(prev).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn dirs_load_and_project_wins() {
        let dir = std::env::temp_dir().join(format!("aic-roles-{}", std::process::id()));
        let agents = dir.join("agents");
        std::fs::create_dir_all(&agents).unwrap();
        std::fs::write(
            agents.join("coder.md"),
            "---\nname: Coder\nharness: oc\n---\nWrite code fast.",
        )
        .unwrap();
        let mut rs = RoleSet::default();
        load_roles_dir(&agents, &mut rs);
        assert_eq!(rs.roles.len(), 1);
        assert_eq!(rs.roles["coder"].instructions, "Write code fast.");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
