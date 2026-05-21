#!/usr/bin/env bash
# Run the UDP-decoupler experiment matrix.
#
# For every (variant, condition, rep) cell:
#   1. Restart MCM on the lab pi with the variant's env (MCM_UDP_DECOUPLER).
#   2. Wait for the stream to come back up.
#   3. (Re)configure cam1 with RTSP + UDP endpoints on lab pi.
#   4. Apply network impairment (if any).
#   5. Run sample_cpu.sh inside the container (background).
#   6. Run stream_latency on the lab PC (foreground).
#   7. Fetch CPU csv + filtered MCM log; tear down impairment.
#
# Outputs: ./results/decoupler_matrix/<timestamp>/<variant>_<condition>_<rep>/
#   stream_latency.csv      per-frame arrivals (RTSP + UDP + WebRTC)
#   stream_latency.txt      summary stats text
#   cpu.csv                 1 Hz CPU + ctx-switch samples (sample_cpu.sh)
#   mcm.log                 mcm_inst markers from the run's time window
#   meta.json               cell metadata
#
# Usage:
#   tools/onvehicle/run_decoupler_matrix.sh             # full 3x3x3 matrix
#   VARIANTS=appsink CONDITIONS=idle REPS=1 ./run...sh  # subset
#   WARMUP=30 DURATION=90 ./run...sh                    # tweak per-cell timing

set -euo pipefail

SCRIPT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
REPO_ROOT=$(cd "$SCRIPT_DIR/../.." && pwd)
# shellcheck source=_lib.sh
source "$SCRIPT_DIR/_lib.sh"

VARIANTS=${VARIANTS:-"appsink b1 proxy"}
CONDITIONS=${CONDITIONS:-"idle impair_mild impair_aggressive"}
REPS=${REPS:-3}
WARMUP=${WARMUP:-30}
DURATION=${DURATION:-90}
REPORT_INTERVAL=${REPORT_INTERVAL:-10}
SETUP_GRACE=${SETUP_GRACE:-8}

# Address the pi will see this host on. Default to the source IP of the
# route to $PI_HOST so UDP packets land here even when the host isn't the
# canonical topside `192.168.2.1`.
if [[ -z "${LAB_PC_IP:-}" ]]; then
    LAB_PC_IP=$(ip -4 -o route get "$PI_HOST" 2>/dev/null | awk '{for(i=1;i<=NF;i++) if ($i=="src") print $(i+1)}')
    LAB_PC_IP=${LAB_PC_IP:-192.168.2.1}
