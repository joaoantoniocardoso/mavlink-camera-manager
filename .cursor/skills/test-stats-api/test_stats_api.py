#!/usr/bin/env python3
"""
Integration tests for the MCM stats API (hierarchical /stats/streams/* model).

Requires a running MCM instance with --pipeline-analysis-level lite|full
and at least one active video stream.

API surface:
  - GET  /stats/streams/snapshot          (StreamsSnapshot)
  - GET  /stats/streams/snapshot/ws       (WebSocket streaming StreamsSnapshot)
  - POST /stats/streams/reset             (global reset)
  - GET  /stats/streams/level
  - POST /stats/streams/level
  - GET  /stats/streams/window-size
  - POST /stats/streams/window-size

Usage:
    python3 test_stats_api.py
    MCM_URL=http://192.168.2.2:6020 python3 test_stats_api.py
"""

import json
import os
import sys
import time
import traceback
import urllib.error
import urllib.request

BASE_URL = os.environ.get("MCM_URL", "http://127.0.0.1:6020").rstrip("/")
STATS = f"{BASE_URL}/stats/streams"
TIMEOUT = int(os.environ.get("MCM_TIMEOUT", "5"))

# ── Helpers ──────────────────────────────────────────────────────────────────

passed = 0
failed = 0
skipped = 0
errors: list[str] = []


def get_json(url: str, expect_status: int = 200) -> dict | list | None:
    """GET a JSON endpoint. Returns parsed body or None on expected error."""
    req = urllib.request.Request(url, method="GET")
    try:
        with urllib.request.urlopen(req, timeout=TIMEOUT) as resp:
            body = json.loads(resp.read().decode())
            assert resp.status == expect_status, (
                f"Expected {expect_status}, got {resp.status}"
            )
            return body
    except urllib.error.HTTPError as e:
        if e.code == expect_status:
            return None
        raise


def post_json(
    url: str, body: dict | None = None, expect_status: int = 200
) -> dict | None:
    """POST JSON to an endpoint. Returns parsed body or None on expected error."""
    data = json.dumps(body).encode() if body else b""
    req = urllib.request.Request(
        url,
        data=data,
        method="POST",
        headers={"Content-Type": "application/json"},
    )
    try:
        with urllib.request.urlopen(req, timeout=TIMEOUT) as resp:
            result = json.loads(resp.read().decode())
            assert resp.status == expect_status, (
                f"Expected {expect_status}, got {resp.status}"
            )
            return result
    except urllib.error.HTTPError as e:
        if e.code == expect_status:
            return None
        raise


def assert_key(obj: dict, key: str, type_: type | tuple | None = None, msg: str = ""):
    """Assert a key exists and optionally check its type."""
    assert key in obj, f"Missing key '{key}' in {list(obj.keys())} {msg}"
    if type_ is not None:
        assert isinstance(obj[key], type_), (
            f"Key '{key}' expected {type_}, "
            f"got {type(obj[key]).__name__} {msg}"
        )


def assert_distribution(d: dict, label: str = ""):
    """Validate statistical invariants on a Distribution object."""
    for field in ("count", "min", "max", "mean", "std", "median", "p95", "p99"):
        assert field in d, f"Distribution {label} missing '{field}'"

    if d["count"] == 0:
        return

    eps = 1e-9
    assert d["min"] - eps <= d["mean"], (
        f"{label}: min ({d['min']}) > mean ({d['mean']})"
    )
    assert d["mean"] <= d["max"] + eps, (
        f"{label}: mean ({d['mean']}) > max ({d['max']})"
    )
    assert d["min"] - eps <= d["median"], (
        f"{label}: min ({d['min']}) > median ({d['median']})"
    )
    assert d["median"] <= d["max"] + eps, (
        f"{label}: median ({d['median']}) > max ({d['max']})"
    )
    assert d["std"] >= -eps, f"{label}: std ({d['std']}) < 0"
    assert d["p95"] >= d["median"] - eps, (
        f"{label}: p95 ({d['p95']}) < median ({d['median']})"
    )
    assert d["p99"] >= d["p95"] - eps, (
        f"{label}: p99 ({d['p99']}) < p95 ({d['p95']})"
    )


