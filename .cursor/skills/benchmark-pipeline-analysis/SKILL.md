---
name: benchmark-pipeline-analysis
description: Run pipeline analysis A/B benchmarks on a Raspberry Pi to measure CPU and memory overhead of --pipeline-analysis-level off/lite/full. Use when the user asks to benchmark, measure performance impact, test overhead, or re-run the A/B test after code changes.
---

# Benchmark Pipeline Analysis

Measures the CPU and memory overhead of `--pipeline-analysis-level off|lite|full` on a Raspberry Pi 4 (armv7) inside the `blueos-core-ab-test` Docker container.

## Architecture

The benchmark uses a **split architecture** with **CPU isolation** to avoid measurement contamination:

- **Pi-side** (`scripts/benchmark-pipeline-analysis.sh`): Runs inside the Docker container.
  Starts MCM, collects `/proc`-based CPU/RSS metrics. Does NOT run any polling clients.
- **Host-side** (`scripts/benchmark-host.sh`): Runs on the development machine.
  Orchestrates the benchmark, configures CPU isolation on the Pi, starts polling clients
  LOCALLY (HTTP curl, WebSocket), and collects results from the Pi.

**CPU isolation** (configured automatically by `benchmark-host.sh` via SSH):
- CPU governor set to `performance` on all 4 cores (no frequency scaling)
- CPU frequency locked to 1500 MHz (min=max=1500000 kHz)
- Docker container pinned to CPUs 1,2,3 (`--cpuset-cpus=1,2,3`)
- All OS userspace processes moved to CPU 0 via `taskset`
- All hardware IRQs pinned to CPU 0 via `/proc/irq/*/smp_affinity_list`
- Setup is validated before sampling begins; script aborts if misconfigured
- After benchmarking, governor is restored to `ondemand` and cpuset pin is removed

This ensures MCM has near-exclusive access to 3 dedicated cores, eliminating scheduling
jitter and frequency scaling noise from measurements.

## Prerequisites

- Pi at `192.168.2.2`, user `pi`, password `raspberry`
- `cross` installed and configured (see `Cross.toml`)
- `sshpass` installed on the host
- Container `blueos-core-ab-test` running on the Pi with `tmux` and `python3`
- For WS tests on host: `pip install websocket-client`
- Previous results at `docs/pipeline-analysis-benchmark.md`

## Quick Run (Full Test Matrix)

The benchmark supports `VARIANT_LABEL` for tagging runs (e.g. for A/B comparisons across
commits). Build once, then run off/lite/full + HTTP + WebSocket tests.

### Helper: build and deploy

```bash
PI=pi@192.168.2.2
SSHP="sshpass -p raspberry"
BIN=target/armv7-unknown-linux-gnueabihf/release/mavlink-camera-manager

build_and_deploy() {
    SKIP_WEB=1 cross build --release --target armv7-unknown-linux-gnueabihf
    $SSHP scp -o StrictHostKeyChecking=no "$BIN" $PI:/tmp/mcm_bench
    $SSHP ssh -o StrictHostKeyChecking=no $PI \
      'docker cp /tmp/mcm_bench blueos-core-ab-test:/root/mavlink-camera-manager && \
       docker exec blueos-core-ab-test chmod +x /root/mavlink-camera-manager'
}
```

### Run benchmark

```bash
build_and_deploy

VARIANT_LABEL=default POLL_MODE=none      ./scripts/benchmark-host.sh "off lite full"
VARIANT_LABEL=default POLL_MODE=http-full ./scripts/benchmark-host.sh full
VARIANT_LABEL=default POLL_MODE=ws-full   ./scripts/benchmark-host.sh full
```

### Update documentation

After collecting results, update `docs/pipeline-analysis-benchmark.md` with:
- New date and commit hash
- Updated tables from the benchmark output
- Comparison with previous results from [reference.md](reference.md)

## POLL_MODE Reference

| Mode | What it measures | Client on host | Server endpoint (on Pi) |
|------|-----------------|----------------|------------------------|
| `none` | Probes + 1 Hz background sampler only | (none) | (none) |
| `http-full` | + full snapshot + JSON serialization | `curl` loop | `GET /stats/streams/snapshot` |
| `ws-full` | + full snapshot + JSON via WebSocket | `websocket-client` | `WS .../ws?interval_ms=1000` |

## Scripts

| Script | Runs on | Purpose |
|--------|---------|---------|
| `scripts/benchmark-host.sh` | Host (dev machine) | Orchestrator: deploy, start clients, collect results |
| `scripts/benchmark-pipeline-analysis.sh` | Pi (container) | Pure `/proc` sampler: start MCM, collect CPU/RSS |

## Benchmark Script Details

**Pi-side** (`benchmark-pipeline-analysis.sh`):
For each level it:
1. Kills any existing MCM process
2. Starts MCM in a tmux session with `--pipeline-analysis-level <level>`
3. Waits 30s warmup, verifies pipelines are streaming (CPU delta check)
4. Collects 60 one-second samples of: process CPU%, RSS (KB), load average
5. Kills MCM, 15s cooldown before next level

**Host-side** (`benchmark-host.sh`):
1. Sets up CPU isolation on Pi (governor, frequency, cpuset, taskset, IRQ affinity)
2. Validates isolation configuration (aborts if misconfigured)
3. Deploys scripts to Pi via SCP
4. For each level:
   a. Starts Pi-side sampler via SSH (background)
   b. Waits for MCM warmup (~40s)
   c. Starts local polling client (HTTP/WS)
   d. Waits for sampling to complete (~75s)
   e. Stops local client
5. Fetches CSV results from Pi
6. Prints summary table
7. Restores Pi CPU defaults (governor=ondemand, removes cpuset pin)

**CPU measurement**: `delta(utime + stime)` from `/proc/<pid>/stat` divided by wall time
and `CLK_TCK`. Reports total process CPU across all cores (100% = 1 full core).

**Configurable**: Environment variables: `PI_HOST`, `PI_USER`, `PI_PASS`, `CONTAINER`,
`POLL_MODE`, `VARIANT_LABEL`. Levels passed as `$1` (default: `"off lite full"`).

## Known Issues and Workarounds

| Issue | Symptom | Fix |
|-------|---------|-----|
| V4L2 device not released | `shmsink` EBUSY errors on next run | 15s cooldown between levels |
| `pgrep -f` matches tmux | Wrong PID | Script uses `pidof mavlink-camera-manager` |
| `bc` not in container | Arithmetic errors | Script uses `awk` for all floating-point math |
| Low CPU during warmup | Pipelines not streaming yet | Auto-retry: waits 30s extra, re-checks |
| SSH warnings | Post-quantum key exchange warning | Cosmetic; use `-o StrictHostKeyChecking=no` |

## Interpreting Results

**Key comparisons** (all deltas are vs `off` level):

| What to check | Expected range | Concern if |
|---------------|---------------|------------|
| Probe overhead (lite, POLL_MODE=none) | +0-2% CPU | > 5% |
| Probe overhead (full, POLL_MODE=none) | +0-3% CPU | > 5% |
| HTTP serving cost | +1-3% CPU | > 10% |
| WS serving cost | +1-3% CPU | > 10% |
| FPS impact | No drop from expected ~65 total | Drop > 5% |

For detailed historical baselines, see [reference.md](reference.md).
