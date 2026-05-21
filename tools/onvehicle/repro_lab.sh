#!/usr/bin/env bash
# Lab-pi orchestrator: cross-build for aarch64, deploy to the lab pi,
# restart MCM with the NiceAgent port-range pinned so the tc filter
# in impair_webrtc.sh can target it. Sub-commands are intentionally
# small so they can be sequenced step-by-step from the README.
#
# Usage:
#   tools/onvehicle/repro_lab.sh deploy           # cross-build + push + restart (buggy: no MCM_QUEUE_PER_SINK_BRANCH)
#   tools/onvehicle/repro_lab.sh good             # restart with MCM_QUEUE_PER_SINK_BRANCH=1 (no rebuild)
#   tools/onvehicle/repro_lab.sh bad              # restart in buggy state (no rebuild)
#   tools/onvehicle/repro_lab.sh configure-cams   # POST 2 RTSP stream configs to MCM's REST API
#   tools/onvehicle/repro_lab.sh status           # quick health check
#   tools/onvehicle/repro_lab.sh logs             # tail MCM log, filtered to the bug fingerprint
#   tools/onvehicle/repro_lab.sh logs all         # tail MCM log unfiltered
#   tools/onvehicle/repro_lab.sh logs scan        # one-shot grep of recent log for the bug fingerprint

set -euo pipefail

REMOTE_USER=${REMOTE_USER:-"pi"}
REMOTE_HOST=${REMOTE_HOST:-"192.168.2.2"}
REMOTE_PASS=${REMOTE_PASS:-"raspberry"}
CONTAINER=${CONTAINER:-"blueos-core"}
TARGET=${TARGET:-"armv7-unknown-linux-gnueabihf"}
BINARY_NAME=${BINARY_NAME:-"mavlink-camera-manager"}
MCM_PORT=${MCM_PORT:-6020}

PORT_MIN=${PORT_MIN:-50176}
PORT_MAX=${PORT_MAX:-50303}

# Stream identifiers picked up by impair_webrtc.sh / measure_rtsp_fps.sh.
STREAM1_NAME=${STREAM1_NAME:-"lab_cam1"}
STREAM2_NAME=${STREAM2_NAME:-"lab_cam2"}
WIDTH=${WIDTH:-1920}
HEIGHT=${HEIGHT:-1080}
FPS=${FPS:-30}

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" &>/dev/null && pwd)
REPO_ROOT=$(cd "$SCRIPT_DIR/../.." &>/dev/null && pwd)

SSH="sshpass -p $REMOTE_PASS ssh -o StrictHostKeyChecking=no -o LogLevel=ERROR $REMOTE_USER@$REMOTE_HOST"

cmd_build() {
    echo "[+] Cross-building for $TARGET (SKIP_WEB=1)..."
    cd "$REPO_ROOT"
    mkdir -p frontend/dist
    SKIP_WEB=1 cross build --release --target "$TARGET"
    local sha
    sha=$(git rev-parse --short HEAD)
    local arch="${TARGET%%-*}"
    local out="/tmp/mcm-juliusz-${arch}-$sha"
    cp "target/$TARGET/release/$BINARY_NAME" "$out"
    echo "[+] Staged binary at $out (sha256: $(sha256sum "$out" | cut -c1-12)...)"
    echo "$out"
}

cmd_push() {
    local bin=$1
    echo "[+] Pushing $bin -> $REMOTE_HOST:/tmp/$BINARY_NAME"
    sshpass -p "$REMOTE_PASS" rsync -az --info=progress2 \
        -e "ssh -o StrictHostKeyChecking=no -o LogLevel=ERROR" \
        "$bin" "$REMOTE_USER@$REMOTE_HOST:/tmp/$BINARY_NAME"
    $SSH "docker cp /tmp/$BINARY_NAME $CONTAINER:/root/$BINARY_NAME && \
          docker exec $CONTAINER chmod +x /root/$BINARY_NAME"
}

