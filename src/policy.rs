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

    pub fn load(cwd: &Path) -> Policy {
        let mut user_rules: Vec<Rule> = Vec::new();
        let mut proj_rules: Vec<Rule> = Vec::new();
        let mut sources = Vec::new();
        let read = |path: PathBuf| -> Option<Vec<Rule>> {
            let raw = std::fs::read_to_string(&path).ok()?;
            let f = serde_yaml::from_str::<PolicyFile>(&raw).ok()?;
            Some(f.rules)
        };
        if let Some(home) = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME")) {
            let p = PathBuf::from(&home)
                .join(".ai-commander")
                .join("policy.yaml");
            if let Some(rules) = read(p.clone()) {
                user_rules = rules;
                sources.push(p);
            }
        }
        let pp = cwd.join(".ai-commander").join("policy.yaml");
        if let Some(rules) = read(pp.clone()) {
            proj_rules = rules;
            sources.push(pp);
        }
        let rules = Self::merge_tighten(user_rules, proj_rules);
        Policy { rules, sources }
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
        };
        assert_eq!(p.check("edit", "prod.env"), Effect::Deny);
        // unrelated project rule — kept
        let rules = Policy::merge_tighten(
            vec![mk("bash", "", Effect::Ask)],
            vec![mk("webfetch", "", Effect::Allow)],
        );
        let p = Policy {
            rules,
            sources: Vec::new(),
        };
        assert_eq!(p.check("webfetch", "x"), Effect::Allow);
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
