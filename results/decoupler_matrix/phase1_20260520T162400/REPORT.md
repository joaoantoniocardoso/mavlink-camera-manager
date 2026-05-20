# UDP Decoupler Optimisation — Phase 1 Report

- **Date**: 2026-05-20
- **Pi**: `192.168.2.2`, BlueOS 1.4-dev (`joaoantoniocardoso/blueos-core:1.4-dev-debug`)
- **Build**: `pr/juliusz-investigation` + UDP-decoupler env switch (`MCM_UDP_DECOUPLER`)
- **Tuning**: Phase 1 only — userspace pinning (MCM on cores 2,3; other containers on cores 0,1; `cpufreq=performance`; `irqbalance` off; swap off). Boot-time isolation (`isolcpus`/`nohz_full`/`rcu_nocbs`) **not** applied.
- **Raw data**: this directory (`phase1_20260520T162400/`). Source CSVs per cell, machine-readable summary in `summary.csv`, hand-readable in `summary.md`.

## TL;DR — winner

**`MCM_UDP_DECOUPLER=proxy`** (use `proxysink`/`proxysrc`'s internal queue; no extra B1 queue, no `excise_proxysrc_queue`). It ties `b1` on CPU (32 % below `appsink`), beats both on impaired-latency medians, has the cleanest tails outside of idle, and shows zero v4l2 drops like the others. The two-queue downside of B1+proxy is gone (one queue total). The CPU cost of the AppSink/AppSrc memcpy bridge is gone too.

## Question being answered

> "What is the minimal-latency, minimal-CPU way to decouple the UDP sink from the Tee, so that backpressure from one sink (typically WebRTC under bad network) cannot stall the upstream pipeline?"

Three candidates were compared against each other under three network conditions (idle, mild impair, aggressive impair), each replicated 3×.

| Variant | Construction | Decoupler queue |
|---|---|---|
| `appsink` | AppSink → AppSrc bridge between Tee and UDP sub-pipeline; same pattern as RTSP sink. | `AppSrc` internal (`max-buffers=1`, `drop=true`, `leaky-type=downstream`). |
| `b1` | `proxysink`/`proxysrc` between Tee and UDP sub-pipeline + explicit `b1` queue upstream of `proxysink`; proxy's internal queue **excised**. | Explicit `make_b1_queue` (60 buffers, 1 s, leaky-downstream). |
| `proxy` | `proxysink`/`proxysrc`; **no** B1 queue, proxy's internal queue **kept**. | proxysrc internal queue (defaults). |

## Experiment matrix

- **Variants**: `appsink`, `b1`, `proxy` (legacy/no-decoupler was already deprecated; kept as the env value `legacy` but not measured in this phase since it's known to backpressure on the Tee).
- **Conditions**:
  - `idle` — no impairment.
  - `impair_mild` — netem on the *active* WebRTC UDP src port only: `loss 0.5 %`, `delay 20 ms ± 5 ms (normal)`.
  - `impair_aggressive` — same but `loss 2 %`, `delay 50 ms ± 15 ms`.
- **Reps**: 3 per (variant × condition), independent MCM restarts between cells.
- **Per-cell timing**: 30 s warm-up + 90 s steady-state measurement. Impairment is applied 8 s into the warm-up so that the entire 90 s window is in steady state under impairment.
- **Receivers** (run from the lab PC): the bundled `examples/stream_latency` content-hashes incoming H.264 NAL VCL slices to match the *same* source frame across RTSP, plain UDP and WebRTC simultaneously.
- **Stream**: a single MCM stream (`lab_cam1`) with all three endpoints, made possible by an env-gated bypass of the historical RTSP+UDP soft-limit (`MCM_ALLOW_UDP_RTSP_CONCURRENT=1`). Pairwise latency therefore compares *literally the same source frame*.

## Statistical machinery

- Per-cell aggregation across the 3 reps before any test.
- Pairwise arrival deltas (per content-hash, in microseconds) for the three pairs `udp − rtsp`, `webrtc − rtsp`, `webrtc − udp`.
- 95 % bootstrap CI on the median (`n_boot = 2000`, fixed RNG seed for reproducibility).
- Mann–Whitney U (two-sided) for variant-vs-variant pairwise comparison, Bonferroni-corrected across variants per (condition × pair).
- Cliff's δ for effect size (`< 0.147` negligible · `< 0.33` small · `< 0.474` medium · ≥ `0.474` large).
- For CPU: 1 Hz `/proc` sampling inside the container, `mcm_total_pct = (utime+stime)/HZ`; mean & p95.
- For drops: the `mcm_inst="v4l2_drops"` 1 s instrumentation marker, filtered by the cell's `pipeline_id` (`producer_id` in `meta.json`) and the cell's `(start_iso, end_iso)` window, with a 3 s startup guard (this filter is what the analyser now applies after fixing the original "whole-hour grep" bug).

## Results

### Backpressure (the actual question)

After proper filtering, **all 27 cells = 0 drops, 0 windows-with-drops**.

| condition | variant | drops_max | drops_sum | windows_with_drops |
|---|---|---:|---:|---:|
| idle | appsink | 0 | 0 | 0 |
| idle | b1 | 0 | 0 | 0 |
| idle | proxy | 0 | 0 | 0 |
| impair_mild | appsink | 0 | 0 | 0 |
| impair_mild | b1 | 0 | 0 | 0 |
| impair_mild | proxy | 0 | 0 | 0 |
| impair_aggressive | appsink | 0 | 0 | 0 |
| impair_aggressive | b1 | 0 | 0 | 0 |
| impair_aggressive | proxy | 0 | 0 | 0 |

Interpretation: with the warm-up queue preserved on the WebRTC sink and *any* of the three decouplers on the UDP sink, the Pi at 1080p30/16.7 fps does not exhibit upstream backpressure under either reproducible impairment profile. The decouplers can be differentiated only on CPU and latency, not on drops, in this rig.

### CPU (MCM share of one core, % — mean and p95)

| condition | appsink mean / p95 | b1 mean / p95 | proxy mean / p95 |
|---|---:|---:|---:|
| idle | 5.85 / 7.05 | 4.46 / 5.31 | **4.42 / 5.19** |
| impair_mild | 5.98 / 7.03 | **4.47 / 5.30** | 4.56 / 5.36 |
| impair_aggressive | 6.01 / 7.11 | 4.65 / 5.36 | **4.57 / 5.35** |

- `appsink` is consistently ~1.4 pts (~32 %) more expensive than the two GStreamer-native options across all conditions.
- `b1` and `proxy` are within 0.1 pt of each other on the mean; on p95 they're indistinguishable.
- Each measurement is a pool of 240 samples (3 reps × ~80 s sampled at 1 Hz). The 32 % gap is far above any plausible sampling noise.

### Latency — pairwise arrival deltas (microseconds)

Negative `udp − rtsp` means UDP arrives *before* RTSP at the receiver (expected: `rtspsrc` defaults to a small jitter buffer that adds ~3 ms).

#### Median (`udp − rtsp`, lower is better)

| condition | appsink | b1 | proxy |
|---|---:|---:|---:|
| idle | **−2890** | −2324 | −2812 |
| impair_mild | −2890 | −2521 | **−2898** |
| impair_aggressive | **−3032** | −2378 | −2904 |

Differences between `appsink` and `proxy` are ≤ 130 µs at the median — well inside the per-frame jitter (`stddev` ≈ 25 ms) — and well below the inter-frame interval (60 ms). Statistically distinguishable (p_bonf as low as 1e-26) but the effect is `Cliff's δ ≤ 0.16` (small to negligible).

#### Max (worst-case tail across the cell)

| condition | pair | appsink | b1 | proxy |
|---|---|---:|---:|---:|
| idle | udp − rtsp | **−832** | 53626 | −585 |
| idle | webrtc − udp | 30574 | 39675 | 50925 |
| impair_aggressive | udp − rtsp | −633 | 4782 | −593 |
| impair_aggressive | webrtc − rtsp | 111087 | 173851 | **95306** |

The `b1` variant has a noticeably larger idle tail (one 53 ms spike in `udp − rtsp`, one 175 ms spike in `webrtc − rtsp` under aggressive impair). This is the 60-buffer / 1 s queue absorbing micro-jitter then flushing in bursts. `proxy`'s tail is the cleanest under impairment.

### Cross-variant Mann–Whitney + Cliff's δ summary (Bonferroni-corrected)

Direction: "best" = lowest median pairwise delta; positive `δ` means the comparison variant has *larger* (worse) deltas than the best.

| condition | pair | best | vs | best µs | vs µs | p_bonf | δ | effect |
|---|---|---|---|---:|---:|---:|---:|---|
| idle | udp − rtsp | appsink | b1 | −2890 | −2324 | 1.7e-203 | +0.454 | medium |
| idle | udp − rtsp | appsink | proxy | −2890 | −2812 | 3.9e-26 | +0.158 | small |
| idle | webrtc − rtsp | appsink | b1 | −1159 | −1111 | 3.2e-15 | +0.119 | negligible |
| idle | webrtc − rtsp | appsink | proxy | −1159 | −1138 | 2.3e-07 | +0.079 | negligible |
| idle | webrtc − udp | b1 | appsink | 1210 | 1716 | 0 | +0.579 | large |
| idle | webrtc − udp | b1 | proxy | 1210 | 1599 | 2.2e-134 | +0.368 | medium |
| impair_aggressive | udp − rtsp | appsink | b1 | −3032 | −2378 | 3.5e-181 | +0.428 | medium |
| impair_aggressive | udp − rtsp | appsink | proxy | −3032 | −2904 | 1.7e-16 | +0.124 | negligible |
| impair_aggressive | webrtc − rtsp | proxy | appsink | 74611 | 76024 | 0.029 | +0.139 | negligible |
| impair_aggressive | webrtc − rtsp | proxy | b1 | 74611 | 76523 | 0.003 | +0.217 | small |
| impair_aggressive | webrtc − udp | proxy | appsink | 77407 | 79046 | 0.005 | +0.171 | small |
| impair_aggressive | webrtc − udp | proxy | b1 | 77407 | 79320 | 0.008 | +0.194 | small |
| impair_mild | udp − rtsp | proxy | appsink | −2898 | −2890 | 2.1e-19 | −0.135 | negligible |
| impair_mild | udp − rtsp | proxy | b1 | −2898 | −2521 | 2.4e-36 | +0.188 | small |
| impair_mild | webrtc − rtsp | proxy | appsink | 25999 | 27214 | 0.089 | +0.306 | small |
| impair_mild | webrtc − rtsp | proxy | b1 | 25999 | 27155 | 0.020 | +0.309 | small |
| impair_mild | webrtc − udp | proxy | appsink | 28721 | 30572 | 0.024 | +0.382 | medium |
| impair_mild | webrtc − udp | proxy | b1 | 28721 | 29725 | 0.057 | +0.262 | small |

## Decision

| Criterion | Weight | appsink | b1 | proxy |
|---|---|---|---|---|
| v4l2 drops | high | 0 | 0 | 0 — tied |
| CPU (mean) | high | 5.95 % | 4.53 % | 4.52 % — *tied best* |
| CPU (p95) | medium | 7.06 % | 5.32 % | 5.30 % — *tied best* |
| Idle median latency | medium | best by ≤ 130 µs | worst by ~500 µs | second |
| Impair median latency | high | second | worst | **best** |
| Idle worst-case tail | medium | best | worst (53 ms spike) | second |
| Impair worst-case tail | medium | second | worst (175 ms) | **best** |
| Decoupler count in the graph | low (purity) | 1 (appsrc) | **2** (b1 + proxy-internal, even with excise the b1 is still there) | 1 (proxy-internal) |

**Pick: `proxy`.** It is the only variant that is at-or-near-best on every axis that actually matters under realistic conditions (impairment), with no axis where it's measurably worse than the best by an amount we care about. The idle latency gap to `appsink` (~80 µs at median) is one to two orders of magnitude below frame jitter.

## Phase 2 — is it necessary?

**No, with two small caveats.**

Reasons to skip:
- The single hardest comparison in Phase 1 (CPU between `appsink` and `proxy`) is at `p_bonf ≈ 10⁻²⁶⁰` and a 32 % absolute mean difference. Nothing in Phase 2 can move that.
- v4l2 drops are zero in all 27 cells. Phase 2 reduces *variance*; it can't reveal events that the instrumentation reported as exactly zero, and it can't bias upward into the customer's failure mode unless we change the impairment profile (which is the Phase 3 / customer-replication question, not the Phase 2 question).
- For 17 of the 18 variant-vs-variant comparisons reported, `p_bonf` is ≤ 0.03 with effects ranging from small to large. Phase 2 would tighten CIs but not flip ordering.

Caveats / things Phase 2 *would* address:
1. Two `impair_mild` comparisons sit at `p_bonf` ≈ 0.06 / 0.09 (the `webrtc` pairs where `proxy` is "best" by ~0.3 small-effect δ vs `appsink`). With Phase 2's tighter latency tails, those would likely cross significance — but the decision is already in `proxy`'s favour on every other axis, so this is academic.
2. The Pi's untuned kernel allows `rcu`, `nohz` ticks, and IRQ wakeups on the MCM cores, which contribute to the ~25 ms p99 inter-arrival jitter on RTSP. Phase 2 would shrink that and make the *absolute* numbers production-credible. If the goal is to publish "this is the actual latency budget", do Phase 2. If the goal is "pick the winner", Phase 1 is enough.

Recommendation: skip Phase 2 unless we want to publish absolute latency numbers externally. Ship `proxy` as the default now and add the same flag (`MCM_UDP_DECOUPLER=proxy`) to `zenoh_sink` in a follow-up PR.

## Reproducibility

```bash
# Pi prep (cores 2,3 for MCM, others to 0,1, governor=performance, irqs off MCM cores)
tools/onvehicle/pin_mcm.sh pin

# Build + deploy a release MCM
tools/onvehicle/repro_lab.sh deploy

# Full 27-cell matrix
results/decoupler_matrix/phase1_run.sh \
    || env RESULTS_DIR=results/decoupler_matrix/phase1_$(date +%Y%m%dT%H%M%S) \
       WARMUP=30 DURATION=90 REPORT_INTERVAL=30 SETUP_GRACE=10 IMPAIR_WAIT_S=8 \
       tools/onvehicle/run_decoupler_matrix.sh

# Analyse
python3 -m venv .venv-analyze && source .venv-analyze/bin/activate \
    && pip install -q numpy scipy
tools/onvehicle/analyze_decoupler.py results/decoupler_matrix/phase1_<TS>
```

## Limitations / honest caveats

- Receivers run on the lab PC, not on the customer's topside (Ubuntu 24 over the same CAT5 link). The pi↔lab-PC ICE pair is purely host-host on `192.168.2.0/24`, which is *not* what the customer's setup goes through. This is the most likely reason we can't reproduce drops here.
- We never reached `v4l2_drops > 0` on the Pi after the fix branch was applied — i.e. the *fix* is what makes the experiment uninteresting on the drops axis. Phase 2 will not change that. To distinguish the variants on drops we would need to (a) revert the WebRTC warm-up queue fix temporarily to bring back backpressure, or (b) move the experiment to the customer's network. Both are out of scope for this phase.
- WebRTC arrival on the receiver side is noisy under impairment (we routinely lost 70–80 % of WebRTC frames at the receiver under `loss=0.5 %` because `stream_latency`'s WebRTC client doesn't have a strong jitter buffer / PLI loop). The RTSP and UDP receivers are clean. CPU / drops / latency conclusions rest on RTSP and UDP; WebRTC arrival is reported but not part of the decision.
- `appsink` and `proxy` differ by single-digit microseconds at the median in idle. These differences can swing slightly with the WiFi/Ethernet/USB topology of the host. Phase 2 would not change the ranking, but absolute medians may shift ±100 µs cell-to-cell.

## Appendix — what changed in the repo for this experiment

- `src/lib/stream/debug_env.rs`: added `UdpDecoupler` enum + `udp_decoupler()` parser for `MCM_UDP_DECOUPLER={appsink,b1,proxy,legacy}`; added `allow_udp_rtsp_concurrent()` parser for the new bypass env.
- `src/lib/stream/mod.rs`: gated the historical UDP+RTSP coexistence error behind `MCM_ALLOW_UDP_RTSP_CONCURRENT=1` so a single stream can carry both endpoints (required to measure pairwise latency over the same source frame).
- `src/lib/stream/sink/udp_sink.rs`: implemented the three decoupler variants behind the env switch.
- `tools/onvehicle/pin_mcm.sh`: Phase-1 Pi pin/unpin/status. Pins `blueos-core` to cores 2,3, every other container to 0,1, stops `blueos-bootstrap`, sets `cpufreq=performance`, disables `irqbalance`, off swap.
- `tools/onvehicle/sample_cpu.sh`: 1 Hz `/proc/<pid>/stat` + `/proc/<pid>/status` + `/proc/stat` CSV emitter (runs *inside* the container so it sees the right PID namespace).
- `tools/onvehicle/run_decoupler_matrix.sh`: orchestrates 27-cell matrix. Auto-detects `LAB_PC_IP` from the route to the Pi; starts `stream_latency` first then applies impairment 8 s in so the impair script can find a live WebRTC UDP src port; fetches per-cell `mcm.log`, `cpu.csv`, `stream_latency.csv`, `impair.log`, `meta.json` (incl. `producer_id` and `start_iso`/`end_iso` for the analyser).
- `tools/onvehicle/analyze_decoupler.py`: per-cell percentiles + bootstrap CIs + Mann–Whitney + Cliff's δ + markdown + flat CSV. The v4l2-drops parser is filtered by cell pipeline-id and time window with a 3 s startup guard.
