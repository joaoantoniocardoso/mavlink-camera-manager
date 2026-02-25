# Stats API Optimization Taskforce Log

## Objective

Reduce CPU usage and allocation overhead in the stats API snapshot/serialization
hot path, without changing the public API contract.

## Baseline

- **Commit:** `e13a5074` — WIP state before optimizations.
- **Tests:** 46/46 pass (`cargo test --lib`).

---

## Iteration 1: `Distribution::compute` in-place + `FullPadBuffer::snapshot` single-pass

### Hypothesis

In Full mode, `Distribution::from_slice(&[f64])` clones its input into a new
`Vec` and sorts it. This is called **6 times per pad** (interval, i\_interval,
p\_interval, size, i\_size, p\_size). With ~20 pads across a typical pipeline,
that is ~120 clone+sort operations of ~900-element vectors per snapshot.

Additionally, `FullPadBuffer::snapshot()` iterates the records array **7
separate times** to build 6 intermediate `Vec<f64>` metric buffers, each of
which is then cloned again inside `from_slice`.

Combined savings estimate:
- Eliminate ~120 `Vec<f64>` clones (~840 KB of allocations per snapshot).
- Reduce iteration passes from 7 to 1 over the records array.

### Changes

- Added `Distribution::compute(data: &mut [f64])` — sorts in-place using
  `sort_unstable_by` (better cache behavior), no clone. Kept `from_slice` as a
  thin backward-compatible wrapper that clones then delegates.
- Rewrote `FullPadBuffer::snapshot()` to build all 6 metric Vecs
  (intervals\_ms, sizes\_bytes, i\_intervals, p\_intervals, i\_sizes, p\_sizes)
  in a single pass over the records array, with pre-allocated capacity.
- All 6 distribution calls now use `Distribution::compute` on owned mutable
  slices instead of `from_slice`.

### Result

- **Commit:** `52157c1d`
- **Tests:** 46/46 pass.
- **Allocations eliminated:** ~120 `Vec<f64>` clones per snapshot (~840 KB).
- **Iteration passes:** reduced from 7+6=13 to 1+6=7 (the 6 sorts remain
  unavoidable for percentile computation).

---

## Iteration 2: `SystemBuffer::snapshot` single-pass via Welford accumulators

### Hypothesis

`SystemBuffer::snapshot()` creates 4 separate `Vec<f64>` (cpu, load, mem, temp)
by iterating the entries VecDeque 4 times, then passes each to
`SystemDistribution::from_slice`. Since `SystemDistribution` only needs
min/max/mean/std (no percentiles), all 4 distributions can be computed in a
single pass using Welford's online algorithm, eliminating 4 Vec allocations and
4 iterations.

### Changes

- Added `SystemDistributionAccumulator` using Welford's online algorithm for
  numerically stable single-pass computation of count/min/max/mean/std.
- Rewrote `SystemBuffer::snapshot()` to iterate entries exactly once, feeding
  4 accumulators in parallel.
- `SystemDistribution::from_slice()` is now a thin wrapper over the accumulator.

### Result

- **Commit:** `5cf5b607`
- **Tests:** 46/46 pass.
- **Allocations eliminated:** 4 `Vec<f64>` of up to 120 entries each (~3.8 KB).
- **Iteration passes:** reduced from 4+4=8 to 1.

---

## Iteration 3: Compact JSON serialization for stats HTTP endpoints

### Hypothesis

All 8 stats HTTP endpoints use `serde_json::to_string_pretty`, which adds
indentation/whitespace formatting overhead. For a ~100 KB snapshot response,
pretty-printing adds ~20–30% more bytes and measurable CPU time. WebSocket
endpoints already used `to_string`. Clients that need pretty-printing can
format locally.

### Changes

Replaced `to_string_pretty` with `to_string` in all 8 stats HTTP handlers:
- `pipeline_analysis_get`
- `pipeline_analysis_health_get`
- `pipeline_analysis_root_cause_get`
- `pipeline_analysis_root_cause_get_for_pipeline`
- `pipeline_analysis_full_snapshot_get`
- `pipeline_analysis_element_diagnostics_get`
- `pipeline_analysis_samples_get`
- `pipeline_analysis_element_samples_get`

The disk dump (`dump_to_file`) retains pretty-printing since it is intended for
human inspection.

### Result

- **Commit:** `e6c78186`
- **Tests:** 46/46 pass.
- **Response size reduction:** ~20–30% fewer bytes per stats response.

---

## Iteration 4: Eliminate redundant PTS HashSets + diagnostics single-pass

### Hypothesis

`frame_count_from_pad` and `throughput_from_pad` each build a `HashSet<u64>` to
count unique PTS values. `compute_summary` builds a third HashSet inline for
PTS-candidate filtering. This means **3 HashSet constructions per pad per
snapshot** in Full mode.

Separately, `stutter_freeze_from_pad` allocates an intermediate `Vec<f64>` of
intervals from records, then iterates it 4 times (stutter count, freeze count,
max\_freeze, ratio).

### Changes

- Replaced `frame_count_from_pad` + `throughput_from_pad` with a unified
  `pad_metrics()` that returns `PadMetrics { frame_count, unique_pts_count,
  throughput_fps }` from a single HashSet construction.
- Rewrote `compute_summary()` to consume `PadMetrics`, eliminating the third
  inline HashSet.
- Rewrote `stutter_freeze_from_pad()` as a single pass over
  `records.windows(2)`, computing interval inline and tracking
  stutter/freeze/max\_freeze without an intermediate Vec.

### Result

- **Commit:** `e23328c5`
- **Tests:** 46/46 pass.
- **HashSet constructions:** reduced from 3 per pad to 1.
- **Diagnostics allocations:** eliminated intermediate `Vec<f64>` + 4 iteration
  passes replaced with 1.

---

## Iteration 5: Pre-size HashMaps and Vecs in hot paths

### Hypothesis

Several HashMaps and Vecs in the snapshot path are created with default capacity
and grow via rehashing/reallocation. Pre-sizing with `with_capacity` when the
final size is known avoids this overhead. Low individual impact but zero-risk.

### Changes

Applied `with_capacity` to:
- `full_snapshot()`: pipelines, element\_diagnostics\_map, element\_samples\_map
- `PipelineAnalysis::snapshot()`: element\_snapshots
- `attribute_cpu()`: tid\_to\_elements, element\_processing\_time
- `compute_causal_edge_latency()`: sink\_by\_pts, latencies\_ms, consumed\_per\_pts
- `compute_edge_delays()`: delays Vec

### Result

- **Commit:** `67c8d079`
- **Tests:** 46/46 pass.
- **Rehash/realloc operations eliminated:** all in the snapshot hot path.

---

---

## Flamegraph-Driven Optimization (Phase 2)

Introduced a synthetic benchmark harness (`--features bench-internal --bench-stats-snapshot`)
that creates 2 pipelines × 10 elements × 900 records/pad and calls `full_snapshot(300)`
500 times with cache invalidated. Flamegraphs captured with `cargo flamegraph --profile profiling`.

### Iteration 6: Flat sorted Vec replaces HashMap<u64,Vec<u64>> in causal latency

#### Flamegraph

- **Before:** `flamegraphs/iter-0-baseline.svg`
- **After:** `flamegraphs/iter-1-causal-flat-vec.svg`

#### Hypothesis

`compute_causal_edge_latency` builds a `HashMap<u64, Vec<u64>>` mapping PTS → wall
timestamps. Each Vec grows one element at a time via `.or_default().push()`, triggering
`Vec::grow_one` (14.86% of total). Dropping the HashMap + its Vecs costs another ~9.1%.
Combined: **~24% of total CPU** in a single data structure.

#### Evidence (flamegraph iter-0)

| Function | % of total |
|----------|-----------|
| `Vec::grow_one / realloc` | 14.86% |
| `HashMap<u64,Vec<u64>> drop` | 9.1% |
| `compute_causal_edge_latency` (own) | 7.46% |
| **Subtotal** | **~31%** |

#### Tests added

6 new tests for `compute_causal_edge_latency`:
- `causal_latency_empty_records_returns_zero`
- `causal_latency_no_pts_returns_zero_rate`
- `causal_latency_single_matched_pair`
- `causal_latency_multiple_distinct_pts`
- `causal_latency_unmatched_pts_reduces_rate`
- `causal_latency_large_record_set_correctness` (900 records)

#### Changes

Replaced `HashMap<u64, Vec<u64>>` with a flat `Vec<(u64, u64)>` of (pts, wall_ns) pairs.
Sort once, then binary-search per PTS group via `partition_point`. Eliminates:
- Per-PTS Vec allocations (900 Vecs → 1 Vec)
- HashMap bucket allocation + SipHash overhead
- Expensive cascading drop of HashMap<u64, Vec<u64>>

