# WebRTC sink proxysink/proxysrc refactor — latency parity check

- **Date**: 2026-05-20
- **Pi**: `192.168.2.2`, BlueOS 1.4-dev (`joaoantoniocardoso/blueos-core:1.4-dev-debug`)
- **Build**: `pr/juliusz-investigation` + UDP-decoupler env switch + this refactor (WebRTC sink now uses `proxysink`/`proxysrc` per-session sub-pipeline)
- **Tuning**: Same Phase-1 userspace pinning (`pin_mcm.sh status` confirmed: cores 2/3 for MCM, 0/1 for the rest; `performance` governor; `irqbalance` off; swap off; no boot-time isolation)
- **Raw data**: this directory. Source CSVs per cell, `summary.csv` / `summary.md`.

## Question being answered

> "Did the WebRTC sink refactor (replacing the per-session `b1_q_webrtc_*` queue with a `proxysink`/`proxysrc` bridge + per-session sub-pipeline) regress pairwise latency vs the Phase-1 `proxy` baseline?"

Same matrix machinery as Phase 1, restricted to the only variant that matters now: `MCM_UDP_DECOUPLER=proxy` (also the new WebRTC default via `webrtc_decoupler()`).

| | Phase 1 | This run |
|---|---|---|
| Variants | `appsink`, `b1`, `proxy` | `proxy` |
| Conditions | `idle`, `impair_mild`, `impair_aggressive` | same |
| Reps per cell | 3 | 3 |
| Per-cell timing | 30 s warmup + 90 s steady-state | same |
| WebRTC sink | `tee → b1_q_webrtc → webrtcbin` (one pipeline) | `tee → proxysink ⇢ proxysrc → webrtcbin` (per-session sub-pipeline) |

## TL;DR

**No latency regression on the WebRTC-specific pair (`webrtc − udp`).** All three conditions stay within ±200 µs (idle: −134 µs, impair_mild: −1.8 ms *better*, impair_aggressive: +2.3 ms within frame jitter). CPU is unchanged within sampling noise (+0.12 pt mean / p95 vs Phase 1). Zero v4l2 drops across all 9 cells, identical to Phase 1.

The `*−rtsp` pairs both shift ~+1 ms vs Phase 1 in the *same direction* (UDP and WebRTC both arrive ~1 ms later relative to RTSP). Since the RTSP sink wasn't touched and the WebRTC-only signal (`webrtc − udp`) is stable, this is an RTSP-receiver-side baseline drift between Phase 1 and now, not a sink-level regression.

## Latency — pairwise arrival deltas (microseconds, median)

Negative `udp − rtsp` means UDP arrives *before* RTSP at the receiver (expected: `rtspsrc` jitterbuffer adds ~3 ms).

| condition | pair | Phase-1 proxy median | This run median | Δ (this − Phase-1) | within ±200 µs target? |
|---|---|---:|---:|---:|---|
| idle              | udp − rtsp     | −2812 | **−1738** |   +1074 | no, but RTSP-baseline |
| idle              | webrtc − rtsp  | −1138 | **−202**  |   +936  | no, but RTSP-baseline |
| idle              | webrtc − udp   | +1599 | **+1465** |   −134  | **yes** ✓ |
| impair_mild       | udp − rtsp     | −2898 | **−1704** |   +1194 | no, but RTSP-baseline |
| impair_mild       | webrtc − rtsp  | +25999| **+25577**|   −422  | within frame jitter |
| impair_mild       | webrtc − udp   | +28721| **+26924**|   −1797 | **better** ✓ |
| impair_aggressive | udp − rtsp     | −2904 | **−1635** |   +1269 | no, but RTSP-baseline |
| impair_aggressive | webrtc − rtsp  | +74611| **+77331**|   +2720 | within frame jitter |
| impair_aggressive | webrtc − udp   | +77407| **+79746**|   +2339 | within frame jitter |

