# The fake VPS: Ubuntu 24.04 with systemd, ssh on 2222 and Docker inside, like the real server.
FROM ubuntu:24.04

ENV DEBIAN_FRONTEND=noninteractive
RUN apt-get update && apt-get install -y --no-install-recommends \
      systemd systemd-sysv dbus openssh-server sudo cron ca-certificates curl \
      iptables docker.io docker-compose-v2 \
      rsync sqlite3 git python3 python3-venv openssl \
    && rm -rf /var/lib/apt/lists/*

# docker-compose as its own command, like on the real server.
RUN ln -s /usr/libexec/docker/cli-plugins/docker-compose /usr/local/bin/docker-compose

# Services a container has no use for, and that shouldn't poke at the laptop's hardware.
RUN systemctl mask getty@tty1.service console-getty.service systemd-udevd.service \
      systemd-udevd-kernel.socket systemd-udevd-control.socket systemd-logind.service \
      systemd-modules-load.service systemd-sysctl.service systemd-firstboot.service \
      systemd-timesyncd.service apt-daily.timer apt-daily-upgrade.timer dpkg-db-backup.timer

# Some package takes user 1000, and the real server's kdresdell has it.
RUN if getent passwd 1000 >/dev/null; then userdel -r "$(getent passwd 1000 | cut -d: -f1)" 2>/dev/null || true; fi \
    && useradd -m -u 1000 -s /bin/bash kdresdell && usermod -aG docker kdresdell \
    && install -d -o kdresdell -g kdresdell -m 700 /home/kdresdell/.ssh \
    && install -d -o kdresdell -g kdresdell /srv/git /var/cache/fakevps

COPY sshd.conf /etc/ssh/sshd_config.d/fakevps.conf
COPY sudoers /etc/sudoers.d/fakevps
RUN chmod 440 /etc/sudoers.d/fakevps
COPY pip.conf /etc/pip.conf

# The firewall comes up before Docker, and Docker refuses to start without it.
COPY firewall.sh /usr/local/sbin/fakevps-firewall
COPY firewall.service /etc/systemd/system/fakevps-firewall.service
COPY docker-override.conf /etc/systemd/system/docker.service.d/fakevps.conf
RUN chmod 755 /usr/local/sbin/fakevps-firewall \
    && systemctl enable fakevps-firewall.service ssh.service docker.service cron.service \
    && systemctl disable ssh.socket 2>/dev/null; true

STOPSIGNAL SIGRTMIN+3
CMD ["/sbin/init"]
