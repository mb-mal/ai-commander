//! Policy engine: allow / ask / deny rules for harness actions.
//!
//! Rules live in `~/.ai-commander/policy.yaml` and `.ai-commander/policy.yaml`
//! (project level merges on top). Evaluation: among all matching rules the
//! strictest effect wins — deny > ask > allow; no match → ask. Patterns use
//! `*` wildcards matched against the action's primary resource (command,
//! path); an empty pattern matches everything.
//!
//! v5 trust model: a checked-out repository is NOT trusted by default, so
//! its policy may only TIGHTEN (deny/ask). Project `allow` rules apply only
//! when the project is listed in the user's `trusted_projects`, and a
//! project `action: "*"` with `allow` is rejected outright — otherwise a
//! cloned repo could enable auto-approval of everything an agent asks.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Effect {
    Allow,
    Ask,
    Deny,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Rule {
    /// action name: "bash", "edit", "webfetch", … or "*"
    pub action: String,
    /// wildcard pattern for the resource, e.g. "git push*", "" = any
    #[serde(default)]
    pub pattern: String,
    pub effect: Effect,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PolicyFile {
    #[serde(default)]
    pub rules: Vec<Rule>,
}

#[derive(Debug, Clone, Default)]
pub struct Policy {
    pub rules: Vec<Rule>,
    pub sources: Vec<PathBuf>,
    /// non-fatal problems (ignored project policy, dropped rules) —
    /// surfaced by the daemon/TUI at startup
    pub warnings: Vec<String>,
}

fn strictest(a: Effect, b: Effect) -> Effect {
    use Effect::*;
    match (a, b) {
        (Deny, _) | (_, Deny) => Deny,
        (Ask, _) | (_, Ask) => Ask,
        _ => Allow,
    }
}

/// Simple `*` wildcard matcher (case-insensitive).
pub fn glob_match(pattern: &str, text: &str) -> bool {
    let (p, t) = (pattern.to_lowercase(), text.to_lowercase());
    fn rec(p: &[char], t: &[char]) -> bool {
        if p.is_empty() {
            return t.is_empty();
        }
        if p[0] == '*' {
            // collapse consecutive stars
            let rest = &p[1..];
            if rest.is_empty() {
                return true;
            }
            for i in 0..=t.len() {
                if rec(rest, &t[i..]) {
                    return true;
                }
            }
            false
        } else if t.is_empty() {
            false
        } else if p[0] == t[0] {
            rec(&p[1..], &t[1..])
        } else {
            false
        }
    }
    let pv: Vec<char> = p.chars().collect();
    let tv: Vec<char> = t.chars().collect();
    rec(&pv, &tv)
}

impl Policy {
    pub fn check(&self, action: &str, resource: &str) -> Effect {
        let mut decision: Option<Effect> = None;
        for r in &self.rules {
            let action_ok = r.action == "*" || r.action.eq_ignore_ascii_case(action);
            let resource_ok = r.pattern.is_empty() || glob_match(&r.pattern, resource);
            if action_ok && resource_ok {
                // v5: an unidentified action (`other`) is never auto-allowed
                // — allow rules skip it, ask/deny still apply
                if action == "other" && r.effect == Effect::Allow {
                    continue;
                }
                decision = Some(match decision {
                    Some(d) => strictest(d, r.effect),
                    None => r.effect,
                });
            }
        }
        decision.unwrap_or(Effect::Ask)
    }

    /// Project policies may only tighten (deny > ask > allow), never weaken
    /// a user-level rule on the same target (review v2 security #7).
    fn merge_tighten(user: Vec<Rule>, project: Vec<Rule>) -> Vec<Rule> {
        let rank = |e: Effect| match e {
            Effect::Deny => 3,
            Effect::Ask => 2,
            Effect::Allow => 1,
        };
        let mut rules = user;
        for pr in project {
            let weakens = rules.iter().any(|ur| {
                let same_target = ur.action == pr.action || ur.action == "*" || pr.action == "*";
                let same_resource = ur.pattern.is_empty()
                    || pr.pattern.is_empty()
                    || glob_match(&ur.pattern, &pr.pattern)
                    || glob_match(&pr.pattern, &ur.pattern);
                same_target && same_resource && rank(ur.effect) > rank(pr.effect)
            });
            if !weakens {
                rules.push(pr);
            }
        }
        rules
    }

    /// Load user + project policies. A broken USER policy is a hard error
    /// (the caller must refuse to run — silently dropping the user's own
    /// `deny` rules is a security hole); a broken PROJECT policy is ignored
    /// with a warning (a cloned repo must not brick the tool).
    pub fn load_checked(cwd: &Path) -> Result<Policy, String> {
        let mut warnings = Vec::new();
        let mut user_rules: Vec<Rule> = Vec::new();
        let mut proj_rules: Vec<Rule> = Vec::new();
        let mut sources = Vec::new();

        if let Some(home) = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME")) {
            let p = PathBuf::from(&home)
                .join(".ai-commander")
                .join("policy.yaml");
            if let Ok(raw) = std::fs::read_to_string(&p) {
                match serde_yaml::from_str::<PolicyFile>(&raw) {
                    Ok(f) => {
                        user_rules = f.rules;
                        sources.push(p);
                    }
                    Err(e) => return Err(format!("{}: {e}", p.display())),
                }
            }
        }
        let pp = cwd.join(".ai-commander").join("policy.yaml");
        if let Ok(raw) = std::fs::read_to_string(&pp) {
            match serde_yaml::from_str::<PolicyFile>(&raw) {
                Ok(f) => {
                    proj_rules = f.rules;
                    sources.push(pp);
                }
                Err(e) => warnings.push(format!(
                    "ignoring invalid project policy {}: {e}",
                    pp.display()
                )),
            }
        }

        // v5 trust model for project allow rules
        let trusted = trusted_projects()
            .iter()
            .any(|t| Path::new(t) == cwd || same_path(t, cwd));
        let mut kept_project = Vec::new();
        for r in proj_rules {
            if r.effect == Effect::Allow && r.action == "*" {
                warnings.push(
                    "project policy: `action: \"*\"` with allow is never applied \
                     (add explicit action rules)"
                        .into(),
                );
                continue;
            }
            if r.effect == Effect::Allow && !trusted {
                warnings.push(format!(
                    "project policy: allow rules of {} are ignored — the project is not in \
                     trusted_projects (project rules may only deny/ask)",
                    cwd.display()
                ));
                continue;
            }
            kept_project.push(r);
        }

        let rules = Self::merge_tighten(user_rules, kept_project);
        Ok(Policy {
            rules,
            sources,
            warnings,
        })
    }

    /// Legacy lenient load for UI paths: a broken user policy degrades to
    /// "ask everything" with a logged warning instead of blocking.
    pub fn load(cwd: &Path) -> Policy {
        match Self::load_checked(cwd) {
            Ok(p) => p,
            Err(e) => {
                crate::log_warn!("policy", "user policy invalid, defaulting to ask: {e}");
                Policy::default()
            }
        }
    }
}

