# Benchmark Reference Data

Historical benchmark results for comparison. When running new benchmarks,
compare against these baselines to detect regressions or confirm improvements.

## Platform

- Raspberry Pi 4 (armv7), `blueos-core-ab-test` Docker container
- 3 pipelines: USB H264 (1080p30), ONVIF RTSP (4K30), videotestsrc (160x120@5fps)
- ~8 active GStreamer pipelines, ~40 probed pads total
- `nice --19`, `GST_DEBUG=2`, release build without LTO

## Round 2 (2026-02-17, commit cd469171)

### Probes Only (POLL_MODE=none)

| Level | CPU% mean | CPU% std | RSS (KB) |
|-------|----------:|---------:|---------:|
| off   |     55.77 |    18.56 |  219,936 |
| lite  |     58.00 |    18.38 |  221,515 |
| full  |     58.08 |    17.26 |  225,311 |

Overhead vs off: lite +2.23% CPU (+4.0% rel), full +2.31% CPU (+4.1% rel)

### Probes + Summary Snapshots (POLL_MODE=http-summary)

| Level | CPU% mean | CPU% std | RSS (KB) | FPS   |
|-------|----------:|---------:|---------:|------:|
| off   |     56.15 |    20.03 |  221,581 |   n/a |
| lite  |     58.51 |    14.82 |  224,109 | 93.16 |
| full  |     66.37 |    13.67 |  230,624 | 74.99 |

Overhead vs off: lite +2.36% CPU (+4.2% rel), full +10.22% CPU (+18.2% rel)

### Full-Snapshot HTTP vs WebSocket (full level only)

| Delivery    | CPU% mean | RSS (KB) | Delta CPU | Delta RSS  |
|-------------|----------:|---------:|----------:|-----------:|
| none        |     58.08 |  225,311 |       --- |        --- |
| http-summary|     66.37 |  230,624 |    +8.29% |  +5,313 KB |
| http-full   |     68.37 |  236,934 |   +10.29% | +11,623 KB |
| ws-full     |     67.91 |  229,379 |    +9.83% |  +4,068 KB |

WS saves ~7.5 MB RSS vs HTTP with ~0.5% less CPU.

## Round 1 (2026-02-17, commit c665ea31)

### Probes Only (POLL_MODE=none / POLL_API=0)

| Level | CPU% mean | CPU% std | RSS (KB) |
|-------|----------:|---------:|---------:|
| off   |     56.76 |    19.25 |  221,278 |
| lite  |     58.56 |    19.03 |  220,011 |
| full  |     58.15 |    19.70 |  223,288 |

Overhead vs off: lite +1.80% CPU (+3.2% rel), full +1.39% CPU (+2.4% rel)

### Probes + Summary Snapshots (POLL_API=1)

| Level | CPU% mean | CPU% std | RSS (KB) | FPS   |
|-------|----------:|---------:|---------:|------:|
| off   |     56.97 |    19.84 |  223,394 |   n/a |
| lite  |     59.92 |    15.44 |  223,087 | 92.08 |
| full  |     66.87 |    13.12 |  231,237 | 75.07 |

Overhead vs off: lite +2.95% CPU (+5.2% rel), full +9.90% CPU (+17.4% rel)

## Cost Breakdown Summary (Round 2)

| Component                        | lite CPU | full CPU | full RSS   |
|----------------------------------|--------:|---------:|-----------:|
| Probes + 1 Hz background sampler |  +2.23% |   +2.31% |  +5,374 KB |
| Snapshot computation (1/s poll)  |  +0.13% |   +7.91% |  +3,669 KB |
| **Total (http-summary)**         |**+2.36%**|**+10.22%**|**+9,043 KB**|

## Round 9 (2026-02-18, commit 146b94f2) -- Feature-Gate Variants

Note: 1 pipeline active (USB H264 only), lower baseline than rounds 1-8.

### Probes Only (POLL_MODE=none, default variant)

| Level | CPU% mean | CPU% std | RSS (KB) |
|-------|----------:|---------:|---------:|
| off   |     14.68 |     5.71 |   90,201 |
| lite  |     15.17 |     6.35 |   92,373 |
| full  |     15.23 |     6.49 |   90,907 |

Overhead vs off: lite +0.49% CPU (+3.3% rel), full +0.55% CPU (+3.7% rel)

### API Delivery (default variant, full level)

