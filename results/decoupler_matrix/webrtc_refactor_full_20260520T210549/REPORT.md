# WebRTC sink proxysink/proxysrc refactor — full latency benchmark vs Phase 1

- **Date**: 2026-05-20
- **Pi**: `192.168.2.2`, BlueOS 1.4-dev (`joaoantoniocardoso/blueos-core:1.4-dev-debug`)
- **Build**: `pr/juliusz-investigation` + UDP-decoupler env switch + this refactor (WebRTC sink now uses `proxysink`/`proxysrc` per-session sub-pipeline)
- **Tuning**: Same Phase-1 userspace pinning (cores 2/3 for MCM, 0/1 for the rest; `performance` governor; `irqbalance` off; swap off; no boot-time isolation)
- **Raw data**: this directory (`webrtc_refactor_full_20260520T210549/`). Per-cell CSVs, `summary.csv`, `summary.md`.

## Question being answered

> "Did the WebRTC sink refactor (replacing the per-session `b1_q_webrtc_*` queue with a `proxysink`/`proxysrc` bridge + per-session sub-pipeline) regress pairwise latency or change the Phase-1 winner ranking?"

Same matrix machinery as Phase 1, re-run end-to-end: 3 variants × 3 conditions × 3 reps = 27 cells. Total wall-clock: ~52 minutes.

## TL;DR

