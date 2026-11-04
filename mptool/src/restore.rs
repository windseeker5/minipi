//! Putting things back on the server. This is the only part of the tool that changes the server.
//! Every restore: locks, check the backup, preview, confirm, then a script on the server saves an
//! undo copy, puts the files back, restarts only what's affected and checks it worked. That script
//! runs detached in ~/mptool-undo/<id>/, so a lost connection can't stop it halfway.

use crate::compare::{self, dir_names, mail_group, mail_key};
use crate::config::Config;
use crate::notify;
use crate::remote::Remote;
use crate::run::{human, take_lock, Event, SERVICE_FILE};
use crate::status::{nights, Status, Step};
use anyhow::{bail, Context, Result};
use chrono::Local;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::fs::File;
use std::io::{BufRead, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::time::{Duration, Instant};

/// Prints the undo folder of any restore still running on the server.
pub const RUNNING: &str = r#"for f in ~/mptool-undo/*/pid; do [ -f "$f" ] || continue; p=$(cat "$f"); if grep -qs job.sh "/proc/$p/cmdline"; then echo "${f%/pid}"; fi; done; true"#;

/// How long a lost connection is retried before telling the user how to finish.
const LOST_LIMIT: Duration = Duration::from_secs(90);
/// Undo folders kept on the server.
const KEEP_UNDO: usize = 5;
const REAL_VPS_IP: &str = "138.199.152.128";

pub const MAIL_PARTS: [&str; 3] = ["accounts", "forwards", "dkim"];

#[derive(Debug, Clone, PartialEq)]
pub enum What {
    Customer(String),
    /// Which settings: accounts, forwards, dkim.
    MailSettings(Vec<String>),
    MailMessages,
    Website,
    Server,
    Everything,
    Undo,
}

impl What {
    pub fn label(&self) -> String {
        match self {
            What::Customer(c) => format!("customer {c}"),
            What::MailSettings(p) => format!("mail settings ({})", p.join(", ")),
            What::MailMessages => "mail messages".into(),
            What::Website => "website".into(),
            What::Server => "server settings".into(),
            What::Everything => "everything".into(),
            What::Undo => "undo of the last restore".into(),
        }
    }

    /// What must be typed to go ahead.
    pub fn confirm_word(&self) -> String {
        match self {
            What::Customer(c) => c.clone(),
            _ => "RESTORE".into(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Job {
    pub what: What,
    pub night: Option<String>,
}

/// Everything found out before asking for the go-ahead. Nothing has changed yet.
pub struct Prepared {
    pub job: Job,
    pub night: String,
    pub night_dir: PathBuf,
    pub preview: Vec<String>,
    pub real_vps: bool,
    undo_id: Option<String>,
}

pub struct Report {
    pub label: String,
    pub night: String,
    pub steps: Vec<Step>,
    pub secs: u64,
}

impl Report {
    pub fn ok(&self) -> bool {
        self.steps.iter().all(|s| s.ok)
    }

    pub fn took(&self) -> String {
        format!("{} min {} s", self.secs / 60, self.secs % 60)
    }

    fn discord(&self) -> String {
        let from = if self.night.is_empty() { String::new() } else { format!(" from {}", self.night) };
        let mark = if self.ok() { "" } else { "**" };
        let word = if self.ok() { "OK" } else { "FAILED" };
        let mut text = format!("{mark}minipass restore {word}{mark}: {}{from}, took {}", self.label, self.took());
        for s in self.steps.iter().filter(|s| !s.ok) {
            text += &format!("\n- {}: {}", s.name, s.detail);
        }
        text
    }
}

// ── The locks: plain functions, checked before any connection ──

/// Is this the real server? Looks at what the config says and at where ssh would really go.
pub fn is_real_vps(server: &str, effective_host: &str) -> bool {
    let host = server.rsplit('@').next().unwrap_or(server);
    [host, effective_host].iter().any(|h| {
        let h = h.trim().trim_end_matches('.').to_ascii_lowercase();
        h == "minipass.me" || h.ends_with(".minipass.me") || h == REAL_VPS_IP
    })
}

pub fn locks(cfg: &Config, what: &What, effective_host: &str) -> Result<()> {
    if !cfg.restore_enabled {
        bail!("Restore is off. Nothing was changed. It takes restore_enabled = true in the config.");
    }
    if is_real_vps(&cfg.server, effective_host) && !cfg.real_vps_ok {
        bail!("This is the real server and real_vps_ok is off. Nothing was changed.");
    }
    if let What::Customer(c) = what {
        if !valid_name(c) {
            bail!("Not a customer name: {c}");
        }
        if !cfg.restore_allowed.iter().any(|a| a == c) {
            bail!("{c} is not on restore_allowed in the config. Nothing was changed.");
        }
    }
    Ok(())
}

fn valid_name(c: &str) -> bool {
    !c.is_empty() && c.chars().all(|x| x.is_ascii_alphanumeric() || x == '-' || x == '_')
}

/// Where ssh would really connect, from its own config. `ssh -G` doesn't touch the network.
fn effective_host(cfg: &Config) -> String {
    Command::new("ssh")
        .args(&cfg.ssh_options)
        .arg("-G")
        .arg(&cfg.server)
        .output()
        .ok()
        .and_then(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .find_map(|l| l.strip_prefix("hostname ").map(String::from))
        })
        .unwrap_or_default()
}

// ── Reading tar archives ──

const BLOCK: u64 = 512;

/// One entry of a tar file. `from` is where its headers start (long names come first),
/// `data` where its contents start.
pub struct TarEntry {
    pub name: String,
    pub kind: u8,
    pub size: u64,
    pub link: String,
    from: u64,
    data: u64,
}

fn octal(field: &[u8]) -> u64 {
    if field[0] & 0x80 != 0 {
        return field[1..].iter().fold(0u64, |n, b| (n << 8) | *b as u64);
    }
    let text: String = field.iter().take_while(|b| **b != 0).map(|b| *b as char).collect();
    u64::from_str_radix(text.trim(), 8).unwrap_or(0)
}

fn c_text(field: &[u8]) -> String {
    let end = field.iter().position(|b| *b == 0).unwrap_or(field.len());
    String::from_utf8_lossy(&field[..end]).into_owned()
}

fn checksum(header: &[u8]) -> u64 {
    header.iter().enumerate().map(|(i, b)| if (148..156).contains(&i) { 32 } else { *b as u64 }).sum()
}

/// Every entry of a tar file, in order. An archive that was cut or damaged is an error,
/// even when it was cut exactly between two files (`tar tf` doesn't notice that).
pub fn tar_entries(path: &Path) -> Result<Vec<TarEntry>> {
    let mut f = File::open(path).with_context(|| format!("cannot open {}", path.display()))?;
    let len = f.metadata()?.len();
    let mut out = vec![];
    let mut pos = 0u64;
    let (mut from, mut long_name, mut long_link) = (None, None, None);
    let mut header = [0u8; BLOCK as usize];
    loop {
        if pos + BLOCK > len {
            bail!("{} is cut short", path.display());
        }
        f.seek(SeekFrom::Start(pos))?;
        f.read_exact(&mut header)?;
        if header.iter().all(|b| *b == 0) {
            return Ok(out);
        }
        if checksum(&header) != octal(&header[148..156]) {
            bail!("{} is damaged", path.display());
        }
        let size = octal(&header[124..136]);
        let kind = header[156];
        let data = pos + BLOCK;
        let next = data + size.div_ceil(BLOCK) * BLOCK;
        if next > len {
            bail!("{} is cut short", path.display());
        }
        from = from.or(Some(pos));
        match kind {
            b'L' | b'K' | b'x' | b'g' => {
                let mut text = vec![0u8; size as usize];
                f.read_exact(&mut text)?;
                match kind {
                    b'L' => long_name = Some(c_text(&text)),
                    b'K' => long_link = Some(c_text(&text)),
                    b'x' => {
                        for (key, value) in pax_records(&text) {
                            match key.as_str() {
                                "path" => long_name = Some(value),
                                "linkpath" => long_link = Some(value),
                                _ => {}
                            }
                        }
                    }
                    _ => {}
                }
            }
            _ => {
                let prefix = if &header[257..263] == b"ustar\0" { c_text(&header[345..500]) } else { String::new() };
                let short = c_text(&header[..100]);
                let name = long_name.take().unwrap_or(if prefix.is_empty() { short } else { format!("{prefix}/{short}") });
                let link = long_link.take().unwrap_or_else(|| c_text(&header[157..257]));
                out.push(TarEntry { name, kind, size, link, from: from.take().unwrap_or(pos), data });
            }
        }
        pos = next;
    }
}

fn pax_records(text: &[u8]) -> Vec<(String, String)> {
    let mut out = vec![];
    let mut at = 0;
    while at < text.len() {
        let Some(space) = text[at..].iter().position(|b| *b == b' ') else { break };
        let Ok(len) = String::from_utf8_lossy(&text[at..at + space]).parse::<usize>() else { break };
        if len == 0 || at + len > text.len() {
            break;
        }
        let record = String::from_utf8_lossy(&text[at + space + 1..at + len - 1]).into_owned();
        if let Some((k, v)) = record.split_once('=') {
            out.push((k.to_string(), v.to_string()));
        }
        at += len;
    }
    out
}

fn copy_range(from: &mut File, to: &mut impl Write, start: u64, len: u64) -> Result<()> {
    from.seek(SeekFrom::Start(start))?;
    std::io::copy(&mut from.take(len), to)?;
    Ok(())
}

/// A smaller tar holding only the kept entries, with owners, dates and modes untouched.
/// A hard link to a file that was left out becomes a normal file.
pub fn tar_filter(src: &Path, dst: &Path, keep: &dyn Fn(&str) -> bool) -> Result<usize> {
    let entries = tar_entries(src)?;
    let mut input = File::open(src)?;
    let mut output = std::io::BufWriter::new(File::create(dst)?);
    let by_name: HashMap<&str, &TarEntry> = entries.iter().map(|e| (e.name.as_str(), e)).collect();
    let mut written: HashSet<&str> = HashSet::new();
    for e in entries.iter().filter(|e| keep(&e.name)) {
        let end = e.data + e.size.div_ceil(BLOCK) * BLOCK;
        let target = by_name.get(e.link.as_str()).filter(|_| e.kind == b'1' && !written.contains(e.link.as_str()));
        if let Some(target) = target {
            let start = e.data - BLOCK;
            copy_range(&mut input, &mut output, e.from, start - e.from)?;
            let mut header = [0u8; BLOCK as usize];
            input.seek(SeekFrom::Start(start))?;
            input.read_exact(&mut header)?;
            header[156] = b'0';
            header[157..257].fill(0);
            header[124..136].copy_from_slice(format!("{:011o}\0", target.size).as_bytes());
            let sum = checksum(&header);
            header[148..156].copy_from_slice(format!("{sum:06o}\0 ").as_bytes());
            output.write_all(&header)?;
            copy_range(&mut input, &mut output, target.data, target.size.div_ceil(BLOCK) * BLOCK)?;
        } else {
            copy_range(&mut input, &mut output, e.from, end - e.from)?;
        }
        written.insert(&e.name);
    }
    output.write_all(&[0u8; 2 * BLOCK as usize])?;
    output.flush()?;
    Ok(written.len())
}

// ── Choosing what mail to add back ──

/// Paths from the backup to add back (folders end in /). A message counts as there when its
/// account has one with the same name before ":2,", in any folder, read or not. Dovecot's own
/// index files only come back with a mailbox that's completely gone. Nothing is ever removed.
pub fn mail_to_add(backup: &[String], server: &[String]) -> Vec<String> {
    let have_keys: BTreeSet<String> = server.iter().filter_map(|p| mail_key(p)).collect();
    let have_paths: BTreeSet<&str> = server.iter().map(|p| p.trim_end_matches('/')).collect();
    let mut out = vec![];
    for p in backup.iter().filter(|p| p.starts_with("var/mail/")) {
        let clean = p.trim_end_matches('/');
        if let Some(key) = mail_key(p) {
            if !have_keys.contains(&key) {
                out.push(p.clone());
            }
            continue;
        }
        if have_paths.contains(clean) {
            continue;
        }
        if !p.ends_with('/') {
            let (dir, name) = clean.rsplit_once('/').unwrap_or(("", clean));
            let is_tmp = dir.ends_with("/tmp");
            let is_index = name.starts_with("dovecot") && have_paths.contains(format!("{dir}/cur").as_str());
            if is_tmp || is_index {
                continue;
            }
        }
        out.push(p.clone());
    }
    out
}

/// The accounts these mail paths belong to, as user@domain.
fn mail_accounts(paths: &[String]) -> Vec<String> {
    let set: BTreeSet<String> = paths
        .iter()
        .filter_map(|p| {
            let parts: Vec<&str> = p.trim_end_matches('/').split('/').collect();
            (parts.len() >= 4).then(|| format!("{}@{}", parts[3], parts[2]))
        })
        .collect();
    set.into_iter().collect()
}

fn setting_part(group: &str) -> &'static str {
    match group {
        "Mail signing key (DKIM)" => "dkim",
        "Forwards to Gmail" => "forwards",
        _ => "accounts",
    }
}

/// The mail server's own lock file sometimes sits in the settings folder. It isn't a setting.
fn is_setting(name: &str) -> bool {
    name.strip_prefix("tmp/docker-mailserver/").is_some_and(|rest| !rest.is_empty()) && !name.ends_with(".lock")
}

/// Mail files that a whole-server restore puts back: everything except mail waiting to be sent.
fn full_mail_keeps(name: &str) -> bool {
    (name.starts_with("var/mail/") || name.starts_with("var/mail-state/") || is_setting(name))
        && !name.starts_with("var/mail-state/spool-postfix")
}

// ── Checking the backup ──

fn db_healthy(path: &Path) -> Result<()> {
    let out = Command::new("sqlite3").arg("-readonly").arg(path).arg("PRAGMA integrity_check;").output()?;
    let answer = String::from_utf8_lossy(&out.stdout);
    if !path.is_file() || answer.trim() != "ok" {
        bail!("{} is not a healthy database", path.display());
    }
    Ok(())
}

fn need(path: &Path) -> Result<()> {
    if !path.exists() {
        bail!("{} is missing from the backup", path.display());
    }
    Ok(())
}

fn check_customer(night: &Path, c: &str) -> Result<()> {
    let dir = night.join("customers").join(c);
    need(&dir)?;
    db_healthy(&dir.join("minipass.db"))?;
    for f in ["uploads", ".env", "docker-compose.yml", "code-version.txt"] {
        need(&dir.join(f))?;
    }
    Ok(())
}

fn check_mail(night: &Path, settings: bool, messages: bool) -> Result<()> {
    let names: Vec<String> = tar_entries(&night.join("mail/mail.tar"))?.into_iter().map(|e| e.name).collect();
    if settings && !names.iter().any(|n| n.ends_with("/postfix-accounts.cf")) {
        bail!("the mail archive has no accounts");
    }
    if messages && !names.iter().any(|n| n.starts_with("var/mail/")) {
        bail!("the mail archive has no mailboxes");
    }
    Ok(())
}

fn check_website(night: &Path) -> Result<()> {
    db_healthy(&night.join("website/customers.db"))?;
    db_healthy(&night.join("website/monitoring.db"))?;
    need(&night.join("website/.env"))
}

fn check_server(night: &Path) -> Result<()> {
    for f in ["docker-compose.yml", "mailserver.env", ".env", "nginx", "vhost.d", ".mailpass", "minipass-web.service", "crontab.txt"] {
        need(&night.join("server").join(f))?;
    }
    tar_entries(&night.join("server/certs.tar")).map(|_| ())
}

/// The backup must be sound before anything on the server is touched.
pub fn check_backup(night: &Path, what: &What) -> Result<()> {
    match what {
        What::Customer(c) => check_customer(night, c),
        What::MailSettings(_) => check_mail(night, true, false),
        What::MailMessages => check_mail(night, false, true),
        What::Website => check_website(night),
        What::Server => check_server(night),
        What::Undo => Ok(()),
        What::Everything => {
            check_server(night)?;
            check_mail(night, true, true)?;
            check_website(night)?;
            for c in dir_names(&night.join("customers")) {
                check_customer(night, &c)?;
            }
            Ok(())
        }
    }
}

// ── Before the go-ahead ──

fn pick_night(cfg: &Config, wanted: Option<&str>) -> Result<(String, PathBuf)> {
    let root = cfg.root();
    let all = nights(&root);
    let found = match wanted {
        Some(w) => all.into_iter().find(|(d, _)| d.format("%Y-%m-%d").to_string() == w),
        None => all
            .into_iter()
            .rev()
            .find(|(_, p)| Status::read(&p.join("status.json")).map(|s| s.ok).unwrap_or(false)),
    };
    match found {
        Some((d, p)) => Ok((d.format("%Y-%m-%d").to_string(), p)),
        None => bail!("No backup found for {}", wanted.unwrap_or("the last good night")),
    }
}

fn wanted_line(what: &What, line: &compare::Line) -> bool {
    match what {
        What::Customer(c) => line.who == *c,
        What::MailSettings(parts) => {
            matches!(line.what.as_str(), "Mail accounts" | "Forwards to Gmail" | "Mail signing key (DKIM)")
                && parts.iter().any(|p| p == setting_part(&line.what))
        }
        What::MailMessages => line.what == "Mail messages",
        What::Website => matches!(line.what.as_str(), "Website customer list" | "API keys + passwords"),
        What::Server => matches!(line.what.as_str(), "Server settings" | "API keys + passwords"),
        What::Everything => true,
        What::Undo => false,
    }
}

fn refuse_if_running(remote: &Remote) -> Result<()> {
    let running = remote.run(RUNNING)?;
    if !running.trim().is_empty() {
        bail!("A restore is still running on the server: {}. Wait for it, then see `mptool restore status`.", running.trim());
    }
    Ok(())
}

/// Locks, backup check and preview. Changes nothing anywhere.
pub fn prepare(cfg: &Config, job: &Job) -> Result<Prepared> {
    let host = effective_host(cfg);
    locks(cfg, &job.what, &host)?;
    let root = cfg.root();
    std::fs::create_dir_all(&root)?;
    drop(take_lock(&root)?);
    let real_vps = is_real_vps(&cfg.server, &host);

    if job.what == What::Undo {
        let remote = Remote::new(cfg);
        refuse_if_running(&remote)?;
        let (id, preview) = last_undo(cfg, &remote)?;
        return Ok(Prepared {
            job: job.clone(),
            night: String::new(),
            night_dir: PathBuf::new(),
            preview,
            real_vps,
            undo_id: Some(id),
        });
    }

    let (night, night_dir) = pick_night(cfg, job.night.as_deref())?;
    check_backup(&night_dir, &job.what).context("The backup failed its check, so nothing was changed")?;
    let remote = Remote::new(cfg);
    remote.run("true").context("Cannot reach the server")?;
    refuse_if_running(&remote)?;
    if job.what == What::Everything {
        refuse_if_not_blank(&remote)?;
    }
    let lines = compare::compare(cfg, &night_dir)?;
    let mut preview = vec![];
    for l in lines.iter().filter(|l| wanted_line(&job.what, l)) {
        let who = if l.who.is_empty() { String::new() } else { format!("{} ", l.who) };
        let mut result = l.result.clone();
        if result.starts_with("could not check") {
            result = "not on the server, or not readable".into();
        }
        preview.push(format!("{who}{}: {result}", l.what));
    }
    if let What::Customer(c) = &job.what {
        preview.extend(code_note(&remote, &night_dir, c));
    }
    Ok(Prepared { job: job.clone(), night, night_dir, preview, real_vps, undo_id: None })
}

/// A restore doesn't touch code that's already there, but the user should know if it differs.
fn code_note(remote: &Remote, night_dir: &Path, c: &str) -> Option<String> {
    let backup = code_commit(night_dir, c);
    let live = remote.run(&format!("git -C {}/deployed/{c}/app rev-parse HEAD 2>/dev/null; true", remote.base)).ok()?;
    let live = live.trim();
    (!live.is_empty() && live != backup).then(|| {
        format!("{c} code: the server runs {live:.8}, the backup had {backup:.8}. The code stays as it is.")
    })
}

fn code_commit(night_dir: &Path, c: &str) -> String {
    std::fs::read_to_string(night_dir.join("customers").join(c).join("code-version.txt"))
        .unwrap_or_default()
        .lines()
        .next()
        .unwrap_or_default()
        .trim()
        .to_string()
}

fn refuse_if_not_blank(remote: &Remote) -> Result<()> {
    let script = format!(
        r#"[ -e ~/mptool-undo/.everything ] && exit 0; cd {} 2>/dev/null || exit 0; ls deployed/*/app/instance/minipass.db 2>/dev/null | head -1; find maildata -mindepth 1 -print -quit 2>/dev/null; true"#,
        remote.base
    );
    if !remote.run(&script)?.trim().is_empty() {
        bail!("This server isn't blank: it has customers or mail. Nothing was changed. Restore the parts you need instead.");
    }
    Ok(())
}

// ── Going ahead ──

/// One thing being put back: what to send over and the script that deals with it.
struct Part {
    id: String,
    label: String,
    vars: Vec<(&'static str, String)>,
    body: &'static str,
    /// Local file or folder, name inside payload/ on the server.
    send: Vec<(PathBuf, String)>,
    /// What the undo copy will hold (relative to the base folder), to check the disk space.
    saves: Vec<String>,
    rollback: bool,
    /// Sudo commands this part can't work without.
    sudo: Vec<String>,
}

fn quoted(s: &str) -> String {
    format!("'{}'", s.replace('\'', r#"'\''"#))
}

fn customer_url(cfg: &Config, c: &str) -> String {
    cfg.monitor_url.replace("{customer}", c)
}

fn customer_part(cfg: &Config, night: &Path, c: &str, rollback: bool) -> Part {
    let dir = night.join("customers").join(c);
    Part {
        id: format!("customer-{c}"),
        label: format!("Customer {c}"),
        vars: vec![
            ("C", c.to_string()),
            ("COMMIT", code_commit(night, c)),
            ("REPO_APP", cfg.repo_app.clone()),
            ("URL", customer_url(cfg, c)),
            ("EXPECT", cfg.monitor_expect.clone()),
        ],
        body: BODY_CUSTOMER,
        send: vec![
            (dir.join("minipass.db"), "minipass.db".into()),
            (dir.join("uploads/"), "uploads/".into()),
            (dir.join(".env"), "env".into()),
            (dir.join("docker-compose.yml"), "docker-compose.yml".into()),
        ],
        saves: vec![format!("deployed/{c}/app/instance"), format!("deployed/{c}/app/static/uploads")],
        rollback,
        sudo: vec![],
    }
}

fn mail_settings_part(night: &Path, tmp: &Path, parts: &[String]) -> Result<Part> {
    let tar = tmp.join("settings.tar");
    tar_filter(&night.join("mail/mail.tar"), &tar, &|name| {
        is_setting(name) && parts.iter().any(|p| p == setting_part(mail_group(name)))
    })?;
    Ok(Part {
        id: "mail-settings".into(),
        label: "Mail settings".into(),
        vars: vec![("PARTS", parts.join(" "))],
        body: BODY_MAIL_SETTINGS,
        send: vec![(tar, "settings.tar".into())],
        saves: vec!["config".into()],
        rollback: true,
        sudo: vec![],
    })
}

fn mail_messages_part(night: &Path, tmp: &Path, remote: &Remote) -> Result<Option<Part>> {
    let tar = night.join("mail/mail.tar");
    let backup: Vec<String> = tar_entries(&tar)?.into_iter().map(|e| e.name).collect();
    // Listed through a fresh mail container: it works with mail stopped, and sees the real folder.
    let listing = remote
        .run(&format!(
            "cd {} && docker-compose run --rm --no-deps -T --entrypoint find mailserver /var/mail 2>/dev/null",
            remote.base
        ))
        .context("Cannot look inside the mail folders. Is the mail server's compose file there? `restore server` puts it back")?;
    let server: Vec<String> =
        listing.lines().filter_map(|l| l.strip_prefix('/')).filter(|l| l.starts_with("var/mail")).map(String::from).collect();
    let add = mail_to_add(&backup, &server);
    if add.is_empty() {
        return Ok(None);
    }
    let wanted: HashSet<&str> = add.iter().map(String::as_str).collect();
    let small = tmp.join("messages.tar");
    tar_filter(&tar, &small, &|name| wanted.contains(name))?;
    let list = tmp.join("added.txt");
    std::fs::write(&list, add.join("\n") + "\n")?;
    let accounts = tmp.join("accounts.txt");
    std::fs::write(&accounts, mail_accounts(&add).join("\n") + "\n")?;
    let count = add.iter().filter(|p| mail_key(p).is_some()).count();
    Ok(Some(Part {
        id: "mail-messages".into(),
        label: format!("Mail messages ({count} to add back)"),
        vars: vec![],
        body: BODY_MAIL_MESSAGES,
        send: vec![(small, "messages.tar".into()), (list, "added.txt".into()), (accounts, "accounts.txt".into())],
        saves: vec![],
        rollback: true,
        sudo: vec![],
    }))
}

fn mail_full_part(night: &Path, tmp: &Path) -> Result<Part> {
    let tar = tmp.join("mail-full.tar");
    tar_filter(&night.join("mail/mail.tar"), &tar, &full_mail_keeps)?;
    Ok(Part {
        id: "mail-full".into(),
        label: "Mail (settings, messages, state)".into(),
        vars: vec![],
        body: BODY_MAIL_FULL,
        send: vec![(tar, "mail-full.tar".into())],
        saves: vec![],
        rollback: false,
        sudo: vec![],
    })
}

fn website_part(cfg: &Config, night: &Path, rollback: bool) -> Part {
    let dir = night.join("website");
    Part {
        id: "website".into(),
        label: "Website".into(),
        vars: vec![("URL", cfg.website_url.clone()), ("EXPECT", cfg.website_expect.clone())],
        body: BODY_WEBSITE,
        send: vec![
            (dir.join("customers.db"), "customers.db".into()),
            (dir.join("monitoring.db"), "monitoring.db".into()),
            (dir.join(".env"), "env".into()),
        ],
        saves: vec!["MinipassWebSite/customers.db".into(), "email_monitoring/monitoring.db".into()],
        rollback,
        sudo: vec!["systemctl restart minipass-web".into()],
    }
}

fn server_part(cfg: &Config, night: &Path, remote: &Remote, blank: bool) -> Part {
    let dir = night.join("server");
    let saved = std::fs::read_to_string(dir.join("minipass-web.service")).unwrap_or_default();
    let live = remote.run(&format!("cat {SERVICE_FILE} 2>/dev/null; true")).unwrap_or_default();
    let mut sudo = vec![];
    if saved != live {
        sudo.push(format!("install -m 644 -o root -g root $STAGE/minipass-web.service {SERVICE_FILE}"));
        sudo.push("systemctl daemon-reload".into());
        sudo.push("systemctl enable minipass-web".into());
        sudo.push("systemctl restart minipass-web".into());
    }
    Part {
        id: "server".into(),
        label: "Server settings and certificates".into(),
        vars: vec![
            ("MONITOR_URL", cfg.monitor_url.clone()),
            ("EXPECT", cfg.monitor_expect.clone()),
            ("WEBSITE_URL", cfg.website_url.clone()),
            ("WEBSITE_EXPECT", cfg.website_expect.clone()),
            ("BLANK", if blank { "1" } else { "0" }.to_string()),
        ],
        body: BODY_SERVER,
        send: vec![
            (dir.join(".env"), "env".into()),
            (dir.join("mailserver.env"), "mailserver.env".into()),
            (dir.join("docker-compose.yml"), "docker-compose.yml".into()),
            (dir.join("nginx/"), "nginx/".into()),
            (dir.join("vhost.d/"), "vhost.d/".into()),
            (dir.join("certs.tar"), "certs.tar".into()),
            (dir.join(".mailpass"), "mailpass".into()),
            (dir.join("minipass-web.service"), "minipass-web.service".into()),
            (dir.join("crontab.txt"), "crontab.txt".into()),
        ],
        saves: vec!["nginx".into(), "vhost.d".into(), "certs".into(), "acme".into()],
        rollback: !blank,
        sudo,
    }
}

fn local_size(path: &Path) -> u64 {
    if path.is_dir() {
        crate::status::folder_size(path)
    } else {
        std::fs::metadata(path).map(|m| m.len()).unwrap_or(0)
    }
}

/// Enough room for what gets sent plus the undo copy, with some to spare.
fn check_disk(remote: &Remote, home: &str, part: &Part) -> Result<()> {
    let send: u64 = part.send.iter().map(|(p, _)| local_size(p)).sum::<u64>() / 1024;
    let free = remote.run(&format!("df -Pk {home} | awk 'NR==2 {{print $4}}'"))?;
    let free: u64 = free.trim().parse().context("cannot read the free disk space")?;
    let mut undo = 0;
    if !part.saves.is_empty() {
        let script = format!(
            "cd {} 2>/dev/null && du -sk {} 2>/dev/null | awk '{{s+=$1}} END {{print s+0}}'; true",
            remote.base,
            part.saves.join(" ")
        );
        undo = remote.run(&script)?.trim().parse().unwrap_or(0);
    }
    let needed = send + undo + undo / 10 + 50 * 1024;
    if free < needed {
        bail!("Not enough disk space on the server: {} free, {} needed. Nothing was changed.", human(free * 1024), human(needed * 1024));
    }
    Ok(())
}

fn check_sudo(remote: &Remote, stage: &str, part: &Part) -> Result<()> {
    for cmd in &part.sudo {
        let cmd = cmd.replace("$STAGE", stage);
        if remote.run(&format!("sudo -n -l {cmd} >/dev/null 2>&1")).is_err() {
            bail!("sudo isn't allowed on the server for: {cmd}. Nothing was changed. Allow it with visudo, then run this again.");
        }
    }
    Ok(())
}

fn launch(remote: &Remote, stage: &str, mode: &str) -> Result<usize> {
    let out = remote.run(&format!(
        "cd {stage} && touch log && wc -l < log && (setsid nohup bash job.sh {mode} >> log 2>&1 < /dev/null & echo $! > pid)"
    ))?;
    Ok(out.trim().parse().unwrap_or(0))
}

/// Follow the script on the server until it finishes, and give back its result line.
fn follow(remote: &Remote, stage: &str, result: &str, mut seen: usize, on: &dyn Fn(Event)) -> Result<String> {
    let mut lost: Option<Instant> = None;
    loop {
        std::thread::sleep(Duration::from_secs(2));
        let script = format!(
            r#"cd {stage} || exit 0; tail -n +{} log 2>/dev/null; echo __result__; cat {result} 2>/dev/null; echo __alive__; p=$(cat pid); grep -qs job.sh "/proc/$p/cmdline" && echo yes; true"#,
            seen + 1
        );
        let out = match remote.run(&script) {
            Ok(o) => o,
            Err(_) => {
                let since = *lost.get_or_insert_with(Instant::now);
                if since.elapsed() > LOST_LIMIT {
                    bail!(
                        "Lost the connection to the server. The restore keeps going there by itself, and undoes itself if its check fails. \
                         When the connection is back, `mptool restore status` shows how it ended, and `mptool restore undo` puts things back as they were before it."
                    );
                }
                on(Event::Log("Connection lost, trying again...".into()));
                continue;
            }
        };
        lost = None;
        let (log, rest) = out.split_once("__result__\n").unwrap_or((&out, ""));
        let (res, alive) = rest.split_once("__alive__\n").unwrap_or((rest, ""));
        for line in log.lines() {
            seen += 1;
            // Docker-compose's complaint about an old setting in the file: true, and just noise here.
            if !line.contains("level=warning") {
                on(Event::Log(format!("  {line}")));
            }
        }
        if !res.trim().is_empty() {
            return Ok(res.trim().to_string());
        }
        if alive.trim().is_empty() {
            bail!(
                "The restore was stopped halfway on the server, and nothing checked it. To go back to how it was: `mptool restore undo`. To try again: run the same restore again."
            );
        }
    }
}

fn run_part(cfg: &Config, remote: &Remote, home: &str, part: Part, on: &dyn Fn(Event)) -> Result<String> {
    let id = format!("{}-{}", Local::now().format("%Y%m%d-%H%M%S"), part.id);
    let stage = format!("{home}/mptool-undo/{id}");
    on(Event::Log(format!("Checking disk space and permissions for {}", part.label)));
    check_sudo(remote, &stage, &part)?;
    check_disk(remote, home, &part)?;

    remote.run(&format!("mkdir -p -m 700 {home}/mptool-undo && mkdir -m 700 {stage} && mkdir -m 700 {stage}/payload"))?;
    on(Event::Log(format!("Sending {} to the server", part.label)));
    for (local, name) in &part.send {
        remote.push(local, &format!("{stage}/payload/{name}"))?;
    }

    let mut script = String::new();
    let mut vars = vec![
        ("BASE", remote.base.clone()),
        ("STAGE", stage.clone()),
        ("HOME_DIR", home.to_string()),
        ("SERVICE", SERVICE_FILE.to_string()),
        ("ROLLBACK", if part.rollback { "1" } else { "0" }.to_string()),
    ];
    vars.extend(part.vars.clone());
    let names: Vec<&str> = vars.iter().map(|(n, _)| *n).collect();
    for (name, value) in &vars {
        script += &format!("export {name}={}\n", quoted(value));
    }
    script += &format!("export EXPORTS={}\n", quoted(&names.join(" ")));
    script += PRELUDE;
    script += part.body;
    script += MAIN;
    remote.run_input(&format!("cat > {stage}/job.sh"), &script)?;
    let meta = format!("{}\n{}\n", part.label, if part.rollback { "undo" } else { "noundo" });
    remote.run_input(&format!("cat > {stage}/meta"), &meta)?;
    write_imap_secret(cfg, remote, &stage)?;

    on(Event::Log(format!("Putting back {}. From here it runs on the server by itself.", part.label)));
    let seen = launch(remote, &stage, "apply")?;
    let result = follow(remote, &stage, "result", seen, on)?;
    explain(&result)
}

/// The password for the mail check goes over stdin, into a file only this user can read.
/// The script removes it when it ends.
fn write_imap_secret(cfg: &Config, remote: &Remote, stage: &str) -> Result<()> {
    if cfg.mail_check_user.is_empty() {
        return Ok(());
    }
    let text = format!("a LOGIN \"{}\" \"{}\"\r\nb LOGOUT\r\n", cfg.mail_check_user, cfg.mail_check_password);
    remote.run_input(&format!("umask 077; cat > {stage}/imap.txt"), &text)?;
    Ok(())
}

/// Turn the script's result line into words.
fn explain(result: &str) -> Result<String> {
    let (code, why) = result.split_once('\t').unwrap_or((result, ""));
    match code {
        "ok" => Ok(if why.is_empty() { "put back and checked".into() } else { why.to_string() }),
        "rolled_back" => bail!("It didn't work ({why}), so the undo copy was put back automatically. The server is as it was before."),
        "rollback_failed" => bail!(
            "It didn't work ({why}), and the check still fails after putting the undo copy back. Look at the server now. The files are as they were before the restore."
        ),
        "failed" => bail!("{why}"),
        other => bail!("Unexpected answer from the server: {other} {why}"),
    }
}

fn prune_undo(remote: &Remote) {
    let script = format!(
        r#"cd ~/mptool-undo 2>/dev/null || exit 0; ls -1 | sort -r | tail -n +{} | while read -r d; do rm -rf -- "$d" 2>/dev/null; done; true"#,
        KEEP_UNDO + 1
    );
    let _ = remote.run(&script);
}

fn step(name: &str, r: Result<String>, on: &dyn Fn(Event)) -> Step {
    let (ok, detail) = match r {
        Ok(d) => (true, d),
        Err(e) => (false, format!("{e:#}")),
    };
    let s = Step { name: name.to_string(), ok, detail };
    on(Event::Done(s.clone()));
    s
}

/// A temporary folder for what gets sent to the server. It can hold keys, so it stays in the backup
/// root (only this user can read that), and it's removed afterwards.
struct TempDir(PathBuf);

impl TempDir {
    fn new(root: &Path) -> Result<TempDir> {
        let dir = root.join(format!(".restore-{}", std::process::id()));
        std::fs::create_dir_all(&dir)?;
        Ok(TempDir(dir))
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Run a prepared restore. `typed` is what the user typed to confirm.
pub fn execute(cfg: &Config, p: &Prepared, typed: &str, snapshot_taken: bool, on: &dyn Fn(Event)) -> Result<Report> {
    locks(cfg, &p.job.what, &effective_host(cfg))?;
    if typed.trim() != p.job.what.confirm_word() {
        bail!("You typed \"{}\", not \"{}\". Nothing was changed.", typed.trim(), p.job.what.confirm_word());
    }
    if p.real_vps && !snapshot_taken {
        bail!("Take a Hetzner snapshot first. Nothing was changed.");
    }
    let root = cfg.root();
    let _lock = take_lock(&root)?;
    let started = Instant::now();
    let remote = Remote::new(cfg);
    let home = remote.run("echo $HOME").context("Cannot reach the server")?.trim().to_string();
    refuse_if_running(&remote)?;
    prune_undo(&remote);
    let tmp = TempDir::new(&root)?;

    let night = p.night_dir.as_path();
    let mut steps = vec![];
    match &p.job.what {
        What::Customer(c) => {
            on(Event::Start(format!("Customer {c}")));
            let r = run_part(cfg, &remote, &home, customer_part(cfg, night, c, true), on);
            steps.push(step(&format!("Customer {c}"), r, on));
        }
        What::MailSettings(parts) => {
            on(Event::Start("Mail settings".into()));
            let r = mail_settings_part(night, &tmp.0, parts).and_then(|part| run_part(cfg, &remote, &home, part, on));
            steps.push(step("Mail settings", r, on));
        }
        What::MailMessages => {
            on(Event::Start("Mail messages".into()));
            let r = mail_messages_part(night, &tmp.0, &remote).and_then(|part| match part {
                Some(part) => run_part(cfg, &remote, &home, part, on),
                None => Ok("nothing was missing".into()),
            });
            steps.push(step("Mail messages", r, on));
        }
        What::Website => {
            on(Event::Start("Website".into()));
            let r = run_part(cfg, &remote, &home, website_part(cfg, night, true), on);
            steps.push(step("Website", r, on));
        }
        What::Server => {
            on(Event::Start("Server settings and certificates".into()));
            let r = run_part(cfg, &remote, &home, server_part(cfg, night, &remote, false), on);
            steps.push(step("Server settings and certificates", r, on));
        }
        What::Everything => steps = everything(cfg, night, &remote, &home, &tmp.0, on),
        What::Undo => {
            let id = p.undo_id.as_deref().context("nothing to undo")?;
            on(Event::Start("Undo".into()));
            let stage = format!("{home}/mptool-undo/{id}");
            let r = write_imap_secret(cfg, &remote, &stage)
                .and_then(|_| launch(&remote, &stage, "undo"))
                .and_then(|seen| follow(&remote, &stage, "undo-result", seen, on))
                .and_then(|res| explain(&res));
            steps.push(step("Undo", r, on));
        }
    }
    Ok(Report { label: p.job.what.label(), night: p.night.clone(), steps, secs: started.elapsed().as_secs() })
}

/// A blank server, in order: code, server settings and certificates, mail, website, customers.
/// A part that fails is reported, and the next one still gets its chance.
fn everything(cfg: &Config, night: &Path, remote: &Remote, home: &str, tmp: &Path, on: &dyn Fn(Event)) -> Vec<Step> {
    let _ = remote.run(&format!("mkdir -p -m 700 {home}/mptool-undo && touch {home}/mptool-undo/.everything"));
    on(Event::Start("Code".into()));
    let mut steps = vec![step("Code", get_code(cfg, remote), on)];
    let mut part = |name: &str, build: &mut dyn FnMut() -> Result<Part>| {
        on(Event::Start(name.to_string()));
        let r = build().and_then(|p| run_part(cfg, remote, home, p, on));
        steps.push(step(name, r, on));
    };
    part("Server settings and certificates", &mut || Ok(server_part(cfg, night, remote, true)));
    part("Mail", &mut || mail_full_part(night, tmp));
    part("Website", &mut || Ok(website_part(cfg, night, false)));
    let mut skipped = vec![];
    for c in dir_names(&night.join("customers")) {
        if cfg.restore_allowed.contains(&c) {
            part(&format!("Customer {c}"), &mut || Ok(customer_part(cfg, night, &c, false)));
        } else {
            skipped.push(c);
        }
    }
    if !skipped.is_empty() {
        steps.push(step(
            "Customers not on restore_allowed",
            Err(anyhow::anyhow!("left out: {}. Add them to the config, then restore each one.", skipped.join(", "))),
            on,
        ));
    }
    if steps.iter().all(|s| s.ok) {
        let _ = remote.run("rm -f ~/mptool-undo/.everything");
    }
    steps
}

fn get_code(cfg: &Config, remote: &Remote) -> Result<String> {
    let script = format!(
        r#"set -e
BASE={base}
if [ -d "$BASE/.git" ]; then echo already there; else
    mkdir -p "$BASE"
    git clone -q {repo} "$BASE"
    echo fetched
fi
cd "$BASE"
mkdir -p config maildata mailstate html certs acme vhost.d deployed email_monitoring
"#,
        base = quoted(&remote.base),
        repo = quoted(&cfg.repo_env)
    );
    let out = remote.run_input("bash -s", &script).context("Could not get the code")?;
    Ok(format!("code {}", out.trim()))
}

// ── Undo ──

/// The newest restore that can still be undone, and what it did.
fn last_undo(cfg: &Config, remote: &Remote) -> Result<(String, Vec<String>)> {
    let script = r#"cd ~/mptool-undo 2>/dev/null || exit 0
for d in $(ls -1 | sort -r); do
    [ -f "$d/saved" ] || continue
    case "$(cat "$d/undo-result" 2>/dev/null)" in ok*) continue;; esac
    case "$(cat "$d/result" 2>/dev/null)" in rolled_back*|rollback_failed*) continue;; esac
    echo "$d"; cat "$d/meta"
    exit 0
done"#;
    let out = remote.run(script)?;
    let mut lines = out.lines();
    let Some(id) = lines.next() else { bail!("There's nothing to undo.") };
    let label = lines.next().unwrap_or_default();
    if lines.next() == Some("noundo") {
        bail!("The last restore was a whole-server restore. It put things on a blank server, so there's nothing to go back to.");
    }
    if let Some(c) = id.split("-customer-").nth(1) {
        if !cfg.restore_allowed.iter().any(|a| a == c) {
            bail!("{c} is not on restore_allowed in the config. Nothing was changed.");
        }
    }
    let when = id.split('-').take(2).collect::<Vec<_>>().join(" ");
    Ok((id.to_string(), vec![format!("Goes back to how things were before: {label} (restored {when})")]))
}

/// How the last restore ended, even if the tool that started it is long gone.
fn print_status(cfg: &Config) -> Result<()> {
    let remote = Remote::new(cfg);
    let script = r#"cd ~/mptool-undo 2>/dev/null || exit 0
d=$(ls -1 | sort -r | head -1); [ -n "$d" ] || exit 0
echo "$d"; head -1 "$d/meta"; p=$(cat "$d/pid" 2>/dev/null)
if grep -qs job.sh "/proc/$p/cmdline"; then echo running
elif [ -f "$d/undo-result" ]; then echo "undo: $(cat "$d/undo-result")"
elif [ -f "$d/result" ]; then cat "$d/result"
else echo stopped; fi"#;
    let out = remote.run(script)?;
    let mut lines = out.lines();
    let Some(id) = lines.next() else {
        println!("No restore has run on this server.");
        return Ok(());
    };
    let label = lines.next().unwrap_or_default();
    let state = lines.next().unwrap_or_default();
    println!("Last restore: {label} ({id})");
    if state == "running" {
        println!("It's still running on the server.");
    } else if state == "stopped" {
        println!("It was stopped halfway. Go back with `mptool restore undo`, or run the same restore again.");
    } else if let Some(undo) = state.strip_prefix("undo: ") {
        println!("It was undone. {}", explain(undo).unwrap_or_else(|e| format!("{e:#}")));
    } else {
        println!("{}", explain(state).unwrap_or_else(|e| format!("{e:#}")));
    }
    Ok(())
}

// ── The commands ──

pub fn parse(args: &[String]) -> Result<(Job, Option<String>, bool)> {
    let (mut words, mut night, mut confirm, mut only, mut snapshot) = (vec![], None, None, None, false);
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--night" => night = Some(it.next().context("--night needs a date")?.clone()),
            "--confirm" => confirm = Some(it.next().context("--confirm needs the text")?.clone()),
            "--only" => only = Some(it.next().context("--only needs accounts,forwards,dkim")?.clone()),
            "--snapshot-taken" => snapshot = true,
            w if w.starts_with("--") => bail!("Unknown option {w}"),
            w => words.push(w),
        }
    }
    let what = match words.as_slice() {
        ["customer", name] => What::Customer(name.to_string()),
        ["mail", "settings"] => {
            let parts: Vec<String> = match &only {
                Some(list) => list.split(',').map(|p: &str| p.trim().to_string()).collect(),
                None => MAIL_PARTS.iter().map(|p| p.to_string()).collect(),
            };
            if let Some(bad) = parts.iter().find(|p| !MAIL_PARTS.contains(&p.as_str())) {
                bail!("--only takes accounts, forwards and dkim, not {bad}");
            }
            What::MailSettings(parts)
        }
        ["mail", "messages"] => What::MailMessages,
        ["website"] => What::Website,
        ["server"] => What::Server,
        ["everything"] => What::Everything,
        ["undo"] => What::Undo,
        _ => bail!("Which restore? See `mptool help`."),
    };
    Ok((Job { what, night }, confirm, snapshot))
}

fn ask(question: &str) -> String {
    println!("{question}");
    let mut answer = String::new();
    let _ = std::io::stdin().lock().read_line(&mut answer);
    answer.trim().to_string()
}

fn run_cli(cfg: &Config, args: &[String]) -> Result<bool> {
    let (job, confirm, mut snapshot) = parse(args)?;
    let p = prepare(cfg, &job)?;
    let from = if p.night.is_empty() { String::new() } else { format!(" from the night of {}", p.night) };
    println!("Restore {}{from}", job.what.label());
    println!("What differs now (nothing has changed yet):");
    for line in &p.preview {
        println!("  {line}");
    }
    if p.real_vps && !snapshot {
        snapshot = ask("This is the real server. Did you take a Hetzner snapshot? Type yes:") == "yes";
    }
    let typed = match confirm {
        Some(t) => t,
        None => ask(&format!("Type {} to go ahead:", job.what.confirm_word())),
    };
    let report = execute(cfg, &p, &typed, snapshot, &crate::run::print_event)?;
    for s in &report.steps {
        println!("{} {}: {}", if s.ok { "OK    " } else { "FAILED" }, s.name, s.detail);
    }
    println!("Restore of {} {}. Took {}.", report.label, if report.ok() { "OK" } else { "FAILED" }, report.took());
    if let Err(e) = notify::send(cfg, &report.discord()) {
        eprintln!("Discord message not sent: {e:#}");
    }
    Ok(report.ok())
}

/// Runs a prepared restore and sends the same Discord report as the command, for the menu.
pub fn execute_and_report(cfg: &Config, p: &Prepared, typed: &str, snapshot: bool, on: &dyn Fn(Event)) -> Result<Report> {
    let report = execute(cfg, p, typed, snapshot, on)?;
    let _ = notify::send(cfg, &report.discord());
    Ok(report)
}

pub fn cli(cfg: &Config, args: &[String]) -> ExitCode {
    if args.first().map(String::as_str) == Some("status") {
        return match print_status(cfg) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("{e:#}");
                ExitCode::FAILURE
            }
        };
    }
    match run_cli(cfg, args) {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(e) => {
            eprintln!("Restore refused or stopped: {e:#}");
            let _ = notify::send(cfg, &format!("**minipass restore FAILED**\n- {e:#}"));
            ExitCode::FAILURE
        }
    }
}

/// `mptool compare`: the menu's Compare, as text.
pub fn compare_cli(cfg: &Config, args: &[String]) -> ExitCode {
    let wanted = args.iter().position(|a| a == "--night").and_then(|i| args.get(i + 1));
    let result = pick_night(cfg, wanted.map(String::as_str)).and_then(|(night, dir)| {
        println!("Backup {night} vs the server now. Nothing is changed.");
        compare::compare(cfg, &dir)
    });
    match result {
        Ok(lines) => {
            let differs = lines.iter().filter(|l| l.differs).count();
            for l in &lines {
                println!("{} {}: {} {}: {}", if l.differs { "DIFF" } else { "same" }, l.group, l.who, l.what, l.result);
            }
            println!("{differs} differ");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("{e:#}");
            ExitCode::FAILURE
        }
    }
}

// ── What runs on the server ──

const PRELUDE: &str = r#"set -u
MODE="${1:-apply}"
cd "$STAGE" || exit 1
RESULT="$STAGE/result"
[ "$MODE" = undo ] && RESULT="$STAGE/undo-result"
ME=$(id -u); export ME
EXPORTS="$EXPORTS ME"
finish() { printf '%b\n' "$1" > "$RESULT.tmp"; mv "$RESULT.tmp" "$RESULT"; rm -f "$STAGE/imap.txt"; exit 0; }

# A short-lived container running as root, for files that root or another user may own.
as_root() {
    local envs=(); local v
    for v in $EXPORTS; do envs+=(-e "$v"); done
    docker run --rm "${envs[@]}" -v "$HOME_DIR:$HOME_DIR" -v "$BASE:$BASE" --entrypoint sh nginx:alpine -c "$1"
}

project_stop() {
    local ids; ids=$(docker ps -q --filter "label=com.docker.compose.project=$1")
    [ -z "$ids" ] || docker stop -t 10 $ids >/dev/null
}

page_ok() {
    local host; host=$(printf '%s' "$1" | sed -E 's#^https?://([^/:]+).*#\1#')
    local args=(-sk --max-time 10 --resolve "$host:443:127.0.0.1" --resolve "$host:80:127.0.0.1")
    if [ -n "$2" ]; then
        curl "${args[@]}" "$1" | grep -qF -- "$2"
    else
        [ "$(curl "${args[@]}" -o /dev/null -w '%{http_code}' "$1")" = 200 ]
    fi
}
wait_page() { local i; for i in $(seq 40); do page_ok "$1" "$2" && return 0; sleep 3; done; return 1; }

# ── mail, shared ──
MAIL_ENV=()
mail_run() { ( cd "$BASE" && docker-compose run --rm --no-deps -T -v "$STAGE:/stage" "${MAIL_ENV[@]}" --entrypoint bash mailserver "$@" ); }
mail_recreate() { ( cd "$BASE" && docker-compose up -d --no-deps --force-recreate mailserver ); }
mail_ready() {
    docker exec mailserver bash -c 'exec 3<>/dev/tcp/127.0.0.1/25 && exec 4<>/dev/tcp/127.0.0.1/143' 2>/dev/null || return 1
    docker exec mailserver sh -c 'u=$(cut -d"|" -f1 /tmp/docker-mailserver/postfix-accounts.cf 2>/dev/null | head -1); [ -z "$u" ] || doveadm user "$u"' >/dev/null 2>&1
}
wait_mail() { local i; for i in $(seq 120); do mail_ready && return 0; sleep 2; done; return 1; }
mail_key_hash() { docker exec mailserver sh -c '[ -d /etc/opendkim/keys ] && cd /etc/opendkim/keys && find . -name "*.private" | sort | xargs -r cat | sha256sum'; }
mail_check() {
    wait_mail || { CHECK_WHY="mail did not come back"; return 1; }
    local i
    if docker exec mailserver sh -c 'ls /etc/opendkim/keys/*/*.private' >/dev/null 2>&1; then
        for i in $(seq 30); do docker exec mailserver pgrep -x opendkim >/dev/null && break; sleep 2; done
        docker exec mailserver pgrep -x opendkim >/dev/null || { CHECK_WHY="mail signing (OpenDKIM) is not running"; return 1; }
    fi
    if [ -f "$STAGE/dkim-expected" ] && [ "$(mail_key_hash)" != "$(cat "$STAGE/dkim-expected")" ]; then
        CHECK_WHY="mail is not signing with the key from the backup"; return 1
    fi
    if [ -f "$STAGE/imap.txt" ]; then
        for i in $(seq 15); do
            docker exec -i mailserver sh -c 'timeout 15 openssl s_client -quiet -connect 127.0.0.1:993 2>/dev/null' < "$STAGE/imap.txt" | grep -q '^a OK' && return 0
            sleep 2
        done
        CHECK_WHY="the IMAP login failed"; return 1
    fi
}
"#;

const MAIN: &str = r#"
CHECK_WHY=""
if [ "$MODE" = apply ]; then
    echo "Saving an undo copy"
    part_stop
    if ! part_save; then
        part_start >/dev/null 2>&1
        finish "failed\tCould not save the undo copy. Nothing was changed."
    fi
    touch "$STAGE/saved"
    why=""
    echo "Putting the files back"
    part_put || why="could not put the files back"
    if [ -z "$why" ]; then echo "Starting"; part_start || why="${CHECK_WHY:-it did not start}"; fi
    if [ -z "$why" ]; then echo "Checking"; part_check || why="${CHECK_WHY:-the check failed}"; fi
    rm -rf "$STAGE/payload"
    [ -z "$why" ] && finish ok
    [ "$ROLLBACK" = 1 ] || finish "failed\t$why"
    echo "It did not work: $why. Putting the undo copy back."
    part_stop
    CHECK_WHY=""
    if part_unput && part_start && part_check; then finish "rolled_back\t$why"; fi
    finish "rollback_failed\t$why"
else
    echo "Putting the undo copy back"
    part_stop
    part_unput || finish "failed\tCould not put the undo copy back."
    if part_start && part_check; then finish ok; fi
    finish "ok\tThe files are back as they were, but the check says: ${CHECK_WHY:-it did not start}. That may be how it was before."
fi
"#;

const BODY_CUSTOMER: &str = r#"
D="$BASE/deployed/$C"
part_stop() { project_stop "$C"; }
part_save() {
    [ -d "$D/app/.git" ] || touch "$STAGE/fresh"
    mkdir -p "$D" || return 1
    as_root '
        set -e; cd "$BASE/deployed/$C"; L=""
        for f in app/instance/minipass.db app/instance/minipass.db-wal app/instance/minipass.db-shm app/static/uploads app/.env docker-compose.yml; do
            [ -e "$f" ] && L="$L $f"
        done
        if [ -n "$L" ]; then tar -cf "$STAGE/undo.tar" $L; else head -c 1024 /dev/zero > "$STAGE/undo.tar"; fi
        chown "$ME" "$STAGE/undo.tar"'
}
part_put() {
    if [ -f "$STAGE/fresh" ]; then
        echo "The folder is gone: getting the code, version ${COMMIT:-newest}"
        as_root 'rm -rf "$BASE/deployed/$C/app"; chown "$ME" "$BASE/deployed/$C"' || return 1
        git clone -q "$REPO_APP" "$D/app" || return 1
        if [ -n "$COMMIT" ]; then git -C "$D/app" checkout -q "$COMMIT" || return 1; fi
    fi
    as_root '
        set -e; cd "$BASE/deployed/$C"; P="$STAGE/payload"
        mkdir -p app/instance app/static
        rm -f app/instance/minipass.db-wal app/instance/minipass.db-shm
        cp "$P/minipass.db" app/instance/minipass.db.new
        mv -f app/instance/minipass.db.new app/instance/minipass.db
        rm -rf app/static/uploads
        cp -a "$P/uploads" app/static/uploads
        cp "$P/env" app/.env
        cp "$P/docker-compose.yml" docker-compose.yml
        chown -R 1000:1000 app/instance app/static/uploads app/.env docker-compose.yml' || return 1
    if [ -f "$STAGE/fresh" ]; then
        echo "Building the app"
        ( cd "$D" && docker-compose build -q ) || return 1
    fi
}
part_start() {
    [ -f "$D/docker-compose.yml" ] && [ -d "$D/app" ] || return 0
    ( cd "$D" && docker-compose up -d )
}
part_check() {
    if [ "$MODE" = undo ] && [ -f "$STAGE/fresh" ]; then return 0; fi
    [ "$(sqlite3 "$D/app/instance/minipass.db" 'PRAGMA integrity_check;' 2>&1)" = ok ] || { CHECK_WHY="the database is not healthy"; return 1; }
    wait_page "$URL" "$EXPECT" || { CHECK_WHY="the login page does not show the form"; return 1; }
}
part_unput() {
    if [ -f "$STAGE/fresh" ]; then
        docker ps -aq --filter "label=com.docker.compose.project=$C" | xargs -r docker rm -f >/dev/null
        as_root 'rm -rf "$BASE/deployed/$C"; mkdir -p "$BASE/deployed/$C"' || return 1
    else
        as_root 'cd "$BASE/deployed/$C"; rm -rf app/instance/minipass.db app/instance/minipass.db-wal app/instance/minipass.db-shm app/static/uploads app/.env docker-compose.yml' || return 1
    fi
    as_root 'set -e; cd "$BASE/deployed/$C"; tar --numeric-owner -xpf "$STAGE/undo.tar"; chown "$ME" .; [ -n "$(ls -A .)" ] || rmdir "$BASE/deployed/$C"'
}
"#;

const BODY_MAIL_SETTINGS: &str = r#"
MAIL_ENV=(-e "PARTS=$PARTS" -e "OWNER=$(id -u)")
cat > "$STAGE/inner.sh" <<'INNER'
cd /tmp/docker-mailserver || exit 1
sel() {
    for p in $PARTS; do
        case $p in
            accounts) find . -mindepth 1 -maxdepth 1 ! -name opendkim ! -name user-patches ! -name postfix-virtual.cf ! -name '*.lock' ;;
            forwards) find . -mindepth 1 -maxdepth 1 \( -name user-patches -o -name postfix-virtual.cf \) ;;
            dkim) find . -mindepth 1 -maxdepth 1 -name opendkim ;;
        esac
    done
}
key_hash() {
    case " $PARTS " in *" dkim "*)
        if [ -d opendkim/keys ]; then (cd opendkim/keys && find . -name '*.private' | sort | xargs -r cat | sha256sum) > /stage/dkim-expected; else rm -f /stage/dkim-expected; fi ;;
    esac
}
forwards() { ls -1 user-patches 2>/dev/null; }
case "$1" in
    save)
        sel > /stage/undo.list
        tar --numeric-owner -cpf /stage/undo.tar -T /stage/undo.list || exit 1
        ;;
    put)
        forwards > /stage/forwards-before
        sel | xargs -r rm -rf
        tar --numeric-owner -xpf /stage/payload/settings.tar --strip-components=2 || exit 1
        key_hash
        forwards > /stage/forwards-now
        ;;
    unput)
        forwards > /stage/forwards-before
        sel | xargs -r rm -rf
        tar --numeric-owner -xpf /stage/undo.tar || exit 1
        key_hash
        forwards > /stage/forwards-now
        ;;
