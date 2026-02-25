# Pipeline Analysis Level -- Performance Benchmark

Date: 2026-02-22 (round 11 -- cpu_stats + processing time improvements)
Platform: Raspberry Pi 4 (armv7), inside `blueos-core-ab-test` Docker container
Binary: `mavlink-camera-manager` (JSON-only API, release build without LTO)

## Test Setup

### Workload

Camera pipelines running via `--default-settings BlueROVUDP`:

| Pipeline | Source | Resolution | FPS |
|----------|--------|-----------|-----|
| USB H264 camera | `/dev/video2` (v4l2src) | 1920x1080 | 30 |

Each pipeline also has UDP sink, RTSP sink, and thumbnail (image-sink) sub-pipelines.

Note: This round ran with fewer active pipelines than previous rounds (1 USB camera
vs 3 pipelines in rounds 1-8). The lower baseline CPU (~15% vs ~56%) reflects this.
Relative comparisons within this round remain valid.

### MCM Command Line

```
env GST_DEBUG=2 nice --19 /root/mavlink-camera-manager \
  --default-settings BlueROVUDP \
  --mavlink udpin:127.0.0.1:5777 \
  --mavlink-system-id 1 \
  --mavlink-camera-component-id-range=100-105 \
  --gst-feature-rank omxh264enc=0,v4l2h264enc=250,x264enc=260 \
  --log-path /var/logs/blueos/services/mavlink-camera-manager \
  --stun-server stun://stun.l.google.com:19302 \
  --verbose \
  --pipeline-analysis-level <off|lite|full>
```

### Methodology

- **Architecture**: Split client/server. MCM + `/proc` measurement on Pi.
  Polling clients (HTTP curl, WebSocket) run on the **host dev machine**
  connecting over the network. This avoids client CPU contending with MCM on the Pi.
- **CPU isolation**: Before benchmarking, the host orchestrator configures the Pi via SSH:
  - CPU governor pinned to `performance` on all 4 cores
  - CPU frequency locked to 1500 MHz (min=max=1500000 kHz) on all 4 cores
  - Docker container pinned to CPUs 1,2,3 (`--cpuset-cpus=1,2,3`)
  - All OS userspace processes moved to CPU 0 via `taskset`
  - All hardware IRQs pinned to CPU 0 via `/proc/irq/*/smp_affinity_list`
  - Setup is validated (governor, frequency, cpuset) before sampling; aborts on mismatch
  - After benchmarking, governor is restored to `ondemand` and cpuset pin is removed
- **Warmup**: 30 seconds after MCM start (pipelines stabilize).
- **Sampling**: 60 one-second samples per level.
- **Cooldown**: 15 seconds between levels (V4L2 device release).
- **CPU measurement**: Delta of `(utime + stime)` from `/proc/<pid>/stat`, divided by
  elapsed wall time and `CLK_TCK` (100). Reports total process CPU across all cores
  (e.g., 100% = 1 full core). With cpuset=1,2,3, MCM has 3 dedicated cores (300% max).
- **Memory measurement**: `VmRSS` from `/proc/<pid>/status`.
- **Feature-gate variants**: Since `pad-caps` and `element-deep-info` are compile-time
  Cargo features, a separate binary was built for each variant. The `VARIANT_LABEL`
  env var labels results to prevent overwriting between runs.

The benchmark uses `scripts/benchmark-host.sh` (host-side orchestrator) and
`scripts/benchmark-pipeline-analysis.sh` (Pi-side `/proc` sampler).

**POLL_MODE** controls which endpoint is polled from the host:

| POLL_MODE | Description | Host client | MCM endpoint | Wire format |
|-----------|-------------|-------------|--------------|-------------|
| `none` | No API calls. Probes + 1 Hz background sampler only. | (none) | (none) | -- |
| `http-full` | HTTP full snapshot 1/s | `curl` loop | `GET /stats/streams/snapshot` | JSON |
| `ws-full` | WebSocket JSON text frames | `websocket-client` | `WS .../ws?interval_ms=1000` | JSON |

