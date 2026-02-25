#!/usr/bin/env python3
"""
Cross-validate MCM pipeline stats against GStreamer coretracers / GstShark.

Runs alongside a live MCM instance that was started with GST_TRACERS enabled.
Collects MCM stats via the HTTP API and parses the GStreamer tracer log file
in parallel, then compares overlapping metrics per time window.

Supported tracer comparisons:
  - Core:     latency (pipeline), element-latency (per-element)
  - GstShark: framerate, bitrate, proctime, scheduletime

Usage:
    # Start MCM with tracers (redirect stdout to a log file):
    RUST_LOG="info,gstreamer=trace" \\
    GST_TRACERS="latency(flags=pipeline+element)" \\
    GST_DEBUG="GST_TRACER:7" \\
    mavlink-camera-manager --pipeline-analysis-level full \\
      > /tmp/mcm_tracer.log 2>&1

    # Run validation:
    GST_TRACE_FILE=/tmp/mcm_tracer.log python3 validate_stats.py

Note: MCM redirects GStreamer debug output through Rust's tracing crate, so
GST_DEBUG_FILE does not work. Instead, set RUST_LOG=info,gstreamer=trace and
redirect MCM's stdout/stderr to a file.

Environment variables:
    MCM_URL          MCM base URL (default: http://127.0.0.1:6020)
    GST_TRACE_FILE   Path to the MCM log file containing tracer output
    MCM_TIMEOUT      HTTP request timeout in seconds (default: 5)
"""

from __future__ import annotations

import argparse
import json
import math
import os
import re
import sys
import threading
import time
import urllib.error
import urllib.request
from dataclasses import dataclass, field
from pathlib import Path

# ── Configuration ────────────────────────────────────────────────────────────

BASE_URL = os.environ.get("MCM_URL", "http://127.0.0.1:6020").rstrip("/")
STATS_URL = f"{BASE_URL}/stats/streams"
TRACE_FILE = os.environ.get("GST_TRACE_FILE", "/tmp/mcm_tracer.log")
TIMEOUT = int(os.environ.get("MCM_TIMEOUT", "5"))

# ── Tracer record types ─────────────────────────────────────────────────────


@dataclass
class TracerRecord:
    """Base for all parsed tracer records."""

    wall_time_s: float  # wall-clock seconds (from GST_DEBUG timestamp)


@dataclass
class LatencyRecord(TracerRecord):
    """Core `latency` tracer: pipeline src-to-sink latency via event injection."""

    src_element: str
    sink_element: str
    latency_ns: int


@dataclass
class ElementLatencyRecord(TracerRecord):
    """Core `element-latency` tracer: per-element processing latency."""

    element: str
    latency_ns: int


@dataclass
class FramerateRecord(TracerRecord):
    """GstShark `framerate` tracer: fps per src pad (1s window)."""

    element: str
    pad: str
    fps: int


@dataclass
class BitrateRecord(TracerRecord):
    """GstShark `bitrate` tracer: bits/sec per src pad (1s window)."""

    element: str
    pad: str
    bitrate: int


@dataclass
class ProctimeRecord(TracerRecord):
    """GstShark `proctime` tracer: per-element processing time."""

    element: str
    time_ns: int


@dataclass
class ScheduletimeRecord(TracerRecord):
    """GstShark `scheduletime` tracer: inter-buffer interval on sink pads."""

    element: str
    pad: str
    time_ns: int


# ── MCM snapshot types (lightweight wrappers for comparison) ─────────────────


@dataclass
class McmPadMetrics:
    """Extracted pad-level metrics from an MCM snapshot."""

    element_name: str
    pad_name: str
    direction: str  # "sink" or "src"
    total_buffers: int
    mean_interval_ms: float | None
    bitrate_bps: float | None


@dataclass
class McmElementMetrics:
    """Extracted element-level metrics from an MCM snapshot."""

    name: str
    element_type: str
    processing_time_us: float | None
    connections: list[dict]


@dataclass
class McmPipelineMetrics:
    """Extracted pipeline-level metrics from an MCM snapshot."""

    name: str
    throughput_fps: float
    causal_latency_ms: dict | None  # Distribution dict or None
    freshness_delay_ms: float
    elements: list[McmElementMetrics]
    pads: list[McmPadMetrics]


@dataclass
class McmSnapshot:
    """One timestamped snapshot from the MCM API."""

    timestamp_ns: int
    wall_time_s: float  # local monotonic time when fetched
    pipelines: list[McmPipelineMetrics]


