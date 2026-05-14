#!/usr/bin/env bash
# Shared helpers sourced by every script in tools/onvehicle/.
# Not directly executable.

set -euo pipefail

PI_HOST=${PI_HOST:-192.168.2.2}
PI_USER=${PI_USER:-pi}
PI_PASS=${PI_PASS:-raspberry}
CONTAINER=${CONTAINER:-blueos-core}
TMUX_SESSION=${TMUX_SESSION:-video}
DEBUG_DIR=${DEBUG_DIR:-/var/log/mcm-debug}
TARGET=${TARGET:-armv7-unknown-linux-gnueabihf}
LOCAL_LOG_DIR=${LOCAL_LOG_DIR:-./onvehicle-logs}

if command -v sshpass >/dev/null 2>&1; then
    SSH_BASE=(sshpass -p "$PI_PASS" ssh -o StrictHostKeyChecking=no
              -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR
              "$PI_USER@$PI_HOST")
    SCP_BASE=(sshpass -p "$PI_PASS" scp -o StrictHostKeyChecking=no
              -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR)
    RSYNC_RSH="sshpass -p $PI_PASS ssh -o StrictHostKeyChecking=no \
               -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR"
else
    echo "warn: sshpass not found, falling back to plain ssh (will prompt for password)" >&2
    SSH_BASE=(ssh -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null
              -o LogLevel=ERROR "$PI_USER@$PI_HOST")
    SCP_BASE=(scp -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null
              -o LogLevel=ERROR)
    RSYNC_RSH="ssh -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null \
               -o LogLevel=ERROR"
fi

ssh_host() {
    "${SSH_BASE[@]}" "$@"
}

ssh_in() {
    # Run a shell command inside blueos-core.
    "${SSH_BASE[@]}" "docker exec $CONTAINER sh -c $(printf %q "$*")"
}

scp_to_host() {
    "${SCP_BASE[@]}" "$1" "$PI_USER@$PI_HOST:$2"
}

# Send a command line to the `video` tmux session inside the container.
# Splits on spaces are preserved via printf %q -- callers can pass a single
# pre-quoted command line.
tmux_send() {
    "${SSH_BASE[@]}" "docker exec $CONTAINER tmux send-keys -t $TMUX_SESSION $(printf %q "$1") Enter"
}

tmux_ctrl_c() {
    "${SSH_BASE[@]}" "docker exec $CONTAINER tmux send-keys -t $TMUX_SESSION C-c"
}

tmux_capture() {
    "${SSH_BASE[@]}" "docker exec $CONTAINER tmux capture-pane -p -t $TMUX_SESSION"
}

container_mkdir() {
    "${SSH_BASE[@]}" "docker exec $CONTAINER mkdir -p $1"
}

log_marker() {
    local label=$1
    local stamp
    stamp=$(date -u +%Y%m%dT%H%M%SZ)
    echo "[$stamp] MARK $label"
}