### Methodology history

**Rounds 1-4**: All polling clients ran **on the Pi** inside the Docker container.
This contaminated measurements: client CPU competed with MCM for the same cores.

**Round 5**: Fixed by running all clients on the host machine over the network.

**Rounds 6-8**: Various transport experiments (Zenoh, MessagePack, Bincode).
Concluded that JSON over HTTP/WebSocket is sufficient; Zenoh and binary formats removed.

**Round 9**: JSON-only API (HTTP + WebSocket). Added **feature-gate variant
testing** to measure the incremental cost of the `pad-caps` and `element-deep-info`
compile-time features. Four binary variants were tested: default, pad-caps,
element-deep-info, and all-features (both enabled). **Critical finding**: the
`element-deep-info` feature caused a ~6x CPU spike due to serializing heavy
GObject properties (notably `last-sample` which contains entire raw video frames
as 16 MB+ hex strings).

**Round 10**: Added a **property deny-list** filtering out heavyweight
and useless GObject properties from `element-deep-info` snapshots (e.g.
`last-sample`, GObject pointer references, debug messages). Also includes
topology fixes (bidirectional pad connections, thread connections, pipeline
connections, tee dynamic src pads).

**Round 11** (current): Added **per-element windowed CPU statistics**
(`ElementStats.cpu_stats`), **PTS-matched causal processing time**
(`processing_time_stats`, `is_cross_thread`), and lightweight
`cpu_attribution_hint()` called at 1 Hz in `poll_thread_cpu()`. These changes
add work to the 1 Hz background sampler path.

---

## Results (Round 11 -- cpu_stats + Processing Time Improvements)

Commit: `146b94f2` + cpu_stats/processing_time patches (uncommitted)

### Phase 1: off / lite / full Comparison (POLL_MODE=none, default variant)

| Level | CPU% mean | CPU% std | RSS (KB) | Load avg |
|-------|----------:|---------:|---------:|---------:|
| off   |     14.76 |     5.73 |   91,741 |     1.94 |
| lite  |     14.88 |     6.00 |   93,087 |     1.31 |
| full  |     15.06 |     6.29 |   93,739 |     1.39 |

#### Probe overhead vs `off`

| Metric         | `lite`    | `full`    |
|----------------|----------:|----------:|
| CPU (absolute) | +0.12%    | +0.30%   |
| CPU (relative) | +0.8%     | +2.0%    |
| RSS            | +1,347 KB | +1,998 KB |

Probe overhead is within normal range, consistent with rounds 9-10.

### Phase 2: API Delivery Overhead (default variant, full level)

Baseline is none/full (CPU 15.06%, RSS 93,739 KB).

| Delivery | CPU% mean | CPU% std | RSS (KB) | Delta CPU vs none | Delta RSS vs none |
|----------|----------:|---------:|---------:|------------------:|------------------:|
| none     |     15.06 |     6.29 |   93,739 |                -- |                -- |
| http-full|     16.32 |     6.05 |   95,179 |           +1.26%  |        +1,440 KB  |
| ws-full  |     16.16 |     5.92 |   93,485 |           +1.10%  |          -254 KB  |

HTTP and WebSocket add ~1% CPU overhead at 1 Hz polling. Consistent with rounds 9-10.

### Phase 3: Feature-Gate Variant Overhead (POLL_MODE=none, full level) -- from Round 10

Feature-gate variants were not re-tested in round 11 (no changes to feature-gated
code paths). Data below is from round 10 and remains the current baseline.

All variants use `--pipeline-analysis-level full`. No API polling -- measures
pure probe + background sampler + GStreamer introspection cost.

| Variant | Cargo features | CPU% mean | CPU% std | RSS (KB) | RSS std |
|---------|----------------|----------:|---------:|---------:|--------:|
| default | (none) | 15.28 | 6.28 | 90,773 | 617 |
| pad-caps | `pad-caps` | 15.20 | 6.05 | 92,812 | 1,494 |
| element-deep-info | `element-deep-info` | 15.24 | 5.50 | 93,333 | 1,148 |
| all-features | both | 15.06 | 5.86 | 92,948 | 868 |