# ── Pad name mapping ────────────────────────────────────────────────────────


def tracer_pad_key(element_name: str, pad_name: str) -> str:
    """Build the tracer-style pad identifier: elementname_padname."""
    return f"{element_name}_{pad_name}"


def split_tracer_pad(tracer_name: str) -> tuple[str, str]:
    """Split a tracer pad name like 'videotestsrc0_src' into (element, pad).

    Handles element names that themselves contain underscores (e.g.
    'my_element0_src') by splitting on the last underscore.
    """
    idx = tracer_name.rfind("_")
    if idx < 0:
        return tracer_name, ""
    return tracer_name[:idx], tracer_name[idx + 1 :]


# ── GstStructure log parser ─────────────────────────────────────────────────

_ANSI_RE = re.compile(r"\x1b\[[0-9;]*m")

# Format 1: Raw GST_DEBUG line
# "H:MM:SS.NNNNNNNNN  PID  0xTHREAD  TRACE  GST_TRACER :0:: <structure>"
_LINE_RE_RAW = re.compile(
    r"(\d+):(\d+):(\d+)\.(\d+)\s+"  # timestamp H:MM:SS.NNNNNNNNN
    r"\d+\s+"  # PID
    r"0x[0-9a-fA-F]+\s+"  # thread pointer
    r"TRACE\s+"
    r"GST_TRACER\s+.*?:0::\s*"  # category
    r"(.+)$"  # GstStructure payload
)

# Format 2: MCM tracing redirect
# "2026-02-20T02:42:24.918900Z  TRACE  src/.../manager.rs:243: GST_TRACER :0: <structure>"
_LINE_RE_TRACING = re.compile(
    r"(\d{4})-(\d{2})-(\d{2})T(\d{2}):(\d{2}):(\d{2})\.(\d+)Z\s+"  # ISO 8601 timestamp
    r"TRACE\s+"
    r"\S+\s+"  # file:line
    r"GST_TRACER\s+:0:\s*"  # category (single colon after 0)
    r"(.+)$"  # GstStructure payload
)

# GstStructure field: key=(type)value or key=value
_FIELD_RE = re.compile(r"(\w[\w-]*)=(?:\((\w+)\))?([^,;]+)")


def _parse_gst_debug_timestamp(h: str, m: str, s: str, ns: str) -> float:
    """Convert GST_DEBUG H:MM:SS.NNNNNNNNN to seconds-since-midnight."""
    return int(h) * 3600 + int(m) * 60 + int(s) + int(ns) / 1e9


def _parse_iso_timestamp(h: str, m: str, s: str, frac: str) -> float:
    """Convert ISO 8601 time component to seconds-since-midnight."""
    ns_str = frac.ljust(9, "0")[:9]
    return int(h) * 3600 + int(m) * 60 + int(s) + int(ns_str) / 1e9


def _parse_gst_time_string(time_str: str) -> int:
    """Parse GstShark H:MM:SS.NNNNNNNNN formatted time to nanoseconds."""
    time_str = time_str.strip()
    parts = time_str.split(":")
    if len(parts) != 3:
        return 0
    h, m = int(parts[0]), int(parts[1])
    sec_parts = parts[2].split(".")
    s = int(sec_parts[0])
    ns = int(sec_parts[1]) if len(sec_parts) > 1 else 0
    return (h * 3600 + m * 60 + s) * 1_000_000_000 + ns


def _parse_fields(payload: str) -> tuple[str, dict[str, str]]:
    """Parse 'structname, k=(type)v, k2=(type)v2, ...;' into (name, fields)."""
    payload = payload.rstrip().rstrip(";")
    comma_idx = payload.find(",")
    if comma_idx < 0:
        return payload.strip(), {}
    name = payload[:comma_idx].strip()
    rest = payload[comma_idx + 1 :]
    fields: dict[str, str] = {}
    for m in _FIELD_RE.finditer(rest):
        fields[m.group(1)] = m.group(3).strip()
    return name, fields


