//! Talking to the VPS. Every command here only reads from the server,
//! except the temporary DB copies in /tmp/mpbackup.* which we remove ourselves.

use crate::config::Config;
use anyhow::{bail, Context, Result};
use std::fs::File;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

pub struct Remote {
    server: String,
    port: u16,
    pub base: String,
}

impl Remote {
    pub fn new(cfg: &Config) -> Remote {
        Remote { server: cfg.server.clone(), port: cfg.port, base: cfg.remote_base.clone() }
    }

    /// One shared SSH connection for the whole run, so the server sees a single login.
    fn ssh_opts(&self) -> Vec<String> {
        vec![
            "-p".into(),
            self.port.to_string(),
            "-o".into(),
            "BatchMode=yes".into(),
            "-o".into(),
            "ConnectTimeout=20".into(),
            "-o".into(),
            "ControlMaster=auto".into(),
            "-o".into(),
            "ControlPath=/tmp/mpbackup-ssh-%C".into(),
            "-o".into(),
            "ControlPersist=120".into(),
        ]
    }

    /// Run a shell command on the server and return its output.
    pub fn run(&self, script: &str) -> Result<String> {
        let out = Command::new("ssh")
            .args(self.ssh_opts())
            .arg(&self.server)
            .arg(script)
            .output()
            .context("cannot start ssh")?;
        if !out.status.success() {
            bail!("server command failed: {}", String::from_utf8_lossy(&out.stderr).trim());
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    /// Run a shell command on the server, feeding `input` to it, and return its output.
    pub fn run_input(&self, script: &str, input: &str) -> Result<String> {
        let mut child = Command::new("ssh")
            .args(self.ssh_opts())
            .arg(&self.server)
            .arg(script)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("cannot start ssh")?;
        child.stdin.take().unwrap().write_all(input.as_bytes())?;
        let out = child.wait_with_output()?;
        if !out.status.success() {
            bail!("server command failed: {}", String::from_utf8_lossy(&out.stderr).trim());
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    /// Run a command on the server and save its output into a local file.
    /// `ok_codes` lists exit codes that still count as success (tar returns 1 when a file changed while read).
    pub fn stream_to_file(&self, script: &str, dest: &Path, ok_codes: &[i32]) -> Result<()> {
        let file = File::create(dest).with_context(|| format!("cannot create {}", dest.display()))?;
        let out = Command::new("ssh")
            .args(self.ssh_opts())
            .arg("-C")
            .arg(&self.server)
            .arg(script)
            .stdout(Stdio::from(file))
            .stderr(Stdio::piped())
            .output()
            .context("cannot start ssh")?;
        let code = out.status.code().unwrap_or(-1);
        if !ok_codes.contains(&code) {
            bail!("exit {}: {}", code, String::from_utf8_lossy(&out.stderr).trim());
        }
        Ok(())
    }

    /// Copy a file or folder from the server. Paths are relative to `base`, unless they start
    /// with `/` (absolute) or `~/` (server home). Folders must end with `/`.
    /// `link_dest` points at the same item in last night's backup.
    pub fn pull(&self, remote: &str, local: &Path, link_dest: Option<&Path>) -> Result<()> {
        let src = self.rsync_path(remote);
        if let Some(parent) = local.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut cmd = Command::new("rsync");
        cmd.arg("-a")
            .arg("-e")
            .arg(format!("ssh {}", self.ssh_opts().join(" ")));
        if let Some(ld) = link_dest.filter(|p| p.exists()) {
            cmd.arg(format!("--link-dest={}", ld.display()));
        }
        let out = cmd
            .arg(format!("{}:{}", self.server, src))
            .arg(local)
            .output()
            .context("cannot start rsync")?;
        if !out.status.success() {
            bail!("rsync {} failed: {}", remote, String::from_utf8_lossy(&out.stderr).trim());
        }
        Ok(())
    }

    /// What `pull` would change in `local`, without copying anything (rsync --dry-run).
    /// Returns rsync's change lines for files: `>f...` new or changed on the server,
    /// `*deleting` gone from the server. Contents are compared, not dates.
    pub fn dry_run(&self, remote: &str, local: &Path) -> Result<Vec<String>> {
        let out = Command::new("rsync")
            .args(["-a", "--checksum", "--dry-run", "--itemize-changes", "--delete", "-e"])
            .arg(format!("ssh {}", self.ssh_opts().join(" ")))
            .arg(format!("{}:{}", self.server, self.rsync_path(remote)))
            .arg(local)
            .output()
            .context("cannot start rsync")?;
        if !out.status.success() {
            bail!("rsync {} failed: {}", remote, String::from_utf8_lossy(&out.stderr).trim());
        }
        Ok(String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter(|l| (l.starts_with(">f") || l.starts_with("*deleting")) && !l.ends_with('/'))
            .map(String::from)
            .collect())
    }

    fn rsync_path(&self, remote: &str) -> String {
        if remote.starts_with('/') {
            remote.to_string()
        } else if let Some(in_home) = remote.strip_prefix("~/") {
            in_home.to_string() // rsync reads paths without a leading / from the home folder
        } else {
            format!("{}/{}", self.base, remote)
        }
    }
}