esac
for f in forwards-before forwards-now undo.list undo.tar dkim-expected; do [ ! -e "/stage/$f" ] || chown "$OWNER" "/stage/$f"; done
true
INNER
part_stop() { :; }
part_save() { mail_run /stage/inner.sh save; }
part_put() { mail_run /stage/inner.sh put; }
part_unput() { mail_run /stage/inner.sh unput; }
part_start() { mail_recreate; }
sync_forwards() {
    case " $PARTS " in *" forwards "*) ;; *) return 0;; esac
    local u home
    for u in $(cat "$STAGE/forwards-now" 2>/dev/null); do
        case "$u" in *@*) ;; *) continue;; esac
        home="/var/mail/${u#*@}/${u%@*}/home"
        docker exec mailserver sh -c "[ -f '/tmp/docker-mailserver/user-patches/$u/sieve/forward.sieve' ]" || continue
        docker exec mailserver sh -c "mkdir -p '$home/sieve' && cp '/tmp/docker-mailserver/user-patches/$u/sieve/forward.sieve' '$home/sieve/forward.sieve' && chown -R docker:docker '$home' && doveadm sieve put -u '$u' forward '$home/sieve/forward.sieve' && doveadm sieve activate -u '$u' forward" \
            || return 1
    done
    # A forward that isn't in what was put back must stop forwarding.
    for u in $(cat "$STAGE/forwards-before" 2>/dev/null); do
        case "$u" in *@*) ;; *) continue;; esac
        grep -qxF "$u" "$STAGE/forwards-now" || docker exec mailserver doveadm sieve deactivate -u "$u" || true
    done
    return 0
}
part_check() {
    mail_check || return 1
    sync_forwards || { CHECK_WHY="could not turn the forwards back on"; return 1; }
}
"#;