def parse_tracer_line(line: str) -> TracerRecord | None:
    """Parse a single GST_DEBUG tracer log line into a typed record.

    Supports two formats:
      1. Raw GST_DEBUG: H:MM:SS.NNN PID 0xTHREAD TRACE GST_TRACER :0:: ...
      2. MCM tracing redirect: ISO8601 TRACE file:line: GST_TRACER :0: ...

    Returns None if the line doesn't match or the structure type is unknown.
    """
    line = _ANSI_RE.sub("", line)

    # Try MCM tracing redirect format first (more common when running via MCM)
    m = _LINE_RE_TRACING.match(line)
    if m:
        wall = _parse_iso_timestamp(m.group(4), m.group(5), m.group(6), m.group(7))
        payload = m.group(8)
    else:
        # Fall back to raw GST_DEBUG format
        m = _LINE_RE_RAW.match(line)
        if not m:
            return None
        wall = _parse_gst_debug_timestamp(m.group(1), m.group(2), m.group(3), m.group(4))
        payload = m.group(5)

    struct_name, fields = _parse_fields(payload)

    if struct_name == "latency":
        src_el = fields.get("src-element", "")
        sink_el = fields.get("sink-element", "")
        time_ns = int(fields.get("time", "0"))
        return LatencyRecord(wall, src_el, sink_el, time_ns)

    if struct_name == "element-latency":
        el = fields.get("element", "")
        time_ns = int(fields.get("time", "0"))
        return ElementLatencyRecord(wall, el, time_ns)

    if struct_name == "framerate":
        pad_full = fields.get("pad", "")
        el_name, pad_name = split_tracer_pad(pad_full)
        fps = int(fields.get("fps", "0"))
        return FramerateRecord(wall, el_name, pad_name, fps)

    if struct_name == "bitrate":
        pad_full = fields.get("pad", "")
        el_name, pad_name = split_tracer_pad(pad_full)
        br = int(fields.get("bitrate", "0"))
        return BitrateRecord(wall, el_name, pad_name, br)

    if struct_name == "proctime":
        el = fields.get("element", "")
        time_ns = _parse_gst_time_string(fields.get("time", "0:00:00.000000000"))
        return ProctimeRecord(wall, el, time_ns)

    if struct_name == "scheduletime":
        pad_full = fields.get("pad", "")
        el_name, pad_name = split_tracer_pad(pad_full)
        time_ns = _parse_gst_time_string(fields.get("time", "0:00:00.000000000"))
        return ScheduletimeRecord(wall, el_name, pad_name, time_ns)

    return None


# ── MCM API client ───────────────────────────────────────────────────────────


def get_json(url: str) -> dict | list | None:
    """GET a JSON endpoint, returning the parsed body."""
    req = urllib.request.Request(url, method="GET")
    with urllib.request.urlopen(req, timeout=TIMEOUT) as resp:
        return json.loads(resp.read().decode())


def fetch_mcm_snapshot() -> McmSnapshot | None:
    """Fetch one MCM snapshot and extract the metrics we need for comparison."""
    t0 = time.monotonic()
    try:
        data = get_json(f"{STATS_URL}/snapshot?buffer_limit=0")
    except Exception as exc:
        print(f"  [warn] MCM snapshot failed: {exc}", file=sys.stderr)
        return None

    if not data or "streams" not in data:
        return None

    pipelines: list[McmPipelineMetrics] = []

    for stream in data["streams"]:
        for pl in stream.get("pipelines", []):
            elements: list[McmElementMetrics] = []
            pads: list[McmPadMetrics] = []

            for thread in pl.get("threads", []):
                for el in thread.get("elements", []):
                    el_name = el["name"]
                    el_stats = el.get("stats", {})
                    elements.append(
                        McmElementMetrics(
                            name=el_name,
                            element_type=el.get("element_type", ""),
                            processing_time_us=el_stats.get("processing_time_us"),
                            connections=el.get("connections", []),
                        )
                    )
                    for pad in el.get("pads", []):
                        ps = pad.get("stats", {})
                        acc = ps.get("accumulators") or {}
                        dist = ps.get("distribution") or {}
                        interval_dist = dist.get("interval") or {}
                        mean_interval = (
                            acc.get("mean_interval_ms")
                            or interval_dist.get("mean")
                        )
                        pads.append(
                            McmPadMetrics(
                                element_name=el_name,
                                pad_name=pad["name"],
                                direction=pad["direction"],
                                total_buffers=ps.get("total_buffers", 0),
                                mean_interval_ms=mean_interval,
                                bitrate_bps=ps.get("bitrate_bps"),
                            )
                        )

            summary = pl.get("stats", {}).get("summary", {})
            pipelines.append(
                McmPipelineMetrics(
                    name=pl["name"],
                    throughput_fps=summary.get("throughput_fps", 0.0),
                    causal_latency_ms=summary.get("total_pipeline_causal_latency_ms"),
                    freshness_delay_ms=summary.get(
                        "total_pipeline_freshness_delay_ms", 0.0
                    ),
                    elements=elements,
                    pads=pads,
                )
            )

    return McmSnapshot(
        timestamp_ns=data.get("timestamp_ns", 0),
        wall_time_s=t0,
        pipelines=pipelines,
    )


