//! Agent roles, declared as files (git-friendly, Claude-Code-style):
//!
//!   .ai-commander/agents/<id>.md   — YAML frontmatter + markdown instructions
//!
//! Teams (.ai-commander/teams) are NOT implemented yet — the loader ignores
//! them (README marks them as planned; no dead surface).

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

#[derive(Debug, Clone, Default)]
pub struct RoleSet {
    pub roles: BTreeMap<String, Role>,
    /// paths that contributed (for UI/debug)
    pub sources: Vec<PathBuf>,
    /// v5 #6: files that failed to load (bad YAML, unknown fields) — shown
    /// in the agents panel and the daemon log instead of vanishing silently
    pub errors: Vec<LoadError>,
}

/// A role/team file that could not be loaded.
#[derive(Debug, Clone)]
pub struct LoadError {
    pub path: PathBuf,
    pub message: String,
}

#[derive(Debug, Deserialize)]
struct Frontmatter {
    #[serde(flatten)]
    role: Role,
}
// Role itself: deny_unknown_fields via a strict mirror (flatten + deny
// conflict, so the strictness lives here)
impl Frontmatter {
    fn parse(fm: &str) -> std::result::Result<Role, String> {
        #[derive(Deserialize, Default)]
        #[serde(deny_unknown_fields, default)]
        struct StrictRole {
            name: String,
            harness: String,
            model: Option<String>,
            autonomy: Option<String>,
        }
        // strict pass first: catches typos like `modle:` with the field name
        let strict: std::result::Result<StrictRole, _> = serde_norway::from_str(fm);
        if let Err(e) = strict {
            return Err(format!("{e}"));
        }
        let parsed: std::result::Result<Frontmatter, _> = serde_norway::from_str(fm);
        parsed.map(|f| f.role).map_err(|e| format!("{e}"))
    }
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

pub fn load_roles_dir_into(dir: &Path, into: &mut RoleSet) {
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
        let raw = match std::fs::read_to_string(&p) {
            Ok(r) => r,
            Err(e) => {
                into.errors.push(LoadError {
                    path: p,
                    message: format!("read: {e}"),
                });
                continue;
            }
        };
        let (fm, body) = match split_frontmatter(&raw) {
            Ok(v) => v,
            Err(e) => {
                into.errors.push(LoadError {
                    path: p,
                    message: format!("{e:#}"),
                });
                continue;
            }
        };
        match Frontmatter::parse(&fm) {
            Ok(mut role) => {
                role.instructions = body;
                into.roles.insert(id, role); // project level overwrites user level
            }
            Err(e) => into.errors.push(LoadError {
                path: p,
                message: e,
            }),
        }
    }
}

impl RoleSet {
    pub fn load(cwd: &Path) -> RoleSet {
        let mut rs = RoleSet::default();
        // user level
        if let Some(home) = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME")) {
            let base = PathBuf::from(&home).join(".ai-commander");
            load_roles_dir_into(&base.join("agents"), &mut rs);
            rs.sources.push(base);
        }
        // project level (wins)
        let proj = cwd.join(".ai-commander");
        load_roles_dir_into(&proj.join("agents"), &mut rs);
        rs.sources.push(proj);
        rs
    }

    /// Д7: cached load — the role/team files are re-read only when their
    /// (path, mtime, len) fingerprint changes, not on every task launch.
    /// Returns the process-wide cached view for `cwd`.
    pub fn cached(cwd: &Path) -> RoleSet {
        static CACHE: std::sync::OnceLock<RoleCache> = std::sync::OnceLock::new();
        CACHE.get_or_init(RoleCache::new).get(cwd)
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

/// Д7: cache of the loaded RoleSet, keyed by a cheap filesystem fingerprint
/// (sorted list of (path, mtime, len) for every role/team file). A hit
/// avoids re-reading and re-parsing the .md/.yaml files per task launch.
#[derive(Debug, Default)]
pub struct RoleCache {
    inner: std::sync::Mutex<Option<(Fingerprint, RoleSet)>>,
    /// full reloads performed (for tests/diagnostics)
    pub loads: std::sync::atomic::AtomicUsize,
}

type Fingerprint = Vec<(PathBuf, std::time::SystemTime, u64)>;

/// Snapshot every role/team source file under the user + project roots.
fn fingerprint(cwd: &Path) -> Fingerprint {
    let mut roots: Vec<PathBuf> = Vec::new();
    if let Some(home) = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME")) {
        roots.push(PathBuf::from(home).join(".ai-commander"));
    }
    roots.push(cwd.join(".ai-commander"));
    let mut out = Vec::new();
    for base in roots {
        for sub in ["agents"] {
            let dir = base.join(sub);
            let Ok(rd) = std::fs::read_dir(&dir) else {
                continue;
            };
            for e in rd.flatten() {
                if let Ok(md) = e.metadata() {
                    if let Ok(mtime) = md.modified() {
                        out.push((e.path(), mtime, md.len()));
                    }
                }
            }
        }
    }
    out.sort();
    out
}

