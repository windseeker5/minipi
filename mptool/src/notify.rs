//! Discord messages: always on failure, and a short "OK" every morning so that
//! silence means the backup did not run.

use crate::config::Config;
use crate::run::human;
use crate::status::Status;
use anyhow::{bail, Result};
use std::process::Command;

pub fn send(cfg: &Config, text: &str) -> Result<()> {
    if cfg.discord_webhook.is_empty() {
        return Ok(());
    }
    let body = serde_json::json!({ "content": text }).to_string();
    let out = Command::new("curl")
        .args(["-fsS", "--retry", "3", "--max-time", "30", "-H", "Content-Type: application/json", "-d"])
        .arg(&body)
        .arg(&cfg.discord_webhook)
        .output()?;
    if !out.status.success() {
        bail!("Discord: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(())
}

pub fn report(cfg: &Config, st: &Status) -> Result<()> {
    let text = if st.ok {
        if !cfg.discord_daily_ok {
            return Ok(());
        }
        format!(
            "Minipass backup OK - {} - {}, customers: {}",
            st.night,
            human(st.size_bytes),
            st.customers.join(", ")
        )
    } else {
        let mut t = format!("**Minipass backup FAILED - {}**", st.night);
        for s in st.failed_steps() {
            t += &format!("\n- {}: {}", s.name, s.detail);
        }
        t
    };
    send(cfg, &text)
}