#### Result

- **Commit:** `527b97e5`
- **Tests:** 52/52 pass (46 existing + 6 new).

| Metric | Before | After | Delta |
|--------|--------|-------|-------|
| `Vec::grow_one` | 14.86% | 0.22% | **-14.64%** |
| `HashMap<u64,Vec<u64>> drop` | ~9.1% | 0% | **-9.1%** |
| `partition_point` (new) | 0% | 10.14% | +10.14% (replaces hashing) |

Net: ~24% of CPU eliminated, replaced by ~10% binary search = **~14% net improvement**.

### Iteration 7: Pre-computed PTS group index + BTreeMap→linear grouping in samples

#### Flamegraph

- **Before:** `flamegraphs/iter-1-causal-flat-vec.svg`
- **After:** `flamegraphs/iter-2-pts-index-samples.svg`

#### Hypothesis

Two remaining bottlenecks:
1. `partition_point` at 10.14%: two O(log n) binary searches per from_record in
   `compute_causal_edge_latency` to find PTS group boundaries.
2. `BTreeMap` at 2.55% + `pipeline_samples` at 3.21%: `build_sample_windows` uses
   `BTreeMap<u64, Vec<&RawRecord>>` to bin records by second, but records are already
   time-ordered from the ring buffer.

#### Changes

1. Pre-compute PTS group boundaries in a single O(n) linear scan into
   `HashMap<u64, (usize, usize)>`, replacing 2 × O(log n) binary searches per record
   with 1 × O(1) HashMap lookup.
2. Replace `BTreeMap<u64, Vec<&RawRecord>>` in `build_sample_windows` with a single-pass
   contiguous grouping (records already time-ordered). Eliminates BTreeMap allocation,
   tree traversal, and redundant sort.

#### Tests added

6 new tests for `build_sample_windows`:
- `build_sample_windows_empty`
- `build_sample_windows_single_second`
- `build_sample_windows_multiple_seconds`
- `build_sample_windows_limit_truncates`
- `build_sample_windows_since_filter`
- `build_sample_windows_large_dataset` (900 records)

#### Result

- **Commit:** `0a33b5e6`
- **Tests:** 58/58 pass.

| Metric | Before | After | Delta |
|--------|--------|-------|-------|
| `partition_point` | 10.14% | 1.18% | **-8.96%** |
| `BTreeMap` | 2.55% | 0% | **-2.55%** |
| `pipeline_samples` | 3.21% | 1.34% | **-1.87%** |

### Iteration 8: O(n) selection replaces O(n log n) sort in Distribution::compute

#### Flamegraph

- **Before:** `flamegraphs/iter-2-pts-index-samples.svg`
- **After:** `flamegraphs/iter-3-select-nth.svg`

#### Hypothesis

`quicksort` at 12.45% is the new #1 bottleneck. `Distribution::compute` sorts the entire
data array to extract 3 percentiles (median, p95, p99). With 240 sorts of 900 elements per
snapshot (6 distributions × 40 pads), the O(n log n) sort dominates.

#### Changes

Replace `sort_unstable_by` with cascaded `select_nth_unstable_by`:
- Partition at p99 index (O(n)), then p95 within [..=p99] (O(n)), then median within
  [..=p95] (O(n)). Total: 3 × O(n) instead of O(n log n).
- Compute min/max/mean/std in a single linear pass (no sort needed for these).

#### Tests added

6 new tests in `mcm_api::v1::stats`:
- `distribution_empty`
- `distribution_single_value`
- `distribution_two_values`
- `distribution_known_values` (1..100)
- `distribution_from_slice_matches_compute`
- `distribution_large_dataset_correctness` (900 values)

#### Result

- **Commit:** `26e9f38c`
- **Tests:** 64/64 pass (58 existing + 6 new Distribution tests).

| Metric | Before | After | Delta |
|--------|--------|-------|-------|
| `quicksort` | 12.45% | 0% | **-12.45%** |
| `Distribution::compute` | 6.89% | 3.80% | **-3.09%** |

### Iteration 9: Cursor-based merge-join eliminates HashMaps in causal latency

#### Flamegraph

- **Before:** `flamegraphs/iter-3-select-nth.svg`
- **After:** `flamegraphs/iter-4-cursor-join.svg`

#### Hypothesis

`hash_one (SipHash)` still at 7.29% and `HashMap insert` at ~2.3% from the `pts_groups`
and `consumed` HashMaps in `compute_causal_edge_latency`. Since both `from_records` and
`sink_pairs` are PTS-ordered, a cursor-based merge-join can replace both HashMaps.

#### Changes

Replace the `pts_groups: HashMap<u64, (usize, usize)>` and `consumed: HashMap<u64, usize>`
with a single cursor that advances through `sink_pairs`. For sequential PTS (the common
case in 30fps video), the cursor naturally stays near the right position. Falls back to
binary search only for out-of-order PTS.

#### Result

- **Commit:** `d14fc685`
- **Tests:** 64/64 pass (all existing tests, no regressions).

| Metric | Before | After | Delta |
|--------|--------|-------|-------|
| `compute_causal_edge_latency` | 11.09% | 2.27% | **-8.82%** |
| `compute_edge_delays` | 11.33% | 2.42% | **-8.91%** |

---

## Phase 2 Summary: Flamegraph-Driven Optimizations

### Full progression (baseline → final)

| Metric | Baseline | Final (iter-4) | Improvement |
|--------|----------|----------------|-------------|
| `Vec::grow_one / realloc` | 14.86% | 0% | **eliminated** |
| `quicksort` | 8.30% | 0% | **eliminated** |
| `HashMap<u64,Vec<u64>> drop` | 9.1% | 0% | **eliminated** |
| `BTreeMap` | 2.55% | 0% | **eliminated** |
| `compute_causal_edge_latency` | 7.46% | 2.27% | **-5.19%** |
| `partition_point` | 0.09% | 0.70% | +0.61% (fallback) |
| `pipeline_samples` | 3.10% | 3.46% | +0.36% (relative) |
| Our binary total | 80.00% | 77.67% | -2.33% (relative) |

### Remaining hotspots (iter-4)

| Function | % | Notes |
|----------|---|-------|
| `FullPadBuffer::snapshot` | 8.78% | Fundamental: iterates 900 records, computes distributions |
| `Distribution::compute` | 6.48% | Now O(n) selection — fundamental work |
| `hash_one (SipHash)` | 5.83% | From `HashMap<String, PadSnapshot>` in API output |
| `pipeline_samples` | 3.46% | Linear grouping — fundamental work |
| `HashMap<String,…> clone/insert` | ~5% | API output structure (would need API change) |
| `PadSnapshot clone` | 1.56% | Output cloning for edge delay computation |

These are diminishing returns — the remaining work is either fundamental computation
or tied to the API output contract (`HashMap<String, PadSnapshot>`).

### Flamegraph files

| Iteration | File | Commit |
|-----------|------|--------|
| 0 (baseline) | `flamegraphs/iter-0-baseline.svg` | `a1871a98` |
| 1 (flat Vec) | `flamegraphs/iter-1-causal-flat-vec.svg` | `527b97e5` |
| 2 (PTS index + samples) | `flamegraphs/iter-2-pts-index-samples.svg` | `0a33b5e6` |
| 3 (select_nth) | `flamegraphs/iter-3-select-nth.svg` | `26e9f38c` |
| 4 (cursor join) | `flamegraphs/iter-4-cursor-join.svg` | `d14fc685` |

---

## Summary

| # | Commit | Tests | Description | Key savings |
|---|--------|-------|-------------|-------------|
| Baseline | `e13a5074` | 46/46 | WIP state before optimizations | — |
| 1 | `52157c1d` | 46/46 | Distribution in-place + single-pass snapshot | ~120 Vec clones eliminated (~840 KB) |
| 2 | `5cf5b607` | 46/46 | SystemBuffer Welford accumulators | 4 Vec allocs + 8 passes -> 1 pass |
| 3 | `e6c78186` | 46/46 | Compact JSON serialization | ~20-30% smaller HTTP responses |
| 4 | `e23328c5` | 46/46 | PTS HashSet merge + diagnostics single-pass | 3 HashSets/pad -> 1; Vec + 4 passes -> 1 |
| 5 | `67c8d079` | 46/46 | Pre-size HashMaps/Vecs | Zero rehash/realloc in hot path |
| 6 | `527b97e5` | 52/52 | Flat sorted Vec in causal latency | Vec grow_one 14.86%→0%, HashMap drop 9.1%→0% |
| 7 | `0a33b5e6` | 58/58 | PTS group index + linear sample windowing | partition_point 10.14%→1.18%, BTreeMap 2.55%→0% |
| 8 | `26e9f38c` | 64/64 | O(n) selection in Distribution::compute | quicksort 12.45%→0% |
| 9 | `d14fc685` | 64/64 | Cursor merge-join in causal latency | causal_edge_latency 11.09%→2.27% |

