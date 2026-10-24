use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::PathBuf;

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    pub server: String,
    pub port: u16,
    pub remote_base: String,
    pub backup_root: String,
    #[serde(default)]
    pub discord_webhook: String,
    #[serde(default = "yes")]
    pub discord_daily_ok: bool,
    #[serde(default)]
    pub restore_allowed: Vec<String>,
}

impl Config {
    pub fn path() -> PathBuf {
        if let Ok(p) = std::env::var("MPBACKUP_CONFIG") {
            return PathBuf::from(p);
        }
        home().join(".config/mpbackup/config.toml")
    }

    pub fn load() -> Result<Config> {
        let path = Config::path();
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("cannot read config {}", path.display()))?;
        toml::from_str(&text).with_context(|| format!("bad config {}", path.display()))
    }

    pub fn root(&self) -> PathBuf {
        match self.backup_root.strip_prefix("~/") {
            Some(rest) => home().join(rest),
            None => PathBuf::from(&self.backup_root),
        }
    }
}

fn yes() -> bool {
    true
}

fn home() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/root".into()))
}
