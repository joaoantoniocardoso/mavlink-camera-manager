#!/usr/bin/env bash
set -euo pipefail

# ── Host-side benchmark orchestrator ──
#
# Runs on the DEVELOPMENT MACHINE (not the Pi). Orchestrates the full
# benchmark: starts the Pi-side /proc sampler via SSH, runs polling
# clients LOCALLY (so they don't compete for Pi CPU), and collects results.
#
# Usage:
#   # Phase 1: Probe overhead (no polling clients)
#   POLL_MODE=none ./scripts/benchmark-host.sh "off lite full"
#
#   # Phase 2: HTTP full snapshot (client runs on host)
#   POLL_MODE=http-full ./scripts/benchmark-host.sh full
#
#   # Phase 2: WebSocket full snapshot (client runs on host)
#   POLL_MODE=ws-full ./scripts/benchmark-host.sh full
#

# ── Configuration ──
PI_HOST="${PI_HOST:-192.168.2.2}"
PI_USER="${PI_USER:-pi}"
PI_PASS="${PI_PASS:-raspberry}"
CONTAINER="${CONTAINER:-blueos-core-ab-test}"
POLL_MODE="${POLL_MODE:-none}"
VARIANT_LABEL="${VARIANT_LABEL:-default}"
LEVELS="${1:-off lite full}"

MCM_API="http://${PI_HOST}:6020"
MCM_WS="ws://${PI_HOST}:6020"

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"

SSHP="sshpass -p $PI_PASS"
SSH_OPTS="-o StrictHostKeyChecking=no -o LogLevel=ERROR"
SSH="$SSHP ssh $SSH_OPTS ${PI_USER}@${PI_HOST}"
SCP="$SSHP scp $SSH_OPTS"
DOCKER_EXEC="docker exec $CONTAINER bash -c"

CLIENT_PID=""

# ── Helpers ──

log() { echo "[$(date +%T)] $*"; }

cleanup() {
    log "Cleaning up..."
    stop_client
    teardown_pi_isolation 2>/dev/null || true
}
trap cleanup EXIT

start_http_client() {
    log "Starting HTTP polling client on host -> $MCM_API/stats/streams/snapshot"
    (
        while true; do
            curl -s --max-time 5 "$MCM_API/stats/streams/snapshot" > /dev/null 2>&1 || true
            sleep 1
        done
    ) &
    CLIENT_PID=$!
    log "HTTP client started (PID=$CLIENT_PID)"
}

start_ws_client() {
    log "Starting WebSocket client on host -> $MCM_WS/stats/streams/snapshot/ws"
    python3 -c '
import websocket, sys, signal
signal.signal(signal.SIGTERM, lambda *_: sys.exit(0))
signal.signal(signal.SIGINT, lambda *_: sys.exit(0))
try:
    ws = websocket.create_connection(
        "'"$MCM_WS"'/stats/streams/snapshot/ws?interval_ms=1000",
        timeout=10,
    )
    while True:
        ws.recv()
except Exception as e:
    print(f"WS client error: {e}", file=sys.stderr)
' &
    CLIENT_PID=$!
    sleep 2
    if kill -0 "$CLIENT_PID" 2>/dev/null; then
        log "WebSocket client started (PID=$CLIENT_PID)"
    else
        log "WARNING: WebSocket client failed to start"
        CLIENT_PID=""
    fi
}

stop_client() {
    if [ -n "$CLIENT_PID" ]; then
        kill "$CLIENT_PID" 2>/dev/null || true
        wait "$CLIENT_PID" 2>/dev/null || true
        log "Client stopped (was PID=$CLIENT_PID)"
        CLIENT_PID=""
    fi
}

start_client_for_mode() {
    local mode=$1
    case "$mode" in
        http-full)  start_http_client ;;
        ws-full)    start_ws_client ;;
        none)       log "No polling client (POLL_MODE=none)" ;;
        *)          log "Unknown POLL_MODE=$mode, no client started" ;;
    esac
}

# ── CPU Isolation ──