const BODY_MAIL_MESSAGES: &str = r#"
cat > "$STAGE/inner.sh" <<'INNER'
case "$1" in
    put)
        tar --numeric-owner -xpf /stage/payload/messages.tar -C / || exit 1
        [ "$(stat -c %u /var/mail)" != 0 ] || chown docker:docker /var/mail
        ;;
    unput)
        tac /stage/added.txt | while IFS= read -r p; do
            case "$p" in var/mail/*) ;; *) continue;; esac
            case "$p" in
                */) rmdir "/$p" 2>/dev/null ;;
                *) rm -f "/$p" ;;
            esac
        done
        ;;
esac
true
INNER
accounts() { cat "$STAGE/accounts.txt" 2>/dev/null; }
part_stop() { :; }
part_save() { cp "$STAGE/payload/added.txt" "$STAGE/added.txt" && cp "$STAGE/payload/accounts.txt" "$STAGE/accounts.txt"; }
part_put() { mail_run /stage/inner.sh put; }
part_unput() { mail_run /stage/inner.sh unput; }
part_start() {
    # The running mail server may hold a folder that has since been deleted and made again.
    if [ "$(stat -c %i "$BASE/maildata" 2>/dev/null)" != "$(docker exec mailserver stat -c %i /var/mail 2>/dev/null)" ]; then
        echo "Mail isn't looking at the real folder: starting it fresh"
        mail_recreate || return 1
    fi
    wait_mail || return 1
    local u
    for u in $(accounts); do
        docker exec mailserver doveadm user "$u" >/dev/null 2>&1 || { echo "No account $u right now: its mail is back, but needs the account"; continue; }
        docker exec mailserver doveadm force-resync -u "$u" '*' || return 1
    done
}
part_check() {
    mail_check || return 1
    local u seen files
    for u in $(accounts); do
        docker exec mailserver doveadm user "$u" >/dev/null 2>&1 || continue
        seen=$(docker exec mailserver doveadm -f flow mailbox status -u "$u" messages '*' | sed -E 's/.*messages=//' | awk '{s+=$1} END {print s+0}')
        files=$(docker exec mailserver sh -c "find '/var/mail/${u#*@}/${u%@*}' -path '*/home' -prune -o -type f \( -path '*/cur/*' -o -path '*/new/*' \) -print | wc -l")
        if [ "$seen" -lt "$files" ]; then CHECK_WHY="$u: mail sees $seen messages out of $files"; return 1; fi
    done
}
"#;

