//! Compare: what differs between one night's backup and the live server.
//! Only looks. Nothing is written on the server or in the backup.

use crate::config::Config;
use crate::remote::Remote;
use crate::run::{tar_list, CUSTOMER_LIST, SERVER_FILES};
use anyhow::{bail, Result};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::process::Command;

pub struct Line {
    /// "Customers" or "Minipass setup".
    pub group: &'static str,
    /// Customer name, empty for the setup.
    pub who: String,
    pub what: String,
    pub result: String,
    pub differs: bool,
}

const MAIL_CONFIG_SUMS: &str =
    "docker exec mailserver sh -c 'cd / && find tmp/docker-mailserver -type f -exec md5sum {} +'";
const MAIL_FILES: &str = "docker exec mailserver sh -c 'cd / && find var/mail -type f'";

pub fn compare(cfg: &Config, night: &Path) -> Result<Vec<Line>> {
    let remote = Remote::new(cfg);
    remote.run("true")?;
    let mut out = vec![];

    let mut add = |group, who: &str, what: &str, r: Result<(String, bool)>| {
        let (result, differs) = r.unwrap_or_else(|e| (format!("could not check: {e:#}"), true));
        out.push(Line { group, who: who.into(), what: what.into(), result, differs });
    };

    // ── Customers ──
    let backed_up = dir_names(&night.join("customers"));
    for c in &backed_up {
        let dir = night.join("customers").join(c);
        add("Customers", c, "database", db_rows(&remote, &dir.join("minipass.db"), &format!("deployed/{c}/app/instance/minipass.db")));
        add("Customers", c, "photos", files(&remote, &[(format!("deployed/{c}/app/static/uploads/"), dir.join("uploads/"))]));
        add(
            "Customers",
            c,
            "settings",
            files(
                &remote,
                &[
                    (format!("deployed/{c}/docker-compose.yml"), dir.join("docker-compose.yml")),
                    (format!("deployed/{c}/app/.env"), dir.join(".env")),
                ],
            ),
        );
    }
    let live = remote.run(&format!("cd {} && {}", remote.base, CUSTOMER_LIST))?;
    for c in live.lines().map(str::trim).filter(|c| !c.is_empty() && !backed_up.iter().any(|b| b == c)) {
        add("Customers", c, "everything", Ok(("new on the server, not in this backup".into(), true)));
    }

    // ── Minipass setup ──
    let (keys, settings): (Vec<_>, Vec<_>) = SERVER_FILES
        .iter()
        .map(|(src, dest)| (src.to_string(), night.join(dest)))
        .partition(|(_, dest)| dest.to_string_lossy().ends_with(".env") || dest.ends_with(".mailpass"));
    add("Minipass setup", "", "API keys + passwords", files(&remote, &keys));
    add("Minipass setup", "", "Server settings", files(&remote, &settings));

    let mail_tar = night.join("mail/mail.tar");
    match mail_config(&remote, &mail_tar, &cfg.root()) {
        Ok(groups) => {
            for (what, r) in groups {
                add("Minipass setup", "", what, Ok(r));
            }
        }
        Err(e) => add("Minipass setup", "", "Mail accounts, forwards, key", Err(e)),
    }
    add(
        "Minipass setup",
        "",
        "Website customer list",
        db_rows(&remote, &night.join("website/customers.db"), "MinipassWebSite/customers.db"),
    );
    add("Minipass setup", "", "Mail messages", mail_messages(&remote, &mail_tar));
    Ok(out)
}

/// Files and folders that rsync would change, summed up in words.
fn files(remote: &Remote, items: &[(String, std::path::PathBuf)]) -> Result<(String, bool)> {
    let mut lines = vec![];
    for (src, local) in items {
        lines.extend(remote.dry_run(src, local)?);
    }
    let new = lines.iter().filter(|l| l.starts_with(">f+++")).count();
    let gone = lines.iter().filter(|l| l.starts_with("*deleting")).count();
    let changed = lines.len() - new - gone;
    Ok(words(new, changed, gone, || "same".into()))
}

fn words(new: usize, changed: usize, gone: usize, same: impl FnOnce() -> String) -> (String, bool) {
    let mut parts = vec![];
    if new > 0 {
        parts.push(format!("{new} new on server"));
    }
    if changed > 0 {
        parts.push(format!("{changed} changed"));
    }
    if gone > 0 {
        parts.push(format!("{gone} gone from server"));
    }
    if parts.is_empty() {
        (same(), false)
    } else {
        (parts.join(", "), true)
    }
}

/// Row counts per table, backup vs server. The server side runs sqlite3 read-only.
fn db_rows(remote: &Remote, local: &Path, server_path: &str) -> Result<(String, bool)> {
    let tables = sqlite(local, "SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%' ORDER BY name;")?;
    let tables: Vec<&str> = tables.lines().filter(|t| !t.is_empty()).collect();
    if tables.is_empty() {
        bail!("no tables in the backup copy");
    }
    let query = tables
        .iter()
        .map(|t| format!("SELECT '{t}', count(*) FROM \"{t}\""))
        .collect::<Vec<_>>()
        .join(" UNION ALL ")
        + ";";
    let mine = counts(&sqlite(local, &query)?);
    let theirs = counts(&remote.run_input(
        &format!("cd {} && sqlite3 -readonly -cmd '.timeout 10000' {server_path}", remote.base),
        &query,
    )?);
    let diffs: Vec<String> = mine
        .iter()
        .filter_map(|(t, n)| {
            let s = theirs.get(t).copied().unwrap_or(0);
            (s != *n).then(|| format!("{t} {:+}", s - n))
        })
        .collect();
    if diffs.is_empty() {
        return Ok(("same".into(), false));
    }
    let mut text = format!("server has {}", diffs.iter().take(3).cloned().collect::<Vec<_>>().join(", "));
    if diffs.len() > 3 {
        text += &format!(" and {} more", diffs.len() - 3);
    }
    Ok((text, true))
}