#### Feature overhead vs default (POLL_MODE=none)

| Variant | Delta CPU | Delta RSS |
|---------|----------:|----------:|
| pad-caps | -0.08% | +2,039 KB |
| element-deep-info | -0.04% | +2,560 KB |
| all-features | -0.22% | +2,175 KB |

**With no API polling, all feature variants have zero measurable CPU overhead.**
Consistent with round 9 -- the 1 Hz GStreamer introspection is negligible.

### Phase 4: Feature-Gate Variant Overhead (POLL_MODE=ws-full, full level) -- from Round 10

All variants use `--pipeline-analysis-level full` with a WebSocket client polling
at 1 Hz. This measures the cost of serializing and transmitting the enriched snapshots.

| Variant | Cargo features | CPU% mean | CPU% std | RSS (KB) | RSS std |
|---------|----------------|----------:|---------:|---------:|--------:|
| default | (none) | 16.02 | 5.83 | 91,824 | 1,045 |
| pad-caps | `pad-caps` | 15.98 | 5.74 | 90,989 | 744 |
| element-deep-info | `element-deep-info` | 15.95 | 5.11 | 91,042 | 73 |
| all-features | both | 16.37 | 5.91 | 93,298 | 705 |

#### Feature overhead vs default (POLL_MODE=ws-full)

| Variant | Delta CPU | Delta RSS |
|---------|----------:|----------:|
| pad-caps | -0.04% | -835 KB |
| element-deep-info | -0.07% | -782 KB |
| all-features | +0.35% | +1,474 KB |

**KEY RESULT**: After adding the property deny-list, `element-deep-info` now has
**zero measurable overhead** under WebSocket load -- a dramatic improvement from
round 9 where it caused +91.6% CPU and +193 MB RSS.

The deny-list filters out:
- `last-sample` (raw video frame data, up to 16.6 MB per element per snapshot)
- GObject pointer references (`proxysink`, `socket`, `used-socket`, etc.)
- Debug messages (`last-message`)
- Identity fields already in the snapshot structure (`name`, `parent`)

---

## Analysis

### All features are production-ready

With the property deny-list in place, all feature combinations show zero
measurable CPU overhead in all configurations:

| Configuration | Overhead |
|---------------|----------|
| `pad-caps` only | ~0% CPU, ~0% RSS |
| `element-deep-info` only | ~0% CPU, ~0% RSS |
| Both features | +0.35% CPU (within noise) |

### Property deny-list was the critical fix

Round 9 showed that `element-deep-info` caused a ~6x CPU spike when serving
snapshots. The root cause was the `last-sample` GObject property on sink elements
(especially `AppSink`) which serialized the **entire raw video frame** (16.6 MB
RGBA hex) into the JSON payload on every snapshot. With the deny-list filtering
these out, the overhead dropped from +91.6% to ~0%.

### New cpu_stats and processing_time features have zero overhead

Round 11 added per-element windowed CPU history (`cpu_stats`), PTS-matched
causal processing time (`processing_time_stats`), and cross-thread detection
(`is_cross_thread`). The `cpu_attribution_hint()` runs at 1 Hz alongside the
existing `poll_thread_cpu()` path.

Probe overhead: +0.30% absolute CPU for full level, consistent with round 10
(+0.35%). The new features add no measurable overhead.

### Probe hot-path cost remains low

Probe overhead is +0.30% absolute CPU for this workload (1 USB pipeline).
Consistent with rounds 9-10 within measurement noise.

### HTTP and WebSocket are both efficient

Both add ~1% CPU overhead at 1 Hz polling, consistent across rounds 7-11.

---

## Comparison with Previous Rounds

### Probe overhead (POLL_MODE=none, vs off baseline in same run)

