#!/usr/bin/env bash
set -euo pipefail

IMAGES=("joaoantoniocardoso/blueos-core:1.4.4-next.7" "bluerobotics/blueos-core:1.4.4-beta.1")
LABELS=("next" "beta")
PI_HOST="192.168.2.2"
PI_USER="pi"
PI_PASS="raspberry"
DURATION=900
PREFLIGHT_DURATION=150
WARMUP=5
TOTAL_TRIALS=9999
SKIP_PREFLIGHT=false
START_TRIAL=1
PRODUCER_ID="a427fa79-7cb3-5405-9a19-25f057a523a8"
CAMERA_HOST="192.168.2.10"
RTSP_URL="rtsp://${CAMERA_HOST}:554/stream_0"
WEBRTC_URL="ws://${PI_HOST}:6021"
MCM_REST="http://${PI_HOST}:6020"
SWITCH_SCRIPT="/home/joaoantoniocardoso/BlueRobotics/BlueOS-docker/switch-pi-version.sh"
STATS_SCRIPT="scripts/pi_stats_collector.py"
OUTPUT_DIR="overnight_tests_4"

SSH_OPTS="-o StrictHostKeyChecking=no -o LogLevel=ERROR -o ConnectTimeout=10"
SSH="sshpass -p ${PI_PASS} ssh ${SSH_OPTS} ${PI_USER}@${PI_HOST}"
SCP="sshpass -p ${PI_PASS} scp ${SSH_OPTS}"
LOCKFILE="${OUTPUT_DIR}/.overnight.lock"

log_msg() {
    echo "[$(date '+%Y-%m-%d %H:%M:%S')] $1"
}

CAMERA_RESTART_TIME_FILE="${OUTPUT_DIR}/.camera_restart_time"

restart_camera_async() {
    log_msg "Triggering camera restart at ${CAMERA_HOST}..."
    local trigger_time
    trigger_time=$(date +%s.%N)
    echo "${trigger_time}" > "${CAMERA_RESTART_TIME_FILE}"

    # Fire-and-forget camera restart (non-blocking)
    (
        local response
        if response=$(curl -sf "http://${CAMERA_HOST}/action/restart" 2>&1); then
            log_msg "Camera restart triggered successfully: $(echo "${response}" | jq -c . 2>/dev/null || echo "${response}")"
        else
            log_msg "WARNING: Camera restart request failed: ${response}"
        fi
    ) &
}

get_camera_recovery_time() {
    if [ -f "${CAMERA_RESTART_TIME_FILE}" ]; then
        local trigger_time rtsp_ready_time
        trigger_time=$(cat "${CAMERA_RESTART_TIME_FILE}")
        rtsp_ready_time=$(date +%s.%N)
        local recovery_time_ms
        recovery_time_ms=$(echo "scale=1; (${rtsp_ready_time} - ${trigger_time}) * 1000" | bc 2>/dev/null || echo "N/A")
        if [ "${recovery_time_ms}" != "N/A" ]; then
            local recovery_time_s
            recovery_time_s=$(echo "scale=1; ${recovery_time_ms} / 1000" | bc)
            echo "${recovery_time_s}"
        else
            echo "unknown"
        fi
        rm -f "${CAMERA_RESTART_TIME_FILE}"
    else
        echo "unknown"
    fi
}

acquire_lock() {
    mkdir -p "${OUTPUT_DIR}"
    if [ -f "${LOCKFILE}" ]; then
        local old_pid
        old_pid=$(cat "${LOCKFILE}" 2>/dev/null || echo "")
        if [ -n "${old_pid}" ] && kill -0 "${old_pid}" 2>/dev/null; then
            log_msg "ERROR: Another overnight test is already running (PID ${old_pid})."
            log_msg "If that process is stale, remove ${LOCKFILE} and try again."
            exit 1
        else
            log_msg "WARNING: Stale lockfile found (PID ${old_pid} is not running). Removing it."
            rm -f "${LOCKFILE}"
        fi
    fi
    echo $$ > "${LOCKFILE}"
    trap 'rm -f "${LOCKFILE}"' EXIT INT TERM
}

switch_image() {
    local image_tag="$1"
    log_msg "Switching Pi to image: ${image_tag}"
    "${SWITCH_SCRIPT}" "${image_tag}"
    log_msg "Switch complete."
}

