# MCM Stats API Contract

All endpoints are served under `/stats/streams`. This document is the
authoritative reference for the remodeled hierarchical stats API.

Requires `--pipeline-analysis-level lite|full` to be set at startup (or
enabled at runtime via the level API).

**Companion documents:**

- [`stats-api-data-model-contract.md`](stats-api-data-model-contract.md) —
  Authoritative type reference for every struct and enum in the
  `StreamsSnapshot` hierarchy, including nullable field conventions and
  serialization details.
- [`stats-api-sample-collection-methodology.md`](stats-api-sample-collection-methodology.md) —
  Statistical methodology: how raw observations are collected, stored, and
  turned into the derived metrics exposed by this API.

---

## Table of Contents

- [Data Model Overview](#data-model-overview)
- [Data Query Endpoints](#data-query-endpoints)
  - [GET /stats/streams/snapshot](#get-statsstreams-snapshot)
  - [GET /stats/streams/snapshot/ws](#get-statsstreams-snapshotws)
- [Control Endpoints](#control-endpoints)
  - [POST /stats/streams/reset](#post-statsstreamsreset)
  - [GET /stats/streams/level](#get-statsstreamslevel)
  - [POST /stats/streams/level](#post-statsstreamslevel)
  - [GET /stats/streams/window-size](#get-statsstreamswindow-size)
  - [POST /stats/streams/window-size](#post-statsstreamswindow-size)
- [Type Reference](#type-reference)

---

## Data Model Overview

The API exposes a hierarchical graph model:

```
StreamsSnapshot (root)
├── stats: FleetStats            — fleet-level health aggregate
└── streams: StreamSnapshot[]
    ├── stats: StreamStats       — stream-level health aggregate
    └── pipelines: PipelineSnapshot[]
        ├── stats: PipelineStats — pipeline metrics, summary, root cause
        ├── connections: PipelineConnection[]
        ├── elements: ElementSnapshot[] — recursive (bins have is_bin + children)
        │   ├── is_bin: bool          — true for GStreamer bins
        │   ├── children: ElementSnapshot[] — nested elements (only when is_bin)
        │   ├── thread_id: Option<u32> — links to ThreadSummary.id
        │   ├── stats: ElementStats
        │   ├── connections: ElementConnection[]
        │   └── pads: PadSnapshot[]
        └── threads: ThreadSummary[] — flat summary, no nested elements
            ├── stats: ThreadStats
            └── connections: ThreadConnection[]
```

**Key design decisions:**

- All collections use `Vec` (not `HashMap`) for performance and
  deterministic ordering.
- Elements form a recursive tree. Bins are `ElementSnapshot`s with
  `is_bin: true` and nested `children`. Threads are a flat summary;
  elements link via `thread_id`.
- Metrics are nested in `*Stats` structs at each level.
- Graph edges (connections) are embedded on the owning entity.
- Element diagnostics (stutter, freeze) are inlined into `ElementStats`.
- Controls are global only (no per-stream or per-pipeline controls).
- One MCM stream maps to one or more GStreamer pipelines (source + sinks).

---

## Data Query Endpoints

### GET /stats/streams/snapshot

Returns the consolidated hierarchical snapshot of all streams, pipelines,
threads, elements, and pads.

**Query parameters:**

| Parameter      | Type   | Default | Range | Description |
|----------------|--------|---------|-------|-------------|
| `buffer_limit` | number | 0       | 0–∞   | Max raw records to include per pad. 0 omits the `buffer` array (reduces payload). Values > 300 are clamped to 300. |

**Response:** `200 OK` — `StreamsSnapshot`

```json
{
  "timestamp_ns": 1739700000000000000,
  "stats": {
    "overall_health": "good",
    "streams_total": 2,
    "streams_good": 2,
    "streams_degraded": 0,
    "streams_bad": 0,
    "total_cpu_pct": 12.3,
    "total_throughput_fps": 29.8,
    "dominant_issue": "unknown"
  },
  "streams": [
    {
      "id": "a1b2c3d4-...",
      "name": "ball - Fake source",
      "running": true,
      "video_and_stream": {
        "name": "ball - Fake source",
        "stream_information": {
          "endpoints": ["udp://127.0.0.1:5610"],
          "configuration": { "type": "video", "encode": "H264", "height": 1080, "width": 1920, "frame_interval": { "numerator": 1, "denominator": 30 } },
          "extended_configuration": { "thermal": false, "disable_mavlink": false, "disable_zenoh": false, "disable_thumbnails": false }
        },
        "video_source": { "Gst": { "name": "Fake source", "source": { "Fake": "ball" } } }
      },
      "mavlink": { "system_id": 1, "component_id": 106 },
      "stats": {
        "health": "good",
        "dominant_issue": "unknown",
        "throughput_fps": 29.8,
        "cpu_pct": 12.3,
        "freshness_delay_ms": 2.1,
        "root_cause_candidates": []
      },
      "pipelines": [
        {
          "name": "pipeline-source-a1b2c3d4",
          "stats": {
            "level": "full",
            "window_size": 900,
            "expected_interval_ms": 33.33,
            "uptime_secs": 120.5,
            "health": "good",
            "dominant_issue": "unknown",
            "cpu_pct": 8.1,
            "cpu_stats": { "count": 120, "min": 5.0, "max": 12.0, "mean": 8.1, "std": 1.2 },
            "summary": {
              "total_frames": 3600,
              "throughput_fps": 29.8,
              "total_pipeline_freshness_delay_ms": 2.1,
              "total_pipeline_causal_latency_ms": null,
              "causal_latency_health": null,
              "verdict": "Healthy"
            },
            "system": {
              "sample_count": 120,
              "current_cpu_pct": 35.0,
              "current_load_1m": 1.5,
              "current_mem_used_pct": 45.0,
              "current_temperature_c": 55.0,
              "cpu_stats": { "...": "..." },
              "load_stats": { "...": "..." },
              "mem_stats": { "...": "..." },
              "temp_stats": { "...": "..." }
            },
            "restarts": {
              "start_count": 1,
              "restart_count": 0,
              "current_uptime_secs": 120.5,
              "total_tracked_secs": 120.5,
              "avg_restart_interval_secs": 0.0,
              "min_restart_interval_secs": 0.0,
              "last_restart_interval_secs": 0.0
            },
            "root_cause_candidates": [],
            "thread_bottlenecks": []
          },
          "connections": [
            {
              "to_pipeline": "pipeline-udp-sink-a1b2c3d4",
              "bridge_type": "proxy_sink",
              "from_element": "tee_video",
              "to_element": "udp_sink"
            }
          ],
          "elements": [
            {
              "name": "v4l2src0",
              "element_type": "v4l2src",
              "thread_id": 12345,
              "stats": {
                "processing_time_us": null,
                "processing_time_stats": null,
                "is_cross_thread": null,
                "cpu_pct": 4.2,
                "health": "good",
                "stutter_events": 0,
                "freeze_events": 0,
                "max_freeze_ms": 0.0,
                "stutter_ratio": 0.0
              },
              "connections": [
                {
                  "to_element": "capsfilter0",
                  "freshness_delay_ms": 0.5,
                  "causal_latency_ms": { "count": 850, "min": 0.1, "...": "..." },
                  "causal_match_rate": 0.98,
                  "causal_matched_samples": 833,
                  "causal_confidence": "high"
                }
              ],
              "pads": [
                {
                  "name": "src",
                  "direction": "src",
                  "stats": {
                    "level": "full",
                    "total_buffers": 3600,
                    "total_keyframes": 120,
                    "total_delta_frames": 3480,
                    "total_dropped": 0,
                    "drop_ratio": 0.0,
                    "bitrate_bps": 5120000.0,
                    "avg_gop_size": 30.0,
                    "last_wall_ns": 1739700000000000000,
                    "accumulators": null,
                    "distribution": {
                      "interval": { "count": 3599, "min": 30.0, "...": "..." },
                      "i_interval": { "...": "..." },
                      "p_interval": { "...": "..." },
                      "size": { "...": "..." },
                      "i_size": { "...": "..." },
                      "p_size": { "...": "..." }
                    }
                  },
                  "buffer": [],
                  "connections": [
                    {
                      "peer_element": "capsfilter0",
                      "peer_pad": "sink",
                      "media_type": "video/x-h264"
                    }
                  ]
                }
              ]
                }
              ],
          "threads": [
            {
              "id": 12345,
              "stats": {
                "name": "src-streaming",
                "cpu_pct": 8.1,
                "cpu_stats": { "...": "..." }
              },
              "connections": []
            }
          ]
        }
      ]
    }
  ]
}
```

When no pipelines are active or analysis is disabled:

```json
{
  "timestamp_ns": 1739700000000000000,
  "stats": {
    "overall_health": "unknown",
    "streams_total": 0,
    "streams_good": 0,
    "streams_degraded": 0,
    "streams_bad": 0,
    "total_cpu_pct": 0.0,
    "total_throughput_fps": 0.0,
    "dominant_issue": "unknown"
  },
  "streams": []
}
```

**Caching:** Results are cached for 900 ms. Concurrent callers share the
same computation.

---

### GET /stats/streams/snapshot/ws

Streams `StreamsSnapshot` at a configurable interval via WebSocket.

After the HTTP upgrade, the server pushes JSON text frames at the specified
interval. The client does not need to send any messages. Sending a Close
frame or disconnecting terminates the stream.

**Query parameters:**

| Parameter      | Type   | Default | Range   | Description |
|----------------|--------|---------|---------|-------------|
| `interval_ms`  | number | 1000    | 500–∞   | Push interval in milliseconds. Values below 500 are clamped to 500. |
| `buffer_limit` | number | 0       | 0–∞     | Max raw records per pad. Values > 300 are clamped to 300. |

**Message format:** JSON text frame — `StreamsSnapshot`

**Note:** The snapshot cache has a 900 ms TTL. At `interval_ms=500`, every
other push may serve cached (identical) data.

---

## Control Endpoints

All controls are global — they affect all pipelines simultaneously.

### POST /stats/streams/reset

Reset all statistics: clears rolling windows, counters, and CPU history.
Re-enables recording.

**Request body:** None.

**Response:** `200 OK`

```json
{"status": "reset"}
```

---

### GET /stats/streams/level

Get the current global stats level.

**Response:** `200 OK`

```json
{"level": "full"}
```

---

### POST /stats/streams/level

Set the global stats level. Only affects newly created pipelines; existing
probes keep their original level until the pipeline restarts.

**Request body:**

```json
{"level": "lite"}
```

| Field   | Type   | Values             | Description |
|---------|--------|--------------------|-------------|
| `level` | string | `"lite"`, `"full"` | Target instrumentation level. |

**Response:** `200 OK`

```json
{"level": "lite"}
```

**Error:** `400 Bad Request` if level is not `"lite"` or `"full"`.

---

### GET /stats/streams/window-size

Get the current global window size (Full backend ring buffer capacity).

**Response:** `200 OK`

```json
{"window_size": 900}
```

---

### POST /stats/streams/window-size

Set the global window size. Only affects newly created pipelines.

**Request body:**

```json
{"window_size": 900}
```

| Field         | Type   | Range    | Description |
|---------------|--------|----------|-------------|
| `window_size` | number | 1–50,000 | Ring buffer capacity (default 900, ~30s at 30fps). |

**Response:** `200 OK`

```json
{"window_size": 900}
```

**Error:** `400 Bad Request` if window_size is 0 or exceeds 50,000.

---

## Type Reference

### Enums

#### StatsLevel

```
"lite" | "full"
```

#### HealthStatus

```
"good" | "degraded" | "bad" | "unknown"
```

#### IssueKind

```
"cpu_saturation" | "freeze_risk" | "latency_spike" | "causal_match_low" | "unknown"
```

#### CausalConfidence

```
"low" | "medium" | "high"
```

#### PadDirection

```
"sink" | "src"
```

---

### Root Snapshot

#### StreamsSnapshot

| Field          | Type             | Description |
|----------------|------------------|-------------|
| `timestamp_ns` | number (u64)     | Server wall-clock time (ns since Unix epoch). |
| `stats`        | FleetStats       | Fleet-level health aggregate. |
| `streams`      | StreamSnapshot[] | Per-stream snapshots. |

#### FleetStats

| Field              | Type         | Description |
|--------------------|--------------|-------------|
| `overall_health`   | HealthStatus | Worst health across all streams. |
| `streams_total`    | number (u64) | Total stream count. |
| `streams_good`     | number (u64) | Count of streams with health = "good". |
| `streams_degraded` | number (u64) | Degraded stream count. |
| `streams_bad`      | number (u64) | Bad stream count. |
| `total_cpu_pct`    | number       | Sum of all stream CPU percentages. |
| `total_throughput_fps` | number   | Sum of all stream throughputs (frames/sec). |
| `dominant_issue`   | IssueKind    | Primary issue across the fleet. |

---

### Stream Level

#### StreamSnapshot

| Field              | Type                    | Description |
|--------------------|-------------------------|-------------|
| `id`               | string                  | Stream identifier (UUID). |
| `name`             | string                  | Human-readable stream name from the stream configuration. |
| `running`          | boolean                 | Whether the stream is currently running. |
| `error`            | string?                  | Error message if the stream is in an error state. Omitted from JSON when null. |
| `video_and_stream` | VideoAndStreamInformation | Full stream configuration (name, endpoints, encode settings, video source). |
| `mavlink`          | MavlinkComponent?       | MAVLink camera component info. Omitted from JSON when null. |
| `stats`            | StreamStats              | Stream-level aggregate statistics. |
| `pipelines`        | PipelineSnapshot[]       | Pipelines belonging to this stream. |

Stream manager data (same as `/streams`) is merged into each stream entry.

#### StreamStats

| Field                   | Type                  | Description |
|-------------------------|-----------------------|-------------|
| `health`                | HealthStatus          | Worst health across this stream's pipelines. `Unknown` for streams without pipeline analysis data. |
| `dominant_issue`        | IssueKind             | Primary issue in this stream. |
| `throughput_fps`        | number                | Sum of pipeline throughputs. |
| `cpu_pct`               | number                | Sum of pipeline CPU usage. |
| `freshness_delay_ms`    | number                | Max freshness delay across pipelines. |
| `root_cause_candidates` | RootCauseCandidate[]  | Merged root cause candidates from all pipelines. |

---

### Pipeline Level

#### PipelineSnapshot

| Field         | Type                   | Description |
|---------------|------------------------|-------------|
| `name`        | string                 | GStreamer pipeline name. |
| `stats`       | PipelineStats          | Pipeline-level metrics. |
| `connections` | PipelineConnection[]   | Links to other pipelines (proxy bridges). |
| `elements`    | ElementSnapshot[]      | Top-level elements. Bins have `is_bin: true` with nested `children`. |
| `threads`     | ThreadSummary[]        | Flat summary of streaming threads (no nested elements). Elements link via `thread_id`. |

#### PipelineStats

| Field                   | Type                  | Nullable | Description |
|-------------------------|-----------------------|----------|-------------|
| `level`                 | StatsLevel            | no       | Active instrumentation level. |
| `window_size`           | number                | no       | Ring buffer capacity. |
| `expected_interval_ms`  | number                | no       | Nominal frame interval (ms). |
| `uptime_secs`           | number                | no       | Seconds since pipeline creation. |
| `health`                | HealthStatus          | no       | Pipeline health classification. |
| `dominant_issue`        | IssueKind             | no       | Primary issue identified in this pipeline. |
| `cpu_pct`               | number                | yes      | Instantaneous pipeline CPU %. |
| `cpu_stats`             | SystemDistribution    | yes      | Windowed pipeline CPU stats. |
| `summary`               | PipelineSummary       | no       | Aggregate health summary. |
| `system`                | SystemSnapshot        | no       | Host-level metrics. |
| `restarts`              | RestartSnapshot       | no       | Pipeline restart statistics. |
| `root_cause_candidates` | RootCauseCandidate[]  | no       | Ranked root cause candidates. |
| `thread_bottlenecks`    | ThreadBottleneck[]    | no       | Threads with high CPU. |

#### PipelineConnection

| Field          | Type   | Description |
|----------------|--------|-------------|
| `to_pipeline`  | string | Target pipeline name. |
| `bridge_type`  | string | Connection type (e.g. `"proxy_sink"`). |
| `from_element` | string | Source element in this pipeline. |
| `to_element`   | string | Target element in the other pipeline. |

#### PipelineSummary

| Field                                | Type             | Nullable | Description |
|--------------------------------------|------------------|----------|-------------|
| `total_frames`                       | number (u64)     | no       | Frames at the most-active element. |
| `throughput_fps`                     | number           | no       | Estimated frames/sec. |
| `total_pipeline_freshness_delay_ms`  | number           | no       | Sum of edge freshness deltas (ms). |
| `total_pipeline_causal_latency_ms`   | Distribution     | yes      | Sum of per-edge causal latency. |
| `causal_latency_health`              | CausalConfidence | yes      | Weighted aggregate confidence. |
| `verdict`                            | string           | no       | Human-readable health verdict. |

---

### Thread Level

#### ThreadSummary

Flat summary of a streaming thread. No nested elements; elements link via
`ElementSnapshot.thread_id`.

| Field         | Type                 | Nullable | Description |
|---------------|----------------------|----------|-------------|
| `id`          | number (u32)         | no       | Linux kernel TID. |
| `name`        | string               | yes      | Thread name from `/proc`. Omitted from JSON when null. |
| `stats`       | ThreadStats          | no       | Thread-level metrics. |
| `connections` | ThreadConnection[]   | no       | Links to other threads (via queue elements). |

#### ThreadStats

| Field       | Type               | Nullable | Description |
|-------------|---------------------|----------|-------------|
| `name`      | string             | yes      | Thread name from `/proc`. |
| `cpu_pct`   | number             | no       | Instantaneous CPU % (1-sec delta). |
| `cpu_stats` | SystemDistribution | yes      | Windowed CPU stats (up to 120 samples). |

#### ThreadConnection

| Field         | Type   | Description |
|---------------|--------|-------------|
| `to_thread`   | number (u32) | Target thread ID. |
| `via_element` | string | Queue element bridging the threads. |

---

### Element Level

#### ElementSnapshot

When `is_bin` is true, this represents a GStreamer bin and `children`
contains its direct child elements (which may themselves be bins).

| Field          | Type                  | Description |
|----------------|-----------------------|-------------|
| `name`         | string                | GStreamer element instance name. |
| `element_type` | string                | Factory/plugin name (e.g. `"v4l2src"`, `"rtspsrc"`). |
| `is_bin`       | boolean               | `true` for bins. Omitted from JSON when `false`. |
| `children`     | ElementSnapshot[]     | Child elements (only when `is_bin`). Omitted from JSON when empty. |
| `thread_id`    | number (u32)?         | Thread ID executing this element. Links to `ThreadSummary.id`. Omitted from JSON when null. |
| `state`        | string?               | Current GStreamer state. Omitted from JSON when null. |
| `properties`   | ElementProperty[]?    | GObject properties. Omitted from JSON when null. |
| `stats`        | ElementStats          | Element-level metrics + diagnostics. |
| `connections`  | ElementConnection[]   | Links to downstream elements. |
| `pads`         | PadSnapshot[]         | All pads (sink and src combined). |

#### ElementStats

Includes inlined diagnostics (stutter, freeze) — no separate diagnostics
endpoint.

| Field                | Type         | Nullable | Description |
|----------------------|--------------|----------|-------------|
| `processing_time_us` | number       | yes      | Per-buffer causal processing time (µs, mean). Filter elements only. Lite mode: atomic accumulator (`src_departure − sink_arrival`). Full mode: PTS-matched intra-element distribution mean. |
| `processing_time_stats` | Distribution | yes   | Windowed processing time distribution (Full mode only). PTS-matched per-buffer transit time across the element's own sink→src pads. |
| `is_cross_thread`    | boolean      | yes      | `true` when the element's sink and src pads run on different streaming threads (e.g. `queue`, `queue2`). When cross-thread, `processing_time_us` includes queuing delay. |
| `cpu_pct`            | number       | yes      | Attributed CPU usage (%). |
| `cpu_stats`          | Distribution | yes      | Windowed element CPU distribution (1 Hz, up to 120 samples). |
| `health`             | HealthStatus | no       | Element health: good/degraded/bad. |
| `stutter_events`     | number (u64) | no       | Count of intervals exceeding stutter threshold. |
| `freeze_events`      | number (u64) | no       | Count of intervals exceeding freeze threshold. |
| `max_freeze_ms`      | number       | no       | Longest freeze duration (ms). |
| `stutter_ratio`      | number       | no       | Fraction of intervals that stutter. |
| `queue_stats`        | QueueStats? | yes      | Queue fill-level stats. Omitted from JSON when null. |

#### ElementConnection

Inter-element delay measurement for one topology edge.

| Field                    | Type             | Nullable | Description |
|--------------------------|------------------|----------|-------------|
| `to_element`             | string           | no       | Downstream element name. |
| `freshness_delay_ms`     | number           | no       | Non-causal freshness delta (ms). |
| `causal_latency_ms`      | Distribution     | yes      | PTS-matched latency distribution (ms). Full mode only. |
| `causal_match_rate`      | number           | yes      | Fraction of upstream PTS values matched downstream. |
| `causal_matched_samples` | number (u64)     | yes      | Count of matched sample pairs. |
| `causal_confidence`      | CausalConfidence | yes      | Quality assessment of causal data. |

---

### Pad Level

#### PadSnapshot

| Field         | Type             | Description |
|---------------|------------------|-------------|
| `name`        | string           | Pad instance name (e.g. `"src"`, `"sink"`). |
| `direction`   | PadDirection     | `"sink"` or `"src"`. |
| `caps`        | string?          | Current negotiated caps string. Omitted from JSON when null. |
| `stats`       | PadStats         | Pad-level metrics. |
| `buffer`      | RawRecord[]      | Raw records from ring buffer (Full mode, controlled by `buffer_limit`). Omitted when empty. |
| `connections` | PadConnection[]  | Links to downstream pads. |

#### PadStats

| Field              | Type                 | Nullable | Description |
|--------------------|----------------------|----------|-------------|
| `level`            | StatsLevel           | no       | `"lite"` or `"full"`. |
| `total_buffers`    | number (u64)         | no       | Lifetime buffer count. |
| `total_keyframes`  | number (u64)         | no       | Lifetime keyframe count. |
| `total_delta_frames`| number (u64)        | no       | Lifetime delta-frame count. |
| `total_dropped`     | number (u64)        | no       | Lifetime dropped buffer count. |
| `drop_ratio`        | number              | no       | Fraction of buffers dropped. |
| `bitrate_bps`       | number              | yes      | Estimated bitrate in bits/sec. |
| `avg_gop_size`      | number              | yes      | Average GOP size. |
| `keyframe_interval_ms` | Distribution     | yes      | Keyframe interval distribution (ms). |
| `last_wall_ns`     | number (u64)         | no       | Wall time of most recent buffer. |
| `accumulators`     | AccumulatorSnapshot  | yes      | Present when `level = "lite"`. |
| `distribution`     | DistributionSnapshot | yes      | Present when `level = "full"`. |

#### PadConnection

| Field          | Type   | Nullable | Description |
|----------------|--------|----------|-------------|
| `peer_element` | string | no       | Destination element name. |
| `peer_pad`     | string | no       | Destination pad name. |
| `media_type`   | string | yes      | Negotiated media type (e.g. `"video/x-h264"`). |

---

### Shared Types

#### RawRecord

A single buffer observation from a pad probe.

| Field        | Type           | Nullable | Description |
|--------------|----------------|----------|-------------|
| `wall_ns`    | number (u64)   | no       | Wall-clock time in nanoseconds since Unix epoch. |
| `pts_ns`     | number (u64)   | yes      | Buffer PTS in nanoseconds. Omitted when absent. |
| `size`       | number (u32)   | no       | Buffer payload size in bytes. |
| `is_keyframe`| boolean        | no       | `true` if intra-coded frame. |

#### AccumulatorSnapshot

Lite backend cumulative statistics (per-pad). Differentiate two successive
snapshots to get windowed rates.

| Field                  | Type         | Description |
|------------------------|--------------|-------------|
| `sum_interval_ns`      | number (u64) | Cumulative sum of inter-buffer intervals (ns). |
| `sum_interval_sq_us`   | number (u64) | Sum of squared intervals (us^2). |
| `sum_size_bytes`       | number (u64) | Cumulative sum of buffer sizes (bytes). |
| `sum_size_sq_units`    | number (u64) | Sum of squared sizes ((bytes/1024)^2). |
| `interval_count`       | number (u64) | Number of intervals recorded. |
| `mean_interval_ms`     | number       | Pre-computed mean interval (ms). |
| `std_interval_ms`      | number       | Pre-computed std deviation of interval (ms). |
| `min_interval_ms`      | number       | Minimum observed interval (ms). |
| `max_interval_ms`      | number       | Maximum observed interval (ms). |
| `mean_size_bytes`      | number       | Pre-computed mean buffer size (bytes). |
| `std_size_bytes`       | number       | Pre-computed std deviation of size (bytes). |

#### Distribution

Full backend statistical distribution summary.

| Field    | Type         | Description |
|----------|--------------|-------------|
| `count`  | number (u64) | Number of samples. |
| `min`    | number       | Minimum value. |
| `max`    | number       | Maximum value. |
| `mean`   | number       | Arithmetic mean. |
| `std`    | number       | Population standard deviation. |
| `median` | number       | 50th percentile (nearest-rank). |
| `p95`    | number       | 95th percentile (nearest-rank). |
| `p99`    | number       | 99th percentile (nearest-rank). |

#### DistributionSnapshot

Full backend per-pad distributions (6 metrics).

| Field          | Type         | Description |
|----------------|--------------|-------------|
| `interval`     | Distribution | All inter-buffer intervals (ms). |
| `i_interval`   | Distribution | Intervals preceding keyframes only (ms). |
| `p_interval`   | Distribution | Intervals preceding delta-frames only (ms). |
| `size`         | Distribution | All buffer sizes (bytes). |
| `i_size`       | Distribution | Keyframe sizes only (bytes). |
| `p_size`       | Distribution | Delta-frame sizes only (bytes). |

#### SystemDistribution

Simplified distribution for system/CPU metrics (no percentiles).

| Field   | Type         | Description |
|---------|--------------|-------------|
| `count` | number (u64) | Number of samples in the ring buffer. |
| `min`   | number       | Minimum value. |
| `max`   | number       | Maximum value. |
| `mean`  | number       | Arithmetic mean. |
| `std`   | number       | Population standard deviation. |

#### SystemSnapshot

| Field                  | Type               | Description |
|------------------------|--------------------|-------------|
| `sample_count`         | number (u64)       | Number of 1-sec samples in the ring buffer. |
| `current_cpu_pct`      | number             | Most recent host CPU %. |
| `current_load_1m`      | number             | Most recent 1-min load average. |
| `current_mem_used_pct` | number             | Most recent memory usage %. |
| `current_temperature_c`| number             | Most recent CPU temperature (Celsius). |
| `cpu_stats`            | SystemDistribution | Windowed CPU stats. |
| `load_stats`           | SystemDistribution | Windowed load stats. |
| `mem_stats`            | SystemDistribution | Windowed memory stats. |
| `temp_stats`           | SystemDistribution | Windowed temperature stats. |

#### RestartSnapshot

| Field                       | Type         | Description |
|-----------------------------|--------------|-------------|
| `start_count`               | number (u64) | Total starts (1 = never restarted). |
| `restart_count`             | number (u64) | Number of restarts (start_count - 1). |
| `current_uptime_secs`       | number       | Seconds since most recent start. |
| `total_tracked_secs`        | number       | Seconds since first start. |
| `avg_restart_interval_secs` | number       | Average seconds between restarts. |
| `min_restart_interval_secs` | number       | Fastest restart interval. |
| `last_restart_interval_secs`| number       | Most recent restart interval. |

---

### Health and Root Cause Types

#### RootCauseCandidate

| Field  | Type      | Description |
|--------|-----------|-------------|
| `cause`| IssueKind | Candidate root cause. |
| `score`| number    | Severity score (higher = more likely). |

#### ThreadBottleneck

| Field              | Type     | Nullable | Description |
|--------------------|----------|----------|-------------|
| `thread_id`        | number   | no       | Linux kernel TID. |
| `thread_name`      | string   | yes      | Thread name. |
| `cpu_pct`          | number   | no       | Thread CPU %. |
| `elements`         | string[] | no       | Elements on this thread. |
| `latency_impact_ms`| number   | no       | Estimated latency contribution. |

#### ElementProperty

| Field   | Type   | Description |
|---------|--------|-------------|
| `name`  | string | GObject property name. |
| `value` | string | Property value as string. |

#### QueueStats

| Field                   | Type         | Description |
|-------------------------|--------------|-------------|
| `current_level_buffers` | number (u32) | Current buffer count in queue. |
| `current_level_bytes`   | number (u64) | Current byte count in queue. |
| `current_level_time_ns` | number (u64) | Current time level (ns). |
| `max_level_buffers`     | number (u32) | Max buffer capacity. |
| `max_level_bytes`       | number (u64) | Max byte capacity. |
| `max_level_time_ns`     | number (u64) | Max time capacity (ns). |
| `fill_pct`              | number       | Fill percentage (0–100). |

---

### Query Parameter Types

#### SnapshotQuery

| Field          | Type   | Default | Description |
|----------------|--------|---------|-------------|
| `buffer_limit` | number | 0       | Max raw records per pad. |

#### SnapshotWsQuery

| Field          | Type   | Default | Description |
|----------------|--------|---------|-------------|
| `interval_ms`  | number | 1000    | Push interval (ms). Min 500. |
| `buffer_limit` | number | 0       | Max raw records per pad. |

#### SetLevelRequest

| Field   | Type   | Description |
|---------|--------|-------------|
| `level` | string | `"lite"` or `"full"`. |

#### SetWindowSizeRequest

| Field         | Type   | Description |
|---------------|--------|-------------|
| `window_size` | number | Ring buffer capacity. Range: 1–50,000. |

---

## Migration from Previous API

| Old Endpoint | New Equivalent |
|---|---|
| `GET /stats/pipeline-analysis/full-snapshot` | `GET /stats/streams/snapshot` |
| `GET /stats/pipeline-analysis/full-snapshot/ws` | `GET /stats/streams/snapshot/ws` |
| `POST /stats/pipeline-analysis/reset` | `POST /stats/streams/reset` |
| `POST /stats/pipeline-analysis/{pipeline}/reset` | *(removed — global reset only)* |
| `GET /stats/pipeline-analysis/level` | `GET /stats/streams/level` |
| `POST /stats/pipeline-analysis/level` | `POST /stats/streams/level` |
| `GET /stats/pipeline-analysis/window-size` | `GET /stats/streams/window-size` |
| `POST /stats/pipeline-analysis/window-size` | `POST /stats/streams/window-size` |
| `GET /stats/pipeline-analysis` | *(removed — use snapshot)* |
| `GET /stats/pipeline-analysis/health` | *(inlined into StreamsSnapshot.stats)* |
| `GET /stats/pipeline-analysis/root-cause` | *(inlined into PipelineStats.root_cause_candidates)* |
| `GET /stats/pipeline-analysis/{pipeline}/root-cause` | *(inlined into PipelineStats.root_cause_candidates)* |
| `GET /stats/pipeline-analysis/{pipeline}/elements/{element}/diagnostics` | *(inlined into ElementStats)* |
| `GET /stats/pipeline-analysis/{pipeline}/samples` | *(use PadSnapshot.buffer in snapshot)* |
| `GET /stats/pipeline-analysis/{pipeline}/elements/{element}/samples` | *(use PadSnapshot.buffer in snapshot)* |

**Key structural changes:**

- `HashMap<String, T>` → `Vec<T>` for all collections.
- `FullSnapshot.pipelines` → `StreamsSnapshot.streams[].pipelines[]`.
- `PipelineSnapshot` has `elements` (recursive tree) + `threads` (flat summary). Bins are `ElementSnapshot`s with `is_bin: true` and `children`.
- `ElementSnapshot.sink_pads` / `src_pads` → unified `pads[]` with `direction` field.
- `ElementSnapshot.thread_id` links elements to `ThreadSummary.id`.
- `EdgeDelay` (top-level array) → `ElementConnection` (on source element).
- `ThreadGroup` (top-level array) → `ThreadSummary` (flat, no nested elements).
- `FleetHealthSummary` / `PipelineHealthSummary` → `FleetStats` / `StreamStats`.
- `PipelineRootCause` / `FleetRootCauseSummary` → inlined into `PipelineStats.root_cause_candidates`.
- `ElementDiagnostics` → inlined into `ElementStats`.
- `PipelineSamplesResponse` / `SampleWindowEntry` → raw `RawRecord[]` in `PadSnapshot.buffer`.
- Query param `sample_limit` → `buffer_limit`.
