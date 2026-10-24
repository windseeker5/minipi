//! Site monitoring (`mptool monitor`, every 5 minutes on the Pi). Each customer's
//! login page must answer, really show the login form, and have a certificate that
//! isn't about to expire. A problem goes to Discord once it shows up twice in a row
//! (one blip is not worth a message), and again when it's gone.
//! Customers come from the last backup, so new ones are picked up overnight.

use crate::config::Config;
use crate::notify;
use crate::status::Status;
use anyhow::Result;
use chrono::{Local, NaiveDateTime};
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::PathBuf;
use std::process::Command;

/// Checks in a row before a problem is reported.
const FAILS_TO_ALERT: u32 = 2;
const MARK: &str = "\n__mptool__ ";

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Site {
    pub customer: String,
    pub url: String,
    /// What's wrong, empty when the site is fine. Slowness is not a problem, just shown.
    pub problem: String,
    pub ms: u64,
    pub cert_days: Option<i64>,
    pub checked: String,
    /// Checks in a row with a problem.
    pub fails: u32,
    /// The problem Discord was told about, empty if none.
    pub alerted: String,
    pub down_since: String,
}

impl Site {
    /// A problem confirmed by enough checks (the screen shows it, Discord has it).
    pub fn down(&self) -> bool {
        !self.problem.is_empty() && self.fails >= FAILS_TO_ALERT
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Monitor {
    pub checked: String,
    pub sites: Vec<Site>,
}

impl Monitor {
    pub fn path() -> PathBuf {
        Config::state_dir().join("monitor.json")
    }

    pub fn read() -> Option<Monitor> {
        serde_json::from_str(&std::fs::read_to_string(Monitor::path()).ok()?).ok()
    }

    fn write(&self) -> Result<()> {
        std::fs::create_dir_all(Config::state_dir())?;
        std::fs::write(Monitor::path(), serde_json::to_string_pretty(self)?)?;
        Ok(())
    }
}

/// One round of checks: update the saved state, tell Discord about changes, print a table.
pub fn run(cfg: &Config) -> Result<()> {
    let customers: Vec<String> = Status::read(&cfg.root().join("last-run.json"))
        .map(|s| s.customers)
        .unwrap_or_default()
        .into_iter()
        .filter(|c| !cfg.monitor_skip.contains(c))
        .collect();
    let before = Monitor::read().unwrap_or_default();

    // All sites at once: a slow one doesn't hold up the others.
    let results: Vec<Check> = std::thread::scope(|s| {
        let jobs: Vec<_> = customers
            .iter()
            .map(|c| {
                let url = cfg.monitor_url.replace("{customer}", c);
                s.spawn(move || check(cfg, &url))
            })
            .collect();
        jobs.into_iter().map(|j| j.join().unwrap_or_else(|_| Check::failed("check crashed"))).collect()
    });

    let now = now();
    let mut sites = vec![];
    for (c, r) in customers.iter().zip(results) {
        let prev = before.sites.iter().find(|s| &s.customer == c).cloned().unwrap_or_default();
        let mut site = Site {
            customer: c.clone(),
            url: cfg.monitor_url.replace("{customer}", c),
            problem: r.problem,
            ms: r.ms,
            cert_days: r.cert_days,
            checked: now.clone(),
            fails: 0,
            alerted: prev.alerted,
            down_since: prev.down_since,
        };
        if site.problem.is_empty() {
            if !site.alerted.is_empty() {
                let msg = format!(
                    "**{c}** is back: login page OK in {} ms (was: {}, since {})",
                    site.ms, site.alerted, site.down_since
                );
                if notify::send(cfg, &msg).is_ok() {
                    site.alerted.clear();
                }
            }
            if site.alerted.is_empty() {
                site.down_since.clear();
            }
        } else {
            site.fails = prev.fails + 1;
            if site.down_since.is_empty() {
                site.down_since = now.clone();
            }
            if site.down() && site.alerted != site.problem {
                let msg = format!("**{c}: {}**\n{} (since {})", site.problem, site.url, site.down_since);
                // Not sent: try again next round.
                if notify::send(cfg, &msg).is_ok() {
                    site.alerted = site.problem.clone();
                }
            }
        }
        sites.push(site);
    }
    // Save before printing: what Discord was told must never be lost.
    let state = Monitor { checked: now, sites };
    state.write()?;
    let mut out = std::io::stdout().lock();
    for site in &state.sites {
        let note = match (&site.problem, site.cert_days) {
            (p, _) if !p.is_empty() => p.clone(),
            (_, Some(d)) => format!("certificate {d} days left"),
            _ => String::new(),
        };
        let word = if site.problem.is_empty() { "OK" } else { "PROBLEM" };
        let _ = writeln!(out, "{:<10} {:<8} {:>5} ms   {}", site.customer, word, site.ms, note);
    }
    Ok(())
}

struct Check {
    problem: String,
    ms: u64,
    cert_days: Option<i64>,
}

impl Check {
    fn failed(problem: &str) -> Check {
        Check { problem: problem.into(), ms: 0, cert_days: None }
    }
}

/// Load the page once with curl: status, time, content and certificate.
fn check(cfg: &Config, url: &str) -> Check {
    let out = Command::new("curl")
        .args(["-sS", "-v", "--max-time", "20", "-w"])
        .arg(format!("{MARK}%{{http_code}} %{{time_total}}"))
        .arg(url)
        .output();
    let Ok(out) = out else { return Check::failed("cannot run curl") };
    let body = String::from_utf8_lossy(&out.stdout);
    let log = String::from_utf8_lossy(&out.stderr);

    // The certificate's end date, from curl's TLS log: "*  expire date: Dec 12 19:19:05 2026 GMT".
    let cert_days = log
        .lines()
        .find_map(|l| l.split_once("expire date:").map(|(_, d)| d.split_whitespace().collect::<Vec<_>>().join(" ")))
        .and_then(|d| NaiveDateTime::parse_from_str(&d, "%b %d %H:%M:%S %Y GMT").ok())
        .map(|end| (end - chrono::Utc::now().naive_utc()).num_days());

    let Some((page, tail)) = body.rsplit_once(MARK) else {
        let why = log.lines().find(|l| l.starts_with("curl: (")).unwrap_or("no answer");
        return Check::failed(&format!("no answer ({})", why.trim_start_matches("curl: ")));
    };
    let mut parts = tail.split_whitespace();
    let code = parts.next().unwrap_or("000");
    let ms = parts.next().and_then(|t| t.parse::<f64>().ok()).map(|t| (t * 1000.0) as u64).unwrap_or(0);

    let problem = if !out.status.success() || code == "000" {
        let why = log.lines().find(|l| l.starts_with("curl: (")).unwrap_or("no answer");
        format!("no answer ({})", why.trim_start_matches("curl: "))
    } else if code != "200" {
        format!("login page gives HTTP {code}")
    } else if !page.to_lowercase().contains(&cfg.monitor_expect.to_lowercase()) {
        "login form missing from the page".into()
    } else if cert_days.is_some_and(|d| d < cfg.monitor_cert_days) {
        format!("certificate expires in {} days", cert_days.unwrap_or(0))
    } else {
        String::new()
    };
    Check { problem, ms, cert_days }
}

fn now() -> String {
    Local::now().format("%Y-%m-%d %H:%M:%S").to_string()
}
