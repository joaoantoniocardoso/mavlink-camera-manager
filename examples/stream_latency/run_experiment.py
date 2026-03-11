#!/usr/bin/env python3
"""
Automated experiment runner for stream_latency measurements.

Builds MCM, starts it, waits for stability, runs N measurement runs,
captures stats API snapshots, and aggregates results.

Usage:
    python run_experiment.py --label baseline --runs 5 --duration 60

Prerequisites:
    - Run from the mavlink-camera-manager repo root
    - MCM should NOT already be running (this script manages the lifecycle)
"""

import argparse
import json
import os
import signal
import subprocess
import sys
import time
from pathlib import Path


DEFAULT_MCM_ARGS = [
    "--mavlink", "udpin:127.0.0.1:5777",
    "--verbose",
    "--rest-server", "127.0.0.1:8020",
    "--signalling-server", "ws://127.0.0.1:8021",
    "--settings-file=./dev/loop_settings.json",
    "--pipeline-analysis-level", "full",
]

DEFAULT_LATENCY_ARGS = [
    "--rtsp", "rtsp://127.0.0.1:8554/ball",
    "--codec", "h264",
]


def wait_for_mcm(rest_url: str, timeout: int = 30) -> bool:
    """Poll MCM REST API until it responds."""
    import urllib.request
    import urllib.error

    start = time.time()
    while time.time() - start < timeout:
        try:
            urllib.request.urlopen(f"{rest_url}/v4l", timeout=2)
            return True
        except (urllib.error.URLError, OSError):
            time.sleep(1)
    return False


def wait_for_streams(rest_url: str, timeout: int = 30) -> list:
    """Wait for at least one stream to be available on the signalling server."""
    import urllib.request
    import urllib.error

    start = time.time()
    while time.time() - start < timeout:
        try:
            resp = urllib.request.urlopen(f"{rest_url}/streams", timeout=2)
            data = json.loads(resp.read())
            if data:
                return data
        except (urllib.error.URLError, OSError, json.JSONDecodeError):
            pass
        time.sleep(1)
    return []


def capture_stats_snapshot(rest_url: str, output_path: str):
    """Capture a stats API snapshot."""
    import urllib.request
    try:
        resp = urllib.request.urlopen(f"{rest_url}/stats/streams/snapshot", timeout=10)
        data = resp.read()
        Path(output_path).parent.mkdir(parents=True, exist_ok=True)
        with open(output_path, "wb") as f:
            f.write(data)
        print(f"  Stats snapshot saved to {output_path}")
    except Exception as e:
        print(f"  Warning: Failed to capture stats snapshot: {e}", file=sys.stderr)