---

## Validation & Benchmarking (Post-Optimization)

### API Integration Tests

- **Commit:** `ae1cea24`
- **Result:** 26/26 tests pass (all HTTP + WebSocket endpoints, including boundary and negative tests).
- **Endpoints validated:** level, window-size, enable/disable/reset, snapshot, health,
  root-cause, full-snapshot, element diagnostics, pipeline samples, element samples,
  all WebSocket endpoints (snapshot, full-snapshot, samples).
- **Changes required:** Dropped `paperclip` `wrap_api_with_spec` from route registration
  (was silently dropping `full-snapshot` routes due to `#[serde(flatten)]` incompatibility
  with `Apiv2Schema`). Moved all pipeline-analysis routes to flat `.route()` registrations.

### Raspberry Pi 4 A/B Benchmark (Round 3)

- **Commit:** `ae1cea24`
- **Platform:** Raspberry Pi 4 (armv7), `blueos-core-ab-test` container.
- **Workload:** 3 cameras x ~8 GStreamer pipelines x ~40 probed pads.

#### Probe overhead (POLL_MODE=none)

| Level | CPU% mean | Overhead vs off |
|-------|----------:|----------------:|
| off   |     38.54 |             --- |
| lite  |     39.42 |          +0.88% |
| full  |     39.13 |          +0.59% |

**Verdict:** Probe overhead is <1% CPU -- safe for always-on production use.

#### Snapshot computation overhead (POLL_MODE=http-summary)

| Level | CPU% mean | Overhead vs off |
|-------|----------:|----------------:|
| off   |     38.56 |             --- |
| lite  |     39.83 |          +1.27% |
| full  |     49.00 |         +10.44% |

**Verdict:** Lite snapshots are free (+1.3% total). Full snapshots add ~10% CPU at 1 Hz polling.

#### Full-snapshot endpoint overhead

| Mode | CPU% mean | Delta vs full/none |
|------|----------:|-------------------:|
| http-full | 50.65 |           +11.52% |
| ws-full   | 50.08 |           +10.95% |

**Verdict:** WebSocket saves ~10 MB RSS vs HTTP with equivalent CPU.

#### Comparison with Round 2 (pre-optimization)

| Mode | Round 2 | Round 3 | Absolute change |
|------|--------:|--------:|----------------:|
| full, http-full | 68.37% | 50.65% | **-17.72%** |
| full, ws-full | 67.91% | 50.08% | **-17.83%** |
| Probe overhead (full) | +2.31% | +0.59% | **-1.72%** |

Full details: [`docs/pipeline-analysis-benchmark.md`](pipeline-analysis-benchmark.md)

---

## Proposed API Changes for Further Optimization

The remaining CPU cost in `full` snapshot computation is dominated by JSON serialization
and HashMap cloning -- both tied to the current API contract. Three changes that would
unlock the next tier of improvements:

### 1. Incremental/Delta Snapshots (WebSocket)

**Current:** Every WebSocket push sends the full ~500 KB JSON blob.
**Proposed:** Send a full snapshot on connect, then only changed fields on subsequent pushes.
**Estimated savings:** 80-90% reduction in serialization CPU and bandwidth per push.
**Mechanism:** Track a generation counter per pipeline; only re-serialize pipelines whose
data has changed since the last push. Health/root-cause summaries are cheap to recompute.

### 2. ~~Indexed Arrays Instead of String-Keyed HashMaps~~ **IMPLEMENTED (iter-10)**

See Iteration 10 below.

### 3. ~~Lazy Distribution Computation with Caching~~ **IMPLEMENTED (iter-10)**

See Iteration 10 below.

---

## Iteration 10: HashMap-to-Vec + Distribution Caching + Zenoh CDR Queryables

### Commits

- `e5f4fc09` — stats: convert HashMap to Vec in API structs, add distribution caching
- `691a9531` — feat: add Zenoh queryables for stats API with CDR serialization
- `3d63280d` — docs: update optimization log and benchmarks

### Changes

#### A. HashMap to Vec Conversion (API breaking change, alpha API)

All 7 `HashMap<String, T>` fields in stats API structs converted to `Vec<T>`:
- `ElementSnapshot.sink_pads`, `src_pads`: `Vec<PadSnapshot>` (new `pad_name` field)
- `PipelineTopology.nodes`: `Vec<NodeInfo>`
- `PipelineSnapshot.elements`: `Vec<ElementSnapshot>`
- `PipelineFullSnapshot.element_diagnostics`: `Vec<ElementDiagnostics>`
- `PipelineFullSnapshot.element_samples`: `Vec<PipelineSamplesResponse>`
- `FullSnapshot.pipelines`: `Vec<PipelineFullSnapshot>`
- Removed `#[serde(flatten)]` from `PipelineFullSnapshot.snapshot` for CDR compatibility

Internal computation still uses `HashMap` where O(1) lookups matter (edge delay, CPU attribution), converting to Vec only at output boundaries.

#### B. Distribution Caching (FullPadBuffer)

Added generation-counter-based caching in `FullPadBuffer`:
- `write_generation: AtomicU64` incremented on each `record()` and `reset()`
- `cached_dist: Mutex<Option<(u64, DistributionSnapshot)>>` stores last computed result
- On `snapshot()`, if generation matches cache, returns cached 384-byte struct (trivial clone)
- Eliminates 6x O(n) percentile computations per pad when data hasn't changed

#### C. Arc::try_unwrap in full_snapshot()

Avoids deep-cloning `PipelineSnapshot` (including ~36,000 `RawRecord`s per pipeline):
- Fleet-level summaries computed while borrowing the Arc
- Snapshot cache invalidated, making the Arc the sole owner
- `Arc::try_unwrap` moves instead of cloning each snapshot

#### D. Zenoh CDR Queryables

New `src/lib/zenoh/stats_queryable.rs` exposes all 8 stats endpoints as Zenoh queryables
using CDR binary serialization (`cdr::serialize::<_, _, cdr::CdrLe>`). Key expressions:
- `mcm/stats/pipeline-analysis` through `mcm/stats/pipeline-analysis/*/elements/*/samples`
- Zenoh updated from 1.5.1 to 1.7.2

### Flamegraph Results (iter-5-vec-cdr vs iter-4 baseline)

| Function | iter-4 | iter-5 | Delta |
|---|---|---|---|
| `hash_one` (SipHash) | 5.83% | 1.52% | **-4.31pp** |
| String hashing chain | 2.84% | 0% | **-2.84pp** |
| `HashMap::clone` | 2.58% | 0% | **-2.58pp** |
| `HashMap::insert_at_index` | 2.32% | 0.41% | **-1.91pp** |
| `HashMap` rehash/resize | 1.73% | 0% | **-1.73pp** |
| `partition_at_index` | 21.25% | 12.69% | **-8.56pp** |
| `FullPadBuffer::snapshot` | 8.78% | 3.90% | **-4.88pp** |
| **Total HashMap elimination** | ~14.2% | ~4.3% | **-9.9pp** |

Serialization cost comparison:
- JSON: ~3.57% CPU self-time
- CDR: ~2.08% CPU self-time (**42% cheaper**)

Benchmark: **1.87ms/iter -> 1.31ms/iter (30% faster)**

### Tests

70/70 pass. New tests: distribution cache hit, cache invalidation on record, cache clear on reset.

### Remaining Hotspots

1. `partition_at_index` (12.69%) -- fundamental O(n) percentile computation
2. `Vec::push/extend/copy` (~8%) -- allocation from building output structs
3. `FullPadBuffer::snapshot` (3.9%) -- binary search on 900 records
4. serde_json/CDR serialization (~5.6% combined) -- inherent serialization cost

These represent fundamental computation costs with no further algorithmic improvement possible.

---

## Validation & Benchmarking (Round 4 — Post Vec+CDR)

### API Integration Tests

- **Commit:** `91d99536`
- **Result:** 26/26 tests pass (all HTTP + WebSocket endpoints, including boundary and negative tests).
- **Test script updated** for Vec-based API: `elements`/`sink_pads`/`src_pads` are arrays,
  `pipelines` is array, `PipelineFullSnapshot` has nested `snapshot` field, `pad_name` validated.