impl RoleCache {
    pub fn new() -> RoleCache {
        RoleCache::default()
    }

    /// Return the cached RoleSet, reloading only when the fingerprint changed.
    pub fn get(&self, cwd: &Path) -> RoleSet {
        let fp = fingerprint(cwd);
        let mut g = self.inner.lock().unwrap();
        if let Some((prev, rs)) = g.as_ref() {
            if *prev == fp {
                return rs.clone();
            }
        }
        let rs = RoleSet::load(cwd);
        self.loads.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        *g = Some((fp, rs.clone()));
        rs
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
        let parsed: Frontmatter = serde_norway::from_str(&fm).unwrap();
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
        load_roles_dir_into(&dir.join(".ai-commander").join("agents"), &mut rs);
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
        load_roles_dir_into(&agents, &mut rs);
        assert_eq!(rs.roles.len(), 1);
        assert_eq!(rs.roles["coder"].instructions, "Write code fast.");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Д7: repeated `get` calls hit the cache (no extra loads); a file edit
    /// bumps the mtime fingerprint and the change IS picked up.
    #[test]
    fn role_cache_hits_and_refreshes_on_change() {
        let dir = std::env::temp_dir().join(format!("aic-role-cache-{}", std::process::id()));
        let agents = dir.join(".ai-commander").join("agents");
        std::fs::create_dir_all(&agents).unwrap();
        let role = agents.join("coder.md");
        std::fs::write(&role, "---\nname: Coder\nharness: oc\n---\nOriginal text.").unwrap();

        let cache = RoleCache::new();
        let rs = cache.get(&dir);
        assert_eq!(rs.roles["coder"].instructions, "Original text.");
        assert_eq!(cache.loads.load(std::sync::atomic::Ordering::SeqCst), 1);

        // second call with no changes: served from cache, no reload
        let rs = cache.get(&dir);
        assert_eq!(rs.roles["coder"].instructions, "Original text.");
        assert_eq!(
            cache.loads.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "unchanged files must not trigger a reload (Д7)"
        );

        // edit the role file and force a distinguishable mtime
        std::fs::write(&role, "---\nname: Coder\nharness: oc\n---\nUpdated text.").unwrap();
        let later = std::time::SystemTime::now() + std::time::Duration::from_secs(1);
        let f = std::fs::File::options().write(true).open(&role).unwrap();
        f.set_times(
            std::fs::FileTimes::new()
                .set_accessed(later)
                .set_modified(later),
        )
        .unwrap();
        drop(f);

        let rs = cache.get(&dir);
        assert_eq!(
            rs.roles["coder"].instructions, "Updated text.",
            "edited role must be picked up without restart"
        );
        assert_eq!(cache.loads.load(std::sync::atomic::Ordering::SeqCst), 2);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
