#!/usr/bin/env bash
# Cross-build the integration test + MCM binary for armv7, deploy to the lab Pi,
# and run remote_integration::thread_leak::test_webrtc_thread_leak inside blueos-core.
#
# Usage:
#   ./cross_run_thread_leak_test.sh              # build, deploy, run test
#   BUILD=0 ./cross_run_thread_leak_test.sh      # reuse last cross-build artifacts
#   GST_DEBUG=3 ./cross_run_thread_leak_test.sh  # forward extra env into the test
#
# Requires: cross, sshpass, rsync, docker (for extracting OpenSSL 1.1 from the cross image)
set -euo pipefail

REMOTE_USER=${REMOTE_USER:-"pi"}
REMOTE_HOST=${REMOTE_HOST:-"192.168.2.2"}
REMOTE_PASS=${REMOTE_PASS:-"raspberry"}
CONTAINER=${CONTAINER:-"blueos-core"}
TARGET=${TARGET:-"armv7-unknown-linux-gnueabihf"}
CROSS_IMAGE=${CROSS_IMAGE:-"joaoantoniocardoso/cross-rs:${TARGET}-bullseye-slim-with-gstreamer"}

REMOTE_TEST_DIR=${REMOTE_TEST_DIR:-"/tmp/pi-thread-leak-test"}
BUILD=${BUILD:-1}

TEST_NAME=${TEST_NAME:-"remote_integration::thread_leak::test_webrtc_thread_leak"}
GST_DEBUG=${GST_DEBUG:-2}

SCRIPT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
cd "$SCRIPT_DIR"

mkdir -p frontend/dist

if [[ "$BUILD" != "0" ]]; then
    echo "==> Cross-building MCM (debug) for ${TARGET}"
    SKIP_WEB=1 cross build --target "$TARGET"

    echo "==> Cross-building integration test (debug) for ${TARGET}"
    SKIP_WEB=1 cross build --target "$TARGET" --test integration
fi

MCM_BINARY="target/${TARGET}/debug/mavlink-camera-manager"
INTEGRATION_BINARY=$(find "target/${TARGET}/debug/deps" -maxdepth 1 -name 'integration-*' -executable -type f | head -1)

if [[ -z "${INTEGRATION_BINARY}" || ! -f "${INTEGRATION_BINARY}" ]]; then
    echo "error: integration test binary not found under target/${TARGET}/debug/deps" >&2
    exit 1
fi
if [[ ! -f "${MCM_BINARY}" ]]; then
    echo "error: MCM binary not found at ${MCM_BINARY}" >&2
    exit 1
fi

echo "==> Using integration binary: ${INTEGRATION_BINARY}"
echo "==> Using MCM binary: ${MCM_BINARY}"

OPENSSL_STAGING=$(mktemp -d -t cross-openssl-XXXXXX)
trap 'rm -rf "${OPENSSL_STAGING}"' EXIT

echo "==> Extracting OpenSSL 1.1 libs from cross image ${CROSS_IMAGE}"
docker run --rm -v "${OPENSSL_STAGING}:/out" "${CROSS_IMAGE}" \
    sh -c 'cp -a /usr/lib/arm-linux-gnueabihf/libssl.so.1.1 /usr/lib/arm-linux-gnueabihf/libcrypto.so.1.1 /out/'

echo "==> Preparing remote test layout on ${REMOTE_USER}@${REMOTE_HOST}:${REMOTE_TEST_DIR}"
sshpass -p "$REMOTE_PASS" ssh -o StrictHostKeyChecking=no \
    "${REMOTE_USER}@${REMOTE_HOST}" \
    "rm -rf ${REMOTE_TEST_DIR} && mkdir -p ${REMOTE_TEST_DIR}/target/debug"

sshpass -p "$REMOTE_PASS" rsync -az --info=progress2 \
    -e "ssh -o StrictHostKeyChecking=no" \
    "${MCM_BINARY}" \
    "${INTEGRATION_BINARY}" \
    "${OPENSSL_STAGING}/libssl.so.1.1" \
    "${OPENSSL_STAGING}/libcrypto.so.1.1" \
    "${REMOTE_USER}@${REMOTE_HOST}:${REMOTE_TEST_DIR}/"

sshpass -p "$REMOTE_PASS" ssh -o StrictHostKeyChecking=no \
    "${REMOTE_USER}@${REMOTE_HOST}" \
    "mv ${REMOTE_TEST_DIR}/mavlink-camera-manager ${REMOTE_TEST_DIR}/target/debug/ && \
     INTEGRATION=\$(ls ${REMOTE_TEST_DIR}/integration-* 2>/dev/null | head -1) && \
     mv \"\${INTEGRATION}\" ${REMOTE_TEST_DIR}/integration && \
     chmod +x ${REMOTE_TEST_DIR}/integration ${REMOTE_TEST_DIR}/target/debug/mavlink-camera-manager"

echo "==> Copying test tree into container ${CONTAINER}"
# docker cp /tmp/pi-thread-leak-test container:/tmp/pi-thread-leak-test, when the
# target already exists, copies the SOURCE *into* the existing target instead of
# overwriting it. That silently keeps a stale MCM binary in the container while
# the host-side tree has the freshly-built one, masking real test outcomes.
# Wipe the container path first so the cp lands on a clean slot.
sshpass -p "$REMOTE_PASS" ssh -o StrictHostKeyChecking=no \
    "${REMOTE_USER}@${REMOTE_HOST}" \
    "docker exec ${CONTAINER} rm -rf ${REMOTE_TEST_DIR} && \
     docker cp ${REMOTE_TEST_DIR} ${CONTAINER}:${REMOTE_TEST_DIR}"

echo "==> Running ${TEST_NAME} in ${CONTAINER} (GST_DEBUG=${GST_DEBUG})"
set +e
sshpass -p "$REMOTE_PASS" ssh -o StrictHostKeyChecking=no \
    "${REMOTE_USER}@${REMOTE_HOST}" \
    "docker exec ${CONTAINER} sh -c 'cd ${REMOTE_TEST_DIR} && \
        LD_LIBRARY_PATH=${REMOTE_TEST_DIR}:\$LD_LIBRARY_PATH \
        GST_DEBUG=${GST_DEBUG} GST_DEBUG_NO_COLOR=1 RUST_BACKTRACE=1 \
        ./integration --exact ${TEST_NAME} --nocapture'"
TEST_EXIT=$?
set -e

if [[ "$TEST_EXIT" -eq 0 ]]; then
    echo ""
    echo "Done. ${TEST_NAME} passed inside ${CONTAINER} on ${REMOTE_HOST}."
else
    echo ""
    echo "Test failed (exit ${TEST_EXIT}). See output above." >&2
    exit "$TEST_EXIT"
fi