fn sqlite(db: &Path, query: &str) -> Result<String> {
    let out = Command::new("sqlite3").arg("-readonly").arg(db).arg(query).output()?;
    if !out.status.success() {
        bail!("{}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn counts(text: &str) -> BTreeMap<String, i64> {
    text.lines()
        .filter_map(|l| l.split_once('|'))
        .filter_map(|(t, n)| Some((t.to_string(), n.trim().parse().ok()?)))
        .collect()
}

/// Mail settings (accounts, forwards, signing key): file checksums in the backup vs in the mail server.
fn mail_config(remote: &Remote, mail_tar: &Path, root: &Path) -> Result<Vec<(&'static str, (String, bool))>> {
    let theirs = sums(&remote.run(MAIL_CONFIG_SUMS)?);

    // Unpack only the settings folder, inside the backup root (it holds the private key).
    let tmp = root.join(format!(".compare-{}", std::process::id()));
    std::fs::create_dir_all(&tmp)?;
    let mine = (|| -> Result<(BTreeMap<String, String>, usize)> {
        let ok = Command::new("tar").arg("xf").arg(mail_tar).arg("-C").arg(&tmp).arg("tmp/docker-mailserver").status()?;
        if !ok.success() {
            bail!("cannot unpack the mail settings from the backup");
        }
        let out = Command::new("sh")
            .arg("-c")
            .arg("find tmp/docker-mailserver -type f -exec md5sum {} +")
            .current_dir(&tmp)
            .output()?;
        let accounts = std::fs::read_to_string(tmp.join("tmp/docker-mailserver/postfix-accounts.cf")).unwrap_or_default();
        Ok((sums(&String::from_utf8_lossy(&out.stdout)), count_lines(&accounts)))
    })();
    let _ = std::fs::remove_dir_all(&tmp);
    let (mine, accounts) = mine?;

    let group = |path: &str| -> &'static str {
        if path.contains("/opendkim/") {
            "Mail signing key (DKIM)"
        } else if path.contains("/user-patches/") || path.ends_with("postfix-virtual.cf") {
            "Forwards to Gmail"
        } else {
            "Mail accounts"
        }
    };
    let groups = ["Mail accounts", "Forwards to Gmail", "Mail signing key (DKIM)"];
    let all: BTreeSet<&String> = mine.keys().chain(theirs.keys()).filter(|p| !p.ends_with(".db")).collect();
    let mut result = vec![];
    for g in groups {
        let (mut new, mut changed, mut gone) = (0, 0, 0);
        for p in all.iter().filter(|p| group(p) == g) {
            match (mine.get(*p), theirs.get(*p)) {
                (None, Some(_)) => new += 1,
                (Some(_), None) => gone += 1,
                (Some(a), Some(b)) if a != b => changed += 1,
                _ => {}
            }
        }
        let same = || match g {
            "Mail accounts" => format!("same ({accounts} accounts)"),
            "Forwards to Gmail" => {
                let n = mine.keys().filter(|p| p.ends_with("/sieve/forward.sieve")).count();
                format!("same ({n} forwards)")
            }
            _ => "same".into(),
        };
        result.push((g, words(new, changed, gone, same)));
    }
    Ok(result)
}

fn count_lines(text: &str) -> usize {
    text.lines().filter(|l| !l.trim().is_empty()).count()
}

fn sums(text: &str) -> BTreeMap<String, String> {
    text.lines()
        .filter_map(|l| l.split_once("  "))
        .map(|(sum, path)| (path.trim().to_string(), sum.to_string()))
        .collect()
}

/// Mail messages: which mail files exist in the backup vs on the server.
/// A message moves from new/ to cur/ and gets flags (":2,S") once read, so those are ignored.
fn mail_messages(remote: &Remote, mail_tar: &Path) -> Result<(String, bool)> {
    let key = |p: &str| -> Option<String> {
        if !(p.contains("/cur/") || p.contains("/new/")) || p.ends_with('/') {
            return None;
        }
        let p = p.replace("/cur/", "/").replace("/new/", "/");
        Some(p.split(':').next().unwrap_or(&p).to_string())
    };
    let mine: BTreeSet<String> = tar_list(mail_tar)?.iter().filter_map(|p| key(p)).collect();
    let theirs: BTreeSet<String> = remote.run(MAIL_FILES)?.lines().filter_map(key).collect();
    let new = theirs.difference(&mine).count();
    let gone = mine.difference(&theirs).count();
    Ok(words(new, 0, gone, || format!("same ({} messages)", mine.len())))
}

fn dir_names(dir: &Path) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.path().is_dir())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    v.sort();
    v
}