setup_pi_isolation() {
    log "Setting up CPU isolation on Pi..."

    # 1. Pin CPU governor to 'performance' and frequency to 1500 MHz on all 4 cores
    log "  Pinning CPU governor=performance, freq=1500 MHz..."
    $SSH 'sudo bash -c "for i in 0 1 2 3; do
        echo performance > /sys/devices/system/cpu/cpu\$i/cpufreq/scaling_governor
        echo 1500000 > /sys/devices/system/cpu/cpu\$i/cpufreq/scaling_min_freq
        echo 1500000 > /sys/devices/system/cpu/cpu\$i/cpufreq/scaling_max_freq
    done"' 2>&1

    # 2. Pin Docker container to CPUs 1,2,3 (reserve core 0 for OS)
    log "  Pinning container $CONTAINER to CPUs 1,2,3..."
    $SSH "docker update --cpuset-cpus=1,2,3 $CONTAINER" 2>&1

    # 3. Move all OS processes to CPU 0
    log "  Moving all OS processes to CPU 0..."
    $SSH 'sudo bash -c "for pid in \$(ps -eo pid= --no-headers 2>/dev/null); do
        taskset -p -c 0 \$pid 2>/dev/null || true
    done"' 2>&1

    # 4. Pin hardware IRQs to CPU 0
    log "  Pinning IRQs to CPU 0..."
    $SSH 'sudo bash -c "for irq in /proc/irq/*/smp_affinity_list; do
        echo 0 > \"\$irq\" 2>/dev/null || true
    done"' 2>&1

    log "CPU isolation setup complete."
}

validate_pi_isolation() {
    log "Validating CPU isolation..."

    local ok=true

    # Check governor on all cores
    local governors
    governors=$($SSH 'cat /sys/devices/system/cpu/cpu{0,1,2,3}/cpufreq/scaling_governor' 2>/dev/null)
    log "  Governors: $(echo $governors | tr '\n' ' ')"
    if echo "$governors" | grep -qv performance; then
        log "  ERROR: Not all governors are 'performance'"
        ok=false
    fi

    # Check frequency on all cores
    local freqs
    freqs=$($SSH 'cat /sys/devices/system/cpu/cpu{0,1,2,3}/cpufreq/scaling_cur_freq' 2>/dev/null)
    log "  Frequencies: $(echo $freqs | tr '\n' ' ')"
    if echo "$freqs" | grep -qv 1500000; then
        log "  ERROR: Not all cores at 1500000 kHz"
        ok=false
    fi

    # Check container cpuset
    local cpuset
    cpuset=$($SSH "docker inspect --format='{{.HostConfig.CpusetCpus}}' $CONTAINER" 2>/dev/null)
    log "  Container cpuset: $cpuset"
    if [ "$cpuset" != "1,2,3" ]; then
        log "  ERROR: Container cpuset is '$cpuset', expected '1,2,3'"
        ok=false
    fi

    if [ "$ok" = false ]; then
        log "ABORT: CPU isolation validation failed. Fix the issues above."
        exit 1
    fi

    log "CPU isolation validated successfully."
}

teardown_pi_isolation() {
    log "Restoring Pi CPU defaults..."

    # Restore governor to ondemand and unlock frequency
    $SSH 'sudo bash -c "for i in 0 1 2 3; do
        echo 600000 > /sys/devices/system/cpu/cpu\$i/cpufreq/scaling_min_freq 2>/dev/null || true
        echo 1500000 > /sys/devices/system/cpu/cpu\$i/cpufreq/scaling_max_freq 2>/dev/null || true
        echo ondemand > /sys/devices/system/cpu/cpu\$i/cpufreq/scaling_governor 2>/dev/null || true
    done"' 2>&1 || true

    # Remove container CPU pin
    $SSH "docker update --cpuset-cpus= $CONTAINER" 2>&1 || true

    log "Pi CPU defaults restored."
}

# ── Main ──

echo "============================================="
echo "  MCM Benchmark Orchestrator (host-side)"
echo "============================================="
echo "Pi: ${PI_USER}@${PI_HOST} (container: $CONTAINER)"
echo "VARIANT: $VARIANT_LABEL | POLL_MODE: $POLL_MODE"
echo "LEVELS: $LEVELS"
echo ""

