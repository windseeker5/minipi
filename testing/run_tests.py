#!/usr/bin/env python3
"""The restore tests, on the fake VPS only. `./run_tests.py` runs them all, `./run_tests.py 1 8` just those.
Each one: clean server, real backup, break something, restore, check, then check nothing else changed."""
import http.server, json, os, re, shlex, shutil, signal, subprocess, sys, threading, time, tomllib
from pathlib import Path

HERE = Path(__file__).resolve().parent
STATE = HERE / ".state"
TOOL = HERE.parent / "mptool/target/release/mptool"
BASE = "/home/kdresdell/minipass_env"
RUN = STATE / "runs" / time.strftime("%Y%m%d-%H%M%S")
DISCORD_PORT = 18925
discord = []
IMAP = "docker exec -i mailserver sh -c 'timeout 15 openssl s_client -quiet -connect 127.0.0.1:993 2>/dev/null'"


class Failed(Exception):
    pass


def check(ok, what):
    if not ok:
        raise Failed(what)


class Discord(http.server.BaseHTTPRequestHandler):
    def do_POST(self):
        discord.append(json.loads(self.rfile.read(int(self.headers["Content-Length"])))["content"])
        self.send_response(204)
        self.end_headers()

    def log_message(self, *a):
        pass


def sh(cmd, ok=True, timeout=900, **kw):
    r = subprocess.run(cmd, shell=isinstance(cmd, str), capture_output=True, text=True, timeout=timeout, **kw)
    if ok and r.returncode != 0:
        raise Failed(f"command failed: {cmd}\n{r.stdout[-1500:]}\n{r.stderr[-1500:]}")
    return r


def root(cmd, ok=True, timeout=600):
    """A command inside the fake VPS, as root. Only ever `docker exec fakevps`."""
    return sh(["docker", "exec", "-i", "fakevps", "bash", "-c", cmd], ok=ok, timeout=timeout)


def me(cmd, ok=True, timeout=600):
    """A command inside the fake VPS, as kdresdell, in the server folder."""
    return sh(["docker", "exec", "-i", "-u", "kdresdell", "-w", BASE, "fakevps", "bash", "-c", cmd], ok=ok, timeout=timeout)


def snap():
    return json.loads(root("/usr/local/sbin/fakevps-snapshot").stdout)


def diff(a, b):
    return sorted(k for k in set(a) | set(b) if a.get(k) != b.get(k))


def only_under(a, b, allowed):
    """Everything outside `allowed` must be the same in both."""
    wrong = [k for k in diff(a, b) if not any(k.startswith(p) for p in allowed)]
    check(not wrong, f"changed but shouldn't have: {wrong[:12]}")


def same_under(a, b, prefix):
    wrong = [k for k in diff(a, b) if k.startswith(prefix)]
    check(not wrong, f"{prefix} isn't as it was: {wrong[:12]}")


def page(host, expect='type="password"'):
    r = root(f"curl -sk --max-time 10 --resolve {host}:443:127.0.0.1 https://{host}/{'login' if expect else ''}", ok=False)
    return expect in r.stdout if expect else False


def login_page(c, tries=40):
    for _ in range(tries):
        if page(f"{c}.minipass.me"):
            return True
        time.sleep(2)
    return False


def website_up(tries=40):
    for _ in range(tries):
        r = root("curl -sk --max-time 10 --resolve minipass.me:443:127.0.0.1 -o /dev/null -w '%{http_code}' https://minipass.me/", ok=False)
        if r.stdout.strip() == "200":
            return True
        time.sleep(2)
    return False


def imap(user, password, tries=20):
    for i in range(tries):
        r = root(f"printf 'a LOGIN \"{user}\" \"{password}\"\\r\\nb LOGOUT\\r\\n' | {IMAP}", ok=False)
        if re.search(r"^a OK", r.stdout, re.M):
            return True
        if re.search(r"^a NO", r.stdout, re.M) and tries == 1:
            return False
        time.sleep(3)
    return False


def mail_up():
    for _ in range(120):
        if root("docker exec mailserver bash -c 'exec 3<>/dev/tcp/127.0.0.1/25 && exec 4<>/dev/tcp/127.0.0.1/143' && docker exec mailserver doveadm user alice@minipass.me", ok=False).returncode == 0:
            return True
        time.sleep(2)
    return False


def send(sender, to, subject):
    message = f"From: {sender}\nTo: {to}\nSubject: {subject}\nMessage-ID: <{os.urandom(8).hex()}@minipass.me>\n\nFake message: {subject}\n"
    sh(["docker", "exec", "-i", "fakevps", "docker", "exec", "-i", "mailserver", "sendmail", "-f", sender, to], input=message)


def box_has(user, subject, tries=30):
    """How many messages in this user's mailbox have this subject, as Dovecot itself sees them."""
    n = 0
    for _ in range(tries):
        r = root(f"docker exec mailserver doveadm search -u {user}@minipass.me mailbox '*' SUBJECT {shlex.quote(subject)}", ok=False)
        n = len(r.stdout.strip().splitlines())
        if n:
            return n
        time.sleep(1)
    return n


def public_key():
    text = root(f"grep -o 'p=[A-Za-z0-9+/=]*' {BASE}/config/opendkim/keys/minipass.me/mail.txt").stdout.strip()
    return text[2:]


def dkim_good(key):
    """Send a mail and check its signature really was made with this key."""
    subject = "dkim-check-" + os.urandom(4).hex()
    send("alice@minipass.me", "bob@minipass.me", subject)
    check(box_has("bob", subject) == 1, "the DKIM test mail didn't arrive")
    r = root(f"f=$(docker exec mailserver sh -c \"grep -rl {subject} /var/mail/minipass.me/bob/new /var/mail/minipass.me/bob/cur\" | head -1); "
             f"docker exec mailserver cat \"$f\" | /var/cache/fakevps/dkim-venv/bin/python /usr/local/sbin/fakevps-check-dkim {key}", ok=False)
    check(r.returncode == 0, f"DKIM: {r.stdout} {r.stderr}")