def run_experiment(args):
    results_dir = Path(args.output_dir) / args.label
    results_dir.mkdir(parents=True, exist_ok=True)

    rest_url = f"http://{args.rest_server}"

    mcm_process = None
    if not args.no_mcm:
        print(f"Building MCM...")
        build_result = subprocess.run(
            ["cargo", "build"],
            capture_output=True,
            text=True,
        )
        if build_result.returncode != 0:
            print(f"Build failed:\n{build_result.stderr}", file=sys.stderr)
            sys.exit(1)
        print("Build complete.")

        print("Starting MCM...")
        mcm_cmd = ["cargo", "run", "--"] + DEFAULT_MCM_ARGS
        if args.mcm_args:
            mcm_cmd.extend(args.mcm_args)
        mcm_process = subprocess.Popen(
            mcm_cmd,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
        )

        print(f"Waiting for MCM REST API at {rest_url}...")
        if not wait_for_mcm(rest_url, timeout=60):
            print("MCM failed to start within timeout", file=sys.stderr)
            mcm_process.terminate()
            sys.exit(1)
        print("MCM is running.")

        print("Waiting for streams to be available...")
        streams = wait_for_streams(rest_url, timeout=30)
        if not streams:
            print("No streams available within timeout", file=sys.stderr)
            mcm_process.terminate()
            sys.exit(1)
        print(f"Found {len(streams)} stream(s).")

        print(f"Stabilization pause ({args.stabilize}s)...")
        time.sleep(args.stabilize)

    # Start camera SoC monitor (if configured)
    camera_process = None
    script_dir = Path(__file__).resolve().parent
    if args.camera_host:
        camera_output = str(results_dir / "camera_soc.ndjson")
        camera_cmd = [
            sys.executable, str(script_dir / "camera_monitor.py"),
            args.camera_host,
            "--user", args.camera_user,
            "--password", args.camera_password,
            "--output", camera_output,
            "--interval", str(args.camera_interval),
        ]
        print(f"Starting camera SoC monitor on {args.camera_host}...")
        camera_process = subprocess.Popen(
            camera_cmd,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
        )

    # Detect producer ID for WebRTC
    webrtc_args = []
    if args.webrtc_url:
        webrtc_args = ["--webrtc", args.webrtc_url]
        if args.producer_id:
            webrtc_args += ["--producer-id", args.producer_id]

    # Run measurements
    print(f"\nStarting {args.runs} measurement runs of {args.duration}s each...")

    latency_cmd = [
        "cargo", "run", "--example", "stream_latency", "--",
    ] + DEFAULT_LATENCY_ARGS + webrtc_args + [
        "--warmup", str(args.warmup),
        "--duration", str(args.duration),
        "--runs", str(args.runs),
        "--run-pause", str(args.run_pause),
        "--csv", str(results_dir),
        "--json", str(results_dir / "summary.json"),
    ]

    print(f"Command: {' '.join(latency_cmd)}")
    result = subprocess.run(latency_cmd)

    if result.returncode != 0:
        print(f"stream_latency exited with code {result.returncode}", file=sys.stderr)

    # Capture final stats snapshot
    capture_stats_snapshot(rest_url, str(results_dir / "stats_snapshot.json"))

    # Cleanup
    if camera_process:
        print("Stopping camera monitor...")
        camera_process.send_signal(signal.SIGINT)
        try:
            camera_process.wait(timeout=5)
        except subprocess.TimeoutExpired:
            camera_process.terminate()
            camera_process.wait(timeout=3)
        print("Camera monitor stopped.")

    if mcm_process:
        print("Stopping MCM...")
        mcm_process.send_signal(signal.SIGINT)
        try:
            mcm_process.wait(timeout=10)
        except subprocess.TimeoutExpired:
            mcm_process.terminate()
            mcm_process.wait(timeout=5)
        print("MCM stopped.")

    print(f"\nResults saved to {results_dir}/")
    print(f"  - run_*.csv (per-run raw data)")
    print(f"  - summary.json (aggregated summary)")
    print(f"  - stats_snapshot.json (pipeline stats)")
    if camera_process:
        print(f"  - camera_soc.ndjson (camera SoC telemetry)")


def main():
    parser = argparse.ArgumentParser(description="Automated stream_latency experiment runner")
    parser.add_argument("--label", required=True, help="Experiment label (used as subdirectory name)")
    parser.add_argument("--runs", type=int, default=5, help="Number of measurement runs")
    parser.add_argument("--duration", type=int, default=60, help="Duration per run in seconds")
    parser.add_argument("--warmup", type=int, default=5, help="Warmup period per run in seconds")
    parser.add_argument("--run-pause", type=int, default=3, help="Pause between runs in seconds")
    parser.add_argument("--output-dir", default="results", help="Base output directory")
    parser.add_argument("--rest-server", default="127.0.0.1:8020", help="MCM REST server address")
    parser.add_argument("--webrtc-url", default="ws://127.0.0.1:8021", help="WebRTC signalling URL")
    parser.add_argument("--producer-id", help="WebRTC producer UUID")
    parser.add_argument("--stabilize", type=int, default=10, help="Seconds to wait after MCM starts")
    parser.add_argument("--no-mcm", action="store_true", help="Don't start/stop MCM (assume already running)")
    parser.add_argument("--mcm-args", nargs="*", help="Extra args to pass to MCM")
    parser.add_argument("--camera-host", help="Camera IP for SoC telnet monitoring (enables camera_monitor.py)")
    parser.add_argument("--camera-user", default="root", help="Camera telnet username (default: root)")
    parser.add_argument("--camera-password", help="Camera telnet password (required when --camera-host is set)")
    parser.add_argument("--camera-interval", type=float, default=2.0, help="Camera sampling interval in seconds (default: 2.0)")
    args = parser.parse_args()

    if args.camera_host and not args.camera_password:
        parser.error("--camera-password is required when --camera-host is set")

    run_experiment(args)


if __name__ == "__main__":
    main()
