#!/usr/bin/env bash
# Fallback path for the t3.19.2 binary - because the t3.19.2 source tree
# does not cross-build cleanly with the current cross-rs (build-script
# / vergen artifact host-vs-target confusion at that tag).
#
# Strategy: boot the stock BlueOS 1.4.3 image locally for a moment,
# `docker cp` /usr/bin/mavlink-camera-manager out, save it as
# `bin/mcm-t3.19.2-armv7`. The binary is dynamically linked against
# armv7 libgstreamer; ELF interpreter is `/lib/ld-linux-armhf.so.3`.
# Works fine on the same gst 1.x ABI across 1.24 and 1.28 - which is
# exactly the `e0_old_mcm_on_new_gst` cell we need.
#
# Requires:
# - docker available locally
# - permission to pull bluerobotics/blueos-core:1.4.3

set -euo pipefail
SCRIPT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)

IMAGE=${IMAGE:-bluerobotics/blueos-core:1.4.3}
BIN_DIR=$SCRIPT_DIR/bin
OUT=$BIN_DIR/mcm-t3.19.2-armv7
TMP_CONTAINER=mcm-extract-$$

mkdir -p "$BIN_DIR"

echo "pulling $IMAGE (may take a few minutes on first run)..."
docker pull "$IMAGE"

echo "spawning throwaway container $TMP_CONTAINER..."
docker create --name "$TMP_CONTAINER" --platform linux/arm/v7 "$IMAGE" /bin/true >/dev/null

# The binary location in BlueOS 1.4.3 - confirm before relying on it.
# (Use docker run if `docker cp` from a created-but-not-started container
# has trouble with armv7 emulation on x86_64.)
CANDIDATES=(
    /root/mavlink-camera-manager
    /usr/bin/mavlink-camera-manager
    /opt/mavlink-camera-manager/bin/mavlink-camera-manager
    /home/pi/mavlink-camera-manager
)
for path in "${CANDIDATES[@]}"; do
    if docker cp "$TMP_CONTAINER:$path" "$OUT" 2>/dev/null; then
        echo "extracted $path from $IMAGE -> $OUT"
        break
    fi
done

docker rm -f "$TMP_CONTAINER" >/dev/null

if [ ! -f "$OUT" ]; then
    cat >&2 <<EOF
fatal: could not locate mavlink-camera-manager inside $IMAGE
       Tried: ${CANDIDATES[*]}
       Manual fallback:
         1) docker run --rm -it --platform linux/arm/v7 --entrypoint /bin/sh \\
              $IMAGE
         2) find / -name mavlink-camera-manager -type f 2>/dev/null
         3) docker cp <container>:<path> $OUT
EOF
    exit 1
fi

chmod +x "$OUT"
file "$OUT" || true
echo "done."
