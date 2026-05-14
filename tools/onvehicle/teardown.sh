#!/usr/bin/env bash
# End an experiment: stop tmux MCM, kill profilers, tar the experiment
# directory, rsync to the laptop.
#
# Usage: ./teardown.sh <experiment_id>

set -euo pipefail
SCRIPT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=_lib.sh
source "$SCRIPT_DIR/_lib.sh"

EXPID=${1:?missing EXPID}
EXPDIR=$DEBUG_DIR/$EXPID
mkdir -p "$LOCAL_LOG_DIR"

echo "$(log_marker teardown_${EXPID})"

# 1) stop MCM. We do this first because some profilers (top -p PID) exit
# when PID dies, which lets `timeout` finalise gracefully.
tmux_ctrl_c || true
sleep 2

# 2) kill any straggler profilers tied to the experiment.
ssh_host "(pkill -f 'dstat .*$EXPID' || true)"
ssh_host "(pkill -f 'iostat .*$EXPID' || true)"

# 3) capture the final tmux pane.
ssh_host "docker exec $CONTAINER tmux capture-pane -p -t $TMUX_SESSION > /tmp/$EXPID.tmux.txt 2>&1 || true"
ssh_host "docker cp /tmp/$EXPID.tmux.txt $CONTAINER:$EXPDIR/post_tmux.txt 2>/dev/null && rm /tmp/$EXPID.tmux.txt"

# 4) tar the in-container directory.
ssh_in "tar -C $DEBUG_DIR -czf /tmp/$EXPID.tgz $EXPID 2>&1 || true"

# 5) docker cp to host, then scp to laptop.
ssh_host "docker cp $CONTAINER:/tmp/$EXPID.tgz /tmp/ 2>&1 || true"
"${SCP_BASE[@]}" "$PI_USER@$PI_HOST:/tmp/$EXPID.tgz" "$LOCAL_LOG_DIR/"
ssh_host "rm -f /tmp/$EXPID.tgz"
ssh_in "rm -f /tmp/$EXPID.tgz || true"

echo "$(log_marker teardown_done_${EXPID}): archive at $LOCAL_LOG_DIR/$EXPID.tgz"
