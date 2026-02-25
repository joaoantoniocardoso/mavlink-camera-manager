---
name: test-stats-api
description: Test the MCM stats API endpoints against a running instance. Use when verifying stats API changes, validating endpoint responses, checking schema correctness, or running integration tests for the /stats/streams HTTP and WebSocket API.
---

# Test MCM Stats API

Integration test workflow for the `/stats/streams` API surface.
Requires a running MCM instance with `--pipeline-analysis-level lite` or `full`
and at least one active video stream.

## API Surface

The stats API has two groups of endpoints:

**Snapshot** (read data):
- `GET /stats/streams/snapshot` — consolidated hierarchical snapshot (StreamsSnapshot)
- `WS /stats/streams/snapshot/ws` — streaming snapshot at configurable interval

**Control** (configure behavior):
- `POST /stats/streams/reset` — reset all pipeline statistics (global)
- `GET /stats/streams/level` — get current stats level
- `POST /stats/streams/level` — set stats level (lite/full)
- `GET /stats/streams/window-size` — get current window size
- `POST /stats/streams/window-size` — set window size

## Prerequisites

- A running MCM instance (default: `http://127.0.0.1:6020`)
- At least one active pipeline (configure a stream before testing)
- `python3` available
- For WebSocket tests: `python3 -m pip install websockets`

## Quick start

Run the full validation suite:

```bash
python3 .cursor/skills/test-stats-api/test_stats_api.py
```

Override base URL or timeout:

```bash
MCM_URL=http://192.168.2.2:6020 python3 .cursor/skills/test-stats-api/test_stats_api.py
```

## Test structure

The script tests all endpoints in three phases:

### Phase 1: Configuration and control endpoints

| Endpoint | Method | Validates |
|----------|--------|-----------|
| `/stats/streams/level` | GET | Returns `{"level": "lite"\|"full"}` |
| `/stats/streams/level` | POST | Accepts `{"level": "full"}`, returns new level |
| `/stats/streams/window-size` | GET | Returns `{"window_size": N}` where 1 <= N <= 50000 |
| `/stats/streams/window-size` | POST | Accepts `{"window_size": 900}`, validates bounds |
| `/stats/streams/reset` | POST | Returns `{"status": "reset"}` |

### Phase 2: Snapshot endpoint

| Endpoint | Method | Validates |
|----------|--------|-----------|
| `/stats/streams/snapshot` | GET | `StreamsSnapshot` with hierarchical streams/pipelines/elements(recursive)/pads + flat thread summaries |
| `/stats/streams/snapshot/ws` | WS | Streaming `StreamsSnapshot` |

## Schema validation rules

**StreamsSnapshot** (top-level response):
- `timestamp_ns`: integer (nanoseconds since epoch)
- `stats`: `FleetStats` object (`overall_health`, `streams_total`, `streams_degraded`, `streams_bad`, `dominant_issue`)
- `streams`: list of `StreamSnapshot` objects

**StreamSnapshot**:
- `id`: non-empty string (stream UUID)
- `stats`: `StreamStats` object (`health`, `dominant_issue`, `throughput_fps`, `cpu_pct`, `freshness_delay_ms`, `root_cause_candidates[]`)
- `pipelines`: list of `PipelineSnapshot` objects

**PipelineSnapshot**:
- `name`: non-empty string
- `stats`: `PipelineStats` object (`level`, `window_size`, `expected_interval_ms`, `uptime_secs`, `summary`, `system`, `restarts`, `root_cause_candidates[]`, `thread_bottlenecks[]`)
- `connections`: list of `PipelineConnection` objects
- `elements`: list of `ElementSnapshot` objects (recursive — bins have `is_bin: true` + `children`)
- `threads`: list of `ThreadSummary` objects (flat, no nested elements)

**ThreadSummary**:
- `id`: integer (Linux TID)
- `stats`: `ThreadStats` object (`cpu_pct`, optional `name`, optional `cpu_stats`)
- `connections`: list of `ThreadConnection` objects

**ElementSnapshot** (recursive — bins and elements share this type):
- `name`: non-empty string
- `element_type`: non-empty string
- `is_bin`: boolean (omitted from JSON when `false`; `true` for bins)
- `children`: list of `ElementSnapshot` objects (omitted from JSON when empty; populated when `is_bin`)
- `thread_id`: optional integer (Linux TID, links to `ThreadSummary.id`)
- `stats`: `ElementStats` object (`health`, `stutter_events`, `freeze_events`, `max_freeze_ms`, `stutter_ratio`, optional `processing_time_us`, optional `cpu_pct`)
- `connections`: list of `ElementConnection` objects (`to_element`, `freshness_delay_ms`, optional causal fields)
- `pads`: list of `PadSnapshot` objects

**PadSnapshot**:
- `name`: string (pad name)
- `direction`: `"sink"` or `"src"`
- `stats`: `PadStats` object (`level`, `total_buffers`, `total_keyframes`, `total_delta_frames`, `last_wall_ns`, optional `accumulators`, optional `distribution`)
- `buffer`: list of `RawRecord` (when `buffer_limit > 0`; omitted when empty)
- `connections`: list of `PadConnection` objects

**Distribution** (statistical invariants):
- `min <= median <= max`, `min <= mean <= max`
- `std >= 0`, `p95 >= median`, `p99 >= p95`, `count >= 0`

## Negative and boundary tests

- `POST /level` with `{"level": "invalid"}` -> 400
- `POST /window-size` with `{"window_size": 0}` -> 400
- `POST /window-size` with `{"window_size": 99999}` -> 400
- `GET /snapshot?buffer_limit=999` -> 200 (clamped to 300)

## Manual curl examples

```bash
BASE=http://127.0.0.1:6020

# Check analysis level
curl -s "$BASE/stats/streams/level" | python3 -m json.tool

# Snapshot (no raw records)
curl -s "$BASE/stats/streams/snapshot" | python3 -m json.tool

# Snapshot with raw records
curl -s "$BASE/stats/streams/snapshot?buffer_limit=5" | python3 -m json.tool

# Reset all pipeline stats
curl -s -X POST "$BASE/stats/streams/reset"

# Switch to full mode
curl -s -X POST "$BASE/stats/streams/level" \
  -H 'Content-Type: application/json' -d '{"level":"full"}'
```

## Adding new endpoint tests

When adding a new stats endpoint:

1. Add the route and handler (see `src/lib/server/manager.rs` and `pages.rs`)
2. Add a test function in `test_stats_api.py` following the pattern:
   - `test_<endpoint_name>()` function
   - Call `get_json()` or `post_json()` helper
   - Validate HTTP status code and response schema
   - Add to the `TESTS` list at the bottom
3. Add the endpoint to the table in this SKILL.md
4. Run the full suite to confirm no regressions