# ── Collector threads ────────────────────────────────────────────────────────


@dataclass
class CollectionResult:
    """Holds everything collected during the sampling period."""

    mcm_snapshots: list[McmSnapshot] = field(default_factory=list)
    tracer_records: list[TracerRecord] = field(default_factory=list)
    detected_tracers: set[str] = field(default_factory=set)


def collect_mcm_snapshots(
    result: CollectionResult,
    duration_s: int,
    interval_s: float,
    stop: threading.Event,
):
    """Poll MCM API at `interval_s` for `duration_s`, appending to result."""
    deadline = time.monotonic() + duration_s
    while time.monotonic() < deadline and not stop.is_set():
        snap = fetch_mcm_snapshot()
        if snap:
            result.mcm_snapshots.append(snap)
        stop.wait(interval_s)


def collect_tracer_records(
    result: CollectionResult,
    trace_path: str,
    stop: threading.Event,
    tail_only: bool = False,
):
    """Read and tail the tracer log file, parsing records until stopped.

    When tail_only=True, seeks to the end first (for live GST_DEBUG_FILE).
    When tail_only=False (default), reads from the start to catch existing
    data (for MCM stdout redirected to a file).
    """
    path = Path(trace_path)
    if not path.exists():
        print(f"  [warn] Tracer log not found: {trace_path}", file=sys.stderr)
        return

    with open(path, "r") as f:
        if tail_only:
            f.seek(0, 2)
        while not stop.is_set():
            line = f.readline()
            if not line:
                stop.wait(0.1)
                continue
            rec = parse_tracer_line(line)
            if rec:
                result.tracer_records.append(rec)
                result.detected_tracers.add(type(rec).__name__)


def run_collection(duration_s: int, trace_path: str) -> CollectionResult:
    """Run parallel MCM + tracer collection for the given duration."""
    result = CollectionResult()
    stop = threading.Event()

    mcm_thread = threading.Thread(
        target=collect_mcm_snapshots,
        args=(result, duration_s, 1.0, stop),
        daemon=True,
    )
    tracer_thread = threading.Thread(
        target=collect_tracer_records,
        args=(result, trace_path, stop),
        daemon=True,
    )

    mcm_thread.start()
    tracer_thread.start()

    mcm_thread.join(timeout=duration_s + 5)
    stop.set()
    tracer_thread.join(timeout=3)

    return result


# ── Metric comparison helpers ────────────────────────────────────────────────


@dataclass
class ComparisonPoint:
    """A single metric comparison between MCM and tracer values."""

    metric: str
    entity: str  # e.g. element name or pad key
    mcm_value: float
    tracer_value: float
    unit: str

    @property
    def abs_error(self) -> float:
        return abs(self.mcm_value - self.tracer_value)

    @property
    def rel_error_pct(self) -> float:
        ref = max(abs(self.mcm_value), abs(self.tracer_value))
        if ref < 1e-12:
            return 0.0
        return 100.0 * self.abs_error / ref


@dataclass
class MetricSummary:
    """Aggregated summary for one metric across all entities."""

    metric: str
    unit: str
    points: list[ComparisonPoint] = field(default_factory=list)

    @property
    def count(self) -> int:
        return len(self.points)

    @property
    def mean_abs_error(self) -> float:
        if not self.points:
            return 0.0
        return sum(p.abs_error for p in self.points) / len(self.points)

    @property
    def mean_rel_error_pct(self) -> float:
        if not self.points:
            return 0.0
        return sum(p.rel_error_pct for p in self.points) / len(self.points)

    @property
    def max_rel_error_pct(self) -> float:
        if not self.points:
            return 0.0
        return max(p.rel_error_pct for p in self.points)


# ── Metric comparators ──────────────────────────────────────────────────────


