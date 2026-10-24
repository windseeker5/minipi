//! Unpack a night on this computer, to look at or test. The server is not touched.

use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Where a night gets unpacked: ~/restore-test/<night>/
pub fn dest_for(night: &str) -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/root".into()))
        .join("restore-test")
        .join(night)
}

pub fn unpack(night_dir: &Path, dest: &Path) -> Result<String> {
    if dest.exists() {
        bail!("already unpacked in {}. Delete that folder to unpack again.", dest.display());
    }
    let parent = dest.parent().context("bad destination")?;
    std::fs::create_dir_all(parent)?;
    // Holds passwords and keys, like the backups.
    std::fs::set_permissions(parent, std::os::unix::fs::PermissionsExt::from_mode(0o700))?;

    sh(Command::new("cp").arg("-a").arg(night_dir).arg(dest))?;
    for (tar, into) in [("mail/mail.tar", "mail"), ("server/certs.tar", "server/certs")] {
        let tar = dest.join(tar);
        if !tar.is_file() {
            continue;
        }
        let into = dest.join(into);
        std::fs::create_dir_all(&into)?;
        sh(Command::new("tar").arg("xf").arg(&tar).arg("--no-same-owner").arg("-C").arg(&into))?;
        std::fs::remove_file(&tar)?;
    }
    Ok(format!("Unpacked in {}", dest.display()))
}

fn sh(cmd: &mut Command) -> Result<()> {
    let out = cmd.output()?;
    if !out.status.success() {
        bail!("{}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(())
}
