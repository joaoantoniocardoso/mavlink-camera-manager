#!/usr/bin/env bash
set -euo pipefail

IMAGE_TAG="${1:-}"
PI_HOST="${2:-${PI_HOST:-192.168.2.2}}"
PI_USER="${3:-${PI_USER:-pi}}"
PI_PASS="${4:-${PI_PASS:-raspberry}}"

if [ -z "${IMAGE_TAG}" ]; then
    echo "Usage: $0 <repository:tag> [pi_host] [pi_user] [pi_pass]" >&2
    exit 1
fi

REPOSITORY="${IMAGE_TAG%:*}"
TAG="${IMAGE_TAG##*:}"
if [ "${REPOSITORY}" = "${TAG}" ]; then
    echo "ERROR: IMAGE_TAG must be in repository:tag format (got '${IMAGE_TAG}')" >&2
    exit 1
fi

SSH_OPTS="-o StrictHostKeyChecking=no -o LogLevel=ERROR -o ConnectTimeout=10"
SSH="sshpass -p ${PI_PASS} ssh ${SSH_OPTS} ${PI_USER}@${PI_HOST}"
VERSION_API="http://${PI_HOST}/version-chooser/v1.0/version"

echo "Switching to image: ${IMAGE_TAG}"

if ! ${SSH} "docker image inspect '${IMAGE_TAG}' > /dev/null 2>&1"; then
    echo "Image not found locally on Pi, pulling..."
    if ! ${SSH} "docker pull '${IMAGE_TAG}'"; then
        echo "ERROR: docker pull failed and image is not available locally" >&2
        exit 1
    fi
fi

echo "Waiting for version chooser API..."
deadline=$(( $(date +%s) + 60 ))
while ! curl -sf --connect-timeout 3 "${VERSION_API}/current" > /dev/null 2>&1; do
    if [ "$(date +%s)" -ge "${deadline}" ]; then
        echo "ERROR: version chooser API not reachable within 60s" >&2
        exit 1
    fi
    sleep 2
done

HTTP_CODE=$(curl -sf -o /dev/null -w '%{http_code}' \
    --connect-timeout 10 --max-time 30 \
    -X POST "${VERSION_API}/current" \
    -H "Content-Type: application/json" \
    -d "{\"repository\": \"${REPOSITORY}\", \"tag\": \"${TAG}\"}" \
    2>/dev/null) || HTTP_CODE="000"

case "${HTTP_CODE}" in
    200|000)
        echo "Image switch complete"
        ;;
    412)
        echo "ERROR: version chooser says image '${IMAGE_TAG}' is not available (HTTP 412)" >&2
        exit 1
        ;;
    *)
        echo "ERROR: version chooser returned unexpected HTTP ${HTTP_CODE}" >&2
        exit 1
        ;;
esac
