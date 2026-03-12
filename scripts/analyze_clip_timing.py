#!/usr/bin/env python3
"""Overlay inter-frame timing gaps with visual frame differences for an event clip.

Usage:
    python scripts/analyze_clip_timing.py <clip.mkv> [--output <path.png>] [--fps <30>]

If --output is omitted the PNG is saved next to the clip as *_timing.png.
"""

import argparse
import subprocess
import sys
from pathlib import Path

import matplotlib.patches as mpatches
import matplotlib.pyplot as plt
import numpy as np


def extract_pts(clip: str) -> np.ndarray:
    """Extract presentation timestamps from a video file via ffprobe."""
    result = subprocess.run(
        [
            "ffprobe",
            "-loglevel", "error",
            "-select_streams", "v:0",
            "-show_entries", "packet=pts_time",
            "-of", "csv=p=0",
            clip,
        ],
        capture_output=True,
        text=True,
        check=True,
    )
    pts = []
    for line in result.stdout.strip().splitlines():
        line = line.strip()
        if line:
            pts.append(float(line))
    pts.sort()
    return np.array(pts)


def extract_frame_pixels(clip: str, frame_idx: int) -> np.ndarray | None:
    """Extract a single frame as grayscale raw bytes via ffmpeg."""
    result = subprocess.run(
        [
            "ffmpeg", "-y", "-loglevel", "error",
            "-i", clip,
            "-vf", f"select=eq(n\\,{frame_idx})",
            "-frames:v", "1",
            "-pix_fmt", "gray",
            "-f", "rawvideo",
            "pipe:1",
        ],
        capture_output=True,
    )
    if result.stdout:
        return np.frombuffer(result.stdout, dtype=np.uint8)
    return None


def compute_visual_diffs(clip: str, n_frames: int) -> np.ndarray:
    """Compute mean-absolute-difference between consecutive frames."""
    print(f"Extracting pixel data for {n_frames} frames...", file=sys.stderr)
    frames = {}
    for fn in range(n_frames):
        data = extract_frame_pixels(clip, fn)
        if data is not None:
            frames[fn] = data

    print("Computing visual diffs...", file=sys.stderr)
    mad = []
    for i in range(1, n_frames):
        if (
            i in frames
            and (i - 1) in frames
            and len(frames[i]) == len(frames[i - 1])
        ):
            diff = np.abs(
                frames[i].astype(np.int16) - frames[i - 1].astype(np.int16)
            )
            mad.append(diff.mean())
        else:
            mad.append(0.0)
    return np.array(mad)


def plot_timing_vs_content(
    pts: np.ndarray,
    mad: np.ndarray,
    deltas_ms: np.ndarray,
    expected_ms: float,
    title: str,
    output: str,
):
    fig, (ax1, ax2) = plt.subplots(
        2, 1, figsize=(14, 7), sharex=True,
        gridspec_kw={"height_ratios": [2, 1.5]},
    )
    frame_nums = np.arange(1, len(deltas_ms) + 1)

    freeze_thresh = expected_ms * 2.4
    burst_thresh = expected_ms * 0.3

    colors = []
    for d in deltas_ms:
        if d > freeze_thresh:
            colors.append("#d62728")
        elif d < burst_thresh:
            colors.append("#ff7f0e")
        else:
            colors.append("#1f77b4")

    ax1.bar(frame_nums, deltas_ms, color=colors, width=1.0, edgecolor="none")
    ax1.axhline(expected_ms, color="#2ca02c", ls="--", lw=1.5, alpha=0.7)
    ax1.set_ylabel("Inter-frame gap (ms)", fontsize=11)
    ax1.set_title(title, fontsize=13, fontweight="bold")
    ax1.set_ylim(0, min(max(deltas_ms) * 1.15, 300))

    for i, d in enumerate(deltas_ms):
        if d > freeze_thresh:
            ax1.annotate(
                f"{d:.0f} ms",
                xy=(i + 1, d),
                xytext=(0, 6),
                textcoords="offset points",
                fontsize=9,
                ha="center",
                color="#d62728",
                fontweight="bold",
            )

    freeze_patch = mpatches.Patch(color="#d62728", label=f"Freeze (>{freeze_thresh:.0f}ms)")
    burst_patch = mpatches.Patch(color="#ff7f0e", label=f"Burst (<{burst_thresh:.0f}ms)")
    normal_patch = mpatches.Patch(color="#1f77b4", label="Normal")
    expected_line = plt.Line2D(
        [0], [0], color="#2ca02c", ls="--", label=f"Expected {expected_ms:.1f}ms"
    )
    ax1.legend(
        handles=[freeze_patch, burst_patch, normal_patch, expected_line],
        loc="upper right", fontsize=9,
    )

    ax2.bar(frame_nums, mad, color="#7f7f7f", width=1.0, edgecolor="none", alpha=0.6)
    for i, d in enumerate(deltas_ms):
        if d > freeze_thresh:
            ax2.axvspan(i + 0.5, i + 1.5, color="#d62728", alpha=0.2)
        elif d < burst_thresh:
            ax2.axvspan(i + 0.5, i + 1.5, color="#ff7f0e", alpha=0.15)

    ax2.set_ylabel("Visual change\n(mean |pixel diff|)", fontsize=11)
    ax2.set_xlabel("Frame pair index", fontsize=11)

    median_mad = np.median(mad[mad > 0]) if np.any(mad > 0) else 0
    ax2.axhline(median_mad, color="#9467bd", ls=":", lw=1.5, label=f"Median MAD={median_mad:.1f}")

    for i, d in enumerate(deltas_ms):
        if d > freeze_thresh:
            ax2.annotate(
                f"MAD={mad[i]:.1f}\n({mad[i] / 255 * 100:.1f}% of range)",
                xy=(i + 1, mad[i]),
                xytext=(15, 15),
                textcoords="offset points",
                fontsize=8,
                ha="center",
                color="#d62728",
                arrowprops=dict(arrowstyle="->", color="#d62728", lw=1),
            )

    ax2.legend(fontsize=9)
    ax2.set_xlim(0, len(deltas_ms) + 1)

    plt.tight_layout()
    plt.savefig(output, dpi=150, bbox_inches="tight")
    plt.close()
    print(f"Saved: {output}", file=sys.stderr)


