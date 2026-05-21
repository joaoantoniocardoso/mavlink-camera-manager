# Latency: with vs without source-side queues

Bench-Pi captures from the last leg of the sink-decoupling investigation
(see `pr/sink-decoupling`), used to decide whether to keep or remove the
single-buffer `queue leaky=downstream` element at the source of
`onvif_pipeline.rs` / `redirect_pipeline.rs`.

Both builds share the proxysink/proxysrc sink-side decoupling. They
differ only in whether the producer pipeline still has a source-side
queue:

| Prefix | Branch / build                                            |
|--------|-----------------------------------------------------------|
| `old_` | `pr/sink-decoupling` *with* source-side queue (max=1 buf) |
| `new_` | `pr/sink-decoupling` *without* source-side queue          |

(The prefix names reflect the test order, not which version is "newer"
on master: the build *without* the queue is what `pr/sink-decoupling`
ultimately ships.)

## Scenarios

Each scenario was captured for 300 s with `stream_latency`, with three
parallel clients reading the same camera through different transports:

- `rtsp-0`: direct RTSP from the ONVIF camera (`192.168.2.10:554`).
- `rtsp-1`: RTSP relayed through MCM (`192.168.2.2:8554`).
- `webrtc-0`: WebRTC through MCM (`ws://192.168.2.2:6021`).

| File              | Scenario                                                                |
|-------------------|-------------------------------------------------------------------------|
| `*_baseline.txt`  | Idle Pi, no impairment.                                                 |
| `*_cpu_stress.txt`| 4x `yes > /dev/null` at `nice 19` to saturate every Pi core.            |
| `*_net_impair.txt`| Delay/loss/reorder via `tc netem` on the Pi -> laptop traffic.          |
| `*_combined.txt`  | CPU stress + network impairment together.                               |

## Files

- `old_*.txt` -- raw `stream_latency` output for the build with source queues.
- `new_*.txt` -- raw `stream_latency` output for the build without source queues.
- `parse.py`  -- diffs each scenario across builds and prints a compact
  per-transport / per-frame-class / pairwise-latency table.

## Reproducing

```sh
cd tools/onvehicle/reports/latency_with_vs_without_source_queues
./parse.py
```
