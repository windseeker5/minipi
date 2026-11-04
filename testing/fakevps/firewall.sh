#!/bin/bash
# Nothing leaves the fake VPS as mail, and nothing reaches the real VPS, from the fake VPS or its containers.
# In the mangle table, because Docker rewrites the filter table when it starts.
set -e
REAL_VPS=138.199.152.128

for cmd in iptables ip6tables; do
    $cmd -t mangle -N FAKEVPS_BLOCK 2>/dev/null || $cmd -t mangle -F FAKEVPS_BLOCK
    for port in 25 465 587 2525; do
        $cmd -t mangle -A FAKEVPS_BLOCK -p tcp --dport "$port" -j DROP
    done
    $cmd -t mangle -D FORWARD -o eth0 -j FAKEVPS_BLOCK 2>/dev/null || true
    $cmd -t mangle -D OUTPUT -o eth0 -j FAKEVPS_BLOCK 2>/dev/null || true
    $cmd -t mangle -I FORWARD -o eth0 -j FAKEVPS_BLOCK
    $cmd -t mangle -I OUTPUT -o eth0 -j FAKEVPS_BLOCK
done
iptables -t mangle -A FAKEVPS_BLOCK -d "$REAL_VPS" -j DROP