reboot_and_wait() {
    log_msg "Rebooting Pi..."
    ${SSH} "sudo reboot" || true

    # Trigger camera restart in parallel with Pi reboot
    restart_camera_async

    sleep 5

    log_msg "Waiting for ping to fail (confirming reboot)..."
    local deadline=$(( $(date +%s) + 30 ))
    while ping -c 1 -W 1 "${PI_HOST}" > /dev/null 2>&1; do
        if [ "$(date +%s)" -ge "${deadline}" ]; then
            log_msg "WARNING: Ping never failed within 30s, proceeding anyway."
            break
        fi
        sleep 1
    done
    log_msg "Ping failed -- reboot confirmed."

    log_msg "Waiting for ping to succeed (up to 500s — bootstrap container replacement takes ~370s)..."
    deadline=$(( $(date +%s) + 500 ))
    while ! ping -c 1 -W 1 "${PI_HOST}" > /dev/null 2>&1; do
        if [ "$(date +%s)" -ge "${deadline}" ]; then
            log_msg "ERROR: Pi did not respond to ping within 500s."
            return 1
        fi
        sleep 2
    done
    log_msg "Ping OK."

    log_msg "Waiting for SSH..."
    deadline=$(( $(date +%s) + 120 ))
    while ! ${SSH} "true" > /dev/null 2>&1; do
        if [ "$(date +%s)" -ge "${deadline}" ]; then
            log_msg "ERROR: SSH did not respond within 120s."
            return 1
        fi
        sleep 3
    done
    log_msg "SSH OK."

    log_msg "Waiting for MCM REST API..."
    deadline=$(( $(date +%s) + 300 ))
    while ! curl -sf --connect-timeout 3 "${MCM_REST}/v4l" > /dev/null 2>&1; do
        if [ "$(date +%s)" -ge "${deadline}" ]; then
            log_msg "ERROR: MCM REST API did not respond within 300s."
            return 1
        fi
        sleep 5
    done
    log_msg "MCM REST API OK."

    local cam_host cam_port
    cam_host=$(echo "${RTSP_URL}" | sed -n 's|rtsp://\([^:/]*\).*|\1|p')
    cam_port=$(echo "${RTSP_URL}" | sed -n 's|rtsp://[^:]*:\([0-9]*\).*|\1|p')
    cam_port="${cam_port:-554}"
    log_msg "Waiting for camera RTSP at ${cam_host}:${cam_port}..."
    deadline=$(( $(date +%s) + 120 ))
    while ! timeout 3 bash -c "echo > /dev/tcp/${cam_host}/${cam_port}" 2>/dev/null; do
        if [ "$(date +%s)" -ge "${deadline}" ]; then
            log_msg "WARNING: Camera RTSP not reachable after 120s, proceeding anyway."
            break
        fi
        sleep 3
    done

    # Calculate and log camera recovery time
    local recovery_time
    recovery_time=$(get_camera_recovery_time)
    log_msg "Camera RTSP ready (recovery: ${recovery_time}s)"

    log_msg "Pi is ready."
}

start_stats_collector() {
    log_msg "Uploading stats collector to Pi..."
    ${SCP} "${STATS_SCRIPT}" "${PI_USER}@${PI_HOST}:/tmp/pi_stats_collector.py"
    log_msg "Starting stats collector..."
    ${SSH} "sudo nohup python3 /tmp/pi_stats_collector.py /tmp/stats_output.csv > /tmp/stats_collector.log 2>&1 &"
    sleep 1
    if ! ${SSH} "pgrep -f pi_stats_collector" > /dev/null 2>&1; then
        log_msg "WARNING: Stats collector does not appear to be running!"
    else
        log_msg "Stats collector running."
    fi
}

stop_stats_collector() {
    local local_output_dir="$1"
    log_msg "Stopping stats collector..."
    ${SSH} "sudo pkill -f pi_stats_collector" || true
    sleep 1
    ${SCP} "${PI_USER}@${PI_HOST}:/tmp/stats_output.csv" "${local_output_dir}/stats.csv" || log_msg "WARNING: Failed to retrieve stats.csv"
    ${SCP} "${PI_USER}@${PI_HOST}:/tmp/stats_collector.log" "${local_output_dir}/stats_collector.log" || log_msg "WARNING: Failed to retrieve stats_collector.log"
    log_msg "Stats collector stopped and data retrieved."
}

run_measurement() {
    local trial_dir="$1"
    local label="$2"
    local duration="$3"
    local csv_dir="${trial_dir}/${label}"
    mkdir -p "${csv_dir}"

    log_msg "Starting stream_latency client (${duration}s)..."
    local exit_code=0
    ./target/debug/examples/stream_latency \
        --webrtc "${WEBRTC_URL}" \
        --producer-id "${PRODUCER_ID}" \
        --rtsp "${RTSP_URL}" \
        --codec h264 \
        --warmup "${WARMUP}" \
        --duration "${duration}" \
        --resilient \
        --retry-delay 2 \
        --csv "${csv_dir}" \
        --json "${csv_dir}/summary.json" \
        > "${csv_dir}/client.log" 2>&1 || exit_code=$?

    if [ "${exit_code}" -eq 0 ]; then
        log_msg "stream_latency client finished successfully."
    else
        log_msg "WARNING: stream_latency client exited with code ${exit_code}."
    fi
    return "${exit_code}"
}

