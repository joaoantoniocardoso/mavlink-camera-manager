# MCM Pipeline Instrumentation: Sample Collection Methodology

This document describes, in precise statistical terms, how the MAVLink Camera
Manager (MCM) collects raw observations from live GStreamer video/audio
pipelines, what each observation contains, how those observations are stored,
and what derived statistics are computed from them.

**Companion documents:**

- [`stats-api-contract.md`](stats-api-contract.md) — HTTP/WebSocket
  endpoints, query parameters, caching, and control API.
- [`stats-api-data-model-contract.md`](stats-api-data-model-contract.md) —
  Authoritative type reference for every struct and enum in the
  `StreamsSnapshot` hierarchy.

---

## 1. The Observation Unit

Each pipeline processes media through a directed acyclic graph (DAG) of
**elements** (software processing stages). Elements are connected via **pads**:
a *src* (source) pad on one element links to a *sink* (input) pad on the next.
Every buffer of media data that traverses a pad link is one potential
observation point.

MCM installs a **probe callback** on every pad of every element in the
pipeline. When a buffer (or a list of buffers representing one logical unit,
such as RTP packets from a single video frame) passes through a pad, the probe
fires and records a single observation.

### 1.1 What the Probe Records

Each observation (called a `RawRecord` in the API) contains four fields:

| Field          | Type           | Source                                    | Description |
|----------------|----------------|-------------------------------------------|-------------|
| `wall_ns`      | `u64`          | `clock_gettime(CLOCK_REALTIME)` via Rust's `SystemTime::now()` | Wall-clock time of observation, nanoseconds since Unix epoch. This is the system's real-time clock, not the GStreamer pipeline clock. |
| `pts_ns`       | `Option<u64>`  | GStreamer buffer `PTS` (presentation timestamp) | The media-domain timestamp assigned by the source, in nanoseconds. May be absent for some buffer types. |
| `size`         | `u32`          | `buffer.size()` or `buffer_list.calculate_size()` | Payload size in bytes. For buffer lists (e.g., one H.264 frame split into multiple RTP packets), this is the **aggregate** size of all packets in the list. |
| `is_keyframe`  | `bool`         | `!(buffer.flags() & DELTA_UNIT)`          | `true` if the buffer carries an I-frame (intra-coded, self-contained). `false` if it is a P/B-frame (delta, depends on prior frames). For buffer lists, the flag of the first buffer in the list is used. |

**Important properties of `wall_ns`:**

- It is the **only** timing source used for all interval, throughput, and
  latency calculations. The GStreamer pipeline clock is never consulted.
- There is exactly **one** `clock_gettime()` call per probe invocation, shared
  by all fields. On a Raspberry Pi 4 (ARMv7), this call takes approximately
  716 ns.
- On x86_64 Linux with `vDSO`, the cost is typically under 25 ns.

**Important properties of `pts_ns`:**

- PTS values originate from the media source (e.g., the camera or RTSP server).
  They are monotonically increasing within a stream but exist in a different
  time domain from `wall_ns`.
- PTS is used **only** for two purposes: (a) deduplicating buffers that share
  the same logical frame (e.g., after RTP packetization), and (b) matching
  buffers across adjacent elements for causal latency measurement.
- PTS may be absent (`None`) for buffers that do not carry media timestamps
  (e.g., some metadata buffers or event-only pads).

### 1.2 Observation Granularity

- For a **single buffer** (the common case on most pads), one observation is
  recorded per buffer.
- For a **buffer list** (e.g., on the src pad of `rtph264pay` in
  zero-latency aggregate mode, where one H.264 frame becomes N RTP packets
  in a single list), **one** aggregate observation is recorded per list. The
  `size` is the sum of all packet payloads; the `is_keyframe` and `pts_ns`
  are taken from the first packet.

This means that at pads *after* RTP payloaders, the observation rate reflects
the **frame rate** (one observation per frame), not the packet rate.
At pads *before* payloaders, the observation rate is also one per frame.

### 1.3 Thread ID Capture

In addition to the four-field record, each probe callback also captures the
**Linux kernel thread ID** (`gettid()` syscall) of the GStreamer streaming
thread executing the callback. Three `AtomicU32` fields are maintained per
element:

- `thread_id` — overwritten by every probe callback (sink or src), used for
  CPU attribution (Section 5).