def assert_system_distribution(d: dict, label: str = ""):
    """Validate a SystemDistribution (no percentiles)."""
    for field in ("count", "min", "max", "mean", "std"):
        assert field in d, f"SystemDistribution {label} missing '{field}'"
    if d["count"] == 0:
        return
    eps = 1e-9
    assert d["min"] - eps <= d["mean"] <= d["max"] + eps, (
        f"{label}: ordering violated: {d['min']} <= {d['mean']} <= {d['max']}"
    )
    assert d["std"] >= 0, f"{label}: std < 0"


WALL_NS_2020 = 1_577_836_800_000_000_000  # 2020-01-01
WALL_NS_2040 = 2_208_988_800_000_000_000  # 2040-01-01


def assert_wall_ns(val: int, label: str = ""):
    """Sanity-check a wall_ns timestamp (should be between 2020 and 2040)."""
    assert WALL_NS_2020 < val < WALL_NS_2040, (
        f"{label}: wall_ns {val} out of sane range"
    )


def run_test(name: str, fn):
    """Run a single test function and track pass/fail."""
    global passed, failed, skipped
    try:
        fn()
        passed += 1
        print(f"  PASS  {name}")
    except Exception as e:
        failed += 1
        msg = f"  FAIL  {name}: {e}"
        print(msg)
        errors.append(msg)
        if os.environ.get("MCM_TEST_VERBOSE"):
            traceback.print_exc()


# ── State ─────────────────────────────────────────────────────────────────────

initial_level: str | None = None


# ── Phase 1: Configuration & Control ─────────────────────────────────────────


def test_level_get():
    global initial_level
    data = get_json(f"{STATS}/level")
    assert_key(data, "level", str)
    assert data["level"] in ("lite", "full"), f"Unexpected level: {data['level']}"
    initial_level = data["level"]


def test_level_set_full():
    data = post_json(f"{STATS}/level", {"level": "full"})
    assert_key(data, "level", str)
    assert data["level"] == "full"


def test_level_set_invalid():
    """POST an invalid level should return 400."""
    post_json(f"{STATS}/level", {"level": "invalid"}, expect_status=400)


def test_window_size_get():
    data = get_json(f"{STATS}/window-size")
    assert_key(data, "window_size", int)
    assert 1 <= data["window_size"] <= 50_000


def test_window_size_set():
    data = post_json(f"{STATS}/window-size", {"window_size": 900})
    assert_key(data, "window_size", int)
    assert data["window_size"] == 900


def test_window_size_set_zero():
    """window_size=0 should return 400."""
    post_json(f"{STATS}/window-size", {"window_size": 0}, expect_status=400)


def test_window_size_set_too_large():
    """window_size > 50000 should return 400."""
    post_json(f"{STATS}/window-size", {"window_size": 99999}, expect_status=400)


def test_reset():
    data = post_json(f"{STATS}/reset")
    assert_key(data, "status", str)
    assert data["status"] == "reset"


# ── Phase 2: Snapshot ─────────────────────────────────────────────────────────


def validate_pad_snapshot(pad: dict, ctx: str):
    """Validate a PadSnapshot in the new hierarchical model."""
    assert_key(pad, "name", str, ctx)
    assert_key(pad, "direction", str, ctx)
    assert pad["direction"] in ("sink", "src"), (
        f"{ctx}: unexpected direction '{pad['direction']}'"
    )
    assert_key(pad, "stats", dict, ctx)
    ps = pad["stats"]
    assert_key(ps, "level", str, f"{ctx}.stats")
    assert ps["level"] in ("lite", "full"), f"{ctx}.stats.level = {ps['level']}"
    assert_key(ps, "total_buffers", int, f"{ctx}.stats")
    assert ps["total_buffers"] >= 0
    assert_key(ps, "total_keyframes", int, f"{ctx}.stats")
    assert_key(ps, "total_delta_frames", int, f"{ctx}.stats")
    assert_key(ps, "last_wall_ns", int, f"{ctx}.stats")

    if ps.get("distribution"):
        dist = ps["distribution"]
        for dist_name in ("interval", "i_interval", "p_interval",
                          "size", "i_size", "p_size"):
            assert dist_name in dist, f"{ctx}: missing distribution '{dist_name}'"
            assert_distribution(dist[dist_name], f"{ctx}.{dist_name}")

    assert_key(pad, "connections", list, ctx)


