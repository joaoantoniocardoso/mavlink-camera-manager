#!/usr/bin/env bash
set -euo pipefail

cd /opt/mcm

cmd="${1:-overnight}"
if [ "$#" -gt 0 ]; then
    shift
fi

case "${cmd}" in
    overnight)
        exec bash ./scripts/overnight_ab_test.sh "$@"
        ;;
    report)
        exec python3 ./scripts/generate_overnight_report.py "$@"
        ;;
    shell)
        exec bash "$@"
        ;;
    *)
        exec "${cmd}" "$@"
        ;;
esac
