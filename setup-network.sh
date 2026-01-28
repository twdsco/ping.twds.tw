#!/bin/bash
# Setup script for MTR Matrix network configuration
# Run as root: sudo ./setup-network.sh

set -e

IFACE="ens19"
GATEWAY="44.30.18.254"
SUBNET="44.30.18"
TABLE=100
MARK=100

# Add IPs to interface
for i in $(seq 1 12); do
    ip addr add ${SUBNET}.$i/24 dev $IFACE 2>/dev/null || true
done

# Setup routing table
ip route add default via $GATEWAY dev $IFACE table $TABLE 2>/dev/null || \
    ip route replace default via $GATEWAY dev $IFACE table $TABLE

# Mark packets from our subnet
iptables -t mangle -C OUTPUT -s ${SUBNET}.0/24 -j MARK --set-mark $MARK 2>/dev/null || \
    iptables -t mangle -A OUTPUT -s ${SUBNET}.0/24 -j MARK --set-mark $MARK

# Route marked packets via our table
ip rule add fwmark $MARK lookup $TABLE prio 50 2>/dev/null || true

echo "Network setup complete"
echo "  Interface: $IFACE"
echo "  IPs: ${SUBNET}.1-12/24"
echo "  Gateway: $GATEWAY"
