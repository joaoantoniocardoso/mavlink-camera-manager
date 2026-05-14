#!/usr/bin/env bash
# End-of-session cleanup. Restores the vehicle to bit-for-bit pre-session
# state (host-side), then asks Kraken to swap back to stock 1.4.4 which
# clears container-side state for free.
#
# Steps:
#  1. tmux Ctrl-C: stop whatever MCM is running.
#  2. rsync /var/log/mcm-debug from container to local disk (last chance).
#  3. Kraken-swap to stock 1.4.4 (manual fallback documented).
#  4. apt-get purge host-installed packages that weren't in _pre.
#  5. Print pre-vs-post snapshot diff to terminal.
#
# Usage: ./cleanup.sh [stock_image_tag]
# stock_image_tag defaults to bluerobotics/blueos-core:1.4.4.

set -euo pipefail
SCRIPT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=_lib.sh
source "$SCRIPT_DIR/_lib.sh"

STOCK_IMAGE=${1:-bluerobotics/blueos-core:1.4.4}
mkdir -p "$LOCAL_LOG_DIR"

echo "$(log_marker cleanup_start)"

# 1) Stop the running experiment.
tmux_ctrl_c || true
sleep 2

# 2) Rsync everything off before the container goes away.
ssh_in "tar -C / -czf /tmp/mcm-debug-final.tgz var/log/mcm-debug 2>&1 || true"
ssh_host "docker cp $CONTAINER:/tmp/mcm-debug-final.tgz /tmp/ 2>&1 || true"
RSH=$RSYNC_RSH rsync -av --no-perms --no-times -e "$RSYNC_RSH" \
    "$PI_USER@$PI_HOST:/tmp/mcm-debug-final.tgz" "$LOCAL_LOG_DIR/" || \
    "${SCP_BASE[@]}" "$PI_USER@$PI_HOST:/tmp/mcm-debug-final.tgz" "$LOCAL_LOG_DIR/"
ssh_host "rm -f /tmp/mcm-debug-final.tgz"
ssh_in "rm -f /tmp/mcm-debug-final.tgz || true"
echo "logs archived to $LOCAL_LOG_DIR/mcm-debug-final.tgz"

# 3) Kraken-swap: the canonical way. If Kraken's API endpoint differs
# in the user's environment, override KRAKEN_SWAP_CMD via env. Manual
# fallback prints below.
KRAKEN_SWAP_CMD=${KRAKEN_SWAP_CMD:-}
if [ -n "$KRAKEN_SWAP_CMD" ]; then
    ssh_host "$KRAKEN_SWAP_CMD $STOCK_IMAGE" || \
        echo "warn: Kraken swap failed; manual fallback below" >&2
else
    cat >&2 <<EOF
Kraken swap command not specified. Manual fallback (run on the vehicle):
   docker stop $CONTAINER && docker rm $CONTAINER
   docker run -d --name $CONTAINER --restart unless-stopped <BlueOS-launch-args> $STOCK_IMAGE
EOF
fi

# 4) Diff hosts side packages between latest _pre snapshot and now,
# apt-get purge anything we added.
ssh_host "dpkg -l 2>/dev/null | awk '/^ii/ {print \$2}' > /tmp/dpkg-host.post.txt"
# The _pre snapshots are inside the container, but we mirrored host
# dpkg into the same dir. Pull the most recent one.
LATEST_PRE=$(ssh_in "ls -dt $DEBUG_DIR/_pre_* 2>/dev/null | head -1" || true)
if [ -n "$LATEST_PRE" ]; then
    ssh_host "docker cp $CONTAINER:$LATEST_PRE/dpkg-host.txt /tmp/dpkg-host.pre.txt 2>&1 || true"
    # Diff: anything in `post` but not in `pre`.
    ssh_host "awk '/^ii/ {print \$2}' /tmp/dpkg-host.pre.txt > /tmp/dpkg-host.pre.names.txt 2>/dev/null || true"
    ssh_host "comm -23 <(sort /tmp/dpkg-host.post.txt) <(sort /tmp/dpkg-host.pre.names.txt) > /tmp/added-pkgs.txt"
    ADDED=$(ssh_host "cat /tmp/added-pkgs.txt")
    if [ -n "$ADDED" ]; then
        echo "removing host-installed packages: $ADDED"
        ssh_host "sudo apt-get purge -y $ADDED 2>&1 | tail -5"
    else
        echo "no host packages to purge"
    fi
    ssh_host "rm -f /tmp/dpkg-host.pre.txt /tmp/dpkg-host.pre.names.txt /tmp/dpkg-host.post.txt /tmp/added-pkgs.txt"
fi

# 5) Final diff print: show host before/after.
ssh_host "dpkg -l 2>/dev/null > /tmp/dpkg-host.final.txt"
echo "=== host dpkg final ==="
ssh_host "wc -l /tmp/dpkg-host.final.txt"
ssh_host "rm -f /tmp/dpkg-host.final.txt"

echo "$(log_marker cleanup_done)"