# Set up CPU isolation on the Pi
setup_pi_isolation
validate_pi_isolation
echo ""

# Deploy scripts to Pi
log "Deploying scripts to Pi..."
$SCP "$SCRIPT_DIR/benchmark-pipeline-analysis.sh" "${PI_USER}@${PI_HOST}:/tmp/benchmark.sh" 2>&1
$SSH "docker cp /tmp/benchmark.sh $CONTAINER:/tmp/benchmark.sh" 2>&1
log "Scripts deployed."

# For each level, we run the Pi-side sampler and the host-side client together
for level in $LEVELS; do
    echo ""
    echo "============================================="
    echo "  Level: $level  (VARIANT=$VARIANT_LABEL, POLL_MODE=$POLL_MODE)"
    echo "============================================="

    # Start the Pi-side sampler for this ONE level
    # It will: start MCM, warmup, sample /proc, kill MCM, cooldown
    log "Starting Pi-side sampler for level=$level ..."
    $SSH "$DOCKER_EXEC 'VARIANT_LABEL=$VARIANT_LABEL POLL_MODE=$POLL_MODE bash /tmp/benchmark.sh $level'" &
    PI_JOB_PID=$!

    # Wait for MCM to come up and warmup to complete
    # The Pi script does: 3s startup + 30s warmup + 3s verify = ~36s before sampling
    log "Waiting for MCM warmup on Pi (~40s) ..."
    sleep 40

    # Start the host-side polling client (only for non-off levels)
    if [ "$level" != "off" ]; then
        start_client_for_mode "$POLL_MODE"
    fi

    # Wait for Pi-side sampler to finish
    # Sampling takes 60s + 15s cooldown = ~75s from sampling start
    log "Waiting for Pi-side sampling to complete (~75s) ..."
    wait "$PI_JOB_PID" 2>/dev/null || true

    # Stop the host-side client
    stop_client

    log "Level $level complete."
done

# Fetch results from Pi
echo ""
log "Fetching results from Pi..."
RESULTS_LOCAL="/tmp/benchmark_results_${VARIANT_LABEL}_$(date +%Y%m%d_%H%M%S)"
PI_RESULTS_DIR="/tmp/benchmark_results_${VARIANT_LABEL}"
mkdir -p "$RESULTS_LOCAL"
$SSH "$DOCKER_EXEC 'cat ${PI_RESULTS_DIR}/*.csv'" > "$RESULTS_LOCAL/all_output.txt" 2>/dev/null || true

# Fetch each CSV individually
for level in $LEVELS; do
    $SSH "$DOCKER_EXEC 'cat ${PI_RESULTS_DIR}/${level}.csv'" > "$RESULTS_LOCAL/${level}.csv" 2>/dev/null || true
done

echo ""
echo "============================================="
echo "  HOST-SIDE RESULTS  (VARIANT=$VARIANT_LABEL, POLL_MODE=$POLL_MODE)"
echo "============================================="
echo ""
echo "Raw CSVs saved to: $RESULTS_LOCAL/"

# Print summary using the fetched CSVs
mean_of_file() {
    awk '{ sum += $1; n++ } END { if (n>0) printf "%.2f", sum/n; else print "0" }' "$1"
}
stddev_of_file() {
    awk '{ sum += $1; sumsq += $1*$1; n++ } END {
        if (n>1) { m=sum/n; printf "%.2f", sqrt((sumsq/n) - m*m) } else print "0"
    }' "$1"
}

printf "%-8s | %10s | %10s | %10s | %10s | %10s\n" \
    "Level" "CPU% mean" "CPU% std" "RSS(KB)avg" "RSS(KB)std" "Load avg"
printf "%-8s-+-%10s-+-%10s-+-%10s-+-%10s-+-%10s\n" \
    "--------" "----------" "----------" "----------" "----------" "----------"

