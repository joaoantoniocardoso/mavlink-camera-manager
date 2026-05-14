#!/usr/bin/env bash
# Apply tc/netem impairment to the *currently active* WebRTC RTP UDP port
# on the lab pi. Reads MCM's "Local ICE candidate created" log lines to
# discover the dynamically-chosen UDP source port the NiceAgent picked
# for the most recent session, then attaches an HTB+netem qdisc and a
# u32 filter that matches that exact UDP src port. RTSP RTP (different
# ephemeral port) stays on the default qdisc.
#
# This is the right repro path: real network backpressure on WebRTC's
# UDP egress -> webrtcbin udpsink stalls -> with the warm-up queue
# EXCISED (bad state) the stall propagates upstream through the tee
# and forces v4l2 to drop frames -> BOTH RTSP and WebRTC see the same
# missing frames. With MCM_QUEUE_PER_SINK_BRANCH=1 (good state) the
# warm-up queue stays, absorbs the stall, and RTSP stays clean.
#
# Usage:
#   tools/onvehicle/impair_active_webrtc.sh [profile] [iface]
#
# Profiles:
#   tether      5 Mbps, 30+-20ms, 1% loss, 1% reorder   (default)
#   aggressive  3 Mbps, 60+-40ms, 3% loss, 2% reorder
#   loss-only   no rate, no delay, 5% loss              (pure loss)
#   bw-only     2 Mbps, no delay, no loss               (pure bw cap)
#
# IFACE defaults to eth0.

set -euo pipefail

REMOTE_USER=${REMOTE_USER:-"pi"}
REMOTE_HOST=${REMOTE_HOST:-"192.168.2.2"}
REMOTE_PASS=${REMOTE_PASS:-"raspberry"}

PROFILE=${1:-tether}
IFACE=${2:-eth0}

case "$PROFILE" in
    tether)
        RATE_DEF="5mbit"; DELAY_DEF="30ms"; JITTER_DEF="20ms"
        LOSS_DEF="1% 25%"; REORDER_DEF="1% 50%"
        ;;
    aggressive)
        RATE_DEF="3mbit"; DELAY_DEF="60ms"; JITTER_DEF="40ms"
        LOSS_DEF="3% 25%"; REORDER_DEF="2% 50%"
        ;;
    loss-only)
        RATE_DEF=""; DELAY_DEF=""; JITTER_DEF=""
        LOSS_DEF="5%"; REORDER_DEF=""
        ;;
    bw-only)
        RATE_DEF="2mbit"; DELAY_DEF=""; JITTER_DEF=""
        LOSS_DEF=""; REORDER_DEF=""
        ;;
    *)
        echo "unknown profile: $PROFILE (want: tether|aggressive|loss-only|bw-only)" >&2
        exit 1
        ;;
esac
# Env vars override the profile defaults so callers can tweak any
# single dial without writing a new profile, e.g. LOSS=30%.
RATE=${RATE-$RATE_DEF}
DELAY=${DELAY-$DELAY_DEF}
JITTER=${JITTER-$JITTER_DEF}
LOSS=${LOSS-$LOSS_DEF}
REORDER=${REORDER-$REORDER_DEF}

SSH="sshpass -p $REMOTE_PASS ssh -o StrictHostKeyChecking=no -o LogLevel=ERROR $REMOTE_USER@$REMOTE_HOST"

# Find the local IP of $IFACE on the pi so we can match the matching
# ICE candidate (NiceAgent gathers one candidate per interface).
IFACE_IP=$($SSH "ip -4 -o addr show dev $IFACE | awk '{print \$4}' | cut -d/ -f1")
if [[ -z "$IFACE_IP" ]]; then
    echo "[!] No IPv4 on $IFACE on the pi" >&2
    exit 1
fi
echo "[+] $IFACE local IP on pi: $IFACE_IP"

