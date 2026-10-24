//! What a night's backup looked like: written as status.json in each night folder,
//! and as last-run.json in the backup root.

use anyhow::Result;
use chrono::NaiveDate;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Step {
    pub name: String,
    pub ok: bool,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Status {
    pub night: String,
    pub started: String,
    pub finished: String,
    pub ok: bool,
    pub size_bytes: u64,
    pub customers: Vec<String>,
    pub steps: Vec<Step>,
}

impl Status {
    pub fn failed_steps(&self) -> Vec<&Step> {
        self.steps.iter().filter(|s| !s.ok).collect()
    }

    pub fn read(path: &Path) -> Option<Status> {
        let text = std::fs::read_to_string(path).ok()?;
        serde_json::from_str(&text).ok()
    }

    pub fn write(&self, path: &Path) -> Result<()> {
        std::fs::write(path, serde_json::to_string_pretty(self)?)?;
        Ok(())
    }
}

/// Night folders in the backup root (named YYYY-MM-DD), oldest first.
pub fn nights(root: &Path) -> Vec<(NaiveDate, PathBuf)> {
    let mut out: Vec<(NaiveDate, PathBuf)> = std::fs::read_dir(root)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            NaiveDate::parse_from_str(&name, "%Y-%m-%d").ok().map(|d| (d, e.path()))
        })
        .collect();
    out.sort();
    out
}

/// Total bytes of all files under a folder (links not followed).
pub fn folder_size(dir: &Path) -> u64 {
    let mut total = 0;
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let Ok(meta) = std::fs::symlink_metadata(entry.path()) else { continue };
        if meta.is_dir() {
            total += folder_size(&entry.path());
        } else if meta.is_file() {
            total += meta.len();
        }
    }
    total
}

/// Plain-text summary for `mptool status`.
pub fn print(root: &Path) {
    match Status::read(&root.join("last-run.json")) {
        None => println!("No backup has run yet ({}).", root.display()),
        Some(st) => {
            println!(
                "Last backup: {} at {}  ->  {}",
                st.night,
                st.finished,
                if st.ok { "OK" } else { "FAILED" }
            );
            for s in &st.steps {
                println!("  {}  {}  {}", if s.ok { "ok  " } else { "FAIL" }, s.name, s.detail);
            }
        }
    }
    let all = nights(root);
    println!("\nNights on disk: {}", all.len());
    for (d, p) in all.iter().rev() {
        let mark = match Status::read(&p.join("status.json")) {
            Some(s) if s.ok => "OK",
            Some(_) => "FAILED",
            None => "?",
        };
        println!("  {d}  {mark}");
    }
}