| Delivery | CPU% mean | RSS (KB) | Delta CPU | Delta RSS |
|----------|----------:|---------:|----------:|----------:|
| none     |     15.23 |   90,907 |       --- |       --- |
| http-full|     16.25 |   93,336 |    +1.02% | +2,429 KB |
| ws-full  |     16.12 |   93,044 |    +0.89% | +2,137 KB |

### Feature-Gate Variants (full level)

#### POLL_MODE=none

| Variant | CPU% mean | RSS (KB) | Delta CPU vs default | Delta RSS |
|---------|----------:|---------:|---------------------:|----------:|
| default |     15.23 |   90,907 |                  --- |       --- |
| pad-caps |     15.10 |   92,867 |              -0.13% | +1,960 KB |
| element-deep-info | 15.23 | 93,252 |          +0.00% | +2,345 KB |
| all-features | 15.18 | 93,641 |              -0.05% | +2,734 KB |

All feature variants: zero measurable CPU overhead without API clients.

#### POLL_MODE=ws-full

| Variant | CPU% mean | RSS (KB) | Delta CPU vs default | Delta RSS |
|---------|----------:|---------:|---------------------:|----------:|
| default |     16.12 |   93,044 |                  --- |       --- |
| pad-caps |     16.13 |   93,518 |              +0.01% |   +474 KB |
| element-deep-info | 107.75 | 286,166 |       +91.63% | +193,122 KB |
| all-features | 83.70 | 205,104 |             +67.58% | +112,060 KB |

**element-deep-info causes massive overhead under API load** (serialization cost).
pad-caps has zero overhead in all configurations.

### Round 10 (property deny-list + topology fixes)

Workload: 1 USB H264 camera (same as round 9).

Changes: Added `PROPERTY_DENY_LIST` filtering out `last-sample` (16.6 MB raw
frame data), GObject pointer references, and debug messages from element-deep-info.
Also fixed topology: bidirectional pad connections, thread connections, pipeline
connections, tee dynamic src pads.

#### Feature-Gate Variants (full level, POLL_MODE=ws-full)

| Variant | CPU% mean | RSS (KB) | Delta CPU vs default | Delta RSS |
|---------|----------:|---------:|---------------------:|----------:|
| default |     16.02 |   91,824 |                  --- |       --- |
| pad-caps |     15.98 |   90,989 |              -0.04% |   -835 KB |
| element-deep-info | 15.95 | 91,042 |          -0.07% |   -782 KB |
| all-features | 16.37 | 93,298 |              +0.35% | +1,474 KB |

**Property deny-list fixed element-deep-info completely**: from +91.63% CPU to
-0.07% CPU, from +193 MB RSS to -782 KB. All features now production-ready.

## Round 11 (2026-02-22, commit 146b94f2 + uncommitted) -- cpu_stats + Processing Time

Workload: 1 USB H264 camera (same as rounds 9-10).

Changes: Added per-element windowed CPU statistics (`ElementStats.cpu_stats`),
PTS-matched causal processing time (`processing_time_stats`, `is_cross_thread`),
and lightweight `cpu_attribution_hint()` called at 1 Hz in `poll_thread_cpu()`.

### Probes Only (POLL_MODE=none, default variant)

| Level | CPU% mean | CPU% std | RSS (KB) |
|-------|----------:|---------:|---------:|
| off   |     14.76 |     5.73 |   91,741 |
| lite  |     14.88 |     6.00 |   93,087 |
| full  |     15.06 |     6.29 |   93,739 |

Overhead vs off: lite +0.12% CPU (+0.8% rel), full +0.30% CPU (+2.0% rel)

### API Delivery (default variant, full level)

| Delivery | CPU% mean | RSS (KB) | Delta CPU | Delta RSS |
|----------|----------:|---------:|----------:|----------:|
| none     |     15.06 |   93,739 |       --- |       --- |
| http-full|     16.32 |   95,179 |    +1.26% | +1,440 KB |
| ws-full  |     16.16 |   93,485 |    +1.10% |   -254 KB |

**New features have zero measurable CPU overhead.** Probe overhead (+0.30%)
and API delivery overhead (~1%) are consistent with rounds 9-10.

## Key Thresholds

When comparing new results, flag if:
- Probe-only overhead (lite or full) exceeds +5% CPU
- Full snapshot computation exceeds +15% CPU
- Total RSS overhead (full + http/ws) exceeds +15 MB
- WebSocket uses more memory than HTTP (regression)
- Feature-gate variant (any) exceeds +3% CPU under WebSocket load