const BODY_MAIL_FULL: &str = r#"
cat > "$STAGE/inner.sh" <<'INNER'
tar --numeric-owner -xpf /stage/payload/mail-full.tar -C / || exit 1
(cd /tmp/docker-mailserver/opendkim/keys 2>/dev/null && find . -name '*.private' | sort | xargs -r cat | sha256sum) > /stage/dkim-expected
chown "$OWNER" /stage/dkim-expected
INNER
MAIL_ENV=(-e "OWNER=$(id -u)")
part_stop() { :; }
part_save() { :; }
part_put() { mail_run /stage/inner.sh; }
part_unput() { return 1; }
part_start() { ( cd "$BASE" && docker-compose up -d ); }
part_check() { mail_check; }
"#;

const BODY_WEBSITE: &str = r#"
W="$BASE/MinipassWebSite"
DBS="MinipassWebSite/customers.db MinipassWebSite/customers.db-wal MinipassWebSite/customers.db-shm email_monitoring/monitoring.db email_monitoring/monitoring.db-wal email_monitoring/monitoring.db-shm"
export DBS; EXPORTS="$EXPORTS DBS"
part_stop() { :; }
part_save() {
    as_root '
        set -e; cd "$BASE"; L=""
        for f in $DBS MinipassWebSite/.env; do [ -e "$f" ] && L="$L $f"; done
        if [ -n "$L" ]; then tar -cf "$STAGE/undo.tar" $L; else head -c 1024 /dev/zero > "$STAGE/undo.tar"; fi
        chown "$ME" "$STAGE/undo.tar"'
}
part_put() {
    if [ ! -f "$W/app.py" ]; then
        echo "The website's code is gone: getting it"
        git -C "$BASE" checkout HEAD -- MinipassWebSite || return 1
    fi
    if [ ! -x "$W/venv/bin/python" ]; then
        echo "Making the website's Python environment"
        python3 -m venv "$W/venv" && "$W/venv/bin/pip" install -q -r "$W/requirements.txt" || return 1
    fi
    sqlite3 "$STAGE/payload/customers.db" 'SELECT count(*) FROM customers;' > "$STAGE/customers-expected" 2>/dev/null
    as_root '
        set -e; cd "$BASE"; P="$STAGE/payload"
        mkdir -p MinipassWebSite email_monitoring
        for f in $DBS; do rm -f "$f"; done
        cp "$P/customers.db" MinipassWebSite/customers.db.new
        mv -f MinipassWebSite/customers.db.new MinipassWebSite/customers.db
        cp "$P/monitoring.db" email_monitoring/monitoring.db.new
        mv -f email_monitoring/monitoring.db.new email_monitoring/monitoring.db
        cp "$P/env" MinipassWebSite/.env
        chown "$ME" MinipassWebSite/customers.db email_monitoring/monitoring.db MinipassWebSite/.env'
}
part_start() {
    sudo -n systemctl restart minipass-web || { CHECK_WHY="sudo would not restart minipass-web"; return 1; }
}
part_check() {
    [ "$(sqlite3 "$W/customers.db" 'PRAGMA integrity_check;' 2>&1)" = ok ] || { CHECK_WHY="the customer list is not healthy"; return 1; }
    if [ -f "$STAGE/customers-expected" ] && [ "$MODE" = apply ]; then
        [ "$(sqlite3 "$W/customers.db" 'SELECT count(*) FROM customers;')" -ge "$(cat "$STAGE/customers-expected")" ] || { CHECK_WHY="customers are missing from the list"; return 1; }
    fi
    wait_page "$URL" "$EXPECT" || { CHECK_WHY="the website's page does not answer"; return 1; }
}
part_unput() {
    as_root 'set -e; cd "$BASE"; for f in $DBS MinipassWebSite/.env; do rm -f "$f"; done; tar --numeric-owner -xpf "$STAGE/undo.tar"'
}
"#;

