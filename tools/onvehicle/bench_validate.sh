#!/usr/bin/env bash
# Local bench validation. Runs one fast integration test under three env
# configurations and greps stdout for the expected mcm_inst markers:
#
#   - baseline (no env flags): expect queue_level samples, no b1/b2 events.
#   - B1+B2 (PER_SINK_BRANCH + DISABLE_THUMBNAIL): expect both markers and
#     the test to pass.  Note: on the fake/test pipeline, B1 alone
#     interacts with the image-sink branch (UDP buffers stall when both
#     are present); the on-vehicle Phase D combines the two for the
#     `e4_q_targeted` experiment, so this is the realistic configuration.
#   - MCM_DISABLE_THUMBNAIL=1: expect b2_sink_disabled.thumbnail event.
#
# Exits 0 if all three pass; non-zero with diagnostic otherwise.
#
# Usage: ./bench_validate.sh [test_name]
# test_name defaults to test_fake_h264_udp_data_flow.

set -euo pipefail
SCRIPT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
WORKSPACE=$(cd "$SCRIPT_DIR/../.." && pwd)
cd "$WORKSPACE"

TEST=${1:-test_fake_h264_udp_data_flow}
OUT=$(mktemp -d -t mcm-bench-XXXXXX)
echo "bench output: $OUT"

run() {
    local label=$1; shift
    local logfile=$OUT/$label.log
    echo "==> $label"
    if (cd "$WORKSPACE" && SKIP_WEB=1 "$@" cargo test --test integration -- \
            --nocapture --test-threads=1 "$TEST" 2>&1) > "$logfile"; then
        echo "$label: cargo test PASSED"
    else
        echo "$label: cargo test FAILED -- see $logfile" >&2
        return 1
    fi
}

assert_grep() {
    local label=$1
    local pattern=$2
    local logfile=$OUT/$label.log
    if rg -q "$pattern" "$logfile"; then
        echo "$label: pattern OK ($pattern)"
    else
        echo "$label: pattern MISSING ($pattern) -- see $logfile" >&2
        return 1
    fi
}

assert_not_grep() {
    local label=$1
    local pattern=$2
    local logfile=$OUT/$label.log
    if rg -q "$pattern" "$logfile"; then
        echo "$label: pattern UNEXPECTED ($pattern) -- see $logfile" >&2
        return 1
    fi
    echo "$label: absence OK ($pattern)"
}

# Logs contain ANSI escape codes; match on the bare event marker, not on
# the structured-field rendering.
# Baseline run: B0 should be active, no B1/B2.
run baseline
assert_grep baseline 'queue_level'
assert_not_grep baseline 'b1_queue_inserted'
assert_not_grep baseline 'b2_sink_disabled'

# B1+B2 run: MCM_QUEUE_PER_SINK_BRANCH=1 + MCM_DISABLE_THUMBNAIL=1.
# This is the recipe for the on-vehicle `e4_q_targeted` experiment
# combined with Phase C's `e2_no_thumbnail` baseline.  B1 should insert
# per-branch queues on the UDP sink, B2 should skip the image sink.
run b1_b2 env MCM_QUEUE_PER_SINK_BRANCH=1 MCM_DISABLE_THUMBNAIL=1
assert_grep b1_b2 'b1_queue_inserted'
assert_grep b1_b2 'b2_sink_disabled'

# B2-only run: MCM_DISABLE_THUMBNAIL=1 should skip thumbnail construction.
run b2_thumb env MCM_DISABLE_THUMBNAIL=1
assert_grep b2_thumb 'b2_sink_disabled'

echo "bench validation: ALL PASSED"
echo "logs: $OUT"