verify_data() {
    local trial_dir="$1"
    local label="$2"
    local data_dir="${trial_dir}/${label}"
    local valid=true

    local segment_count
    segment_count=$(find "${data_dir}" -name "segment_*.csv" 2>/dev/null | wc -l)
    if [ "${segment_count}" -eq 0 ]; then
        log_msg "VERIFY FAIL: No segment_*.csv files in ${data_dir}"
        valid=false
    else
        local first_segment
        first_segment=$(find "${data_dir}" -name "segment_*.csv" | head -1)
        local line_count
        line_count=$(wc -l < "${first_segment}")
        if [ "${line_count}" -le 1 ]; then
            log_msg "VERIFY FAIL: segment CSV has only ${line_count} line(s)"
            valid=false
        fi
    fi

    if [ ! -f "${data_dir}/stats.csv" ]; then
        log_msg "VERIFY FAIL: stats.csv missing in ${data_dir}"
        valid=false
    else
        local stats_lines
        stats_lines=$(wc -l < "${data_dir}/stats.csv")
        if [ "${stats_lines}" -le 1 ]; then
            log_msg "VERIFY FAIL: stats.csv has only ${stats_lines} line(s)"
            valid=false
        fi
    fi

    local total_segment_rows=0
    for f in "${data_dir}"/segment_*.csv; do
        [ -f "$f" ] || continue
        total_segment_rows=$(( total_segment_rows + $(wc -l < "$f") - 1 ))
    done
    local total_stats_rows=0
    if [ -f "${data_dir}/stats.csv" ]; then
        total_stats_rows=$(( $(wc -l < "${data_dir}/stats.csv") - 1 ))
    fi
    log_msg "Data summary: ${segment_count} segment file(s), ${total_segment_rows} data rows, ${total_stats_rows} stats rows"

    if [ -f "${data_dir}/client.log" ]; then
        local errors
        errors=$(grep -i "error" "${data_dir}/client.log" | tail -5) || true
        if [ -n "${errors}" ]; then
            log_msg "Last errors in client.log:"
            echo "${errors}" | while IFS= read -r line; do log_msg "  ${line}"; done
        fi
    fi

    if [ "${valid}" = "true" ]; then
        log_msg "Data verification PASSED for ${label}."
        return 0
    else
        log_msg "Data verification FAILED for ${label}."
        return 1
    fi
}

run_single_trial() {
    local trial_name="$1"
    local image="$2"
    local label="$3"
    local duration="$4"
    local data_dir="${OUTPUT_DIR}/${trial_name}/${label}"
    mkdir -p "${data_dir}"

    log_msg "=== ${trial_name} / ${label} (${image}) ==="

    if ! switch_image "${image}"; then
        log_msg "ABORT TRIAL: Failed to switch image to ${image}"
        return 1
    fi
    if ! reboot_and_wait; then
        log_msg "ABORT TRIAL: Pi did not come back after reboot"
        return 1
    fi

    start_stats_collector
    run_measurement "${OUTPUT_DIR}/${trial_name}" "${label}" "${duration}" || true
    stop_stats_collector "${data_dir}"
    verify_data "${OUTPUT_DIR}/${trial_name}" "${label}"
}

main() {
    acquire_lock

    exec > >(tee -a "${OUTPUT_DIR}/overnight.log") 2>&1

    log_msg "=========================================="
    log_msg "  OVERNIGHT A/B TEST"
    log_msg "  Images: ${IMAGES[*]}"
    log_msg "  Trials: unlimited (randomized A/B, Ctrl-C to stop)"
    log_msg "  Duration: ${DURATION}s per run"
    log_msg "=========================================="

    if [ "${SKIP_PREFLIGHT}" = "true" ]; then
        log_msg "Skipping preflight (SKIP_PREFLIGHT=true)."
    else
        log_msg ""
        log_msg ">>> PREFLIGHT (${PREFLIGHT_DURATION}s per run) <<<"

        local preflight_ok=true
        for i in 0 1; do
            if ! run_single_trial "preflight" "${IMAGES[$i]}" "${LABELS[$i]}" "${PREFLIGHT_DURATION}"; then
                log_msg "PREFLIGHT FAILED for ${LABELS[$i]}. Aborting overnight test."
                preflight_ok=false
                break
            fi
        done

        if [ "${preflight_ok}" != "true" ]; then
            log_msg "Preflight failed. Check logs in ${OUTPUT_DIR}/preflight/"
            exit 1
        fi

        log_msg "Preflight passed for both images."
    fi

    log_msg ""
    log_msg ">>> MAIN TEST (unlimited randomized trials, ${DURATION}s per run, Ctrl-C to stop) <<<"

    local successes=0
    local failures=0
    local total_start
    total_start=$(date +%s)
    local trial=${START_TRIAL}

    while true; do
        trial=$((trial + 1))
        trial_name=$(printf "trial_%04d" "${trial}")

        local first=$((RANDOM % 2))
        local second=$(( 1 - first ))
        log_msg "Trial ${trial}: order = ${LABELS[$first]} then ${LABELS[$second]}"

        for i in "${first}" "${second}"; do
            if run_single_trial "${trial_name}" "${IMAGES[$i]}" "${LABELS[$i]}" "${DURATION}"; then
                successes=$((successes + 1))
            else
                failures=$((failures + 1))
                log_msg "WARNING: ${trial_name}/${LABELS[$i]} data verification failed."
            fi
        done

        local now
        now=$(date +%s)
        local elapsed=$(( now - total_start ))
        log_msg "Progress: ${trial} trials done, ${successes} OK, ${failures} failed, elapsed $(( elapsed / 3600 ))h $(( (elapsed % 3600) / 60 ))m"
    done
}

main "$@"
