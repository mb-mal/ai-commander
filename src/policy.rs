//! Policy engine: allow / ask / deny rules for harness actions.
//!
//! Rules live in `~/.ai-commander/policy.yaml` and `.ai-commander/policy.yaml`
//! (project level merges on top). Evaluation: among all matching rules the
//! strictest effect wins — deny > ask > allow; no match → ask. Patterns use
//! `*` wildcards matched against the action's primary resource (command,
//! path); an empty pattern matches everything.

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
pub struct Rule {
    /// action name: "bash", "edit", "webfetch", … or "*"
    pub action: String,
    /// wildcard pattern for the resource, e.g. "git push*", "" = any
    #[serde(default)]
    pub pattern: String,
    pub effect: Effect,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PolicyFile {
    #[serde(default)]
    pub rules: Vec<Rule>,
}

#[derive(Debug, Clone, Default)]
pub struct Policy {
    pub rules: Vec<Rule>,
    pub sources: Vec<PathBuf>,
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
                decision = Some(match decision {
                    Some(d) => strictest(d, r.effect),
                    None => r.effect,
                });
            }
        }
        decision.unwrap_or(Effect::Ask)
    }

    pub fn load(cwd: &Path) -> Policy {
        let mut policy = Policy::default();
        let load_one = |path: PathBuf, policy: &mut Policy| {
            let Ok(raw) = std::fs::read_to_string(&path) else {
                return;
            };
            if let Ok(f) = serde_yaml::from_str::<PolicyFile>(&raw) {
                policy.rules.extend(f.rules);
                policy.sources.push(path);
            }
        };
        if let Some(home) = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME")) {
            load_one(
                PathBuf::from(&home)
                    .join(".ai-commander")
                    .join("policy.yaml"),
                &mut policy,
            );
        }
        load_one(cwd.join(".ai-commander").join("policy.yaml"), &mut policy);
        policy
    }
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

    #[test]
    fn glob_basics() {
        assert!(glob_match("git push*", "git push origin master"));
        assert!(glob_match("*", "anything"));
        assert!(glob_match("", ""));
        assert!(!glob_match("git push*", "git pull"));
        assert!(glob_match("READ*", "readme.md"));
    }
}