def undo_folders():
    return root("ls -1 /home/kdresdell/mptool-undo 2>/dev/null", ok=False).stdout.split()


class Test:
    def __init__(self, name, **config):
        self.name = name
        self.dir = RUN / name
        self.dir.mkdir(parents=True)
        self.backups = self.dir / "backups"
        settings = tomllib.loads((STATE / "config.toml").read_text())
        check(settings["server"] == "kdresdell@127.0.0.1" and settings["port"] == 2223 and "real_vps_ok" not in settings,
              "this config isn't the fake VPS")
        settings.update(backup_root=str(self.backups), discord_webhook=f"http://127.0.0.1:{DISCORD_PORT}/{name}")
        settings.update(config)
        self.config = self.write_config("config.toml", settings)
        self.settings = settings
        discord.clear()

    def write_config(self, name, settings):
        lines = [f"{k} = {json.dumps(v)}" for k, v in settings.items()]
        path = self.dir / name
        path.write_text("\n".join(lines) + "\n")
        return path

    def env(self, config=None):
        return {**os.environ, "MPTOOL_CONFIG": str(config or self.config), "HOME": str(STATE / "home")}

    def tool(self, *args, ok=True, config=None, timeout=900):
        r = sh([str(TOOL), *args], ok=False, timeout=timeout, env=self.env(config), stdin=subprocess.DEVNULL)
        (self.dir / "tool.log").open("a").write(f"\n$ mptool {' '.join(args)}\n{r.stdout}\n{r.stderr}\nexit {r.returncode}\n")
        r.text = r.stdout + r.stderr
        if ok is True:
            check(r.returncode == 0, f"mptool {' '.join(args)} failed:\n{r.text[-1500:]}")
        if ok is False:
            check(r.returncode != 0, f"mptool {' '.join(args)} should have failed:\n{r.text[-800:]}")
        return r

    def background(self, *args):
        log = open(self.dir / "tool.log", "a")
        log.write(f"\n$ mptool {' '.join(args)} (in the background)\n")
        log.flush()
        return subprocess.Popen([str(TOOL), *args], env=self.env(), stdout=subprocess.PIPE, stderr=subprocess.STDOUT, stdin=subprocess.DEVNULL, text=True)

    def start(self):
        """Clean server, a real backup, and moved back a day so the next backup doesn't replace it."""
        sh([str(HERE / "reset.sh")])
        self.backup_and_age()

    def backup_and_age(self):
        self.tool("run")
        today = time.strftime("%Y-%m-%d")
        self.night = "2026-01-15"
        os.rename(self.backups / today, self.backups / self.night)
        (self.backups / "latest").unlink(missing_ok=True)
        self.before = snap()
        self.key = public_key()

    @property
    def night_dir(self):
        return self.backups / self.night

    def compare(self, night=None):
        r = self.tool("compare", *(["--night", night] if night else []))
        return [l for l in r.stdout.splitlines() if l.startswith("DIFF")]

    def finish(self, others=(), compare_may_differ=(), expect_same=None, dkim=False):
        """Nothing else changed, Compare agrees, and a new backup matches the server."""
        after = snap()
        only_under(self.before if expect_same is None else expect_same, after, others)
        odd = [l for l in self.compare(self.night) if not any(m in l for m in compare_may_differ)]
        check(not odd, f"Compare with the backup shows: {odd}")
        if dkim:
            dkim_good(self.key)
        self.tool("run")
        odd = self.compare(time.strftime("%Y-%m-%d"))
        check(not odd, f"Compare with a new backup shows: {odd}")


def replace(path, data):
    """Change a backup file without touching anything hard-linked to it."""
    tmp = path.with_name(path.name + ".new")
    tmp.write_bytes(data)
    os.replace(tmp, path)


def break_kdc():
    me("""sqlite3 deployed/kdc/app/instance/minipass.db "DELETE FROM user;" && rm deployed/kdc/app/static/uploads/photo-kdc.png""")


def t01():
    t = Test("t01-customer-data")
    t.start()
    break_kdc()
    r = t.tool("restore", "customer", "kdc", "--confirm", "kdc")
    check("gone from server" in r.stdout and "user -2" in r.stdout, "the preview didn't show what's different")
    check(login_page("kdc"), "kdc's login page doesn't work")
    check(me("sqlite3 deployed/kdc/app/instance/minipass.db 'SELECT count(*) FROM user'").stdout.strip() == "2", "the users aren't back")
    check(login_page("testb"), "testb's login page doesn't work")
    check(any("restore OK" in m for m in discord), f"no report on Discord: {discord}")
    t.finish()


def t02():
    t = Test("t02-customer-folder")
    t.start()
    root(f"rm -rf {BASE}/deployed/kdc")
    t.tool("restore", "customer", "kdc", "--confirm", "kdc")
    check(login_page("kdc"), "kdc wasn't rebuilt")
    commit = (t.night_dir / "customers/kdc/code-version.txt").read_text().split()[0]
    check(me("git -C deployed/kdc/app rev-parse HEAD").stdout.strip() == commit, "not the code version from the backup")
    check(me("stat -c %u deployed/kdc/app/instance/minipass.db").stdout.strip() == "1000", "wrong owner")
    t.finish()


def t03():
    t = Test("t03-not-on-list", restore_allowed=["kdc"])
    t.start()
    r = t.tool("restore", "customer", "testb", "--confirm", "testb", ok=False)
    check("restore_allowed" in r.text, r.text)
    r = t.tool("restore", "customer", "nobody", "--confirm", "nobody", ok=False)
    check("restore_allowed" in r.text, r.text)
    check(not undo_folders(), "something was left on the server")
    t.finish()