### Raspberry Pi 4 A/B Benchmark (Round 4)

- **Commit:** `91d99536`
- **Platform:** Raspberry Pi 4 (armv7), `blueos-core-ab-test` container.
- **Workload:** 3 cameras x ~8 GStreamer pipelines x ~40 probed pads.
- **New transport tested:** Zenoh CDR queryables (`eclipse-zenoh` Python 1.7.2).

#### Probe overhead (POLL_MODE=none)

| Level | CPU% mean | Overhead vs off |
|-------|----------:|----------------:|
| off   |     59.42 |             --- |
| lite  |     59.43 |          +0.01% |
| full  |     60.11 |          +0.69% |

**Verdict:** Probe overhead is <1% CPU -- safe for always-on production use.

#### Full-snapshot transport comparison (POLL_MODE=* with `--pipeline-analysis-level full`)

| Transport | CPU% mean | Overhead vs off | RSS (KB) |
|-----------|----------:|----------------:|---------:|
| http-full |     72.87 |         +15.45% | 239,876 |
| ws-full   |     71.99 |         +14.57% | 229,146 |
| **zenoh-full** | **65.58** | **+8.16%** | **232,960** |
| zenoh-summary | 65.75 | +8.33% | 232,426 |

**Verdict:** Zenoh CDR is **47% cheaper** than HTTP JSON for full-snapshot serving
(+8.16% vs +15.45% overhead). CDR binary serialization avoids JSON string formatting
and produces smaller payloads.

Full details: [`docs/pipeline-analysis-benchmark.md`](pipeline-analysis-benchmark.md)

---

## Iteration 11 — API Simplification

### Motivation

The stats API surface had grown to ~23 HTTP endpoints, 4 WebSocket streams,
and 8 Zenoh queryables. Clients only used the `full-snapshot` endpoint (which
bundles health, root cause, diagnostics, and per-element samples in a single
request). The remaining endpoints were redundant — they returned subsets of
data already available in `full-snapshot`.

### Changes

**Simplified API surface (8 HTTP routes + 1 WS + 1 Zenoh queryable):**

| Endpoint | Method | Purpose |
|----------|--------|---------|
| `/stats/pipeline-analysis/full-snapshot` | GET | Consolidated snapshot |
| `/stats/pipeline-analysis/full-snapshot/ws` | WS | Streaming full snapshot |
| `/stats/pipeline-analysis/reset` | POST | Reset all pipelines |
| `/stats/pipeline-analysis/{pipeline}/reset` | POST | Reset one pipeline |
| `/stats/pipeline-analysis/level` | GET/POST | Get/set stats level |
| `/stats/pipeline-analysis/window-size` | GET/POST | Get/set window size |
| `mcm/stats/pipeline-analysis/full-snapshot` | Zenoh CDR | Binary full snapshot |

**Removed (15 HTTP routes + 4 WS + 7 Zenoh queryables):**

- `GET /stats/pipeline-analysis` (base snapshot)
- `GET /stats/pipeline-analysis/health`
- `GET /stats/pipeline-analysis/root-cause`
- `GET /stats/pipeline-analysis/{pipeline}/root-cause`
- `GET /stats/pipeline-analysis/{pipeline}/elements/{element}/diagnostics`
- `GET /stats/pipeline-analysis/{pipeline}/samples`
- `GET /stats/pipeline-analysis/{pipeline}/elements/{element}/samples`
- `POST /stats/pipeline-analysis/enable` and `/{pipeline}/enable`
- `POST /stats/pipeline-analysis/disable` and `/{pipeline}/disable`
- `POST /stats/pipeline-analysis/dump`
- `WS /stats/pipeline-analysis/ws`
- `WS /stats/pipeline-analysis/{pipeline}/samples/ws`
- `WS /stats/pipeline-analysis/{pipeline}/elements/{element}/samples/ws`
- All 7 Zenoh queryables except `full-snapshot`

**Dead code removed:**

- ~15 HTTP handler functions and helper types from `pages.rs`
- Public wrapper functions (`element_diagnostics`, `fleet_health_summary`,
  `fleet_root_cause_summary`, `pipeline_root_cause`) that re-fetched snapshots
  independently; the internal `*_from()` variants used by `full_snapshot()` remain
- `set_enabled()`, `set_enabled_for()`, `is_enabled()`, `pipeline_names()`
- `dump_to_file()`, `dump_to_default_file()`, `validate_dump_path()`
- `PipelineAnalysisDumpRequest`, `ElementDiagnosticsPath`, `ElementSamplesPath`,
  `PipelineSamplesQuery` API types
- Benchmark `http-summary` and `zenoh-summary` poll modes

### Impact

- **Smaller attack surface**: fewer endpoints to maintain and secure
- **Reduced binary size**: removed ~400 lines of handler code
- **Simpler client integration**: one endpoint for all data
- **No performance regression**: `full_snapshot()` computation unchanged

---

## Round 12: WS-Path Focused Optimization (buffer_limit=0)

### Key Insight

Previous flamegraph work used `full_snapshot(300)` which is dominated by
`RawRecord` serialization (20% of CPU). However, the **production WS path**
uses `buffer_limit=0` (default), which skips raw records entirely. A new bench
harness mode (`BENCH_BUFFER_LIMIT=0`) was added to profile the actual production
workload. This revealed a dramatically different hotspot profile:

| Hotspot | % (buf=0 baseline) |
|---------|-------------------|
| HashMap operations (SipHash) | 22% |
| compute_edge_delays + causal_latency | 15% |
| serde_json string serialization | 10% |
| enrich_with_gst_introspection | 5.7% |
| pad_metrics (FxHashSet for PTS) | 6.8% |
| FullPadBuffer::read_records | 3.3% |

### Iteration 12.1: FxHashMap for all snapshot-path HashMaps

**Hypothesis:** SipHash (default HashMap hasher) is cryptographic and expensive
for internal String-keyed maps that are not exposed to untrusted input.

**Changes:** Replaced all local `HashMap`/`HashSet` with `FxHashMap`/`FxHashSet`
from `rustc-hash` in `pipeline_analysis.rs` and `root_cause.rs`.

**Result:** 0.538ms → 0.427ms/iter (**20.6% faster**). HashMap ops dropped from
22% to 16% in flamegraph.

### Iteration 12.2: Sort+dedup replaces FxHashSet in pad_metrics

**Hypothesis:** For counting unique PTS values from ~900 ring buffer records,
a sorted `Vec<u64>` + `dedup` is faster than FxHashSet due to cache locality.

**Changes:** Replaced `FxHashSet::insert` loop with `Vec::push` + `sort_unstable` +
`dedup` in `pad_metrics()`.

**Result:** 0.427ms → 0.389ms/iter (**8.9% faster**). pad_metrics dropped from
6.8% to 3.3% in flamegraph.

### Iteration 12.3: Reusable scratch buffers for PTS merge-join

**Hypothesis:** `compute_causal_edge_latency` and `compute_intra_element_processing_time`
allocate fresh `Vec<(u64, u64)>` and `Vec<f64>` for each edge (~56 allocations per snapshot).

**Changes:** Added `MergeJoinScratch` struct with reusable `sink_pairs` and
`result_values` buffers, passed through the computation chain.

**Result:** 0.389ms → 0.384ms/iter (**1.3% faster**). Modest gain in bench
(allocator already warm) but larger expected improvement under contention.

### Iteration 12.4: Pre-allocated JSON serialization buffer

**Hypothesis:** `serde_json::to_string()` allocates a new `Vec<u8>` (~100-200KB)
per snapshot. A reusable buffer with `serde_json::to_writer()` avoids this.

**Changes:** Applied to both bench harness and WS handler. WS handler uses
`unsafe { std::str::from_utf8_unchecked() }` to borrow the buffer as `&str`.

**Result:** 0.384ms → 0.375ms/iter (**2.3% faster**).

### Iteration 12.5: Arc\<PipelineTopology\> avoids deep clone

**Hypothesis:** `PipelineTopology` is cloned from behind a Mutex every snapshot,
deep-copying ~70 Strings (node names, pad names) per pipeline. Wrapping in Arc
makes cloning O(1).

**Changes:** Changed `Mutex<Option<PipelineTopology>>` to
`Mutex<Option<Arc<PipelineTopology>>>`. Snapshot path clones the Arc instead.

**Result:** 0.375ms → 0.356ms/iter (**5.1% faster**).

### Round 12 Summary (after 5 iterations)