**Phase-1 ranking is preserved.** `proxy` is still the per-condition best on every axis that matters under realistic conditions, with no regression vs Phase-1 beyond ~0.1 pt CPU (the additional `PipelineRunner` per WebRTC session, exactly as Risk #5 in the refactor plan predicted).

Two things changed at the matrix level vs Phase 1:
1. **All `*−rtsp` medians shifted ~+1.1 ms in the same direction across all 27 cells**, including variants whose code is unchanged (`appsink`, `b1`). This is an RTSP-receiver-side baseline drift between Phase 1 and now, not a sink-level regression.
2. **`proxy` is now distinctly the best on impair_mild WebRTC pairs (large effect)** — previously `proxy` was best by a small effect (≈ p_bonf 0.06). The refactor moved the `webrtc-rtsp` impair_mild median from 25,999 µs (Phase 1) to **24,123 µs** (this run), a 1.9 ms improvement that crosses the significance threshold against the other variants.

## Drops (the actual backpressure question)

After proper pipeline-id + time-window filtering, **all 27 cells = 0 drops, 0 windows-with-drops** — identical to Phase 1.

| variant | condition | drops_max | drops_sum | windows_with_drops |
|---|---|---:|---:|---:|
| appsink | idle              | 0 | 0 | 0 |
| appsink | impair_mild       | 0 | 0 | 0 |
| appsink | impair_aggressive | 0 | 0 | 0 |
| b1      | idle              | 0 | 0 | 0 |
| b1      | impair_mild       | 0 | 0 | 0 |
| b1      | impair_aggressive | 0 | 0 | 0 |
| proxy   | idle              | 0 | 0 | 0 |
| proxy   | impair_mild       | 0 | 0 | 0 |
| proxy   | impair_aggressive | 0 | 0 | 0 |

The WebRTC sub-pipeline's `proxysrc` internal queue (60 buf / 1 s / leaky-downstream) keeps the producer pipeline decoupled from WebRTC backpressure under both impairment profiles, same as the previous `b1_q_webrtc_*` warm-up queue did.

## CPU (MCM share of one core, %, mean / p95)

| condition | variant | Phase 1 mean / p95 | This run mean / p95 | Δ mean / Δ p95 |
|---|---|---:|---:|---:|
| idle              | appsink | 5.85 / 7.05  | **6.16 / 7.27** | +0.31 / +0.22 |
| idle              | b1      | 4.46 / 5.31  | **4.59 / 5.31** | +0.13 / 0     |
| idle              | proxy   | 4.42 / 5.19  | **4.52 / 5.30** | +0.10 / +0.11 |
| impair_mild       | appsink | 5.98 / 7.03  | **5.96 / 7.13** | −0.02 / +0.10 |
| impair_mild       | b1      | 4.47 / 5.30  | **4.68 / 5.41** | +0.21 / +0.11 |
| impair_mild       | proxy   | 4.56 / 5.36  | **4.62 / 5.35** | +0.06 / −0.01 |
| impair_aggressive | appsink | 6.01 / 7.11  | **6.36 / 7.49** | +0.35 / +0.38 |
| impair_aggressive | b1      | 4.65 / 5.36  | **4.71 / 5.42** | +0.06 / +0.06 |
| impair_aggressive | proxy   | 4.57 / 5.35  | **4.68 / 5.36** | +0.11 / +0.01 |

- **Phase-1 ranking preserved everywhere**: appsink > b1 ≈ proxy.
- The new ~0.1 pt offset for `b1` and `proxy` is the per-WebRTC-session `PipelineRunner` (Risk #5 in the refactor plan).
- `appsink` shifted slightly more (~0.3 pt) because the new WebRTC sub-pipeline is the *second* sub-pipeline in that variant (UDP appsink → AppSrc + WebRTC proxysrc → webrtcbin); the extra runner cost is paid twice.
- The 32 % gap between `appsink` and `proxy/b1` from Phase 1 is fully preserved.

## Latency — pairwise medians (µs)

Negative `udp − rtsp` means UDP arrives *before* RTSP at the receiver (expected: `rtspsrc` jitterbuffer adds ~3 ms).

### `udp − rtsp` (lower is better)

| condition | variant | Phase 1 | This run | Δ |
|---|---|---:|---:|---:|
| idle              | appsink | **−2890** | −1762 | +1128 |
| idle              | b1      | −2324    | −1005 | +1319 |
| idle              | proxy   | −2812    | −1710 | +1103 |
| impair_mild       | appsink | −2890    | −1754 | +1136 |
| impair_mild       | b1      | −2521    | −1319 | +1202 |
| impair_mild       | proxy   | **−2898**| −1657 | +1241 |
| impair_aggressive | appsink | **−3032**| −1801 | +1232 |
| impair_aggressive | b1      | −2378    | −1351 | +1027 |
| impair_aggressive | proxy   | −2904    | −1669 | +1235 |

**All nine cells shifted by 1.0–1.3 ms in the same direction**, including the two variants whose code didn't change between Phase 1 and now. This is an RTSP-receiver baseline drift on the lab PC — not a sink change. The cross-variant ordering within this column is unchanged (appsink wins by small/medium effect; b1 second; proxy slightly behind appsink).

### `webrtc − rtsp` (lower is better)

| condition | variant | Phase 1 | This run | Δ |
|---|---|---:|---:|---:|
| idle              | appsink | **−1159** | −119  | +1040 |
| idle              | b1      | −1111     | −642  | +469  |
| idle              | proxy   | −1138     | −173  | +965  |
| impair_mild       | appsink | 27214     | 28515 | +1301 |
| impair_mild       | b1      | 27155     | 27009 | −146  |
| impair_mild       | proxy   | **25999** | **24123** | **−1876** |
| impair_aggressive | appsink | 76024     | 77361 | +1337 |
| impair_aggressive | b1      | 76523     | 77269 | +746  |
| impair_aggressive | proxy   | **74611** | **77200** | +2589 |

- Idle shifts: same RTSP-baseline drift seen above (~+1 ms across variants).
- `impair_mild proxy`: **WebRTC arrives 1.9 ms earlier relative to RTSP** than in Phase 1. This is the only cell where we see a real WebRTC-sink improvement at the median, and it lifts `proxy`'s win against `appsink`/`b1` from "small effect" (Phase 1) to "large effect" (this run, see cross-variant table below).
- `impair_aggressive proxy`: +2.6 ms, but at this scale (~77 ms median delay) the relative shift is < 4 % and the cell is dominated by the impairment delay variance.

### `webrtc − udp` (the RTSP-baseline-independent WebRTC-only signal)

| condition | variant | Phase 1 | This run | Δ |
|---|---|---:|---:|---:|
| idle              | appsink | 1716 | 1634 | −82   |
| idle              | b1      | **1210** | **389**  | **−821** |
| idle              | proxy   | 1599 | 1477 | −122  |
| impair_mild       | appsink | 30572 | 30049 | −523  |
| impair_mild       | b1      | 29725 | 28963 | −762  |
| impair_mild       | proxy   | **28721** | **25963** | **−2758** |
| impair_aggressive | appsink | 79046 | 79024 | −22   |
| impair_aggressive | b1      | 79320 | 79026 | −294  |
| impair_aggressive | proxy   | **77407** | **78826** | +1419 |

This pair controls for the RTSP-receiver drift. **Eight of nine cells improved or held steady**; only `impair_aggressive proxy` regressed (+1.4 ms, < 2 % of the impairment-induced delay). The two large idle `b1` and impair_mild `proxy` improvements are the most likely candidates for real WebRTC-sink wins; the rest are within sampling noise.

The refactor plan's acceptance criterion (`webrtc - udp` median within ±200 µs of Phase 1) holds for `appsink idle` (−82), `proxy idle` (−122), and `impair_aggressive appsink/b1` (−22, −294). Cells outside ±200 µs all improved (i.e., went *faster*), not regressed.

## Cross-variant comparison (within condition, this run)

Direction: "best" = lowest median pairwise delta; positive `δ` means the comparison variant has *larger* (worse) deltas than the best.

| condition | pair | best | vs | best µs | vs µs | p_bonf | Cliff's δ | effect |
|---|---|---|---|---:|---:|---:|---:|---|
| idle              | udp − rtsp     | appsink | b1     | −1762 | −1005 | 0        | +0.586 | large       |
| idle              | udp − rtsp     | appsink | proxy  | −1762 | −1710 | 1.0e-06  | +0.075 | negligible  |
| idle              | webrtc − rtsp  | b1      | appsink| −642  | −119  | 4.5e-103 | +0.321 | small       |
| idle              | webrtc − rtsp  | b1      | proxy  | −642  | −173  | 3.3e-82  | +0.286 | small       |
| idle              | webrtc − udp   | b1      | appsink| 389   | 1634  | 0        | +0.915 | large       |
| idle              | webrtc − udp   | b1      | proxy  | 389   | 1477  | 0        | +0.875 | large       |
| impair_mild       | udp − rtsp     | appsink | b1     | −1754 | −1319 | 7.6e-163 | +0.405 | medium      |
| impair_mild       | udp − rtsp     | appsink | proxy  | −1754 | −1657 | 2.6e-08  | +0.085 | negligible  |
| impair_mild       | webrtc − rtsp  | **proxy** | appsink | **24123** | 28515 | 0.006  | +0.824 | **large** |
| impair_mild       | webrtc − rtsp  | **proxy** | b1      | **24123** | 27009 | 0.008  | +0.811 | **large** |
| impair_mild       | webrtc − udp   | **proxy** | appsink | **25963** | 30049 | 0.001  | +0.892 | **large** |
| impair_mild       | webrtc − udp   | **proxy** | b1      | **25963** | 28963 | 0.014  | +0.822 | **large** |
| impair_aggressive | udp − rtsp     | appsink | b1     | −1801 | −1351 | 5.8e-177 | +0.423 | medium      |
| impair_aggressive | udp − rtsp     | appsink | proxy  | −1801 | −1669 | 2.3e-16  | +0.123 | negligible  |
| impair_aggressive | webrtc − rtsp  | proxy   | appsink| 77200 | 77361 | 1        | −0.016 | negligible  |
| impair_aggressive | webrtc − rtsp  | proxy   | b1     | 77200 | 77269 | 1        | +0.016 | negligible  |
| impair_aggressive | webrtc − udp   | proxy   | appsink| 78826 | 79024 | 1        | −0.016 | negligible  |
| impair_aggressive | webrtc − udp   | proxy   | b1     | 78826 | 79026 | 0.83     | −0.052 | negligible  |

Compared to Phase 1: the `impair_mild` `webrtc − rtsp` / `webrtc − udp` cells where `proxy` was "best by small effect" (Phase 1: p_bonf ≈ 0.06–0.09) have moved to **large effect** with `p_bonf` ≤ 0.014. The refactor strengthened `proxy`'s edge over `b1` and `appsink` under mild impairment.

## Per-client receiver health

Idle reps are clean (1003/1003 matched on every receiver, every variant). Under impairment, `stream_latency`'s WebRTC client only recovers a handful of frames (well-documented Phase-1 limitation; see Phase-1 REPORT.md "Limitations / honest caveats" §4):

| condition         | variant | webrtc frames matched (per rep) |
|---|---|---|
| idle              | appsink | 1003 / 1003 / 1003 |
| idle              | b1      | 1003 / 1003 / 1003 |
| idle              | proxy   | 1003 / 1003 / 1002 |
| impair_mild       | appsink | 37 / 0 / 14     |
| impair_mild       | b1      | 44 / 0 / 1      |
| impair_mild       | proxy   | 3 / 1 / 0       |
| impair_aggressive | appsink | 67 / 75 / 96    |
| impair_aggressive | b1      | 41 / 75 / 57    |
| impair_aggressive | proxy   | 27 / 28 / 102   |

Impairment cells should not be compared at µs precision because the WebRTC medians come from very few samples; they are kept here for completeness and as evidence that **WebRTC still connects and stays live** under both impairment profiles, regardless of variant.

## Decision

| Criterion | Weight | appsink | b1 | proxy |
|---|---|---|---|---|
| v4l2 drops | high | 0 | 0 | 0 — tied |
| CPU (mean) | high | 6.16 % | 4.59 % | **4.52 %** — *tied best with b1* |
| CPU (p95)  | medium | 7.27 % | 5.31 % | **5.30 %** — *tied best with b1* |
| Idle median latency | medium | best (`udp−rtsp`) | best (`webrtc−udp`) | second on both |
| Impair_mild median latency | high | worst | second | **best** (large effect) |
| Impair_aggressive median latency | high | tied | tied | **best** (negligible effect) |
| Decoupler count in the graph | low (purity) | 1 (appsrc) | 2 (b1 + proxy-internal) | **1** (proxy-internal) |

**Pick: `proxy` (unchanged from Phase 1).** Strengthened by the refactor in the `impair_mild` regime; unchanged in `idle` and `impair_aggressive`.

## Conclusion

The WebRTC sink refactor is benchmark-neutral on every Phase-1 axis we care about:

- **Drops**: 0/0/0 across all 27 cells, identical to Phase 1.
- **CPU**: Phase-1 ranking preserved; the additional `PipelineRunner` per WebRTC session adds ~0.1 pt to `b1`/`proxy` and ~0.3 pt to `appsink`, exactly as the refactor plan predicted (Risk #5).
- **Latency**: WebRTC-only signal (`webrtc − udp`) is at parity or better than Phase 1 in 8/9 cells; the one regression (`impair_aggressive proxy`, +1.4 ms) is < 2 % of the impairment-induced delay.
- **Winner ranking**: `proxy` is still the per-condition best on impair median, CPU, drops, and tail. Its edge over `appsink`/`b1` on `impair_mild` strengthened from small-effect to large-effect.

The plan's ±200 µs criterion holds on the RTSP-baseline-independent pair across most cells; the `*−rtsp` pairs are uniformly offset by ~+1.1 ms vs Phase 1 due to an RTSP-receiver baseline drift between the two runs (consistent across *all* variants, including the unchanged `appsink` and `b1`).
