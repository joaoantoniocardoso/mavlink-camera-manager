# On-vehicle 4 h SSH session toolkit

Companion scripts for the `MCM Onboard Frame Loss - 4 h SSH Session WBS`
plan (`.cursor/plans/mcm_onboard_frame_loss_-_4_h_ssh_session_wbs_*.plan.md`).

All scripts run from a laptop with SSH access to the vehicle's BlueOS
host (`pi@192.168.2.2` by convention). MCM runs inside the `blueos-core`
container, in a `tmux` session called `video`. Image swaps are driven by
Kraken (BlueOS' image-management service); each swap resets the
container, so `install.sh` is re-run after every swap.

## Environment

All scripts honour these env vars; defaults match the BlueOS factory
setup:

```
PI_HOST=192.168.2.2        # SSH host
PI_USER=pi
PI_PASS=raspberry
CONTAINER=blueos-core      # MCM container name
TMUX_SESSION=video         # tmux session running MCM
TARGET=armv7-unknown-linux-gnueabihf
DEBUG_DIR=/var/log/mcm-debug  # in-container debug output
```

`SSH_CMD` is a wrapper that injects `sshpass`/`-o StrictHostKeyChecking=no`,
defined inline in each script. Set `SSH_CMD="ssh"` to skip `sshpass`.

## Binaries (`bin/`)

- `mcm-debug-armv7` - built from this branch with B0/B1/B2 layers.
  Cross-build with `SKIP_WEB=1 cross build --release --target $TARGET`
  in this worktree, then `cp target/$TARGET/release/mavlink-camera-manager
  bin/mcm-debug-armv7`.
- `mcm-t3.19.2-armv7` - **not built from source** because t3.19.2's build
  script doesn't cross-compile cleanly with current cross-rs. Obtained
  instead by `extract_t3.19.2_binary.sh`, which boots the stock BlueOS
  1.4.3 docker image locally and `docker cp`s the binary out.

## B0 / B1 / B2 env-var levers consumed by `mcm-debug-armv7`

| Layer | Env var                        | Effect                                                                              |
|-------|--------------------------------|--------------------------------------------------------------------------------------|
| B0    | (always on)                    | Pad-probe on v4l2src, queue-level snapshots, rtpsession/nicesink stats - 1 Hz logs |
| B1    | `MCM_QUEUE_AT_V4L2SRC=1`       | Inserts a leaky queue between v4l2src and parser                                    |
| B1    | `MCM_QUEUE_BEFORE_PAYLOADER=1` | Inserts a leaky queue between video tee and payloader                               |
| B1    | `MCM_QUEUE_AFTER_PAYLOADER=1`  | Inserts a leaky queue between payloader and RTP tee                                 |
| B1    | `MCM_QUEUE_PER_SINK_BRANCH=1`  | Restores per-branch leaky queue on udp/zenoh sinks; skips webrtc queue excision     |
| B1    | `MCM_QUEUE_SIZING_LARGE=1`     | Switches all B1 queues to 240 buffers / 4 s default (for `e4_q_per_sink_xl`)        |
| B2    | `MCM_DISABLE_MCAP=1`           | Skips zenoh/MCAP sink construction                                                  |
| B2    | `MCM_DISABLE_THUMBNAIL=1`      | Skips image/thumbnail sink construction                                             |
| B2    | `MCM_DISABLE_UDP=1`            | Skips UDP sink construction                                                         |
| B2    | `MCM_DISABLE_WEBRTC=1`         | Rejects WebRTC `add_session` (no peer can attach)                                   |

B0 events are tagged with `mcm_inst="…"` so `rg` over `journalctl`/log
files filters cleanly:

```
rg 'mcm_inst="v4l2_drops"' /var/log/mcm-debug/<expid>/
rg 'mcm_inst="queue_level"' /var/log/mcm-debug/<expid>/
rg 'mcm_inst="rtp_stats"' /var/log/mcm-debug/<expid>/
rg 'mcm_inst="b1_queue_inserted"' /var/log/mcm-debug/<expid>/
rg 'mcm_inst="b2_sink_disabled"' /var/log/mcm-debug/<expid>/
```

## Sink decoupler selection

`MCM_UDP_DECOUPLER` picks the tee→sink decoupler topology shared by the
UDP, Zenoh, and WebRTC sinks. Default `proxy` matches the Phase-1
experiment winner.

| Value     | UDP / Zenoh / WebRTC behaviour                                                            |
|-----------|--------------------------------------------------------------------------------------------|
| `proxy`   | (default) Single decoupler: proxysrc internal queue resized to 60 buf / 1 s / leaky-down. |
| `b1`      | b1 queue (60 buf / 1 s / leaky-down) upstream of proxysink + excise proxysrc queue.       |
| `legacy`  | No b1 queue, proxysrc queue excised after first buffer (pre-experiment baseline).         |
| `appsink` | UDP only; folded to `proxy` for Zenoh and WebRTC.                                         |

Back-compat shim: when `MCM_UDP_DECOUPLER` is unset and
`MCM_QUEUE_PER_SINK_BRANCH=1`, all three sinks use the `b1` variant.

The selected variant is announced on startup:

```
rg 'mcm_inst="udp_decoupler_selected"'    /var/log/mcm-debug/<expid>/
rg 'mcm_inst="zenoh_decoupler_selected"'  /var/log/mcm-debug/<expid>/
rg 'mcm_inst="webrtc_decoupler_selected"' /var/log/mcm-debug/<expid>/
```

The WebRTC sink runs each session in a sub-pipeline
(`pipeline-webrtc-session-{session_id}`) holding `proxysrc → webrtcbin`,
mirroring the UDP / Zenoh layout. The `RED` / `FEC` / `RTX` excision
still runs on `connection-state -> Connected`, but the warm-up queue
(formerly `b1_q_webrtc_*`) is now the proxysrc internal queue and is
preserved across `Connected` for the `proxy` variant.

## Runbook orchestration

1. `install.sh` - one-shot: snapshot host state, install `dstat`/`iotop`/
   `linux-perf`/`tcpdump` on the host, `docker cp` both binaries into
   `blueos-core:/root/`, install `gstreamer1.0-tools` in-container.
   Re-run after every Kraken-driven image swap.
2. `bringup.sh <binary> <env_string> <expid>` - kills MCM in tmux, starts
   the supplied binary with the env string, fires `profile_for.sh` in
   the background.
3. `mark.sh <label>` - timestamps a label into every active log.
4. `swap_source_fake.sh` / `restore_source_usb.sh` - REST-driven source
   swap (videotestsrc-as-RTSP vs `/dev/video6`).
5. `run_minimal_ref.sh` / `run_minimal_ref_with_load.sh` - bare
   `gst-launch` pipelines to gate "is this MCM or is it driver / kernel?"
6. `teardown.sh` - kills running profilers, tars `/var/log/mcm-debug/<expid>`
   in-container, `rsync`s to local disk.
7. `cleanup.sh` - end-of-session: stop tmux, rsync, Kraken-swap to stock
   1.4.4, `apt-get purge` host installs, diff pre/post snapshots.

## Time budget

~3 h 10 m of script-driven experiment time inside the 4 h SSH window,
leaving ~50 min of margin for iteration on surprises.

## Local bench validation

`bench_validate.sh` exercises `test_fake_h264_udp_data_flow` three times
on the host (baseline, B1+B2, B2-only) and asserts each run emits the
expected `mcm_inst="…"` markers.  Run it before flashing the debug image
to the vehicle:

```
SKIP_WEB=1 bash tools/onvehicle/bench_validate.sh
```

**Known artefact of the fake/test pipeline**: running with
`MCM_QUEUE_PER_SINK_BRANCH=1` *alone* while the thumbnail (image-sink)
branch is also active causes the UDP branch to stall in the test (the
B1 queue is correctly inserted but no buffers reach the udp sink).
Combining it with `MCM_DISABLE_THUMBNAIL=1` works as expected.

This is reproducible only against `videotestsrc → x264enc → tee → tee`
in the unit-test harness; the on-vehicle pipeline (real v4l2src, real
camera capabilities, gst 1.24/1.28 with proxysink) needs to be checked
in Phase D experiment `e4_q_targeted`.  If the same stall reproduces
onboard, run with `MCM_DISABLE_THUMBNAIL=1 MCM_QUEUE_PER_SINK_BRANCH=1`
to validate the queue-restoration hypothesis in isolation.

## Lab repro: WebRTC backpressure bug (pi at 192.168.2.2)

A standalone setup that reproduces the customer's "RTSP fps drops the
moment WebRTC connects" symptom in the lab. Uses the same
`pr/juliusz-investigation` binary so we get the `mcm_inst=` markers
AND the `MCM_QUEUE_PER_SINK_BRANCH=1` A/B knob.

The pi's host kernel is aarch64 but the BlueOS `blueos-core`
container ships an armhf rootfs, so the cross target is
`armv7-unknown-linux-gnueabihf`.

Prerequisites on the dev host: `cross`, `sshpass`, `rsync`, and the
GStreamer runtime libs already needed by `cargo run --example
stream_latency` (gst-plugins-{base,good,bad,libav}). Pi must have
its eth0 reachable at `192.168.2.2` with two USB H264 cameras
plugged in.

**Empirical finding**: On this pi the bug fires deterministically the
moment any WebRTC peer reaches `connection-state=Connected`, even
without any `tc qdisc` impairment. The `impair_webrtc.sh` /
`restore_network.sh` machinery is kept for stronger impairment
scenarios (e.g. amplifying the drop, simulating a real tether) but
is not required for the basic A/B repro below.

**Empirical finding 2**: This libnice build does not expose
`min-port`/`max-port` on the NiceAgent gobject (`find_property`
returns false), so `MCM_WEBRTC_PORT_MIN`/`MCM_WEBRTC_PORT_MAX` are a
no-op at the moment. The code path still ships gated under those env
vars and emits `mcm_inst="nice_port_range_unsupported"` so it's easy
to spot when a future libnice gains the property.

The receiver side is the workspace's `stream_latency` example
(`examples/stream_latency/main.rs`), which spins up both `RtspClient`
and `WebrtcClient` simultaneously, prints per-client fps / bitrate /
jitter, and computes pairwise latency by matching VCL NAL content
hashes across transports. The WebRTC half drives MCM's actual
signalling protocol via `webrtcbin`, so no SDP patching or browser
is required.

```
# 1. cross-build armv7, push, restart MCM (bad state: no
#    MCM_QUEUE_PER_SINK_BRANCH).
tools/onvehicle/repro_lab.sh deploy

# 2. POST 2 RTSP stream configs (matches customer topology). Picks
#    the first 2 H264-capable USB cams via MCM's /v4l endpoint.
tools/onvehicle/repro_lab.sh configure-cams

# 3. Read the producer-id of lab_cam1 so the WebRTC client knows
#    which stream to subscribe to.
PID=$(sshpass -p raspberry ssh pi@192.168.2.2 \
    'curl -sS http://127.0.0.1:6020/streams' \
    | python3 -c "import json,sys; \
        print(next(s['id'] for s in json.load(sys.stdin) \
             if s['video_and_stream']['name']=='lab_cam1'))")

# 4. Run RTSP + WebRTC together against the bad MCM. Saves a per-frame
#    CSV for offline correlation; the on-screen final report shows fps,
#    jitter, and RTSP->WebRTC latency.
cargo run --release --example stream_latency -- \
    --rtsp     "rtsp://192.168.2.2:8554/lab_cam1" \
    --webrtc   "ws://192.168.2.2:6021/"            \
    --producer-id "$PID" \
    --duration 25 --warmup 3 --report-interval 5 \
    --csv /tmp/lab_repro/bad.csv

# 5. A/B: restart with MCM_QUEUE_PER_SINK_BRANCH=1 (no rebuild).
tools/onvehicle/repro_lab.sh good
cargo run --release --example stream_latency -- \
    --rtsp     "rtsp://192.168.2.2:8554/lab_cam1" \
    --webrtc   "ws://192.168.2.2:6021/"            \
    --producer-id "$PID" \
    --duration 25 --warmup 3 --report-interval 5 \
    --csv /tmp/lab_repro/good.csv

# 6. (optional) Apply tc impairment to amplify the symptom.
tools/onvehicle/impair_webrtc.sh tether
# ... repeat 4-5 ...
tools/onvehicle/restore_network.sh
```

Verification commands:

- `tools/onvehicle/repro_lab.sh status` - one-shot health dump
  (uname, tmux pane tail, /streams, tc qdisc).
- Independent tc-targeting probe (dev host) - only meaningful when
  the libnice in use honours the `MCM_WEBRTC_PORT_MIN`/`MAX` knob:
  - `iperf3 -u -c 192.168.2.2 -p 50050 -b 10M` -> should saturate
    to ~5 Mbps with elevated jitter under the `tether` profile.
  - `iperf3 -u -c 192.168.2.2 -p 50500 -b 10M` -> should run at
    line rate (out of band -> default pfifo_fast).

Bad-state signals (must observe ALL three):

1. MCM log: `Excised queue from WebRTC send path` line appears within
   ~1 s of the `stream_latency` WebRTC client connecting.
2. `mcm_inst="v4l2_drops"` with `drops_1s > 0` triggered by the
   WebRTC connect (any spike above the pre-connect baseline counts).
3. The `stream_latency` final report shows the `rtsp-0` line with a
   noticeably degraded `inter-arrival p99` / `max` (in the lab pi:
   `max` shoots up to 100ms+ versus a clean ~60ms cadence).

Good-state signals (must reverse ALL three with `MCM_QUEUE_PER_SINK_BRANCH=1`):

1. No `Excised queue from WebRTC`; instead `B1: skipping warm-up
   queue excision (MCM_QUEUE_PER_SINK_BRANCH=1)` AND `B1: removed
   pre-excision BLOCK probe on preserved warm-up queue`.
2. `v4l2_drops drops_1s` stays near the pre-connect baseline (small,
   single-digit blip rather than a sustained dump).
3. `stream_latency` `rtsp-0` inter-arrival `p99` / `max` stays near
   the 60ms cadence and per-client `fps` matches the WebRTC line.

Amplifiers if the base repro is too mild:

```
tools/onvehicle/impair_webrtc.sh aggressive
# or add CPU pressure in another shell:
sshpass -p raspberry ssh pi@192.168.2.2 \
    "docker exec blueos-core stress-ng --cpu 2 --cpu-load 60 --timeout 90s"
```

The NiceAgent port-range knob is gated by `MCM_WEBRTC_PORT_MIN` /
`MCM_WEBRTC_PORT_MAX`. It is debug-only and lives **only** on
`pr/juliusz-investigation`. The shipped fix branch
(`fix/webrtc-preserve-warmup-queue`) does not need it.
