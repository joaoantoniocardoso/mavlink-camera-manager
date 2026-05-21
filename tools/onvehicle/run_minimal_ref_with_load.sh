#!/usr/bin/env bash
# Same as run_minimal_ref.sh but adds a `tee` + `filesink` that writes to
# a tmpfs-backed file to simulate MCAP-style I/O load without MCM.
# Stop condition: if this stutters too, the regression is outside MCM
# (kernel/driver/storage) and we escalate.
#
# Usage: TOPSIDE_IP=192.168.2.1 ./run_minimal_ref_with_load.sh [seconds]

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

EXPID=${EXPID:-ref_minimal_load}
container_mkdir "$DEBUG_DIR/$EXPID"
# Use /tmp (tmpfs in BlueOS) for the filesink target so we exercise the
# memory write path - same as MCAP doing rolling buffers.
LOAD_PATH=/tmp/mcm_load_$EXPID.h264

CMD="gst-launch-1.0 -e \
  v4l2src device=$DEVICE do-timestamp=true ! \
  video/x-h264,width=$WIDTH,height=$HEIGHT,framerate=$FRAMERATE/1 ! \
  h264parse config-interval=-1 ! \
  tee name=t allow-not-linked=true \
  t. ! queue leaky=downstream max-size-buffers=60 ! rtph264pay aggregate-mode=zero-latency pt=96 ! udpsink host=$TOPSIDE_IP port=$UDP_PORT sync=false \
  t. ! queue leaky=downstream max-size-buffers=60 ! filesink location=$LOAD_PATH"

echo "$(log_marker ref_minimal_load_start): $CMD"

tmux_ctrl_c || true
sleep 2

ssh_in "(timeout $DURATION $CMD) >$DEBUG_DIR/$EXPID/gst.stdout 2>$DEBUG_DIR/$EXPID/gst.stderr; \
        echo exit_code=\$? > $DEBUG_DIR/$EXPID/gst.exit; \
        ls -la $LOAD_PATH > $DEBUG_DIR/$EXPID/load_file.ls; \
        rm -f $LOAD_PATH"

echo "$(log_marker ref_minimal_load_end): exit recorded to $DEBUG_DIR/$EXPID/gst.exit"
