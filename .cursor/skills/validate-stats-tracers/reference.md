# Reference: GStreamer Tracer Output Formats

Quick reference for the tracer log line formats that `validate_stats.py` parses.

## Log Line Structure

All tracer output appears in the GST_DEBUG log at TRACE level (7) under the
`GST_TRACER` category. The general format is:

```
H:MM:SS.NNNNNNNNN  PID  0xTHREAD  TRACE  GST_TRACER :0:: <structure-name>, <field>=(<type>)<value>, ...;
```

## Core Tracers

### `latency` (pipeline src-to-sink)

Activated with `GST_TRACERS="latency(flags=pipeline)"`.

```
latency, src-element-id=(string)0x55a1234, src-element=(string)videotestsrc0, src=(string)src, sink-element-id=(string)0x55a5678, sink-element=(string)autovideosink0, sink=(string)sink, time=(guint64)21594000, ts=(guint64)6072456000;
```

| Field | Type | Description |
|-------|------|-------------|
| `src-element` | string | Source element name |
| `sink-element` | string | Sink element name |
| `time` | guint64 | Latency in nanoseconds |
| `ts` | guint64 | Timestamp in nanoseconds |

### `element-latency` (per-element)

Activated with `GST_TRACERS="latency(flags=element)"`.

```
element-latency, element-id=(string)0x11e848210, element=(string)videotestsrc0, src=(string)src, time=(guint64)16000, ts=(guint64)6072172000;
```

| Field | Type | Description |
|-------|------|-------------|
| `element` | string | Element name |
| `time` | guint64 | Per-element latency in nanoseconds |

## GstShark Tracers

### `framerate`

```
framerate, pad=(string)videorate0_src, fps=(uint)15;
```

| Field | Type | Description |
|-------|------|-------------|
| `pad` | string | Source pad: `elementname_padname` |
| `fps` | uint | Frames counted in the last 1-second interval |

### `bitrate`

```
bitrate, pad=(string)avenc_h263p0_src, bitrate=(guint64)1408000;
```

| Field | Type | Description |
|-------|------|-------------|
| `pad` | string | Source pad: `elementname_padname` |
| `bitrate` | guint64 | Bits transmitted in the last 1-second interval |

### `proctime`

```
proctime, element=(string)identity0, time=(string)0:00:00.008593923;
```

| Field | Type | Description |
|-------|------|-------------|
| `element` | string | Element name |
| `time` | string | Processing time as `H:MM:SS.NNNNNNNNN` |

### `scheduletime`

```
scheduletime, pad=(string)capsfilter0_sink, time=(string)0:00:00.099998663;
```

| Field | Type | Description |
|-------|------|-------------|
| `pad` | string | Sink pad: `elementname_padname` |
| `time` | string | Inter-buffer interval as `H:MM:SS.NNNNNNNNN` |

## Pad Name Mapping

GStreamer tracer pad names use the format `elementname_padname` (underscore-joined).
MCM exposes `element.name` and `pad.name` separately. The validation script joins
them as `{element_name}_{pad_name}` and splits tracer names on the last underscore
(to handle element names that contain underscores).

## Methodology Comparison

| Aspect | MCM (pad probes) | Core latency tracer | GstShark |
|--------|-------------------|---------------------|----------|
| Mechanism | `gst_pad_add_probe(BUFFER)` | Event injection | Tracer hooks |
| Latency method | PTS-matching (passive) | Custom event (active) | Event injection |
| Framerate | `1000 / mean_interval_ms` | N/A | Buffer count per 1s |
| Bitrate | `sum(size) * 8 / interval` | N/A | `sum(size * 8)` per 1s |
| Processing time | `src.wall - sink.wall` | Event sink-to-src delta | Wall-clock delta |
| Granularity | Per-buffer, lock-free | Per-buffer (events) | 1s aggregate (fps/bps) |
| Data access | In-process API | GST_DEBUG log | GST_DEBUG log |

## Expected Report Output

With core tracers + GstShark enabled and a simple test pipeline (`videotestsrc ! x264enc ! rtph264pay ! udpsink`), typical output:

```
====================================================================================================
MCM vs GStreamer Tracer Validation Report
====================================================================================================

--- framerate (fps) ---
  Entity                                          MCM       Tracer     AbsErr  RelErr%   Status
  ---------------------------------------- ------------ ------------ ---------- -------- --------
  x264enc0_src                                    30.00        30.00       0.00     0.0%     PASS

--- bitrate (bps) ---
  Entity                                          MCM       Tracer     AbsErr  RelErr%   Status
  ---------------------------------------- ------------ ------------ ---------- -------- --------
  rtph264pay0_src                            2048000.00   2048000.00       0.00     0.0%     PASS

--- processing_time (us) ---
  Entity                                          MCM       Tracer     AbsErr  RelErr%   Status
  ---------------------------------------- ------------ ------------ ---------- -------- --------
  x264enc0                                     3200.00      3150.00      50.00     1.6%     PASS

--- pipeline_latency (ms) ---
  Entity                                          MCM       Tracer     AbsErr  RelErr%   Status
  ---------------------------------------- ------------ ------------ ---------- -------- --------
  pipeline_name (mean)                           12.50        11.80       0.70     5.6%     PASS
  pipeline_name (p50)                            12.00        11.50       0.50     4.2%     PASS
```

Values are illustrative. Actual numbers depend on hardware, encoding settings, and
pipeline complexity.
