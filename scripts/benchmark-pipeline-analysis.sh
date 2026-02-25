#!/usr/bin/env bash
set -euo pipefail

# ── Pi-side /proc sampler ──
#
# Runs ON the Raspberry Pi (inside Docker container). Starts MCM, collects
# /proc-based CPU and RSS metrics, and reports results. Does NOT run any
# polling clients -- those run on the host machine to avoid CPU contention.
#
# PREREQUISITES (set up by the host-side benchmark-host.sh):
#   - CPU governor pinned to 'performance' at 1500 MHz on all cores
#   - This container pinned to CPUs 1,2,3 (core 0 reserved for OS)
#   - All OS processes and IRQs moved to CPU 0
#
# POLL_MODE is passed through only for labeling results.
# No API calls are made from this script.

# ── Configuration ──
WARMUP_SECS=30
SAMPLE_SECS=60
COOLDOWN_SECS=15
LEVELS="${1:-off lite full}"
MCM_BIN="/root/mavlink-camera-manager"
MCM_COMMON_ARGS="--default-settings BlueROVUDP \
  --mavlink udpin:127.0.0.1:5777 \
  --mavlink-system-id 1 \
  --mavlink-camera-component-id-range=100-105 \
  --gst-feature-rank omxh264enc=0,v4l2h264enc=250,x264enc=260 \
  --log-path /var/logs/blueos/services/mavlink-camera-manager \
  --stun-server stun://stun.l.google.com:19302 \
  --verbose"

POLL_MODE="${POLL_MODE:-none}"
VARIANT_LABEL="${VARIANT_LABEL:-default}"

RESULTS_DIR="/tmp/benchmark_results_${VARIANT_LABEL}"
CLK_TCK=$(getconf CLK_TCK)

mkdir -p "$RESULTS_DIR"

# ── Helpers ──

get_pid() {
    pidof mavlink-camera-manager 2>/dev/null | awk '{print $1}' || true
}

read_proc_cpu() {
    local pid=$1
    local stat
    stat=$(cat "/proc/$pid/stat" 2>/dev/null) || { echo "0 0"; return; }
    local utime stime
    utime=$(echo "$stat" | awk '{print $14}')
    stime=$(echo "$stat" | awk '{print $15}')
    echo "$((utime + stime))"
}

read_proc_rss_kb() {
    local pid=$1
    awk '/^VmRSS:/ {print $2}' "/proc/$pid/status" 2>/dev/null || echo "0"
}

read_loadavg() {
    awk '{print $1}' /proc/loadavg
}

mean_of_file() {
    awk '{ sum += $1; n++ } END { if (n>0) printf "%.2f", sum/n; else print "0" }' "$1"
}

stddev_of_file() {
    awk '{ sum += $1; sumsq += $1*$1; n++ } END {
        if (n>1) { m=sum/n; printf "%.2f", sqrt((sumsq/n) - m*m) } else print "0"
    }' "$1"
}

# ── Main benchmark loop ──

echo "============================================="
echo "  MCM Pipeline Analysis A/B Benchmark"
echo "============================================="
echo "Warmup: ${WARMUP_SECS}s | Sampling: ${SAMPLE_SECS}s | Cooldown: ${COOLDOWN_SECS}s"
echo "CLK_TCK: $CLK_TCK"
echo "VARIANT: $VARIANT_LABEL | POLL_MODE: $POLL_MODE"
echo "NOTE: This script only collects /proc metrics."
echo "      Polling clients must run on the HOST machine."
echo "      CPU isolation (governor, cpuset, IRQ affinity) must be"
echo "      configured by benchmark-host.sh before this script runs."
echo ""

