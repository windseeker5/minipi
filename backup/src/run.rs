//! The nightly backup. Read-only toward the server.

use crate::config::Config;
use crate::remote::Remote;
use crate::retention;
use crate::status::{folder_size, nights, Status, Step};
use anyhow::{bail, Context, Result};
use chrono::Local;
use std::path::{Path, PathBuf};
use std::process::Command;

pub const CUSTOMER_LIST: &str = r#"cd deployed && for d in */; do [ -f "$d/app/instance/minipass.db" ] && echo "${d%/}"; done; true"#;

const IMAGES: &str = r#"docker ps --format '{{.Names}}  {{.Image}}  {{.ID}}' && echo && docker image inspect --format '{{.RepoTags}}  {{.Id}}  {{.RepoDigests}}' $(docker ps --format '{{.Image}}' | sort -u)"#;

const MAIL_TAR: &str = "docker exec mailserver tar cf - -C / var/mail var/mail-state tmp/docker-mailserver";
const CERTS_TAR: &str = "docker exec nginx-letsencrypt tar cf - -C /etc nginx/certs acme.sh";

/// Server files and folders copied as-is: (path on server, path in the night folder).
pub const SERVER_FILES: &[(&str, &str)] = &[
    ("docker-compose.yml", "server/docker-compose.yml"),
    ("mailserver.env", "server/mailserver.env"),
    (".env", "server/.env"),
    ("nginx/", "server/nginx/"),
    ("vhost.d/", "server/vhost.d/"),
    ("~/.mailpass", "server/.mailpass"),
    ("/etc/systemd/system/minipass-web.service", "server/minipass-web.service"),
    ("MinipassWebSite/.env", "website/.env"),
];

/// What the backup reports while it works: printed by `mpbackup run`, drawn by the menu.
pub enum Event {
    Log(String),
    /// How many steps this run will have, once the customers are known.
    Total(usize),
    Start(String),
    Done(Step),
}

/// `mpbackup run` prints the events with a time in front.
pub fn print_event(e: Event) {
    let msg = match e {
        Event::Log(m) => m,
        Event::Total(_) => return,
        Event::Start(name) => format!("{name}..."),
        Event::Done(s) => format!("  {} {}", if s.ok { "OK" } else { "FAILED" }, s.detail),
    };
    println!("[{}] {}", Local::now().format("%H:%M:%S"), msg);
}

fn now() -> String {
    Local::now().format("%Y-%m-%d %H:%M:%S").to_string()
}

struct Run<'a> {
    status: Status,
    on: &'a dyn Fn(Event),
}

impl Run<'_> {
    fn step(&mut self, name: &str, f: impl FnOnce() -> Result<String>) -> bool {
        (self.on)(Event::Start(name.into()));
        let (ok, detail) = match f() {
            Ok(d) => (true, d),
            Err(e) => (false, format!("{e:#}")),
        };
        let step = Step { name: name.into(), ok, detail };
        self.status.steps.push(step.clone());
        (self.on)(Event::Done(step));
        ok
    }
}

