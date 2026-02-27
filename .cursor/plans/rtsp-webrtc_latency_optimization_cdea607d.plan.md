---
name: RTSP-WebRTC Latency Optimization
overview: Systematically optimize the RTSP-to-WebRTC streaming path in mavlink-camera-manager-next on a Raspberry Pi 4 to minimize the latency/jitter gap versus direct RTSP, using iterative measurement-driven methodology with statistical rigor.
todos:
  - id: phase0-baseline
    content: "Phase 0: Deploy current HEAD and collect statistically robust baseline measurements (3+ runs, 120s each)"
    status: completed
  - id: phase1a-collect
    content: "Phase 1a: Collect system-level diagnostics under load (network drops, IRQ distribution, CPU per-core, scheduler latency, socket buffers, Docker network mode)"
    status: completed
  - id: phase1b-analyze
    content: "Phase 1b: Analyze system data -- identify actual bottlenecks from evidence"
    status: completed
  - id: phase1c-iterate
    content: "Phase 1c: Hypothesize and test system-level changes one at a time, each with full measurement and statistical comparison"
    status: pending
  - id: phase2-trace
    content: "Phase 2: Enable GStreamer latency tracers, analyze per-element contributions, identify bottlenecks"
    status: completed
  - id: phase3-pipeline
    content: "Phase 3: Iterative pipeline optimizations based on tracing data (rtspsrc tuning, queue tuning, webrtcbin config, RTP path)"
    status: completed
  - id: phase4-advanced
    content: "Phase 4: Advanced optimizations if gap persists (bypass depay/repay, thread affinity, allocator, Docker overhead)"
    status: pending
  - id: phase5-stress
    content: "Phase 5: Stress testing -- multi-stream, CPU stress, network stress -- progressively harden the pipeline"
    status: completed
isProject: false
---

# RTSP-to-WebRTC Latency Optimization Plan

## System Context

- **Target**: Raspberry Pi 4 Model B (4x Cortex-A72, 2GB RAM, kernel 5.10.33)
- **Camera**: RadCam at `192.168.2.10:554` -- 4K @ 30fps H.264/H.265, ~50Mbps
- **MCM**: Running in Docker container `blueos-core` on BlueOS
- **Measurement client**: This machine, connecting to Pi at `192.168.2.2`
- **Branch**: `next-2`, clean working tree
- **Goal**: WebRTC latency/jitter indistinguishable from direct RTSP

## Methodology (per the request document)

Each iteration follows: **Measure -> Analyze -> Hypothesize -> Implement -> Deploy -> Compare -> Journal**

Every code change gets a semantic commit. Changes that don't show statistically significant improvement get `git revert` to preserve history.

---

## Phase 0: Baseline Measurement

Collect a statistically robust baseline with the current code (HEAD of `next-2`).

1. Deploy current `next-2` HEAD to the Pi via `cross_build_and_run.sh`
2. Run measurement at the camera's native configuration:

```
   cargo run --example stream_latency -- \
     --webrtc ws://192.168.2.2:6021 \
     --rtsp rtsp://192.168.2.10:554/stream_0 \
     --codec h264 --warmup 10 --duration 120 \
     --csv results/baseline.csv
   

```

1. Run at least 3 repetitions to establish statistical confidence
2. Generate plots with `plot_results.py`, record: median latency delta, p95, p99, jitter, drop rate, stutter rate
3. Journal the baseline numbers as the reference point

---

## Phase 1: System-Level Diagnostics and Tuning

Same methodology as the GStreamer phases: **collect data first, diagnose, hypothesize, change one thing, measure, decide**.

### 1a. System-level data collection (during baseline streaming)

Collect the following **while the baseline measurement is running**, so we see the system under real load:

- **Network drops**: `cat /proc/net/udp` (drops column), `netstat -su` (UDP errors), `cat /proc/net/softnet_stat` (per-CPU backlog overflows)
- **IRQ distribution**: `cat /proc/interrupts` snapshots at start and end (compute per-CPU delta for eth0 IRQs)
- **CPU utilization per core**: `mpstat -P ALL 1` during the measurement window -- identify if any core is saturated
- **Context switches and softirq time**: `vmstat 1` or `sar -w 1` -- measure context switch rate and softirq overhead
- **Scheduler latency**: `perf sched latency` if available, or `/proc/schedstat` deltas -- measure how long GStreamer threads wait to be scheduled
- **Socket buffer usage**: `ss -u -m` to see actual socket buffer fill levels under load
- **Docker overhead**: `docker inspect blueos-core` to check network mode (host vs bridge)

### 1b. Analyze system data

From the collected data, answer these questions:

- Are UDP receive/send buffers overflowing? (If `drops` column in `/proc/net/udp` is non-zero, buffer sizing is a real issue)
- Is any single CPU core saturated while others are idle? (IRQ imbalance or thread pinning issue)
- Are GStreamer threads experiencing scheduling delays? (High context switch rate or scheduler latency)
- Is softirq processing a bottleneck? (High softirq time on one core)
- Is Docker's network namespace adding measurable overhead?

### 1c. Hypothesize and act (one change at a time)

Based on the analysis, form a specific hypothesis and test it:

**Example hypotheses** (only pursue if data supports them):

- "UDP drops are non-zero -> increasing `rmem_max`/`wmem_max` will reduce frame drops"
- "eth0 IRQs are 100% on CPU0 which is also at 95% utilization -> redistributing IRQs will reduce latency spikes"
- "GStreamer threads show >Xms scheduling latency -> CPU isolation or priority tuning will reduce jitter"
- "Docker bridge networking adds measurable latency -> `--net=host` will improve latency"

For each hypothesis:

1. Apply a single change
2. Re-run the same measurement (same duration, same warmup, same number of repetitions)
3. Compare statistically against baseline
4. Journal: what the data said, the hypothesis, the result, decision to keep or revert

### 1d. Iterate

Repeat 1a-1c after each kept change, since the system profile may shift after tuning.

---

## Phase 2: GStreamer Pipeline Analysis

Use GStreamer tracing and profiling to identify internal bottlenecks.

### 2a. Pipeline latency tracing

Enable GStreamer latency tracer (`GST_TRACERS=latency`) to measure per-element latency contributions. Identify which elements in the path `rtspsrc -> depay -> parse -> tee -> queue -> webrtcbin` contribute the most.

### 2b. Queue behavior analysis

- Monitor queue levels at runtime using `GST_DEBUG` or pad probes
- Verify the dynamic queue state machine in WebRTC sink is working as intended
- Check if queue overruns/underruns correlate with latency spikes

### 2c. Buffer copy analysis

Check for unnecessary buffer copies in the pipeline. The path from RTSP source to WebRTC should ideally be zero-copy for the RTP payloads.

---

## Phase 3: Iterative Pipeline Optimizations

Based on Phase 2 findings, apply changes one at a time.

### Candidate optimizations (prioritized by expected impact):

**3a. rtspsrc tuning** ([src/lib/stream/pipeline/onvif_pipeline.rs](src/lib/stream/pipeline/onvif_pipeline.rs))

- `latency=0` (already set based on recent commits)
- `do-retransmission=false` to avoid NACK delays
- `ntp-time-source=running-time` for consistent timestamps
- `buffer-mode=none` or `buffer-mode=slave` to minimize jitterbuffer

**3b. Queue element tuning**

- Verify queues between depay and tee are truly minimal (1 buffer or time-based at frame interval)
- Consider replacing queue with `identity` where buffering isn't needed

**3c. webrtcbin configuration** ([src/lib/stream/sink/webrtc_sink.rs](src/lib/stream/sink/webrtc_sink.rs))

- Verify `do-nack` behavior isn't adding latency via retransmission waits
- Check TWCC (Transport-Wide Congestion Control) isn't throttling
- Verify no congestion controller is introducing artificial delay
- Consider disabling `rtx` (retransmission) if reliability is less important than latency

**3d. RTP payloader/depayloader**

- `aggregate-mode=zero-latency` on payloaders (already applied)
- Consider `config-interval=-1` to avoid periodic SPS/PPS re-injection delays
- Verify no unnecessary parsing/re-parsing in the depay->pay chain

**3e. Clock and synchronization**

- Ensure `sync=false` on the final sink where applicable
- Check pipeline clock source -- NTP vs monotonic
- Verify no clock drift between source and sink

---

## Phase 4: Advanced Optimizations (if gap persists)

**4a. Bypass depay/repay for RTP tee path**
If the RTSP source provides RTP packets, and WebRTC consumes RTP packets, the ideal path is:

```
rtspsrc (RTP out) -> tee -> queue -> webrtcbin
```

Without depay->parse->pay in between. This eliminates 3 elements and their buffer copies. This requires ensuring the RTP caps are compatible between source and webrtcbin.

**4b. Thread pool and affinity**

- Pin GStreamer streaming threads to isolated cores using `taskset`
- Reduce context switching by minimizing thread count

**4c. Memory allocation**

- Use `jemalloc` or `mimalloc` for reduced allocation latency
- Pre-allocate buffer pools

**4d. Docker overhead**

- Measure if running outside Docker reduces latency (network namespace overhead)
- Consider `--net=host` if not already used

---

## Journaling and Decision Framework

- **Journal location**: `results/JOURNAL.md`
- **Commit policy**: Individual semantic commits per change. `git revert` if not statistically proven.
- **No reboots**: Only runtime changes (sysctl, taskset, cgroups, etc.). No kernel cmdline or boot config changes.
- **System safety**: Always collect and record the previous value before any system-level change.

For each iteration:

1. **Record**: Commit hash, sysctl/config changes, measurement CSV
2. **Compare**: Use `plot_results.py` with both baseline and new data
3. **Decision criteria**: Keep if p-value < 0.05 on a paired test (Wilcoxon signed-rank or similar) for median latency improvement
4. **Revert**: `git revert` if not statistically significant
5. **Accumulate**: Combine kept changes and re-measure the compound effect

## Phase 5: Stress Testing

Once latency is optimized under normal conditions, progressively add stress:

1. **Multi-stream**: Multiple simultaneous WebRTC + UDP + RTSP clients
2. **CPU stress**: stress-ng simulating heavy ArduPilot + services load
3. **Network stress**: Concurrent traffic, bandwidth saturation
4. Measure degradation under each scenario and harden the pipeline

---

## Key Files

- Pipeline construction: [src/lib/stream/pipeline/onvif_pipeline.rs](src/lib/stream/pipeline/onvif_pipeline.rs), [src/lib/stream/pipeline/redirect_pipeline.rs](src/lib/stream/pipeline/redirect_pipeline.rs)
- WebRTC sink: [src/lib/stream/sink/webrtc_sink.rs](src/lib/stream/sink/webrtc_sink.rs)
- UDP sink: [src/lib/stream/sink/udp_sink.rs](src/lib/stream/sink/udp_sink.rs)
- RTSP server: [src/lib/stream/rtsp/rtsp_server.rs](src/lib/stream/rtsp/rtsp_server.rs)
- Thread priority: [src/lib/helper/threads.rs](src/lib/helper/threads.rs)
- Pipeline runner: [src/lib/stream/pipeline/runner.rs](src/lib/stream/pipeline/runner.rs)
- Measurement tool: [examples/stream_latency/](examples/stream_latency/)
- Deploy script: [cross_build_and_run.sh](cross_build_and_run.sh)

