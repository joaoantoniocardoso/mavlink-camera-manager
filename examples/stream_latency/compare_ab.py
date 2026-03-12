#!/usr/bin/env python3
"""
A/B comparison tool for stream_latency results.

Compares two sets of JSON summaries (before / after) and reports
statistical significance using Mann-Whitney U tests.

Usage:
    python compare_ab.py --before results/baseline/ --after results/optimized/
    python compare_ab.py --before-json results/baseline/summary.json --after-json results/optimized/summary.json
    python compare_ab.py --before-csv results/baseline/ --after-csv results/optimized/

The tool accepts either:
  - JSON summary files produced by stream_latency --json
  - Directories containing run_*.csv files
"""

import argparse
import json
import sys
from pathlib import Path

import numpy as np

try:
    from scipy import stats as sp_stats
    HAS_SCIPY = True
except ImportError:
    HAS_SCIPY = False

import matplotlib
matplotlib.use("Agg")
import matplotlib.pyplot as plt


CLIENTS = ["rtsp-0", "webrtc-0"]
CLIENT_LABELS = {"rtsp-0": "RTSP", "webrtc-0": "WebRTC"}


def load_json_summary(path: str) -> dict:
    with open(path) as f:
        return json.load(f)


def load_csv_dir(csv_dir: str) -> dict:
    """Load run_*.csv files and compute per-run summaries matching JSON format."""
    import pandas as pd

    csv_dir = Path(csv_dir)
    csv_files = sorted(csv_dir.glob("run_*.csv"))
    if not csv_files:
        print(f"No run_*.csv files found in {csv_dir}", file=sys.stderr)
        sys.exit(1)

    runs = []
    for i, csv_path in enumerate(csv_files, 1):
        df = pd.read_csv(csv_path)
        clients_summary = []
        for c in CLIENTS:
            arr_col = f"{c}_arrival_us"
            bytes_col = f"{c}_bytes"
            if arr_col not in df.columns:
                continue
            valid = df.dropna(subset=[arr_col])
            if len(valid) < 2:
                continue
            arrivals = np.sort(valid[arr_col].values)
            bytesarr = valid[bytes_col].values
            ia_us = np.diff(arrivals)
            duration_s = (arrivals[-1] - arrivals[0]) / 1e6
            n = len(valid)
            fps = (n - 1) / duration_s if duration_s > 0 else 0
            jitter = float(np.std(ia_us))
            expected_us = 1e6 / fps if fps > 0 else 0
            freeze_threshold = expected_us * 1.5 if expected_us > 0 else 0
            burst_threshold = expected_us * 0.5 if expected_us > 0 else 0

            true_drops = 0
            estimated_missed = 0
            freeze_bursts = 0
            isolated_stutters = 0

            if expected_us > 0:
                in_window = False
                w_total = 0.0
                w_gaps = 0
                w_has_burst = False

                def _classify():
                    nonlocal in_window, true_drops, estimated_missed, freeze_bursts
                    exp_frames = round(w_total / expected_us)
                    deficit = max(0, exp_frames - w_gaps)
                    if deficit > 0:
                        true_drops += 1
                        estimated_missed += deficit
                    elif w_has_burst:
                        freeze_bursts += 1
                    in_window = False

                for gap in ia_us:
                    is_freeze = gap > freeze_threshold
                    is_burst = gap < burst_threshold
                    if in_window:
                        if is_freeze or is_burst:
                            w_gaps += 1
                            w_total += gap
                            w_has_burst |= is_burst
                        else:
                            _classify()
                    elif is_freeze:
                        in_window = True
                        w_gaps = 1
                        w_total = float(gap)
                        w_has_burst = False
                    elif is_burst:
                        isolated_stutters += 1
                if in_window:
                    _classify()

            clients_summary.append({
                "name": c,
                "frames": n,
                "fps": fps,
                "bitrate_mbps": float(bytesarr.sum() * 8 / duration_s / 1e6) if duration_s > 0 else 0,
                "jitter_stddev_us": jitter,
                "inter_arrival_p50_us": float(np.percentile(ia_us, 50)),
                "inter_arrival_p95_us": float(np.percentile(ia_us, 95)),
                "inter_arrival_p99_us": float(np.percentile(ia_us, 99)),
                "stutters": {
                    "true_drop_events": true_drops,
                    "estimated_missed_frames": estimated_missed,
                    "freeze_burst_events": freeze_bursts,
                    "isolated_stutter_events": isolated_stutters,
                },
            })

        pairs_summary = []
        for ci, a in enumerate(CLIENTS):
            for b in CLIENTS[ci+1:]:
                a_col, b_col = f"{a}_arrival_us", f"{b}_arrival_us"
                if a_col not in df.columns or b_col not in df.columns:
                    continue
                matched = df.dropna(subset=[a_col, b_col])
                if matched.empty:
                    continue
                deltas_us = (matched[b_col] - matched[a_col]).values
                pairs_summary.append({
                    "client_a": a,
                    "client_b": b,
                    "matched_frames": len(matched),
                    "delta_mean_us": float(np.mean(deltas_us)),
                    "delta_p50_us": float(np.percentile(deltas_us, 50)),
                    "delta_p95_us": float(np.percentile(deltas_us, 95)),
                    "delta_p99_us": float(np.percentile(deltas_us, 99)),
                    "delta_stddev_us": float(np.std(deltas_us)),
                })

        runs.append({
            "run_index": i,
            "clients": clients_summary,
            "pairs": pairs_summary,
        })

    return {"runs": runs}