pub fn run(cfg: &Config, on: &dyn Fn(Event)) -> Result<Status> {
    let root = cfg.root();
    std::fs::create_dir_all(&root).with_context(|| format!("cannot create {}", root.display()))?;
    // The backups hold passwords and keys: only this user may read them.
    std::fs::set_permissions(&root, std::os::unix::fs::PermissionsExt::from_mode(0o700))?;
    // The timer and the menu could start a backup at the same time: only one may run.
    let lock = std::fs::File::create(root.join(".lock"))?;
    if lock.try_lock().is_err() {
        bail!("another backup is already running");
    }
    let night = Local::now().format("%Y-%m-%d").to_string();
    on(Event::Log(format!("Minipass backup {night} -> {}", root.display())));

    // Leftovers from a run that was cut off (power loss, Ctrl-C).
    for entry in std::fs::read_dir(&root)?.flatten() {
        if entry.file_name().to_string_lossy().starts_with(".incomplete-") {
            std::fs::remove_dir_all(entry.path())?;
        }
    }
    let work = root.join(format!(".incomplete-{night}"));
    std::fs::create_dir_all(&work)?;

    // Last good night: unchanged files get hard-linked to it, and its size is the yardstick.
    let prev = nights(&root)
        .into_iter()
        .rev()
        .filter(|(d, _)| d.format("%Y-%m-%d").to_string() != night)
        .map(|(_, p)| p)
        .find(|p| Status::read(&p.join("status.json")).map(|s| s.ok).unwrap_or(false));
    let prev_size = prev.as_ref().and_then(|p| Status::read(&p.join("status.json"))).map(|s| s.size_bytes);

    let remote = Remote::new(cfg);
    let mut r = Run {
        status: Status {
            night: night.clone(),
            started: now(),
            finished: String::new(),
            ok: false,
            size_bytes: 0,
            customers: vec![],
            steps: vec![],
        },
        on,
    };

    if !r.step("Connect to server", || remote.run("true").map(|_| cfg.server.clone())) {
        std::fs::remove_dir_all(&work)?;
        return finish(r.status, &root, None, on);
    }

    let mut customers = vec![];
    r.step("Find customers", || {
        let out = remote.run(&format!("cd {} && {}", remote.base, CUSTOMER_LIST))?;
        customers = out.lines().map(|l| l.trim().to_string()).filter(|l| !l.is_empty()).collect();
        if customers.is_empty() {
            bail!("no customers found in deployed/");
        }
        Ok(customers.join(", "))
    });
    r.status.customers = customers.clone();
    on(Event::Total(11 + customers.len()));

    r.step("Databases (safe copy)", || copy_databases(&remote, &work, &customers));

    for c in &customers {
        let dir = work.join("customers").join(c);
        let prev_dir = prev.as_ref().map(|p| p.join("customers").join(c));
        r.step(&format!("Customer {c}: photos and settings"), || {
            remote.pull(
                &format!("deployed/{c}/app/static/uploads/"),
                &dir.join("uploads/"),
                prev_dir.as_ref().map(|p| p.join("uploads")).as_deref(),
            )?;
            remote.pull(&format!("deployed/{c}/docker-compose.yml"), &dir.join("docker-compose.yml"), None)?;
            remote.pull(&format!("deployed/{c}/app/.env"), &dir.join(".env"), None)?;
            // The app code is on GitHub (dpm); note which version this customer runs.
            let code = remote.run(&format!(
                "cd {}/deployed/{c}/app && git rev-parse HEAD && cat version.txt 2>/dev/null; true",
                remote.base
            ))?;
            std::fs::write(dir.join("code-version.txt"), code)?;
            Ok(format!("{} files", count_files(&dir.join("uploads"))))
        });
    }

    r.step("Server settings and password files", || {
        for (src, dest) in SERVER_FILES {
            let link = prev.as_ref().map(|p| p.join(dest));
            remote.pull(src, &work.join(dest), link.as_deref())?;
        }
        std::fs::write(work.join("server/crontab.txt"), remote.run("crontab -l")?)?;
        std::fs::write(work.join("server/images.txt"), remote.run(IMAGES)?)?;
        Ok(format!("{} items", SERVER_FILES.len() + 2))
    });

    r.step("Mail: messages, accounts, forwards, keys", || {
        let dest = work.join("mail/mail.tar");
        std::fs::create_dir_all(dest.parent().unwrap())?;
        remote.stream_to_file(MAIL_TAR, &dest, &[0, 1])?;
        Ok(human(std::fs::metadata(&dest)?.len()))
    });

    r.step("SSL certificates", || {
        let dest = work.join("server/certs.tar");
        remote.stream_to_file(CERTS_TAR, &dest, &[0, 1])?;
        Ok(human(std::fs::metadata(&dest)?.len()))
    });

    // ── Checks on what we just copied ──
    r.step("Check: every customer has a database", || {
        let missing: Vec<&String> =
            customers.iter().filter(|c| !work.join("customers").join(c).join("minipass.db").is_file()).collect();
        if !missing.is_empty() {
            bail!("missing: {missing:?}");
        }
        Ok(format!("{} customers", customers.len()))
    });

    r.step("Check: databases open and are healthy", || {
        let dbs = find_files(&work, "db");
        if dbs.is_empty() {
            bail!("no databases found");
        }
        for db in &dbs {
            let out = Command::new("sqlite3").arg(db).arg("PRAGMA integrity_check;").output()?;
            let answer = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if answer != "ok" {
                bail!("{} says: {}", db.strip_prefix(&work).unwrap_or(db).display(), answer);
            }
        }
        Ok(format!("{} databases", dbs.len()))
    });

    r.step("Check: mail archive has the DKIM key", || {
        let list = tar_list(&work.join("mail/mail.tar"))?;
        let keys = list.iter().filter(|l| l.contains("opendkim/keys/") && l.ends_with(".private")).count();
        let mails = list.iter().filter(|l| l.starts_with("var/mail/") && !l.ends_with('/')).count();
        if keys == 0 {
            bail!("no DKIM private key in the archive");
        }
        Ok(format!("{keys} DKIM keys, {mails} mail files"))
    });

    r.step("Check: certificate archive opens", || {
        let n = tar_list(&work.join("server/certs.tar"))?.len();
        Ok(format!("{n} entries"))
    });

    let size = folder_size(&work);
    r.status.size_bytes = size;
    r.step("Check: size close to last night", || match prev_size {
        Some(p) if size * 2 < p => bail!("{} tonight vs {} last night", human(size), human(p)),
        Some(p) => Ok(format!("{} (last night {})", human(size), human(p))),
        None => Ok(format!("{} (first backup)", human(size))),
    });

    // Keep what we got, even if a step failed: a partial night is better than none.
    let dest = root.join(&night);
    if dest.exists() {
        std::fs::remove_dir_all(&dest)?; // same-day re-run replaces the earlier one
    }
    std::fs::rename(&work, &dest)?;
    finish(r.status, &root, Some(&dest), on)
}

