---
name: validate-stats-tracers
description: Cross-validate MCM pipeline stats against GStreamer coretracers and GstShark. Use when validating measurement accuracy, comparing MCM metrics against reference implementations, or investigating discrepancies between MCM stats and GStreamer's built-in tracer infrastructure.
---

# Validate MCM Stats Against GStreamer Tracers

Cross-validation tool that compares MCM's custom pad-probe-based metrics against
GStreamer's well-established core tracer and GstShark measurements to validate
measurement methodology.

## Prerequisites

- A running MCM instance with `--pipeline-analysis-level full` and at least one active stream
- MCM must have been started with GStreamer tracers enabled (see below)
- `python3` available
- No additional Python packages required (stdlib only)

## Starting MCM with Tracers

MCM redirects GStreamer debug output through Rust's `tracing` crate, so
`GST_DEBUG_FILE` does not work. Instead, set `RUST_LOG=info,gstreamer=trace`
and redirect MCM's stdout/stderr to a file.

### Core tracers only (always available, no extra dependencies):

```bash
RUST_LOG="info,gstreamer=trace" \
GST_TRACERS="latency(flags=pipeline+element)" \
GST_DEBUG="GST_TRACER:7" \
mavlink-camera-manager --pipeline-analysis-level full \
  > /tmp/mcm_tracer.log 2>&1
```

This enables:
- `latency`: pipeline src-to-sink latency (event injection)
- `element-latency`: per-element processing latency (event injection)

### With GstShark (richer comparison, requires GstShark installed):

```bash
RUST_LOG="info,gstreamer=trace" \
GST_TRACERS="latency(flags=pipeline+element);framerate;bitrate;proctime;scheduletime" \
GST_DEBUG="GST_TRACER:7" \
mavlink-camera-manager --pipeline-analysis-level full \
  > /tmp/mcm_tracer.log 2>&1
```

This adds:
- `framerate`: fps per src pad (1s window)
- `bitrate`: bits/sec per src pad (1s window)
- `proctime`: per-element processing time
- `scheduletime`: inter-buffer interval on sink pads

## Running the Validation

In a separate terminal (MCM must already be running with output redirected):

```bash
GST_TRACE_FILE=/tmp/mcm_tracer.log \
python3 .cursor/skills/validate-stats-tracers/validate_stats.py --duration 60
```

### Environment variables

| Variable | Default | Description |
|----------|---------|-------------|
| `MCM_URL` | `http://127.0.0.1:6020` | MCM base URL |
| `GST_TRACE_FILE` | `/tmp/mcm_tracer.log` | Path to the MCM log file with tracer output |
| `MCM_TIMEOUT` | `5` | HTTP request timeout in seconds |

### Command-line arguments

| Argument | Default | Description |
|----------|---------|-------------|
| `--duration` | `30` | Collection duration in seconds |
| `--threshold` | `10.0` | Relative error % threshold for warnings |
| `--warmup` | `5` | Warmup period in seconds before collection |
| `--json-out` | (none) | Path to write the JSON report file |

## Metric Comparison Matrix

| MCM Metric | Tracer Source | Match Quality |
|------------|---------------|---------------|
| `1000/mean_interval_ms` (src pads) | GstShark `framerate` | Direct |
| `bitrate_bps` (src pads) | GstShark `bitrate` | Direct |
| `processing_time_us` (per element) | Core `element-latency` or GstShark `proctime` | Direct |
| `mean_interval_ms` (sink pads) | GstShark `scheduletime` | Direct |
| `total_pipeline_causal_latency_ms` | Core `latency` (pipeline mode) | Methodological difference |

The pipeline latency comparison is the most interesting: MCM uses PTS-matching
(passive observation) while the core tracer uses event injection (active measurement).

## Output

The script prints a comparison table to stdout and optionally writes a JSON report:

```
--- framerate (fps) ---
  Entity                                          MCM       Tracer     AbsErr  RelErr%   Status
  x264enc0_src                                  30.01        30.00       0.01     0.0%     PASS
  rtph264pay0_src                               30.01        30.00       0.01     0.0%     PASS
```

### Exit codes

| Code | Meaning |
|------|---------|
| 0 | All comparison points within threshold |
| 1 | One or more points exceeded threshold |
| 2 | No overlapping data found (tracers not detected or no matching entities) |

## Interpreting Results

- **< 5% relative error** on framerate, bitrate, and inter-buffer interval is expected.
  These metrics use nearly identical measurement approaches.
- **Larger divergence on pipeline latency** is normal and valuable. PTS-matching and
  event-injection measure fundamentally different things. Document the delta.
- **Processing time** may diverge if elements have multiple sink/src pads, since MCM
  and the core tracer may pick different pad pairs for the measurement.
