# RTSP-to-WebRTC Latency Optimization — Journal

**Goal**: Minimize RTSP-to-WebRTC latency/jitter on Raspberry Pi 4  
**Target**: RPi 4B (4x Cortex-A72, 2GB RAM, kernel 5.10.33), BlueOS Docker (`--net=host`)  
**Camera**: RadCam at 192.168.2.10:554, 4K@30fps H.264, **32Mbps** (100Mbps link limitation)  
**Branch**: `next-2`, start commit `89edf556`  
**Methodology**: Measure → Hypothesize → Implement one change → Compare → Keep or Revert

---

## Phase 0: Baseline

### Measurement tool fixes (commit `37ac8cc0`)
- VCL-only NAL hashing for cross-path frame matching (depay/parse/pay change non-VCL NALs)
- Rewrote RTSP client using `gst::parse::launch` for correct 30fps measurement
- Camera capped to 32Mbps due to 100Mbps link bandwidth

### Corrected baseline (3 × 120s, 10s warmup)

| Metric | Run 1 | Run 2 | Run 3 | **Mean** |
|---|---|---|---|---|
| Frame loss | 2.0% | 4.1% | 7.1% | **4.4%** |
| p50 (ms) | 14.6 | 15.5 | 16.8 | **15.6** |
| p95 (ms) | 50.0 | 49.6 | 68.0 | **55.9** |
| p99 (ms) | 97.6 | 98.3 | 106.4 | **100.8** |
| max (ms) | 223.4 | 171.6 | 163.4 | **186.1** |
| WebRTC jitter | 15.3 | 17.5 | 23.0 | **18.6** |

---

## Phase 1: Data-Driven Optimizations

### 1.1 — Increase kernel UDP receive buffers ✅ KEEP

**Evidence**: 1,654 UDP RcvbufErrors per 45s under load; `rmem_max=180KB`.  
**Change**: `sysctl rmem_max=2621440 wmem_max=2621440 rmem_default=524288 wmem_default=524288`  
**Result**: Frame loss 4.4% → 0.29% (-93%), p99 100.8 → 76.9ms (-24%).

### 1.2 — Set udp-buffer-size=2.5MB on rtspsrc ✅ KEEP (commit `fe1fbd59`)

**Evidence**: MCM's rtspsrc socket had 21,888 total drops, rx_queue maxed at 1MB.  
**Change**: Added `udp-buffer-size=2621440` to rtspsrc in onvif_pipeline, redirect_pipeline.  
**Result**: Socket drops 21,888 → **0**. Makes MCM robust regardless of sysctl settings.

### 1.3 — Disable clocksync in webrtcbin send path ✅ KEEP (commit `5b1c12d6`)

**Evidence**: clocksync elements inside webrtcbin pace buffers to pipeline clock.  
**Change**: Set `sync=false` on clocksync elements in `optimise_webrtcbin_send_path`.  
**Result**: **Biggest single win** — p50 15.3 → 10.2ms (-33%), p95 49.1 → 15.5ms (-68%).

### 1.4 — Enable RPS on eth0 ❌ REVERTED

**Hypothesis**: Spread softirq across CPUs to reduce contention on CPU0.  
**Result**: WORSE — single-flow RPS adds IPI overhead. p50 10.2 → 11.9ms. Reverted.

---

## Analysis: Remaining Latency

### Per-frame-type latency (from CSV analysis)

| | P-frames (133KB avg) | I-frames (337KB avg) |
|---|---|---|
| Count per 120s | ~3300 | ~60 |
| p50 latency | **10.0ms** | 27.7ms |
| p95 latency | 13.1ms | 34.9ms |
| p99 latency | 17.9ms | — |

**Key insight**: Latency scales linearly with frame size (2.5× bigger = 2.8× latency).  
The overall p95/p99 tail is dominated by keyframe processing, not system jitter.

### 10ms P-frame breakdown (estimated)