- `sink_thread_id` — overwritten only by sink-pad probe callbacks.
- `src_thread_id` — overwritten only by src-pad probe callbacks.

The cost is approximately 20 ns per callback (kernel-cached). These thread IDs
are not part of the `RawRecord`. The generic `thread_id` is used for CPU
attribution. The direction-specific IDs enable **cross-thread detection**: when
`sink_thread_id ≠ src_thread_id`, the element's sink and src pads are driven
by different streaming threads (e.g., `queue`, `queue2`). This is exposed as
`is_cross_thread` in `ElementStats` and signals that processing time includes
queuing delay.

---

## 2. Storage Backends

MCM offers two storage backends, selectable at runtime. The choice affects
what derived statistics are available, but the raw observation (the four fields
above) is the same in both cases.

### 2.1 Lite Backend — Cumulative Accumulators (O(1) Memory)

The Lite backend stores no individual observations. Instead, it maintains a
fixed set of **atomic accumulators** that are updated on every probe callback.
Each pad has its own independent set of accumulators.

#### Stored accumulators

| Accumulator            | Type       | Update rule                             | Unit |
|------------------------|------------|-----------------------------------------|------|
| `sum_interval_ns`      | `AtomicU64`| `+= (wall_ns - prev_wall_ns)`          | ns   |
| `sum_interval_sq_us`   | `AtomicU64`| `+= (interval_us)^2`                   | µs²  |
| `sum_size_bytes`       | `AtomicU64`| `+= size`                              | bytes|
| `sum_size_sq_units`    | `AtomicU64`| `+= (size / 1024)^2`                   | (bytes/1024)² |
| `min_interval_ns`      | `AtomicU64`| CAS loop: keep minimum                  | ns   |
| `max_interval_ns`      | `AtomicU64`| CAS loop: keep maximum                  | ns   |
| `total_buffers`        | `AtomicU64`| `+= 1`                                 | count|
| `total_keyframes`      | `AtomicU64`| `+= 1` if `is_keyframe`                | count|
| `total_delta_frames`   | `AtomicU64`| `+= 1` if `!is_keyframe`               | count|
| `last_wall_ns`         | `AtomicU64`| `= wall_ns`                            | ns   |
| `prev_wall_ns`         | `AtomicU64`| `= wall_ns` (previous value used for interval) | ns |

**Important notes on the squared accumulators:**

- `sum_interval_sq_us` stores the sum of squared intervals in **microseconds
  squared** (not nanoseconds squared). The interval is divided by 1,000 before
  squaring. This prevents `u64` overflow: at 10 ms intervals (100 fps), each
  term is 10^8, giving approximately 58 years of headroom before overflow.
- `sum_size_sq_units` stores the sum of squared sizes after dividing the raw
  byte size by 1,024. This prevents overflow for large frames (e.g., raw 8K
  NV12 at ~50 MB/frame has approximately 2.5 years of headroom at 100 fps).

**How statistics are derived from Lite accumulators:**

Since all values are cumulative, statistics for a time window are obtained by
**differentiating** two snapshots taken at different times:

```
n              = snap₂.total_buffers − snap₁.total_buffers
interval_count = n − 1   (first buffer in each snapshot has no predecessor)

mean_interval  = (snap₂.sum_interval_ns − snap₁.sum_interval_ns) / interval_count

                   snap₂.sum_interval_sq_us − snap₁.sum_interval_sq_us
variance       = ——————————————————————————————————————————————————————— − mean²
                                    interval_count

std_interval   = √(variance)    [clamped to ≥ 0 before sqrt]

throughput     = n / Δt          [where Δt is wall-clock time between snapshots]
```

The `mean` and `std` for buffer sizes follow the same pattern using
`sum_size_bytes` and `sum_size_sq_units` (with appropriate unit conversion).

In the current implementation, the snapshot endpoint computes these from the
**lifetime** accumulators (i.e., snap₁ is the zero-state at pipeline start).
The system also reports the raw accumulator values so that an external consumer
can difference any two snapshots.

**Variance estimator type:** The variance formula used is the **population
variance** (divides by `N`, not `N-1`). For the typical sample counts involved
(hundreds to millions), the distinction is negligible.

### 2.2 Full Backend — Atomic Ring Buffer (O(W) Memory)