def extract_per_run_metric(data: dict, metric_path: str) -> list[float]:
    """Extract a metric from each run.

    metric_path examples:
      "client:rtsp-0:fps"
      "client:webrtc-0:jitter_stddev_us"
      "pair:rtsp-0:webrtc-0:delta_mean_us"
    """
    parts = metric_path.split(":")
    values = []
    for run in data["runs"]:
        if parts[0] == "client":
            cname, field = parts[1], parts[2]
            for c in run["clients"]:
                if c["name"] == cname:
                    if field in c:
                        values.append(float(c[field]))
                    elif "stutters" in c and field in c["stutters"]:
                        values.append(float(c["stutters"][field]))
        elif parts[0] == "pair":
            a, b, field = parts[1], parts[2], parts[3]
            for p in run["pairs"]:
                if p["client_a"] == a and p["client_b"] == b:
                    values.append(float(p[field]))
    return values


def mann_whitney_u(a: list[float], b: list[float]) -> dict:
    """Run Mann-Whitney U test if scipy is available, else return a basic comparison."""
    a_arr, b_arr = np.array(a), np.array(b)
    result = {
        "before_mean": float(np.mean(a_arr)),
        "before_std": float(np.std(a_arr, ddof=1)) if len(a_arr) > 1 else 0.0,
        "after_mean": float(np.mean(b_arr)),
        "after_std": float(np.std(b_arr, ddof=1)) if len(b_arr) > 1 else 0.0,
        "delta": float(np.mean(b_arr) - np.mean(a_arr)),
        "delta_pct": float((np.mean(b_arr) - np.mean(a_arr)) / np.mean(a_arr) * 100) if np.mean(a_arr) != 0 else 0.0,
        "n_before": len(a),
        "n_after": len(b),
    }

    if HAS_SCIPY and len(a) >= 3 and len(b) >= 3:
        u_stat, p_value = sp_stats.mannwhitneyu(a_arr, b_arr, alternative="two-sided")
        n1, n2 = len(a), len(b)
        effect_size = u_stat / (n1 * n2) - 0.5  # rank-biserial correlation
        result["u_statistic"] = float(u_stat)
        result["p_value"] = float(p_value)
        result["effect_size_r"] = float(effect_size)
        result["significant_at_005"] = p_value < 0.05
        result["significant_at_001"] = p_value < 0.01
    else:
        result["p_value"] = None
        result["note"] = "scipy unavailable or insufficient samples for test"

    return result


def format_sig(result: dict) -> str:
    if result.get("p_value") is None:
        return "N/A"
    p = result["p_value"]
    if p < 0.001:
        return f"p={p:.1e} ***"
    elif p < 0.01:
        return f"p={p:.4f} **"
    elif p < 0.05:
        return f"p={p:.4f} *"
    else:
        return f"p={p:.4f} (not significant)"


METRICS = [
    ("client:rtsp-0:fps", "RTSP FPS"),
    ("client:webrtc-0:fps", "WebRTC FPS"),
    ("client:rtsp-0:jitter_stddev_us", "RTSP Jitter (us)"),
    ("client:webrtc-0:jitter_stddev_us", "WebRTC Jitter (us)"),
    ("client:rtsp-0:true_drop_events", "RTSP True Drops"),
    ("client:webrtc-0:true_drop_events", "WebRTC True Drops"),
    ("client:rtsp-0:freeze_burst_events", "RTSP Freeze-Bursts"),
    ("client:webrtc-0:freeze_burst_events", "WebRTC Freeze-Bursts"),
    ("client:rtsp-0:isolated_stutter_events", "RTSP Isolated Stutters"),
    ("client:webrtc-0:isolated_stutter_events", "WebRTC Isolated Stutters"),
    ("pair:rtsp-0:webrtc-0:delta_mean_us", "RTSP->WebRTC Mean Delta (us)"),
    ("pair:rtsp-0:webrtc-0:delta_p50_us", "RTSP->WebRTC P50 Delta (us)"),
    ("pair:rtsp-0:webrtc-0:delta_p95_us", "RTSP->WebRTC P95 Delta (us)"),
    ("pair:rtsp-0:webrtc-0:delta_p99_us", "RTSP->WebRTC P99 Delta (us)"),
    ("pair:rtsp-0:webrtc-0:delta_stddev_us", "RTSP->WebRTC Stddev Delta (us)"),
]