| Component | Time |
|---|---|
| rtspsrc jitter buffer + depay | ~1ms |
| queue + h264parse + capsfilter + tee | ~1ms |
| rtph264pay packetization | ~0.5ms |
| webrtcbin DTLS-SRTP encryption (ARM32) | ~3ms |
| WebRTC queue + network send | ~1ms |
| Client DTLS decrypt + jitter buffer | ~3.5ms |

### System observations
- AES-128-GCM: 62MB/s on ARM32; ChaCha20: 199MB/s (3.2× faster, unused)
- `OPENSSL_armcap=0x1` — no hardware AES on armv7 (32-bit mode limitation)
- All GStreamer threads at SCHED_RR 99, perfectly isolated from CPU contention
- Network RTT: camera 2.2ms, RPi 1.5ms — extra hop adds ~0.15ms

---

## Phase 5: Stress Testing

### 5.1 — Multiple simultaneous WebRTC clients

| Metric | 1 client | 2 clients |
|---|---|---|
| fps (each) | 30.6 | 30.4–30.5 |
| Frame loss | 0.2% | 0.5–0.8% |
| p50 | 10.2ms | 10.8ms |
| p95 | 15.5ms | 16.8–18.5ms |

Both clients maintain full 30fps. Latency increase is marginal.

### 5.2 — CPU stress (stress-ng)

| Metric | No stress | 2 cores | 3 cores | 4 cores |
|---|---|---|---|---|
| fps | 30.6 | 30.5 | 30.5 | 30.6 |
| Frame loss | 0.2% | 0.39% | 0.42% | 0.30% |
| p50 | 10.2ms | 10.3ms | 10.4ms | 10.3ms |
| p95 | 15.5ms | 15.7ms | 15.8ms | 15.7ms |
| p99 | 30.9ms | 34.3ms | 38.1ms | 30.6ms |

**Conclusion**: Pipeline is fully isolated from CPU contention via SCHED_RR 99.  
Even with all 4 cores under maximum stress, performance is **unchanged**.

### 5.3 — Long-duration stability (10 minutes continuous)

| Metric | Value |
|---|---|
| Duration | 600s |
| Total RTSP frames | 18,085 |
| Total WebRTC frames | 18,039 |
| Frame loss | 46 frames (0.25%) |
| fps | 30.6 (both) |
| p50 | 10.3ms |
| p95 | 16.1ms |
| p99 | 33.2ms |
| max | 154.5ms |
| Match rate | 100% |

**No degradation over time.** No memory leaks, no resource accumulation.

---

## Summary: Before and After

| Metric | Before | After | Improvement |
|---|---|---|---|
| **Frame loss** | **4.4%** | **0.2%** | **-95%** |
| **p50** | **15.6ms** | **10.2ms** | **-35%** |
| **p95** | **55.9ms** | **15.5ms** | **-72%** |
| **p99** | **100.8ms** | **30.9ms** | **-69%** |
| **max** | **186.1ms** | **119.9ms** | **-36%** |
| **Jitter** | **18.6ms** | **14.5ms** | **-22%** |
| CPU stress resilience | untested | all 4 cores: no impact | robust |
| Long-term stability | untested | 10 min: 0.25% loss | stable |

### Changes (3 commits)

1. **`fe1fbd59`** — `udp-buffer-size=2621440` on rtspsrc (eliminates socket drops)
2. **`5b1c12d6`** — Disable clocksync pacing in webrtcbin send path (biggest latency win)
3. **`548df25f`** — Multi-WebRTC support in measurement tool

### Required runtime sysctl

```bash
sysctl -w net.core.rmem_max=2621440 net.core.wmem_max=2621440 \
       net.core.rmem_default=524288 net.core.wmem_default=524288
```

### Further improvements (diminishing returns)

- **aarch64 migration**: Would enable hardware AES, potentially saving ~2ms/frame on DTLS
- **ChaCha20-Poly1305 for SRTP**: 3.2× faster than AES on ARM32 (needs GnuTLS config)
- **Reduce camera keyframe frequency**: Currently 2/sec; less frequent IDRs reduce tail
- **64-bit Docker image**: Would allow OPENSSL_armcap=0x7 (NEON + PMULL + AES)