for level in $LEVELS; do
    echo "---------------------------------------------"
    echo "  Level: $level  (POLL_MODE=$POLL_MODE)"
    echo "---------------------------------------------"

    CSV="$RESULTS_DIR/${level}.csv"
    echo "sample,cpu_pct,rss_kb,loadavg" > "$CSV"

    # Kill any existing MCM
    tmux kill-session -t video 2>/dev/null || true
    killall -9 mavlink-camera-manager 2>/dev/null || true
    sleep 2

    # Start MCM
    echo "[$(date +%T)] Starting MCM with --pipeline-analysis-level $level ..."
    tmux new-session -d -s video \
        "env GST_DEBUG=2 nice --19 $MCM_BIN $MCM_COMMON_ARGS --pipeline-analysis-level $level 2>&1 | tee /tmp/mcm_${level}.log"

    # Wait for process to appear
    sleep 3
    PID=$(get_pid)
    if [ -z "$PID" ]; then
        echo "ERROR: MCM did not start. Skipping level=$level"
        cat "/tmp/mcm_${level}.log" 2>/dev/null | tail -20
        continue
    fi
    echo "[$(date +%T)] MCM PID=$PID"

    # Warmup
    echo "[$(date +%T)] Warming up for ${WARMUP_SECS}s ..."
    sleep "$WARMUP_SECS"

    # Verify still running and actually streaming
    if ! kill -0 "$PID" 2>/dev/null; then
        echo "ERROR: MCM died during warmup. Skipping level=$level"
        cat "/tmp/mcm_${level}.log" 2>/dev/null | tail -30
        continue
    fi

    # Verify the process is consuming CPU (pipelines are running)
    verify_t1=$(read_proc_cpu "$PID")
    sleep 3
    verify_t2=$(read_proc_cpu "$PID")
    verify_delta=$((verify_t2 - verify_t1))
    verify_rss=$(read_proc_rss_kb "$PID")
    echo "[$(date +%T)] Verification: delta_ticks=$verify_delta over 3s, RSS=${verify_rss}KB"
    if [ "$verify_delta" -lt 10 ]; then
        echo "WARNING: Low CPU activity (delta=$verify_delta ticks). Pipelines may not be streaming."
        echo "  Waiting 30 more seconds for recovery..."
        sleep 30
        verify_t1=$(read_proc_cpu "$PID")
        sleep 3
        verify_t2=$(read_proc_cpu "$PID")
        verify_delta=$((verify_t2 - verify_t1))
        echo "[$(date +%T)] Re-verification: delta_ticks=$verify_delta over 3s"
        if [ "$verify_delta" -lt 10 ]; then
            echo "WARNING: Still low CPU. Proceeding anyway (data may not be representative)."
        fi
    fi

    # Signal that sampling is about to start (host client should already be running)
    echo "[$(date +%T)] SAMPLING_START"

    # Sampling -- pure /proc collection, no API polling
    echo "[$(date +%T)] Sampling for ${SAMPLE_SECS}s ..."
    prev_ticks=$(read_proc_cpu "$PID")
    prev_time=$(date +%s%N)

    for i in $(seq 1 "$SAMPLE_SECS"); do
        sleep 1

        cur_ticks=$(read_proc_cpu "$PID")
        cur_time=$(date +%s%N)

        delta_ticks=$((cur_ticks - prev_ticks))
        delta_ns=$((cur_time - prev_time))

        if [ "$delta_ticks" -gt 0 ] 2>/dev/null; then
            cpu_pct=$(awk "BEGIN { printf \"%.2f\", $delta_ticks / ($delta_ns / 1000000000.0) / $CLK_TCK * 100 }")
        else
            cpu_pct="0.00"
        fi

        rss_kb=$(read_proc_rss_kb "$PID")
        loadavg=$(read_loadavg)

        echo "$i,$cpu_pct,$rss_kb,$loadavg" >> "$CSV"

        prev_ticks=$cur_ticks
        prev_time=$cur_time
    done

    echo "[$(date +%T)] SAMPLING_DONE"
    echo "[$(date +%T)] Sampling complete."

    # Kill MCM
    tmux kill-session -t video 2>/dev/null || true
    killall -9 mavlink-camera-manager 2>/dev/null || true
    sleep "$COOLDOWN_SECS"
done

# ── Summary ──

echo ""
echo "============================================="
echo "  RESULTS SUMMARY  (VARIANT=$VARIANT_LABEL, POLL_MODE=$POLL_MODE)"
echo "============================================="
echo ""
printf "%-8s | %10s | %10s | %10s | %10s | %10s\n" \
    "Level" "CPU% mean" "CPU% std" "RSS(KB)avg" "RSS(KB)std" "Load avg"
