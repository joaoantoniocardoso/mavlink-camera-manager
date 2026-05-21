#!/usr/bin/env bash
# Remove every tc qdisc added by impair_webrtc.sh on the pi.
# Idempotent. Does not touch MCM_WEBRTC_PORT_* env vars (those go
# away on the next plain restart_with_env.sh call without them).

set -euo pipefail

REMOTE_USER=${REMOTE_USER:-"pi"}
REMOTE_HOST=${REMOTE_HOST:-"192.168.2.2"}
REMOTE_PASS=${REMOTE_PASS:-"raspberry"}
IFACE=${IFACE:-eth0}

SSH="sshpass -p $REMOTE_PASS ssh -o StrictHostKeyChecking=no -o LogLevel=ERROR $REMOTE_USER@$REMOTE_HOST"

TC=/usr/sbin/tc
echo "[+] Removing root qdisc on $REMOTE_HOST:$IFACE..."
$SSH "sudo $TC qdisc del dev $IFACE root 2>/dev/null || true"

echo "[+] State after teardown:"
$SSH "sudo $TC qdisc show dev $IFACE"