| Metric | Round 7 | Round 8 | Round 9 | Round 10 | Round 11 (current) |
|--------|--------:|--------:|--------:|---------:|-------------------:|
| lite CPU (absolute) | +2.23% | +0.90% | +0.49% | +0.13% | +0.12% |
| full CPU (absolute) | +2.31% | +2.44% | +0.55% | +0.35% | +0.30% |

Lower absolute values in rounds 9-11 reflect the lighter workload (1 pipeline vs 3).

### Transport overhead (vs full-no-client baseline in same run)

| Transport | Round 7 | Round 8 | Round 9 | Round 10 | Round 11 (current) |
|-----------|--------:|--------:|--------:|---------:|-------------------:|
| HTTP (JSON) | +0.94% | +2.67% | +1.02% | +1.05% | +1.26% |
| WS (JSON) | +1.80% | +1.33% | +0.89% | +0.74% | +1.10% |

HTTP and WebSocket overhead remains in the ~1% range across rounds.

### element-deep-info overhead under WebSocket load

| Metric | Round 9 | Round 10 |
|--------|--------:|---------:|
| CPU delta | +91.63% | -0.07% |
| RSS delta | +193,122 KB | -782 KB |

The property deny-list eliminated the serialization bottleneck entirely.

---

## Recommendations

1. **`full` probes are safe for always-on production use.** Overhead is negligible.

2. **Enable `pad-caps` in production.** Zero measurable overhead. Provides
   useful caps information for debugging pipeline issues.

3. **Enable `element-deep-info` in production.** With the property deny-list,
   overhead is now zero. Provides element state, queue fill levels, and filtered
   GObject properties. Keeping it behind a feature gate is no longer necessary
   for performance reasons (but may still be useful for binary size).

4. **Use WebSocket for continuous monitoring dashboards.** Adds ~1% CPU at 1 Hz.

5. **Use HTTP for occasional spot-checks.** Similar overhead to WebSocket.

## Reproducing

The benchmark uses a split architecture (see [SKILL.md](../.cursor/skills/benchmark-pipeline-analysis/SKILL.md)):

```bash
# Cross-compile (default variant)
SKIP_WEB=1 cross build --release --target armv7-unknown-linux-gnueabihf

# Cross-compile with features
SKIP_WEB=1 cross build --release --target armv7-unknown-linux-gnueabihf --features pad-caps
SKIP_WEB=1 cross build --release --target armv7-unknown-linux-gnueabihf --features element-deep-info
SKIP_WEB=1 cross build --release --target armv7-unknown-linux-gnueabihf --features pad-caps,element-deep-info

# Deploy binary to Pi
sshpass -p raspberry scp target/armv7-unknown-linux-gnueabihf/release/mavlink-camera-manager pi@192.168.2.2:/tmp/mcm_bench
sshpass -p raspberry ssh pi@192.168.2.2 'docker cp /tmp/mcm_bench blueos-core-ab-test:/root/mavlink-camera-manager && docker exec blueos-core-ab-test chmod +x /root/mavlink-camera-manager'

# Run benchmarks from host machine:
VARIANT_LABEL=default           POLL_MODE=none      ./scripts/benchmark-host.sh "off lite full"
VARIANT_LABEL=default           POLL_MODE=http-full ./scripts/benchmark-host.sh full
VARIANT_LABEL=default           POLL_MODE=ws-full   ./scripts/benchmark-host.sh full
VARIANT_LABEL=pad-caps          POLL_MODE=none      ./scripts/benchmark-host.sh full
VARIANT_LABEL=pad-caps          POLL_MODE=ws-full   ./scripts/benchmark-host.sh full
VARIANT_LABEL=element-deep-info POLL_MODE=none      ./scripts/benchmark-host.sh full
VARIANT_LABEL=element-deep-info POLL_MODE=ws-full   ./scripts/benchmark-host.sh full
VARIANT_LABEL=all-features      POLL_MODE=none      ./scripts/benchmark-host.sh full
VARIANT_LABEL=all-features      POLL_MODE=ws-full   ./scripts/benchmark-host.sh full
```