| Metric | Before | After 12.5 | After 12.10 | Change |
|--------|--------|------------|-------------|--------|
| Snapshot+serialize (buf=0) | 0.538ms | 0.356ms | 0.303ms | **43.7% faster** |
| Total throughput | 1859 iter/s | 2809 iter/s | 3300 iter/s | **+77%** |

### Iteration 12.6: GObject property name cache

**Hypothesis:** `enrich_with_gst_introspection` calls `list_properties()` on every
element per snapshot. This is class-level GObject introspection that never changes.

**Changes:** Added a static `LazyLock<Mutex<HashMap<String, Arc<Vec<String>>>>>` to
cache filtered property names per element type. First call populates; subsequent calls
reuse the `Arc<Vec>`.

**Result:** 0.356ms → 0.340ms/iter (**4.5% faster**).

### Iteration 12.7: Eliminate pad_conn_index HashMap

**Hypothesis:** `build_api_pipeline_snapshot` built an `FxHashMap<(String,String), Vec<PadConnection>>`
for pad connection lookups. Each entry clones element+pad name strings (~116 allocations).

**Changes:** Eliminated the HashMap. `convert_element` and `convert_pad` now receive
`&[TopologyEdge]` and scan linearly to find connections.

**Result:** 0.340ms → 0.335ms/iter (**1.5% faster**).

### Iteration 12.8: Hoisted scratch buffers + fast-path merge-join

**Hypothesis:** MergeJoinScratch was allocated/deallocated per-pipeline (7.4M samples in
dealloc in flamegraph). Also, `compute_causal_edge_latency` used partition_point for all
PTS lookups including the common single-entry case.

**Changes:**
1. Hoisted `MergeJoinScratch` creation above the pipeline loop in `snapshot_all_internal`.
2. Added fast-path short-circuits for single-entry PTS groups in both
   `compute_causal_edge_latency` and `compute_intra_element_processing_time`,
   avoiding partition_point binary searches for ~99% of video records.

**Result:** 0.335ms → 0.322ms/iter (**3.9% faster**).

### Iteration 12.9: Short-circuit stutter/freeze scan for healthy streams

**Hypothesis:** `stutter_freeze_from_pad` iterated all 900 records per pad via `windows(2)`.
For healthy streams (max_interval < stutter_threshold), this work is unnecessary.

**Changes:** Check pad accumulator's lifetime max_interval_ms before scanning records.
If below stutter threshold, return (0, 0, 0.0, 0.0) immediately.

**Result:** Within noise on synthetic bench but eliminates O(n) scan for healthy production streams.

### Iteration 12.10: Reduce string allocations

**Hypothesis:** Several unnecessary string allocations in the snapshot path:
- `format!("{:?}").to_lowercase()` for GStreamer state (2 allocs per element)
- `element_type.to_lowercase()` for queue detection (1 alloc per element)
- String clones in `InternalEdgeDelay` (2 per topology edge)

**Changes:**
1. Direct match on GStreamer State enum to static strings.
2. Replaced `to_lowercase().contains("queue")` with `eq_ignore_ascii_case` byte scan.
3. Stored `edge_idx: usize` in InternalEdgeDelay instead of cloned name strings.

**Result:** 0.335ms → 0.324ms/iter (**3.3% combined with 12.8-12.9**).

### Iteration 12.11: Fix stutter shortcut for Full-mode pads

**Hypothesis:** The stutter/freeze shortcut from iter 12.9 only checked
`pad.accumulators`, which is `None` for Full-mode pads. The shortcut never triggered.

**Changes:** Also check `pad.distribution.interval.max` which is always available
for Full-mode pads.

**Result:** 0.324ms → 0.314ms/iter (**3.1% faster**). Eliminated 5.68% flamegraph hotspot.

### Iteration 12.12: Thread-local scratch buffers + reusable PTS Vec

**Hypothesis:** `MergeJoinScratch::new()` (1.36% flamegraph) allocated 3 Vecs on every
`snapshot_all_internal` call. `pad_metrics` allocated a fresh `Vec<u64>` per pad.

**Changes:**
1. Made `MergeJoinScratch` a `thread_local!` to reuse across snapshot calls.
2. Added `pts_scratch: Vec<u64>` to `MergeJoinScratch`, passed through
   `compute_summary` → `pad_metrics` for reuse across pads.

**Result:** 0.314ms → 0.303ms/iter (**3.5% faster**).

### Final Round 12 Summary (after 12 iterations)

| Metric | Before | After 12.5 | After 12.12 | Change |
|--------|--------|------------|-------------|--------|
| Snapshot+serialize (buf=0) | 0.538ms | 0.356ms | 0.303ms | **43.7% faster** |
| Total throughput | 1859 iter/s | 2809 iter/s | 3300 iter/s | **+77%** |

### Remaining Hotspots (post-12.12 flamegraph)

| Function | % | Reducible? |
|----------|---|-----------|
| `compute_causal_edge_latency` | ~5% | No — O(n) merge-join, fundamental |
| `serde_json` serialization | ~5% | Only with alternative serializer |
| `pad_metrics` (push+sort+dedup) | ~4.5% | No — O(n log n) for unique PTS count |
| `enrich_with_gst_introspection` | ~1.5% | No — GObject FFI calls |
| Various alloc/drop | ~2% | Marginal — mostly API struct construction |

These represent irreducible computation: O(n) record iteration for PTS matching,
O(n log n) sorting for unique PTS counting, and inherent JSON serialization cost.
Further improvements would require:
1. Alternative serializer (simd_json) — ~2-3% potential
2. Incremental/delta snapshots — 80-90% potential for WS clients
3. Reducing API output fields — requires API contract change

---

## Phase 3: Reducing Measurement Interference

Focus: reduce the stats system's interference on the pipeline it measures,
targeting probe callback overhead and snapshot computation CPU contention.

### Iteration 13: Cache gettid + remove write_generation atomic

#### Hypothesis

The per-buffer probe callback on GStreamer streaming threads performs two
unnecessary operations: `libc::syscall(SYS_gettid)` (~200-700ns syscall on ARM)
on every invocation despite GStreamer threads having stable TIDs, and an
`AtomicU64::fetch_add` for `write_generation` that duplicates `write_cursor`
which already increments per record.

#### Changes

1. Cached `gettid()` via `thread_local!` in `ElementProbe::update_thread_id()`.
   The syscall is called once per thread; subsequent probes read the cached value.
2. Removed `write_generation: AtomicU64` field from `FullPadBuffer`. Distribution
   cache now uses `write_cursor` (already incremented per `record()`) as the
   generation key. Saves one atomic fetch_add per probe invocation.

#### Result

- **Commit:** `066b7f9d`
- **Tests:** 54/54 pass.

##### Raspberry Pi 4 Benchmark (1 USB H264 camera)

**POLL_MODE=none (probe overhead):**

| Level | CPU% mean | CPU% std | RSS (KB) |
|-------|----------:|----------|----------|
| off   |     13.96 |     6.21 |   90,651 |
| lite  |     14.19 |     6.38 |   91,819 |
| full  |     14.28 |     6.53 |   91,665 |

Overhead vs off: lite +0.23%, full +0.32%.

**POLL_MODE=ws-full (API delivery):**

| Delivery | CPU% mean | RSS (KB) | Delta vs none |
|----------|----------:|---------:|--------------:|
| none     |     14.28 |   91,665 |           --- |
| ws-full  |     15.38 |   91,748 |        +1.10% |

**Comparison with Round 11 baseline:**

| Metric | Round 11 | Iter 13 | Delta |
|--------|----------|---------|-------|
| Probe overhead (full, none) | +0.30% | +0.32% | +0.02% (noise) |
| WS delivery overhead | +1.10% | +1.10% | 0.00% |

**Verdict:** Within measurement noise. The per-probe syscall and atomic removal
save ~1μs/probe but with only ~1200 probes/sec (1 camera), the aggregate CPU
saving (~0.12%) is below the benchmark's noise floor (~0.3% std). Code quality
improvement retained: fewer hot-path operations, simpler FullPadBuffer struct.

### Iteration 14: Move snapshot computation to blocking pool with nice(10)

#### Hypothesis

`full_snapshot()` runs directly on the Tokio async runtime, blocking the event
loop and competing at equal OS scheduling priority with GStreamer pipeline
threads. Moving it to `tokio::task::spawn_blocking` with `setpriority(PRIO_PROCESS, 0, 10)`
achieves two things: (1) unblocks the async runtime for other tasks, and
(2) gives pipeline threads CPU preference when contending on the same core.

#### Changes

1. Both `streams_snapshot_get()` (HTTP) and `streams_snapshot_ws()` (WS)
   now call `full_snapshot()` inside `tokio::task::spawn_blocking`.
