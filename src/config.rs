//! Persistent configuration: `%APPDATA%/ai-commander/config.toml` (or XDG on unix).

use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};

use crate::i18n::Lang;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelRef {
    pub provider_id: String,
    pub model_id: String,
}

impl ModelRef {
    pub fn display(&self) -> String {
        format!("{}/{}", self.provider_id, self.model_id)
    }
}

#[derive(Debug, Clone)]
pub struct HarnessCfg {
    /// Short tag shown in UI, e.g. "oc"
    pub id: String,
    /// Display name, e.g. "OpenCode"
    pub name: String,
    /// Backend kind: "opencode" for now; "hermes"/"deepseek-harness" reserved
    pub kind: String,
    pub enabled: bool,
    /// Empty = autostart local server, otherwise connect to this URL
    pub server_url: String,
    /// OPENCODE_SERVER_PASSWORD of the external server (Basic auth, user "opencode")
    pub server_password: String,
    /// Directory the harness server runs in (empty = process cwd)
    pub workdir: String,
    /// Sub-agent name for the harness (opencode: build/plan/…)
    pub agent: String,
    /// Run the harness with unattended tool execution (hermes --yolo).
    /// Off by default: the agent asks before running tools that need it.
    pub auto_approve: bool,
    /// v5 #7: let the dsh backend reuse the DeepSeek key stored by opencode
    /// (~/.local/share/opencode/auth.json). Off by default — another app's
    /// credential store must not be read in secret.
    pub reuse_opencode_key: bool,
    pub default_model: Option<ModelRef>,
}

impl HarnessCfg {
    pub fn opencode_default() -> Self {
        HarnessCfg {
            id: "oc".into(),
            name: "OpenCode".into(),
            kind: "opencode".into(),
            enabled: true,
            server_url: String::new(),
            server_password: String::new(),
            workdir: String::new(),
            agent: "build".into(),
            auto_approve: false,
            reuse_opencode_key: false,
            default_model: None, // picked live from GET /api/model on first Ctrl+K
        }
    }

    pub fn reserved_stub(id: &str, name: &str, kind: &str) -> Self {
        HarnessCfg {
            id: id.into(),
            name: name.into(),
            kind: kind.into(),
            enabled: false,
            server_url: String::new(),
            server_password: String::new(),
            workdir: String::new(),
            agent: String::new(),
            auto_approve: false,
            reuse_opencode_key: false,
            default_model: None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Config {
    pub lang: Lang,
    pub profile_name: String,
    pub harnesses: Vec<HarnessCfg>,
    /// v5 policy trust model: absolute paths of projects whose `allow`
    /// rules are honored. Untrusted project policies may only deny/ask.
    pub trusted_projects: Vec<String>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            lang: Lang::En,
            profile_name: whoami(),
            harnesses: vec![
                HarnessCfg::opencode_default(),
                HarnessCfg::reserved_stub("hermes", "Hermes Agent", "hermes"),
                HarnessCfg::reserved_stub("dsh", "DeepSeek Harness", "deepseek-harness"),
            ],
            trusted_projects: Vec::new(),
        }
    }
}

fn whoami() -> String {
    std::env::var("USERNAME")
        .or_else(|_| std::env::var("USER"))
        .unwrap_or_else(|_| "user".into())
}

pub fn config_dir() -> PathBuf {
    // explicit override first (tests, portable installs, multi-instance)
    if let Some(p) = std::env::var_os("AI_COMMANDER_CONFIG_DIR") {
        let base = PathBuf::from(p);
        let _ = fs::create_dir_all(&base);
        return base;
    }
    let base = dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("ai-commander");
    let _ = fs::create_dir_all(&base);
    base
}

pub fn config_path() -> PathBuf {
    config_dir().join("config.toml")
}

impl Config {
    /// Best-effort load for places that must not fail (policy trust list).
    pub fn load_or_default() -> Config {
        Config::load().unwrap_or_default()
    }

    pub fn load() -> Result<Config> {
        let path = config_path();
        if !path.exists() {
            let cfg = Config::default();
            cfg.save()?;
            return Ok(cfg);
        }
        let raw = fs::read_to_string(&path).context("read config.toml")?;
        let cfg: Config = toml::from_str(&raw).context("parse config.toml")?;
        Ok(cfg)
    }