def t04():
    # Every kind of restore can be undone, back to exactly how it was just before.
    t = Test("t04-undo")
    t.start()

    break_kdc()
    me("""sqlite3 deployed/kdc/app/instance/minipass.db "INSERT INTO user (name,email,email_opt_out) VALUES ('New','new@example.invalid',0);" """)
    me("echo extra > deployed/kdc/app/static/uploads/extra.txt; echo '# changed' >> deployed/kdc/app/.env")
    broken = snap()
    t.tool("restore", "customer", "kdc", "--confirm", "kdc")
    same_under(t.before, snap(), "customer/kdc")
    t.tool("restore", "undo", "--confirm", "RESTORE")
    same_under(broken, snap(), "customer/kdc")
    check(login_page("kdc"), "kdc doesn't work after the undo")
    t.tool("restore", "customer", "kdc", "--confirm", "kdc")

    root(f"rm -rf {BASE}/config/postfix-virtual.cf; echo '# changed' >> {BASE}/config/postfix-accounts.cf")
    broken = snap()
    t.tool("restore", "mail", "settings", "--confirm", "RESTORE")
    same_under(t.before, snap(), "mail/settings")
    t.tool("restore", "undo", "--confirm", "RESTORE")
    same_under(broken, snap(), "mail/settings")
    t.tool("restore", "mail", "settings", "--confirm", "RESTORE")

    root(f"rm -f {BASE}/maildata/minipass.me/bob/new/*")
    broken = snap()
    t.tool("restore", "mail", "messages", "--confirm", "RESTORE")
    same_under(t.before, snap(), "mail/msg")
    t.tool("restore", "undo", "--confirm", "RESTORE")
    same_under(broken, snap(), "mail/msg")
    t.tool("restore", "mail", "messages", "--confirm", "RESTORE")

    me("""sqlite3 MinipassWebSite/customers.db "DELETE FROM customers WHERE subdomain='testb';"; echo '# changed' >> MinipassWebSite/.env""")
    broken = snap()
    t.tool("restore", "website", "--confirm", "RESTORE")
    same_under(t.before, snap(), "website")
    t.tool("restore", "undo", "--confirm", "RESTORE")
    same_under(broken, snap(), "website")
    t.tool("restore", "website", "--confirm", "RESTORE")

    me("echo '# changed' >> nginx/controller-proxy.conf; rm vhost.d/kdc.minipass.me_location; echo '# changed' >> .env; crontab -r")
    broken = snap()
    t.tool("restore", "server", "--confirm", "RESTORE")
    same_under(t.before, snap(), "server")
    t.tool("restore", "undo", "--confirm", "RESTORE")
    same_under(broken, snap(), "server")
    t.tool("restore", "server", "--confirm", "RESTORE")

    check(login_page("kdc") and website_up(), "a site is down at the end")
    t.finish(others=("mailstate/",))


def t05():
    t = Test("t05-bad-env-rolls-back")
    t.start()
    # The app refuses to start without its secret key.
    env = t.night_dir / "customers/kdc/.env"
    replace(env, re.sub(rb"FLASK_SECRET_KEY=.*", b"FLASK_SECRET_KEY=", env.read_bytes()))
    r = t.tool("restore", "customer", "kdc", "--confirm", "kdc", ok=False, timeout=900)
    check("undo copy was put back" in r.text, r.text[-800:])
    check(login_page("kdc"), "kdc doesn't work after the rollback")
    same_under(t.before, snap(), "customer/kdc")
    check(any("FAILED" in m and "undo copy" in m for m in discord), f"not reported on Discord: {discord}")
    t.finish(compare_may_differ=("kdc settings",))


def t06():
    t = Test("t06-mail-settings")
    t.start()
    root(f"cd {BASE}/config && rm -rf postfix-accounts.cf postfix-virtual.cf user-patches opendkim; rm -rf {BASE}/maildata/minipass.me/kdc_app/home/sieve {BASE}/maildata/minipass.me/kdc_app/home/.dovecot.sieve")
    t.tool("restore", "mail", "settings", "--confirm", "RESTORE")
    check(imap("alice@minipass.me", "alice-pass-1"), "alice can't log in")
    check(imap("bob@minipass.me", "bob-pass-1"), "bob can't log in")
    check("ACTIVE" in root("docker exec mailserver doveadm sieve list -u kdc_app@minipass.me").stdout, "the forward isn't on")
    subject = "forward-check-" + os.urandom(4).hex()
    send("bob@minipass.me", "kdc_app@minipass.me", subject)
    check(box_has("alice", subject) == 1, "the forward doesn't forward")
    send("bob@minipass.me", "sales@minipass.me", subject + "-alias")
    check(box_has("bob", subject + "-alias") == 1, "the alias doesn't work")
    t.finish(others=("mail/msg/minipass.me/alice/", "mail/msg/minipass.me/bob/", "mail/msg/minipass.me/kdc_app/", "mailstate/"),
             compare_may_differ=("Mail messages",), dkim=True)
    after = snap()
    gone = [k for k in t.before if k.startswith("mail/msg/") and after.get(k) != t.before[k]]
    check(not gone, f"messages changed: {gone}")


def t07():
    t = Test("t07-keep-passwords", mail_check_user="bob@minipass.me", mail_check_password="bob-pass-1")
    t.start()
    # A new password today, as after a hack. The forwards get lost too.
    new = root("docker exec mailserver doveadm pw -s SHA512-CRYPT -p alice-pass-2").stdout.strip()
    root(f"""sed -i 's#^alice@minipass.me|.*#alice@minipass.me|{new}#' {BASE}/config/postfix-accounts.cf; cd {BASE}/config && rm -rf postfix-virtual.cf user-patches""")
    accounts = snap()["mail/settings/accounts/postfix-accounts.cf"]
    r = t.tool("restore", "mail", "settings", "--only", "forwards", "--confirm", "RESTORE")
    check("Forwards to Gmail" in r.stdout and "Mail accounts" not in r.stdout, "the preview shows the wrong things")
    after = snap()
    check(after["mail/settings/accounts/postfix-accounts.cf"] == accounts, "the accounts file changed")
    same_under(t.before, after, "mail/settings/forwards")
    check(imap("alice@minipass.me", "alice-pass-2"), "the new password stopped working")
    check(not imap("alice@minipass.me", "alice-pass-1", tries=1), "the old password came back")
    t.finish(others=("mail/settings/accounts/postfix-accounts.cf", "mailstate/"), compare_may_differ=("Mail accounts",))