The Full backend stores the most recent **W** individual observations in a
lock-free ring buffer (where **W** is the configurable window size, default
900, range 1–50,000). At 30 fps, a window of 900 holds approximately 30
seconds of data.

Each slot in the ring stores all four observation fields (`wall_ns`, `pts_ns`,
`size`, `is_keyframe`) as separate atomics. A monotonically increasing write
cursor determines the current slot (`cursor % capacity`). When the ring is
full, older observations are silently overwritten.

**Torn reads:** Because the ring is written by one thread and read by another
without locks, a reader may see a partially updated slot at the write frontier.
This is considered harmless: all statistics are computed over hundreds of
samples, and a single torn read has negligible impact on aggregate metrics.

In addition to the ring buffer, the Full backend also maintains the same
monotonic counters as Lite (`total_buffers`, `total_keyframes`,
`total_delta_frames`, `last_wall_ns`) for consistency.

---

## 3. Derived Statistics from Observations

### 3.1 From the Lite Backend (Snapshot-time)

At snapshot time, the Lite backend produces an `AccumulatorSnapshot`:

| Statistic            | Formula | Unit |
|----------------------|---------|------|
| `mean_interval_ms`   | `sum_interval_ns / interval_count / 10⁶` | ms |
| `std_interval_ms`    | `√(sum_interval_sq_us / interval_count − (mean_interval_ns / 1000)²) / 1000` | ms |
| `min_interval_ms`    | `min_interval_ns / 10⁶` | ms |
| `max_interval_ms`    | `max_interval_ns / 10⁶` | ms |
| `mean_size_bytes`    | `sum_size_bytes / total_buffers` | bytes |
| `std_size_bytes`     | `√(sum_size_sq_units / total_buffers − (mean_size_bytes / 1024)²) × 1024` | bytes |

### 3.2 From the Full Backend (Snapshot-time)

At snapshot time, the Full backend reads all valid records from the ring buffer
and computes **six distributions**, each as a `Distribution` struct containing
`{count, min, max, mean, std, median, p95, p99}`:

| Distribution    | Input data                                   | Unit  |
|-----------------|----------------------------------------------|-------|
| `interval`      | Wall-clock intervals between consecutive records | ms |
| `i_interval`    | Intervals preceding keyframe records only    | ms    |
| `p_interval`    | Intervals preceding delta-frame records only | ms    |
| `size`          | Buffer sizes of all records                  | bytes |
| `i_size`        | Sizes of keyframe records only               | bytes |
| `p_size`        | Sizes of delta-frame records only            | bytes |

**Interval computation:** For a sequence of records `r₀, r₁, …, rₙ`, the
i-th interval is `(r[i+1].wall_ns − r[i].wall_ns) / 10⁶` ms. The frame-type
filter applies to the **second** record in each pair (the record "arriving"
after the interval).

**Percentile computation:** Percentiles are computed using O(n) cascaded
selection (`select_nth_unstable_by`), not a full O(n log n) sort. The p99
index is selected first (partitioning the array around it), then p95 is
selected within the [0..=p99] prefix, and finally the median within the
[0..=p95] prefix. The k-th percentile uses nearest-rank:
`index = round(k/100 × (n−1))`, then the value at that index.
Min, max, mean, and variance are computed in a single prior pass.

**Variance and standard deviation:** Computed as population variance
(`1/N × Σ(xᵢ − x̄)²`), same as in the Lite backend.

### 3.3 Throughput (Frames per Second)

Throughput is estimated per-pad using one of two methods, in order of
preference:

1. **PTS-based (preferred):** Count the number of **unique** PTS values in the
   ring buffer records. Divide by the wall-clock span
   `(max(wall_ns) − min(wall_ns))` in seconds. This correctly counts frames
   even when one frame produces multiple buffers (e.g., after RTP
   packetization).

2. **Interval-based (fallback):** Compute `1000 / mean_interval_ms` from
   either the Lite accumulators or the Full interval distribution.

The **pipeline-level throughput** is the median of all per-pad throughput
candidates that fall in the plausible range [0.1, 240] fps.

### 3.4 Stutter and Freeze Detection

From the interval data (either full distribution or Lite min/max):

- **Stutter threshold:** `max(expected_interval × 2, expected_interval + 20ms)`
- **Freeze threshold:** `max(expected_interval × 10, 500ms)`