# Restart MCM in tmux 'video' with a configurable env prefix. Mirrors
# the command line used by cross_build_and_run.sh + restart_with_env.sh.
# Set RT_THREADS=0 to drop --enable-realtime-threads (matches the
# customer's no-rt config and makes the WebRTC backpressure bug far
# more visible).
restart_mcm() {
    local label=$1
    shift
    local env_prefix="$*"

    local rt_flag="--enable-realtime-threads"
    if [[ "${RT_THREADS:-1}" == "0" ]]; then
        rt_flag=""
        label="$label-no-rt"
    fi

    local mcm_cmd="${env_prefix} GST_DEBUG=3 /root/${BINARY_NAME} \
--default-settings BlueROVUDP \
--mavlink tcpout:127.0.0.1:5777 \
--mavlink-system-id 1 \
--mavlink-camera-component-id-range=100-105 \
--gst-feature-rank omxh264enc=0,v4l2h264enc=250,x264enc=260 \
--log-path /var/logs/blueos/services/mavlink-camera-manager \
--stun-server stun://stun.l.google.com:19302 \
$rt_flag \
--verbose"

    echo "[+] Stopping current MCM..."
    $SSH "docker exec $CONTAINER tmux send-keys -t video C-c" || true
    sleep 1
    echo "[+] Marking restart ($label) in log..."
    $SSH "docker exec $CONTAINER bash -lc 'echo \"=== MCM RESTART label=$label env=\\\"$env_prefix\\\" ts=\$(date -Iseconds) ===\" >> /var/logs/blueos/services/mavlink-camera-manager/restart_marks.log'"
    echo "[+] Restarting MCM (env prefix: $env_prefix)"
    $SSH "docker exec $CONTAINER tmux send-keys -t video '$mcm_cmd' Enter"
    echo "[+] Done. Give MCM ~5 s to come up."
}

cmd_deploy() {
    local bin
    bin=$(cmd_build | tail -1)
    cmd_push "$bin"
    cmd_bad
}

build_debug_repro_env() {
    local out=""
    if [[ "${EXCISE_DELAY_MS:-0}" != "0" ]]; then
        out+="MCM_WEBRTC_EXCISE_DELAY_MS=$EXCISE_DELAY_MS "
    fi
    printf '%s' "$out"
}

cmd_bad() {
    local extra
    extra=$(build_debug_repro_env)
    restart_mcm "bad" "${extra}MCM_WEBRTC_PORT_MIN=$PORT_MIN MCM_WEBRTC_PORT_MAX=$PORT_MAX"
}

cmd_good() {
    local extra
    extra=$(build_debug_repro_env)
    restart_mcm "good" "${extra}MCM_QUEUE_PER_SINK_BRANCH=1 MCM_WEBRTC_PORT_MIN=$PORT_MIN MCM_WEBRTC_PORT_MAX=$PORT_MAX"
}

# Resolve the /dev/video* device paths for the USB H264 cams via
# MCM's own /v4l endpoint, keeping only sources that actually
# advertise the H264 fourcc at WIDTHxHEIGHT@FPS. Returns one
# `/dev/videoN` path per line, ordered as MCM enumerates them.
detect_cams() {
    $SSH "curl -sS http://127.0.0.1:$MCM_PORT/v4l" \
      | python3 -c "
import json, sys
data = json.load(sys.stdin)
for cam in data:
    src = cam.get('source', '')
    if not src.startswith('/dev/video'):
        continue
    for f in cam.get('formats', []):
        if f.get('encode') == 'H264':
            for s in f.get('sizes', []):
                if s.get('width') == $WIDTH and s.get('height') == $HEIGHT:
                    print(src)
                    break
            else:
                continue
            break
"
}

