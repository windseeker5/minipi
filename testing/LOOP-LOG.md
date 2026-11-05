# Loop log

Short notes: what failed, why, what fixed it.

## Part 1: the fake VPS

- `useradd` exit 4: the Ubuntu image already gives uid 1000 to `ubuntu`. Dockerfile removes that user first.
- DNS failed inside: the laptop's DNS is 172.17.0.1, which inside fakevps is the inner Docker bridge. Fixed with `--dns`.
- Containers wouldn't start (overlay on overlay at `/var/lib/containerd`). The firewall test's control caught it. Fixed with a volume.
- Secret scan stopped on docs that only mention the prefix `sk_live_`. Now it needs a key after the prefix.
- `/login` gave 500, then a redirect to `/setup`: the seed lacked the real deploy's upgrade step and an admin row. Added both.
- `tar: --numeric-owners` typo in the seed and reset scripts. Fixed.
- A safety check refused deleting a failed fake backup in `.state/backups/*`. Not worked around and told Ken. The delete wasn't
  needed: a same-day backup replaces it, and each test run has its own folder under `.state/runs/`.
- Sudo on the fake VPS allows four commands, not the one in the plan: a blank server also has to install and enable the
  website's service file. Told Ken.

## Part 2 and 3: the restore tool

- T2: the undo copy failed when the customer folder was gone: BusyBox tar refuses to make an empty archive. Now writes an empty
  tar directly.
- T12: the break step itself stopped the test: Docker refuses to restart a proxy whose settings are gone. That is the break we
  want, so the test now expects it and checks the site really is down first.
- T18: the menu's result line never showed. Long log lines wrapped and pushed it off the pop-up. Results now have their own space
  at the bottom, and docker-compose's warning about an old setting no longer fills the log.
- T4, first clean pass: `check-for-changes.sh.lock` showed up as a changed mail setting. It's the mail server's own short-lived
  lock in the settings folder. A restore could have put a stale one back and deleted a live one. Restore and Compare now leave it
  alone, a unit test covers it, and only then did the test's fingerprint stop counting it.

## Finish line

Commit d6643c4, each pass from a clean rebuild: 20 of 20 restore tests, 18 of 18 `cargo test` with no network, no build warnings,
twice in a row. Whole-server restore: 68 s both times. (One "PASS 1 FAILED" line along the way was my own wrapper reading the exit
code the bash way in zsh. That pass was 20 of 20, and pass 2 was run again with a proper exit code.)

## After the finish line

- GitGuardian flagged a password in commit 2d2044d: a made-up test login in `testing/build.sh`, usable only on the fake server.
  No real secret was on GitHub (compared against the real settings files). Fake or not, it shouldn't be in a public repo. The fake
  server's logins and app secrets are now random, made at build or seed time, and kept in `.state/`, which git ignores.
