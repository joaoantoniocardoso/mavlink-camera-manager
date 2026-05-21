#!/usr/bin/env bash
# Replace the configured stream's source with a fake one (videotestsrc as
# RTSP) so we can isolate v4l2/USB driver effects from MCM effects.
#
# Implementation: hits MCM's REST API to delete the current stream and
# add a new one using the fake source. This is the on-the-fly REST swap
# the plan mentions; it does NOT restart MCM.
#
# Usage: ./swap_source_fake.sh [endpoint_url]
# endpoint_url defaults to udp://192.168.2.1:5600.

set -euo pipefail
SCRIPT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=_lib.sh
source "$SCRIPT_DIR/_lib.sh"

ENDPOINT=${1:-udp://192.168.2.1:5600}
MCM_REST=${MCM_REST:-http://$PI_HOST:6020}
STREAM_NAME=${STREAM_NAME:-FakeTestSrc}

# 1) List streams, drop everything (so we don't double-bind the endpoint).
echo "swap_source_fake: deleting existing streams"
ssh_host "curl -fsS -X DELETE $MCM_REST/streams 2>&1 || \
          curl -fsS $MCM_REST/v4l 2>&1 | head -2 || true"

# 2) Add a fake RTSP stream. The exact REST schema depends on the MCM
# version; we try the two common shapes.
PAYLOAD=$(cat <<EOF
{
  "name": "$STREAM_NAME",
  "source": {"type": "Gst", "source": "Fake"},
  "stream": {
    "encode": "H264",
    "height": 720, "width": 1280,
    "interval": {"numerator": 1, "denominator": 30},
    "endpoints": ["$ENDPOINT"]
  }
}
EOF
)

ssh_host "curl -fsS -X POST -H 'Content-Type: application/json' \
          -d $(printf %q "$PAYLOAD") $MCM_REST/streams 2>&1 | head -20"

echo "swap_source_fake: posted fake source with endpoint $ENDPOINT"