# Discover the active WebRTC RTP source port. NiceAgent binds one UDP
# socket per host interface (gather phase), so the WebRTC port is the
# one bound to $IFACE_IP specifically. RTSP server's udpsink binds to
# 0.0.0.0 with an ephemeral port -- excluding 0.0.0.0 here is what
# discriminates WebRTC from RTSP egress.
if [[ -n "${WEBRTC_PORT:-}" ]]; then
    echo "[+] Using WEBRTC_PORT override: $WEBRTC_PORT"
else
    # 1) Snapshot MCM-owned UDP sockets bound on $IFACE_IP (these are
    #    NiceAgent host candidates; RTSP udpsinks bound on 0.0.0.0 are
    #    correctly excluded by this filter).
    CANDIDATE_PORTS=$($SSH "sudo ss -ulnp 2>/dev/null" \
        | grep -E "mavlink-camera-" \
        | grep -oE "$IFACE_IP:[0-9]+" \
        | cut -d: -f2 | sort -u)
    if [[ -z "$CANDIDATE_PORTS" ]]; then
        echo "[!] No MCM UDP sockets bound on $IFACE_IP." >&2
        echo "    Connect a WebRTC peer first." >&2
        exit 1
    fi
    # 2) Watch eth0 for 3 s and pick the highest-volume port that is
    #    in CANDIDATE_PORTS. Intersecting with CANDIDATE_PORTS is what
    #    prevents an unrelated high-volume port (e.g. RTSP's 0.0.0.0-
    #    bound udpsink) from being picked. `timeout` exits 124 when it
    #    fires, so capture with `|| true` to keep set -e + pipefail OK.
    CAND_RE=$(echo "$CANDIDATE_PORTS" | paste -sd'|' -)
    echo "[+] Candidate WebRTC UDP src ports on $IFACE_IP: $(echo $CANDIDATE_PORTS | tr '\n' ' ')"
    echo "    Sniffing $IFACE for 3 s to find the live RTP port..."
    TCPDUMP_OUT=$($SSH "sudo timeout 3 tcpdump -i $IFACE -nn 'udp and src host $IFACE_IP and greater 200' 2>/dev/null" || true)
    LIVE=$(printf '%s\n' "$TCPDUMP_OUT" \
        | grep -oE "$IFACE_IP\.[0-9]+ >" \
        | sed -E "s/$IFACE_IP\.([0-9]+) >/\1/" \
        | grep -E "^($CAND_RE)$" \
        | sort | uniq -c | sort -rn | head -1 | awk '{print $2}' || true)
    if [[ -z "$LIVE" ]]; then
        echo "[!] No active media UDP egress from a WebRTC candidate port in 3 s." >&2
        echo "    tcpdump captured $(printf '%s\n' "$TCPDUMP_OUT" | wc -l) lines." >&2
        echo "    Is the WebRTC session actually streaming? Try WEBRTC_PORT=<port> to override." >&2
        exit 1
    fi
    WEBRTC_PORT=$LIVE
fi
echo "[+] Active WebRTC RTP local UDP port on $IFACE_IP: $WEBRTC_PORT"

# Sanity: confirm MCM actually owns that UDP socket right now.
# NiceAgent may bind to either a specific IP or to 0.0.0.0/*, so accept
# both forms.
OWN=$($SSH "sudo ss -ulnp 2>/dev/null \
    | grep -E '(^| )($IFACE_IP|0\.0\.0\.0|\*):$WEBRTC_PORT( |$)' || true")
if [[ -z "$OWN" ]]; then
    echo "[!] WARNING: UDP :$WEBRTC_PORT is NOT currently bound by any" >&2
    echo "    process. The WebRTC session likely ended; reconnect and retry." >&2
    exit 1
fi
echo "    Owner: $(echo "$OWN" | sed -E 's/.*users:\(\(/  /' )"

NETEM_ARGS=""
# netem refuses `reorder` without a `delay`, so auto-inject a 1 ms delay
# if the caller asked for reordering but no explicit delay.
if [[ -z "$DELAY" && -n "$REORDER" ]]; then
    DELAY="1ms"