def generate_comparison_report(before: dict, after: dict, output_prefix: str, before_label: str, after_label: str):
    print(f"\n{'='*90}")
    print(f"  A/B COMPARISON: {before_label} vs {after_label}")
    print(f"{'='*90}\n")

    results = {}
    for metric_path, label in METRICS:
        a_vals = extract_per_run_metric(before, metric_path)
        b_vals = extract_per_run_metric(after, metric_path)
        if not a_vals and not b_vals:
            continue
        result = mann_whitney_u(a_vals, b_vals)
        results[metric_path] = result

        sig = format_sig(result)
        direction = "improved" if result["delta"] < 0 else "regressed" if result["delta"] > 0 else "unchanged"
        if "jitter" in label.lower() or "delta" in label.lower() or "drop" in label.lower() or "stutter" in label.lower():
            pass  # lower is better
        elif "fps" in label.lower():
            direction = "improved" if result["delta"] > 0 else "regressed" if result["delta"] < 0 else "unchanged"

        print(f"  {label:45s}  {before_label}: {result['before_mean']:>12.1f} +/- {result['before_std']:>8.1f}")
        print(f"  {' ':45s}  {after_label}:  {result['after_mean']:>12.1f} +/- {result['after_std']:>8.1f}")
        print(f"  {' ':45s}  delta: {result['delta']:>+12.1f} ({result['delta_pct']:>+.1f}%) [{direction}]  {sig}")
        print()

    # Generate comparison plot
    plot_metrics = [
        (m, l) for m, l in METRICS
        if m in results and results[m].get("before_mean", 0) != 0
    ]

    if plot_metrics:
        fig, axes = plt.subplots(2, 3, figsize=(18, 10))
        fig.suptitle(f"A/B Comparison: {before_label} vs {after_label}", fontsize=14)
        axes = axes.flatten()

        plot_idx = 0
        for metric_path, label in plot_metrics:
            if plot_idx >= len(axes):
                break
            r = results[metric_path]
            ax = axes[plot_idx]

            a_vals = extract_per_run_metric(before, metric_path)
            b_vals = extract_per_run_metric(after, metric_path)

            positions = [1, 2]
            bp = ax.boxplot(
                [a_vals, b_vals],
                positions=positions,
                tick_labels=[before_label, after_label],
                patch_artist=True,
                widths=0.6,
            )
            bp["boxes"][0].set_facecolor("#3498db")
            bp["boxes"][0].set_alpha(0.6)
            bp["boxes"][1].set_facecolor("#e74c3c")
            bp["boxes"][1].set_alpha(0.6)

            sig_text = format_sig(r)
            ax.set_title(f"{label}\n{sig_text}", fontsize=9)
            ax.scatter([1]*len(a_vals), a_vals, color="#3498db", alpha=0.7, zorder=5, s=20)
            ax.scatter([2]*len(b_vals), b_vals, color="#e74c3c", alpha=0.7, zorder=5, s=20)

            plot_idx += 1

        for i in range(plot_idx, len(axes)):
            axes[i].set_visible(False)

        fig.tight_layout()
        fig.savefig(f"{output_prefix}_comparison.png", bbox_inches="tight", dpi=120)
        print(f"Saved {output_prefix}_comparison.png")

    # Save JSON results
    json_out = {
        "before_label": before_label,
        "after_label": after_label,
        "metrics": {label: results[mp] for mp, label in METRICS if mp in results},
    }
    json_path = f"{output_prefix}_comparison.json"

    class NumpyEncoder(json.JSONEncoder):
        def default(self, obj):
            if hasattr(obj, "item"):
                return obj.item()
            return super().default(obj)

    with open(json_path, "w") as f:
        json.dump(json_out, f, indent=2, cls=NumpyEncoder)
    print(f"Saved {json_path}")


def main():
    parser = argparse.ArgumentParser(description="A/B comparison of stream_latency results")
    parser.add_argument("--before-json", help="Path to before JSON summary")
    parser.add_argument("--after-json", help="Path to after JSON summary")
    parser.add_argument("--before-csv", help="Path to directory with before run_*.csv files")
    parser.add_argument("--after-csv", help="Path to directory with after run_*.csv files")
    parser.add_argument("--before", help="Path to before results dir (auto-detects json/csv)")
    parser.add_argument("--after", help="Path to after results dir (auto-detects json/csv)")
    parser.add_argument("--before-label", default="Before", help="Label for before dataset")
    parser.add_argument("--after-label", default="After", help="Label for after dataset")
    parser.add_argument("--output", default="ab_results", help="Output file prefix")
    args = parser.parse_args()

    def load_dataset(json_path, csv_path, auto_path):
        if json_path:
            return load_json_summary(json_path)
        if csv_path:
            return load_csv_dir(csv_path)
        if auto_path:
            p = Path(auto_path)
            json_file = p / "summary.json"
            if json_file.exists():
                return load_json_summary(str(json_file))
            if list(p.glob("run_*.csv")):
                return load_csv_dir(str(p))
            if p.suffix == ".json":
                return load_json_summary(str(p))
        print("Could not find data. Provide --before-json/--before-csv/--before", file=sys.stderr)
        sys.exit(1)

    before_data = load_dataset(args.before_json, args.before_csv, args.before)
    after_data = load_dataset(args.after_json, args.after_csv, args.after)

    generate_comparison_report(before_data, after_data, args.output, args.before_label, args.after_label)


if __name__ == "__main__":
    main()
