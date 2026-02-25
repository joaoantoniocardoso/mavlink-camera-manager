#!/usr/bin/env bash
set -euo pipefail

REMOTE_USER="pi"
REMOTE_HOST="192.168.2.2"
REMOTE_PASS="raspberry"
CONTAINER="blueos-core"
TARGET="armv7-unknown-linux-gnueabihf"
BINARY_NAME="mavlink-camera-manager-stats"

mkdir -p frontend/dist

SKIP_WEB=1 cross build --release --target "$TARGET"

BINARY="target/${TARGET}/release/mavlink-camera-manager"

sshpass -p "$REMOTE_PASS" scp -o StrictHostKeyChecking=no \
    "$BINARY" "${REMOTE_USER}@${REMOTE_HOST}:/tmp/${BINARY_NAME}"

sshpass -p "$REMOTE_PASS" ssh -o StrictHostKeyChecking=no \
    "${REMOTE_USER}@${REMOTE_HOST}" \
    "docker cp /tmp/${BINARY_NAME} ${CONTAINER}:/root/${BINARY_NAME} && \
     docker exec ${CONTAINER} chmod +x /root/${BINARY_NAME}"

echo "Done. Binary deployed to /root/${BINARY_NAME} inside container ${CONTAINER}"
