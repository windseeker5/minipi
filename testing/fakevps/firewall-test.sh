#!/bin/bash
# Prove the block from the fake VPS itself and from a container inside it, before any app starts.
# Each attempt must bump the DROP rule's own counter: that shows our rule stopped it.
set -u
TARGET=1.1.1.1
QUIZ=portquiz.net   # answers on every port, so a failed connect there really means "blocked"
fail=0

counter() { # port
    iptables -t mangle -L FAKEVPS_BLOCK -v -n -x | awk -v p="dpt:$1" '$0 ~ p" *$" || $0 ~ p"$" {print $1; exit}'
}
real_counter() {
    iptables -t mangle -L FAKEVPS_BLOCK -v -n -x | awk '$9 == "138.199.152.128" {print $1; exit}'
}
try_host() { timeout 4 bash -c "exec 3<>/dev/tcp/$1/$2" 2>/dev/null; }
try_box() { docker run --rm alpine:3.20 sh -c "timeout 4 nc -w 3 $1 $2 </dev/null" >/dev/null 2>&1; }

docker image inspect alpine:3.20 >/dev/null 2>&1 || docker pull -q alpine:3.20 >/dev/null
# The control must connect, or a failed connect below proves nothing.
if try_host "$QUIZ" 8080; then echo "ok   control: the fake VPS reaches $QUIZ on port 8080"
else echo "FAIL control: the fake VPS can't reach $QUIZ on port 8080"; fail=1; fi
if try_box "$QUIZ" 8080; then echo "ok   control: a container reaches $QUIZ on port 8080"
else echo "FAIL control: a container can't reach $QUIZ on port 8080"; fail=1; fi

for port in 25 465 587 2525; do
    before=$(counter "$port")
    if try_host "$QUIZ" "$port"; then echo "FAIL host -> $QUIZ:$port connected"; fail=1; fi
    if try_box "$QUIZ" "$port"; then echo "FAIL container -> $QUIZ:$port connected"; fail=1; fi
    try_host "$TARGET" "$port" || true
    after=$(counter "$port")
    if [ "${after:-0}" -le "${before:-0}" ]; then echo "FAIL port $port: the rule's counter did not move"; fail=1
    else echo "ok   port $port blocked (host and container, $((after - before)) packets dropped)"; fi
done

before=$(real_counter)
try_host 138.199.152.128 2222 && { echo "FAIL real VPS reachable from the fake VPS"; fail=1; }
after=$(real_counter)
if [ "${after:-0}" -le "${before:-0}" ]; then echo "FAIL real VPS: the rule's counter did not move"; fail=1
else echo "ok   real VPS blocked by our rule, the packet never left"; fi

[ "$fail" = 0 ] && echo "Firewall holds." || { echo "FIREWALL NOT HOLDING. Do not start anything."; exit 1; }