fn same_path(a: &str, b: &Path) -> bool {
    let pa = std::fs::canonicalize(a).unwrap_or_else(|_| PathBuf::from(a));
    let pb = std::fs::canonicalize(b).unwrap_or_else(|_| b.to_path_buf());
    pa == pb
}

/// Projects the user explicitly trusts (their `allow` rules apply).
/// Listed in the user config as `trusted_projects = ["/abs/path"]`.
fn trusted_projects() -> Vec<String> {
    crate::config::Config::load_or_default().trusted_projects
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(rules: &[(&str, &str, Effect)]) -> Policy {
        Policy {
            rules: rules
                .iter()
                .map(|(a, p, e)| Rule {
                    action: (*a).into(),
                    pattern: (*p).into(),
                    effect: *e,
                })
                .collect(),
            sources: Vec::new(),
            warnings: Vec::new(),
        }
    }

    #[test]
    fn strictest_effect_wins() {
        let p = policy(&[
            ("bash", "", Effect::Allow),
            ("bash", "git push*", Effect::Deny),
        ]);
        assert_eq!(p.check("bash", "npm test"), Effect::Allow);
        assert_eq!(p.check("bash", "git push origin main"), Effect::Deny);
        assert_eq!(
            p.check("edit", "src/main.rs"),
            Effect::Ask,
            "unmatched -> ask"
        );
    }

    #[test]
    fn wildcards_and_star_action() {
        let p = policy(&[
            ("*", "*.env", Effect::Deny),
            ("edit", "src/*", Effect::Allow),
        ]);
        assert_eq!(p.check("edit", ".env"), Effect::Deny);
        assert_eq!(p.check("bash", "cat prod.env"), Effect::Deny);
        assert_eq!(p.check("edit", "src/lib.rs"), Effect::Allow);
        assert_eq!(p.check("edit", "docs/lib.rs"), Effect::Ask);
    }

    /// v5: an unidentified action (`other`) is never auto-allowed, even by
    /// `allow *` — only ask/deny apply to it.
    #[test]
    fn other_action_is_never_auto_allowed() {
        let p = policy(&[("*", "", Effect::Allow)]);
        assert_eq!(p.check("edit", "src/lib.rs"), Effect::Allow);
        assert_eq!(p.check("other", "mystery"), Effect::Ask);
        let p2 = policy(&[("other", "", Effect::Allow), ("*", "", Effect::Deny)]);
        assert_eq!(p2.check("other", "x"), Effect::Deny);
    }

    #[test]
    fn project_policies_only_tighten() {
        let mk = |a: &str, p: &str, e: Effect| Rule {
            action: a.into(),
            pattern: p.into(),
            effect: e,
        };
        // project allow must not weaken user deny
        let rules = Policy::merge_tighten(
            vec![mk("bash", "git push*", Effect::Deny)],
            vec![mk("bash", "", Effect::Allow)],
        );
        let p = Policy {
            rules,
            sources: Vec::new(),
            warnings: Vec::new(),
        };
        assert_eq!(p.check("bash", "git push origin"), Effect::Deny);
        // project deny on top of user ask — kept
        let rules = Policy::merge_tighten(
            vec![mk("edit", "", Effect::Ask)],
            vec![mk("edit", "*.env", Effect::Deny)],
        );
        let p = Policy {
            rules,
            sources: Vec::new(),
            warnings: Vec::new(),
        };
        assert_eq!(p.check("edit", "prod.env"), Effect::Deny);
    }

    #[test]
    fn glob_basics() {
        assert!(glob_match("git push*", "git push origin master"));
        assert!(glob_match("*", "anything"));
        assert!(glob_match("", ""));
        assert!(!glob_match("git push*", "git pull"));
        assert!(glob_match("READ*", "readme.md"));
    }
}