Where `expected_interval` is the nominal interval for the stream (e.g.,
33.3 ms for 30 fps). The expected interval is a pipeline configuration
parameter, not derived from the data.

Stutter count is the number of observed intervals exceeding the stutter
threshold. Freeze count is the number exceeding the freeze threshold.

---

## 4. Inter-Element Delay Measurements

The pipeline topology (the DAG of element connections) is extracted once when
probes are installed. For each **edge** (pad link) in this graph, two delay
metrics are computed at snapshot time.

### 4.1 Freshness Delay (Non-Causal)

For an edge from element A's src pad to element B's sink pad:

```
freshness_delay_ms = (B.sink.last_wall_ns − A.src.last_wall_ns) / 10⁶
```

This compares the **most recent** wall-clock timestamps observed on each side
of the link. It is an instantaneous snapshot, not a matched-buffer measurement.
It represents how "stale" the downstream element's most recent input is
relative to the upstream element's most recent output.

**Limitation:** This is not a true latency measurement. If the two pads are
read at slightly different times or process buffers at different rates, the
delta may not correspond to any single buffer's transit time.

### 4.2 Causal Latency (PTS-Matched, Full Mode Only)

For edges where both sides have PTS values in their ring buffers, the system
performs **exact PTS matching** to measure per-buffer transit times:

1. Build a lookup table of `PTS → [wall_ns₁, wall_ns₂, …]` from the
   downstream (sink) pad's records, sorted by wall time.
2. For each upstream (src) pad record with PTS=P, find the **earliest**
   downstream record with the same PTS=P whose `wall_ns` is ≥ the upstream
   record's `wall_ns`.
3. The latency for this matched pair is
   `(downstream.wall_ns − upstream.wall_ns) / 10⁶` ms.
4. Build a `Distribution` from all matched latencies.

**Match rate:** The fraction of upstream records with a PTS that found a
downstream match. A low match rate can occur when the ring buffers on both
sides do not overlap in time, or when PTS values are modified or absent on
one side.

**Confidence levels** are assigned based on match quality:

| Confidence | Criteria |
|------------|----------|
| High       | ≥ 50 matched samples **and** match rate ≥ 0.8 |
| Medium     | ≥ 20 matched samples **and** match rate ≥ 0.4 |
| Low        | Anything else |

**Pipeline-level causal latency health** is a weighted average of per-edge
confidence scores, weighted by the number of matched samples on each edge.

**Tee element handling:** GStreamer tee elements (e.g., `VideoTee`, `RTPTee`)
split a single input stream to multiple outputs. Their src pads are **not
probed** because they carry duplicate data (identical buffers forwarded to each
output). However, because a tee copies PTS identically from its sink pad to
all src pads, causal latency can still be computed for edges originating from a
tee. When the upstream element has no src pad records (which uniquely
identifies a tee with `skip_src_pads`), the system falls back to using the
tee's **sink pad** records as the upstream PTS reference. The resulting latency
measures the real transit time from buffer arrival at the tee's input to buffer
arrival at each downstream element's input.

### 4.3 Per-Element Processing Time

For **filter-like elements** (those with at least one sink pad and at least one
src pad), processing time is measured using per-buffer causal techniques at
two levels of detail.

#### Lite Mode — Atomic Accumulators

Each `ElementProbe` maintains three atomics:

| Field                   | Type         | Updated by        | Description |
|-------------------------|-------------|-------------------|-------------|
| `last_sink_arrival_ns`  | `AtomicU64` | Sink-pad probes   | Wall-clock time of the most recent buffer arrival on any sink pad. |
| `proc_time_sum_ns`      | `AtomicU64` | Src-pad probes    | Cumulative sum of per-buffer processing times (ns). |
| `proc_time_count`       | `AtomicU64` | Src-pad probes    | Number of per-buffer processing time samples. |

**Protocol:**

1. When a buffer arrives on a **sink pad**, the probe stores `wall_ns` into
   `last_sink_arrival_ns` (atomic store, relaxed ordering).
2. When a buffer departs from a **src pad**, the probe loads
   `last_sink_arrival_ns` and computes `delta = wall_ns − last_sink_arrival_ns`.
   If `delta > 0`, it accumulates `proc_time_sum_ns += delta` and
   `proc_time_count += 1` (atomic fetch-add).

At snapshot time: `processing_time_us = (proc_time_sum_ns / proc_time_count) / 1000`.

