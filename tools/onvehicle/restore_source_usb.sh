#!/usr/bin/env bash
# Reverse swap_source_fake.sh: drop the fake source and re-add the USB
# camera stream that BlueROVUDP default-settings would create.
#
# Usage: ./restore_source_usb.sh [device_path] [endpoint_url]

set -euo pipefail
SCRIPT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=_lib.sh
source "$SCRIPT_DIR/_lib.sh"

DEVICE=${1:-/dev/video6}
ENDPOINT=${2:-udp://192.168.2.1:5600}
MCM_REST=${MCM_REST:-http://$PI_HOST:6020}
STREAM_NAME=${STREAM_NAME:-Stream /dev/video6}

ssh_host "curl -fsS -X DELETE $MCM_REST/streams 2>&1 | head -3 || true"

PAYLOAD=$(cat <<EOF
{
  "name": "$STREAM_NAME",
  "source": {"type": "Local", "device_path": "$DEVICE"},
  "stream": {
    "encode": "H264",
    "height": 1080, "width": 1920,
    "interval": {"numerator": 1, "denominator": 30},
    "endpoints": ["$ENDPOINT"]
  }
}
EOF
)

ssh_host "curl -fsS -X POST -H 'Content-Type: application/json' \
          -d $(printf %q "$PAYLOAD") $MCM_REST/streams 2>&1 | head -20"

echo "restore_source_usb: posted USB camera ($DEVICE) -> $ENDPOINT"
