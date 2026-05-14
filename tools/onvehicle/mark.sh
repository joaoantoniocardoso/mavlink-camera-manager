#!/usr/bin/env bash
# Write a timestamped marker into every active log under
# /var/log/mcm-debug/<expid>/. Use it to delimit "before swap_source",
# "after swap_source", "client started recording", etc., so post-session
# diffs can align tmux/profile logs with the Cockpit timeline.
#
# Usage: ./mark.sh <experiment_id> <label>

set -euo pipefail
SCRIPT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=_lib.sh
source "$SCRIPT_DIR/_lib.sh"

EXPID=${1:?missing EXPID}
LABEL=${2:?missing LABEL}
STAMP=$(date -u +%Y%m%dT%H%M%S.%3NZ)

ssh_in "echo '[$STAMP] MARK $LABEL' | tee -a $DEBUG_DIR/$EXPID/marks.txt"
# Also signal MCM via SIGUSR1 if we want a tracing flush in future versions.
# Currently MCM ignores SIGUSR1, but the signal is harmless.
ssh_in "pkill -USR1 -f mavlink-camera-manager 2>/dev/null || true"

echo "$STAMP $LABEL"