def validate_element_snapshot(el: dict, ctx: str):
    """Validate an ElementSnapshot (recursive — bins have is_bin=true + children)."""
    assert_key(el, "name", str, ctx)
    assert_key(el, "element_type", str, ctx)
    assert_key(el, "stats", dict, ctx)

    es = el["stats"]
    assert_key(es, "health", str, f"{ctx}.stats")
    assert es["health"] in ("good", "degraded", "bad", "unknown"), (
        f"{ctx}.stats.health = {es['health']}"
    )
    assert_key(es, "stutter_events", int, f"{ctx}.stats")
    assert es["stutter_events"] >= 0
    assert_key(es, "freeze_events", int, f"{ctx}.stats")
    assert es["freeze_events"] >= 0
    assert_key(es, "max_freeze_ms", (int, float), f"{ctx}.stats")
    assert_key(es, "stutter_ratio", (int, float), f"{ctx}.stats")

    assert_key(el, "connections", list, ctx)
    for conn in el["connections"]:
        assert_key(conn, "to_element", str, f"{ctx}.connections")
        assert_key(conn, "freshness_delay_ms", (int, float), f"{ctx}.connections")

    assert_key(el, "pads", list, ctx)
    for i, pad in enumerate(el["pads"]):
        validate_pad_snapshot(pad, f"{ctx}.pads[{i}]")

    # Bins: is_bin present and true means children contains nested elements
    if el.get("is_bin", False):
        assert_key(el, "children", list, ctx)
        for i, child in enumerate(el["children"]):
            validate_element_snapshot(child, f"{ctx}.children[{i}]")


def validate_thread_summary(thread: dict, ctx: str):
    """Validate a ThreadSummary (flat, no nested elements)."""
    assert_key(thread, "id", int, ctx)
    assert_key(thread, "stats", dict, ctx)
    ts = thread["stats"]
    assert_key(ts, "cpu_pct", (int, float), f"{ctx}.stats")
    assert_key(thread, "connections", list, ctx)
    assert "elements" not in thread, (
        f"{ctx}: ThreadSummary should not contain 'elements' field"
    )



def validate_pipeline_snapshot(pipeline: dict, ctx: str):
    """Validate a PipelineSnapshot in the new hierarchical model."""
    assert_key(pipeline, "name", str, ctx)
    assert_key(pipeline, "stats", dict, ctx)

    ps = pipeline["stats"]
    assert_key(ps, "level", str, f"{ctx}.stats")
    assert ps["level"] in ("lite", "full"), f"{ctx}.stats.level = {ps['level']}"
    assert_key(ps, "window_size", int, f"{ctx}.stats")
    assert ps["window_size"] >= 1
    assert_key(ps, "expected_interval_ms", (int, float), f"{ctx}.stats")
    assert_key(ps, "uptime_secs", (int, float), f"{ctx}.stats")

    # Summary
    assert_key(ps, "summary", dict, f"{ctx}.stats")
    summary = ps["summary"]
    assert_key(summary, "total_frames", int, f"{ctx}.stats.summary")
    assert_key(summary, "throughput_fps", (int, float), f"{ctx}.stats.summary")
    assert_key(summary, "total_pipeline_freshness_delay_ms", (int, float), f"{ctx}.stats.summary")
    assert_key(summary, "verdict", str, f"{ctx}.stats.summary")

    # System
    assert_key(ps, "system", dict, f"{ctx}.stats")
    sys_snap = ps["system"]
    assert_key(sys_snap, "sample_count", int, f"{ctx}.stats.system")
    assert_key(sys_snap, "current_cpu_pct", (int, float), f"{ctx}.stats.system")
    for dist_key in ("cpu_stats", "load_stats", "mem_stats", "temp_stats"):
        if dist_key in sys_snap:
            assert_system_distribution(sys_snap[dist_key], f"{ctx}.stats.system.{dist_key}")

    # Restarts
    assert_key(ps, "restarts", dict, f"{ctx}.stats")

    # Root cause candidates
    assert_key(ps, "root_cause_candidates", list, f"{ctx}.stats")
    for c in ps["root_cause_candidates"]:
        assert_key(c, "cause", str)
        assert c["cause"] in (
            "cpu_saturation", "freeze_risk", "latency_spike",
            "causal_match_low", "unknown",
        )
        assert_key(c, "score", (int, float))

    # Thread bottlenecks
    assert_key(ps, "thread_bottlenecks", list, f"{ctx}.stats")

    # Connections
    assert_key(pipeline, "connections", list, ctx)
    for conn in pipeline["connections"]:
        assert_key(conn, "to_pipeline", str, f"{ctx}.connections")
        assert_key(conn, "bridge_type", str, f"{ctx}.connections")

    # Elements (recursive — bins have is_bin=true + children)
    assert_key(pipeline, "elements", list, ctx)
    for i, el in enumerate(pipeline["elements"]):
        validate_element_snapshot(el, f"{ctx}.elements[{i}]")

    # Thread summaries (flat, no nested elements)
    assert_key(pipeline, "threads", list, ctx)
    for i, thread in enumerate(pipeline["threads"]):
        validate_thread_summary(thread, f"{ctx}.threads[{i}]")