This is a **per-buffer causal measurement**: for single-threaded elements (the
common case), the sink probe fires immediately before the element processes the
buffer, and the src probe fires immediately after. The delta is the actual
processing time for that buffer.

**Same-thread guarantee:** GStreamer calls sink and src probes on the same
streaming thread for non-queuing elements, so atomic relaxed ordering is
sufficient (single-producer, single-consumer on the hot path).

#### Full Mode — PTS-Matched Intra-Element Distribution

At snapshot time, the Full backend additionally computes a PTS-matched
processing time distribution using the element's own sink and src pad ring
buffers (the same technique used for inter-element causal latency in
Section 4.2, but applied *within* a single element):

1. Build a lookup table of `PTS → [wall_ns₁, wall_ns₂, …]` from the element's
   **src pad** records.
2. For each **sink pad** record with PTS=P, find the earliest src pad record
   with the same PTS=P whose `wall_ns ≥ sink.wall_ns`.
3. Compute `transit_us = (src.wall_ns − sink.wall_ns) / 1000`.
4. Build a `Distribution` from all matched transit times.

The result is stored in `processing_time_stats` (a full `Distribution` with
count, min, max, mean, std, median, p95, p99 in microseconds) and
`processing_time_us` is set to the distribution mean.

#### Cross-Thread Detection

Elements whose sink and src pads are driven by different streaming threads
(e.g., `queue`, `queue2`) are flagged with `is_cross_thread: true` in the API.
For these elements, both the atomic accumulator and PTS-matched measurements
include **queuing delay** — the time buffers spend waiting in the internal
queue — not just CPU processing time. This is an inherent limitation shared
by GstShark's `proctime` tracer.

The cross-thread flag is determined by comparing `sink_thread_id` and
`src_thread_id` (Section 1.3). When they differ, the flag is set.

### 4.4 Pipeline-Total Causal Latency

The pipeline summary includes a `total_pipeline_causal_latency_ms` field that
aggregates per-edge causal latency into a single pipeline-wide metric. This is
computed by summing selected statistics across all edges that have causal
latency data:

```
total_pipeline_causal_latency_ms.mean = Σ edge.causal_latency_ms.mean
total_pipeline_causal_latency_ms.p95  = Σ edge.causal_latency_ms.p95
total_pipeline_causal_latency_ms.p99  = Σ edge.causal_latency_ms.p99
total_pipeline_causal_latency_ms.min  = Σ edge.causal_latency_ms.min
total_pipeline_causal_latency_ms.max  = Σ edge.causal_latency_ms.max
```

The `count` is the **minimum** count across all contributing edges (the most
conservative estimate of sample support). The `std` is set to 0 because
computing the standard deviation of a sum requires inter-edge covariance data
that is not available. The `median` is approximated as the sum of means.

This field is `None` when no edges have causal latency data (e.g., Lite mode
or when PTS is absent).

**Motivation:** The existing `total_pipeline_freshness_delay_ms` sums
instantaneous freshness deltas, which are noisy single-sample readings
susceptible to scheduling artifacts. The causal latency sum uses windowed,
PTS-matched measurements (typically ~900 samples per edge), yielding a far
more stable pipeline-wide latency estimate.

---

## 5. Per-Thread CPU Measurement

### 5.1 Data Source

Once per second, a background task reads the Linux procfs file
`/proc/self/task/{tid}/stat` for each streaming thread ID that was captured by
the probe callbacks (Section 1.3). From this file, two cumulative counters are
extracted:

| Field   | Position in stat file | Meaning |
|---------|-----------------------|---------|
| `utime` | Field 14 (1-indexed)  | Cumulative user-mode CPU ticks for this thread |
| `stime` | Field 15 (1-indexed)  | Cumulative kernel-mode CPU ticks for this thread |

The thread name (`comm`) is also extracted from the parenthesized field in the
stat line.

### 5.2 CPU Percentage Calculation

```
total_ticks = utime + stime
delta_ticks = total_ticks_now − total_ticks_prev
cpu_pct     = (delta_ticks / (elapsed_secs × ticks_per_sec)) × 100
```

Where:
- `elapsed_secs` is the wall-clock time since the previous poll (nominally 1
  second).
- `ticks_per_sec` is obtained from `sysconf(_SC_CLK_TCK)`, typically 100 on
  Linux (meaning each "tick" is 10 ms of CPU time).