def t08():
    t = Test("t08-messages")
    t.start()
    alice = f"{BASE}/maildata/minipass.me/alice"
    # Lost: one unread message, one in the Archive folder. Read since the backup: one more. New: one.
    lost = root(f"ls {alice}/new | grep -v ':2,' | head -1").stdout.strip()
    keep = root(f"ls {alice}/new | grep -v ':2,' | tail -1").stdout.strip()
    check(lost and keep and lost != keep, "the seed has too few unread messages")
    archived = root(f"ls {alice}/.Archive/cur | head -1").stdout.strip()
    root(f"rm {shlex.quote(alice + '/new/' + lost)} {shlex.quote(alice + '/.Archive/cur/' + archived)}")
    root(f"docker exec mailserver doveadm force-resync -u alice@minipass.me '*'")
    subject = "new-after-backup"
    send("bob@minipass.me", "alice@minipass.me", subject)
    check(box_has("alice", subject) == 1, "the new mail didn't arrive")
    # Reading renames the file, moving it to cur/ with a flag.
    bob_new = root(f"ls {BASE}/maildata/minipass.me/bob/new | grep -v ':2,' | head -1").stdout.strip()
    root(f"cd {BASE}/maildata/minipass.me/bob && chown 5000:5000 . && mv {shlex.quote('new/' + bob_new)} {shlex.quote('cur/' + bob_new + ':2,S')}")
    root(f"cd {alice} && mv {shlex.quote('new/' + keep)} {shlex.quote('cur/' + keep + ':2,RS')}")
    root("docker exec mailserver doveadm force-resync -u alice@minipass.me '*'; docker exec mailserver doveadm force-resync -u bob@minipass.me '*'")
    read_state = snap()

    t.tool("restore", "mail", "messages", "--confirm", "RESTORE")
    after = snap()
    for key in (lost, archived.split(":2,")[0]):
        check(after.get(f"mail/msg/minipass.me/alice/{key}") == t.before[f"mail/msg/minipass.me/alice/{key}"], f"not back: {key}")
    check(box_has("alice", subject) == 1, "the new mail is gone")
    check(not [k for k in after if k.endswith("+dup")], "duplicates: " + str([k for k in after if k.endswith("+dup")]))
    check(after[f"mailstate/flags/minipass.me/bob/{bob_new}"] == read_state[f"mailstate/flags/minipass.me/bob/{bob_new}"], "a read message lost its flag")
    check(after[f"mailstate/flags/minipass.me/alice/{keep}"].endswith("RS"), "a read message lost its flag")
    expected = sum(1 for k in after if k.startswith("mail/msg/minipass.me/alice/"))
    seen = root("docker exec mailserver doveadm -f flow mailbox status -u alice@minipass.me messages '*'").stdout
    check(sum(int(n) for n in re.findall(r"messages=(\d+)", seen)) == expected, f"Dovecot sees: {seen} expected {expected}")
    check(imap("alice@minipass.me", "alice-pass-1"), "alice can't log in")
    t.finish(others=("mail/msg/minipass.me/alice/", "mailstate/"), compare_may_differ=("Mail messages",))
    gone = [k for k in t.before if k.startswith("mail/msg/") and after.get(k) != t.before[k]]
    check(not gone, f"messages changed: {gone}")


def t09():
    t = Test("t09-mail-folders")
    t.start()
    # The contents first, then the folder itself, which the running mail server still holds on to.
    for how in (f"rm -rf {BASE}/maildata/minipass.me", f"rm -rf {BASE}/maildata"):
        root(how)
        t.tool("restore", "mail", "messages", "--confirm", "RESTORE")
        after = snap()
        wrong = [k for k in t.before if k.startswith("mail/msg/") and after.get(k) != t.before[k]]
        check(not wrong, f"messages not back after {how}: {wrong}")
        check(not [k for k in after if k.endswith("+dup")], "duplicates")
        check(mail_up(), "mail isn't up")
        check(imap("alice@minipass.me", "alice-pass-1"), "alice can't log in")
        seen = root("docker exec mailserver doveadm -f flow mailbox status -u alice@minipass.me messages '*'").stdout
        check(sum(int(n) for n in re.findall(r"messages=(\d+)", seen)) == 5, f"Dovecot sees: {seen}")
        check("Archive" in seen, f"the Archive folder is gone: {seen}")
    subject = "after-the-restore"
    send("bob@minipass.me", "alice@minipass.me", subject)
    check(box_has("alice", subject) == 1, "new mail doesn't arrive")
    check("ACTIVE" in root("docker exec mailserver doveadm sieve list -u kdc_app@minipass.me").stdout, "the forward isn't on")
    t.finish(others=("mail/msg/minipass.me/alice/", "mailstate/"), compare_may_differ=("Mail messages",))


def hold_a_message(subject):
    """Mail that's waiting to be delivered: the delivery side is on hold, so it sits in the queue."""
    root("docker exec mailserver postconf -e defer_transports=lmtp && docker exec mailserver postfix reload")
    time.sleep(2)
    send("bob@minipass.me", "alice@minipass.me", subject)
    for _ in range(30):
        if root("docker exec mailserver postqueue -j").stdout.strip():
            return
        time.sleep(1)
    raise Failed("the message didn't wait in the queue")


def queue_in_backup(t):
    listing = sh(["tar", "tf", str(t.night_dir / "mail/mail.tar")]).stdout
    return re.search(r"spool-postfix/(deferred|active|incoming|maildrop)/.*[^/]$", listing, re.M)