    pub fn save(&self) -> Result<()> {
        let path = config_path();
        let raw = toml::to_string_pretty(self).context("serialize config")?;
        fs::write(&path, raw).context("write config.toml")?;
        // owner-only permissions (review v2: password in plaintext, 0644 default)
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(&path, fs::Permissions::from_mode(0o600));
        }
        Ok(())
    }

    pub fn harness(&self, id: &str) -> Option<&HarnessCfg> {
        self.harnesses.iter().find(|h| h.id == id)
    }

    pub fn harness_mut(&mut self, id: &str) -> Option<&mut HarnessCfg> {
        self.harnesses.iter_mut().find(|h| h.id == id)
    }

    /// First enabled harness id
    pub fn default_harness_id(&self) -> String {
        self.harnesses
            .iter()
            .find(|h| h.enabled)
            .map(|h| h.id.clone())
            .unwrap_or_else(|| "oc".into())
    }
}

// --- toml (de)serialization mirrors ---

#[derive(serde::Serialize, serde::Deserialize)]
struct ModelToml {
    provider: String,
    model: String,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct HarnessToml {
    id: String,
    name: String,
    kind: String,
    #[serde(default)]
    enabled: bool,
    #[serde(default)]
    server_url: String,
    #[serde(default)]
    server_password: String,
    #[serde(default)]
    workdir: String,
    #[serde(default)]
    agent: String,
    #[serde(default)]
    auto_approve: bool,
    #[serde(default)]
    reuse_opencode_key: bool,
    default_model: Option<ModelToml>,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct ConfigToml {
    lang: String,
    profile_name: String,
    #[serde(default)]
    trusted_projects: Vec<String>,
    #[serde(default)]
    harness: Vec<HarnessToml>,
}

impl serde::Serialize for Config {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let cfg = ConfigToml {
            lang: self.lang.code().into(),
            profile_name: self.profile_name.clone(),
            trusted_projects: self.trusted_projects.clone(),
            harness: self
                .harnesses
                .iter()
                .map(|h| HarnessToml {
                    id: h.id.clone(),
                    name: h.name.clone(),
                    kind: h.kind.clone(),
                    enabled: h.enabled,
                    server_url: h.server_url.clone(),
                    server_password: h.server_password.clone(),
                    workdir: h.workdir.clone(),
                    agent: h.agent.clone(),
                    auto_approve: h.auto_approve,
                    reuse_opencode_key: h.reuse_opencode_key,
                    default_model: h.default_model.as_ref().map(|m| ModelToml {
                        provider: m.provider_id.clone(),
                        model: m.model_id.clone(),
                    }),
                })
                .collect(),
        };
        cfg.serialize(serializer)
    }
}

impl<'de> serde::Deserialize<'de> for Config {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = ConfigToml::deserialize(deserializer)?;
        Ok(Config {
            lang: Lang::parse(&raw.lang),
            profile_name: raw.profile_name,
            trusted_projects: raw.trusted_projects,
            harnesses: raw
                .harness
                .into_iter()
                .map(|h| HarnessCfg {
                    id: h.id,
                    name: h.name,
                    kind: h.kind,
                    enabled: h.enabled,
                    server_url: h.server_url,
                    server_password: h.server_password,
                    workdir: h.workdir,
                    agent: h.agent,
                    auto_approve: h.auto_approve,
                    reuse_opencode_key: h.reuse_opencode_key,
                    default_model: h.default_model.map(|m| ModelRef {
                        provider_id: m.provider,
                        model_id: m.model,
                    }),
                })
                .collect(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_roundtrip() {
        let mut cfg = Config {
            lang: Lang::Ru,
            profile_name: "@tsingular".into(),
            ..Config::default()
        };
        cfg.harness_mut("oc").unwrap().default_model = Some(ModelRef {
            provider_id: "deepseek".into(),
            model_id: "deepseek-chat".into(),
        });
        let raw = toml::to_string_pretty(&cfg).unwrap();
        let back: Config = toml::from_str(&raw).unwrap();
        assert_eq!(back.lang, Lang::Ru);
        assert_eq!(back.profile_name, "@tsingular");
        assert_eq!(
            back.harness("oc")
                .unwrap()
                .default_model
                .as_ref()
                .unwrap()
                .display(),
            "deepseek/deepseek-chat"
        );
        assert!(!back.harness("hermes").unwrap().enabled);
    }
}
