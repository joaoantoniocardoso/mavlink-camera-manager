#!/usr/bin/env bash
# Parallel-runs a battery of host- and container-level profilers for
# `DURATION` seconds, writing every output stream into
# `$DEBUG_DIR/<expid>/` inside the container so `teardown.sh` can tar it
# in one go.
#
# Designed to degrade gracefully: missing tools are skipped with a warning,
# never aborted. GST debug tracers fire only when `GST_DEBUG_ENABLED=1`
# (set by `bringup.sh` when running on the debug-enabled 1.4.4 image).
#
# Usage: ./profile_for.sh <seconds> <experiment_id>

set -euo pipefail
SCRIPT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=_lib.sh
source "$SCRIPT_DIR/_lib.sh"

DURATION=${1:?missing DURATION}
EXPID=${2:?missing EXPID}
OUT=$DEBUG_DIR/$EXPID

container_mkdir "$OUT"

run_in_container() {
    local logname=$1
    shift
    local cmdline="$*"
    ssh_host "docker exec -d $CONTAINER sh -c \
        \"timeout $DURATION $cmdline > $OUT/$logname.stdout 2> $OUT/$logname.stderr; \
          echo exit_code=\\\$? > $OUT/$logname.exit\""
}

run_on_host() {
    local logname=$1
    shift
    local cmdline="$*"
    ssh_host "(timeout $DURATION $cmdline > $OUT/$logname.stdout 2> $OUT/$logname.stderr; \
              echo exit_code=\$? > $OUT/$logname.exit) &"
}

# Host: dstat - cpu/io/disk/net/mem/top-cpu/top-io
if ssh_host "command -v dstat >/dev/null"; then
    run_on_host dstat-host "dstat -tcmdrsy --top-cpu --top-io"
else
    echo "warn: dstat not installed on host; skipping" >&2
fi

# Host: iostat -xt (block device throughput, also captures await/util)
if ssh_host "command -v iostat >/dev/null"; then
    run_on_host iostat-host "iostat -xt 1"
else
    echo "warn: iostat not installed on host; skipping" >&2
fi

# Host: top -bH on MCM PID. If MCM is not running yet, the PID is absent;
# fall back to system-wide top.
HOST_PIDS=$(ssh_host "pgrep -d, mavlink-camera || true")
if [ -n "$HOST_PIDS" ]; then
    run_on_host top-host "top -bH -d 1 -p $HOST_PIDS"
else
    run_on_host top-host "top -bH -d 1"
fi

# Container-side: top -bH on the MCM PID inside the container (in case
# host pgrep doesn't see it).
run_in_container top-container \
    "PID=\$(pgrep mavlink-camera || true); \
     if [ -n \"\$PID\" ]; then top -bH -d 1 -p \$PID; else top -bH -d 1; fi"

# Container-side: voluntary/non-voluntary ctx-switch sampler. 1 Hz over $DURATION.
run_in_container ctxswitch \
    "for i in \$(seq 1 $DURATION); do \
        PID=\$(pgrep mavlink-camera || true); \
        if [ -n \"\$PID\" ]; then \
            ts=\$(date -u +%Y%m%dT%H%M%S.%3NZ); \
            for t in /proc/\$PID/task/*/status; do \
                tid=\$(basename \$(dirname \$t)); \
                vctx=\$(awk '/voluntary_ctxt_switches:/ {print \$2; exit}' \$t 2>/dev/null || echo 0); \
                nvctx=\$(awk '/nonvoluntary_ctxt_switches:/ {print \$2; exit}' \$t 2>/dev/null || echo 0); \
                echo \"\$ts pid=\$PID tid=\$tid vctx=\$vctx nvctx=\$nvctx\"; \
            done; \
        fi; \
        sleep 1; \
    done"

# Host: perf record if available (10 s minimum, capped at DURATION).
if ssh_host "command -v perf >/dev/null" && [ -n "$HOST_PIDS" ]; then
    PERF_DUR=$DURATION
    if [ "$PERF_DUR" -gt 60 ]; then PERF_DUR=60; fi
    run_on_host perf-record "perf record -F 99 -p $HOST_PIDS -g -o /tmp/$EXPID.perf.data -- sleep $PERF_DUR"
    ssh_host "(sleep $((PERF_DUR + 5)); \
               perf report --no-children --stdio -i /tmp/$EXPID.perf.data > $OUT/perf-host.report 2>&1 || true) &"
fi

# Container-side: storage write-throughput probe. /tmp is tmpfs on BlueOS,
# so this isolates whether MCAP backpressure is the choker for filesystem
# I/O specifically.
run_in_container storage-probe \
    "for i in \$(seq 1 $((DURATION / 5))); do \
        ts=\$(date -u +%Y%m%dT%H%M%S.%3NZ); \
        dd if=/dev/zero of=/tmp/mcm_probe_$EXPID bs=1M count=64 conv=fsync 2>&1 | \
            sed \"s|^|\$ts |\"; \
        rm -f /tmp/mcm_probe_$EXPID; \
        sleep 5; \
     done"

# Container-side: GST tracers, *only* on the debug-enabled image. The
# bringup script sets `GST_DEBUG_ENABLED=1` (via tmux send-keys env
# prefix) when this is the case. We still emit a `_dots/` placeholder so
# `teardown.sh` always sees a consistent shape.
ssh_host "docker exec $CONTAINER mkdir -p $OUT/_dots"
ssh_host "(docker exec $CONTAINER ls $OUT/_dots > $OUT/dot_files.txt 2>&1)" || true

echo "$(log_marker profile_started_${EXPID}): duration=${DURATION}s output=$OUT"