fi
if [[ -n "$DELAY" ]]; then
    if [[ -n "$JITTER" ]]; then
        NETEM_ARGS="delay $DELAY $JITTER distribution normal"
    else
        NETEM_ARGS="delay $DELAY"
    fi
fi
# Deep netem queue so packets are HELD, not dropped. Lets the kernel
# SO_SNDBUF fill instead of the qdisc shedding packets.
NETEM_LIMIT=${NETEM_LIMIT:-10000}
NETEM_ARGS="${NETEM_ARGS:+$NETEM_ARGS }limit $NETEM_LIMIT"
if [[ -n "$LOSS" ]]; then
    NETEM_ARGS="${NETEM_ARGS:+$NETEM_ARGS }loss $LOSS"
fi
if [[ -n "$REORDER" ]]; then
    NETEM_ARGS="${NETEM_ARGS:+$NETEM_ARGS }reorder $REORDER"
fi

TC=/usr/sbin/tc
REMOTE_SCRIPT=$(cat <<EOF
set -euo pipefail
echo "[+] Tearing down any existing root qdisc on $IFACE..."
sudo $TC qdisc del dev $IFACE root 2>/dev/null || true

echo "[+] Adding prio root qdisc (bands 0,1,2; all default to band 0)"
sudo $TC qdisc add dev $IFACE root handle 1: prio bands 3 \\
    priomap 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0
EOF
)

if [[ -n "$RATE" ]]; then
    # Deep queue (limit 4Mb) so packets BUFFER instead of being dropped.
    # Dropping at the qdisc removes the packet from the socket buffer
    # immediately, which prevents the kernel SO_SNDBUF from filling and
    # therefore prevents real backpressure up to udpsink. Holding packets
    # in tbf's queue forces the socket buffer to fill instead.
    TBF_LIMIT=${TBF_LIMIT:-4mb}
    TBF_BURST=${TBF_BURST:-32kbit}
    REMOTE_SCRIPT="$REMOTE_SCRIPT
echo \"[+] Attaching TBF rate=$RATE burst=$TBF_BURST limit=$TBF_LIMIT on band 1:2\"
sudo $TC qdisc add dev $IFACE parent 1:2 handle 10: tbf rate $RATE burst $TBF_BURST limit $TBF_LIMIT"
    NEXT_PARENT="10:1"
else
    NEXT_PARENT="1:2"
fi

if [[ -n "$NETEM_ARGS" ]]; then
    REMOTE_SCRIPT="$REMOTE_SCRIPT
echo \"[+] Attaching netem ($NETEM_ARGS) under $NEXT_PARENT\"
sudo $TC qdisc add dev $IFACE parent $NEXT_PARENT handle 20: netem $NETEM_ARGS"
fi

REMOTE_SCRIPT="$REMOTE_SCRIPT
echo \"[+] Filter: UDP src port == $WEBRTC_PORT -> band 1:2 (impaired)\"
sudo $TC filter add dev $IFACE protocol ip parent 1: prio 1 u32 \\
    match ip protocol 17 0xff \\
    match ip sport $WEBRTC_PORT 0xffff \\
    flowid 1:2

echo
echo '=== qdisc state ==='
sudo $TC -s qdisc show dev $IFACE
echo
echo '=== filter state ==='
sudo $TC -s filter show dev $IFACE parent 1:
"

echo
echo "[+] Profile: $PROFILE"
echo "    rate=${RATE:-uncapped}  delay=${DELAY:-0}  jitter=${JITTER:-0}  loss=${LOSS:-none}  reorder=${REORDER:-none}"
echo
$SSH "$REMOTE_SCRIPT"
echo
echo "[+] Impairment ACTIVE on udp src port $WEBRTC_PORT only."
echo "    Restore with: tools/onvehicle/restore_network.sh"
