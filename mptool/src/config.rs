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
    /// Page checked for each customer; {customer} is replaced by its name.
    #[serde(default = "monitor_url")]
    pub monitor_url: String,
    /// Text the page must contain (proves the login form really rendered).
    #[serde(default = "monitor_expect")]
    pub monitor_expect: String,
    /// Slower than this shows in yellow on the screen (no alert).
    #[serde(default = "monitor_slow_ms")]
    pub monitor_slow_ms: u64,
    /// Alert when a certificate expires in fewer days than this.
    #[serde(default = "monitor_cert_days")]
    pub monitor_cert_days: i64,
    /// Customers not to check.
    #[serde(default)]
    pub monitor_skip: Vec<String>,
    /// The little screen's brightness, in percent (its light can't be turned down, the colors can).
    #[serde(default = "screen_brightness")]
    pub screen_brightness: u8,
}

impl Config {
    pub fn path() -> PathBuf {
        if let Ok(p) = std::env::var("MPTOOL_CONFIG") {
            return PathBuf::from(p);
        }
        let path = home().join(".config/mptool/config.toml");
        // Before the rename the tool was called mpbackup.
        let old = home().join(".config/mpbackup/config.toml");
        if !path.exists() && old.exists() {
            return old;
        }
        path
    }

    /// Small files the tool keeps between runs (site check results).
    pub fn state_dir() -> PathBuf {
        home().join(".local/state/mptool")
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

fn monitor_url() -> String {
    "https://{customer}.minipass.me/login".into()
}

fn monitor_expect() -> String {
    "type=\"password\"".into()
}

fn monitor_slow_ms() -> u64 {
    3000
}

fn monitor_cert_days() -> i64 {
    14
}

fn screen_brightness() -> u8 {
    70
}

fn home() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/root".into()))
}