def clear_queue():
    root("docker exec mailserver postsuper -d ALL; docker exec mailserver postconf -e defer_transports=; docker exec mailserver postfix reload")


def t10():
    t = Test("t10-queue")
    sh([str(HERE / "reset.sh")])
    subject = "waiting-at-backup-time"
    hold_a_message(subject)
    t.backup_and_age()
    check(queue_in_backup(t), "the test isn't fair: the waiting mail isn't in the backup")
    clear_queue()
    root(f"rm -f {BASE}/maildata/minipass.me/bob/new/*")
    t.tool("restore", "mail", "settings", "--confirm", "RESTORE")
    t.tool("restore", "mail", "messages", "--confirm", "RESTORE")
    check(mail_up(), "mail isn't up")
    time.sleep(15)
    check(not root("docker exec mailserver postqueue -j").stdout.strip(), "the queue isn't empty")
    check(box_has("alice", subject, tries=1) == 0, "the waiting mail was delivered after the restore")
    subject2 = "mail-still-works"
    send("bob@minipass.me", "alice@minipass.me", subject2)
    check(box_has("alice", subject2) == 1, "mail doesn't deliver")
    t.finish(others=("mail/msg/minipass.me/alice/", "mailstate/"), compare_may_differ=("Mail messages",))


def t11():
    t = Test("t11-website")
    t.start()
    me("rm MinipassWebSite/customers.db; echo 'SECRET_KEY_IS_GONE=1' > MinipassWebSite/.env")
    root("systemctl restart minipass-web")
    t.tool("restore", "website", "--confirm", "RESTORE")
    check(website_up(), "the website's page doesn't work")
    names = me("sqlite3 MinipassWebSite/customers.db 'SELECT subdomain FROM customers ORDER BY 1'").stdout.split()
    check(names == ["kdc", "testb"], f"customer list: {names}")
    t.finish()


def t11b_sudo():
    # Without sudo it must say so clearly, and change nothing.
    t = Test("t11b-no-sudo")
    t.start()
    root("mv /etc/sudoers.d/fakevps /root/fakevps-sudoers")
    try:
        me("echo '# changed' >> MinipassWebSite/.env")
        changed = snap()
        r = t.tool("restore", "website", "--confirm", "RESTORE", ok=False)
        check("sudo isn't allowed" in r.text, r.text[-500:])
        same_under(changed, snap(), "website")
        check(not undo_folders(), "something was left on the server")
    finally:
        root("mv /root/fakevps-sudoers /etc/sudoers.d/fakevps")


def t12():
    t = Test("t12-server")
    t.start()
    root(f"cd {BASE} && rm -rf nginx vhost.d certs acme docker-compose.yml .env mailserver.env; rm -f /home/kdresdell/.mailpass; crontab -u kdresdell -r; echo '# changed' >> /etc/systemd/system/minipass-web.service")
    # With its settings gone the proxy can't come back up: Docker refuses to restart it, and the sites are down.
    root("docker restart nginx-proxy flask-controller-proxy", ok=False)
    check(not page("kdc.minipass.me"), "the test isn't fair: the site still answers")
    t.tool("restore", "server", "--confirm", "RESTORE")
    for c in ("kdc", "testb"):
        check(login_page(c), f"{c} doesn't answer")
    check(website_up(), "the website doesn't answer")
    check(mail_up(), "mail isn't up")
    check(imap("alice@minipass.me", "alice-pass-1"), "alice can't log in")
    check(me("stat -c %a ~/.mailpass").stdout.strip() == "600", ".mailpass is readable by others")
    subject = sh("openssl x509 -noout -subject", input=root(f"cat {BASE}/certs/kdc.minipass.me/fullchain.pem").stdout).stdout
    check("kdc.minipass.me" in subject, subject)
    served = root("curl -skv --resolve kdc.minipass.me:443:127.0.0.1 https://kdc.minipass.me/login 2>&1 | grep -i 'subject:'", ok=False).stdout
    check("kdc.minipass.me" in served, f"nginx serves the wrong certificate: {served}")
    t.finish(others=("mailstate/",))


def blank_server():
    root(f"""systemctl stop minipass-web; docker ps -aq | xargs -r docker rm -f; docker network rm minipass_env_proxy; docker volume rm minipass_env_nginx-cache
             docker rmi kdc-flask-app testb-flask-app
             rm -rf {BASE} /home/kdresdell/.mailpass /home/kdresdell/mptool-undo /etc/systemd/system/minipass-web.service
             systemctl daemon-reload; crontab -u kdresdell -r; true""")


def t13():
    t = Test("t13-everything")
    sh([str(HERE / "reset.sh")])
    subject = "waiting-at-backup-time"
    hold_a_message(subject)
    t.backup_and_age()
    check(queue_in_backup(t), "the test isn't fair: the waiting mail isn't in the backup")
    clear_queue()
    blank_server()
    start = time.time()
    r = t.tool("restore", "everything", "--confirm", "RESTORE", timeout=1800)
    took = time.time() - start
    check(re.search(r"Took \d+ min \d+ s", r.stdout), "no time printed")
    print(f"      whole-server restore took {int(took)} s", flush=True)
    (RUN / "whole-server-seconds").write_text(str(int(took)))
    for part in ("Code", "Server settings and certificates", "Mail", "Website", "Customer kdc", "Customer testb"):
        check(f"OK     {part}:" in r.stdout, f"{part} isn't reported OK:\n{r.stdout[-1200:]}")
    for c in ("kdc", "testb"):
        check(login_page(c), f"{c} doesn't work")
    check(website_up(), "the website doesn't work")
    names = me("sqlite3 MinipassWebSite/customers.db 'SELECT subdomain FROM customers ORDER BY 1'").stdout.split()
    check(names == ["kdc", "testb"], f"customer list: {names}")
    check(mail_up(), "mail isn't up")
    for user, password in (("alice", "alice-pass-1"), ("bob", "bob-pass-1"), ("kdc_app", "kdc-pass-1")):
        check(imap(f"{user}@minipass.me", password), f"{user} can't log in")
    check("ACTIVE" in root("docker exec mailserver doveadm sieve list -u kdc_app@minipass.me").stdout, "the forward isn't on")
    check(not root("docker exec mailserver postqueue -j").stdout.strip(), "the queue isn't empty")
    check(box_has("alice", subject, tries=1) == 0, "the waiting mail was delivered after the restore")
    check(root("systemctl is-enabled minipass-web").stdout.strip() == "enabled", "the website service isn't enabled")
    time.sleep(1)
    r = t.tool("restore", "undo", "--confirm", "RESTORE", ok=False)
    check("whole-server" in r.text, r.text[-300:])
    t.finish(others=("mailstate/",), dkim=True)


