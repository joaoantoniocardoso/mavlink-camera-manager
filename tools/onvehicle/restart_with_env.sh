#!/usr/bin/env bash
# Restart the already-deployed MCM binary in tmux session `video` with a
# prefix of env vars.  Use this for fast A/B experiments without
# cross-building or `docker cp`-ing a new binary.
#
# Usage:
#   tools/onvehicle/restart_with_env.sh <label> [ENV1=v1 ENV2=v2 ...]
#
# Example:
#   tools/onvehicle/restart_with_env.sh thumb_off MCM_DISABLE_THUMBNAIL=1
#
# The MCM command line mirrors cross_build_and_run.sh so behaviour is
# identical except for the env prefix.  Logs go where MCM was already
# writing them (`/var/logs/blueos/services/mavlink-camera-manager/`).

set -euo pipefail

REMOTE_USER=${REMOTE_USER:-"pi"}
REMOTE_HOST=${REMOTE_HOST:-"10.147.20.70"}
REMOTE_PASS=${REMOTE_PASS:-"raspberry"}
CONTAINER=${CONTAINER:-"blueos-core"}
BINARY_NAME=${BINARY_NAME:-"mavlink-camera-manager"}
TMUX_SESSION=${TMUX_SESSION:-"video"}
LOG_DIR=${LOG_DIR:-"/var/logs/blueos/services/mavlink-camera-manager"}
LOCAL_OUT=${LOCAL_OUT:-"/tmp/mcm_session"}

if [[ $# -lt 1 ]]; then
    echo "usage: $0 <label> [ENV1=v1 ENV2=v2 ...]" >&2
    exit 1
fi

LABEL=$1
shift
ENV_PREFIX="$*"

MCM_CMD="${ENV_PREFIX} GST_DEBUG=3 /root/${BINARY_NAME} \
--default-settings BlueROVUDP \
--mavlink tcpout:127.0.0.1:5777 \
--mavlink-system-id 1 \
--mavlink-camera-component-id-range=100-105 \
--gst-feature-rank omxh264enc=0,v4l2h264enc=250,x264enc=260 \
--log-path ${LOG_DIR} \
--stun-server stun://stun.l.google.com:19302 \
--enable-realtime-threads \
--verbose"

SSH="sshpass -p ${REMOTE_PASS} ssh -o StrictHostKeyChecking=no ${REMOTE_USER}@${REMOTE_HOST}"

echo "[+] Stopping current MCM in tmux session '${TMUX_SESSION}'..."
$SSH "docker exec ${CONTAINER} tmux send-keys -t ${TMUX_SESSION} C-c"
sleep 1

echo "[+] Marking restart in log..."
$SSH "docker exec ${CONTAINER} bash -lc 'echo \"=== MCM RESTART label=${LABEL} env=\\\"${ENV_PREFIX}\\\" ts=\$(date -Iseconds) ===\" >> ${LOG_DIR}/restart_marks.log'"

echo "[+] Restarting MCM with env prefix: ${ENV_PREFIX}"
$SSH "docker exec ${CONTAINER} tmux send-keys -t ${TMUX_SESSION} '${MCM_CMD}' Enter"

echo "[+] Done.  Label='${LABEL}'.  Give it ~5 s to come up, then check the stream visually."
echo
echo "    To pull logs after the experiment:"
echo "      mkdir -p ${LOCAL_OUT}/${LABEL}"
echo "      sshpass -p ${REMOTE_PASS} rsync -az ${REMOTE_USER}@${REMOTE_HOST}:${LOG_DIR}/ ${LOCAL_OUT}/${LABEL}/"