def validate_stream_snapshot(stream: dict, ctx: str):
    """Validate a StreamSnapshot in the new hierarchical model."""
    assert_key(stream, "id", str, ctx)
    assert_key(stream, "stats", dict, ctx)

    ss = stream["stats"]
    assert_key(ss, "health", str, f"{ctx}.stats")
    assert ss["health"] in ("good", "degraded", "bad", "unknown"), (
        f"{ctx}.stats.health = {ss['health']}"
    )
    assert_key(ss, "dominant_issue", str, f"{ctx}.stats")
    assert_key(ss, "throughput_fps", (int, float), f"{ctx}.stats")
    assert_key(ss, "cpu_pct", (int, float), f"{ctx}.stats")
    assert_key(ss, "freshness_delay_ms", (int, float), f"{ctx}.stats")
    assert_key(ss, "root_cause_candidates", list, f"{ctx}.stats")

    assert_key(stream, "pipelines", list, ctx)
    for i, pipeline in enumerate(stream["pipelines"]):
        validate_pipeline_snapshot(pipeline, f"{ctx}.pipelines[{i}]")


def validate_streams_snapshot(data: dict, ctx: str = "snapshot"):
    """Validate the full StreamsSnapshot response."""
    assert_key(data, "timestamp_ns", int, ctx)
    assert_wall_ns(data["timestamp_ns"], f"{ctx}.timestamp_ns")

    assert_key(data, "stats", dict, ctx)
    fleet = data["stats"]
    assert_key(fleet, "overall_health", str, f"{ctx}.stats")
    assert fleet["overall_health"] in ("good", "degraded", "bad", "unknown")
    assert_key(fleet, "streams_total", int, f"{ctx}.stats")
    assert_key(fleet, "streams_degraded", int, f"{ctx}.stats")
    assert_key(fleet, "streams_bad", int, f"{ctx}.stats")
    assert_key(fleet, "dominant_issue", str, f"{ctx}.stats")

    assert_key(data, "streams", list, ctx)
    for i, stream in enumerate(data["streams"]):
        validate_stream_snapshot(stream, f"{ctx}.streams[{i}]")


def test_snapshot():
    """Test the consolidated snapshot endpoint with the new hierarchical model."""
    data = get_json(f"{STATS}/snapshot?buffer_limit=3")
    validate_streams_snapshot(data)

    # Verify we have at least one stream with at least one pipeline
    assert len(data["streams"]) > 0, "Expected at least one stream"
    has_pipeline = any(len(s["pipelines"]) > 0 for s in data["streams"])
    assert has_pipeline, "Expected at least one pipeline in some stream"