def t13b_not_blank():
    t = Test("t13b-not-blank")
    t.start()
    r = t.tool("restore", "everything", "--confirm", "RESTORE", ok=False)
    check("isn't blank" in r.text, r.text[-300:])
    check(not undo_folders(), "something was left on the server")
    t.finish()


def t14():
    t = Test("t14-damaged-backup")
    t.start()
    me("""sqlite3 deployed/kdc/app/instance/minipass.db "DELETE FROM user;" """)
    me("echo '# changed' >> MinipassWebSite/.env")
    broken = snap()
    keep = t.dir / "good"
    keep.mkdir()
    cases = [
        ("customers/kdc/minipass.db", lambda b: b"this is not a database" * 100, [("customer", "kdc", "--confirm", "kdc"), ("everything", "--confirm", "RESTORE")]),
        ("website/customers.db", lambda b: b[:2000] + b"\xff" * 2000 + b[4000:], [("website", "--confirm", "RESTORE")]),
        ("mail/mail.tar", lambda b: b[:len(b) // 2], [("mail", "messages", "--confirm", "RESTORE"), ("mail", "settings", "--confirm", "RESTORE")]),
        ("server/certs.tar", lambda b: b[:len(b) // 2], [("server", "--confirm", "RESTORE")]),
    ]
    for rel, damage, commands in cases:
        path = t.night_dir / rel
        good = path.read_bytes()
        replace(path, damage(good))
        for command in commands:
            r = t.tool("restore", *command, ok=False)
            check("failed its check" in r.text, f"{command}: {r.text[-400:]}")
            check(not undo_folders(), f"{command} left something on the server")
            only_under(broken, snap(), ())
        replace(path, good)
    # The archive cut exactly between two files, which plain `tar` doesn't notice.
    path = t.night_dir / "mail/mail.tar"
    good = path.read_bytes()
    entries = sh(["tar", "tvRf", str(path)]).stdout.splitlines()
    blocks = int(re.match(r"block (\d+):", entries[len(entries) // 2]).group(1))
    replace(path, good[: blocks * 512])
    sh(["tar", "tf", str(path)])
    r = t.tool("restore", "mail", "messages", "--confirm", "RESTORE", ok=False)
    check("failed its check" in r.text, r.text[-400:])
    replace(path, good)
    t.finish(expect_same=broken, compare_may_differ=("kdc database", "API keys"))


def wait_for_log(text, tries=1500):
    for _ in range(tries):
        r = root(f"grep -ls {shlex.quote(text)} /home/kdresdell/mptool-undo/*/log", ok=False)
        if r.stdout.strip():
            return r.stdout.split()[-1].rsplit("/", 1)[0]
        time.sleep(0.05)
    raise Failed(f"never saw: {text}")


def t15():
    t = Test("t15-connection-cut")
    t.start()
    break_kdc()
    broken = snap()
    p = t.background("restore", "customer", "kdc", "--confirm", "kdc")
    stage = wait_for_log("Putting the files back")
    # Cut it: every session dropped, and no new connection possible, for longer than the tool waits.
    root("iptables -I INPUT -p tcp --dport 2222 -j DROP; pkill -u kdresdell -x sshd; true")
    try:
        output, _ = p.communicate(timeout=300)
        (t.dir / "tool.log").open("a").write(output)
        check(p.returncode != 0, f"the tool says it worked without knowing:\n{output[-500:]}")
        check("mptool restore undo" in output and "mptool restore status" in output, f"it doesn't say how to finish:\n{output[-600:]}")
    finally:
        root("iptables -D INPUT -p tcp --dport 2222 -j DROP")
    for _ in range(120):
        if root(f"cat {stage}/result", ok=False).stdout.strip():
            break
        time.sleep(1)
    check(root(f"cat {stage}/result").stdout.strip() == "ok", "the restore didn't finish by itself")
    check(login_page("kdc"), "kdc doesn't work after the cut")
    same_under(t.before, snap(), "customer/kdc")
    r = t.tool("restore", "status")
    check("put back and checked" in r.stdout, r.stdout)
    t.tool("restore", "undo", "--confirm", "RESTORE")
    same_under(broken, snap(), "customer/kdc")

    # Worse: the script on the server is killed halfway.
    p = t.background("restore", "customer", "kdc", "--confirm", "kdc")
    undone = root("ls -1 /home/kdresdell/mptool-undo").stdout.split()
    for _ in range(600):
        stages = root("ls -1 /home/kdresdell/mptool-undo").stdout.split()
        new = [s for s in stages if s not in undone]
        if new and root(f"grep -qs 'Putting the files back' /home/kdresdell/mptool-undo/{new[0]}/log", ok=False).returncode == 0:
            break
        time.sleep(0.02)
    else:
        raise Failed("the second restore never got going")
    stage = f"/home/kdresdell/mptool-undo/{new[0]}"
    root(f"kill -9 $(cat {stage}/pid)")
    output, _ = p.communicate(timeout=300)
    (t.dir / "tool.log").open("a").write(output)
    if root(f"cat {stage}/result", ok=False).stdout.strip() == "":
        check(p.returncode != 0 and "mptool restore undo" in output, f"it doesn't say how to finish:\n{output[-600:]}")
    t.tool("restore", "undo", "--confirm", "RESTORE")
    same_under(broken, snap(), "customer/kdc")
    check(login_page("kdc"), "kdc doesn't work after the undo")
    t.tool("restore", "customer", "kdc", "--confirm", "kdc")
    t.finish()


def t16():
    t = Test("t16-one-at-a-time")
    t.start()
    # A restore while a backup runs. The backup must really still be running when it's refused.
    for attempt in range(10):
        p = t.background("run")
        while p.poll() is None and not list(t.backups.glob(".incomplete-*")):
            time.sleep(0.002)
        r = t.tool("restore", "website", "--confirm", "RESTORE", ok=None)
        was_running = p.poll() is None
        p.communicate()
        if was_running:
            check(r.returncode != 0 and "already running" in r.text, r.text[-300:])
            break
    else:
        raise Failed("the backup was always too quick to test against")
    today = time.strftime("%Y-%m-%d")

    # A backup while a restore runs.
    night_before = (t.backups / today / "status.json").read_bytes()
    p = t.background("restore", "mail", "settings", "--confirm", "RESTORE", "--night", t.night)
    wait_for_log("Saving an undo copy")
    r = t.tool("run", ok=False)
    check("already running" in r.text, r.text[-300:])
    r = t.tool("restore", "website", "--confirm", "RESTORE", ok=False)
    check("already running" in r.text, r.text[-300:])
    check(p.poll() is None, "the restore finished too soon for this test to mean anything")
    output, _ = p.communicate(timeout=600)
    check(p.returncode == 0, output[-500:])
    check((t.backups / today / "status.json").read_bytes() == night_before, "the refused backup changed a night")

    # The tool that started the restore is gone, but the restore is still running on the server.
    root("rm -rf /home/kdresdell/mptool-undo")
    p = t.background("restore", "mail", "settings", "--confirm", "RESTORE", "--night", t.night)
    stage = wait_for_log("Starting")
    p.kill()
    p.communicate()
    check(root(f"cat {stage}/result", ok=False).stdout == "", "the restore finished too soon for this test to mean anything")
    r = t.tool("run", ok=False)
    check("still running" in r.text, r.text[-300:])
    r = t.tool("restore", "website", "--confirm", "RESTORE", ok=False)
    check("still running" in r.text, r.text[-300:])
    for _ in range(300):
        if root(f"cat {stage}/result", ok=False).stdout.strip():
            break
        time.sleep(1)
    check(root(f"cat {stage}/result").stdout.strip() == "ok", "the restore didn't finish by itself")
    t.finish(others=("mailstate/",))


def t17():
    t = Test("t17-locks")
    t.start()
    me("""sqlite3 deployed/kdc/app/instance/minipass.db "DELETE FROM user;" """)
    broken = snap()
    # A wrong word changes nothing.
    for args, word in ((("customer", "kdc"), "testb"), (("customer", "kdc"), "KDC"), (("customer", "kdc"), ""),
                       (("website",), "restore"), (("server",), "yes"), (("mail", "messages"), "RES")):
        r = t.tool("restore", *args, "--confirm", word, ok=False)
        check("Nothing was changed" in r.text, r.text[-300:])
    # Nobody answering the question counts as no.
    r = t.tool("restore", "customer", "kdc", ok=False)
    check("Nothing was changed" in r.text, r.text[-300:])
    check(not undo_folders(), "something was left on the server")
    only_under(broken, snap(), ())

    # The lock is off: refused before any connection, so an unreachable server gets the same answer.
    off = {**t.settings, "restore_enabled": False, "port": 1}
    config = t.write_config("off.toml", off)
    for args in (("customer", "kdc", "--confirm", "kdc"), ("mail", "settings", "--confirm", "RESTORE"), ("mail", "messages", "--confirm", "RESTORE"),
                 ("website", "--confirm", "RESTORE"), ("server", "--confirm", "RESTORE"), ("everything", "--confirm", "RESTORE"), ("undo", "--confirm", "RESTORE")):
        r = t.tool("restore", *args, ok=False, config=config)
        check("Restore is off" in r.text, f"{args}: {r.text[-300:]}")
    # On by default? No: a config that doesn't mention it is off.
    silent = {k: v for k, v in off.items() if k != "restore_enabled"}
    r = t.tool("restore", "website", "--confirm", "RESTORE", ok=False, config=t.write_config("silent.toml", silent))
    check("Restore is off" in r.text, r.text[-300:])
    t.finish(expect_same=broken, compare_may_differ=("kdc database",))


class Screen:
    """The real menu, in a real terminal, on a tmux server of its own."""

    def __init__(self, t):
        self.tmux = ["tmux", "-L", "fakevps"]
        sh(self.tmux + ["kill-server"], ok=False)
        env = " ".join(f"{k}={shlex.quote(v)}" for k, v in t.env().items() if k in ("MPTOOL_CONFIG", "HOME"))
        sh(self.tmux + ["-f", "/dev/null", "new-session", "-d", "-s", "menu", "-x", "130", "-y", "45", f"env {env} TERM=xterm-256color {TOOL}"])
        self.wait("Nights kept")

    def keys(self, *keys):
        sh(self.tmux + ["send-keys", "-t", "menu", *keys])
        time.sleep(0.5)

    def type(self, text):
        sh(self.tmux + ["send-keys", "-t", "menu", "-l", text])
        time.sleep(0.3)
        self.keys("Enter")

    def text(self):
        return sh(self.tmux + ["capture-pane", "-p", "-t", "menu"]).stdout

    def wait(self, wanted, seconds=600):
        for _ in range(seconds * 2):
            screen = self.text()
            if wanted in screen:
                return screen
            time.sleep(0.5)
        raise Failed(f"never showed '{wanted}':\n{screen}")

    def close(self):
        sh(self.tmux + ["kill-server"], ok=False)


def t18():
    t = Test("t18-menu")
    t.start()
    screen = Screen(t)
    try:
        # r: a wrong word first, then the right one.
        break_kdc()
        screen.keys("r")
        screen.wait("kdc")
        screen.keys("Enter")
        screen.wait("Type kdc to go ahead")
        check("user -2" in screen.text(), "the preview doesn't show what's different")
        screen.type("testb")
        screen.wait("Nothing was changed")
        check(not undo_folders(), "a wrong word left something on the server")
        screen.keys("Enter")
        screen.wait("Enter to preview")
        screen.keys("Enter")
        screen.wait("Type kdc to go ahead")
        screen.type("kdc")
        screen.wait("Restore of customer kdc OK")
        check(login_page("kdc"), "kdc doesn't work")
        same_under(t.before, snap(), "customer/kdc")
        screen.keys("Escape")

        # s: mail settings, only the forwards.
        root(f"cd {BASE}/config && rm -rf postfix-virtual.cf user-patches; sed -i 's/^bob@minipass.me|.*/bob@minipass.me|{{SHA512-CRYPT}}changed/' postfix-accounts.cf")
        accounts = snap()["mail/settings/accounts/postfix-accounts.cf"]
        screen.keys("s")
        screen.wait("Mail settings")
        screen.keys("a")
        screen.keys("d")
        check("[ ] a accounts" in screen.text() and "[x] f forwards" in screen.text() and "[ ] d DKIM key" in screen.text(), "the choices don't show")
        screen.keys("Enter")
        screen.wait("Type RESTORE to go ahead")
        screen.type("RESTORE")
        screen.wait("Restore of mail settings (forwards) OK")
        after = snap()
        same_under(t.before, after, "mail/settings/forwards")
        check(after["mail/settings/accounts/postfix-accounts.cf"] == accounts, "the accounts were put back too")
        screen.keys("Escape")

        # s: the accounts, on their own.
        screen.keys("s")
        screen.wait("Mail settings")
        screen.keys("f")
        screen.keys("d")
        screen.keys("Enter")
        screen.wait("Type RESTORE to go ahead")
        screen.type("RESTORE")
        screen.wait("Restore of mail settings (accounts) OK")
        same_under(t.before, snap(), "mail/settings")
        screen.keys("Escape")

        # s: mail messages, website, server, then undo, each by moving the cursor.
        root(f"rm -f {BASE}/maildata/minipass.me/bob/new/*")
        screen.keys("s")
        screen.wait("Mail messages")
        screen.keys("Down")
        screen.keys("Enter")
        screen.wait("Type RESTORE to go ahead")
        screen.type("RESTORE")
        screen.wait("Restore of mail messages OK")
        same_under(t.before, snap(), "mail/msg")
        screen.keys("Escape")

        me("""sqlite3 MinipassWebSite/customers.db "DELETE FROM customers;" """)
        screen.keys("s")
        screen.wait("Website")
        screen.keys("Down", "Down")
        screen.keys("Enter")
        screen.wait("Type RESTORE to go ahead")
        screen.type("RESTORE")
        screen.wait("Restore of website OK")
        same_under(t.before, snap(), "website")
        check(website_up(), "the website doesn't work")
        screen.keys("Escape")

        me("rm vhost.d/kdc.minipass.me_location")
        screen.keys("s")
        screen.wait("Server settings")
        screen.keys("Down", "Down", "Down")
        screen.keys("Enter")
        screen.wait("Type RESTORE to go ahead")
        screen.type("RESTORE")
        screen.wait("Restore of server settings OK")
        same_under(t.before, snap(), "server")
        screen.keys("Escape")

        # Undo the server restore from the menu, then do it again.
        screen.keys("s")
        screen.wait("Undo the last restore")
        screen.keys("Down", "Down", "Down", "Down", "Down")
        screen.keys("Enter")
        screen.wait("Type RESTORE to go ahead")
        screen.type("RESTORE")
        screen.wait("Restore of undo of the last restore OK")
        check("server/vhost.d/kdc.minipass.me_location" not in snap(), "the undo didn't undo")
        screen.keys("Escape")
        t.tool("restore", "server", "--confirm", "RESTORE", "--night", t.night)

        # q leaves the menu normally.
        screen.keys("q")
    finally:
        screen.close()
    t.finish(others=("mailstate/",))


ALL = {"1": t01, "2": t02, "3": t03, "4": t04, "5": t05, "6": t06, "7": t07, "8": t08, "9": t09, "10": t10, "11": t11, "11b": t11b_sudo,
       "12": t12, "13": t13, "13b": t13b_not_blank, "14": t14, "15": t15, "16": t16, "17": t17, "18": t18}


def main():
    check(TOOL.exists(), f"build mptool first: {TOOL}")
    server = http.server.ThreadingHTTPServer(("127.0.0.1", DISCORD_PORT), Discord)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    wanted = sys.argv[1:] or list(ALL)
    RUN.mkdir(parents=True)
    failed = []
    started = time.time()
    for name in wanted:
        began = time.time()
        print(f"test {name}: {ALL[name].__name__}...", flush=True)
        try:
            ALL[name]()
            print(f"  PASS ({int(time.time() - began)} s)", flush=True)
        except Exception as e:  # a failure in one test doesn't stop the others
            failed.append(name)
            print(f"  FAIL ({int(time.time() - began)} s): {e}", flush=True)
        root("iptables -D INPUT -p tcp --dport 2222 -j DROP 2>/dev/null; [ ! -f /root/fakevps-sudoers ] || mv /root/fakevps-sudoers /etc/sudoers.d/fakevps; true", ok=False)
    print(f"\n{len(wanted) - len(failed)} of {len(wanted)} passed in {int(time.time() - started)} s.", flush=True)
    if failed:
        print("Failed: " + ", ".join(failed), flush=True)
    print(f"Details: {RUN}", flush=True)
    sys.exit(1 if failed else 0)


if __name__ == "__main__":
    main()