- `cpu_pct` represents the percentage of **one CPU core** consumed by this
  thread. A value of 100% means one core is fully utilized.

The first poll after a thread appears always returns 0% (no previous baseline).

### 5.3 CPU Attribution to Elements

Multiple GStreamer elements may share a single streaming thread. To attribute
the thread's CPU usage to individual elements:

1. **Group** elements by their thread ID.
2. For each filter element in the group, use `processing_time_us` from the
   per-buffer causal measurement (Section 4.3).
3. **Proportional attribution:** Each filter element receives:
   ```
   element_cpu_pct = (element_processing_time / Σ group_processing_times) × thread_cpu_pct
   ```
   Because the fractions sum to 1.0, this fully distributes the thread's CPU
   among measured elements.
4. **Residual attribution:** The CPU not accounted for by measured elements
   (`thread_cpu_pct − Σ attributed_cpu`) is distributed **equally** among
   unmeasured elements (source-only, sink-only, or tee elements) in the same
   thread group.

**Sum invariant:** For each thread, the sum of all element CPU attributions
equals the thread's total CPU percentage.

### 5.4 Windowed CPU Statistics

The instantaneous `cpu_pct` value (Section 5.2) represents a single 1-second
delta and can be noisy. To provide stable windowed statistics, both per-thread
and per-pipeline CPU values are recorded into ring buffers.

**Per-thread history:** The `ThreadCpuTracker` maintains a `VecDeque<f64>` per
TID with a capacity of 120 entries (2 minutes at 1 sample/second). Each
`poll()` call pushes the computed `cpu_pct` into the corresponding TID's
history buffer. When the buffer is full, the oldest entry is evicted. Stale
TIDs (threads that no longer exist) have their history pruned alongside the
existing stale-entry cleanup.

**Per-pipeline history:** `PipelineAnalysis` maintains a separate
`VecDeque<f64>` (capacity 120) that records the pipeline-total CPU (the sum
of all streaming thread CPUs) after each `poll_thread_cpu()` cycle.

At snapshot time, both buffers are summarized as a `SystemDistribution`
containing `{count, min, max, mean, std}`:

- **`cpu_stats`** on `PipelineStats`: windowed statistics of pipeline-total
  CPU. A single snapshot's `cpu_stats.mean` has variance reduced by
  approximately the ring buffer size (up to ~120x) compared to the
  instantaneous `cpu_pct`.
- **`cpu_stats`** on each `ThreadStats`: windowed statistics of that
  thread's CPU usage.

Both fields are `None` when no CPU history is available (e.g., immediately
after pipeline start before any polls have completed).

---

## 6. System-Level Metrics

A separate 1 Hz background sampler collects host-level metrics from procfs and
sysfs. These are **not** per-pipeline but per-host.

| Metric             | Source file                              | Calculation |
|--------------------|------------------------------------------|-------------|
| CPU usage (%)      | `/proc/stat` first line                  | `((Δtotal − Δidle) / Δtotal) × 100` where idle includes iowait |
| 1-min load average | `/proc/loadavg`                          | First field, read directly |
| Memory usage (%)   | `/proc/meminfo`                          | `((MemTotal − MemAvailable) / MemTotal) × 100` |
| Temperature (°C)   | `/sys/class/thermal/thermal_zone0/temp`  | Raw value ÷ 1000 (millidegrees to Celsius) |

These four values are appended once per second to a **VecDeque** of capacity
120 (2 minutes of history). At snapshot time, each metric is summarized as a
`SystemDistribution` containing `{count, min, max, mean, std}`. The most
recent value is also provided as a `current_*` field.

---

## 7. Concurrency and Synchronization Model

All probe callbacks execute on GStreamer's internal **streaming threads**, which
are real-time-priority threads managed by GStreamer, not by MCM. The critical
design constraint is that probe callbacks must not block or contend with locks.

### 7.1 Hot Path (Probe Callbacks)

- **Lock-free writes:** Both Lite and Full backends use only atomic operations
  (`fetch_add`, `store`, `compare_exchange_weak`) with `Relaxed` ordering.
  There are no mutexes, no allocations, and no syscalls (except the single
  `clock_gettime` and `gettid` per callback).