def _collect_all_elements(pipeline: dict) -> list:
    """Recursively collect all elements from a pipeline (including bin children)."""
    result = []
    def _visit(elements):
        for el in elements:
            result.append(el)
            _visit(el.get("children", []))
    _visit(pipeline.get("elements", []))
    return result


def test_snapshot_no_buffer():
    """Snapshot with buffer_limit=0 should omit raw records."""
    data = get_json(f"{STATS}/snapshot?buffer_limit=0")
    validate_streams_snapshot(data)

    for stream in data["streams"]:
        for pipeline in stream["pipelines"]:
            for el in _collect_all_elements(pipeline):
                for pad in el["pads"]:
                    buf = pad.get("buffer", [])
                    assert len(buf) == 0, (
                        f"Expected empty buffer with buffer_limit=0, got {len(buf)} records"
                    )


def test_snapshot_buffer_limit_clamped():
    """buffer_limit > 300 is clamped to 300 (returns 200, not 400)."""
    data = get_json(f"{STATS}/snapshot?buffer_limit=999")
    assert_key(data, "timestamp_ns", int)
    assert_key(data, "stats", dict)
    assert_key(data, "streams", list)


def test_websocket_snapshot():
    """Test the snapshot WebSocket endpoint receives a valid StreamsSnapshot."""
    try:
        import asyncio
        import websockets  # type: ignore[import-untyped]
    except ImportError:
        print("    (websockets not installed, skipping WS tests)")
        return

    ws_url = (
        STATS.replace("http://", "ws://").replace("https://", "wss://")
        + "/snapshot/ws?interval_ms=500&buffer_limit=2"
    )

    async def _test():
        async with websockets.connect(ws_url) as ws:
            msg = await asyncio.wait_for(ws.recv(), timeout=5)
            data = json.loads(msg)
            assert isinstance(data, dict), f"Expected dict, got {type(data).__name__}"
            validate_streams_snapshot(data, "ws_snapshot")

    asyncio.run(_test())


# ── Phase 3: Restore original state ──────────────────────────────────────────


def test_restore_level():
    """Restore the stats level to whatever it was before the test run."""
    if initial_level:
        post_json(f"{STATS}/level", {"level": initial_level})


# ── Runner ────────────────────────────────────────────────────────────────────

TESTS = [
    # Phase 1: Configuration & Control
    ("level_get", test_level_get),
    ("level_set_full", test_level_set_full),
    ("level_set_invalid (negative)", test_level_set_invalid),
    ("window_size_get", test_window_size_get),
    ("window_size_set", test_window_size_set),
    ("window_size_set_zero (negative)", test_window_size_set_zero),
    ("window_size_set_too_large (negative)", test_window_size_set_too_large),
    ("reset", test_reset),
    # Let data accumulate after reset
    ("(wait 3s for data)", lambda: time.sleep(3)),
    # Phase 2: Snapshot
    ("snapshot", test_snapshot),
    ("snapshot_no_buffer", test_snapshot_no_buffer),
    ("snapshot_buffer_limit_clamped", test_snapshot_buffer_limit_clamped),
    ("websocket_snapshot", test_websocket_snapshot),
    # Phase 3: Restore
    ("restore_level", test_restore_level),
]


def main():
    print(f"MCM Stats API Integration Tests")
    print(f"Target: {BASE_URL}")
    print(f"Timeout: {TIMEOUT}s")
    print()

    # Connectivity check
    try:
        urllib.request.urlopen(
            f"{BASE_URL}/stats/streams/level", timeout=TIMEOUT
        )
    except Exception as e:
        print(f"ERROR: Cannot connect to MCM at {BASE_URL}: {e}")
        print("Is MCM running with --pipeline-analysis-level lite|full?")
        sys.exit(1)

    print(f"Running {len(TESTS)} tests...\n")

    for name, fn in TESTS:
        run_test(name, fn)

    print(f"\n{'='*60}")
    print(f"Results: {passed} passed, {failed} failed")
    if errors:
        print(f"\nFailures:")
        for e in errors:
            print(f"  {e}")
    print()

    sys.exit(1 if failed > 0 else 0)


if __name__ == "__main__":
    main()