fi
RTSP_URL=${RTSP_URL:-rtsp://$PI_HOST:8554/lab_cam1}
UDP_ENDPOINT=${UDP_ENDPOINT:-$LAB_PC_IP:5601}
WEBRTC_WS=${WEBRTC_WS:-ws://$PI_HOST:6021}

STREAM_NAME=${STREAM_NAME:-lab_cam1}
WIDTH=${WIDTH:-1920}
HEIGHT=${HEIGHT:-1080}
FPS=${FPS:-30}
PORT_MIN=${PORT_MIN:-50176}
PORT_MAX=${PORT_MAX:-50303}

TS=$(date +%Y%m%dT%H%M%S)
RESULTS_DIR=${RESULTS_DIR:-$REPO_ROOT/results/decoupler_matrix/$TS}
mkdir -p "$RESULTS_DIR"

LOG_PATH_CONTAINER=/var/logs/blueos/services/mavlink-camera-manager
SAMPLE_CPU_PATH_CONTAINER=/root/sample_cpu.sh

echo "[+] Pushing sample_cpu.sh into $CONTAINER..."
scp_to_host "$SCRIPT_DIR/sample_cpu.sh" "/tmp/sample_cpu.sh" >/dev/null
ssh_host "docker cp /tmp/sample_cpu.sh $CONTAINER:$SAMPLE_CPU_PATH_CONTAINER && docker exec $CONTAINER chmod +x $SAMPLE_CPU_PATH_CONTAINER"

stream_latency_bin() {
    local bin="$REPO_ROOT/target/release/examples/stream_latency"
    if [[ ! -x "$bin" ]]; then
        echo "[+] Building stream_latency (release)..." >&2
        (cd "$REPO_ROOT" && cargo build --release --example stream_latency) >&2
    fi
    echo "$bin"
}

restart_mcm_with_env() {
    local label=$1
    local env_prefix=$2

    local mcm_cmd="${env_prefix} GST_DEBUG=3 /root/mavlink-camera-manager \
--default-settings BlueROVUDP \
--mavlink tcpout:127.0.0.1:5777 \
--mavlink-system-id 1 \
--mavlink-camera-component-id-range=100-105 \
--gst-feature-rank omxh264enc=0,v4l2h264enc=250,x264enc=260 \
--log-path $LOG_PATH_CONTAINER \
--stun-server stun://stun.l.google.com:19302 \
--enable-realtime-threads \
--verbose"

    ssh_host "docker exec $CONTAINER tmux send-keys -t $TMUX_SESSION C-c" >/dev/null 2>&1 || true
    sleep 1
    ssh_host "docker exec $CONTAINER bash -lc 'echo \"=== MATRIX RESTART label=$label env=\\\"$env_prefix\\\" ts=\$(date -Iseconds) ===\" >> $LOG_PATH_CONTAINER/restart_marks.log'" >/dev/null
    ssh_host "docker exec $CONTAINER tmux send-keys -t $TMUX_SESSION '$mcm_cmd' Enter"
}

wait_for_mcm_http() {
    local n=0
    while (( n < 30 )); do
        if ssh_host "curl -fsS http://127.0.0.1:6020/streams" >/dev/null 2>&1; then
            return 0
        fi
        sleep 1
        n=$((n + 1))
    done
    return 1
}

wait_for_stream_running() {
    local name=$1
    local n=0
    while (( n < 30 )); do
        if ssh_host "curl -fsS http://127.0.0.1:6020/streams" 2>/dev/null \
            | python3 -c "
import json,sys
for s in json.load(sys.stdin):
    if s.get('video_and_stream',{}).get('name')=='$name' and s.get('running'):
        sys.exit(0)
sys.exit(1)" \
            >/dev/null 2>&1; then
            return 0
        fi
        sleep 1
        n=$((n + 1))
    done
    return 1
}

delete_all_streams() {
    # MCM's default-settings auto-creates "UDP Stream 0" against the H264 USB
    # cam, which conflicts with our reconfiguration. Wipe every stream name
    # before posting our matrix config. The /delete_stream endpoint takes
    # the stream name as a query parameter.
    local names
    names=$(ssh_host "curl -sS http://127.0.0.1:6020/streams 2>/dev/null" \
        | python3 -c "
import json, sys, urllib.parse
for s in json.load(sys.stdin):
    name = s.get('video_and_stream',{}).get('name','')
    if name:
        print(urllib.parse.quote(name, safe=''))
")
    while IFS= read -r encoded_name; do
        [[ -z "$encoded_name" ]] && continue
        ssh_host "curl -sS -X DELETE 'http://127.0.0.1:6020/delete_stream?name=$encoded_name'" >/dev/null 2>&1 || true
    done <<<"$names"
}

configure_lab_cam1() {
    # Single stream, three endpoints (UDP + RTSP + WebRTC via signalling).
    # The historical UDP+RTSP soft-limit in Stream::try_new is bypassed via
    # MCM_ALLOW_UDP_RTSP_CONCURRENT=1 set in env_for_variant().
    delete_all_streams
    sleep 2
    local cam_dev
    cam_dev=$(ssh_host "curl -sS http://127.0.0.1:6020/v4l" \
      | python3 -c "
import json,sys
for cam in json.load(sys.stdin):
    src=cam.get('source','')
    if not src.startswith('/dev/video'): continue
    for f in cam.get('formats',[]):
        if f.get('encode') != 'H264': continue
        for s in f.get('sizes',[]):
            if s.get('width')==$WIDTH and s.get('height')==$HEIGHT:
                print(src); sys.exit()
")
    if [[ -z "$cam_dev" ]]; then
        echo "[!] No H264 USB cam at ${WIDTH}x${HEIGHT} on the pi" >&2
        return 1
    fi
    echo "[+] cam1 -> $cam_dev (rtsp + udp + webrtc)"
    local body
    body=$(cat <<JSON
{
  "name": "$STREAM_NAME",
  "source": "$cam_dev",
  "stream_information": {
    "endpoints": [
      "rtsp://0.0.0.0:8554/$STREAM_NAME",
      "udp://$LAB_PC_IP:5601"
    ],
    "configuration": {
      "type": "video",
      "encode": "H264",
      "height": $HEIGHT,
      "width": $WIDTH,
      "frame_interval": { "numerator": 1, "denominator": $FPS }
    },
    "extended_configuration": {
      "thermal": false,
      "disable_mavlink": true,
      "disable_zenoh": true,
      "disable_thumbnails": false,
      "disable_lazy": false
    }
  }
}
JSON
)
    ssh_host "curl -sS -X POST http://127.0.0.1:6020/streams -H 'Content-Type: application/json' --data-binary @-" \
        <<<"$body" >/dev/null
}

discover_producer_id() {
    ssh_host "curl -sS http://127.0.0.1:6020/streams" \
      | python3 -c "
import json, sys
for s in json.load(sys.stdin):
    if s.get('video_and_stream',{}).get('name')=='$STREAM_NAME':
        print(s.get('id','')); sys.exit()
"
}

env_for_variant() {
    local v=$1
    local common="MCM_ALLOW_UDP_RTSP_CONCURRENT=1 MCM_DISABLE_MCAP=1 MCM_WEBRTC_PORT_MIN=$PORT_MIN MCM_WEBRTC_PORT_MAX=$PORT_MAX"
    case "$v" in
        appsink) echo "MCM_UDP_DECOUPLER=appsink $common" ;;
        b1)      echo "MCM_UDP_DECOUPLER=b1 $common" ;;
        proxy)   echo "MCM_UDP_DECOUPLER=proxy $common" ;;
        legacy)  echo "MCM_UDP_DECOUPLER=legacy $common" ;;
        *) echo "unknown variant: $v" >&2; return 1 ;;
    esac
}