cmd_configure_cams() {
    echo "[+] Detecting H264-capable USB cameras on $REMOTE_HOST (via /v4l)..."
    mapfile -t cams < <(detect_cams)
    if [[ ${#cams[@]} -lt 2 ]]; then
        echo "expected 2 H264 USB cameras at ${WIDTH}x${HEIGHT}, found ${#cams[@]}:" >&2
        printf '   %s\n' "${cams[@]}" >&2
        exit 1
    fi
    echo "    cam1: ${cams[0]}"
    echo "    cam2: ${cams[1]}"

    post_stream() {
        local name=$1 device=$2
        echo "[+] POSTing $name -> $device"
        local body
        body=$(cat <<JSON
{
  "name": "$name",
  "source": "$device",
  "stream_information": {
    "endpoints": [ "rtsp://0.0.0.0:8554/$name" ],
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
      "disable_zenoh": false,
      "disable_thumbnails": false,
      "disable_lazy": false
    }
  }
}
JSON
        )
        printf %s "$body" \
          | $SSH "curl -sS -X POST http://127.0.0.1:$MCM_PORT/streams \
                  -H 'Content-Type: application/json' --data-binary @-" \
          | head -c 1000
        echo
    }

    post_stream "$STREAM1_NAME" "${cams[0]}"
    post_stream "$STREAM2_NAME" "${cams[1]}"

    echo "[+] Current /streams:"
    $SSH "curl -sS http://127.0.0.1:$MCM_PORT/streams" | head -c 2000
    echo
}

cmd_status() {
    echo "=== Pi reachability ==="
    $SSH "echo ok && uname -m && cat /etc/os-release | head -2"
    echo
    echo "=== Container tmux 'video' tail ==="
    $SSH "docker exec $CONTAINER tmux capture-pane -p -t video | tail -20" || true
    echo
    echo "=== /streams ==="
    $SSH "curl -sS http://127.0.0.1:$MCM_PORT/streams" | head -c 2000
    echo
    echo "=== tc qdisc on eth0 ==="
    $SSH "sudo /usr/sbin/tc qdisc show dev eth0"
}

# Regex grepped from the 'video' tmux pane to surface only the warm-up-queue
# excision / pipeline-stall fingerprint that maps to the customer's symptom.
BUG_PATTERN='Position (unchanged|normalized)|Excised queue|Excised rtp|ICE connection changed|v4l2_drops|mcm_inst'

# Path inside the container where we mirror the 'video' pane while following.
LIVE_FILE='/tmp/mcm-video-live.log'

# Cleanup hook: turn the pane mirror off so we don't keep tee'ing output to
# /tmp forever after this invocation exits.
stop_video_mirror() {
    $SSH "docker exec $CONTAINER tmux pipe-pane -t video" >/dev/null 2>&1 || true
}

follow_video() {
    local filter=$1
    $SSH "docker exec $CONTAINER bash -c \": > $LIVE_FILE; tmux pipe-pane -t video 'cat >> $LIVE_FILE'\""
    trap stop_video_mirror EXIT INT TERM
    if [[ -n "$filter" ]]; then
        $SSH "docker exec $CONTAINER tail -n 200 -F $LIVE_FILE" \
            | grep --line-buffered -E "$filter"
    else
        $SSH "docker exec $CONTAINER tail -n 200 -F $LIVE_FILE"
    fi
}

cmd_logs() {
    local mode=${1:-}
    case "$mode" in
        scan)
            $SSH "docker exec $CONTAINER tmux capture-pane -p -t video -S -10000" \
                | grep -E "$BUG_PATTERN" || true
            ;;
        all)
            follow_video ""
            ;;
        ""|follow)
            follow_video "$BUG_PATTERN"
            ;;
        *)
            echo "logs: unknown variant '$mode' (want: follow|all|scan)" >&2
            exit 1
            ;;
    esac
}

main() {
    if [[ $# -lt 1 ]]; then
        sed -n '2,12p' "$0"
        exit 1
    fi
    case "$1" in
        deploy)         cmd_deploy ;;
        bad)            cmd_bad ;;
        good)           cmd_good ;;
        build)          cmd_build ;;
        configure-cams) cmd_configure_cams ;;
        status)         cmd_status ;;
        logs)           shift; cmd_logs "${1:-}" ;;
        *) echo "unknown subcommand: $1" >&2; exit 1 ;;
    esac
}

main "$@"