- **No contention between pads:** Each pad has its own `PadBuffer` (behind an
  `Arc`). Pads on different elements or different pads on the same element
  never share a buffer.

### 7.2 Cold Path (Snapshot / API Request)

- Snapshots are computed on the **API thread** (Tokio runtime).
- The snapshot reader holds `Mutex` locks on `elements`, `topology`, `system`,
  and `thread_cpu` — but these are only contended with other API readers or
  the 1 Hz system sampler, never with streaming threads.
- Snapshot results are **cached** with a 900 ms TTL. Multiple concurrent API
  consumers (e.g., several WebSocket handlers ticking at 1 Hz) share a single
  computation.

---

## 8. Raw Record Export

Raw observations from the Full backend's ring buffer can be included
directly in the `StreamsSnapshot` response via the `buffer_limit` query
parameter.

When `buffer_limit > 0`, each `PadSnapshot` includes a `buffer` array
containing up to `buffer_limit` most-recent `RawRecord` entries from that
pad's ring buffer. The records are returned in chronological order (oldest
first). When `buffer_limit = 0` (the default), the `buffer` key is omitted
from the JSON payload entirely, reducing response size.

The maximum allowed value is 300 (values above this are clamped server-side).
In Lite mode, the `buffer` array is always empty because no individual
records are retained.

This replaces the previous per-pipeline/per-element samples endpoint that
binned records into 1-second windows. Clients that need time-bucketed views
can group `RawRecord` entries by `wall_ns / 1_000_000_000` on the client
side.

---

## 9. Consolidated Snapshot API

A single endpoint returns the complete hierarchical snapshot of all streams,
pipelines, threads, elements, and pads:

```
GET /stats/streams/snapshot?buffer_limit=0
```

The response is a `StreamsSnapshot` — the root of the object graph described
in the [data model contract](stats-api-data-model-contract.md). It
consolidates all previously separate endpoints (health, root cause,
diagnostics, per-element samples) into a single response:

- **Fleet health** → `StreamsSnapshot.stats` (`FleetStats`)
- **Root cause analysis** → `PipelineStats.root_cause_candidates`
- **Element diagnostics** → `ElementStats` (stutter/freeze fields inlined)
- **Raw samples** → `PadSnapshot.buffer` (controlled by `buffer_limit`)
- **CPU attribution** → `ElementStats.cpu_pct`, `ThreadStats.cpu_pct`
- **Causal latency** → `ElementConnection.causal_latency_ms`

### 9.1 Query Parameters

| Parameter      | Default | Range   | Description |
|----------------|---------|---------|-------------|
| `buffer_limit` | 0       | 0–300   | Maximum number of raw records to include per pad. 0 omits the `buffer` array (reduces payload size). Values > 300 are clamped to 300. |

### 9.2 WebSocket Streaming Variant

A WebSocket endpoint pushes snapshots at a configurable interval:

```
GET /stats/streams/snapshot/ws?interval_ms=1000&buffer_limit=0
```

| Parameter      | Default | Range       | Description |
|----------------|---------|-------------|-------------|
| `interval_ms`  | 1000    | 500–∞       | Push interval in milliseconds. Values below 500 are clamped to 500. |
| `buffer_limit` | 0       | 0–300       | Same as the GET endpoint. |

Each push sends a JSON-serialized `StreamsSnapshot` as a WebSocket text
frame.

**Interaction with the snapshot cache:** The snapshot cache has a 900 ms TTL.
At the minimum push interval of 500 ms, approximately every other push will
serve a cached snapshot. This is efficient (no redundant computation) but
means the consumer may see the same data twice in rapid succession. At
1000 ms intervals, each push typically triggers a fresh computation.

### 9.3 Implementation Details

Internally, `full_snapshot()` constructs the `StreamsSnapshot` by computing
the pipeline-level analysis once per pipeline and then assembling the
hierarchical tree (grouping pipelines under their parent stream, grouping
elements under their streaming thread). Health, root cause, diagnostics,
and edge delays are all computed from the same shared snapshot, so there is
no redundant computation. Snapshot results are cached with a 900 ms TTL;
concurrent callers (HTTP, WebSocket) share a single computation.

---

## 10. Summary of Statistical Estimators