def compare_framerate(
    snapshots: list[McmSnapshot],
    records: list[TracerRecord],
) -> MetricSummary:
    """Compare MCM pad-level fps against GstShark framerate tracer."""
    summary = MetricSummary("framerate", "fps")
    fr_records = [r for r in records if isinstance(r, FramerateRecord)]
    if not fr_records:
        return summary

    # Aggregate tracer fps per pad: mean over all samples
    tracer_fps: dict[str, list[int]] = {}
    for r in fr_records:
        key = tracer_pad_key(r.element, r.pad)
        tracer_fps.setdefault(key, []).append(r.fps)

    # Get MCM fps per src pad from the last snapshot (most settled)
    if not snapshots:
        return summary
    snap = snapshots[-1]

    for pl in snap.pipelines:
        for pad in pl.pads:
            if pad.direction != "src" or not pad.mean_interval_ms:
                continue
            key = tracer_pad_key(pad.element_name, pad.pad_name)
            if key not in tracer_fps:
                continue
            mcm_fps = 1000.0 / pad.mean_interval_ms if pad.mean_interval_ms > 0 else 0
            tracer_mean = sum(tracer_fps[key]) / len(tracer_fps[key])
            summary.points.append(
                ComparisonPoint("framerate", key, mcm_fps, tracer_mean, "fps")
            )

    return summary


def compare_bitrate(
    snapshots: list[McmSnapshot],
    records: list[TracerRecord],
) -> MetricSummary:
    """Compare MCM pad-level bitrate against GstShark bitrate tracer."""
    summary = MetricSummary("bitrate", "bps")
    br_records = [r for r in records if isinstance(r, BitrateRecord)]
    if not br_records:
        return summary

    tracer_bps: dict[str, list[int]] = {}
    for r in br_records:
        key = tracer_pad_key(r.element, r.pad)
        tracer_bps.setdefault(key, []).append(r.bitrate)

    if not snapshots:
        return summary
    snap = snapshots[-1]

    for pl in snap.pipelines:
        for pad in pl.pads:
            if pad.direction != "src" or pad.bitrate_bps is None:
                continue
            key = tracer_pad_key(pad.element_name, pad.pad_name)
            if key not in tracer_bps:
                continue
            tracer_mean = sum(tracer_bps[key]) / len(tracer_bps[key])
            summary.points.append(
                ComparisonPoint("bitrate", key, pad.bitrate_bps, tracer_mean, "bps")
            )

    return summary


def compare_processing_time(
    snapshots: list[McmSnapshot],
    records: list[TracerRecord],
) -> MetricSummary:
    """Compare MCM element processing_time_us against core element-latency tracer."""
    summary = MetricSummary("processing_time", "us")
    el_records = [r for r in records if isinstance(r, ElementLatencyRecord)]
    if not el_records:
        # Fall back to GstShark proctime if available
        pt_records = [r for r in records if isinstance(r, ProctimeRecord)]
        if not pt_records:
            return summary

        tracer_us: dict[str, list[float]] = {}
        for r in pt_records:
            tracer_us.setdefault(r.element, []).append(r.time_ns / 1000.0)

        if not snapshots:
            return summary
        snap = snapshots[-1]

        for pl in snap.pipelines:
            for el in pl.elements:
                if el.processing_time_us is None:
                    continue
                if el.name not in tracer_us:
                    continue
                tracer_mean = sum(tracer_us[el.name]) / len(tracer_us[el.name])
                summary.points.append(
                    ComparisonPoint(
                        "processing_time", el.name, el.processing_time_us, tracer_mean, "us"
                    )
                )
        return summary

    # Core element-latency records
    tracer_us: dict[str, list[float]] = {}
    for r in el_records:
        tracer_us.setdefault(r.element, []).append(r.latency_ns / 1000.0)

    if not snapshots:
        return summary
    snap = snapshots[-1]

    for pl in snap.pipelines:
        for el in pl.elements:
            if el.processing_time_us is None:
                continue
            if el.name not in tracer_us:
                continue
            tracer_mean = sum(tracer_us[el.name]) / len(tracer_us[el.name])
            summary.points.append(
                ComparisonPoint(
                    "processing_time", el.name, el.processing_time_us, tracer_mean, "us"
                )
            )

    return summary


