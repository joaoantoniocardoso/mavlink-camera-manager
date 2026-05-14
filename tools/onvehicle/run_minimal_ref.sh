#!/usr/bin/env bash
# Bare gst-launch reference pipeline. Runs *inside* the BlueOS container,
# completely outside MCM. Use to gate "is the symptom MCM-specific or
# kernel/driver-level?"
#
# Usage: TOPSIDE_IP=192.168.2.1 ./run_minimal_ref.sh [seconds] [device] [resolution] [framerate]
#
# Default: 60 s, /dev/video6, 1920x1080@30.

set -euo pipefail
SCRIPT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=_lib.sh
source "$SCRIPT_DIR/_lib.sh"

DURATION=${1:-60}
DEVICE=${2:-/dev/video6}
RESOLUTION=${3:-1920x1080}
FRAMERATE=${4:-30}
TOPSIDE_IP=${TOPSIDE_IP:-192.168.2.1}
UDP_PORT=${UDP_PORT:-6000}

WIDTH=${RESOLUTION%x*}
HEIGHT=${RESOLUTION#*x}

CMD="gst-launch-1.0 -e \
  v4l2src device=$DEVICE do-timestamp=true ! \
  video/x-h264,width=$WIDTH,height=$HEIGHT,framerate=$FRAMERATE/1 ! \
  h264parse config-interval=-1 ! \
  rtph264pay aggregate-mode=zero-latency config-interval=-1 pt=96 ! \
  udpsink host=$TOPSIDE_IP port=$UDP_PORT sync=false"

echo "$(log_marker ref_minimal_start): $CMD"

# First make sure MCM is not holding the camera.
tmux_ctrl_c || true
sleep 2

# Run with a watchdog timeout. Capture both stdout and stderr inside the
# container for later rsync.
EXPID=${EXPID:-ref_minimal}
container_mkdir "$DEBUG_DIR/$EXPID"
ssh_in "(timeout $DURATION $CMD) >$DEBUG_DIR/$EXPID/gst.stdout 2>$DEBUG_DIR/$EXPID/gst.stderr; \
        echo exit_code=\$? > $DEBUG_DIR/$EXPID/gst.exit"

echo "$(log_marker ref_minimal_end): exit recorded to $DEBUG_DIR/$EXPID/gst.exit"