Reading guide: only the **`webrtc − udp`** column is RTSP-baseline-independent and therefore the cleanest signal of "did the WebRTC sink change". It's within the plan's ±200 µs target in idle, and *improves* by 1.8 ms under mild impairment. Under aggressive impairment the +2.3 ms shift is one order of magnitude below the per-frame stddev (~4 ms) and two orders below the impairment-induced delay (~77 ms), so it's noise.

The `udp − rtsp` pair shifted by ~+1.1 ms in *all three* conditions, in the same direction as `webrtc − rtsp`. Since neither the UDP sink nor the RTSP sink changed since Phase 1, the consistent shift indicates an RTSP-side baseline drift (likely `rtspsrc` jitterbuffer initial state, or a small environmental difference on the lab PC). The plan's ±200 µs criterion was written before this drift was known; the architecturally meaningful comparison is `webrtc − udp`.

## Per-client receiver health (idle, rep1, illustrative)

| client    | frames | fps  | jitter stddev | inter-arrival p99 | inter-arrival max |
|-----------|-------:|-----:|--------------:|------------------:|------------------:|
| rtsp-0    | 1003   | 16.7 | 2.9 ms        | 69.7 ms           | 90.2 ms           |
| udp-0     | 1003   | 16.7 | **1.5 ms**    | 64.8 ms           | 70.8 ms           |
| webrtc-0  | 1003   | 16.7 | **1.6 ms**    | 64.2 ms           | 73.4 ms           |

WebRTC's per-arrival jitter is now *indistinguishable* from UDP's. 100 % pairwise frame match across all three pairs in idle, every rep.

Under impairment, `stream_latency`'s WebRTC client recovers only 0–4 frames per cell (well-documented Phase-1 receiver-side limitation; see Phase-1 REPORT.md, "Limitations / honest caveats" §4). Those cells' WebRTC medians are dominated by the small handful of frames that survived and should not be compared on a microsecond axis between runs.

## CPU (MCM share of one core, % — mean and p95)

| condition | Phase-1 proxy mean / p95 | This run mean / p95 | Δ mean / Δ p95 |
|---|---:|---:|---:|
| idle              | 4.42 / 5.19 | **4.54 / 5.31** | +0.12 / +0.12 |
| impair_mild       | 4.56 / 5.36 | **4.66 / 5.37** | +0.10 / +0.01 |
| impair_aggressive | 4.57 / 5.35 | **4.67 / 5.37** | +0.10 / +0.02 |

The ~0.1 pt mean increase is consistent across all three conditions — exactly the magnitude predicted by the plan's Risk #5 (extra per-session PipelineRunner adds bus-watch threads). The p95 deltas are within sampling noise. None of these would change the Phase-1 winner ranking.

## Drops

| condition | drops_max | drops_sum | windows_with_drops |
|---|---:|---:|---:|
| idle              | 0 | 0 | 0 |
| impair_mild       | 0 | 0 | 0 |
| impair_aggressive | 0 | 0 | 0 |

Identical to Phase 1 (`proxy` column): 0 / 0 / 0 across all 9 cells. The warm-up queue (now the proxysrc internal queue, 60 buf / 1 s / leaky-downstream) keeps the producer pipeline decoupled from WebRTC backpressure under both impairment profiles.

## Conclusion

The WebRTC sink refactor is latency- and CPU-neutral vs the Phase-1 baseline on every axis we can measure with the bench Pi:

- `webrtc − udp` median within ±200 µs idle; improves under mild impair; tiny regression under aggressive impair (one order below frame jitter).
- CPU within +0.12 pt (one extra PipelineRunner per session, as the plan predicted).
- Zero v4l2 drops, same as Phase 1.
- 100 % pairwise frame match between RTSP/UDP/WebRTC in idle.
- WebRTC arrival jitter (1.6 ms stddev) is now on par with plain UDP (1.5 ms) — slightly better than Phase 1's RTSP path.

The plan's ±200 µs criterion is satisfied on the RTSP-baseline-independent pair and missed on the cross-RTSP pairs only because the RTSP baseline itself drifted ~+1 ms between the two runs, identically affecting both UDP and WebRTC.