printf "%-8s-+-%10s-+-%10s-+-%10s-+-%10s-+-%10s\n" \
    "--------" "----------" "----------" "----------" "----------" "----------"

for level in $LEVELS; do
    CSV="$RESULTS_DIR/${level}.csv"
    if [ ! -f "$CSV" ]; then
        printf "%-8s | %10s | %10s | %10s | %10s | %10s\n" \
            "$level" "SKIP" "SKIP" "SKIP" "SKIP" "SKIP"
        continue
    fi

    # Extract columns (skip header)
    tail -n +2 "$CSV" | cut -d, -f2 > "$RESULTS_DIR/${level}_cpu.tmp"
    tail -n +2 "$CSV" | cut -d, -f3 > "$RESULTS_DIR/${level}_rss.tmp"
    tail -n +2 "$CSV" | cut -d, -f4 > "$RESULTS_DIR/${level}_load.tmp"

    cpu_mean=$(mean_of_file "$RESULTS_DIR/${level}_cpu.tmp")
    cpu_std=$(stddev_of_file "$RESULTS_DIR/${level}_cpu.tmp")
    rss_mean=$(mean_of_file "$RESULTS_DIR/${level}_rss.tmp")
    rss_std=$(stddev_of_file "$RESULTS_DIR/${level}_rss.tmp")
    load_mean=$(mean_of_file "$RESULTS_DIR/${level}_load.tmp")

    printf "%-8s | %10s | %10s | %10s | %10s | %10s\n" \
        "$level" "$cpu_mean" "$cpu_std" "$rss_mean" "$rss_std" "$load_mean"
done

echo ""

# ── Delta analysis ──
if [ -f "$RESULTS_DIR/off_cpu.tmp" ] && [ -f "$RESULTS_DIR/lite_cpu.tmp" ] && [ -f "$RESULTS_DIR/full_cpu.tmp" ]; then
    off_cpu=$(mean_of_file "$RESULTS_DIR/off_cpu.tmp")
    lite_cpu=$(mean_of_file "$RESULTS_DIR/lite_cpu.tmp")
    full_cpu=$(mean_of_file "$RESULTS_DIR/full_cpu.tmp")

    off_rss=$(mean_of_file "$RESULTS_DIR/off_rss.tmp")
    lite_rss=$(mean_of_file "$RESULTS_DIR/lite_rss.tmp")
    full_rss=$(mean_of_file "$RESULTS_DIR/full_rss.tmp")

    echo "--- Overhead vs OFF ---"
    lite_delta_cpu=$(awk "BEGIN { printf \"%.2f\", $lite_cpu - $off_cpu }")
    full_delta_cpu=$(awk "BEGIN { printf \"%.2f\", $full_cpu - $off_cpu }")
    lite_delta_rss=$(awk "BEGIN { printf \"%.0f\", $lite_rss - $off_rss }")
    full_delta_rss=$(awk "BEGIN { printf \"%.0f\", $full_rss - $off_rss }")

    if awk "BEGIN { exit !($off_cpu > 0) }"; then
        lite_rel=$(awk "BEGIN { printf \"%.1f\", ($lite_cpu - $off_cpu) / $off_cpu * 100 }")
        full_rel=$(awk "BEGIN { printf \"%.1f\", ($full_cpu - $off_cpu) / $off_cpu * 100 }")
    else
        lite_rel="n/a"
        full_rel="n/a"
    fi

    echo "  LITE CPU overhead:  ${lite_delta_cpu}% absolute (${lite_rel}% relative)"
    echo "  FULL CPU overhead:  ${full_delta_cpu}% absolute (${full_rel}% relative)"
    echo "  LITE RSS overhead:  ${lite_delta_rss} KB"
    echo "  FULL RSS overhead:  ${full_delta_rss} KB"
fi

echo ""
echo "Raw CSVs saved in $RESULTS_DIR/"
echo "Done."