def compare_inter_buffer_interval(
    snapshots: list[McmSnapshot],
    records: list[TracerRecord],
) -> MetricSummary:
    """Compare MCM pad mean_interval_ms against GstShark scheduletime tracer.

    GstShark scheduletime measures push intervals on src pads (despite the
    docs saying sink pads). We match against both src and sink MCM pads.
    """
    summary = MetricSummary("inter_buffer_interval", "ms")
    st_records = [r for r in records if isinstance(r, ScheduletimeRecord)]
    if not st_records:
        return summary

    tracer_ms: dict[str, list[float]] = {}
    for r in st_records:
        key = tracer_pad_key(r.element, r.pad)
        tracer_ms.setdefault(key, []).append(r.time_ns / 1_000_000.0)

    if not snapshots:
        return summary
    snap = snapshots[-1]

    for pl in snap.pipelines:
        for pad in pl.pads:
            if not pad.mean_interval_ms:
                continue
            key = tracer_pad_key(pad.element_name, pad.pad_name)
            if key not in tracer_ms:
                continue
            tracer_mean = sum(tracer_ms[key]) / len(tracer_ms[key])
            summary.points.append(
                ComparisonPoint(
                    "inter_buffer_interval",
                    key,
                    pad.mean_interval_ms,
                    tracer_mean,
                    "ms",
                )
            )

    return summary


def compare_pipeline_latency(
    snapshots: list[McmSnapshot],
    records: list[TracerRecord],
) -> MetricSummary:
    """Compare MCM pipeline causal latency against core latency tracer.

    This is the most interesting comparison: MCM uses PTS-matching (passive),
    while the core tracer uses event injection (active). Systematic differences
    are expected and valuable to document.
    """
    summary = MetricSummary("pipeline_latency", "ms")
    lat_records = [r for r in records if isinstance(r, LatencyRecord)]
    if not lat_records:
        return summary

    # Aggregate all tracer pipeline latency samples (ms)
    tracer_latency_ms = [r.latency_ns / 1_000_000.0 for r in lat_records]
    if not tracer_latency_ms:
        return summary

    tracer_mean = sum(tracer_latency_ms) / len(tracer_latency_ms)
    tracer_sorted = sorted(tracer_latency_ms)
    tracer_p50 = _percentile(tracer_sorted, 50)
    tracer_p95 = _percentile(tracer_sorted, 95)
    tracer_p99 = _percentile(tracer_sorted, 99)

    if not snapshots:
        return summary
    snap = snapshots[-1]

    for pl in snap.pipelines:
        cl = pl.causal_latency_ms
        if not cl:
            # Fall back to freshness delay if causal latency is unavailable
            summary.points.append(
                ComparisonPoint(
                    "pipeline_latency (freshness vs event-injection)",
                    pl.name,
                    pl.freshness_delay_ms,
                    tracer_mean,
                    "ms",
                )
            )
            continue

        mcm_mean = cl.get("mean", 0.0)
        summary.points.append(
            ComparisonPoint(
                "pipeline_latency (mean)",
                pl.name,
                mcm_mean,
                tracer_mean,
                "ms",
            )
        )

        mcm_p50 = cl.get("median", 0.0)
        summary.points.append(
            ComparisonPoint(
                "pipeline_latency (p50)",
                pl.name,
                mcm_p50,
                tracer_p50,
                "ms",
            )
        )

        mcm_p95 = cl.get("p95", 0.0)
        summary.points.append(
            ComparisonPoint(
                "pipeline_latency (p95)",
                pl.name,
                mcm_p95,
                tracer_p95,
                "ms",
            )
        )

        mcm_p99 = cl.get("p99", 0.0)
        summary.points.append(
            ComparisonPoint(
                "pipeline_latency (p99)",
                pl.name,
                mcm_p99,
                tracer_p99,
                "ms",
            )
        )

    return summary


def _percentile(sorted_vals: list[float], pct: int) -> float:
    """Compute the pth percentile from a sorted list using nearest-rank."""
    if not sorted_vals:
        return 0.0
    idx = max(0, min(len(sorted_vals) - 1, int(math.ceil(pct / 100.0 * len(sorted_vals))) - 1))
    return sorted_vals[idx]


# ── Report generation ────────────────────────────────────────────────────────