apply_impairment() {
    local cond=$1
    local out=${IMPAIR_LOG:-/dev/null}
    case "$cond" in
        idle) return 0 ;;
        impair_mild)
            LOSS="0.5%" DELAY="20ms" JITTER="5ms" \
                "$SCRIPT_DIR/impair_active_webrtc.sh" loss-only >"$out" 2>&1
            ;;
        impair_aggressive)
            LOSS="2%" DELAY="50ms" JITTER="15ms" \
                "$SCRIPT_DIR/impair_active_webrtc.sh" loss-only >"$out" 2>&1
            ;;
        *) echo "unknown condition: $cond" >&2; return 1 ;;
    esac
}

teardown_impairment() {
    "$SCRIPT_DIR/restore_network.sh" >/dev/null 2>&1 || true
}

start_cpu_sampler_async() {
    local out_in_container=$1
    local total_s=$2
    ssh_host "docker exec -d $CONTAINER bash -lc '$SAMPLE_CPU_PATH_CONTAINER $total_s $out_in_container'"
}

fetch_logs_for_window() {
    local start_iso=$1 end_iso=$2 out=$3
    ssh_host "docker exec $CONTAINER bash -lc 'grep -aE \"mcm_inst|udp_decoupler_selected|b1_queue_inserted|Excised|v4l2_drops\" $LOG_PATH_CONTAINER/mavlink-camera-manager.\$(date -u +%Y-%m-%d-%H).log 2>/dev/null'" \
        > "$out" 2>/dev/null || true
}