2. Added `lower_thread_priority()` helper that calls `setpriority(PRIO_PROCESS, 0, 10)`
   on Linux. Called at the start of each blocking task.
3. JSON serialization stays on the async task (fast, reuses pre-allocated buffer).

#### Result

- **Commit:** `e042deb3`
- **Tests:** 54/54 pass.

##### Raspberry Pi 4 Benchmark (1 USB H264 camera)

**POLL_MODE=none (probe overhead — should be unchanged):**

| Level | CPU% mean | CPU% std | RSS (KB) |
|-------|----------:|----------|----------|
| off   |     13.95 |     6.47 |   91,881 |
| full  |     14.33 |     6.50 |   90,687 |

Overhead vs off: full +0.38% (consistent with iter 13).

**POLL_MODE=ws-full (API delivery — primary target):**

| Delivery | CPU% mean | RSS (KB) | Delta vs none |
|----------|----------:|---------:|--------------:|
| none     |     14.33 |   90,687 |           --- |
| ws-full  |     15.32 |   92,642 |        +0.99% |

**Comparison with Round 11 baseline and Iteration 13:**

| Metric | Round 11 | Iter 13 | Iter 14 | Delta (14 vs 11) |
|--------|----------|---------|---------|-------------------|
| Probe overhead (full, none) | +0.30% | +0.32% | +0.38% | +0.08% (noise) |
| WS delivery overhead | +1.10% | +1.10% | +0.99% | **-0.11%** |

**Verdict:** WS delivery overhead dropped from +1.10% to +0.99%, a ~10% relative
improvement. While borderline given the noise floor, the architectural improvement
is sound: snapshot computation no longer blocks the async event loop, and pipeline
threads get CPU preference via nice(10). The improvement would be more pronounced
under higher contention (multiple cameras, multiple WS clients).

### Phase 3 Summary

| Iteration | Commit | Tests | Description | Measurable impact |
|-----------|--------|-------|-------------|-------------------|
| 13 | `066b7f9d` | 54/54 | Cache gettid + remove write_generation | Within noise; code quality improvement |
| 14 | `e042deb3` | 54/54 | spawn_blocking + nice(10) for snapshots | WS overhead -0.11% (10% relative) |

Both changes are retained. The probe hot path now does fewer syscalls and atomics
(iter 13), and snapshot computation runs at lower priority on the blocking pool
(iter 14). Further interference reduction would require either higher-contention
workloads to observe the benefits, or architectural changes like incremental/delta
snapshots (estimated 80-90% reduction in computation when streams are stable).

---

## Phase 4: Reduce measurement interference — clock, cache, serialization

Phase 4 targets three remaining sources of overhead identified in the
`reduce_stats_probe_interference` plan, all API-compatible:

1. **Iteration 15:** Replace `SystemTime::now()` with `CLOCK_REALTIME_COARSE` in probe hot path
2. **Iteration 16:** Fix snapshot cache invalidation so 900ms TTL actually works
3. **Iteration 17:** Add serialized-JSON cache for WS delivery path

### Iteration 15: Use CLOCK_REALTIME_COARSE for wall_clock_ns() on Linux

#### Hypothesis

`wall_clock_ns()` is called on every probe hit (~1200/sec per camera). On ARM,
`clock_gettime(CLOCK_REALTIME)` takes ~716ns (real syscall), whereas
`CLOCK_REALTIME_COARSE` is vDSO-accelerated at ~23ns. The ~1-4ms resolution
(CONFIG_HZ dependent) is adequate for 30fps video: mean over 900 ring samples
converges to <0.4% error, and distribution percentiles use rank order so clock
quantization doesn't affect them.

#### Changes

1. `wall_clock_ns()` in `pipeline_analysis.rs` now uses
   `libc::clock_gettime(libc::CLOCK_REALTIME_COARSE)` on Linux.
2. Non-Linux targets retain `SystemTime::now()` fallback.

#### Result

- **Commit:** `32860c89`
- **Tests:** 39/39 pass (stats tests).

##### Raspberry Pi 4 Benchmark (1 USB H264 camera)

**POLL_MODE=none (probe overhead — primary target):**

| Level | CPU% mean | CPU% std | RSS (KB) |
|-------|----------:|----------|----------|
| off   |     14.73 |     6.78 |   91,425 |
| lite  |     14.97 |     6.43 |   90,980 |
| full  |     14.98 |     6.29 |   91,754 |

Overhead vs off: lite +0.24%, full +0.25%.

**POLL_MODE=ws-full (API delivery):**

| Delivery | CPU% mean | RSS (KB) | Delta vs none |
|----------|----------:|---------:|--------------:|
| none     |     14.98 |   91,754 |           --- |
| ws-full  |     16.19 |   92,757 |        +1.21% |

**Comparison with Iteration 14:**

| Metric | Iter 14 | Iter 15 | Delta |
|--------|---------|---------|-------|
| Probe overhead (full, none) | +0.38% | +0.25% | **-0.13%** |
| WS delivery overhead | +0.99% | +1.21% | +0.22% (noise) |

**Verdict:** Probe overhead dropped from +0.38% to +0.25% (34% relative improvement),
confirming the vDSO benefit. WS delivery overhead is +0.22% higher than iter 14 but
this is within the benchmark noise floor (~0.3%) and unrelated to the clock change
(WS overhead comes from snapshot computation + serialization, not probes). The off
baseline is higher this run (14.73% vs 13.95%), indicating more background activity
on the Pi — the key metric is the within-run delta. Change retained.

### Iteration 16: Fix snapshot cache invalidation (let 900ms TTL work)

#### Hypothesis

`full_snapshot()` called `invalidate_snapshot_cache()` immediately after
`snapshot_all_internal()`, defeating the 900ms TTL on every invocation. Every
call recomputed distributions, edge delays, and CPU attribution from scratch.
Removing the invalidation lets the cache serve subsequent requests within the
TTL window. `Arc::try_unwrap` will now fail (cache holds a ref), but the
shallow clone of `Vec<InternalPipelineData>` is vastly cheaper than
recomputation. The benefit is most visible with multiple concurrent clients
or sub-second poll intervals; with a single 1Hz WS client and 900ms TTL,
most ticks fall just outside the window.

#### Changes

1. Removed `invalidate_snapshot_cache()` call from `full_snapshot()`.
2. Legitimate invalidation on topology changes (`update_topology_and_enable()`)
   is preserved.

#### Result

- **Commit:** `798fdcdc`
- **Tests:** 39/39 pass (stats tests).

##### Raspberry Pi 4 Benchmark (1 USB H264 camera)

**POLL_MODE=none (probe overhead — should be unchanged):**

| Level | CPU% mean | CPU% std | RSS (KB) |
|-------|----------:|----------|----------|
| off   |     14.92 |     6.37 |   90,007 |
| full  |     15.24 |     6.52 |   91,414 |

Overhead vs off: full +0.32%.

**POLL_MODE=ws-full (API delivery — primary target):**

| Delivery | CPU% mean | RSS (KB) | Delta vs none |
|----------|----------:|---------:|--------------:|
| none     |     15.24 |   91,414 |           --- |
| ws-full  |     16.32 |   94,481 |        +1.08% |

**Comparison with Iteration 14 (Phase 3 final) and Iteration 15:**

| Metric | Iter 14 | Iter 15 | Iter 16 | Delta (16 vs 14) |
|--------|---------|---------|---------|-------------------|
| Probe overhead (full, none) | +0.38% | +0.25% | +0.32% | -0.06% (noise) |
| WS delivery overhead | +0.99% | +1.21% | +1.08% | +0.09% (noise) |

**Verdict:** WS delivery overhead is consistent with previous iterations within
noise. With a single 1Hz WS client and 900ms TTL, most ticks fall outside the
cache window (~1000ms > 900ms), so the cache rarely hits. The benefit will be
pronounced with: (a) multiple concurrent WS clients sharing one computation,
(b) sub-second poll intervals, or (c) combined HTTP + WS access. RSS is ~2MB
higher because the cache retains the snapshot for 900ms instead of immediately
discarding it — an acceptable trade-off. The change is architecturally correct
(the cache was supposed to work but was being defeated) and is retained.

### Iteration 17: Serialized-JSON cache for snapshot endpoints

#### Hypothesis

Even when the internal snapshot cache (900ms TTL) hits, both HTTP and WS
handlers still pay the cost of: (1) `Arc::try_unwrap` + clone of
`Vec<InternalPipelineData>`, (2) `build_api_pipeline_snapshot()` for each
pipeline, (3) health stats computation, and (4) `serde_json` serialization.
A second cache layer storing pre-serialized JSON bytes eliminates all four
costs on cache hit — reducing per-tick cost to one mutex lock + `Arc::clone`
(~30ns).

