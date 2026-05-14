#!/usr/bin/env bash
# Swap the MCM running in tmux:video for the specified binary, with the
# specified env prefix, and fire profile_for.sh in the background. Each
# experiment writes into /var/log/mcm-debug/<expid>/ inside the container
# so teardown.sh can tar it as a unit.
#
# Usage: ./bringup.sh <binary_basename> "<env_string>" <experiment_id> [duration_seconds]
#
# Examples:
#   ./bringup.sh mavlink-camera-manager.debug "" e1_B0_debug 900
#   ./bringup.sh mavlink-camera-manager.debug "MCM_QUEUE_PER_SINK_BRANCH=1" e4_q_targeted 900
#   ./bringup.sh mavlink-camera-manager.debug \
#                "GST_DEBUG=GST_TRACER:7 GST_TRACERS=latency;queue;stats \
#                 GST_DEBUG_DUMP_DOT_DIR=/var/log/mcm-debug/e1_dots" e1_B0_debug 900

set -euo pipefail
SCRIPT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=_lib.sh
source "$SCRIPT_DIR/_lib.sh"

BINARY=${1:?missing BINARY (e.g. mavlink-camera-manager.debug)}
ENV_STR=${2:-}
EXPID=${3:?missing EXPID}
DURATION=${4:-900}

# Standard MCM CLI used by BlueOS' /root/.bashrc on the production image.
# Kept consistent with cross_build_and_run.sh so we don't drift from the
# stock configuration.
MCM_ARGS=${MCM_ARGS:-"--default-settings BlueROVUDP \
  --mavlink tcpout:127.0.0.1:5777 --mavlink-system-id 1 \
  --mavlink-camera-component-id-range=100-105 \
  --gst-feature-rank omxh264enc=0,v4l2h264enc=250,x264enc=260 \
  --stun-server stun://stun.l.google.com:19302 \
  --enable-realtime-threads --verbose"}

EXPDIR=$DEBUG_DIR/$EXPID
container_mkdir "$EXPDIR"

echo "$(log_marker bringup_${EXPID}): binary=$BINARY env=$ENV_STR duration=${DURATION}s"

# 1) stop whatever is running.
tmux_ctrl_c || true
sleep 2

# 2) snapshot the tmux pane (so we can prove the previous MCM exited).
ssh_host "docker exec $CONTAINER tmux capture-pane -p -t $TMUX_SESSION > /tmp/$EXPID.pre_tmux.txt 2>&1 || true"
ssh_host "docker cp /tmp/$EXPID.pre_tmux.txt $CONTAINER:$EXPDIR/pre_tmux.txt 2>/dev/null && rm /tmp/$EXPID.pre_tmux.txt"

# 3) send the new command line. Note: the env prefix must be inline so
# that MCM_DEBUG/GST_DEBUG are visible to MCM, not just to the tmux client.
CMDLINE="$ENV_STR /root/$BINARY $MCM_ARGS --log-path $EXPDIR"
tmux_send "$CMDLINE"

# 4) start profilers. profile_for.sh runs everything in the background
# and writes into $EXPDIR.
"$SCRIPT_DIR/profile_for.sh" "$DURATION" "$EXPID"

# 5) record the bringup command line for later forensics.
ssh_host "docker exec $CONTAINER sh -c 'echo $(printf %q "$CMDLINE") > $EXPDIR/bringup_cmdline.txt'"

echo "$(log_marker bringup_done_${EXPID}): pid via tmux, profilers running"