METHODOLOGY_NOTES = """\
Methodology Notes
=================

Pipeline latency: MCM measures causal latency by matching buffers with identical
PTS values across linked pads (passive, zero-overhead observation). The GStreamer
core latency tracer measures latency by injecting custom downstream events at the
source element and reading them back at the sink (active, event-injection).
These are fundamentally different methodologies. Systematic divergence is expected
and does not indicate a bug in either system. The comparison is valuable because:
  - Agreement validates that both approaches converge on ground truth.
  - Divergence reveals queuing/batching effects visible to one method but not the other.

Processing time: MCM computes per-buffer causal processing time: for each buffer,
it records wall-clock time on the sink pad (arrival) and src pad (departure) and
accumulates the delta. GstShark's proctime uses an equivalent approach (pad-push-pre
hooks). Both measure the same thing and agree within <10% for filter elements.
The core element-latency tracer uses injected events (a different methodology)
and may diverge more on lightweight or queuing elements.

Framerate / Bitrate / Inter-buffer interval: These are the most directly comparable
metrics. MCM computes them from per-buffer pad probes with lock-free atomics.
GstShark computes them with its own pad hooks at 1Hz aggregation. Small differences
(< 5%) are expected due to windowing boundaries.
"""


def format_table(summaries: list[MetricSummary], threshold_pct: float) -> str:
    """Format a human-readable comparison table."""
    lines: list[str] = []

    lines.append("")
    lines.append("=" * 100)
    lines.append("MCM vs GStreamer Tracer Validation Report")
    lines.append("=" * 100)

    any_data = False

    for s in summaries:
        if not s.points:
            lines.append(f"\n--- {s.metric} ({s.unit}) ---")
            lines.append("  (no matching data -- tracer not detected or no overlapping entities)")
            continue

        any_data = True
        lines.append(f"\n--- {s.metric} ({s.unit}) ---")
        lines.append(
            f"  {'Entity':<40} {'MCM':>12} {'Tracer':>12} {'AbsErr':>10} {'RelErr%':>8} {'Status':>8}"
        )
        lines.append(f"  {'-'*40} {'-'*12} {'-'*12} {'-'*10} {'-'*8} {'-'*8}")

        for p in s.points:
            status = "PASS" if p.rel_error_pct <= threshold_pct else "WARN"
            entity = p.entity[:40]
            lines.append(
                f"  {entity:<40} {p.mcm_value:>12.2f} {p.tracer_value:>12.2f} "
                f"{p.abs_error:>10.2f} {p.rel_error_pct:>7.1f}% {status:>8}"
            )

        lines.append(
            f"  Summary: {s.count} points, "
            f"mean |err| = {s.mean_abs_error:.2f} {s.unit}, "
            f"mean rel err = {s.mean_rel_error_pct:.1f}%, "
            f"max rel err = {s.max_rel_error_pct:.1f}%"
        )

    if not any_data:
        lines.append("\nNo overlapping data found between MCM and tracer output.")
        lines.append("Ensure tracers are enabled and MCM has active pipelines.")

    lines.append("")
    return "\n".join(lines)


def build_json_report(
    summaries: list[MetricSummary],
    result: CollectionResult,
    duration_s: int,
    threshold_pct: float,
) -> dict:
    """Build a machine-readable JSON report."""
    metrics = {}
    for s in summaries:
        points_data = []
        for p in s.points:
            points_data.append(
                {
                    "entity": p.entity,
                    "mcm_value": round(p.mcm_value, 4),
                    "tracer_value": round(p.tracer_value, 4),
                    "abs_error": round(p.abs_error, 4),
                    "rel_error_pct": round(p.rel_error_pct, 2),
                    "pass": p.rel_error_pct <= threshold_pct,
                }
            )
        metrics[s.metric] = {
            "unit": s.unit,
            "point_count": s.count,
            "mean_abs_error": round(s.mean_abs_error, 4),
            "mean_rel_error_pct": round(s.mean_rel_error_pct, 2),
            "max_rel_error_pct": round(s.max_rel_error_pct, 2),
            "points": points_data,
        }

    warns = sum(
        1
        for s in summaries
        for p in s.points
        if p.rel_error_pct > threshold_pct
    )

    return {
        "validation": "mcm_vs_gstreamer_tracers",
        "timestamp": time.strftime("%Y-%m-%dT%H:%M:%S%z"),
        "config": {
            "mcm_url": BASE_URL,
            "trace_file": TRACE_FILE,
            "duration_s": duration_s,
            "threshold_pct": threshold_pct,
        },
        "collection": {
            "mcm_snapshots": len(result.mcm_snapshots),
            "tracer_records": len(result.tracer_records),
            "detected_tracers": sorted(result.detected_tracers),
        },
        "overall_pass": warns == 0,
        "warnings": warns,
        "metrics": metrics,
        "methodology_notes": METHODOLOGY_NOTES.strip(),
    }


