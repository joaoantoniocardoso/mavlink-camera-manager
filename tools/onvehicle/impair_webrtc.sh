#!/usr/bin/env bash
# Apply targeted egress impairment on the pi for WebRTC traffic only.
#
# Targets UDP packets whose SOURCE port falls in [PORT_MIN..PORT_MAX].
# Pair with MCM_WEBRTC_PORT_MIN/MCM_WEBRTC_PORT_MAX env vars so the
# NiceAgent picks ports inside that band. RTSP RTP (sourced from
# random ephemeral ports outside the band) stays on the default
# pfifo_fast queue and sees zero impairment.
#
# Usage:
#   tools/onvehicle/impair_webrtc.sh [profile]
#
# Profiles:
#   tether     5 Mbps, 30 ms +-20 ms, 1% loss (default; ROV-tether-ish)
#   aggressive 3 Mbps, 60 ms +-40 ms, 3% loss
#   minimal    no rate cap, 20 ms +-10 ms, no loss
#
# Overridable via env: IFACE, PORT_MIN, PORT_MAX, RATE, DELAY,
# JITTER, LOSS, REORDER. Must run as root on the pi.

set -euo pipefail

REMOTE_USER=${REMOTE_USER:-"pi"}
REMOTE_HOST=${REMOTE_HOST:-"192.168.2.2"}
REMOTE_PASS=${REMOTE_PASS:-"raspberry"}

PROFILE=${1:-tether}

case "$PROFILE" in
    tether)
        RATE=${RATE:-5mbit}
        DELAY=${DELAY:-30ms}
        JITTER=${JITTER:-20ms}
        LOSS=${LOSS:-"1% 25%"}
        REORDER=${REORDER:-"1% 50%"}
        ;;
    aggressive)
        RATE=${RATE:-3mbit}
        DELAY=${DELAY:-60ms}
        JITTER=${JITTER:-40ms}
        LOSS=${LOSS:-"3% 25%"}
        REORDER=${REORDER:-"2% 50%"}
        ;;
    minimal)
        RATE=${RATE:-}
        DELAY=${DELAY:-20ms}
        JITTER=${JITTER:-10ms}
        LOSS=${LOSS:-}
        REORDER=${REORDER:-}
        ;;
    *)
        echo "unknown profile: $PROFILE (want: tether|aggressive|minimal)" >&2
        exit 1
        ;;
esac

IFACE=${IFACE:-eth0}
# PORT_MIN must be 128-aligned (low 7 bits zero); PORT_MAX = PORT_MIN + 127.
# u32 filter uses mask 0xff80 -> exactly 128 ports matched.
PORT_MIN=${PORT_MIN:-50176}
PORT_MAX=${PORT_MAX:-50303}

# Build netem args string
NETEM_ARGS="delay $DELAY $JITTER distribution normal"
if [[ -n "$LOSS" ]]; then
    NETEM_ARGS="$NETEM_ARGS loss $LOSS"
fi
if [[ -n "$REORDER" ]]; then
    NETEM_ARGS="$NETEM_ARGS reorder $REORDER"
fi

SSH="sshpass -p $REMOTE_PASS ssh -o StrictHostKeyChecking=no -o LogLevel=ERROR $REMOTE_USER@$REMOTE_HOST"

TC=/usr/sbin/tc
REMOTE_SCRIPT=$(cat <<EOF
set -euo pipefail
echo "[+] Tearing down any existing root qdisc on $IFACE..."
sudo $TC qdisc del dev $IFACE root 2>/dev/null || true

echo "[+] Adding prio qdisc (3 bands, everything defaults to band 0 -> clean)"
sudo $TC qdisc add dev $IFACE root handle 1: prio bands 3 \
    priomap 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0

echo "[+] Attaching ${RATE:+TBF rate=$RATE +}netem ($NETEM_ARGS) on band 1:2"
EOF
)

if [[ -n "$RATE" ]]; then
    REMOTE_SCRIPT="$REMOTE_SCRIPT
sudo $TC qdisc add dev $IFACE parent 1:2 handle 10: tbf rate $RATE burst 32kbit latency 100ms
sudo $TC qdisc add dev $IFACE parent 10:1 handle 20: netem $NETEM_ARGS"
else
    REMOTE_SCRIPT="$REMOTE_SCRIPT
sudo $TC qdisc add dev $IFACE parent 1:2 handle 20: netem $NETEM_ARGS"
fi

REMOTE_SCRIPT="$REMOTE_SCRIPT
echo \"[+] Adding u32 filter: UDP src port in [$PORT_MIN..$PORT_MAX] -> band 1:2\"
sudo $TC filter add dev $IFACE protocol ip parent 1: prio 1 u32 \\
    match ip protocol 17 0xff \\
    match ip sport $PORT_MIN 0xff80 \\
    flowid 1:2

echo
echo '=== qdisc state ==='
sudo $TC -s qdisc show dev $IFACE
echo
echo '=== filter state ==='
sudo $TC -s filter show dev $IFACE parent 1:
"

echo "[+] Applying '$PROFILE' profile on $REMOTE_HOST:$IFACE..."
echo "    rate=${RATE:-uncapped} delay=$DELAY jitter=$JITTER loss=${LOSS:-none} reorder=${REORDER:-none}"
echo "    matched ports: UDP src $PORT_MIN-$PORT_MAX"
echo
$SSH "$REMOTE_SCRIPT"
echo
echo "[+] Done. Restore with: tools/onvehicle/restore_network.sh"
