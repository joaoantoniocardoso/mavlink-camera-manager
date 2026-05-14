#!/usr/bin/env bash
# Idempotent installer. Run once after every BlueOS image swap (Kraken
# replaces blueos-core, so container-internal state needs to be reseeded).
#
# Steps:
#  1. Snapshot host + container state into $DEBUG_DIR/_pre_<phase>/ for
#     diffing in cleanup.sh.
#  2. apt-get install host-side observability tools (idempotent: skips
#     anything already present).
#  3. docker cp the two MCM binaries (mcm-debug-armv7, mcm-t3.19.2-armv7)
#     into the container.
#  4. apt-get install gstreamer1.0-tools in-container (for minimal_ref).
#
# Usage: ./install.sh [phase_label]
# phase_label defaults to a timestamp, but pass e.g. "phaseA_1.4.3" so
# pre/post diffs are anchored cleanly.

set -euo pipefail
SCRIPT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=_lib.sh
source "$SCRIPT_DIR/_lib.sh"

PHASE=${1:-$(date -u +pre_%Y%m%dT%H%M%SZ)}
PRE=$DEBUG_DIR/_pre_$PHASE

echo "$(log_marker install_$PHASE): snapshotting and installing"

container_mkdir "$PRE"

# Container-side snapshot (resets every image swap; cleanup compares against
# the most recent _pre_*).
ssh_in "dpkg -l > $PRE/dpkg-container.txt 2>&1 || true"
ssh_in "ls -la /root/mavlink-camera* 2>/dev/null > $PRE/mcm-binaries.txt || true"
ssh_host "docker exec $CONTAINER tmux capture-pane -p -t $TMUX_SESSION > /tmp/tmux-video.txt 2>&1 || true"
ssh_host "docker cp /tmp/tmux-video.txt $CONTAINER:$PRE/tmux-video.txt && rm /tmp/tmux-video.txt"
ssh_in "uname -a > $PRE/uname-container.txt 2>&1 || true"
ssh_in "(gst-launch-1.0 --version 2>&1 || echo missing) > $PRE/gst-version.txt"

# Host-side snapshot
ssh_host "dpkg -l > /tmp/dpkg-host.txt 2>&1 || true"
ssh_host "systemctl list-units --type=service --no-pager > /tmp/systemctl-host.txt 2>&1 || true"
ssh_host "uname -a > /tmp/uname-host.txt 2>&1 || true"

# Mirror the host snapshot into the container so it gets tarred up with
# everything else by teardown.sh.
ssh_host "docker cp /tmp/dpkg-host.txt    $CONTAINER:$PRE/dpkg-host.txt"
ssh_host "docker cp /tmp/systemctl-host.txt $CONTAINER:$PRE/systemctl-host.txt"
ssh_host "docker cp /tmp/uname-host.txt   $CONTAINER:$PRE/uname-host.txt"
ssh_host "rm /tmp/dpkg-host.txt /tmp/systemctl-host.txt /tmp/uname-host.txt"

# Host: install observability tools. apt-get is idempotent (skips packages
# already present). We don't `update` aggressively to keep the cleanup
# diff tight.
HOST_PKGS="dstat iotop linux-perf tcpdump rsync"
ssh_host "sudo apt-get install -y --no-install-recommends $HOST_PKGS 2>&1 | tail -10 || \
          (sudo apt-get update && sudo apt-get install -y --no-install-recommends $HOST_PKGS 2>&1 | tail -10)"

# Container: gst-launch + gst-inspect (for ref pipelines and DOT introspection).
ssh_in "command -v gst-launch-1.0 >/dev/null || \
        (apt-get update && apt-get install -y --no-install-recommends gstreamer1.0-tools 2>&1 | tail -5)"

# Push the MCM binaries from the laptop into the container.
BIN_DIR=$SCRIPT_DIR/bin
DEBUG_BIN=$BIN_DIR/mcm-debug-armv7
LEGACY_BIN=$BIN_DIR/mcm-t3.19.2-armv7

if [ ! -x "$DEBUG_BIN" ]; then
    echo "fatal: $DEBUG_BIN not found - run cross build first" >&2
    exit 1
fi

scp_to_host "$DEBUG_BIN" "/tmp/mcm-debug-armv7"
ssh_host "docker cp /tmp/mcm-debug-armv7 $CONTAINER:/root/mavlink-camera-manager.debug \
          && docker exec $CONTAINER chmod +x /root/mavlink-camera-manager.debug \
          && rm /tmp/mcm-debug-armv7"

if [ -x "$LEGACY_BIN" ]; then
    scp_to_host "$LEGACY_BIN" "/tmp/mcm-t3.19.2-armv7"
    ssh_host "docker cp /tmp/mcm-t3.19.2-armv7 $CONTAINER:/root/mavlink-camera-manager.t3.19.2 \
              && docker exec $CONTAINER chmod +x /root/mavlink-camera-manager.t3.19.2 \
              && rm /tmp/mcm-t3.19.2-armv7"
else
    echo "warn: $LEGACY_BIN missing - extract via extract_t3.19.2_binary.sh before Phase B step 3a" >&2
fi

# Record what binaries are now in place.
ssh_in "ls -la /root/mavlink-camera* > $PRE/mcm-binaries-post-install.txt 2>&1 || true"

echo "$(log_marker install_done_$PHASE): pre snapshot at $PRE"