# ── Main ─────────────────────────────────────────────────────────────────────


def main():
    parser = argparse.ArgumentParser(
        description="Cross-validate MCM stats against GStreamer coretracers.",
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog=__doc__,
    )
    parser.add_argument(
        "--duration",
        type=int,
        default=30,
        help="Collection duration in seconds (default: 30)",
    )
    parser.add_argument(
        "--threshold",
        type=float,
        default=10.0,
        help="Relative error %% threshold for warnings (default: 10.0)",
    )
    parser.add_argument(
        "--json-out",
        type=str,
        default=None,
        help="Path to write the JSON report (default: stdout only)",
    )
    parser.add_argument(
        "--warmup",
        type=int,
        default=5,
        help="Warmup period in seconds before collection starts (default: 5)",
    )
    args = parser.parse_args()

    print(f"MCM Stats vs GStreamer Tracer Validation")
    print(f"  MCM URL:    {BASE_URL}")
    print(f"  Trace file: {TRACE_FILE}")
    print(f"  Duration:   {args.duration}s (+ {args.warmup}s warmup)")
    print(f"  Threshold:  {args.threshold}%")
    print()

    # Connectivity check
    print("Checking MCM connectivity...", end=" ")
    try:
        level_data = get_json(f"{STATS_URL}/level")
        level = level_data.get("level", "unknown") if level_data else "unknown"
        print(f"OK (level: {level})")
    except Exception as exc:
        print(f"FAILED: {exc}")
        print("Is MCM running with --pipeline-analysis-level lite|full?")
        sys.exit(1)

    if level not in ("lite", "full"):
        print(f"WARNING: stats level is '{level}', some metrics may be unavailable.")

    # Check trace file
    trace_path = Path(TRACE_FILE)
    if not trace_path.exists():
        print(f"WARNING: Trace file not found: {TRACE_FILE}")
        print("Ensure MCM was started with GST_DEBUG_FILE set.")
        print("Will continue collecting MCM data only.\n")
    else:
        size = trace_path.stat().st_size
        print(f"Trace file: {size} bytes (will tail for new records)")

    # Warmup: let the pipeline settle
    if args.warmup > 0:
        print(f"\nWarming up for {args.warmup}s...")
        time.sleep(args.warmup)

    # Collection phase
    print(f"\nCollecting data for {args.duration}s...")
    result = run_collection(args.duration, TRACE_FILE)

    print(f"  MCM snapshots collected: {len(result.mcm_snapshots)}")
    print(f"  Tracer records parsed:   {len(result.tracer_records)}")
    if result.detected_tracers:
        print(f"  Detected tracer types:   {', '.join(sorted(result.detected_tracers))}")
    else:
        print("  No tracer records detected in the log file.")

    # Comparison phase
    print("\nComparing metrics...")
    summaries = [
        compare_framerate(result.mcm_snapshots, result.tracer_records),
        compare_bitrate(result.mcm_snapshots, result.tracer_records),
        compare_processing_time(result.mcm_snapshots, result.tracer_records),
        compare_inter_buffer_interval(result.mcm_snapshots, result.tracer_records),
        compare_pipeline_latency(result.mcm_snapshots, result.tracer_records),
    ]

    # Report
    table = format_table(summaries, args.threshold)
    print(table)
    print(METHODOLOGY_NOTES)

    # JSON output
    report = build_json_report(summaries, result, args.duration, args.threshold)

    if args.json_out:
        out_path = Path(args.json_out)
        out_path.write_text(json.dumps(report, indent=2) + "\n")
        print(f"JSON report written to: {out_path}")
    else:
        print("(Use --json-out <path> to save the machine-readable JSON report)")

    # Exit code
    total_warns = report["warnings"]
    total_points = sum(s.count for s in summaries)
    if total_points == 0:
        print("\nResult: NO DATA -- no overlapping metrics found.")
        sys.exit(2)
    elif total_warns > 0:
        print(
            f"\nResult: {total_warns} warning(s) out of {total_points} comparison points "
            f"exceeded {args.threshold}% threshold."
        )
        sys.exit(1)
    else:
        print(f"\nResult: ALL PASS -- {total_points} points within {args.threshold}% threshold.")
        sys.exit(0)


if __name__ == "__main__":
    main()
