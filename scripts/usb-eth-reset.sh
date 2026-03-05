#!/usr/bin/env bash
# Reset a USB ethernet adapter by cycling its authorization.
# Intended to be run via sudo from the overnight test script.
set -euo pipefail

DEVICE="${1:-2-4}"
IFACE="${2:-enp13s0u4c2}"
STATIC_IP="${3:-192.168.2.1/24}"
AUTH="/sys/bus/usb/devices/${DEVICE}/authorized"

if [ ! -f "${AUTH}" ]; then
    echo "ERROR: USB device ${DEVICE} not found (${AUTH})" >&2
    exit 1
fi

echo 0 > "${AUTH}"
sleep 2
echo 1 > "${AUTH}"

deadline=$(( $(date +%s) + 15 ))
while [ ! -d "/sys/class/net/${IFACE}" ]; do
    if [ "$(date +%s)" -ge "${deadline}" ]; then
        echo "ERROR: Interface ${IFACE} did not reappear within 15s" >&2
        exit 1
    fi
    sleep 1
done

ip link set "${IFACE}" up
ip addr add "${STATIC_IP}" dev "${IFACE}" 2>/dev/null || true
echo "USB device ${DEVICE} reset complete, ${IFACE} is up at ${STATIC_IP}."