const BODY_SERVER: &str = r#"
PROJECT_FILES=".env mailserver.env docker-compose.yml nginx vhost.d certs acme"
export PROJECT_FILES; EXPORTS="$EXPORTS PROJECT_FILES"
cat > "$STAGE/items.sh" <<'ITEMS'
fp() {
    if [ -d "$1" ]; then
        ( cd "$1" && { find . -type f -exec md5sum {} + ; find . -type l | while read -r l; do echo "$l -> $(readlink "$l")"; done; } | sort | md5sum )
    elif [ -f "$1" ]; then
        md5sum < "$1"
    else
        echo none
    fi
}
swap() {
    [ -e "$2" ] || return 0
    if [ "$(fp "$2")" != "$(fp "$BASE/$1")" ]; then
        rm -rf "$BASE/$1"
        cp -a "$2" "$BASE/$1"
        echo "$1" >> "$STAGE/changed"
    fi
}
ITEMS
part_stop() { :; }
part_save() {
    mkdir -p "$BASE" || return 1
    if crontab -l > "$STAGE/undo-crontab" 2>/dev/null; then :; else rm -f "$STAGE/undo-crontab"; fi
    [ ! -f "$SERVICE" ] || cp "$SERVICE" "$STAGE/undo-service"
    as_root '
        set -e; cd "$BASE"; L=""
        for f in $PROJECT_FILES; do [ -e "$f" ] && L="$L $f"; done
        if [ -n "$L" ]; then tar -cf "$STAGE/undo.tar" $L; else head -c 1024 /dev/zero > "$STAGE/undo.tar"; fi
        [ ! -f "$HOME_DIR/.mailpass" ] || cp -p "$HOME_DIR/.mailpass" "$STAGE/undo-mailpass"
        chown "$ME" "$STAGE/undo.tar"; [ ! -f "$STAGE/undo-mailpass" ] || chown "$ME" "$STAGE/undo-mailpass"'
}
put_service() {
    cmp -s "$1" "$SERVICE" && return 0
    cp "$1" "$STAGE/minipass-web.service"
    sudo -n install -m 644 -o root -g root "$STAGE/minipass-web.service" "$SERVICE" || return 1
    sudo -n systemctl daemon-reload || return 1
    sudo -n systemctl enable minipass-web || return 1
    touch "$STAGE/service-changed"
}
part_put() {
    : > "$STAGE/changed"
    as_root '
        set -e; . "$STAGE/items.sh"; P="$STAGE/payload"
        mkdir -p "$STAGE/new"; tar --numeric-owner -xpf "$P/certs.tar" -C "$STAGE/new"
        swap .env "$P/env"
        swap mailserver.env "$P/mailserver.env"
        swap docker-compose.yml "$P/docker-compose.yml"
        swap nginx "$P/nginx"
        swap vhost.d "$P/vhost.d"
        swap certs "$STAGE/new/nginx/certs"
        swap acme "$STAGE/new/acme.sh"
        rm -rf "$STAGE/new"
        cp "$P/mailpass" "$HOME_DIR/.mailpass"
        chown "$ME" "$HOME_DIR/.mailpass" "$STAGE/changed"; chmod 600 "$HOME_DIR/.mailpass"' || return 1
    crontab "$STAGE/payload/crontab.txt" || return 1
    put_service "$STAGE/payload/minipass-web.service" || return 1
    echo "Changed: $(tr '\n' ' ' < "$STAGE/changed")"
}
# Restart only what uses something that changed.
part_start() {
    [ "$BLANK" = 0 ] || return 0
    cd "$BASE" || return 1
    docker-compose up -d || return 1
    local affected="" svc id src item
    for svc in $(docker-compose config --services); do
        for id in $(docker-compose ps -aq "$svc"); do
            for src in $(docker inspect --format '{{range .Mounts}}{{.Source}} {{end}}' "$id"); do
                while IFS= read -r item; do
                    case "$src/" in "$BASE/$item/"*) case " $affected " in *" $svc "*) ;; *) affected="$affected $svc";; esac;; esac
                done < "$STAGE/changed"
            done
        done
    done
    if [ -n "$affected" ]; then
        echo "Restarting:$affected"
        docker-compose up -d --no-deps --force-recreate $affected || return 1
    fi
    if [ -f "$STAGE/service-changed" ]; then sudo -n systemctl restart minipass-web || return 1; fi
}
part_check() {
    [ "$BLANK" = 0 ] || return 0
    cd "$BASE" || return 1
    local want have i
    want=$(docker-compose config --services | sort)
    for i in $(seq 40); do
        have=$(docker-compose ps --status running --services | sort)
        [ "$want" = "$have" ] && break
        sleep 3
    done
    [ "$want" = "$have" ] || { CHECK_WHY="not every container is running"; return 1; }
    wait_page "$WEBSITE_URL" "$WEBSITE_EXPECT" || { CHECK_WHY="the website's page does not answer"; return 1; }
    local d c
    for d in "$BASE"/deployed/*/; do
        [ -f "$d/docker-compose.yml" ] || continue
        c=$(basename "$d")
        wait_page "${MONITOR_URL//\{customer\}/$c}" "$EXPECT" || { CHECK_WHY="$c's login page does not show the form"; return 1; }
    done
}
part_unput() {
    : > "$STAGE/changed"
    as_root '
        set -e; . "$STAGE/items.sh"
        mkdir -p "$STAGE/old"; tar --numeric-owner -xpf "$STAGE/undo.tar" -C "$STAGE/old"
        for f in $PROJECT_FILES; do
            if [ -e "$STAGE/old/$f" ]; then swap "$f" "$STAGE/old/$f"
            elif [ -e "$BASE/$f" ]; then rm -rf "$BASE/$f"; echo "$f" >> "$STAGE/changed"; fi
        done
        rm -rf "$STAGE/old"
        if [ -f "$STAGE/undo-mailpass" ]; then cp "$STAGE/undo-mailpass" "$HOME_DIR/.mailpass"; chown "$ME" "$HOME_DIR/.mailpass"; chmod 600 "$HOME_DIR/.mailpass"; else rm -f "$HOME_DIR/.mailpass"; fi
        chown "$ME" "$STAGE/changed"' || return 1
    if [ -f "$STAGE/undo-crontab" ]; then crontab "$STAGE/undo-crontab" || return 1; else crontab -r 2>/dev/null; fi
    if [ -f "$STAGE/undo-service" ]; then put_service "$STAGE/undo-service" || return 1; fi
}
"#;

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(server: &str, enabled: bool, real_ok: bool, allowed: &[&str]) -> Config {
        let text = format!(
            "server = \"{server}\"\nport = 2222\nremote_base = \"/x\"\nbackup_root = \"/x\"\nrestore_enabled = {enabled}\nreal_vps_ok = {real_ok}\nrestore_allowed = [{}]\n",
            allowed.iter().map(|a| format!("\"{a}\"")).collect::<Vec<_>>().join(",")
        );
        toml::from_str(&text).unwrap()
    }

    #[test]
    fn restore_is_off_by_default() {
        let c: Config = toml::from_str("server = \"a@b\"\nport = 22\nremote_base = \"/x\"\nbackup_root = \"/x\"\n").unwrap();
        assert!(!c.restore_enabled);
        assert!(!c.real_vps_ok);
        assert!(locks(&c, &What::Website, "b").is_err());
    }

    #[test]
    fn real_server_needs_its_own_lock() {
        let c = cfg("kdresdell@minipass.me", true, false, &["kdc"]);
        for what in [What::Website, What::Server, What::Everything, What::Undo, What::MailMessages, What::Customer("kdc".into())] {
            assert!(locks(&c, &what, "minipass.me").is_err(), "{what:?}");
        }
        assert!(locks(&cfg("kdresdell@minipass.me", true, true, &[]), &What::Website, "minipass.me").is_ok());
    }

    #[test]
    fn real_server_is_seen_through_aliases() {
        assert!(is_real_vps("kdresdell@minipass.me", ""));
        assert!(is_real_vps("MINIPASS.ME", ""));
        assert!(is_real_vps("kdresdell@mail.minipass.me.", ""));
        assert!(is_real_vps("kdresdell@vps", "minipass.me"));
        assert!(is_real_vps("kdresdell@vps", "138.199.152.128"));
        assert!(is_real_vps("kdresdell@138.199.152.128", ""));
        assert!(!is_real_vps("kdresdell@127.0.0.1", "127.0.0.1"));
        assert!(!is_real_vps("kdresdell@notminipass.me", "notminipass.me"));
        let c = cfg("kdresdell@vps", true, false, &[]);
        assert!(locks(&c, &What::Website, "minipass.me").is_err());
    }

    #[test]
    fn customers_must_be_on_the_list() {
        let c = cfg("kdresdell@127.0.0.1", true, false, &["kdc"]);
        assert!(locks(&c, &What::Customer("kdc".into()), "127.0.0.1").is_ok());
        assert!(locks(&c, &What::Customer("testb".into()), "127.0.0.1").is_err());
        assert!(locks(&c, &What::Customer("../kdc".into()), "127.0.0.1").is_err());
    }

    #[test]
    fn confirm_words() {
        assert_eq!(What::Customer("kdc".into()).confirm_word(), "kdc");
        assert_eq!(What::Everything.confirm_word(), "RESTORE");
        assert_eq!(What::Undo.confirm_word(), "RESTORE");
    }

    #[test]
    fn forbidden_commands_are_not_in_the_source() {
        // Built from pieces, so this test doesn't trip over itself.
        let forbidden = [
            ["git reset", "--hard"].join(" "),
            ["docker image", "prune"].join(" "),
            ["docker system", "prune"].join(" "),
            ["docker-compose", "pull"].join(" "),
            ["docker compose", "pull"].join(" "),
        ];
        for entry in std::fs::read_dir(concat!(env!("CARGO_MANIFEST_DIR"), "/src")).unwrap() {
            let path = entry.unwrap().path();
            let mut text = std::fs::read_to_string(&path).unwrap();
            // The guide warns the user against these. It's text on the screen, not a command.
            if path.ends_with("menu.rs") {
                let start = text.find("const GUIDE").unwrap();
                let end = start + text[start..].find("];").unwrap();
                text.replace_range(start..end, "");
            }
            for f in &forbidden {
                assert!(!text.contains(f.as_str()), "{} holds: {f}", path.display());
            }
        }
    }

    #[test]
    fn mail_name_ignores_read_state_and_folder() {
        let unread = "var/mail/d/u/new/1.M1P1.mail,S=10,W=11";
        let read = "var/mail/d/u/cur/1.M1P1.mail,S=10,W=11:2,S";
        let replied = "var/mail/d/u/cur/1.M1P1.mail,S=10,W=11:2,RS";
        let moved = "var/mail/d/u/.Archive/cur/1.M1P1.mail,S=10,W=11:2,S";
        let key = mail_key(unread).unwrap();
        assert_eq!(key, "d/u/1.M1P1.mail,S=10,W=11");
        for other in [read, replied, moved] {
            assert_eq!(mail_key(other).unwrap(), key);
        }
        assert_ne!(mail_key("var/mail/d/other/new/1.M1P1.mail,S=10,W=11").unwrap(), key);
        assert_eq!(mail_key("var/mail/d/u/dovecot-uidlist"), None);
        assert_eq!(mail_key("var/mail/d/u/cur/"), None);
        assert_eq!(mail_key("var/mail/d/u/tmp/1.M1"), None);
        assert_eq!(mail_key("var/mail/d/u/home/cur/x"), None);
    }

    fn strings(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn only_missing_mail_is_added() {
        let backup = strings(&[
            "var/mail/d/u/",
            "var/mail/d/u/cur/",
            "var/mail/d/u/new/",
            "var/mail/d/u/new/1.A,S=1",
            "var/mail/d/u/new/2.B,S=1",
            "var/mail/d/u/cur/3.C,S=1:2,S",
            "var/mail/d/u/tmp/junk",
            "var/mail/d/u/dovecot-uidlist",
            "var/mail/d/u/subscriptions",
        ]);
        // 1 was read since the backup, 2 is lost, 3 was moved, 4 is new since the backup.
        let server = strings(&[
            "var/mail",
            "var/mail/d",
            "var/mail/d/u",
            "var/mail/d/u/cur",
            "var/mail/d/u/new",
            "var/mail/d/u/cur/1.A,S=1:2,S",
            "var/mail/d/u/.Archive/cur/3.C,S=1:2,S",
            "var/mail/d/u/new/4.D,S=1",
        ]);
        assert_eq!(mail_to_add(&backup, &server), strings(&["var/mail/d/u/new/2.B,S=1", "var/mail/d/u/subscriptions"]));
    }

    #[test]
    fn a_mailbox_that_is_gone_comes_back_whole() {
        let backup = strings(&["var/mail/d/u/", "var/mail/d/u/cur/", "var/mail/d/u/cur/1.A:2,S", "var/mail/d/u/dovecot-uidlist", "var/mail/d/u/tmp/"]);
        assert_eq!(mail_to_add(&backup, &strings(&["var/mail"])), backup);
    }

    #[test]
    fn mail_waiting_to_be_sent_stays_out() {
        assert!(full_mail_keeps("var/mail/d/u/cur/1"));
        assert!(full_mail_keeps("var/mail-state/lib-dovecot/x"));
        assert!(full_mail_keeps("tmp/docker-mailserver/postfix-accounts.cf"));
        assert!(!full_mail_keeps("tmp/docker-mailserver/check-for-changes.sh.lock"));
        assert!(!is_setting("tmp/docker-mailserver/check-for-changes.sh.lock"));
        assert!(!is_setting("tmp/docker-mailserver/"));
        assert!(!full_mail_keeps("var/mail-state/spool-postfix/deferred/A/123"));
        assert!(!full_mail_keeps("var/mail-state/spool-postfix/"));
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("mptool-test-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn sh(dir: &Path, script: &str) -> String {
        let out = Command::new("sh").arg("-c").arg(script).current_dir(dir).output().unwrap();
        assert!(out.status.success(), "{script}: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    #[test]
    fn tar_filter_keeps_only_what_was_asked_and_stays_readable() {
        let dir = scratch("filter");
        let long = "d".repeat(150);
        sh(&dir, &format!("mkdir -p src/{long} && echo one > src/a && echo two > src/{long}/b && echo three > src/c && ln src/a src/link && tar cf all.tar -C src ."));
        let all = dir.join("all.tar");
        let names: Vec<String> = tar_entries(&all).unwrap().into_iter().map(|e| e.name).collect();
        let long_file = format!("./{long}/b");
        assert!(names.contains(&long_file));

        // The hard link is kept but the file it points at isn't: it must become a real file.
        let link = names.iter().find(|n| n.ends_with("link") || n.ends_with("/a")).unwrap().clone();
        let other = if link.ends_with("link") { "./a" } else { "./link" };
        let small = dir.join("small.tar");
        let keep = [long_file.clone(), other.to_string()];
        tar_filter(&all, &small, &|n| keep.iter().any(|k| k == n)).unwrap();
        sh(&dir, "mkdir out && tar xf small.tar -C out");
        assert_eq!(std::fs::read_to_string(dir.join("out").join(&long).join("b")).unwrap(), "two\n");
        assert_eq!(std::fs::read_to_string(dir.join("out").join(other.trim_start_matches("./"))).unwrap(), "one\n");
        assert!(!dir.join("out/c").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_cut_or_damaged_archive_is_noticed() {
        let dir = scratch("cut");
        sh(&dir, "head -c 5000 /dev/zero | tr '\\0' x > a; echo hi > b; tar cf all.tar a b");
        let all = dir.join("all.tar");
        assert!(tar_entries(&all).is_ok());
        // Cut in the middle of a file.
        sh(&dir, "head -c 2000 all.tar > mid.tar");
        assert!(tar_entries(&dir.join("mid.tar")).is_err());
        // Cut exactly between two files, which `tar tf` accepts.
        sh(&dir, "head -c 6144 all.tar > edge.tar");
        assert!(tar_entries(&dir.join("edge.tar")).is_err());
        // Damaged header.
        sh(&dir, "head -c 100000 /dev/urandom > junk.tar");
        assert!(tar_entries(&dir.join("junk.tar")).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_damaged_database_stops_the_restore() {
        let dir = scratch("db");
        need(&dir).unwrap();
        std::fs::write(dir.join("bad.db"), "not a database").unwrap();
        assert!(db_healthy(&dir.join("bad.db")).is_err());
        assert!(db_healthy(&dir.join("missing.db")).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn wrong_confirm_words_are_noticed_when_parsing_options() {
        let args = |list: &[&str]| list.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert!(parse(&args(&["mail", "settings", "--only", "passwords"])).is_err());
        let (job, confirm, _) = parse(&args(&["mail", "settings", "--only", "forwards,dkim", "--confirm", "RESTORE"])).unwrap();
        assert_eq!(job.what, What::MailSettings(vec!["forwards".into(), "dkim".into()]));
        assert_eq!(confirm.as_deref(), Some("RESTORE"));
        assert!(parse(&args(&["nothing"])).is_err());
    }
}