run_cell() {
    local variant=$1
    local condition=$2
    local rep=$3
    local cell_dir="$RESULTS_DIR/${variant}_${condition}_${rep}"
    mkdir -p "$cell_dir"

    local env_prefix
    env_prefix=$(env_for_variant "$variant")

    echo "[+] [$variant/$condition/rep$rep] restart MCM env=\"$env_prefix\""
    restart_mcm_with_env "matrix_${variant}_${condition}_${rep}" "$env_prefix"

    if ! wait_for_mcm_http; then
        echo "[!] MCM HTTP didn't come up; skipping cell" >&2
        echo "fail_mcm_not_up" > "$cell_dir/SKIPPED"
        return 0
    fi
    configure_lab_cam1
    if ! wait_for_stream_running "$STREAM_NAME"; then
        echo "[!] Stream $STREAM_NAME never reached running; skipping cell" >&2
        echo "fail_stream_not_running" > "$cell_dir/SKIPPED"
        return 0
    fi
    sleep "$SETUP_GRACE"

    local producer_id
    producer_id=$(discover_producer_id)
    if [[ -z "$producer_id" ]]; then
        echo "[!] Could not discover producer_id for $STREAM_NAME; skipping" >&2
        echo "fail_no_producer_id" > "$cell_dir/SKIPPED"
        return 0
    fi
    echo "[+] producer_id=$producer_id"

    local total_s=$((WARMUP + DURATION + 5))
    start_cpu_sampler_async /tmp/cpu_cell.csv "$total_s"

    # `impair_active_webrtc.sh` finds the live UDP source port by sniffing
    # eth0, so the WebRTC peer must already be sending media. We start
    # `stream_latency` (which includes the WebRTC client) in the
    # background, wait $IMPAIR_WAIT_S for media to flow, apply impairment
    # while we're still well inside the $WARMUP window, and only then
    # block waiting for the probe to finish.
    local start_iso
    start_iso=$(date -Iseconds)
    local sl_bin
    sl_bin=$(stream_latency_bin)
    "$sl_bin" \
        --rtsp "$RTSP_URL" \
        --udp  "$UDP_ENDPOINT" \
        --webrtc "$WEBRTC_WS" \
        --producer-id "$producer_id" \
        --warmup "$WARMUP" \
        --duration "$DURATION" \
        --report-interval "$REPORT_INTERVAL" \
        --csv "$cell_dir/stream_latency.csv" \
        > "$cell_dir/stream_latency.txt" 2>&1 &
    local sl_pid=$!

    if [[ "$condition" != "idle" ]]; then
        sleep "${IMPAIR_WAIT_S:-8}"
        if ! IMPAIR_LOG="$cell_dir/impair.log" apply_impairment "$condition"; then
            echo "fail_impair" > "$cell_dir/SKIPPED"
            kill "$sl_pid" 2>/dev/null || true
            wait "$sl_pid" 2>/dev/null || true
            teardown_impairment
            return 0
        fi
    fi

    wait "$sl_pid" || echo "[!] stream_latency exit nonzero; CSV may still be partial" >&2
    local end_iso
    end_iso=$(date -Iseconds)

    teardown_impairment

    # Pull the CPU CSV out of the container (the in-container sampler writes
    # to /tmp/cpu_cell.csv; the file is also visible from the docker host
    # via the container's overlay fs, but `docker cp` is simpler).
    ssh_host "docker cp $CONTAINER:/tmp/cpu_cell.csv /tmp/cpu_cell.csv" >/dev/null 2>&1 || true
    scp_from_host /tmp/cpu_cell.csv "$cell_dir/cpu.csv" 2>/dev/null \
        || ssh_host "cat /tmp/cpu_cell.csv" > "$cell_dir/cpu.csv" 2>/dev/null \
        || true

    fetch_logs_for_window "$start_iso" "$end_iso" "$cell_dir/mcm.log"

    cat > "$cell_dir/meta.json" <<META
{
  "variant": "$variant",
  "condition": "$condition",
  "rep": $rep,
  "producer_id": "$producer_id",
  "start_iso": "$start_iso",
  "end_iso": "$end_iso",
  "rtsp_url": "$RTSP_URL",
  "udp_endpoint": "$UDP_ENDPOINT",
  "webrtc_ws": "$WEBRTC_WS",
  "warmup_s": $WARMUP,
  "duration_s": $DURATION,
  "env": "$env_prefix"
}
META

    echo "[+] [$variant/$condition/rep$rep] done -> $cell_dir"
}

scp_from_host() {
    local src=$1 dst=$2
    if command -v sshpass >/dev/null 2>&1; then
        sshpass -p "$PI_PASS" scp -o StrictHostKeyChecking=no -o LogLevel=ERROR \
            "$PI_USER@$PI_HOST:$src" "$dst"
    else
        scp -o StrictHostKeyChecking=no -o LogLevel=ERROR \
            "$PI_USER@$PI_HOST:$src" "$dst"
    fi
}

main() {
    echo "[+] Results dir: $RESULTS_DIR"
    cat > "$RESULTS_DIR/matrix_meta.json" <<META
{
  "ts": "$TS",
  "variants": "$VARIANTS",
  "conditions": "$CONDITIONS",
  "reps": $REPS,
  "warmup_s": $WARMUP,
  "duration_s": $DURATION,
  "pi_host": "$PI_HOST",
  "lab_pc_ip": "$LAB_PC_IP"
}
META

    for variant in $VARIANTS; do
        for condition in $CONDITIONS; do
            for ((rep = 1; rep <= REPS; rep++)); do
                run_cell "$variant" "$condition" "$rep" || true
            done
        done
    done

    teardown_impairment
    echo
    echo "[+] Matrix complete. Analyse with:"
    echo "      tools/onvehicle/analyze_decoupler.py $RESULTS_DIR"
}

main "$@"