for level in $LEVELS; do
    CSV="$RESULTS_LOCAL/${level}.csv"
    if [ ! -f "$CSV" ] || [ ! -s "$CSV" ]; then
        printf "%-8s | %10s | %10s | %10s | %10s | %10s\n" \
            "$level" "SKIP" "SKIP" "SKIP" "SKIP" "SKIP"
        continue
    fi

    tail -n +2 "$CSV" | cut -d, -f2 > "$RESULTS_LOCAL/${level}_cpu.tmp"
    tail -n +2 "$CSV" | cut -d, -f3 > "$RESULTS_LOCAL/${level}_rss.tmp"
    tail -n +2 "$CSV" | cut -d, -f4 > "$RESULTS_LOCAL/${level}_load.tmp"

    cpu_mean=$(mean_of_file "$RESULTS_LOCAL/${level}_cpu.tmp")
    cpu_std=$(stddev_of_file "$RESULTS_LOCAL/${level}_cpu.tmp")
    rss_mean=$(mean_of_file "$RESULTS_LOCAL/${level}_rss.tmp")
    rss_std=$(stddev_of_file "$RESULTS_LOCAL/${level}_rss.tmp")
    load_mean=$(mean_of_file "$RESULTS_LOCAL/${level}_load.tmp")

    printf "%-8s | %10s | %10s | %10s | %10s | %10s\n" \
        "$level" "$cpu_mean" "$cpu_std" "$rss_mean" "$rss_std" "$load_mean"
done

echo ""

# Delta analysis
if [ -f "$RESULTS_LOCAL/off_cpu.tmp" ] && [ -f "$RESULTS_LOCAL/full_cpu.tmp" ]; then
    off_cpu=$(mean_of_file "$RESULTS_LOCAL/off_cpu.tmp")
    full_cpu=$(mean_of_file "$RESULTS_LOCAL/full_cpu.tmp")
    off_rss=$(mean_of_file "$RESULTS_LOCAL/off_rss.tmp")
    full_rss=$(mean_of_file "$RESULTS_LOCAL/full_rss.tmp")

    full_delta_cpu=$(awk "BEGIN { printf \"%.2f\", $full_cpu - $off_cpu }")
    full_delta_rss=$(awk "BEGIN { printf \"%.0f\", $full_rss - $off_rss }")

    if awk "BEGIN { exit !($off_cpu > 0) }"; then
        full_rel=$(awk "BEGIN { printf \"%.1f\", ($full_cpu - $off_cpu) / $off_cpu * 100 }")
    else
        full_rel="n/a"
    fi

    echo "--- Overhead vs OFF ---"
    echo "  FULL CPU overhead:  ${full_delta_cpu}% absolute (${full_rel}% relative)"
    echo "  FULL RSS overhead:  ${full_delta_rss} KB"
fi

if [ -f "$RESULTS_LOCAL/lite_cpu.tmp" ] && [ -f "$RESULTS_LOCAL/off_cpu.tmp" ]; then
    off_cpu=$(mean_of_file "$RESULTS_LOCAL/off_cpu.tmp")
    lite_cpu=$(mean_of_file "$RESULTS_LOCAL/lite_cpu.tmp")
    off_rss=$(mean_of_file "$RESULTS_LOCAL/off_rss.tmp")
    lite_rss=$(mean_of_file "$RESULTS_LOCAL/lite_rss.tmp")

    lite_delta_cpu=$(awk "BEGIN { printf \"%.2f\", $lite_cpu - $off_cpu }")
    lite_delta_rss=$(awk "BEGIN { printf \"%.0f\", $lite_rss - $off_rss }")

    if awk "BEGIN { exit !($off_cpu > 0) }"; then
        lite_rel=$(awk "BEGIN { printf \"%.1f\", ($lite_cpu - $off_cpu) / $off_cpu * 100 }")
    else
        lite_rel="n/a"
    fi

    echo "  LITE CPU overhead:  ${lite_delta_cpu}% absolute (${lite_rel}% relative)"
    echo "  LITE RSS overhead:  ${lite_delta_rss} KB"
fi

echo ""
echo "Done."
