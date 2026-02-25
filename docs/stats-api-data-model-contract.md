# MCM Stats API — Data Model Contract

This document is the authoritative reference for the data model behind the
MCM Stats API. It describes every type that appears in the `StreamsSnapshot`
hierarchy: what each field means semantically, what units it uses, which
fields are optional, and how clients should interpret the data.

For transport-level details (HTTP endpoints, WebSocket parameters, caching)
see [`stats-api-contract.md`](stats-api-contract.md).
For the statistical methodology behind these numbers see
[`stats-api-sample-collection-methodology.md`](stats-api-sample-collection-methodology.md).

---

## Table of Contents

- [Design Principles](#design-principles)
- [Object Graph](#object-graph)
- [Serialization Format](#serialization-format)
- [TypeScript Bindings](#typescript-bindings)
- [Enums](#enums)
  - [StatsLevel](#statslevel)
  - [HealthStatus](#healthstatus)
  - [IssueKind](#issuekind)
  - [CausalConfidence](#causalconfidence)
  - [PadDirection](#paddirection)
- [Root Level](#root-level)
  - [StreamsSnapshot](#streamssnapshot)
  - [FleetStats](#fleetstats)
- [Stream Level](#stream-level)
  - [StreamSnapshot](#streamsnapshot)
  - [VideoAndStreamInformation](#videoandstreaminformation)
  - [StreamInformation](#streaminformation)
  - [MavlinkComponent](#mavlinkcomponent)
  - [StreamStats](#streamstats)
- [Pipeline Level](#pipeline-level)
  - [PipelineSnapshot](#pipelinesnapshot)
  - [PipelineStats](#pipelinestats)
  - [PipelineConnection](#pipelineconnection)
  - [PipelineSummary](#pipelinesummary)
- [Thread Level](#thread-level)
  - [ThreadSummary](#threadsummary)
  - [ThreadStats](#threadstats)
  - [ThreadConnection](#threadconnection)
- [Element Level](#element-level)
  - [ElementSnapshot](#elementsnapshot)
  - [ElementStats](#elementstats)
  - [ElementConnection](#elementconnection)
- [Pad Level](#pad-level)
  - [PadSnapshot](#padsnapshot)
  - [PadStats](#padstats)
  - [PadConnection](#padconnection)
- [Shared Value Types](#shared-value-types)
  - [RawRecord](#rawrecord)
  - [AccumulatorSnapshot](#accumulatorsnapshot)
  - [Distribution](#distribution)
  - [DistributionSnapshot](#distributionsnapshot)
  - [SystemDistribution](#systemdistribution)
  - [SystemSnapshot](#systemsnapshot)
  - [RestartSnapshot](#restartsnapshot)
- [Health and Root Cause Types](#health-and-root-cause-types)
  - [RootCauseCandidate](#rootcausecandidate)
  - [ThreadBottleneck](#threadbottleneck)
- [Additional Types](#additional-types)
  - [ElementProperty](#elementproperty)
  - [QueueStats](#queuestats)
- [Nullable Field Conventions](#nullable-field-conventions)
- [Numeric Precision Notes](#numeric-precision-notes)

---

## Design Principles

1. **Hierarchical graph.** The data forms a tree:
   Fleet → Streams → Pipelines → Bins (recursive) / Elements → Pads. Elements
   are organized by structural GStreamer bins (e.g., rtspsrc, webrtcbin) and
   direct pipeline children. Threads are a flat summary (no nested elements);
   elements link to threads via `thread_id`. Each level carries its own
   `*Stats` struct with aggregated metrics and a `connections` array describing
   edges to siblings.

2. **Vec, not HashMap.** All collections are `Vec<T>`, never `HashMap`.
   This is intentional: ordered arrays are cheaper to serialize/deserialize
   and preserve insertion order for clients that iterate by index.

3. **Stats + topology co-located.** Every snapshot node (stream, pipeline,
   bin, element, pad) bundles a `stats` object for metrics and a
   `connections` array for graph edges. Threads are a flat summary with stats
   and connections but no nested elements. Clients never need a separate call
   to get topology.

4. **Optional fields are omitted when null.** Fields annotated
   `skip_serializing_if` are **absent from the JSON payload** when their
   value is `None` (for `Option<T>`) or empty (for `Vec<T>` where noted).
   Clients must tolerate missing keys — do not assume every field is always
   present.

5. **Dual-backend awareness.** Pad-level statistics differ by
   instrumentation level. In `lite` mode, `accumulators` is present and
   `distribution` is absent. In `full` mode, the opposite is true. Always
   check `PadStats.level` (or `PipelineStats.level`) before accessing
   backend-specific fields.

6. **Inlined diagnostics.** Element health, stutter, and freeze data are
   inlined into `ElementStats` rather than served from a separate endpoint.
   Similarly, root cause analysis is inlined into `PipelineStats`.

---

## Object Graph

```
StreamsSnapshot (root)
├── stats: FleetStats                 — fleet-wide health aggregate
└── streams: StreamSnapshot[]
    ├── name, running, error, video_and_stream, mavlink — stream metadata
    ├── stats: StreamStats            — per-stream health aggregate
    └── pipelines: PipelineSnapshot[]
        ├── stats: PipelineStats      — pipeline metrics, summary, root cause
        ├── connections: PipelineConnection[]
        ├── elements: ElementSnapshot[] — recursive (bins have is_bin + children)
        │   ├── is_bin: bool          — true for GStreamer bins (rtspsrc, webrtcbin, etc.)
        │   ├── children: ElementSnapshot[] — nested elements (only when is_bin)
        │   ├── thread_id: Option<u32> — links to ThreadSummary.id
        │   ├── stats: ElementStats    — diagnostics inlined
        │   ├── connections: ElementConnection[]
        │   └── pads: PadSnapshot[]
        └── threads: ThreadSummary[]  — flat summary, no nested elements
            ├── stats: ThreadStats
            └── connections: ThreadConnection[]

ElementSnapshot (recursive — bins and elements share this type)
├── is_bin: bool                      — true for bins (omitted from JSON when false)
├── children: ElementSnapshot[]       — bin children (omitted from JSON when empty)
├── thread_id: Option<u32>            — links to ThreadSummary.id
├── stats: ElementStats               — diagnostics inlined
├── connections: ElementConnection[]
└── pads: PadSnapshot[]
    ├── stats: PadStats
    ├── buffer: RawRecord[]            — raw ring-buffer records
    └── connections: PadConnection[]
```

**Mapping to the real world:**

- One **MCM stream** (a user-configured video endpoint) maps to one or more
  **GStreamer pipelines** (source pipeline + sink pipelines joined by proxy
  bridges).
- Each pipeline has a recursive tree of **elements**. Bins (e.g., rtspsrc,
  webrtcbin) are marked with `is_bin: true` and contain nested `children`.
- Each pipeline executes on one or more **streaming threads** (flat summary;
  elements link via `thread_id`).
- Each element exposes one or more **pads** (data ports) where buffers flow.

---

## Serialization Format

The data model is serialized as JSON for all transports:

| Format | Transport        | Content-Type       | Notes |
|--------|------------------|--------------------|-------|
| JSON   | HTTP / WebSocket | `application/json` | Human-readable. `u64` fields serialize as JSON numbers. Optional fields omitted when `None`. |

All types derive `serde::Serialize` + `serde::Deserialize`, so adding
alternative binary formats in the future is straightforward.

---

**Historical note:** The fields `caps` (on `PadSnapshot`), `state`, `properties`
(on `ElementSnapshot`), and `queue_stats` (on `ElementStats`) were historically
behind compile-time feature gates (`pad-caps`, `element-deep-info`). They are now
always present in the API; clients may assume these fields exist (subject to
nullable conventions — `Option` fields are still omitted from JSON when `None`).

---

## TypeScript Bindings

Auto-generated TypeScript definitions are produced by
[ts-rs](https://github.com/Aleph-Alpha/ts-rs) and written to
`frontend/bindings/v1/api.d.ts`. The bindings cover every type in this
document. Rust `u64` fields are mapped to TypeScript `number` (safe for
values up to 2^53).

Clients written in TypeScript should import from the generated bindings
rather than hand-coding interfaces.

---

## Enums

All enums serialize as **lowercase strings** in JSON (via
`#[serde(rename_all = "...")]`).

### StatsLevel

The instrumentation detail level. Determines which pad-level statistics are
available.

| Value    | JSON string | Description |
|----------|-------------|-------------|
| `Lite`   | `"lite"`    | O(1) memory accumulators. Provides cumulative sums for interval/size, plus min/max and pre-computed mean/std. No percentiles, no raw buffer. |
| `Full`   | `"full"`    | O(W) ring buffer storing the last W raw records. Provides full distributions with percentiles (median, p95, p99) and optional raw buffer export. |

### HealthStatus

Tri-state health classification with an unknown sentinel.

| Value      | JSON string   | Semantics |
|------------|---------------|-----------|
| `Good`     | `"good"`      | Operating normally. No issues detected. |
| `Degraded` | `"degraded"`  | One or more soft issues detected (stutter, elevated latency, moderate CPU). Stream is still producing output but quality may be impacted. |
| `Bad`      | `"bad"`       | Severe issues detected (freeze, CPU saturation, stream stalled). Intervention likely needed. |
| `Unknown`  | `"unknown"`   | Health cannot be determined. Typically seen before enough data has been collected, or when analysis is disabled. |

Health propagates **upward**: a stream's health is the worst of its
pipelines' health. The fleet health is the worst across all streams.

### IssueKind

Classifies the dominant problem detected by root cause analysis.

| Value             | JSON string          | Semantics |
|-------------------|----------------------|-----------|
| `CpuSaturation`   | `"cpu_saturation"`   | A streaming thread is consuming a disproportionate share of CPU, causing scheduling delays. |
| `FreezeRisk`       | `"freeze_risk"`      | One or more elements have freeze-level gaps (typically >500 ms) in their buffer intervals. |
| `LatencySpike`     | `"latency_spike"`    | Causal (PTS-matched) latency on one or more edges is abnormally high. |
| `CausalMatchLow`   | `"causal_match_low"` | PTS match rate is low, meaning causal latency measurements are unreliable. This is informational — it flags data quality, not a pipeline problem. |
| `Unknown`          | `"unknown"`          | No specific issue identified, or analysis data is insufficient. |

### CausalConfidence

Confidence assessment of PTS-matched causal latency measurements on a
single edge or aggregated across a pipeline.

| Value    | JSON string  | Criteria |
|----------|--------------|----------|
| `Low`    | `"low"`      | Fewer than 20 matched samples, or match rate < 0.4. Results are unreliable. |
| `Medium` | `"medium"`   | At least 20 matched samples and match rate >= 0.4. Results are usable but should be interpreted with care. |
| `High`   | `"high"`     | At least 50 matched samples and match rate >= 0.8. Results are reliable. |

### PadDirection

Direction of a pad within an element.

| Value  | JSON string | Description |
|--------|-------------|-------------|
| `Sink` | `"sink"`    | Input pad — receives buffers from upstream. |
| `Src`  | `"src"`     | Output pad — sends buffers downstream. |

---

## Root Level

### StreamsSnapshot

The top-level object returned by every data query endpoint. Represents a
point-in-time snapshot of the entire system.

| Field          | Type               | Nullable | JSON key        | Description |
|----------------|--------------------|----------|-----------------|-------------|
| `timestamp_ns` | `u64`             | no       | `timestamp_ns`  | Server wall-clock time at snapshot creation, in nanoseconds since Unix epoch (`CLOCK_REALTIME`). |
| `stats`        | `FleetStats`      | no       | `stats`         | Fleet-wide health aggregate. |
| `streams`      | `Vec<StreamSnapshot>` | no   | `streams`       | Per-stream snapshots. Empty array when no streams are configured or analysis is disabled. |

### FleetStats

Aggregate health across all streams. Always present, even when `streams`
is empty (in which case `overall_health` is `"unknown"` and all counters
are 0).

| Field              | Type           | JSON key            | Description |
|--------------------|----------------|---------------------|-------------|
| `overall_health`   | `HealthStatus` | `overall_health`    | Worst health across all streams. |
| `streams_total`    | `u64`          | `streams_total`     | Total number of streams in this snapshot. |
| `streams_good`     | `u64`          | `streams_good`      | Count of **running** streams with `health = "good"`. Non-running streams and streams with `Unknown` health are not counted here. |
| `streams_degraded` | `u64`          | `streams_degraded`  | Count of running streams with `health = "degraded"`. |
| `streams_bad`      | `u64`          | `streams_bad`       | Count of running streams with `health = "bad"` **or** `health = "unknown"`. Running streams with no pipeline analysis data yet are counted as bad. |
| `total_cpu_pct`    | `f64`          | `total_cpu_pct`     | Sum of all stream CPU percentages. |
| `total_throughput_fps` | `f64`     | `total_throughput_fps` | Sum of all stream throughputs (frames/sec). |
| `dominant_issue`   | `IssueKind`    | `dominant_issue`    | The most prevalent issue across the fleet. `"unknown"` when no issues detected. |

---

## Stream Level

### StreamSnapshot

One MCM stream. A stream is a user-configured video endpoint (e.g., a
camera feed routed to one or more sinks). Each stream may contain multiple
GStreamer pipelines.

| Field               | Type                     | Nullable | JSON key        | Description |
|---------------------|--------------------------|----------|-----------------|-------------|
| `id`                | `String`                 | no       | `id`            | Stream identifier (UUID). Matches the stream ID used elsewhere in the MCM API. |
| `name`              | `String`                 | no       | `name`          | Human-readable stream name. |
| `running`           | `bool`                   | no       | `running`       | Whether the stream is currently running. When `false`, `stats.health` is `"unknown"`. |
| `error`             | `Option<String>`         | **yes**  | `error`         | Error message if the stream failed to start or stopped with an error. Omitted from JSON when `None`. |
| `video_and_stream`  | `VideoAndStreamInformation` | no     | `video_and_stream` | Video source, stream name, and capture configuration. See type definition below. |
| `mavlink`           | `Option<MavlinkComponent>` | **yes** | `mavlink`       | MAVLink system/component IDs when MAVLink integration is enabled. Omitted from JSON when `None`. |
| `stats`             | `StreamStats`            | no       | `stats`         | Stream-level aggregate statistics. |
| `pipelines`         | `Vec<PipelineSnapshot>` | no       | `pipelines`     | GStreamer pipelines belonging to this stream. Typically one source pipeline and one or more sink pipelines. |

#### VideoAndStreamInformation

Video source and stream configuration (from `api/src/v1/stream.rs`).

| Field                | Type                | JSON key               | Description |
|----------------------|---------------------|------------------------|-------------|
| `name`               | `String`            | `name`                 | Stream display name. |
| `stream_information` | `StreamInformation` | `stream_information`   | Endpoints, capture config, and extended options. See below. |
| `video_source`       | `VideoSourceType`   | `video_source`         | Video source type and parameters (Gst, Local, Onvif, Redirect variants). |

#### StreamInformation

| Field                   | Type                         | Nullable | JSON key                 | Description |
|-------------------------|------------------------------|----------|--------------------------|-------------|
| `endpoints`             | `Vec<Url>`                   | no       | `endpoints`              | Stream output URLs (e.g., UDP, RTSP). |
| `configuration`        | `CaptureConfiguration`       | no       | `configuration`          | Video capture configuration (encode, resolution, frame interval). |
| `extended_configuration`| `Option<ExtendedConfiguration>` | **yes** | `extended_configuration` | Optional flags (thermal, disable_mavlink, etc.). Omitted from JSON when `None`. |

#### MavlinkComponent

MAVLink component identifiers for vehicle integration.

| Field          | Type  | JSON key       | Description |
|----------------|-------|----------------|-------------|
| `system_id`    | `u8`  | `system_id`    | MAVLink system ID. |
| `component_id` | `u8`  | `component_id` | MAVLink component ID. |

### StreamStats

Aggregated view of a single stream's health, derived from its pipelines.
Computed by `compute_stream_stats(pipelines, running)` — the `running` parameter
ensures non-running streams get `health = "unknown"`. Running streams with no
pipeline analysis data yet (e.g., during startup) also receive `Unknown` health.

| Field                   | Type                     | JSON key                  | Description |
|-------------------------|--------------------------|---------------------------|-------------|
| `health`                | `HealthStatus`           | `health`                  | Worst health across this stream's pipelines. |
| `dominant_issue`        | `IssueKind`              | `dominant_issue`          | Primary issue in this stream (from root cause analysis). |
| `throughput_fps`        | `f64`                    | `throughput_fps`          | **Sum** of throughput (frames/sec) across all pipelines. For a stream with one source and two sinks, this is the sum of all three pipeline throughputs. |
| `cpu_pct`               | `f64`                    | `cpu_pct`                 | **Sum** of pipeline CPU usage (% of one core). |
| `freshness_delay_ms`    | `f64`                    | `freshness_delay_ms`      | **Maximum** freshness delay (ms) across all pipelines. Represents the worst-case data staleness. |
| `root_cause_candidates` | `Vec<RootCauseCandidate>`| `root_cause_candidates`   | Merged root cause candidates from all pipelines, sorted by descending score. |

---

## Pipeline Level

### PipelineSnapshot

One GStreamer pipeline within a stream.

| Field         | Type                     | JSON key       | Description |
|---------------|--------------------------|----------------|-------------|
| `name`        | `String`                 | `name`         | GStreamer pipeline name (e.g., `"pipeline-source-a1b2c3d4"`). Unique within the snapshot. |
| `stats`       | `PipelineStats`          | `stats`        | Pipeline-level metrics. |
| `connections` | `Vec<PipelineConnection>`| `connections`  | Links to other pipelines (proxy bridges connecting source to sinks). |
| `elements`    | `Vec<ElementSnapshot>`   | `elements`     | Top-level elements. Bins have `is_bin: true` with nested `children`. |
| `threads`     | `Vec<ThreadSummary>`     | `threads`      | Flat summary of streaming threads (no nested elements). Elements link via `thread_id`. |

### PipelineStats

Comprehensive metrics for a single pipeline.

| Field                   | Type                     | Nullable | JSON key                  | Description |
|-------------------------|--------------------------|----------|---------------------------|-------------|
| `level`                 | `StatsLevel`             | no       | `level`                   | Active instrumentation level for this pipeline's probes. |
| `window_size`           | `usize`                  | no       | `window_size`             | Ring buffer capacity (Full mode). Determines how many raw records each pad retains. |
| `expected_interval_ms`  | `f64`                    | no       | `expected_interval_ms`    | Nominal frame interval in milliseconds, derived from the stream's configured frame rate (e.g., 33.33 ms for 30 fps). Used as the reference for stutter/freeze detection thresholds. |
| `uptime_secs`           | `f64`                    | no       | `uptime_secs`             | Seconds since this pipeline was created (wall-clock). |
| `health`                | `HealthStatus`           | no       | `health`                  | Pipeline health classification derived from CPU, latency, and freeze analysis. |
| `dominant_issue`        | `IssueKind`              | no       | `dominant_issue`          | Primary issue identified in this pipeline. `"unknown"` when healthy. |
| `cpu_pct`               | `Option<f64>`            | **yes**  | `cpu_pct`                 | Instantaneous pipeline CPU usage (% of one core). Sum of all streaming thread CPUs. Absent before the first 1-second CPU poll completes. Omitted from JSON when `None`. |
| `cpu_stats`             | `Option<SystemDistribution>` | **yes** | `cpu_stats`            | Windowed pipeline CPU statistics (up to 120 samples = 2 minutes). Absent before any CPU history is available. Omitted from JSON when `None`. |
| `summary`               | `PipelineSummary`        | no       | `summary`                 | Aggregate health summary (throughput, latency, verdict). |
| `system`                | `SystemSnapshot`         | no       | `system`                  | Host-level metrics (CPU, memory, load, temperature). Shared across all pipelines on the same host. |
| `restarts`              | `RestartSnapshot`        | no       | `restarts`                | Pipeline restart/uptime statistics. |
| `root_cause_candidates` | `Vec<RootCauseCandidate>`| no       | `root_cause_candidates`   | Ranked root cause candidates for this pipeline. Empty when healthy. |
| `thread_bottlenecks`    | `Vec<ThreadBottleneck>`  | no       | `thread_bottlenecks`      | Threads with disproportionately high CPU usage. Empty when no bottleneck is detected. |

### PipelineConnection

Describes a link from this pipeline to another pipeline via a proxy bridge
(e.g., the `interpipesink`/`interpipesrc` pair connecting a source pipeline
to a sink pipeline).

| Field          | Type     | JSON key        | Description |
|----------------|----------|-----------------|-------------|
| `to_pipeline`  | `String` | `to_pipeline`   | Name of the target pipeline. |
| `bridge_type`  | `String` | `bridge_type`   | Type of bridge (e.g., `"proxy_sink"`). |
| `from_element` | `String` | `from_element`  | Element in this pipeline that is the bridge source. |
| `to_element`   | `String` | `to_element`    | Element in the target pipeline that is the bridge sink. |

### PipelineSummary

High-level health summary derived from pad and edge analysis.

| Field                                | Type                   | Nullable | JSON key                               | Description |
|--------------------------------------|------------------------|----------|----------------------------------------|-------------|
| `total_frames`                       | `u64`                  | no       | `total_frames`                         | Frame count at the most-active element (highest `total_buffers` across all pads). |
| `throughput_fps`                     | `f64`                  | no       | `throughput_fps`                       | Estimated throughput in frames/sec. Median of per-pad throughput candidates in the plausible range [0.1, 240]. |
| `total_pipeline_freshness_delay_ms`  | `f64`                  | no       | `total_pipeline_freshness_delay_ms`    | Sum of all inter-element freshness deltas (ms). An instantaneous, non-causal metric. |
| `total_pipeline_causal_latency_ms`   | `Option<Distribution>` | **yes**  | `total_pipeline_causal_latency_ms`     | Sum of per-edge PTS-matched causal latency. A windowed, stable pipeline-wide latency metric. Only present when at least one edge has causal data (Full mode with PTS). See [interpretation notes](#causal-latency-sum-interpretation) below. Omitted from JSON when `None`. |
| `causal_latency_health`              | `Option<CausalConfidence>` | **yes** | `causal_latency_health`            | Weighted aggregate confidence of causal latency across all edges. Omitted from JSON when `None`. |
| `verdict`                            | `String`               | no       | `verdict`                              | Human-readable health verdict (e.g., `"Healthy"`, `"Degraded: 3 stutter events"`). For display only — do not parse programmatically. |

#### Causal Latency Sum Interpretation

When `total_pipeline_causal_latency_ms` is present, its `Distribution`
fields have special semantics because they are **sums** across independent
edges:

- **`mean`** — Exact (expectation of a sum = sum of expectations).
- **`p95`, `p99`** — Conservative upper bounds (sum of per-edge
  percentiles >= true pipeline percentile). Useful as worst-case estimates.
- **`min`, `max`** — Sum of per-edge extremes.
- **`count`** — Minimum per-edge sample count (the bottleneck edge), not
  the total. Indicates the weakest link in sample support.
- **`std`** — Always 0. Computing the true standard deviation of a sum
  would require cross-edge covariance data that is not available.
- **`median`** — Approximated as the sum of per-edge means (same as
  `mean`).

---

## Thread Level

### ThreadSummary

A GStreamer streaming thread within a pipeline. Flat summary — no nested
elements. Elements link to threads via `ElementSnapshot.thread_id`.

| Field         | Type                    | Nullable | JSON key      | Description |
|---------------|-------------------------|----------|---------------|-------------|
| `id`          | `u32`                   | no       | `id`          | Linux kernel thread ID (`gettid()`). Unique within the pipeline. |
| `name`        | `Option<String>`        | **yes**  | `name`        | Thread name read from `/proc/self/task/{tid}/stat`. Omitted from JSON when `None`. |
| `stats`       | `ThreadStats`           | no       | `stats`       | Thread-level metrics. |
| `connections` | `Vec<ThreadConnection>` | no       | `connections` | Links to other threads (via queue elements that bridge thread boundaries). |

### ThreadStats

Per-thread CPU metrics.

| Field       | Type                         | Nullable | JSON key    | Description |
|-------------|------------------------------|----------|-------------|-------------|
| `name`      | `Option<String>`             | **yes**  | `name`      | Thread name read from `/proc/self/task/{tid}/stat`. May be absent if the thread exited before its name could be read. Omitted from JSON when `None`. |
| `cpu_pct`   | `f64`                        | no       | `cpu_pct`   | Instantaneous CPU usage (% of one core), computed from a 1-second delta of `/proc` counters. |
| `cpu_stats` | `Option<SystemDistribution>` | **yes**  | `cpu_stats` | Windowed CPU statistics (up to 120 samples = 2 minutes). Absent before any CPU history is available. Omitted from JSON when `None`. |

### ThreadConnection

Describes a queue element that bridges two streaming threads. Queues
decouple the threading of upstream and downstream elements.

| Field         | Type     | JSON key      | Description |
|---------------|----------|---------------|-------------|
| `to_thread`   | `u32`    | `to_thread`   | Target thread ID. |
| `via_element` | `String` | `via_element` | Name of the queue element bridging the threads. |

---

## Element Level

### ElementSnapshot

A GStreamer element (processing stage) within a pipeline. When `is_bin` is
true, this represents a GStreamer bin (e.g., rtspsrc, webrtcbin) and
`children` contains its direct child elements (which may themselves be bins).

| Field          | Type                      | Nullable | JSON key       | Description |
|----------------|---------------------------|----------|----------------|-------------|
| `name`         | `String`                  | no       | `name`         | GStreamer element instance name (e.g., `"v4l2src0"`, `"x264enc0"`). Unique within the pipeline. |
| `element_type` | `String`                  | no       | `element_type` | GStreamer factory/plugin name (e.g., `"v4l2src"`, `"x264enc"`, `"rtspsrc"`). Multiple instances of the same type can exist. |
| `is_bin`       | `bool`                    | no       | `is_bin`       | `true` if this element is a GStreamer bin. Omitted from JSON when `false`. |
| `children`     | `Vec<ElementSnapshot>`    | no       | `children`     | Child elements when this is a bin. Omitted from JSON when empty. |
| `thread_id`    | `Option<u32>`             | **yes**  | `thread_id`    | Linux kernel thread ID of the streaming thread that executes this element. Links to `ThreadSummary.id`. Omitted from JSON when `None`. |
| `state`         | `Option<String>`             | **yes** | `state`        | Current GStreamer element state (e.g. `"playing"`, `"paused"`). Omitted from JSON when `None`. |
| `properties`     | `Option<Vec<ElementProperty>>`| **yes** | `properties`   | All GObject properties as name-value pairs. Omitted from JSON when `None`. |
| `stats`        | `ElementStats`            | no       | `stats`        | Element-level metrics with inlined diagnostics. |
| `connections`  | `Vec<ElementConnection>`  | no       | `connections`  | Links to downstream elements. |
| `pads`         | `Vec<PadSnapshot>`        | no       | `pads`         | All pads (both sink and src) combined in a single array. Use `PadSnapshot.direction` to distinguish. |

### ElementStats

Per-element metrics with inlined stutter/freeze diagnostics.

| Field              | Type           | Nullable | JSON key           | Description |
|--------------------|----------------|----------|--------------------|-------------|
| `processing_time_us` | `Option<f64>` | **yes** | `processing_time_us` | Per-buffer causal processing time in microseconds (mean). Only available for filter elements (those with both sink and src pads). **Lite mode:** atomic accumulator — each src-pad probe computes `wall_ns − last_sink_arrival_ns` and accumulates into `sum/count`. **Full mode:** PTS-matched intra-element distribution mean — matches buffers by PTS between the element's own sink and src pad ring buffers. Omitted from JSON when `None`. |
| `processing_time_stats` | `Option<Distribution>` | **yes** | `processing_time_stats` | Windowed distribution of per-buffer processing time (Full mode only). Each sample is the wall-clock transit time `src_wall_ns − sink_wall_ns` for a PTS-matched buffer pair within the element. Contains `{count, min, max, mean, std, median, p95, p99}` in microseconds. Omitted from JSON when `None`. |
| `is_cross_thread`  | `Option<bool>` | **yes**  | `is_cross_thread`  | `true` when the element's sink and src pads are driven by different streaming threads (e.g. `queue`, `queue2`). When cross-thread, `processing_time_us` includes queuing delay and should be interpreted accordingly. `None` for source-only or sink-only elements. Omitted from JSON when `None`. |
| `cpu_pct`          | `Option<f64>`  | **yes**  | `cpu_pct`          | Attributed CPU usage (% of one core). For filter elements: proportional to processing time. For source/sink elements: equal share of residual CPU. Omitted from JSON when `None`. |
| `cpu_stats`        | `Option<Distribution>` | **yes** | `cpu_stats` | Windowed distribution of element CPU samples (1 Hz, up to 120 samples = 2 minutes). Computed from per-element CPU attribution history. Absent before any CPU history is available (first ~1 second after pipeline start) or for elements without a thread assignment. Omitted from JSON when `None`. |
| `health`           | `HealthStatus` | no       | `health`           | Element health: `"good"` if no stutter/freeze, `"degraded"` if stutter detected, `"bad"` if freeze detected. |
| `stutter_events`   | `u64`          | no       | `stutter_events`   | Count of intervals exceeding the stutter threshold (`max(expected_interval * 2, expected_interval + 20ms)`). |
| `freeze_events`    | `u64`          | no       | `freeze_events`    | Count of intervals exceeding the freeze threshold (`max(expected_interval * 10, 500ms)`). |
| `max_freeze_ms`    | `f64`          | no       | `max_freeze_ms`    | Duration of the longest observed freeze (ms). 0.0 when no freezes occurred. |
| `stutter_ratio`    | `f64`          | no       | `stutter_ratio`    | Fraction of observed intervals that exceed the stutter threshold. Range [0.0, 1.0]. |
| `queue_stats`   | `Option<QueueStats>`         | **yes** | `queue_stats` | Fill-level statistics for queue/queue2 elements. Omitted from JSON when `None`. |

### ElementConnection

Describes the delay measurement for one topology edge (from this element to
a downstream neighbor).

| Field                    | Type                   | Nullable | JSON key                  | Description |
|--------------------------|------------------------|----------|---------------------------|-------------|
| `to_element`             | `String`               | no       | `to_element`              | Downstream element name. |
| `freshness_delay_ms`     | `f64`                  | no       | `freshness_delay_ms`      | Non-causal freshness delta in ms. Compares `last_wall_ns` of the src pad on this element to the sink pad on the downstream element. An instantaneous point-in-time estimate, not a matched-buffer measurement. |
| `causal_latency_ms`      | `Option<Distribution>` | **yes**  | `causal_latency_ms`       | PTS-matched per-buffer transit time distribution (ms). Full mode only. Each sample matches a buffer by its PTS on both sides of the edge, measuring the wall-clock time delta. Omitted from JSON when `None`. |
| `causal_match_rate`      | `Option<f64>`          | **yes**  | `causal_match_rate`       | Fraction of upstream PTS values that found a downstream match. Range [0.0, 1.0]. Omitted from JSON when `None`. |
| `causal_matched_samples` | `Option<u64>`          | **yes**  | `causal_matched_samples`  | Count of successfully matched sample pairs. Omitted from JSON when `None`. |
| `causal_confidence`      | `Option<CausalConfidence>` | **yes** | `causal_confidence`    | Quality assessment of the causal latency data. Omitted from JSON when `None`. |

---

## Pad Level

### PadSnapshot

A GStreamer pad (data port) on an element.

| Field         | Type                | Nullable | JSON key      | Description |
|---------------|---------------------|----------|---------------|-------------|
| `name`        | `String`            | no       | `name`        | Pad instance name (e.g., `"src"`, `"sink"`, `"src_0"`). |
| `direction`   | `PadDirection`      | no       | `direction`   | `"sink"` (input) or `"src"` (output). |
| `caps`          | `Option<String>`             | **yes** | `caps`         | Current negotiated caps string (e.g. `"video/x-h264, width=(int)1920, height=(int)1080"`). Omitted from JSON when `None`. |
| `stats`       | `PadStats`          | no       | `stats`       | Pad-level statistics. Content depends on `stats.level`. |
| `buffer`      | `Vec<RawRecord>`    | no       | `buffer`      | Raw records from the ring buffer (Full mode only). Controlled by the `buffer_limit` query parameter. **Omitted from JSON when empty** (via `skip_serializing_if`). |
| `connections` | `Vec<PadConnection>`| no       | `connections` | Links to downstream pads. |

### PadStats

Per-pad statistics. The available data depends on the instrumentation level.

| Field               | Type                        | Nullable | JSON key            | Description |
|---------------------|-----------------------------|----------|---------------------|-------------|
| `level`             | `StatsLevel`                | no       | `level`             | `"lite"` or `"full"`. Determines which of `accumulators` / `distribution` is present. |
| `total_buffers`     | `u64`                       | no       | `total_buffers`     | Lifetime buffer count (monotonically increasing since pipeline start). |
| `total_keyframes`   | `u64`                       | no       | `total_keyframes`   | Lifetime keyframe (I-frame) count. |
| `total_delta_frames`| `u64`                       | no       | `total_delta_frames`| Lifetime delta-frame (P/B-frame) count. `total_keyframes + total_delta_frames = total_buffers`. |
| `total_dropped`    | `u64`                       | no       | `total_dropped`     | Lifetime dropped buffer count. Currently always 0; populated in future when QoS event tracking is added. |
| `drop_ratio`       | `f64`                       | no       | `drop_ratio`        | Fraction of buffers dropped (`total_dropped / total_buffers`). Range [0.0, 1.0]. Currently always 0.0. |
| `bitrate_bps`      | `Option<f64>`               | **yes**  | `bitrate_bps`       | Estimated bitrate in bits per second, computed from mean size × throughput. Omitted from JSON when `None`. |
| `avg_gop_size`     | `Option<f64>`               | **yes**  | `avg_gop_size`      | Average Group of Pictures size (`total_buffers / total_keyframes`). Absent when no keyframes have been observed. Omitted from JSON when `None`. |
| `keyframe_interval_ms` | `Option<Distribution>` | **yes** | `keyframe_interval_ms` | Distribution of wall-clock intervals between consecutive keyframes (ms). Requires Full mode with at least 2 keyframes in the raw buffer. Omitted from JSON when `None`. |
| `last_wall_ns`      | `u64`                       | no       | `last_wall_ns`      | Wall-clock time (ns since epoch) of the most recent buffer. 0 when no buffers have been observed. |
| `accumulators`      | `Option<AccumulatorSnapshot>`| **yes** | `accumulators`      | Lite backend cumulative accumulators. **Present only when `level = "lite"`**. Omitted from JSON when `None`. |
| `distribution`      | `Option<DistributionSnapshot>`| **yes**| `distribution`      | Full backend windowed distributions. **Present only when `level = "full"`**. Omitted from JSON when `None`. |

### PadConnection

Describes a link from this pad to a downstream pad on another element.

| Field          | Type            | Nullable | JSON key       | Description |
|----------------|-----------------|----------|----------------|-------------|
| `peer_element` | `String`        | no       | `peer_element` | Destination element name. |
| `peer_pad`     | `String`        | no       | `peer_pad`     | Destination pad name. |
| `media_type`   | `Option<String>`| **yes**  | `media_type`   | Negotiated caps media type (e.g., `"video/x-h264"`, `"video/x-raw"`). Absent when caps have not been negotiated. Omitted from JSON when `None`. |

---

## Shared Value Types

### RawRecord

A single buffer observation recorded by a pad probe.

| Field        | Type            | Nullable | JSON key     | Description |
|--------------|-----------------|----------|--------------|-------------|
| `wall_ns`    | `u64`           | no       | `wall_ns`    | Wall-clock time of observation, nanoseconds since Unix epoch. Source: `clock_gettime(CLOCK_REALTIME)`. |
| `pts_ns`     | `Option<u64>`   | **yes**  | `pts_ns`     | GStreamer buffer presentation timestamp in nanoseconds. Media-domain time from the source. Used for deduplication and causal latency matching. Absent for buffers without timestamps. Omitted from JSON when `None`. |
| `size`       | `u32`           | no       | `size`       | Payload size in bytes. For buffer lists, this is the aggregate size of all buffers in the list. |
| `is_keyframe`| `bool`          | no       | `is_keyframe`| `true` for intra-coded frames (I-frames), `false` for inter-coded frames (P/B-frames). |

### AccumulatorSnapshot

Lite backend cumulative statistics (per-pad). All counters are monotonically
increasing since pipeline start. To compute windowed rates, differentiate
two successive snapshots.

| Field                  | Type  | JSON key               | Unit    | Description |
|------------------------|-------|------------------------|---------|-------------|
| `sum_interval_ns`      | `u64` | `sum_interval_ns`      | ns      | Cumulative sum of inter-buffer intervals. |
| `sum_interval_sq_us`   | `u64` | `sum_interval_sq_us`   | us^2    | Sum of squared intervals (divided by 1000 before squaring to prevent u64 overflow). |
| `sum_size_bytes`       | `u64` | `sum_size_bytes`       | bytes   | Cumulative sum of buffer sizes. |
| `sum_size_sq_units`    | `u64` | `sum_size_sq_units`    | (B/1024)^2 | Sum of squared sizes (divided by 1024 before squaring to prevent u64 overflow). |
| `interval_count`       | `u64` | `interval_count`       | count   | Number of intervals recorded (`total_buffers - 1` for the first buffer has no predecessor). |
| `mean_interval_ms`     | `f64` | `mean_interval_ms`     | ms      | Pre-computed mean interval for convenience. |
| `std_interval_ms`      | `f64` | `std_interval_ms`      | ms      | Pre-computed population std deviation of interval. |
| `min_interval_ms`      | `f64` | `min_interval_ms`      | ms      | Minimum observed interval. |
| `max_interval_ms`      | `f64` | `max_interval_ms`      | ms      | Maximum observed interval. |
| `mean_size_bytes`      | `f64` | `mean_size_bytes`      | bytes   | Pre-computed mean buffer size. |
| `std_size_bytes`       | `f64` | `std_size_bytes`       | bytes   | Pre-computed population std deviation of size. |

### Distribution

Full statistical distribution summary. Used for causal latency, pad
interval/size distributions, and pipeline-level causal latency sums.

| Field    | Type  | JSON key | Description |
|----------|-------|----------|-------------|
| `count`  | `u64` | `count`  | Number of samples. |
| `min`    | `f64` | `min`    | Minimum value. |
| `max`    | `f64` | `max`    | Maximum value. |
| `mean`   | `f64` | `mean`   | Arithmetic mean. |
| `std`    | `f64` | `std`    | Population standard deviation (divides by N, not N-1). |
| `median` | `f64` | `median` | 50th percentile (nearest-rank). |
| `p95`    | `f64` | `p95`    | 95th percentile (nearest-rank). |
| `p99`    | `f64` | `p99`    | 99th percentile (nearest-rank). |

Default value (when empty): all fields are `0.0` / `0`.

### DistributionSnapshot

Full backend per-pad distributions. Six metrics capturing interval and size
characteristics for all buffers, keyframes only, and delta-frames only.

| Field          | Type           | JSON key       | Unit  | Description |
|----------------|----------------|----------------|-------|-------------|
| `interval`     | `Distribution` | `interval`     | ms    | All inter-buffer intervals. |
| `i_interval`   | `Distribution` | `i_interval`   | ms    | Intervals preceding keyframes only. |
| `p_interval`   | `Distribution` | `p_interval`   | ms    | Intervals preceding delta-frames only. |
| `size`         | `Distribution` | `size`         | bytes | All buffer sizes. |
| `i_size`       | `Distribution` | `i_size`       | bytes | Keyframe sizes only. |
| `p_size`       | `Distribution` | `p_size`       | bytes | Delta-frame sizes only. |

### SystemDistribution

Simplified distribution for system/CPU metrics. No percentiles (only
count, min, max, mean, std).

| Field   | Type  | JSON key | Description |
|---------|-------|----------|-------------|
| `count` | `u64` | `count`  | Number of samples in the ring buffer. |
| `min`   | `f64` | `min`    | Minimum value. |
| `max`   | `f64` | `max`    | Maximum value. |
| `mean`  | `f64` | `mean`   | Arithmetic mean. |
| `std`   | `f64` | `std`    | Population standard deviation. |

Default value (when empty): all fields are `0.0` / `0`.

### SystemSnapshot

Host-level metrics sampled at 1 Hz from Linux procfs/sysfs. These are
per-host, not per-pipeline — all pipelines on the same host share the
same `SystemSnapshot`.

| Field                   | Type                 | JSON key                 | Unit  | Description |
|-------------------------|----------------------|--------------------------|-------|-------------|
| `sample_count`          | `u64`                | `sample_count`           | count | Number of 1-second samples in the ring buffer (up to 120 = 2 minutes). |
| `current_cpu_pct`       | `f64`                | `current_cpu_pct`        | %     | Most recent host-level CPU usage. 100% = all cores fully utilized. |
| `current_load_1m`       | `f64`                | `current_load_1m`        | —     | Most recent 1-minute load average. |
| `current_mem_used_pct`  | `f64`                | `current_mem_used_pct`   | %     | Most recent memory usage (MemTotal - MemAvailable) / MemTotal * 100. |
| `current_temperature_c` | `f64`                | `current_temperature_c`  | °C    | Most recent CPU temperature. Read from `thermal_zone0`. |
| `cpu_stats`             | `SystemDistribution` | `cpu_stats`              | %     | Windowed CPU usage statistics. |
| `load_stats`            | `SystemDistribution` | `load_stats`             | —     | Windowed load average statistics. |
| `mem_stats`             | `SystemDistribution` | `mem_stats`              | %     | Windowed memory usage statistics. |
| `temp_stats`            | `SystemDistribution` | `temp_stats`             | °C    | Windowed temperature statistics. |

### RestartSnapshot

Pipeline restart and uptime tracking.

| Field                        | Type  | JSON key                       | Unit    | Description |
|------------------------------|-------|--------------------------------|---------|-------------|
| `start_count`                | `u64` | `start_count`                  | count   | Total number of times this pipeline has been started. 1 = first start, never restarted. |
| `restart_count`              | `u64` | `restart_count`                | count   | Number of restarts (`start_count - 1`). |
| `current_uptime_secs`        | `f64` | `current_uptime_secs`          | seconds | Seconds since the most recent start. |
| `total_tracked_secs`         | `f64` | `total_tracked_secs`           | seconds | Seconds since the very first start (total tracking duration). |
| `avg_restart_interval_secs`  | `f64` | `avg_restart_interval_secs`    | seconds | Average seconds between consecutive starts. 0.0 when no restarts. |
| `min_restart_interval_secs`  | `f64` | `min_restart_interval_secs`    | seconds | Minimum seconds between consecutive starts (fastest crash-loop interval). 0.0 when no restarts. |
| `last_restart_interval_secs` | `f64` | `last_restart_interval_secs`   | seconds | Seconds between the two most recent starts. 0.0 when no restarts. |

---

## Health and Root Cause Types

### RootCauseCandidate

A scored hypothesis for the root cause of a health issue.

| Field   | Type        | JSON key | Description |
|---------|-------------|----------|-------------|
| `cause` | `IssueKind` | `cause`  | The candidate root cause category. |
| `score` | `f64`       | `score`  | Severity score (higher = more likely / more severe). Scores are relative — compare within the same list, not across snapshots. |

### ThreadBottleneck

Identifies a streaming thread that is consuming a disproportionate amount
of CPU and may be causing scheduling delays.

| Field               | Type            | Nullable | JSON key            | Description |
|---------------------|-----------------|----------|---------------------|-------------|
| `thread_id`         | `u32`           | no       | `thread_id`         | Linux kernel thread ID. |
| `thread_name`       | `Option<String>`| **yes**  | `thread_name`       | Thread name. Omitted from JSON when `None`. |
| `cpu_pct`           | `f64`           | no       | `cpu_pct`           | Thread CPU usage (% of one core). |
| `elements`          | `Vec<String>`   | no       | `elements`          | Names of elements executing on this thread. |
| `latency_impact_ms` | `f64`           | no       | `latency_impact_ms` | Estimated latency contribution of this bottleneck (ms). |

---

## Additional Types

### ElementProperty

A single GObject property exported as a name-value string pair.

| Field   | Type     | JSON key | Description |
|---------|----------|----------|-------------|
| `name`  | `String` | `name`   | GObject property name (e.g. `"bitrate"`, `"tune"`). |
| `value` | `String` | `value`  | Property value serialized to string via `g_value_serialize()`. Falls back to debug format when serialization is unavailable. |

### QueueStats

Fill-level statistics for GStreamer `queue` and `queue2` elements, read at snapshot time (~1 Hz).

| Field                   | Type  | JSON key                | Unit    | Description |
|-------------------------|-------|-------------------------|---------|--------------|
| `current_level_buffers` | `u32` | `current_level_buffers` | count   | Current buffer count in the queue. |
| `current_level_bytes`   | `u64` | `current_level_bytes`   | bytes   | Current byte count in the queue. |
| `current_level_time_ns` | `u64` | `current_level_time_ns` | ns      | Current time level in nanoseconds. |
| `max_level_buffers`     | `u32` | `max_level_buffers`     | count   | Maximum buffer capacity. |
| `max_level_bytes`       | `u64` | `max_level_bytes`       | bytes   | Maximum byte capacity. |
| `max_level_time_ns`     | `u64` | `max_level_time_ns`     | ns      | Maximum time capacity in nanoseconds. |
| `fill_pct`              | `f64` | `fill_pct`              | %       | Fill percentage (0–100), computed from buffer or byte level. |

---

## Nullable Field Conventions

Fields that may be absent from JSON follow a consistent pattern:

| Rust type / annotation | JSON behavior | When absent |
|------------------------|---------------|-------------|
| `Option<T>` + `skip_serializing_if = "Option::is_none"` | Key omitted entirely | Value is `None` in Rust |
| `Vec<T>` + `skip_serializing_if = "Vec::is_empty"` | Key omitted entirely | Vector is empty |
| `Vec<T>` without skip annotation | `[]` (empty array) | Vector is empty |

**Client guidance:**

- Always access optional fields defensively. In TypeScript:
  `snapshot.stats.cpu_pct ?? null`. In Python: `stats.get("cpu_pct")`.
- For `buffer` on `PadSnapshot`: the key is absent when the array is empty
  (i.e., `buffer_limit=0` or Lite mode). Treat a missing `buffer` key as
  an empty array.

---

## Numeric Precision Notes

| Rust type | JSON type | TypeScript type | Range / precision notes |
|-----------|-----------|-----------------|------------------------|
| `u64`     | number    | `number`        | Safe up to 2^53 in JavaScript. Timestamps (`wall_ns`, `pts_ns`) and counters (`total_buffers`, etc.) may exceed this for extreme values, but in practice timestamps fit within 2^53 until ~2255 CE and buffer counts fit within 2^53 for lifetimes under ~9 million years at 100 fps. |
| `u32`     | number    | `number`        | Always safe. Used for `thread_id` and `size`. |
| `usize`   | number    | `number`        | `window_size` is `usize` on the Rust side, serialized as a JSON number. On 64-bit platforms this is u64, but values are clamped to ≤50,000 by the server. |
| `f64`     | number    | `number`        | IEEE 754 double. All floating-point metrics use this type. JSON serialization preserves full double precision. NaN and Infinity are not produced by the server. |
| `bool`    | boolean   | `boolean`       | Standard JSON boolean. |
| `String`  | string    | `string`        | UTF-8. No maximum length enforced. |