def print_summary(pts: np.ndarray, deltas_ms: np.ndarray, mad: np.ndarray, expected_ms: float):
    freeze_thresh = expected_ms * 2.4
    burst_thresh = expected_ms * 0.3

    print(f"\n{'='*60}")
    print(f"  Frames:   {len(pts)}")
    print(f"  Duration: {pts[-1] - pts[0]:.3f} s")
    print(f"  Mean Δ:   {np.mean(deltas_ms):.1f} ms  (expected {expected_ms:.1f} ms)")
    print(f"  Median Δ: {np.median(deltas_ms):.1f} ms")
    print(f"  Std Δ:    {np.std(deltas_ms):.1f} ms")
    print(f"  Min Δ:    {np.min(deltas_ms):.1f} ms   Max Δ: {np.max(deltas_ms):.1f} ms")
    print(f"{'='*60}")

    freeze_pairs = [
        (i, deltas_ms[i], mad[i])
        for i in range(len(deltas_ms))
        if deltas_ms[i] > freeze_thresh
    ]
    normal_mad = [
        mad[i]
        for i in range(len(deltas_ms))
        if burst_thresh <= deltas_ms[i] <= freeze_thresh
    ]

    if freeze_pairs:
        normal_mean = np.mean(normal_mad) if normal_mad else 1.0
        print(f"\n  Freeze gaps (>{freeze_thresh:.0f}ms): {len(freeze_pairs)}")
        for i, dt, m in freeze_pairs:
            equiv = m / normal_mean if normal_mean > 0 else 0
            print(f"    Frame {i}→{i+1}: {dt:.0f}ms gap, MAD={m:.2f}")
            print(f"      {m / 255 * 100:.1f}% pixel range changed across {dt:.0f}ms hold")
            print(f"      ~{equiv:.1f} normal frames of visual change")

    burst_events = [
        (i, deltas_ms[i]) for i in range(len(deltas_ms)) if deltas_ms[i] < burst_thresh
    ]
    if burst_events:
        print(f"\n  Burst gaps (<{burst_thresh:.0f}ms): {len(burst_events)}")
        for i, d in burst_events:
            print(f"    Frame {i}→{i+1}: {d:.1f}ms  (PTS {pts[i]:.3f}→{pts[i+1]:.3f})")
    print()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("clip", help="Path to the event clip (.mkv)")
    parser.add_argument("--output", "-o", help="Output PNG path (default: <clip>_timing.png)")
    parser.add_argument("--fps", type=float, default=30.0, help="Nominal FPS (default: 30)")
    args = parser.parse_args()

    clip = args.clip
    if not Path(clip).exists():
        print(f"Error: {clip} not found", file=sys.stderr)
        sys.exit(1)

    output = args.output
    if not output:
        p = Path(clip)
        output = str(p.with_name(p.stem + "_timing.png"))

    expected_ms = 1000.0 / args.fps

    print(f"Analyzing: {clip}", file=sys.stderr)
    pts = extract_pts(clip)
    if len(pts) < 2:
        print("Error: fewer than 2 frames found", file=sys.stderr)
        sys.exit(1)

    deltas_ms = np.diff(pts) * 1000.0
    mad = compute_visual_diffs(clip, len(pts))

    clip_name = Path(clip).parent.parent.name + "/" + Path(clip).name
    title = f"{clip_name}: Timing gaps vs visual content change"

    plot_timing_vs_content(pts, mad, deltas_ms, expected_ms, title, output)
    print_summary(pts, deltas_ms, mad, expected_ms)


if __name__ == "__main__":
    main()
