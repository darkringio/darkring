#!/usr/bin/env bash
# The makers' prover (started through the Zolana CLI) listens on every
# interface: 3001 for proofs, 9998 for metrics. Nothing outside this machine
# has a reason to reach either, and a stranger could use the first to spend
# this machine's CPU. This drops whatever does not come over loopback.
#
# Run as root. The desk's systemd unit runs it before each start, so the
# rules are back after a reboot. Safe to run twice.
set -euo pipefail

for tables in iptables ip6tables; do
  command -v "$tables" >/dev/null || continue
  rule=(INPUT ! -i lo -p tcp -m multiport --dports 3001,9998 -j DROP)
  "$tables" -C "${rule[@]}" 2>/dev/null || "$tables" -I "${rule[@]}"
done