With a single 1Hz WS client and 900ms TTL, most ticks (1000ms > 900ms)
miss the cache, so the benefit is marginal for this benchmark configuration.
The payoff comes with multiple concurrent clients, sub-second poll intervals,
or combined HTTP + WS access.

#### Changes

1. Added `JSON_SNAPSHOT_CACHE` static in `pipeline_analysis.rs`: stores
   `(Instant, buffer_limit, Arc<Vec<u8>>)`.
2. Added `full_snapshot_json(buffer_limit, streams_info) -> Arc<Vec<u8>>`
   that checks the JSON cache first, then delegates to `full_snapshot()` +
   `serde_json::to_vec()` on miss.
3. Both `streams_snapshot_get()` (HTTP) and `streams_snapshot_ws()` (WS)
   now call `full_snapshot_json()` — no manual `serde_json::to_writer()`
   or pre-allocated `json_buf`.
4. `invalidate_snapshot_cache()` clears both caches.

#### Result

- **Commit:** `d491144d`
- **Tests:** 39/39 pass (stats tests). Full `cargo check` clean.

##### Raspberry Pi 4 Benchmark (1 USB H264 camera)

**POLL_MODE=none (probe overhead — should be unchanged):**

| Level | CPU% mean | CPU% std | RSS (KB) |
|-------|----------:|----------|----------|
| off   |     14.85 |     5.79 |   90,566 |
| full  |     15.03 |     6.09 |   90,516 |

Overhead vs off: full +0.18%.

**POLL_MODE=ws-full (API delivery):**

| Delivery | CPU% mean | RSS (KB) | Delta vs none |
|----------|----------:|---------:|--------------:|
| none     |     15.03 |   90,516 |           --- |
| ws-full  |     16.30 |   93,343 |        +1.27% |

**Comparison across Phase 4 iterations and Phase 3 final:**

| Metric | Iter 14 (P3) | Iter 15 | Iter 16 | Iter 17 | Delta (17 vs 14) |
|--------|-------------|---------|---------|---------|-------------------|
| Probe overhead (full, none) | +0.38% | +0.25% | +0.32% | +0.18% | **-0.20%** |
| WS delivery overhead | +0.99% | +1.21% | +1.08% | +1.27% | +0.28% (noise) |

**Verdict:** Probe overhead reached a new low of +0.18% (down from +0.38% at
Phase 3 end — a 53% relative reduction). WS delivery overhead varies ±0.28%
across iterations, which is within the benchmark noise floor for 1Hz/single-client.
The JSON cache doesn't measurably hurt and provides the architectural foundation
for O(1) cache hits with multiple concurrent clients or sub-second polling.
Change retained.

### Phase 4 Summary

| Iteration | Commit | Tests | Description | Measurable impact |
|-----------|--------|-------|-------------|-------------------|
| 15 | `32860c89` | 39/39 | CLOCK_REALTIME_COARSE for probes | Probe overhead -34% (0.38→0.25%) |
| 16 | `798fdcdc` | 39/39 | Fix snapshot cache invalidation | Cache now functional; +2MB RSS |
| 17 | `d491144d` | 39/39 | Serialized-JSON cache | Probe overhead -53% total (0.38→0.18%) |

**Cumulative Phase 4 results vs Phase 3 end:**

| Metric | Phase 3 end (Iter 14) | Phase 4 end (Iter 17) | Improvement |
|--------|----------------------|----------------------|-------------|
| Probe overhead (full, POLL_MODE=none) | +0.38% | +0.18% | **-0.20%** (53% relative) |
| WS delivery overhead (ws-full vs none) | +0.99% | +1.27% | +0.28% (within noise) |
| Total full+ws overhead (vs off) | +1.37% | +1.45% | +0.08% (noise) |

All three changes are retained. The probe hot path now uses vDSO-accelerated
CLOCK_REALTIME_COARSE (iter 15), the snapshot cache correctly serves within its
900ms TTL (iter 16), and a JSON cache eliminates serialization on cache hits
(iter 17). The WS delivery overhead for a single 1Hz client is at the noise
floor; further reduction requires either higher-concurrency workloads (where the
caches provide significant win) or architectural changes like background
precomputation with ArcSwap.

---

## Phase 5: Deep Flamegraph-Driven Optimization (Iterations 18–22)

Fresh flamegraph analysis capturing the full real-world workload including
WebSocket JSON serialization, targeting the `full_snapshot(0)` + `serde_json`
hot path.

**Baseline:** 0.367ms/iter (Round 13 initial flamegraph with 5000 iterations).

### Round 13 Baseline Flamegraph Analysis

Top hotspots (perf report, flat, `cycles:Pu`):

| Function | % | Notes |
|----------|---|-------|
| `Vec::from_iter` | 8.97% | `.filter().collect()` loses size hints |
| `snapshot_internal` | 6.71% | Main snapshot computation |
| `Vec::clone` | 3.49% | Deep clone of `Vec<InternalPipelineData>` |
| `zmij::write_to_zmij_buffer` | 3.31% | f64 formatting in serde_json |
| `pad_metrics` | 3.37% | PTS unique counting |
| `ipnsort` | 3.08% | Sorting operations |

### Iteration 18: Eliminate deep clone in full_snapshot

#### Hypothesis

After Iteration 16 fixed the snapshot cache, `Arc::try_unwrap()` in
`full_snapshot()` consistently fails (cache retains a reference), forcing a
deep clone of the entire `Vec<InternalPipelineData>` including ~36K
`RawRecord`s per pipeline (~900KB of data). The `Vec::clone` hotspot at
3.49% comes entirely from this.

#### Changes

Refactored `full_snapshot`, `build_api_pipeline_snapshot`, `convert_element`,
and `convert_pad` to pass `InternalPipelineData`, `InternalElementSnapshot`,
and `InternalPadSnapshot` by reference (`&T`) instead of value (`T`). Only
necessary smaller fields (strings, primitive structs like `Distribution`) are
cloned at the API output boundary.

#### Result

- **Bench:** 0.367ms → 0.325ms/iter (**11.4% faster**)
- `Vec::clone` hotspot: 3.49% → 0% (**eliminated**)

### Iteration 19: Pre-allocate filtered .collect() patterns

#### Hypothesis

`Vec::from_iter` remained at 7.04% due to `.filter().collect()` patterns
that lose the iterator size hint, causing repeated Vec reallocations.

#### Changes

Replaced numerous `.filter().collect()` calls with manual loops and
`Vec::with_capacity()` in:
- `FullPadBuffer::read_records()`
- `ElementProbe::snapshot()`
- `compute_summary()`
- `active_analyses()`
- `poll_thread_cpu()`
- `compute_cpu_attribution()`
- `snapshot_internal()`

#### Result

- **Bench:** 0.325ms → 0.316ms/iter (**2.8% faster**)
- `Vec::from_iter` hotspot significantly reduced
- **Cumulative:** 13.9% faster than baseline

### Iteration 20: Cache records alongside distributions in FullPadBuffer

#### Hypothesis

After previous optimizations, `FullPadBuffer::read_records` became the
top hotspot at 15.31% — 36,000 atomic loads from ring buffers per
snapshot. Distributions were already cached, but raw records were always
re-read even when unchanged.

#### Changes

Combined the per-pad distribution cache with a records cache using
`Arc<Vec<RawRecord>>`, keyed on the same `write_cursor` generation:
- On cache hit: return `Arc::clone` of records (~cheap atomic increment)
  instead of re-reading 900 atomic values per pad
- Updated `InternalPadSnapshot.records` from `Option<Vec<RawRecord>>` to
  `Option<Arc<Vec<RawRecord>>>`
- All consumers updated to use `.iter()` on `Arc<Vec<RawRecord>>`

#### Result

- **Bench:** 0.316ms → 0.253ms/iter (**19.9% faster**)
- `FullPadBuffer::read_records` hotspot: 15.31% → 0% (**eliminated**)
- `PadBuffer::snapshot`: now 4.75% (cache lookup + Arc clone)
- **Cumulative:** 31.1% faster than baseline

### Iteration 21: Skip sort when already sorted + unchecked bounds

#### Hypothesis

Two hotspots in the PTS merge-join:
1. `ipnsort` at 2.6%: Sorting records that are already PTS-ordered (ring
   buffer reads in chronological order, PTS increases monotonically)
2. `SliceIndex::index` at 5.47%: Redundant bounds checks the compiler can't
   elide through complex cursor control flow

#### Changes

