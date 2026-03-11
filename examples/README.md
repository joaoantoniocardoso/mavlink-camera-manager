# Examples

This folder contains standalone examples and helper scripts for measuring stream behavior outside the main `mavlink-camera-manager` binary.

## `stream_latency`

The `stream_latency` example measures end-to-end timing and delivery quality across one or more RTSP, WebRTC, or UDP inputs. It writes per-frame CSV data and an aggregated JSON summary that can be reused for A/B comparisons and report generation.

Build it with:

```bash
cargo build --example stream_latency
```

Run it with:

```bash
cargo run --example stream_latency -- \
  --rtsp "rtsp://192.168.2.10:554/stream_0" \
  --webrtc "ws://192.168.2.2:6021" \
  --producer-id "<producer-uuid>" \
  --codec h264 \
  --warmup 5 \
  --duration 60 \
  --csv results/example_run \
  --json results/example_run/summary.json
```

Pass `--record <DIR>` to capture raw RTP packets as pcap files (one per client). These files can be opened in Wireshark or replayed with GStreamer's `pcapparse` element for offline analysis:

```bash
cargo run --example stream_latency -- \
  --rtsp "rtsp://192.168.2.10:554/stream_0" \
  --webrtc "ws://192.168.2.2:6021" \
  --producer-id "<producer-uuid>" \
  --codec h264 \
  --warmup 5 \
  --duration 60 \
  --record results/recordings
```

## Helper scripts

- `stream_latency/run_experiment.py`: Runs repeated measurements, manages the MCM lifecycle, captures stats API snapshots, and optionally monitors camera SoC telemetry.
- `stream_latency/camera_monitor.py`: Standalone telnet-based monitor for HiSilicon camera SoC internals (temperature, voltage, CPU, memory, network counters). Writes NDJSON snapshots. Also supports `--dump-dmesg` for one-shot kernel log capture. Used automatically by `run_experiment.py` when `--camera-host` is provided.
- `stream_latency/compare_ab.py`: Compares two result sets and reports statistical significance.
- `stream_latency/plot_results.py`: Plots CSV results across different bitrate configurations.

Run the Python helpers from the repository root so their relative paths resolve correctly.

To include camera SoC monitoring alongside the measurements, pass the camera telnet credentials:

```bash
python examples/stream_latency/run_experiment.py \
  --label overnight-baseline \
  --runs 10 --duration 300 \
  --camera-host 192.168.2.10 \
  --camera-password YOUR_CAMERA_PASSWORD
```

This starts `camera_monitor.py` in the background for the duration of the experiment and captures the kernel ring buffer before and after. Results include:

- `camera_soc.ndjson` -- periodic snapshots of temperature, voltages, CPU, memory, and network counters (TX/RX bytes, packets, errors, drops). Kernel version and uptime are recorded in the first sample.
- `dmesg_before.log` / `dmesg_after.log` -- kernel ring buffer dumps. Diff them to find warnings, errors, or OOM events that occurred during the test.

## Overnight A/B test runner

The overnight runner lives at `scripts/overnight_ab_test.sh`. It alternates between two BlueOS images, reboots the Pi between runs, collects Pi stats, and saves one directory per trial.

### Docker image

Cross-platform users can run the prebuilt container instead of installing Rust, GStreamer, and the other local dependencies:

```bash
docker run --rm -it \
  -e OUTPUT_DIR=/results \
  -e PI_HOST=192.168.2.2 \
  -e PI_USER=pi \
  -e PI_PASS=raspberry \
  -e CAMERA_HOST=192.168.2.10 \
  -e PRODUCER_ID=<producer-uuid> \
  -e SKIP_PREFLIGHT=false \
  -e START_TRIAL=1 \
  -e ENABLE_USB_ETH_RESET=false \
  -v "$(pwd)/overnight_tests_1:/results" \
  joaoantoniocardoso/mcm-overnight_ab_test:latest
```

To generate the PDF report from the same image:

```bash
docker run --rm -it \
  -v "$(pwd)/overnight_tests_1:/results" \
  joaoantoniocardoso/mcm-overnight_ab_test:latest \
  report /results --output /results/report.pdf
```

The container already includes the BlueOS image switch helper, so users do not need a separate `BlueOS-docker` checkout.

Before starting a new overnight campaign:

1. Build the measurement client:

```bash
cargo build --example stream_latency
```

2. Decide the run configuration. The script now accepts environment variable overrides directly from the CLI, so you usually do not need to edit the file.

Common overrides:
- `IMAGE_NEXT` and `IMAGE_BETA`: image tags to compare.
- `PRODUCER_ID`, `CAMERA_HOST`, `PI_HOST`, `PI_USER`, `PI_PASS`: hardware and access details.
- `OUTPUT_DIR`: new folder name for this campaign so results do not mix with an older run.
- `SKIP_PREFLIGHT=false` and `START_TRIAL=1`: fresh start.
- `SKIP_PREFLIGHT=true` and `START_TRIAL=<n>`: resume from trial `n`.
- `ENABLE_USB_ETH_RESET=true`: enable the optional USB ethernet adapter reset workaround for setups that need it.
- `ENABLE_CAMERA_RESTART=false`: disable the camera restart request if your setup should leave the camera alone. Default is `true`.
- `ENABLE_CAMERA_MONITOR=true`: enable camera SoC telnet monitoring (temperature, voltage, CPU, memory, network counters, dmesg). Requires `CAMERA_USER` and `CAMERA_PASSWORD` to be set.
- `CAMERA_USER` and `CAMERA_PASSWORD`: telnet credentials for the camera SoC monitor. Required when `ENABLE_CAMERA_MONITOR=true`. Never hardcoded -- must be passed via environment.
- `STATUS_FILE`: path to a gate file that must contain `DONE` before the test proceeds (default: `~/BlueRobotics/blueos-docker-base/test_status.md`). Useful for sequencing with other test runs.

3. Start the run from the repository root:

```bash
OUTPUT_DIR=overnight_tests_6 \
SKIP_PREFLIGHT=false \
START_TRIAL=1 \
bash scripts/overnight_ab_test.sh
```

Example resume command:

```bash
OUTPUT_DIR=overnight_tests_6 \
SKIP_PREFLIGHT=true \
START_TRIAL=18 \
bash scripts/overnight_ab_test.sh
```

If your setup depends on the USB ethernet reset workaround, enable it explicitly:

```bash
OUTPUT_DIR=overnight_tests_6 \
ENABLE_USB_ETH_RESET=true \
SKIP_PREFLIGHT=false \
START_TRIAL=1 \
bash scripts/overnight_ab_test.sh
```

If the camera restart endpoint is not available or should not be used, disable it explicitly:

```bash
OUTPUT_DIR=overnight_tests_6 \
ENABLE_CAMERA_RESTART=false \
SKIP_PREFLIGHT=false \
START_TRIAL=1 \
bash scripts/overnight_ab_test.sh
```

4. Stop it with `Ctrl-C` when you have enough trials.

5. Generate a PDF report after the run:

```bash
python scripts/generate_overnight_report.py <output_dir>/ --output <output_dir>/report.pdf
```

The script uses a lock file in the output directory, so if a previous run crashed you may need to remove `<output_dir>/.overnight.lock` before restarting.