| Statistic | Estimator | Backend | Notes |
|-----------|-----------|---------|-------|
| Mean (interval, size) | Arithmetic mean | Both | Lite: from cumulative sums. Full: single-pass accumulation. |
| Variance / Std Dev | Population variance (÷N) | Both | Lite: Welford-like from sum-of-squares. Full: single-pass accumulation. |
| Min, Max | Running (Lite) or single-pass (Full) | Both | Lite min uses atomic CAS loop. |
| Median | Nearest-rank via O(n) cascaded selection | Full only | `index = round(0.5 × (n−1))` |
| P95, P99 | Nearest-rank via O(n) cascaded selection | Full only | Same formula with 0.95, 0.99 |
| Throughput (fps) | Unique PTS count / wall span, or 1000/mean_interval | Both | PTS method preferred; median of candidates across pads. |
| Processing time (per-element) | Per-buffer causal: `src_wall − sink_wall` per buffer | Both | Lite: atomic accumulators (mean only). Full: PTS-matched distribution (mean, p50, p95, p99). Cross-thread elements flagged via `is_cross_thread`. |
| CPU % (per-thread) | Delta ticks / (elapsed × ticks_per_sec) × 100 | N/A (procfs) | Polled at 1 Hz. Instantaneous value. |
| CPU % (per-thread, windowed) | `SystemDistribution` from per-TID ring buffer (capacity 120) | N/A (derived) | `ThreadStats.cpu_stats`. Up to ~120x variance reduction. |
| CPU % (per-pipeline, windowed) | `SystemDistribution` from pipeline CPU ring buffer (capacity 120) | N/A (derived) | `PipelineStats.cpu_stats`. Sum of thread CPUs, windowed. |
| CPU % (per-element) | Proportional to processing time within thread group | N/A (derived) | Residual distributed equally to unmeasured elements. |
| Freshness delay | Instantaneous wall-clock delta between adjacent pads | Both | Non-causal. |
| Causal latency (per-edge) | PTS-matched buffer transit time distribution | Full only | Requires PTS on both sides of edge. Tee edges use sink pad as upstream PTS reference. |
| Causal latency (pipeline total) | Sum of per-edge causal latency statistics | Full only | `PipelineSummary.total_pipeline_causal_latency_ms`. Stable pipeline-wide metric. |

---

## 11. Known Limitations and Caveats

1. **Wall-clock drift:** `wall_ns` comes from `CLOCK_REALTIME`, which can be
   adjusted by NTP. If the system clock is stepped during observation, interval
   calculations will be affected. Monotonic clock is not used because the
   observations need to be correlated across elements (which may be on
   different threads with independent TSC offsets on some architectures).

2. **Lite variance quantization:** The interval variance accumulator operates
   in microseconds (not nanoseconds), and the size variance accumulator
   operates in units of 1024 bytes. For intervals below 1 µs or sizes below
   1024 bytes, the squared terms lose precision. This is acceptable for video
   streaming workloads where intervals are typically in the millisecond range
   and sizes in the kilobyte-to-megabyte range.

3. **Lite backend provides only lifetime aggregates:** Without two snapshots
   to differentiate, the Lite backend reports statistics since pipeline start.
   It cannot provide windowed statistics on its own. The raw accumulator values
   are exposed to allow external consumers to implement differencing.

4. **Full backend ring buffer size is fixed at creation:** Changing the window
   size requires restarting the pipeline (or at least re-creating the probes).
   The setting applies to newly created pipelines only.

5. **Causal latency requires PTS presence on both sides:** Some elements strip
   or do not propagate PTS. In those cases, causal latency is unavailable for
   edges involving those elements. Tee elements are an exception: although
   their src pads are not probed, the system falls back to the tee's sink pad
   for PTS matching (Section 4.2).

6. **CPU attribution is approximate:** The processing time used for CPU
   attribution is a per-buffer causal measurement (Section 4.3), averaged
   across all buffers since the last snapshot. While significantly more stable
   than the previous point-in-time approach, it still has limitations for
   cross-thread elements (where processing time includes queuing delay) and
   for elements with highly variable processing times. The attribution is
   most reliable when the thread CPU is sampled over the same interval as
   the processing time measurement (both at ~1 Hz).

7. **Torn reads in Full backend:** A snapshot reader may observe a partially
   written slot at the ring buffer's write frontier. The system tolerates this
   because aggregates over hundreds of samples absorb a single corrupted
   entry. Slot values of `wall_ns = 0` are filtered out as uninitialized.