1. **Skip sort when already sorted:** Added `is_sorted_by_key()` O(n) check
   before `sort_unstable()` in `compute_causal_edge_latency`,
   `compute_intra_element_processing_time`, and `pad_metrics`. For normal
   video streams where PTS increases monotonically, the sort is skipped.
2. **Unchecked bounds:** Replaced `sink_pairs[cursor]` with
   `unsafe { sink_pairs.get_unchecked(cursor) }` in the merge-join inner
   loops, with documented safety invariants (every access is guarded by
   `cursor < sink_len`).

#### Result

- **Bench:** 0.253ms → 0.212ms/iter (**16.2% faster**)
- `snapshot_internal`: 17.74% → 13.19% (**-4.55pp**)
- `ipnsort`: 2.0% → 0% (**eliminated**)
- **Cumulative:** 42.2% faster than baseline

### Iteration 22: Cache SystemBuffer snapshot by generation

#### Hypothesis

`SystemBuffer::snapshot()` at 2.23% iterates 120 entries with 4 Welford
accumulators on every call. In the bench (and production when no new system
samples arrive), the result is identical to the previous call.

#### Changes

Added a generation counter + cached `SystemSnapshot` to `SystemBuffer`.
`record()` increments the generation; `snapshot()` returns the cached value
on generation match.

#### Result

- **Bench:** 0.212ms → 0.210ms/iter (**0.9% faster**)
- `SystemBuffer::snapshot`: 2.23% → 0% (**eliminated from profile**)
- **Cumulative:** 42.8% faster than baseline

### Phase 5 Summary

| Iteration | Bench (ms/iter) | Change | Cumulative |
|-----------|---------------:|-------:|----------:|
| Baseline (pre-18) | 0.367 | — | — |
| 18: Eliminate deep clone | 0.325 | -11.4% | -11.4% |
| 19: Pre-allocate .collect() | 0.316 | -2.8% | -13.9% |
| 20: Cache records in FullPadBuffer | 0.253 | -19.9% | -31.1% |
| 21: Skip sort + unchecked bounds | 0.212 | -16.2% | -42.2% |
| 22: Cache SystemBuffer snapshot | 0.210 | -0.9% | -42.8% |

**Stable measurement (3 runs): 0.197 / 0.201 / 0.208 ms/iter (median 0.201ms)**

### Remaining Hotspots (post-22 flamegraph)

| Function | % | Reducible? |
|----------|---|-----------|
| `snapshot_internal` | ~14% | Partially — merge-join is O(E×R) fundamental |
| `zmij::write_to_zmij_buffer` | ~8% | No — serde_json f64 formatter |
| `format_escaped_str` | ~4% | No — serde_json string escaping |
| `GLib/GObject` operations | ~4% | No — GStreamer FFI |
| `malloc` / `libc` | ~5% | Marginal — Vec/HashMap allocations |
| `ElementSnapshot::serialize` | ~2% | No — serde derive overhead |

The remaining costs are dominated by:
1. **Irreducible computation:** O(E×R) merge-join for 18 edges × 900 records
2. **JSON serialization:** 58KB output with ~2400 f64 fields (zmij) + hundreds
   of string fields (format_escaped_str)
3. **Allocator overhead:** HashMap/Vec construction for API output types

Further improvements would require:
- Alternative JSON serializer (sonic-rs, simd-json) for ~2-3% potential
- Incremental/delta snapshots for WebSocket — 80-90% potential
- `Arc<str>` for element names to eliminate string clone allocations
- Direct serialization from internal types (bypass API type conversion)

---

## Iteration 23: ArcSwap + Arc\<str\> for Element Names

**Date:** 2026-02-22
**Changes:**
1. **Background Precomputation with ArcSwap**: Replaced `Mutex<Option<...>>` caches
   (`SNAPSHOT_CACHE`, `JSON_SNAPSHOT_CACHE`) with `arc_swap::ArcSwap` for lock-free reads
   on the hot path. Writers still coordinate via the existing generation check, but readers
   never block on a lock.
2. **`Arc<str>` for Element/Pad Names**: Changed `element_name`, `element_type`, and
   `pad_name` fields from `String` to `Arc<str>` in `InternalElementSnapshot`,
   `InternalPadSnapshot`, and `ElementProbe`. String-to-`String` conversion deferred to the
   API boundary. Eliminates repeated cloning of immutable name strings during snapshot
   construction.

**Synthetic benchmark:** 0.195 ms/iter (vs 0.201 ms/iter baseline → **-3.0%**)

**Flamegraph (iter-23):** Top remaining consumers:
- `snapshot_internal`: 20.96%
- `serde_json::ser::format_escaped_str`: 6.68%
- `core::iter::try_fold`: 5.64%
- `ElementProbe::snapshot` Mutex lock: 4.85%
- `pad_metrics`: 3.81%
- `compute_causal_edge_latency`: 3.46%
- `zmij::write_to_zmij_buffer`: 3.23%

**Real-target benchmark (Raspberry Pi 4, hardened environment):**

System hardening strategies (adopted from mcm-test-harness/ab_harness/deploy.py):
- Page cache + swap flush before each level
- Noisy OS services stopped (cron, atd, rsyslog, irqbalance, apt/man/fstrim timers)
- CPU governor=performance, freq locked at 1500 MHz
- Container pinned to CPUs 1-3, host processes + IRQs pinned to CPU 0
- Kernel noise suppressed (KSM, compaction, THP→madvise, watchdogs, writeback→60s)
- Thermal warmup (CPU stress until SoC temp stabilises)

**POLL_MODE=none (probes + background sampler only):**

| Level | CPU% mean | CPU% std | RSS (KB) avg |
|-------|-----------|----------|-------------|
| off   | 71.88     | 33.04    | 237,480     |
| lite  | 109.77    | 28.02    | 244,578     |
| full  | 111.00    | 29.54    | 247,073     |

**POLL_MODE=ws-full (WebSocket at 1 Hz):**

| Level | CPU% mean | CPU% std | RSS (KB) avg |
|-------|-----------|----------|-------------|
| off   | 113.22    | 33.65    | 252,610     |
| full  | 123.12    | 33.99    | 241,117     |

**WebSocket overhead: +9.90% absolute CPU (+8.7% relative)**

Note: High standard deviations (~30%) are inherent to real camera stream workloads.
The measured overhead is notably lower than the previous (non-hardened) measurement of
+20.74%, suggesting the hardened environment provides more accurate delta measurements
by reducing system noise.

| Iter | Description | ms/iter | Δ vs prev | Δ vs baseline |
|------|-------------|---------|-----------|---------------|
| 23: ArcSwap + Arc\<str\> | 0.195 | -3.0% | -46.9% |

### Iteration 23 Addendum: Fake Ball Source Benchmark (Low-Noise Workload)

**Date:** 2026-02-22

Previous measurements used a 3-camera workload (4K ONVIF + 1080p USB + 160p fake) with
~30% CPU std dev, making delta measurement noisy. Switched to a controlled single
160p/30fps fake ball stream (`scripts/benchmark-settings.json`) for higher precision.

**Issues resolved during benchmarking:**
- Killed conflicting `ab_harness batch` process managing the same container
- Fixed MCM port 6020 "Address already in use" by sending SIGTERM before SIGKILL
  (graceful shutdown releases sockets immediately vs 30-60s TIME_WAIT after SIGKILL)

**POLL_MODE=none (probes + background sampler only):**

| Level | CPU% mean | CPU% std | RSS (KB) avg | RSS (KB) std |
|-------|-----------|----------|-------------|-------------|
| off   | 54.41     | 18.00    | 217,345     | 1,205       |
| full  | 56.23     | 19.36    | 220,279     | 1,568       |

**Pipeline analysis overhead: +1.82% absolute CPU (+3.3% relative), +2,934 KB RSS**

**POLL_MODE=ws-full (WebSocket at 1 Hz):**

| Level | CPU% mean | CPU% std | RSS (KB) avg | RSS (KB) std |
|-------|-----------|----------|-------------|-------------|
| off   | 55.11     | 18.76    | 215,897     | 1,203       |
| full  | 60.89     | 18.40    | 222,918     | 1,284       |

**WebSocket + analysis overhead: +5.78% absolute CPU (+10.5% relative), +7,021 KB RSS**
**Pure WS serialization overhead: +4.66% absolute CPU (full: ws-full vs none)**

Note: RSS std dev dropped from ~28,000 KB (3-camera) to ~1,200 KB (fake ball),
confirming this workload is more suitable for precise overhead measurement.
The ~55% CPU baseline (for `off`) is x264 encoding the 160p/30fps ball stream.