fn finish(mut st: Status, root: &Path, night_dir: Option<&Path>, on: &dyn Fn(Event)) -> Result<Status> {
    st.finished = now();
    st.ok = st.steps.iter().all(|s| s.ok);
    if let Some(dir) = night_dir {
        st.write(&dir.join("status.json"))?;
        if st.ok {
            let latest = root.join("latest");
            let _ = std::fs::remove_file(&latest);
            std::os::unix::fs::symlink(&st.night, &latest)?;
            prune(root, on)?;
        }
    }
    st.write(&root.join("last-run.json"))?;
    on(Event::Log(if st.ok { "Backup OK" } else { "Backup FAILED" }.into()));
    Ok(st)
}

fn prune(root: &Path, on: &dyn Fn(Event)) -> Result<()> {
    let all = nights(root);
    let dates: Vec<_> = all.iter().map(|(d, _)| *d).collect();
    for d in retention::to_delete(&dates, Local::now().date_naive()) {
        if let Some((_, path)) = all.iter().find(|(nd, _)| *nd == d) {
            on(Event::Log(format!("Removing old night {d}")));
            std::fs::remove_dir_all(path)?;
        }
    }
    Ok(())
}

/// Safe copies of every SQLite database, made on the server with `sqlite3 .backup`
/// into a temporary folder, pulled home, then the temporary folder is removed.
fn copy_databases(remote: &Remote, work: &Path, customers: &[String]) -> Result<String> {
    let mut jobs: Vec<(String, String)> = customers
        .iter()
        .map(|c| (format!("deployed/{c}/app/instance/minipass.db"), format!("customer-{c}.db")))
        .collect();
    jobs.push(("MinipassWebSite/customers.db".into(), "website-customers.db".into()));
    jobs.push(("email_monitoring/monitoring.db".into(), "website-monitoring.db".into()));

    let mut script = format!(
        "set -e; cd {}; T=$(mktemp -d /tmp/mpbackup.XXXXXX); trap 'rm -rf \"$T\"' ERR; ",
        remote.base
    );
    for (src, name) in &jobs {
        script += &format!("sqlite3 -readonly -cmd '.timeout 10000' {src} \".backup '$T/{name}'\"; ");
    }
    script += "echo \"$T\"";
    let tmp = remote.run(&script)?.trim().to_string();
    if !tmp.starts_with("/tmp/mpbackup.") {
        bail!("unexpected temp folder: {tmp}");
    }

    let landing = work.join(".dbs");
    let pulled = remote.pull(&format!("{tmp}/"), &landing.join(""), None);
    let cleaned = remote.run(&format!("rm -rf '{tmp}'"));
    pulled?;
    cleaned.context("could not remove the temporary copy on the server")?;

    for c in customers {
        let dir = work.join("customers").join(c);
        std::fs::create_dir_all(&dir)?;
        std::fs::rename(landing.join(format!("customer-{c}.db")), dir.join("minipass.db"))?;
    }
    std::fs::create_dir_all(work.join("website"))?;
    std::fs::rename(landing.join("website-customers.db"), work.join("website/customers.db"))?;
    std::fs::rename(landing.join("website-monitoring.db"), work.join("website/monitoring.db"))?;
    std::fs::remove_dir(&landing)?;
    Ok(format!("{} databases", jobs.len()))
}

pub fn tar_list(path: &Path) -> Result<Vec<String>> {
    let out = Command::new("tar").arg("tf").arg(path).output()?;
    if !out.status.success() {
        bail!("cannot read {}: {}", path.display(), String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).lines().map(String::from).collect())
}

pub fn find_files(dir: &Path, ext: &str) -> Vec<PathBuf> {
    let mut out = vec![];
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let p = entry.path();
        let Ok(meta) = std::fs::symlink_metadata(&p) else { continue };
        if meta.is_dir() {
            out.extend(find_files(&p, ext));
        } else if p.extension().map(|e| e == ext).unwrap_or(false) {
            out.push(p);
        }
    }
    out
}

fn count_files(dir: &Path) -> usize {
    std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| if e.path().is_dir() { count_files(&e.path()) } else { 1 })
        .sum()
}

pub fn human(bytes: u64) -> String {
    let mb = bytes as f64 / 1_048_576.0;
    if mb >= 1024.0 {
        format!("{:.1} GB", mb / 1024.0)
    } else {
        format!("{mb:.1} MB")
    }
}
