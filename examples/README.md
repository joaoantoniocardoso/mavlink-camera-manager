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

## Helper scripts

- `stream_latency/run_experiment.py`: Runs repeated measurements, manages the MCM lifecycle, and captures stats API snapshots.
- `stream_latency/compare_ab.py`: Compares two result sets and reports statistical significance.
- `stream_latency/plot_results.py`: Plots CSV results across different bitrate configurations.

Run the Python helpers from the repository root so their relative paths resolve correctly.

## Overnight A/B test runner

The overnight runner lives at `scripts/overnight_ab_test.sh`. It alternates between two BlueOS images, reboots the Pi between runs, collects Pi stats, and saves one directory per trial.

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

4. Stop it with `Ctrl-C` when you have enough trials.

5. Generate a PDF report after the run:

```bash
python scripts/generate_overnight_report.py <output_dir>/ --output <output_dir>/report.pdf
```

The script uses a lock file in the output directory, so if a previous run crashed you may need to remove `<output_dir>/.overnight.lock` before restarting.
