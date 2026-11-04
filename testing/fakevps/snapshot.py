#!/usr/bin/env python3
"""A fingerprint of everything a restore could touch, as JSON. Runs as root inside fakevps. Only reads."""
import hashlib, json, os, sqlite3, subprocess

BASE = "/home/kdresdell/minipass_env"
out = {}


def sha(data):
    return hashlib.sha256(data).hexdigest()[:16]


def add_file(key, path):
    if os.path.islink(path):
        out[key] = "link:" + os.readlink(path)
    elif os.path.isfile(path):
        with open(path, "rb") as f:
            out[key] = sha(f.read())


def add_tree(prefix, top):
    for root, dirs, files in os.walk(top):
        for name in files + [d for d in dirs if os.path.islink(os.path.join(root, d))]:
            p = os.path.join(root, name)
            add_file(f"{prefix}/{os.path.relpath(p, top)}", p)


def add_db(key, path):
    if not os.path.isfile(path):
        return
    uri = f"file:{path}?mode=ro"
    try:
        c = sqlite3.connect(uri, uri=True, timeout=10)
        out[key] = sha("\n".join(c.iterdump()).encode())
        c.close()
    except sqlite3.Error as e:
        out[key] = f"unreadable: {e}"


deployed = f"{BASE}/deployed"
for c in sorted(os.listdir(deployed)) if os.path.isdir(deployed) else []:
    d = f"{deployed}/{c}"
    add_db(f"customer/{c}/db", f"{d}/app/instance/minipass.db")
    add_tree(f"customer/{c}/uploads", f"{d}/app/static/uploads")
    add_file(f"customer/{c}/env", f"{d}/app/.env")
    add_file(f"customer/{c}/compose", f"{d}/docker-compose.yml")
    db = f"{d}/app/instance/minipass.db"
    if os.path.exists(db):
        out[f"customer/{c}/db-owner"] = str(os.stat(db).st_uid)

mail = f"{BASE}/maildata"
for root, _dirs, files in os.walk(mail) if os.path.isdir(mail) else []:
    rel = os.path.relpath(root, mail).split("/")
    if os.path.basename(root) not in ("cur", "new") or (len(rel) > 2 and rel[2] == "home"):
        continue
    account = "/".join(rel[:2])
    folder = "/".join(rel[2:-1])
    for name in files:
        key, _, flags = name.partition(":2,")
        with open(os.path.join(root, name), "rb") as f:
            k = f"mail/msg/{account}/{key}"
            # The same message twice would be a duplicate: keep them apart so a test can see it.
            while k in out:
                k += "+dup"
            out[k] = sha(f.read())
        out[f"mailstate/flags/{account}/{key}"] = f"{folder}:{flags}"

cfg = f"{BASE}/config"
for root, _dirs, files in os.walk(cfg) if os.path.isdir(cfg) else []:
    for name in files:
        p = os.path.join(root, name)
        rel = os.path.relpath(p, cfg)
        # The mail server's own lock file comes and goes. restore.rs leaves it alone too.
        if name.endswith(".lock"):
            continue
        if rel.startswith("opendkim"):
            group = "dkim"
        elif rel.startswith("user-patches") or rel == "postfix-virtual.cf":
            group = "forwards"
        else:
            group = "accounts"
        add_file(f"mail/settings/{group}/{rel}", p)

add_db("website/customers", f"{BASE}/MinipassWebSite/customers.db")
add_db("website/monitoring", f"{BASE}/email_monitoring/monitoring.db")
add_file("website/env", f"{BASE}/MinipassWebSite/.env")

for f in (".env", "mailserver.env", "docker-compose.yml"):
    add_file(f"server/{f}", f"{BASE}/{f}")
for d in ("nginx", "vhost.d", "certs", "acme"):
    add_tree(f"server/{d}", f"{BASE}/{d}")
add_file("server/mailpass", "/home/kdresdell/.mailpass")
add_file("server/service", "/etc/systemd/system/minipass-web.service")
cron = subprocess.run(["crontab", "-u", "kdresdell", "-l"], capture_output=True)
if cron.returncode == 0:
    out["server/crontab"] = sha(cron.stdout)
print(json.dumps(out, indent=0, sort_keys=True))
