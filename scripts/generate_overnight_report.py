#!/usr/bin/env python3
"""
Generate a LaTeX-typeset PDF report from overnight A/B test data.

Usage:
    python scripts/generate_overnight_report.py overnight_tests/
    python scripts/generate_overnight_report.py overnight_tests/ --output report.pdf
"""

import argparse
import json
import os
import shutil
import subprocess
import sys
import tempfile
from datetime import date
from pathlib import Path

import numpy as np
import pandas as pd
import matplotlib
matplotlib.use("Agg")
import matplotlib.pyplot as plt
from scipy import stats as sp_stats

# ── Colours ──────────────────────────────────────────────────────────────────

BLUE = "#3498db"
ORANGE = "#e67e22"
RED = "#e74c3c"
GREEN = "#2ecc71"
DARK = "#2c3e50"

BASELINE_COLOR = ORANGE
IMPROVED_COLOR = BLUE

BASELINE = "beta"
IMPROVED = "next"
LABELS = {"next": "next", "beta": "beta"}  # overridden by metadata.json

# ── Matplotlib defaults (plots only – no page text) ─────────────────────────

plt.rcParams.update({
    "figure.facecolor": "white",
    "axes.facecolor": "#f8f9fa",
    "axes.grid": True,
    "grid.alpha": 0.3,
    "font.size": 11,
    "axes.titlesize": 13,
    "axes.labelsize": 11,
    "xtick.labelsize": 10,
    "ytick.labelsize": 10,
    "legend.fontsize": 10,
    "figure.dpi": 200,
})

PLOT_W, PLOT_H = 14, 7       # 2x2 figure – extra height for below-axis legends
PLOT_W3, PLOT_H3 = 14, 9    # 3-row figure

LATENCY_MAX_MS = 1000.0

# ── Data loading ─────────────────────────────────────────────────────────────

def _load_camera_soc(path: Path) -> pd.DataFrame:
    """Parse camera_soc.ndjson into a DataFrame with derived columns."""
    if not path.exists():
        return pd.DataFrame()
    try:
        records = [json.loads(line) for line in path.read_text().splitlines() if line.strip()]
    except Exception:
        return pd.DataFrame()
    if not records:
        return pd.DataFrame()
    df = pd.DataFrame(records)
    df.drop(columns=["system_info"], errors="ignore", inplace=True)

    df["cam_mem_used_mb"] = (df["mem_memtotal_kb"] - df["mem_memavailable_kb"]) / 1024

    dt = df["ts"].diff()
    d_busy = df["cpu_busy"].diff()
    d_total = df["cpu_total"].diff()
    mask = (dt > 0) & (d_total > 0)
    df["cam_cpu_pct"] = np.where(mask, 100.0 * d_busy / d_total, np.nan)

    d_tx = df["eth0_tx_bytes"].diff()
    df["cam_tx_rate_mbps"] = np.where(dt > 0, 8.0 * d_tx / dt / 1e6, np.nan)

    d_rx = df["eth0_rx_bytes"].diff()
    df["cam_rx_rate_mbps"] = np.where(dt > 0, 8.0 * d_rx / dt / 1e6, np.nan)

    return df


def _collect_event_records(csv_df: pd.DataFrame) -> dict:
    """Extract per-event records (timestamp, duration) from raw CSV data.

    Uses the same disruption-window algorithm as the Rust ``detect_stutters``
    implementation.  Each disruption window (consecutive freeze/burst gaps) is
    classified as:

    * **true_drop** – frames genuinely lost (expected_frames > received gaps).
    * **freeze_burst** – frames delayed then caught up (window has burst gaps,
      no frame deficit).
    * **episode** – every disruption window regardless of classification.

    Returns a dict keyed by client name with lists of per-event records
    enabling duration and frequency distribution analysis.
    """
    result = {}
    for client in ("rtsp-0", "webrtc-0"):
        col = f"{client}_arrival_us"
        kf_col = f"{client}_is_keyframe"
        empty = {"true_drops": [], "freeze_bursts": [], "episodes": []}
        if col not in csv_df.columns:
            result[client] = empty
            continue

        sorted_idx = csv_df[col].dropna().sort_values().index
        arrivals = csv_df.loc[sorted_idx, col].values
        has_kf = kf_col in csv_df.columns
        kf_flags = csv_df.loc[sorted_idx, kf_col].values.astype(int) if has_kf else None

        if len(arrivals) < 3:
            result[client] = empty
            continue

        ia = np.diff(arrivals).astype(float)
        fps = (len(arrivals) - 1) / ((arrivals[-1] - arrivals[0]) / 1e6)
        if fps <= 0:
            result[client] = empty
            continue

        expected = 1e6 / fps
        freeze_th = expected * 1.5
        burst_th = expected * 0.5

        true_drops = []
        freeze_bursts = []
        episodes = []

        in_window = False
        win_start = 0.0
        win_total = 0.0
        win_gaps = 0
        win_has_burst = False
        win_severity = 0.0
        win_start_idx = 0

        def _commit_window():
            expected_frames = round(win_total / expected)
            deficit = max(0, expected_frames - win_gaps)
            episodes.append({"timestamp_us": win_start,
                             "duration_us": win_total,
                             "n_frames": win_gaps})
            if deficit > 0:
                true_drops.append({"timestamp_us": win_start,
                                   "duration_us": win_total})
            elif win_has_burst:
                at_kf = bool(kf_flags is not None
                             and win_start_idx + 1 < len(kf_flags)
                             and kf_flags[win_start_idx + 1])
                freeze_bursts.append({
                    "timestamp_us": win_start,
                    "duration_us": win_severity,
                    "at_keyframe": at_kf,
                })

        for i, g in enumerate(ia):
            is_freeze = g > freeze_th
            is_burst = g < burst_th
            if in_window:
                if is_freeze or is_burst:
                    win_gaps += 1
                    win_total += g
                    win_has_burst |= is_burst
                    if is_freeze:
                        win_severity += g - expected
                else:
                    _commit_window()
                    in_window = False
            elif is_freeze:
                in_window = True
                win_start_idx = i
                win_start = float(arrivals[i])
                win_gaps = 1
                win_total = float(g)
                win_has_burst = False
                win_severity = g - expected
        if in_window:
            _commit_window()

        result[client] = {"true_drops": true_drops,
                          "freeze_bursts": freeze_bursts,
                          "episodes": episodes}
    return result


def load_trial_data(base_dir: Path):
    data = {"next": [], "beta": []}
    trial_dirs = sorted(d for d in base_dir.iterdir()
                        if d.is_dir() and d.name.startswith("trial_"))
    for trial_dir in trial_dirs:
        for label in ("next", "beta"):
            run_dir = trial_dir / label
            if not run_dir.is_dir():
                continue
            summary_path = run_dir / "summary.json"
            if not summary_path.exists():
                continue
            seg_csvs = sorted(run_dir.glob("segment_*.csv"))
            if not seg_csvs:
                continue
            biggest_csv = max(seg_csvs, key=lambda p: p.stat().st_size)
            stats_path = run_dir / "stats.csv"
            camera_soc_path = run_dir / "camera_soc.ndjson"
            entry = {"trial": trial_dir.name,
                     "summary": json.loads(summary_path.read_text())}
            try:
                entry["csv"] = pd.read_csv(biggest_csv)
            except Exception:
                entry["csv"] = pd.DataFrame()
            if stats_path.exists():
                try:
                    entry["stats"] = pd.read_csv(stats_path)
                except Exception:
                    entry["stats"] = pd.DataFrame()
            else:
                entry["stats"] = pd.DataFrame()
            entry["camera_soc"] = _load_camera_soc(camera_soc_path)
            entry["event_records"] = _collect_event_records(entry["csv"])
            data[label].append(entry)
    return data


def balance_trial_counts(data):
    n = min(len(data[BASELINE]), len(data[IMPROVED]))
    data[BASELINE] = data[BASELINE][:n]
    data[IMPROVED] = data[IMPROVED][:n]
    return data


OUTLIER_MATCH_THRESHOLD = 80.0
OUTLIER_FRAME_RATIO = 0.5
OUTLIER_MIN_EFFECTIVE_FPS = 15.0
OUTLIER_MIN_DURATION_RATIO = 0.5
OUTLIER_MIN_STATS_ROWS = 60
# CAMERA_FIXED_BITRATE_MBPS = 32.768
CAMERA_FIXED_BITRATE_MBPS = 16.384
OUTLIER_MIN_RTSP_BITRATE_RATIO = 0.8
OUTLIER_MIN_RTSP_BITRATE_MBPS = CAMERA_FIXED_BITRATE_MBPS * OUTLIER_MIN_RTSP_BITRATE_RATIO


def outlier_reasons(entry, median_duration=None):
    """Return a list of reasons why an entry should be excluded."""
    reasons = []
    try:
        stats_df = entry.get("stats", pd.DataFrame())
        if len(stats_df) < OUTLIER_MIN_STATS_ROWS:
            reasons.append(f"stats<{OUTLIER_MIN_STATS_ROWS}")
        run = extract_run(entry)
        dur = run.get("duration_s", 0)
        if median_duration is not None and dur < median_duration * OUTLIER_MIN_DURATION_RATIO:
            reasons.append(f"duration<{median_duration * OUTLIER_MIN_DURATION_RATIO:.0f}s")
        p = run["pairs"][0]
        if p.get("match_pct", 100) < OUTLIER_MATCH_THRESHOLD:
            reasons.append(f"match<{OUTLIER_MATCH_THRESHOLD:.0f}%")
        wc = extract_client(entry, "webrtc-0")
        rc = extract_client(entry, "rtsp-0")
        if wc and wc.get("fps", 30) < 1:
            reasons.append("webrtc_fps<1")
        if rc and wc:
            rtsp_frames = rc.get("frames", 0)
            webrtc_frames = wc.get("frames", 0)
            if rtsp_frames > 0 and webrtc_frames / rtsp_frames < OUTLIER_FRAME_RATIO:
                reasons.append(f"webrtc_frames<{OUTLIER_FRAME_RATIO * 100:.0f}%_rtsp")
            rtsp_bitrate = rc.get("bitrate_mbps")
            if rtsp_bitrate is not None and rtsp_bitrate < OUTLIER_MIN_RTSP_BITRATE_MBPS:
                reasons.append(f"rtsp_bitrate<{OUTLIER_MIN_RTSP_BITRATE_MBPS:.1f}Mbps")
        if dur > 60:
            for cl in [rc, wc]:
                if cl and cl.get("frames", 0) / dur < OUTLIER_MIN_EFFECTIVE_FPS:
                    reasons.append(f"{cl['name']}_fps<{OUTLIER_MIN_EFFECTIVE_FPS:.0f}")
    except (KeyError, IndexError, TypeError):
        return ["parse_error"]
    return reasons


def is_outlier(entry, median_duration=None):
    """Flag entries with incomplete data, connection failures, data loss, or bad input bitrate."""
    return bool(outlier_reasons(entry, median_duration=median_duration))


def split_outliers(data):
    """Return (clean_data, outlier_set_of_indices, outlier_detail_list).

    A paired trial is flagged if *either* side is an outlier.
    Trials with duration < 50% of the median duration are considered incomplete.
    """
    all_durations = []
    for label in [BASELINE, IMPROVED]:
        for e in data[label]:
            try:
                all_durations.append(extract_run(e).get("duration_s", 0))
            except (KeyError, IndexError, TypeError):
                pass
    median_dur = float(np.median(all_durations)) if all_durations else None

    def _side_info(e):
        try:
            dur = extract_run(e).get("duration_s", 0)
            rtsp = extract_client(e, "rtsp-0") or {}
        except Exception:
            dur = 0
            rtsp = {}
        return dict(duration_s=dur,
                    stats_rows=len(e.get("stats", pd.DataFrame())),
                    rtsp_bitrate_mbps=rtsp.get("bitrate_mbps"))

    flagged = set()
    flagged_sides = {}
    for label in [BASELINE, IMPROVED]:
        for i, e in enumerate(data[label]):
            reasons = outlier_reasons(e, median_duration=median_dur)
            if reasons:
                flagged.add(i)
                flagged_sides.setdefault(i, {})[label] = reasons

    details = []
    for i in sorted(flagged):
        trial_name = data[BASELINE][i]["trial"] if i < len(data[BASELINE]) else f"trial_{i}"
        sides = flagged_sides[i]
        b_info = _side_info(data[BASELINE][i]) if i < len(data[BASELINE]) else {}
        i_info = _side_info(data[IMPROVED][i]) if i < len(data[IMPROVED]) else {}
        reason = []
        if BASELINE in sides:
            reason.append(
                f"{LABELS[BASELINE]}={b_info.get('duration_s',0):.0f}s/"
                f"{b_info.get('stats_rows',0)}rows/"
                f"{b_info.get('rtsp_bitrate_mbps',0):.1f}Mbps"
                f" ({', '.join(sides[BASELINE])})"
            )
        if IMPROVED in sides:
            reason.append(
                f"{LABELS[IMPROVED]}={i_info.get('duration_s',0):.0f}s/"
                f"{i_info.get('stats_rows',0)}rows/"
                f"{i_info.get('rtsp_bitrate_mbps',0):.1f}Mbps"
                f" ({', '.join(sides[IMPROVED])})"
            )
        details.append(dict(
            trial=trial_name,
            baseline_dur=b_info.get("duration_s", 0),
            baseline_stats=b_info.get("stats_rows", 0),
            improved_dur=i_info.get("duration_s", 0),
            improved_stats=i_info.get("stats_rows", 0),
            flagged_sides=", ".join(reason),
        ))
    clean = {BASELINE: [], IMPROVED: []}
    n = min(len(data[BASELINE]), len(data[IMPROVED]))
    for i in range(n):
        if i not in flagged:
            clean[BASELINE].append(data[BASELINE][i])
            clean[IMPROVED].append(data[IMPROVED][i])
    return clean, flagged, details

# ── Metric helpers ───────────────────────────────────────────────────────────

def extract_run(e): return e["summary"]["runs"][0]
def extract_pair(e): return extract_run(e)["pairs"][0]

def extract_client(e, name="rtsp-0"):
    for c in extract_run(e)["clients"]:
        if c["name"] == name:
            return c
    return None

def extract_freeze_bursts(e, client="rtsp-0"):
    c = extract_client(e, client)
    if c is None:
        return None
    return c["stutters"]["freeze_burst_events"]

def extract_fb_at_keyframe(e, client="rtsp-0"):
    c = extract_client(e, client)
    if c is None:
        return None
    return c["stutters"].get("freeze_burst_at_keyframe", 0)

def extract_fb_at_delta(e, client="rtsp-0"):
    c = extract_client(e, client)
    if c is None:
        return None
    return c["stutters"].get("freeze_burst_at_delta", 0)

def extract_disruption_episodes(e, client="rtsp-0"):
    c = extract_client(e, client)
    if c is None:
        return None
    return c["stutters"].get("disruption_episodes", 0)

def series(entries, fn):
    vals = []
    for e in entries:
        try:
            v = fn(e)
            if v is not None:
                vals.append(float(v))
        except (KeyError, IndexError, TypeError):
            pass
    return np.array(vals)

def pair(data, fn):
    return series(data[BASELINE], fn), series(data[IMPROVED], fn)

def mwu(a, b):
    if len(a) < 2 or len(b) < 2:
        return np.nan, np.nan
    try:
        u, p = sp_stats.mannwhitneyu(a, b, alternative="two-sided")
        r = 1 - (2 * u) / (len(a) * len(b))
        return p, r
    except Exception:
        return np.nan, np.nan

def sigmark(p):
    if np.isnan(p): return ""
    if p < 0.01: return "**"
    if p < 0.05: return "*"
    return ""

def siglabel(p):
    if np.isnan(p): return "n/a"
    if p < 0.01: return f"p={p:.3f}**"
    if p < 0.05: return f"p={p:.3f}*"
    return "n.s."

WARMUP_ROWS = 30

def stats_mean(entries, col):
    vals = []
    for e in entries:
        df = e.get("stats", pd.DataFrame())
        if col not in df.columns:
            continue
        s = pd.to_numeric(df[col][WARMUP_ROWS:], errors="coerce").dropna()
        if len(s) > 0:
            vals.append(s.mean())
    return np.array(vals)


CAMERA_WARMUP_ROWS = 15

def camera_soc_mean(entries, col):
    vals = []
    for e in entries:
        df = e.get("camera_soc", pd.DataFrame())
        if col not in df.columns:
            continue
        s = pd.to_numeric(df[col][CAMERA_WARMUP_ROWS:], errors="coerce").dropna()
        if len(s) > 0:
            vals.append(s.mean())
    return np.array(vals)

# ── Plot helpers ─────────────────────────────────────────────────────────────

def save_fig(fig, path):
    fig.savefig(path, bbox_inches="tight", pad_inches=0.15)
    plt.close(fig)


def place_legend(ax, ncol=2, **kw):
    """Place legend outside the axes, centred below the x-axis label."""
    kw.setdefault("fontsize", 10)
    kw.setdefault("framealpha", 0.9)
    kw.setdefault("edgecolor", "#cccccc")
    kw.setdefault("fancybox", True)
    ax.legend(loc="upper center", bbox_to_anchor=(0.5, -0.28),
              ncol=ncol, **kw)


def compat_boxplot(ax, data, tick_labels, **kwargs):
    """Use the Matplotlib label kwarg supported by the installed version."""
    try:
        return ax.boxplot(data, tick_labels=tick_labels, **kwargs)
    except TypeError:
        return ax.boxplot(data, labels=tick_labels, **kwargs)


def paired_boxplot(ax, bv, iv, ylabel, title):
    bp = compat_boxplot(ax, [bv, iv],
                        tick_labels=[LABELS[BASELINE] + "\n(baseline)",
                                     LABELS[IMPROVED] + "\n(new)"],
                        patch_artist=True, widths=0.45,
                        medianprops=dict(color=DARK, linewidth=1.5))
    bp["boxes"][0].set_facecolor(BASELINE_COLOR); bp["boxes"][0].set_alpha(0.6)
    bp["boxes"][1].set_facecolor(IMPROVED_COLOR); bp["boxes"][1].set_alpha(0.6)
    ax.set_ylabel(ylabel); ax.set_title(title)
    p, r = mwu(bv, iv)
    if not np.isnan(p):
        ax.text(0.97, 0.95, f"p={p:.4f}{sigmark(p)}  r={r:.2f}",
                transform=ax.transAxes, ha="right", va="top", fontsize=10,
                color="#333333")


def grouped_bar(ax, labels, bm, bs, im, ist, ylabel, title,
                b_series=None, i_series=None):
    x = np.arange(len(labels))
    w = 0.32
    ax.bar(x - w/2, bm, w, yerr=bs, color=BASELINE_COLOR, alpha=0.7,
           capsize=3, edgecolor="white", label=f"{LABELS[BASELINE]} (baseline)",
           error_kw=dict(lw=0.8))
    ax.bar(x + w/2, im, w, yerr=ist, color=IMPROVED_COLOR, alpha=0.7,
           capsize=3, edgecolor="white", label=f"{LABELS[IMPROVED]} (new)",
           error_kw=dict(lw=0.8))
    if b_series and i_series:
        max_top = 0
        tops = []
        for i in range(len(labels)):
            t = max(bm[i] + bs[i], im[i] + ist[i])
            tops.append(t); max_top = max(max_top, t)
        gap = max_top * 0.08
        for i in range(len(labels)):
            p, _ = mwu(b_series[i], i_series[i])
            ax.text(x[i], tops[i] + gap, siglabel(p), ha="center",
                    va="bottom", fontsize=9, color="#333333")
        ax.set_ylim(top=max_top + gap * 4)
    ax.set_xticks(x); ax.set_xticklabels(labels)
    ax.set_ylabel(ylabel); ax.set_title(title)
    place_legend(ax)


def quad_boxplot(ax, bv1, iv1, bv2, iv2, labels_pair, ylabel, title):
    bp = compat_boxplot(ax, [bv1, iv1, bv2, iv2],
                        tick_labels=[f"{labels_pair[0]}\n{LABELS[BASELINE]}",
                                     f"{labels_pair[0]}\n{LABELS[IMPROVED]}",
                                     f"{labels_pair[1]}\n{LABELS[BASELINE]}",
                                     f"{labels_pair[1]}\n{LABELS[IMPROVED]}"],
                        patch_artist=True, widths=0.45,
                        medianprops=dict(color=DARK, linewidth=1.5))
    for i, c in enumerate([BASELINE_COLOR, IMPROVED_COLOR,
                           BASELINE_COLOR, IMPROVED_COLOR]):
        bp["boxes"][i].set_facecolor(c); bp["boxes"][i].set_alpha(0.6)
    ax.set_ylabel(ylabel); ax.set_title(title)

# ── Generate all plot figures ────────────────────────────────────────────────

def gen_plots(data, data_all, outlier_indices, out_dir: Path):
    """Generate all plot PDFs.

    *data* is the clean (outlier-free) dataset used for statistical plots.
    *data_all* is the unfiltered dataset used for per-trial evolution plots.
    *outlier_indices* marks which trial indices (0-based) in data_all are outliers.
    """
    plots = {}

    # -- bar_latency (2x2) --
    fig, axes = plt.subplots(2, 2, figsize=(PLOT_W, PLOT_H))
    fig.subplots_adjust(hspace=0.75, wspace=0.35)

    ax = axes[0, 0]
    lbl = ["Mean", "P50", "P95", "P99"]
    keys = ["delta_mean_us", "delta_p50_us", "delta_p95_us", "delta_p99_us"]
    bm, bst, im_, ist_, bsl, isl = [], [], [], [], [], []
    for k in keys:
        b, i = pair(data, lambda e, k=k: extract_pair(e)[k] / 1000)
        bm.append(np.mean(b)); bst.append(np.std(b))
        im_.append(np.mean(i)); ist_.append(np.std(i))
        bsl.append(b); isl.append(i)
    grouped_bar(ax, lbl, bm, bst, im_, ist_, "ms",
                r"Pairwise Latency (RTSP$\rightarrow$WebRTC) [$\downarrow$ lower is better]", bsl, isl)

    ax = axes[0, 1]
    b1, i1 = pair(data, lambda e: extract_pair(e)["delta_stddev_us"] / 1000)
    b2, i2 = pair(data, lambda e: extract_pair(e)["match_pct"])
    grouped_bar(ax, ["Stddev (ms)", "Match %"],
                [np.mean(b1), np.mean(b2)], [np.std(b1), np.std(b2)],
                [np.mean(i1), np.mean(i2)], [np.std(i1), np.std(i2)],
                "", r"Latency Variability [$\downarrow$] & Frame Match [$\uparrow$]", [b1, b2], [i1, i2])

    ax = axes[1, 0]
    brj, irj = pair(data, lambda e: extract_client(e, "rtsp-0")["jitter_stddev_us"] / 1000)
    bwj, iwj = pair(data, lambda e: extract_client(e, "webrtc-0")["jitter_stddev_us"] / 1000)
    grouped_bar(ax, ["RTSP", "WebRTC"],
                [np.mean(brj), np.mean(bwj)], [np.std(brj), np.std(bwj)],
                [np.mean(irj), np.mean(iwj)], [np.std(irj), np.std(iwj)],
                "ms", r"Jitter [$\downarrow$ lower is better]",
                [brj, bwj], [irj, iwj])

    ax = axes[1, 1]
    brf, irf = pair(data, lambda e: extract_client(e, "rtsp-0")["fps"])
    bwf, iwf = pair(data, lambda e: extract_client(e, "webrtc-0")["fps"])
    grouped_bar(ax, ["RTSP", "WebRTC"],
                [np.mean(brf), np.mean(bwf)], [np.std(brf), np.std(bwf)],
                [np.mean(irf), np.mean(iwf)], [np.std(irf), np.std(iwf)],
                "FPS", r"Frames Per Second [$\uparrow$ higher is better]",
                [brf, bwf], [irf, iwf])
    p = out_dir / "bar_latency.pdf"
    save_fig(fig, p); plots["bar_latency"] = p

    # -- bar_delivery (2x2) --
    fig, axes = plt.subplots(2, 2, figsize=(PLOT_W, PLOT_H))
    fig.subplots_adjust(hspace=0.75, wspace=0.35)
    def _p(cl, k):
        return pair(data, lambda e, c=cl, k=k: extract_client(e, c)["stutters"][k])
    for idx, (key, title) in enumerate([
        ("true_drop_events", r"True Drop Events [$\downarrow$ lower is better]"),
        ("isolated_stutter_events", r"Isolated Stutter Events [$\downarrow$ lower is better]"),
        ("estimated_missed_frames", r"Missed Frames [$\downarrow$ lower is better]"),
    ]):
        ax = axes[idx // 2, idx % 2]
        br, ir = _p("rtsp-0", key); bw, iw = _p("webrtc-0", key)
        grouped_bar(ax, ["RTSP", "WebRTC"],
                    [np.mean(br), np.mean(bw)], [np.std(br), np.std(bw)],
                    [np.mean(ir), np.mean(iw)], [np.std(ir), np.std(iw)],
                    "Count", title, [br, bw], [ir, iw])
    ax = axes[1, 1]
    sm = [("Lat Mean", lambda e: extract_pair(e)["delta_mean_us"]/1000),
          ("Lat P50",  lambda e: extract_pair(e)["delta_p50_us"]/1000),
          ("Lat P95",  lambda e: extract_pair(e)["delta_p95_us"]/1000),
          ("Lat P99",  lambda e: extract_pair(e)["delta_p99_us"]/1000),
          ("Drops",    lambda e: extract_client(e,"webrtc-0")["stutters"]["true_drop_events"]),
          ("Missed",   lambda e: extract_client(e,"webrtc-0")["stutters"]["estimated_missed_frames"]),
          ("F-Burst",  lambda e: extract_freeze_bursts(e, "webrtc-0"))]
    deltas, colors, xl = [], [], []
    for lb, fn in sm:
        b, i = pair(data, fn)
        d = 100*(np.mean(i)-np.mean(b))/abs(np.mean(b)) if len(b)>0 and np.mean(b)!=0 else 0
        deltas.append(d); colors.append(GREEN if d < 0 else RED); xl.append(lb)
    x = np.arange(len(deltas))
    bars = ax.bar(x, deltas, color=colors, alpha=0.75, edgecolor="white")
    for bar, d in zip(bars, deltas):
        ax.text(bar.get_x()+bar.get_width()/2, bar.get_height(), f"{d:+.0f}%",
                ha="center", va="bottom" if d>=0 else "top", fontsize=10, fontweight="bold")
    ax.set_xticks(x); ax.set_xticklabels(xl, fontsize=9)
    ax.set_ylabel("Change from baseline (%)"); ax.set_title(r"Improvement Summary [$\downarrow$ negative = better]")
    ax.axhline(0, color=DARK, linewidth=0.8)
    p = out_dir / "bar_delivery.pdf"
    save_fig(fig, p); plots["bar_delivery"] = p

    # -- latency_dist (2x2) --
    fig, axes = plt.subplots(2, 2, figsize=(PLOT_W, PLOT_H))
    fig.subplots_adjust(hspace=0.75, wspace=0.35)
    bd, id_ = [], []
    for e in data[BASELINE]:
        df = e["csv"]
        if "rtsp-0_arrival_us" in df.columns and "webrtc-0_arrival_us" in df.columns:
            m = df.dropna(subset=["rtsp-0_arrival_us","webrtc-0_arrival_us"])
            bd.append((m["webrtc-0_arrival_us"]-m["rtsp-0_arrival_us"]).values/1000)
    for e in data[IMPROVED]:
        df = e["csv"]
        if "rtsp-0_arrival_us" in df.columns and "webrtc-0_arrival_us" in df.columns:
            m = df.dropna(subset=["rtsp-0_arrival_us","webrtc-0_arrival_us"])
            id_.append((m["webrtc-0_arrival_us"]-m["rtsp-0_arrival_us"]).values/1000)
    ab = np.concatenate(bd) if bd else np.array([])
    ai = np.concatenate(id_) if id_ else np.array([])
    ax = axes[0,0]
    if len(ab)>0 and len(ai)>0:
        bins = np.linspace(0, np.percentile(np.concatenate([ab,ai]),98), 80)
        ax.hist(ab, bins=bins, alpha=0.5, color=BASELINE_COLOR, label=f"{LABELS[BASELINE]} (baseline)", density=True)
        ax.hist(ai, bins=bins, alpha=0.5, color=IMPROVED_COLOR, label=f"{LABELS[IMPROVED]} (new)", density=True)
        place_legend(ax)
    ax.set_xlabel("Latency (ms)"); ax.set_ylabel("Density"); ax.set_title(r"Distribution, up to P98 [$\downarrow$ lower is better]")
    ax = axes[0,1]
    if len(ab)>0 and len(ai)>0:
        bf = np.linspace(0, max(np.max(ab),np.max(ai)),100)
        ax.hist(ab, bins=bf, alpha=0.5, color=BASELINE_COLOR, label=f"{LABELS[BASELINE]}", density=True)
        ax.hist(ai, bins=bf, alpha=0.5, color=IMPROVED_COLOR, label=f"{LABELS[IMPROVED]}", density=True)
        ax.set_yscale("log"); place_legend(ax)
    ax.set_xlabel("Latency (ms)"); ax.set_ylabel("Density (log)"); ax.set_title(r"Distribution, full range [$\downarrow$ lower is better]")
    for col,(name,key) in enumerate([(r"P50 Latency [$\downarrow$]","delta_p50_us"),(r"P95 Latency [$\downarrow$]","delta_p95_us")]):
        ax = axes[1,col]
        bv = series(data[BASELINE], lambda e,k=key: extract_pair(e)[k]/1000)
        iv = series(data[IMPROVED], lambda e,k=key: extract_pair(e)[k]/1000)
        paired_boxplot(ax, bv, iv, "ms", name)
    p = out_dir / "latency_dist.pdf"
    save_fig(fig, p); plots["latency_dist"] = p

    # -- fps_delivery (2x2) --
    fig, axes = plt.subplots(2, 2, figsize=(PLOT_W, PLOT_H))
    fig.subplots_adjust(hspace=0.75, wspace=0.35)
    ax = axes[0,0]
    br = series(data[BASELINE], lambda e: extract_client(e,"rtsp-0")["fps"])
    ir = series(data[IMPROVED], lambda e: extract_client(e,"rtsp-0")["fps"])
    bw = series(data[BASELINE], lambda e: extract_client(e,"webrtc-0")["fps"])
    iw = series(data[IMPROVED], lambda e: extract_client(e,"webrtc-0")["fps"])
    quad_boxplot(ax, br, ir, bw, iw, ["RTSP","WebRTC"], "FPS", r"Frames Per Second [$\uparrow$]")
    ax = axes[0,1]
    bm_ = series(data[BASELINE], lambda e: extract_pair(e)["match_pct"])
    im2 = series(data[IMPROVED], lambda e: extract_pair(e)["match_pct"])
    paired_boxplot(ax, bm_, im2, "%", r"Frame Match % [$\uparrow$]")
    ax = axes[1,0]
    bd_ = series(data[BASELINE], lambda e: extract_client(e,"webrtc-0")["stutters"]["true_drop_events"])
    id2 = series(data[IMPROVED], lambda e: extract_client(e,"webrtc-0")["stutters"]["true_drop_events"])
    paired_boxplot(ax, bd_, id2, "Count", r"WebRTC Drop Events [$\downarrow$]")
    ax = axes[1,1]
    bs_ = series(data[BASELINE], lambda e: extract_freeze_bursts(e, "webrtc-0"))
    is2 = series(data[IMPROVED], lambda e: extract_freeze_bursts(e, "webrtc-0"))
    paired_boxplot(ax, bs_, is2, "Count", r"WebRTC Freeze-Burst Events [$\downarrow$]")
    p = out_dir / "fps_delivery.pdf"
    save_fig(fig, p); plots["fps_delivery"] = p

    # -- jitter (2x2) --
    fig, axes = plt.subplots(2, 2, figsize=(PLOT_W, PLOT_H))
    fig.subplots_adjust(hspace=0.75, wspace=0.35)
    for col,(cl,lb) in enumerate([("rtsp-0","RTSP"),("webrtc-0","WebRTC")]):
        ax = axes[0,col]
        bj = series(data[BASELINE], lambda e,c=cl: extract_client(e,c)["jitter_stddev_us"]/1000)
        ij = series(data[IMPROVED], lambda e,c=cl: extract_client(e,c)["jitter_stddev_us"]/1000)
        paired_boxplot(ax, bj, ij, "ms", f"{lb} Jitter" + r" [$\downarrow$]")
    for col,(pc,key) in enumerate([("P95","inter_arrival_p95_us"),("P99","inter_arrival_p99_us")]):
        ax = axes[1,col]
        br_ = series(data[BASELINE], lambda e,k=key: extract_client(e,"rtsp-0")[k]/1000)
        ir_ = series(data[IMPROVED], lambda e,k=key: extract_client(e,"rtsp-0")[k]/1000)
        bw_ = series(data[BASELINE], lambda e,k=key: extract_client(e,"webrtc-0")[k]/1000)
        iw_ = series(data[IMPROVED], lambda e,k=key: extract_client(e,"webrtc-0")[k]/1000)
        quad_boxplot(ax, br_, ir_, bw_, iw_, ["RTSP","WebRTC"], "ms", f"Inter-Arrival {pc}" + r" [$\downarrow$]")
    p = out_dir / "jitter.pdf"
    save_fig(fig, p); plots["jitter"] = p

    # -- resources (2x2) --
    fig, axes = plt.subplots(2, 2, figsize=(PLOT_W, PLOT_H))
    fig.subplots_adjust(hspace=0.75, wspace=0.35)
    for idx,(col_,tit,u) in enumerate([("sys_cpu_pct",r"System CPU % [$\downarrow$]","%"),
                                        ("mcm_cpu_pct",r"MCM CPU % [$\downarrow$]","%"),
                                        ("mcm_rss_mb",r"MCM RSS [$\downarrow$]","MB"),
                                        ("cpu_temp_c",r"CPU Temperature [$\downarrow$]","°C")]):
        ax = axes[idx//2, idx%2]
        bv = stats_mean(data[BASELINE], col_)
        iv = stats_mean(data[IMPROVED], col_)
        paired_boxplot(ax, bv, iv, u, tit)
    p = out_dir / "resources.pdf"
    save_fig(fig, p); plots["resources"] = p

    # -- camera_resources (2x2) --
    fig, axes = plt.subplots(2, 2, figsize=(PLOT_W, PLOT_H))
    fig.subplots_adjust(hspace=0.75, wspace=0.35)
    for idx, (col_, tit, u) in enumerate([
        ("temp_c",           r"Camera SoC Temperature [$\downarrow$]", "°C"),
        ("cam_cpu_pct",      r"Camera CPU % [$\downarrow$]",           "%"),
        ("cam_mem_used_mb",  r"Camera Memory Used [$\downarrow$]",     "MB"),
        ("cam_tx_rate_mbps", r"Camera TX Rate",                        "Mbps"),
    ]):
        ax = axes[idx // 2, idx % 2]
        bv = camera_soc_mean(data[BASELINE], col_)
        iv = camera_soc_mean(data[IMPROVED], col_)
        paired_boxplot(ax, bv, iv, u, tit)
    p = out_dir / "camera_resources.pdf"
    save_fig(fig, p); plots["camera_resources"] = p

    # -- camera_voltages (2x2) --
    fig, axes = plt.subplots(2, 2, figsize=(PLOT_W, PLOT_H))
    fig.subplots_adjust(hspace=0.75, wspace=0.35)
    for idx, (col_, tit, u) in enumerate([
        ("core_volt", r"Camera Core Voltage", "mV"),
        ("cpu_volt",  r"Camera CPU Voltage",  "mV"),
        ("npu_volt",  r"Camera NPU Voltage",  "mV"),
        ("cam_rx_rate_mbps", r"Camera RX Rate", "Mbps"),
    ]):
        ax = axes[idx // 2, idx % 2]
        bv = camera_soc_mean(data[BASELINE], col_)
        iv = camera_soc_mean(data[IMPROVED], col_)
        paired_boxplot(ax, bv, iv, u, tit)
    p = out_dir / "camera_voltages.pdf"
    save_fig(fig, p); plots["camera_voltages"] = p

    # -- camera_thermal (2x2) --
    fig, axes = plt.subplots(2, 2, figsize=(PLOT_W, PLOT_H))
    fig.subplots_adjust(hspace=0.75, wspace=0.35)
    for idx, (col_, tit, u) in enumerate([
        ("core_temp_comp", r"Core Temp Compensation", ""),
        ("cpu_temp_comp",  r"CPU Temp Compensation",  ""),
        ("npu_temp_comp",  r"NPU Temp Compensation",  ""),
    ]):
        ax = axes[idx // 2, idx % 2]
        bv = camera_soc_mean(data[BASELINE], col_)
        iv = camera_soc_mean(data[IMPROVED], col_)
        paired_boxplot(ax, bv, iv, u, tit)
    axes[1, 1].set_visible(False)
    p = out_dir / "camera_thermal.pdf"
    save_fig(fig, p); plots["camera_thermal"] = p

    # -- timeseries (3x1) – averaged across all trials --
    GRID_S = np.arange(0, 901)  # 0..900 seconds at 1-second resolution

    def avg_stats_col(entries, col):
        """Resample each trial's stats column onto GRID_S, return mean ± std."""
        aligned = []
        for e in entries:
            df = e.get("stats", pd.DataFrame())
            if "timestamp" not in df.columns or col not in df.columns:
                continue
            ts = pd.to_numeric(df["timestamp"], errors="coerce")
            vals = pd.to_numeric(df[col], errors="coerce")
            mask = ts.notna() & vals.notna()
            ts, vals = ts[mask].values, vals[mask].values
            if len(ts) < 10:
                continue
            ts = ts - ts[0]
            resampled = np.interp(GRID_S, ts, vals, left=np.nan, right=np.nan)
            aligned.append(resampled)
        if not aligned:
            return None, None, None
        mat = np.array(aligned)
        mean = np.nanmean(mat, axis=0)
        std = np.nanstd(mat, axis=0)
        valid = np.sum(~np.isnan(mat), axis=0) >= 2
        mean[~valid] = np.nan
        std[~valid] = np.nan
        return GRID_S, mean, std

    def avg_latency(entries, bin_width=5):
        """Bin per-frame latency into time buckets, return mean ± std per bin."""
        all_t, all_d = [], []
        for e in entries:
            df = e.get("csv", pd.DataFrame())
            if "rtsp-0_arrival_us" not in df.columns or "webrtc-0_arrival_us" not in df.columns:
                continue
            m = df.dropna(subset=["rtsp-0_arrival_us", "webrtc-0_arrival_us"])
            if len(m) == 0:
                continue
            ra = m["rtsp-0_arrival_us"].values
            t = (ra - ra.min()) / 1e6
            d = (m["webrtc-0_arrival_us"].values - ra) / 1000
            all_t.append(t)
            all_d.append(d)
        if not all_t:
            return None, None, None
        cat_t = np.concatenate(all_t)
        cat_d = np.concatenate(all_d)
        bins = np.arange(0, cat_t.max() + bin_width, bin_width)
        idx = np.digitize(cat_t, bins) - 1
        bin_centers, means, stds = [], [], []
        for i in range(len(bins) - 1):
            mask = idx == i
            if mask.sum() < 5:
                continue
            bin_centers.append((bins[i] + bins[i + 1]) / 2)
            means.append(np.mean(cat_d[mask]))
            stds.append(np.std(cat_d[mask]))
        return np.array(bin_centers), np.array(means), np.array(stds)

    fig, axes = plt.subplots(3, 1, figsize=(PLOT_W3, PLOT_H3))
    fig.subplots_adjust(hspace=0.7)

    for lb, c in [(BASELINE, BASELINE_COLOR), (IMPROVED, IMPROVED_COLOR)]:
        t, mean, std = avg_stats_col(data[lb], "mcm_cpu_pct")
        if t is not None:
            axes[0].plot(t, mean, color=c, label=f"{LABELS[lb]}", linewidth=1)
            axes[0].fill_between(t, mean - std, mean + std, alpha=0.15, color=c)
    axes[0].set_ylabel("CPU %"); axes[0].set_title(r"MCM CPU Usage Over Time [$\downarrow$] (mean ± std)")
    axes[0].set_xlim(0, 900)
    place_legend(axes[0])

    for lb, c in [(BASELINE, BASELINE_COLOR), (IMPROVED, IMPROVED_COLOR)]:
        t, mean, std = avg_stats_col(data[lb], "cpu_temp_c")
        if t is not None:
            axes[1].plot(t, mean, color=c, label=f"{LABELS[lb]}", linewidth=1)
            axes[1].fill_between(t, mean - std, mean + std, alpha=0.15, color=c)
    axes[1].set_ylabel("Temperature (°C)"); axes[1].set_title(r"CPU Temperature Over Time [$\downarrow$] (mean ± std)")
    axes[1].set_xlim(0, 900)
    place_legend(axes[1])

    for lb, c in [(BASELINE, BASELINE_COLOR), (IMPROVED, IMPROVED_COLOR)]:
        t, mean, std = avg_latency(data[lb])
        if t is not None:
            axes[2].plot(t, mean, color=c, label=f"{LABELS[lb]} mean", linewidth=1)
            axes[2].fill_between(t, np.maximum(mean - std, 0), mean + std,
                                 alpha=0.12, color=c, label=f"{LABELS[lb]} ± std")
    axes[2].set_xlabel("Time (s)"); axes[2].set_ylabel("Latency (ms)")
    axes[2].set_title(r"Per-Frame Pairwise Latency Over Time [$\downarrow$] (mean ± std, 5 s bins)")
    axes[2].set_xlim(0, 900)
    place_legend(axes[2])
    p = out_dir / "timeseries.pdf"
    save_fig(fig, p); plots["timeseries"] = p

    # -- camera_timeseries (3x1) – camera SoC metrics over time --
    GRID_CAM = np.arange(0, 901, 2)  # ~2s sampling → 2s grid

    def avg_camera_soc_col(entries, col):
        """Resample each trial's camera_soc column onto GRID_CAM, return mean ± std."""
        aligned = []
        for e in entries:
            df = e.get("camera_soc", pd.DataFrame())
            if "ts" not in df.columns or col not in df.columns:
                continue
            ts = pd.to_numeric(df["ts"], errors="coerce")
            vals = pd.to_numeric(df[col], errors="coerce")
            mask = ts.notna() & vals.notna()
            ts, vals = ts[mask].values, vals[mask].values
            if len(ts) < 10:
                continue
            ts = ts - ts[0]
            resampled = np.interp(GRID_CAM, ts, vals, left=np.nan, right=np.nan)
            aligned.append(resampled)
        if not aligned:
            return None, None, None
        mat = np.array(aligned)
        mean = np.nanmean(mat, axis=0)
        std = np.nanstd(mat, axis=0)
        valid = np.sum(~np.isnan(mat), axis=0) >= 2
        mean[~valid] = np.nan; std[~valid] = np.nan
        return GRID_CAM, mean, std

    fig, axes = plt.subplots(3, 1, figsize=(PLOT_W3, PLOT_H3))
    fig.subplots_adjust(hspace=0.7)

    for lb, c in [(BASELINE, BASELINE_COLOR), (IMPROVED, IMPROVED_COLOR)]:
        t, mean, std = avg_camera_soc_col(data[lb], "temp_c")
        if t is not None:
            axes[0].plot(t, mean, color=c, label=f"{LABELS[lb]}", linewidth=1)
            axes[0].fill_between(t, mean - std, mean + std, alpha=0.15, color=c)
    axes[0].set_ylabel("°C"); axes[0].set_title(r"Camera SoC Temperature Over Time [$\downarrow$] (mean ± std)")
    axes[0].set_xlim(0, 900); place_legend(axes[0])

    for lb, c in [(BASELINE, BASELINE_COLOR), (IMPROVED, IMPROVED_COLOR)]:
        t, mean, std = avg_camera_soc_col(data[lb], "cam_cpu_pct")
        if t is not None:
            axes[1].plot(t, mean, color=c, label=f"{LABELS[lb]}", linewidth=1)
            axes[1].fill_between(t, mean - std, mean + std, alpha=0.15, color=c)
    axes[1].set_ylabel("CPU %"); axes[1].set_title(r"Camera CPU Usage Over Time [$\downarrow$] (mean ± std)")
    axes[1].set_xlim(0, 900); place_legend(axes[1])

    for lb, c in [(BASELINE, BASELINE_COLOR), (IMPROVED, IMPROVED_COLOR)]:
        t, mean, std = avg_camera_soc_col(data[lb], "cam_tx_rate_mbps")
        if t is not None:
            axes[2].plot(t, mean, color=c, label=f"{LABELS[lb]}", linewidth=1)
            axes[2].fill_between(t, mean - std, mean + std, alpha=0.15, color=c)
    axes[2].set_xlabel("Time (s)"); axes[2].set_ylabel("Mbps")
    axes[2].set_title(r"Camera TX Bitrate Over Time (mean ± std)")
    axes[2].set_xlim(0, 900); place_legend(axes[2])
    p = out_dir / "camera_timeseries.pdf"
    save_fig(fig, p); plots["camera_timeseries"] = p

    # -- camera_voltage_ts (3x1) – camera voltages over time --
    fig, axes = plt.subplots(3, 1, figsize=(PLOT_W3, PLOT_H3))
    fig.subplots_adjust(hspace=0.7)
    for ax_idx, (col_, ylabel, title) in enumerate([
        ("core_volt", "mV", r"Camera Core Voltage Over Time (mean ± std)"),
        ("cpu_volt",  "mV", r"Camera CPU Voltage Over Time (mean ± std)"),
        ("npu_volt",  "mV", r"Camera NPU Voltage Over Time (mean ± std)"),
    ]):
        for lb, c in [(BASELINE, BASELINE_COLOR), (IMPROVED, IMPROVED_COLOR)]:
            t, mean, std = avg_camera_soc_col(data[lb], col_)
            if t is not None:
                axes[ax_idx].plot(t, mean, color=c, label=f"{LABELS[lb]}", linewidth=1)
                axes[ax_idx].fill_between(t, mean - std, mean + std, alpha=0.15, color=c)
        axes[ax_idx].set_ylabel(ylabel); axes[ax_idx].set_title(title)
        axes[ax_idx].set_xlim(0, 900); place_legend(axes[ax_idx])
    axes[2].set_xlabel("Time (s)")
    p = out_dir / "camera_voltage_ts.pdf"
    save_fig(fig, p); plots["camera_voltage_ts"] = p

    # -- per-trial evolution pages (2x2 each) – uses data_all --
    def trial_plot(ax, title, ylabel, extractor):
        """Plot a single metric per trial for both groups (all data).

        Outlier trials are drawn with red 'X' markers so they are visible
        but clearly distinguished from healthy trials.
        """
        for lb, c in [(BASELINE, BASELINE_COLOR), (IMPROVED, IMPROVED_COLOR)]:
            ent = data_all[lb]
            tr = list(range(1, len(ent) + 1))
            vals = []
            for e in ent:
                try:
                    v = extractor(e)
                    vals.append(float(v) if v is not None else np.nan)
                except (KeyError, IndexError, TypeError):
                    vals.append(np.nan)
            ax.plot(tr, vals, "o-", color=c, label=f"{LABELS[lb]}",
                    markersize=5, linewidth=1.2)
            if outlier_indices:
                ox = [tr[i] for i in outlier_indices if i < len(tr)]
                oy = [vals[i] for i in outlier_indices if i < len(vals)]
                if ox:
                    ax.scatter(ox, oy, marker="X", s=90, color=RED,
                               zorder=5, edgecolors="white", linewidths=0.5)
        if outlier_indices:
            ax.scatter([], [], marker="X", s=60, color=RED, label="outlier")
        ax.set_xlabel("Trial #")
        ax.set_ylabel(ylabel)
        ax.set_title(title)
        ax.xaxis.set_major_locator(plt.MaxNLocator(integer=True))
        place_legend(ax, ncol=3)

    D = r" [$\downarrow$]"
    U = r" [$\uparrow$]"
    EVOLUTION_PAGES = [
        ("evo_latency", [
            ("Latency Mean" + D, "ms", lambda e: extract_pair(e)["delta_mean_us"] / 1000),
            ("Latency P50" + D, "ms", lambda e: extract_pair(e)["delta_p50_us"] / 1000),
            ("Latency P95" + D, "ms", lambda e: extract_pair(e)["delta_p95_us"] / 1000),
            ("Latency P99" + D, "ms", lambda e: extract_pair(e)["delta_p99_us"] / 1000),
        ]),
        ("evo_quality", [
            ("Latency Stddev" + D, "ms", lambda e: extract_pair(e)["delta_stddev_us"] / 1000),
            ("Frame Match %" + U, "%", lambda e: extract_pair(e)["match_pct"]),
            ("RTSP FPS" + U, "FPS", lambda e: extract_client(e, "rtsp-0")["fps"]),
            ("WebRTC FPS" + U, "FPS", lambda e: extract_client(e, "webrtc-0")["fps"]),
        ]),
        ("evo_jitter", [
            ("RTSP Jitter" + D, "ms", lambda e: extract_client(e, "rtsp-0")["jitter_stddev_us"] / 1000),
            ("WebRTC Jitter" + D, "ms", lambda e: extract_client(e, "webrtc-0")["jitter_stddev_us"] / 1000),
            ("RTSP Inter-Arrival P95" + D, "ms", lambda e: extract_client(e, "rtsp-0")["inter_arrival_p95_us"] / 1000),
            ("WebRTC Inter-Arrival P95" + D, "ms", lambda e: extract_client(e, "webrtc-0")["inter_arrival_p95_us"] / 1000),
        ]),
        ("evo_drops", [
            ("RTSP True Drops" + D, "count", lambda e: extract_client(e, "rtsp-0")["stutters"]["true_drop_events"]),
            ("WebRTC True Drops" + D, "count", lambda e: extract_client(e, "webrtc-0")["stutters"]["true_drop_events"]),
            ("RTSP Isolated Stutters" + D, "count", lambda e: extract_client(e, "rtsp-0")["stutters"]["isolated_stutter_events"]),
            ("WebRTC Isolated Stutters" + D, "count", lambda e: extract_client(e, "webrtc-0")["stutters"]["isolated_stutter_events"]),
        ]),
        ("evo_missed", [
            ("RTSP Missed Frames" + D, "count", lambda e: extract_client(e, "rtsp-0")["stutters"]["estimated_missed_frames"]),
            ("WebRTC Missed Frames" + D, "count", lambda e: extract_client(e, "webrtc-0")["stutters"]["estimated_missed_frames"]),
            ("RTSP Freeze-Bursts" + D, "count", lambda e: extract_freeze_bursts(e, "rtsp-0")),
            ("WebRTC Freeze-Bursts" + D, "count", lambda e: extract_freeze_bursts(e, "webrtc-0")),
        ]),
        ("evo_fb_attribution", [
            ("WebRTC FB@Keyframe" + D, "count", lambda e: extract_fb_at_keyframe(e, "webrtc-0")),
            ("WebRTC FB@Delta" + D, "count", lambda e: extract_fb_at_delta(e, "webrtc-0")),
            ("WebRTC Episodes" + D, "count", lambda e: extract_disruption_episodes(e, "webrtc-0")),
            ("RTSP Episodes" + D, "count", lambda e: extract_disruption_episodes(e, "rtsp-0")),
        ]),
        ("evo_bitrate", [
            ("RTSP Bitrate" + D, "Mbps", lambda e: extract_client(e, "rtsp-0")["bitrate_mbps"]),
            ("WebRTC Bitrate" + D, "Mbps", lambda e: extract_client(e, "webrtc-0")["bitrate_mbps"]),
            ("RTSP Avg Frame Size" + D, "KB/frame", lambda e: extract_client(e, "rtsp-0")["avg_frame_kb"]),
            ("WebRTC Avg Frame Size" + D, "KB/frame", lambda e: extract_client(e, "webrtc-0")["avg_frame_kb"]),
        ]),
        ("evo_resources", [
            ("System CPU" + D, "%", lambda e: _stats_trial_mean(e, "sys_cpu_pct")),
            ("MCM CPU" + D, "%", lambda e: _stats_trial_mean(e, "mcm_cpu_pct")),
            ("MCM RSS" + D, "MB", lambda e: _stats_trial_mean(e, "mcm_rss_mb")),
            ("CPU Temperature" + D, "°C", lambda e: _stats_trial_mean(e, "cpu_temp_c")),
        ]),
        ("evo_camera", [
            ("Camera Temp" + D, "°C", lambda e: _cam_trial_mean(e, "temp_c")),
            ("Camera CPU" + D, "%", lambda e: _cam_trial_mean(e, "cam_cpu_pct")),
            ("Camera Memory" + D, "MB", lambda e: _cam_trial_mean(e, "cam_mem_used_mb")),
            ("Camera TX Rate", "Mbps", lambda e: _cam_trial_mean(e, "cam_tx_rate_mbps")),
        ]),
        ("evo_camera_voltage", [
            ("Camera Core Voltage", "mV", lambda e: _cam_trial_mean(e, "core_volt")),
            ("Camera CPU Voltage", "mV", lambda e: _cam_trial_mean(e, "cpu_volt")),
            ("Camera NPU Voltage", "mV", lambda e: _cam_trial_mean(e, "npu_volt")),
            ("Camera RX Rate", "Mbps", lambda e: _cam_trial_mean(e, "cam_rx_rate_mbps")),
        ]),
    ]

    for page_key, metric_list in EVOLUTION_PAGES:
        n_metrics = len(metric_list)
        nrows = (n_metrics + 1) // 2
        fig, axes = plt.subplots(nrows, 2, figsize=(PLOT_W, PLOT_H))
        fig.subplots_adjust(hspace=0.75, wspace=0.35)
        if nrows == 1:
            axes = axes.reshape(1, -1)
        for idx, (title, ylabel, fn) in enumerate(metric_list):
            ax = axes[idx // 2, idx % 2]
            trial_plot(ax, title, ylabel, fn)
        for idx in range(n_metrics, nrows * 2):
            axes[idx // 2, idx % 2].set_visible(False)
        p = out_dir / f"{page_key}.pdf"
        save_fig(fig, p)
        plots[page_key] = p

    # -- p-value convergence – uses data_all, one subplot per METRICS_DEF entry
    CONVERGENCE_METRICS = [(name.replace("\\%", "%"), fn) for name, fn, _ in METRICS_DEF]

    n_conv = len(CONVERGENCE_METRICS)
    ncols_c = 8
    nrows_c = (n_conv + ncols_c - 1) // ncols_c
    fig, axes = plt.subplots(nrows_c, ncols_c,
                             figsize=(PLOT_W, 1.2 * nrows_c),
                             sharex=True, sharey=True)
    fig.subplots_adjust(hspace=0.55, wspace=0.2)
    if nrows_c == 1:
        axes = axes.reshape(1, -1)

    n_all = min(len(data_all[BASELINE]), len(data_all[IMPROVED]))
    for idx, (metric_name, fn) in enumerate(CONVERGENCE_METRICS):
        r, c = idx // ncols_c, idx % ncols_c
        ax = axes[r, c]
        ns, pvals = [], []
        for k in range(2, n_all + 1):
            b = series(data_all[BASELINE][:k], fn)
            i = series(data_all[IMPROVED][:k], fn)
            if len(b) >= 2 and len(i) >= 2:
                p, _ = mwu(b, i)
                ns.append(k)
                pvals.append(p if not np.isnan(p) else 1.0)
        if ns:
            ax.plot(ns, pvals, "o-", color=IMPROVED_COLOR, markersize=3, linewidth=1)
            ax.axhline(0.05, color=ORANGE, linestyle="--", linewidth=1, alpha=0.8)
            ax.axhline(0.01, color=RED, linestyle="--", linewidth=1, alpha=0.8)
            ax.set_yscale("log")
            ax.set_ylim(1e-5, 1.5)
        ax.set_title(metric_name, fontsize=7, pad=3)
        ax.tick_params(labelsize=6)
        ax.xaxis.set_major_locator(plt.MaxNLocator(integer=True))
        if r == nrows_c - 1 or idx + ncols_c >= n_conv:
            ax.set_xlabel("n", fontsize=7)
        if c == 0:
            ax.set_ylabel("p-value", fontsize=7)
    for idx in range(n_conv, nrows_c * ncols_c):
        axes[idx // ncols_c, idx % ncols_c].set_visible(False)

    from matplotlib.lines import Line2D
    legend_handles = [
        Line2D([0], [0], color=IMPROVED_COLOR, marker="o", markersize=4,
               linewidth=1, label="p-value"),
        Line2D([0], [0], color=ORANGE, linestyle="--", linewidth=1,
               label="p = 0.05"),
        Line2D([0], [0], color=RED, linestyle="--", linewidth=1,
               label="p = 0.01"),
    ]
    fig.legend(handles=legend_handles, loc="lower center",
               ncol=3, fontsize=9, framealpha=0.9, edgecolor="#cccccc",
               bbox_to_anchor=(0.5, -0.01))

    p = out_dir / "pvalue_convergence.pdf"
    save_fig(fig, p)
    plots["pvalue_convergence"] = p

    # -- stutter duration & frequency distributions (per-client) ---------------
    def _gather_durations(entries, client, event_type, field="duration_us"):
        """Concatenate per-event durations (in ms) across all trials."""
        vals = []
        for e in entries:
            recs = e.get("event_records", {}).get(client, {}).get(event_type, [])
            vals.extend(r[field] / 1000.0 for r in recs)
        return np.array(vals)

    def _gather_inter_event(entries, client, event_type):
        """Concatenate inter-event intervals (in seconds) across all trials."""
        vals = []
        for e in entries:
            recs = e.get("event_records", {}).get(client, {}).get(event_type, [])
            if len(recs) < 2:
                continue
            ts = np.array([r["timestamp_us"] for r in recs])
            vals.extend(np.diff(ts) / 1e6)
        return np.array(vals)

    def _gather_normalized_timestamps(entries, client, event_type):
        """Event timestamps as percentage of trial runtime, pooled across trials."""
        vals = []
        for e in entries:
            dur_s = extract_run(e).get("duration_s", 0)
            if dur_s <= 0:
                continue
            recs = e.get("event_records", {}).get(client, {}).get(event_type, [])
            dur_us = dur_s * 1e6
            vals.extend(r["timestamp_us"] / dur_us * 100.0 for r in recs)
        return np.array(vals)

    def _ab_hist(ax, b_vals, i_vals, xlabel, ylabel, title, *, fixed_bins=None):
        if len(b_vals) > 0 or len(i_vals) > 0:
            if fixed_bins is not None:
                bins = fixed_bins
            else:
                all_v = np.concatenate([x for x in [b_vals, i_vals] if len(x) > 0])
                bins = np.linspace(0, np.percentile(all_v, 98), 50)
            if len(b_vals) > 0:
                ax.hist(b_vals, bins=bins, alpha=0.5, color=BASELINE_COLOR,
                        label=f"{LABELS[BASELINE]} (n={len(b_vals)})", density=True)
            if len(i_vals) > 0:
                ax.hist(i_vals, bins=bins, alpha=0.5, color=IMPROVED_COLOR,
                        label=f"{LABELS[IMPROVED]} (n={len(i_vals)})", density=True)
            place_legend(ax)
        ax.set_xlabel(xlabel); ax.set_ylabel(ylabel); ax.set_title(title)

    for client_id, client_label in [("webrtc-0", "WebRTC"), ("rtsp-0", "RTSP")]:
        # -- stutter_duration_dist (2x2) --------
        fig, axes = plt.subplots(2, 2, figsize=(PLOT_W, PLOT_H))
        fig.subplots_adjust(hspace=0.75, wspace=0.35)

        _ab_hist(axes[0, 0],
                 _gather_durations(data[BASELINE], client_id, "true_drops"),
                 _gather_durations(data[IMPROVED], client_id, "true_drops"),
                 "Duration (ms)", "Density",
                 rf"True Drop Duration ({client_label})" + r" [$\downarrow$]")

        _ab_hist(axes[0, 1],
                 _gather_durations(data[BASELINE], client_id, "freeze_bursts"),
                 _gather_durations(data[IMPROVED], client_id, "freeze_bursts"),
                 "Duration (ms)", "Density",
                 rf"Freeze-Burst Duration ({client_label})" + r" [$\downarrow$]")

        _ab_hist(axes[1, 0],
                 _gather_durations(data[BASELINE], client_id, "episodes"),
                 _gather_durations(data[IMPROVED], client_id, "episodes"),
                 "Duration (ms)", "Density",
                 rf"Disruption Episode Duration ({client_label})" + r" [$\downarrow$]")

        ax = axes[1, 1]
        for event_type, label, color in [
            ("true_drops", "True Drops", RED),
            ("freeze_bursts", "Freeze-Bursts", ORANGE),
            ("episodes", "Episodes", DARK),
        ]:
            all_vals = np.concatenate([
                _gather_durations(data[lb], client_id, event_type)
                for lb in [BASELINE, IMPROVED]
            ]) if any(len(_gather_durations(data[lb], client_id, event_type)) > 0
                      for lb in [BASELINE, IMPROVED]) else np.array([])
            if len(all_vals) > 0:
                sorted_v = np.sort(all_vals)
                cdf = np.arange(1, len(sorted_v) + 1) / len(sorted_v)
                ax.plot(sorted_v, cdf, label=label, color=color, linewidth=1.5)
        ax.set_xlabel("Duration (ms)"); ax.set_ylabel("CDF")
        ax.set_title(f"Duration CDF ({client_label}, all trials)")
        if ax.get_legend_handles_labels()[1]:
            place_legend(ax, ncol=3)

        tag = "webrtc" if client_id == "webrtc-0" else "rtsp"
        p = out_dir / f"stutter_duration_dist_{tag}.pdf"
        save_fig(fig, p); plots[f"stutter_duration_dist_{tag}"] = p

        # -- stutter_frequency_dist (2x2) --------
        fig, axes = plt.subplots(2, 2, figsize=(PLOT_W, PLOT_H))
        fig.subplots_adjust(hspace=0.75, wspace=0.35)

        _ab_hist(axes[0, 0],
                 _gather_inter_event(data[BASELINE], client_id, "true_drops"),
                 _gather_inter_event(data[IMPROVED], client_id, "true_drops"),
                 "Inter-event interval (s)", "Density",
                 rf"Time Between True Drops ({client_label})" + r" [$\uparrow$]")

        _ab_hist(axes[0, 1],
                 _gather_inter_event(data[BASELINE], client_id, "freeze_bursts"),
                 _gather_inter_event(data[IMPROVED], client_id, "freeze_bursts"),
                 "Inter-event interval (s)", "Density",
                 rf"Time Between Freeze-Bursts ({client_label})" + r" [$\uparrow$]")

        _ab_hist(axes[1, 0],
                 _gather_inter_event(data[BASELINE], client_id, "episodes"),
                 _gather_inter_event(data[IMPROVED], client_id, "episodes"),
                 "Inter-event interval (s)", "Density",
                 rf"Time Between Episodes ({client_label})" + r" [$\uparrow$]")

        ax = axes[1, 1]
        bp_data, bp_labels = [], []
        for event_type, short_lbl in [("true_drops", "Drops"), ("freeze_bursts", "FB"),
                                       ("episodes", "Eps")]:
            for lb in [BASELINE, IMPROVED]:
                vals = _gather_inter_event(data[lb], client_id, event_type)
                if len(vals) > 0:
                    bp_data.append(vals)
                    bp_labels.append(f"{short_lbl}\n{LABELS[lb]}")
        if bp_data:
            bp = compat_boxplot(ax, bp_data, tick_labels=bp_labels,
                                patch_artist=True, widths=0.5,
                                medianprops=dict(color=DARK, linewidth=1.5))
            colors_cycle = [BASELINE_COLOR, IMPROVED_COLOR] * 3
            for i, box in enumerate(bp["boxes"]):
                box.set_facecolor(colors_cycle[i]); box.set_alpha(0.6)
        ax.set_ylabel("Inter-event interval (s)")
        ax.set_title(rf"Inter-Event Intervals ({client_label})" + r" [$\uparrow$]")

        p = out_dir / f"stutter_frequency_dist_{tag}.pdf"
        save_fig(fig, p); plots[f"stutter_frequency_dist_{tag}"] = p

        # -- stutter_temporal_dist (2x2) --------
        temporal_bins = np.linspace(0, 100, 21)  # 20 bins, each 5% of runtime

        fig, axes = plt.subplots(2, 2, figsize=(PLOT_W, PLOT_H))
        fig.subplots_adjust(hspace=0.75, wspace=0.35)

        _ab_hist(axes[0, 0],
                 _gather_normalized_timestamps(data[BASELINE], client_id, "true_drops"),
                 _gather_normalized_timestamps(data[IMPROVED], client_id, "true_drops"),
                 "Runtime position (%)", "Density",
                 f"True Drop Timing ({client_label})",
                 fixed_bins=temporal_bins)

        _ab_hist(axes[0, 1],
                 _gather_normalized_timestamps(data[BASELINE], client_id, "freeze_bursts"),
                 _gather_normalized_timestamps(data[IMPROVED], client_id, "freeze_bursts"),
                 "Runtime position (%)", "Density",
                 f"Freeze-Burst Timing ({client_label})",
                 fixed_bins=temporal_bins)

        _ab_hist(axes[1, 0],
                 _gather_normalized_timestamps(data[BASELINE], client_id, "episodes"),
                 _gather_normalized_timestamps(data[IMPROVED], client_id, "episodes"),
                 "Runtime position (%)", "Density",
                 f"Episode Timing ({client_label})",
                 fixed_bins=temporal_bins)

        ax = axes[1, 1]
        density_bins = np.linspace(0, 100, 41)  # finer bins for smooth density lines
        midpoints = (density_bins[:-1] + density_bins[1:]) / 2.0
        for event_type, label, color in [
            ("true_drops", "True Drops", RED),
            ("freeze_bursts", "Freeze-Bursts", ORANGE),
            ("episodes", "Episodes", DARK),
        ]:
            all_vals = np.concatenate([
                _gather_normalized_timestamps(data[lb], client_id, event_type)
                for lb in [BASELINE, IMPROVED]
            ]) if any(len(_gather_normalized_timestamps(data[lb], client_id, event_type)) > 0
                      for lb in [BASELINE, IMPROVED]) else np.array([])
            if len(all_vals) > 0:
                counts, _ = np.histogram(all_vals, bins=density_bins, density=True)
                ax.plot(midpoints, counts, label=label, color=color, linewidth=1.5)
        ax.set_xlabel("Runtime position (%)"); ax.set_ylabel("Density")
        ax.set_title(f"Event Density ({client_label}, all trials)")
        if ax.get_legend_handles_labels()[1]:
            place_legend(ax, ncol=3)

        p = out_dir / f"stutter_temporal_dist_{tag}.pdf"
        save_fig(fig, p); plots[f"stutter_temporal_dist_{tag}"] = p

    # ── I-frame vs P-frame Latency Analysis (2x2) ───────────────────────────
    _gen_frame_type_latency(data, out_dir, plots)

    # ── RTP Fragmentation Analysis (2x2, if data available) ─────────────────
    _gen_rtp_fragmentation(data, out_dir, plots)

    # ── Temporal / Burst-Position Analysis (3x2, next build only) ─────────
    _gen_burst_position_analysis(data, out_dir, plots)

    # ── Bitrate Efficiency / NAL Composition Analysis (2x2) ──────────────
    _gen_bitrate_efficiency(data, out_dir, plots)

    return plots


def _compute_per_frame_latency(entries):
    """Return (kf_deltas_ms, pf_deltas_ms, frame_bytes, frame_deltas_ms, frame_is_kf)
    pooled across all entries."""
    kf_deltas, pf_deltas = [], []
    frame_bytes, frame_delta_ms, frame_kf = [], [], []
    for e in entries:
        df = e["csv"]
        r_col, w_col = "rtsp-0_arrival_us", "webrtc-0_arrival_us"
        r_kf_col, r_bytes_col = "rtsp-0_is_keyframe", "rtsp-0_bytes"
        if not all(c in df.columns for c in (r_col, w_col, r_kf_col, r_bytes_col)):
            continue
        m = df.dropna(subset=[r_col, w_col]).copy()
        if m.empty:
            continue
        delta_ms = (m[w_col] - m[r_col]).values / 1000.0
        valid = (delta_ms >= 0) & (delta_ms <= LATENCY_MAX_MS)
        delta_ms, m = delta_ms[valid], m.iloc[valid]
        if len(delta_ms) == 0:
            continue
        is_kf = np.array(m[r_kf_col].values.astype(int) == 1, dtype=bool, copy=True)
        pf_mask = is_kf == False
        kf_deltas.append(delta_ms[is_kf])
        pf_deltas.append(delta_ms[pf_mask])
        frame_bytes.append(m[r_bytes_col].values)
        frame_delta_ms.append(delta_ms)
        frame_kf.append(is_kf)
    kf_all = np.concatenate(kf_deltas) if kf_deltas else np.array([])
    pf_all = np.concatenate(pf_deltas) if pf_deltas else np.array([])
    fb = np.concatenate(frame_bytes) if frame_bytes else np.array([])
    fd = np.concatenate(frame_delta_ms) if frame_delta_ms else np.array([])
    fk = np.concatenate(frame_kf) if frame_kf else np.array([])
    return kf_all, pf_all, fb, fd, fk


def _scatter_size_vs_latency(ax, fb, fd, fk, build_label):
    """Plot frame size vs pairwise latency scatter for a single build.

    *fb*, *fd*, *fk* are pre-computed frame_bytes, frame_delta_ms, frame_is_kf
    arrays from ``_compute_per_frame_latency``.
    """
    if len(fb) == 0:
        return
    fk = np.array(fk, dtype=bool, copy=True)
    lat_cap = np.percentile(fd, 99)
    mask = fd <= lat_cap
    fb_m, fd_m, fk_m = fb[mask], fd[mask], fk[mask]
    pf_m = fk_m == False
    ax.scatter(fb_m[pf_m] / 1024, fd_m[pf_m], s=1, alpha=0.15,
               color=BLUE, label="P-frames", rasterized=True)
    ax.scatter(fb_m[fk_m] / 1024, fd_m[fk_m], s=3, alpha=0.3,
               color=RED, label="I-frames", rasterized=True)
    place_legend(ax)
    if np.std(fb_m) > 0:
        r = np.corrcoef(fb_m, fd_m)[0, 1]
        ax.text(0.97, 0.95, f"r={r:.3f}", transform=ax.transAxes,
                ha="right", va="top", fontsize=10, color="#333333")
    ax.set_xlabel("Frame size (KB)")
    ax.set_ylabel("Latency (ms)")
    ax.set_title(f"Frame Size vs Latency ({build_label})")


def _gen_frame_type_latency(data, out_dir, plots):
    """I-frame vs P-frame latency chart page (3 rows x 2 cols)."""
    kf_b, pf_b, fb_b, fd_b, fk_b = _compute_per_frame_latency(data[BASELINE])
    kf_i, pf_i, fb_i, fd_i, fk_i = _compute_per_frame_latency(data[IMPROVED])

    if len(kf_b) == 0 and len(kf_i) == 0:
        return

    fig, axes = plt.subplots(3, 2, figsize=(PLOT_W, PLOT_H3))
    fig.subplots_adjust(hspace=0.75, wspace=0.35)

    # Row 0, col 0: histogram overlay I-frame vs P-frame latency (pooled)
    ax = axes[0, 0]
    kf_all = np.concatenate([kf_b, kf_i]) if len(kf_b) + len(kf_i) > 0 else np.array([])
    pf_all = np.concatenate([pf_b, pf_i]) if len(pf_b) + len(pf_i) > 0 else np.array([])
    if len(kf_all) > 0 and len(pf_all) > 0:
        hi = np.percentile(np.concatenate([kf_all, pf_all]), 98)
        bins = np.linspace(0, hi, 80)
        ax.hist(pf_all, bins=bins, alpha=0.5, color=BLUE, label=f"P-frames (n={len(pf_all):,})", density=True)
        ax.hist(kf_all, bins=bins, alpha=0.5, color=RED, label=f"I-frames (n={len(kf_all):,})", density=True)
        place_legend(ax)
    ax.set_xlabel("Latency (ms)")
    ax.set_ylabel("Density")
    ax.set_title(r"I-frame vs P-frame Latency (pooled, P98)")

    # Row 0, col 1: boxplot split by frame type and build
    ax = axes[0, 1]
    all_pooled = np.concatenate([a for a in (kf_b, kf_i, pf_b, pf_i) if len(a) > 0])
    y_cap = np.percentile(all_pooled, 99) if len(all_pooled) > 0 else 100
    box_data, box_labels = [], []
    for arr, lbl in [(kf_b, f"I-frame\n{LABELS[BASELINE]}"),
                     (kf_i, f"I-frame\n{LABELS[IMPROVED]}"),
                     (pf_b, f"P-frame\n{LABELS[BASELINE]}"),
                     (pf_i, f"P-frame\n{LABELS[IMPROVED]}")]:
        clamped = np.clip(arr, 0, y_cap) if len(arr) > 0 else np.array([0])
        box_data.append(clamped)
        box_labels.append(lbl)
    bp = compat_boxplot(ax, box_data, tick_labels=box_labels,
                        patch_artist=True, widths=0.45,
                        medianprops=dict(color=DARK, linewidth=1.5))
    for idx, c in enumerate([RED, RED, BLUE, BLUE]):
        bp["boxes"][idx].set_facecolor(c)
        bp["boxes"][idx].set_alpha(0.4 if idx % 2 == 0 else 0.7)
    ax.set_ylim(bottom=0, top=y_cap * 1.1)
    ax.set_ylabel("Latency (ms)")
    ax.set_title(r"Latency by Frame Type & Build (P99 cap)")

    # Row 1: per-build scatter plots (shared axes)
    ax_b = axes[1, 0]
    ax_i = axes[1, 1]
    _scatter_size_vs_latency(ax_b, fb_b, fd_b, fk_b, LABELS[BASELINE])
    _scatter_size_vs_latency(ax_i, fb_i, fd_i, fk_i, LABELS[IMPROVED])
    xmax = max(ax_b.get_xlim()[1], ax_i.get_xlim()[1])
    ymax = max(ax_b.get_ylim()[1], ax_i.get_ylim()[1])
    for a in (ax_b, ax_i):
        a.set_xlim(0, xmax)
        a.set_ylim(ax_b.get_ylim()[0], ymax)

    # Row 2, col 0-1 merged: bar chart mean/P50/P95 per frame type with error bars
    axes[2, 0].remove()
    axes[2, 1].remove()
    ax = fig.add_subplot(3, 2, (5, 6))
    labels = ["Mean", "P50", "P95"]
    per_type_data = {}
    for build_label, entries in [(BASELINE, data[BASELINE]), (IMPROVED, data[IMPROVED])]:
        for e in entries:
            pair = extract_pair(e)
            if pair is None:
                continue
            for ft in ("keyframe", "pframe"):
                key = (build_label, ft)
                if key not in per_type_data:
                    per_type_data[key] = {"mean": [], "p50": [], "p95": []}
                per_type_data[key]["mean"].append(pair.get(f"delta_{ft}_mean_us", 0) / 1000)
                per_type_data[key]["p50"].append(pair.get(f"delta_{ft}_p50_us", 0) / 1000)
                per_type_data[key]["p95"].append(pair.get(f"delta_{ft}_p95_us", 0) / 1000)

    x = np.arange(len(labels))
    w = 0.18
    offsets = [-1.5 * w, -0.5 * w, 0.5 * w, 1.5 * w]
    colors = [RED, RED, BLUE, BLUE]
    alphas = [0.5, 0.8, 0.5, 0.8]
    bar_labels = [f"I-frame {LABELS[BASELINE]}", f"I-frame {LABELS[IMPROVED]}",
                  f"P-frame {LABELS[BASELINE]}", f"P-frame {LABELS[IMPROVED]}"]
    bar_keys = [(BASELINE, "keyframe"), (IMPROVED, "keyframe"),
                (BASELINE, "pframe"), (IMPROVED, "pframe")]
    for idx, (bk, bl) in enumerate(zip(bar_keys, bar_labels)):
        d = per_type_data.get(bk, {"mean": [], "p50": [], "p95": []})
        means = [np.mean(d["mean"]) if d["mean"] else 0,
                 np.mean(d["p50"]) if d["p50"] else 0,
                 np.mean(d["p95"]) if d["p95"] else 0]
        stds = [np.std(d["mean"]) if d["mean"] else 0,
                np.std(d["p50"]) if d["p50"] else 0,
                np.std(d["p95"]) if d["p95"] else 0]
        ax.bar(x + offsets[idx], means, w, yerr=stds, color=colors[idx],
               alpha=alphas[idx], capsize=2, edgecolor="white", label=bl,
               error_kw=dict(lw=0.6))
    ax.set_xticks(x)
    ax.set_xticklabels(labels)
    ax.set_ylabel("Latency (ms)")
    ax.set_title(r"Per-Frame-Type Latency Stats [$\downarrow$]")
    place_legend(ax, ncol=4)

    p = out_dir / "frame_type_latency.pdf"
    save_fig(fig, p)
    plots["frame_type_latency"] = p


def _scatter_rtp_vs_latency(ax, rtp_arr, delta_arr, kf_arr, build_label, xlabel):
    """Scatter an RTP metric vs latency, colored by frame type, with Pearson r."""
    if len(rtp_arr) == 0:
        return
    kf = np.array(kf_arr, dtype=bool, copy=True)
    cap = np.percentile(delta_arr, 99)
    mask = delta_arr <= cap
    rtp_m, d_m, kf_m = rtp_arr[mask], delta_arr[mask], kf[mask]
    pf_m = kf_m == False
    ax.scatter(rtp_m[pf_m], d_m[pf_m], s=1, alpha=0.1,
               color=BLUE, label="P-frames", rasterized=True)
    ax.scatter(rtp_m[kf_m], d_m[kf_m], s=2, alpha=0.25,
               color=RED, label="I-frames", rasterized=True)
    place_legend(ax)
    if np.std(rtp_m.astype(float)) > 0:
        r = np.corrcoef(rtp_m.astype(float), d_m)[0, 1]
        ax.text(0.97, 0.95, f"r={r:.3f}", transform=ax.transAxes,
                ha="right", va="top", fontsize=10, color="#333333")
    ax.set_xlabel(xlabel)
    ax.set_ylabel("Latency (ms)")
    ax.set_title(f"{xlabel} vs Latency ({build_label})")


def _gen_rtp_fragmentation(data, out_dir, plots):
    """RTP fragmentation analysis chart page (3x2, per-build scatters)."""
    has_rtp = False
    for lb in (BASELINE, IMPROVED):
        for e in data[lb]:
            if "rtsp-0_rtp_packets" in e["csv"].columns:
                has_rtp = True
                break
        if has_rtp:
            break
    if not has_rtp:
        return

    # Gather per-build RTP data
    build_data = {}
    for lb in (BASELINE, IMPROVED):
        bd = dict(kf_pkts=[], pf_pkts=[], pkts=[], delta=[], kf=[],
                  span_kf_rtsp=[], span_pf_rtsp=[],
                  span_kf_webrtc=[], span_pf_webrtc=[],
                  span_rtsp=[], span_delta=[])
        for e in data[lb]:
            df = e["csv"]
            rpkt, rkf = "rtsp-0_rtp_packets", "rtsp-0_is_keyframe"
            rspan, wspan = "rtsp-0_rtp_span_us", "webrtc-0_rtp_span_us"
            r_col, w_col = "rtsp-0_arrival_us", "webrtc-0_arrival_us"
            if rpkt not in df.columns or rkf not in df.columns:
                continue
            m = df.dropna(subset=[r_col, w_col]).copy()
            if m.empty:
                continue
            delta_ms = (m[w_col] - m[r_col]).values / 1000.0
            valid = (delta_ms >= 0) & (delta_ms <= LATENCY_MAX_MS)
            delta_ms, m = delta_ms[valid], m.iloc[valid]
            if len(delta_ms) == 0:
                continue
            pkts = m[rpkt].values
            is_kf = np.array(m[rkf].values.astype(int) == 1, dtype=bool, copy=True)
            is_pf = is_kf == False
            bd["kf_pkts"].append(pkts[is_kf])
            bd["pf_pkts"].append(pkts[is_pf])
            bd["pkts"].append(pkts)
            bd["delta"].append(delta_ms)
            bd["kf"].append(is_kf)
            if rspan in m.columns:
                spans = m[rspan].values.astype(float)
                bd["span_kf_rtsp"].append(spans[is_kf])
                bd["span_pf_rtsp"].append(spans[is_pf])
                bd["span_rtsp"].append(spans)
                bd["span_delta"].append(delta_ms)
            if wspan in m.columns:
                wspans = m[wspan].values.astype(float)
                bd["span_kf_webrtc"].append(wspans[is_kf])
                bd["span_pf_webrtc"].append(wspans[is_pf])
        build_data[lb] = {k: np.concatenate(v) if v else np.array([])
                          for k, v in bd.items()}

    def _cat(key):
        """Concatenate a key across both builds."""
        arrs = [build_data[lb][key] for lb in (BASELINE, IMPROVED)
                if len(build_data[lb][key]) > 0]
        return np.concatenate(arrs) if arrs else np.array([])

    fig, axes = plt.subplots(3, 2, figsize=(PLOT_W, PLOT_H3))
    fig.subplots_adjust(hspace=0.75, wspace=0.35)

    # Row 0, col 0: histogram RTP packets per frame (pooled, structural)
    ax = axes[0, 0]
    kf_pkts, pf_pkts = _cat("kf_pkts"), _cat("pf_pkts")
    if len(kf_pkts) > 0 and len(pf_pkts) > 0:
        hi = np.percentile(np.concatenate([kf_pkts, pf_pkts]), 99)
        bins = np.arange(0, hi + 2, max(1, int(hi / 60)))
        ax.hist(pf_pkts, bins=bins, alpha=0.5, color=BLUE,
                label=f"P-frames (med={np.median(pf_pkts):.0f})", density=True)
        ax.hist(kf_pkts, bins=bins, alpha=0.5, color=RED,
                label=f"I-frames (med={np.median(kf_pkts):.0f})", density=True)
        place_legend(ax)
    ax.set_xlabel("RTP Packets per Frame")
    ax.set_ylabel("Density")
    ax.set_title("RTP Fragmentation by Frame Type")

    # Row 0, col 1: boxplot RTP span by frame type (pooled, structural)
    ax = axes[0, 1]
    sk_r, sp_r = _cat("span_kf_rtsp"), _cat("span_pf_rtsp")
    sk_w, sp_w = _cat("span_kf_webrtc"), _cat("span_pf_webrtc")
    box_data = [sk_r / 1000 if len(sk_r) > 0 else np.array([0]),
                sp_r / 1000 if len(sp_r) > 0 else np.array([0]),
                sk_w / 1000 if len(sk_w) > 0 else np.array([0]),
                sp_w / 1000 if len(sp_w) > 0 else np.array([0])]
    bp = compat_boxplot(ax, box_data,
                        tick_labels=["I-frame\nRTSP", "P-frame\nRTSP",
                                     "I-frame\nWebRTC", "P-frame\nWebRTC"],
                        patch_artist=True, widths=0.45,
                        medianprops=dict(color=DARK, linewidth=1.5))
    for idx, c in enumerate([RED, BLUE, RED, BLUE]):
        bp["boxes"][idx].set_facecolor(c)
        bp["boxes"][idx].set_alpha(0.6)
    ax.set_ylabel("RTP Span (ms)")
    ax.set_title(r"RTP Reassembly Span [$\downarrow$]")

    # Row 1: per-build scatter — RTP packet count vs latency (shared axes)
    ax_b, ax_i = axes[1, 0], axes[1, 1]
    for ax, lb in [(ax_b, BASELINE), (ax_i, IMPROVED)]:
        bd = build_data[lb]
        _scatter_rtp_vs_latency(ax, bd["pkts"], bd["delta"], bd["kf"],
                                LABELS[lb], "RTP Packets")
    xmax = max(ax_b.get_xlim()[1], ax_i.get_xlim()[1])
    ymax = max(ax_b.get_ylim()[1], ax_i.get_ylim()[1])
    for a in (ax_b, ax_i):
        a.set_xlim(0, xmax)
        a.set_ylim(a.get_ylim()[0], ymax)

    # Row 2: per-build scatter — RTP span (RTSP) vs latency (shared axes)
    ax_b2, ax_i2 = axes[2, 0], axes[2, 1]
    for ax, lb in [(ax_b2, BASELINE), (ax_i2, IMPROVED)]:
        bd = build_data[lb]
        span_ms = bd["span_rtsp"] / 1000.0 if len(bd["span_rtsp"]) > 0 else np.array([])
        delta = bd["span_delta"]
        kf = bd["kf"][:len(delta)] if len(bd["kf"]) >= len(delta) else bd["kf"]
        if len(span_ms) > 0 and len(delta) > 0:
            n = min(len(span_ms), len(delta), len(kf))
            _scatter_rtp_vs_latency(ax, span_ms[:n], delta[:n], kf[:n],
                                    LABELS[lb], "RTP Span RTSP (ms)")
    xmax2 = max(ax_b2.get_xlim()[1], ax_i2.get_xlim()[1])
    ymax2 = max(ax_b2.get_ylim()[1], ax_i2.get_ylim()[1])
    for a in (ax_b2, ax_i2):
        a.set_xlim(0, xmax2)
        a.set_ylim(a.get_ylim()[0], ymax2)

    p = out_dir / "rtp_fragmentation.pdf"
    save_fig(fig, p)
    plots["rtp_fragmentation"] = p


# ── Temporal / Burst-Position Analysis ────────────────────────────────────────


def _compute_wake_profile(entries, n_after=5):
    """Compute mean latency at offsets 0..n_after after I-frames vs P-frame control.

    Returns (offsets, kf_means, kf_stds, pf_means, pf_stds) where each *_means
    and *_stds array has length n_after+1.  Returns all-empty arrays when there
    is insufficient data.
    """
    offsets = np.arange(n_after + 1)
    kf_profiles = []
    pf_profiles = []
    rng = np.random.default_rng(42)

    for e in entries:
        df = e["csv"]
        r_col, w_col = "rtsp-0_arrival_us", "webrtc-0_arrival_us"
        r_kf_col = "rtsp-0_is_keyframe"
        if not all(c in df.columns for c in (r_col, w_col, r_kf_col)):
            continue
        m = df.dropna(subset=[r_col, w_col]).reset_index(drop=True)
        if len(m) < n_after + 10:
            continue
        lat_ms = (m[w_col] - m[r_col]).values / 1000.0
        valid = (lat_ms >= 0) & (lat_ms <= LATENCY_MAX_MS)
        lat_ms, m = lat_ms[valid], m.iloc[valid].reset_index(drop=True)
        if len(m) < n_after + 10:
            continue
        is_kf = np.array(m[r_kf_col].values.astype(int) == 1, dtype=bool, copy=True)
        kf_idx = np.where(is_kf)[0]
        kf_idx = kf_idx[kf_idx + n_after < len(lat_ms)]
        for i in kf_idx:
            kf_profiles.append(lat_ms[i:i + n_after + 1])

        pf_idx = np.where(is_kf == False)[0]
        pf_idx = pf_idx[pf_idx + n_after < len(lat_ms)]
        if len(pf_idx) > 0:
            sample_n = min(len(pf_idx), len(kf_idx) * 3)
            chosen = rng.choice(pf_idx, size=sample_n, replace=False)
            for i in chosen:
                pf_profiles.append(lat_ms[i:i + n_after + 1])

    empty = (offsets, np.array([]), np.array([]), np.array([]), np.array([]))
    if not kf_profiles or not pf_profiles:
        return empty
    kf_arr = np.vstack(kf_profiles)
    pf_arr = np.vstack(pf_profiles)
    return (offsets,
            np.mean(kf_arr, axis=0), np.std(kf_arr, axis=0),
            np.mean(pf_arr, axis=0), np.std(pf_arr, axis=0))


def _match_trigger_frames(entries):
    """For each freeze-burst event, locate the trigger frame and extract its latency.

    Returns (trigger_latencies_ms, trigger_is_kf, population_latencies_ms,
             population_is_kf) — arrays for the next build.
    """
    trigger_lat = []
    trigger_kf = []
    pop_lat = []
    pop_kf = []

    for e in entries:
        df = e["csv"]
        r_col, w_col = "rtsp-0_arrival_us", "webrtc-0_arrival_us"
        r_kf_col = "rtsp-0_is_keyframe"
        if not all(c in df.columns for c in (r_col, w_col, r_kf_col)):
            continue
        m = df.dropna(subset=[r_col, w_col]).reset_index(drop=True)
        if m.empty:
            continue
        lat_ms = (m[w_col] - m[r_col]).values / 1000.0
        valid = (lat_ms >= 0) & (lat_ms <= LATENCY_MAX_MS)
        lat_ms, m = lat_ms[valid], m.iloc[valid].reset_index(drop=True)
        if m.empty:
            continue
        is_kf = m[r_kf_col].values.astype(int) == 1
        arrivals = m[w_col].values.astype(float)

        pop_lat.append(lat_ms)
        pop_kf.append(is_kf)

        fb_events = e["event_records"].get("webrtc-0", {}).get("freeze_bursts", [])
        for fb in fb_events:
            ts = fb["timestamp_us"]
            idx = np.argmin(np.abs(arrivals - ts))
            if np.abs(arrivals[idx] - ts) < 200_000:
                trigger_lat.append(lat_ms[idx])
                trigger_kf.append(is_kf[idx])

    t_lat = np.array(trigger_lat) if trigger_lat else np.array([])
    t_kf = np.array(trigger_kf) if trigger_kf else np.array([])
    p_lat = np.concatenate(pop_lat) if pop_lat else np.array([])
    p_kf = np.concatenate(pop_kf) if pop_kf else np.array([])
    return t_lat, t_kf, p_lat, p_kf


def _wilson_ci(k, n, z=1.96):
    """Wilson score interval for binomial proportion k/n."""
    if n == 0:
        return 0.0, 0.0, 0.0
    p_hat = k / n
    denom = 1 + z * z / n
    centre = (p_hat + z * z / (2 * n)) / denom
    radicand = (p_hat * (1 - p_hat) + z * z / (4 * n)) / n
    spread = z * np.sqrt(max(0.0, radicand)) / denom
    return p_hat, max(0, centre - spread), min(1, centre + spread)


def _compute_conditional_rates(entries, spike_pct=95):
    """Compute conditional probabilities linking frame type, spikes, and freeze-bursts.

    Returns a dict of {label: (rate, ci_lo, ci_hi)} suitable for display.
    Only uses the IMPROVED (next) build entries.
    """
    all_lat = []
    all_kf = []
    all_is_fb_trigger = []

    for e in entries:
        df = e["csv"]
        r_col, w_col = "rtsp-0_arrival_us", "webrtc-0_arrival_us"
        r_kf_col = "rtsp-0_is_keyframe"
        if not all(c in df.columns for c in (r_col, w_col, r_kf_col)):
            continue
        m = df.dropna(subset=[r_col, w_col]).reset_index(drop=True)
        if m.empty:
            continue
        lat_ms = (m[w_col] - m[r_col]).values / 1000.0
        valid = (lat_ms >= 0) & (lat_ms <= LATENCY_MAX_MS)
        lat_ms, m = lat_ms[valid], m.iloc[valid].reset_index(drop=True)
        if m.empty:
            continue
        is_kf = m[r_kf_col].values.astype(int) == 1
        arrivals = m[w_col].values.astype(float)

        fb_triggers = np.zeros(len(lat_ms), dtype=bool)
        fb_events = e["event_records"].get("webrtc-0", {}).get("freeze_bursts", [])
        for fb in fb_events:
            ts = fb["timestamp_us"]
            idx = np.argmin(np.abs(arrivals - ts))
            if np.abs(arrivals[idx] - ts) < 200_000:
                fb_triggers[idx] = True

        all_lat.append(lat_ms)
        all_kf.append(is_kf)
        all_is_fb_trigger.append(fb_triggers)

    if not all_lat:
        return {}

    lat = np.concatenate(all_lat)
    kf = np.array(np.concatenate(all_kf), dtype=bool, copy=True)
    fb_arr = np.concatenate(all_is_fb_trigger)
    spike_th = np.percentile(lat, spike_pct)
    spike = lat > spike_th

    n_kf = int(kf.sum())
    n_pf = len(kf) - n_kf
    n_spike = int(spike.sum())
    n_nonspike = len(spike) - n_spike
    n_fb = int(fb_arr.sum())

    fb_and_kf = int((fb_arr & kf).sum())
    fb_and_pf = n_fb - fb_and_kf
    spike_and_kf = int((spike & kf).sum())
    spike_and_pf = n_spike - spike_and_kf
    fb_and_spike = int((fb_arr & spike).sum())
    fb_and_nonspike = n_fb - fb_and_spike

    results = {}
    results["P(FB | I-frame)"] = _wilson_ci(fb_and_kf, n_kf)
    results["P(FB | P-frame)"] = _wilson_ci(fb_and_pf, n_pf)
    results["P(spike | I-frame)"] = _wilson_ci(spike_and_kf, n_kf)
    results["P(spike | P-frame)"] = _wilson_ci(spike_and_pf, n_pf)
    results["P(FB | spike)"] = _wilson_ci(fb_and_spike, n_spike)
    results["P(FB | non-spike)"] = _wilson_ci(fb_and_nonspike, n_nonspike)
    results["_spike_threshold_ms"] = spike_th
    results["_n_frames"] = len(lat)
    results["_n_fb"] = n_fb
    results["_kf_fraction"] = float(n_kf) / len(lat) if len(lat) > 0 else 0.0
    return results


def _gen_burst_position_analysis(data, out_dir, plots):
    """Temporal burst-position analysis page (3x2, next build only)."""
    entries = data[IMPROVED]
    if not entries:
        return

    fig, axes = plt.subplots(3, 2, figsize=(PLOT_W, PLOT_H3 + 2))
    fig.subplots_adjust(hspace=0.85, wspace=0.35)

    # ── Row 0: Latency time-series with freeze-burst overlay ──────────────

    # Col 0: sample trial (most freeze-bursts)
    best_e = max(entries, key=lambda e: len(
        e["event_records"].get("webrtc-0", {}).get("freeze_bursts", [])))
    ax = axes[0, 0]
    _plot_latency_timeseries(ax, best_e, f"Latency Time-Series ({LABELS[IMPROVED]}, worst trial)")

    # Col 1: all trials superimposed (normalised frame index)
    ax = axes[0, 1]
    _plot_latency_all_trials(ax, entries)

    # ── Row 1: Wake effect and trigger enrichment ─────────────────────────

    # Col 0: I-frame wake profile
    ax = axes[1, 0]
    offsets, kf_m, kf_s, pf_m, pf_s = _compute_wake_profile(entries)
    if len(kf_m) > 0:
        ax.plot(offsets, kf_m, "o-", color=RED, label="After I-frame", linewidth=1.5)
        ax.fill_between(offsets, kf_m - kf_s, kf_m + kf_s, color=RED, alpha=0.15)
        ax.plot(offsets, pf_m, "s--", color=BLUE, label="After P-frame (control)", linewidth=1.5)
        ax.fill_between(offsets, pf_m - pf_s, pf_m + pf_s, color=BLUE, alpha=0.15)
        place_legend(ax)
    ax.set_xlabel("Frame offset from anchor")
    ax.set_ylabel("Pairwise latency (ms)")
    ax.set_title("I-Frame Wake Effect (head-of-line test)")
    ax.set_xticks(offsets)

    # Col 1: trigger frame latency distribution
    ax = axes[1, 1]
    t_lat, t_kf, p_lat, p_kf = _match_trigger_frames(entries)
    if len(p_lat) > 0:
        cap = np.percentile(p_lat, 99)
        bins = np.linspace(0, cap, 80)
        ax.hist(p_lat[p_lat <= cap], bins=bins, density=True, alpha=0.35,
                color=BLUE, label="All frames")
        if len(t_lat) > 0:
            ax.hist(t_lat[t_lat <= cap], bins=bins, density=True, alpha=0.7,
                    color=ORANGE, label=f"FB triggers (n={len(t_lat)})")
            ax.axvline(np.median(t_lat), color=ORANGE, ls="--", lw=1.2,
                       label=f"Trigger median={np.median(t_lat):.1f}ms")
        ax.axvline(np.median(p_lat), color=BLUE, ls="--", lw=1.0,
                   label=f"Population median={np.median(p_lat):.1f}ms")
        place_legend(ax)
    ax.set_xlabel("Pairwise latency (ms)")
    ax.set_ylabel("Density")
    ax.set_title("Freeze-Burst Trigger Frame Latency")

    # ── Row 2: Spike composition and conditional rates ────────────────────

    # Col 0: spike composition bars
    ax = axes[2, 0]
    rates = _compute_conditional_rates(entries)
    if rates:
        spike_th = rates["_spike_threshold_ms"]
        kf_frac = rates["_kf_fraction"]
        sp_i = rates["P(spike | I-frame)"][0]
        sp_p = rates["P(spike | P-frame)"][0]

        x = np.arange(2)
        w = 0.35
        ax.bar(x - w / 2, [sp_i * 100, sp_p * 100], w,
               color=[RED, BLUE], alpha=0.7, label="Spike rate (%)")
        ax.bar(x + w / 2,
               [kf_frac * 100,
                (1 - kf_frac) * 100],
               w, color=[RED, BLUE], alpha=0.25, label="Stream composition (%)")
        ax.set_xticks(x)
        ax.set_xticklabels(["I-frames", "P-frames"])
        ax.set_ylabel("Percentage")
        ax.set_title(f"Spike Composition (threshold={spike_th:.1f}ms = P95)")
        for i, v in enumerate([sp_i * 100, sp_p * 100]):
            ax.text(x[i] - w / 2, v + 0.5, f"{v:.2f}%", ha="center", va="bottom",
                    fontsize=9)
        place_legend(ax)

    # Col 1: conditional probability summary
    ax = axes[2, 1]
    ax.axis("off")
    if rates:
        lines = []
        for label in ["P(FB | I-frame)", "P(FB | P-frame)",
                       "P(spike | I-frame)", "P(spike | P-frame)",
                       "P(FB | spike)", "P(FB | non-spike)"]:
            rate, lo, hi = rates[label]
            lines.append(f"{label:25s} = {rate:.5f}  [{lo:.5f}, {hi:.5f}]")
        lines.append("")
        lines.append(f"{'Spike threshold':25s} = {rates['_spike_threshold_ms']:.1f} ms (P95)")
        lines.append(f"{'Total frames':25s} = {rates['_n_frames']:,}")
        lines.append(f"{'Total FB triggers':25s} = {rates['_n_fb']}")
        lines.append(f"{'I-frame fraction':25s} = {rates['_kf_fraction']:.3f}")
        ax.text(0.05, 0.95, "\n".join(lines), transform=ax.transAxes,
                va="top", ha="left", fontsize=9, family="monospace",
                bbox=dict(boxstyle="round,pad=0.4", fc="#f8f9fa", ec="#cccccc"))
        ax.set_title("Conditional Probability Summary (Wilson 95% CI)")

    p = out_dir / "burst_position.pdf"
    save_fig(fig, p)
    plots["burst_position"] = p


def _gen_bitrate_efficiency(data, out_dir, plots):
    """Bitrate efficiency / NAL composition analysis page (2x2).

    Compares VCL vs filler vs overhead byte breakdown across builds,
    using the rtsp-0_vcl_bytes and rtsp-0_filler_bytes CSV columns.
    Gracefully skips if columns are absent (older CSVs).
    """
    vcl_col = "rtsp-0_vcl_bytes"
    fill_col = "rtsp-0_filler_bytes"
    bytes_col = "rtsp-0_bytes"
    kf_col = "rtsp-0_is_keyframe"

    # Gather per-frame (total, vcl, filler, is_kf) across both builds
    build_frames = {}
    for build_label in [BASELINE, IMPROVED]:
        total_arr, vcl_arr, filler_arr, kf_arr = [], [], [], []
        for e in data[build_label]:
            df = e["csv"]
            needed = [bytes_col, kf_col, vcl_col, fill_col]
            if not all(c in df.columns for c in needed):
                continue
            sub = df.dropna(subset=[bytes_col])
            if sub.empty:
                continue
            total_arr.append(sub[bytes_col].values.astype(float))
            vcl_arr.append(sub[vcl_col].values.astype(float))
            filler_arr.append(sub[fill_col].values.astype(float))
            kf_arr.append(np.array(sub[kf_col].values.astype(int) == 1, dtype=bool, copy=True))
        if not total_arr:
            build_frames[build_label] = None
            continue
        build_frames[build_label] = {
            "total": np.concatenate(total_arr),
            "vcl": np.concatenate(vcl_arr),
            "filler": np.concatenate(filler_arr),
            "is_kf": np.concatenate(kf_arr),
        }

    if all(v is None for v in build_frames.values()):
        return

    fig, axes = plt.subplots(2, 2, figsize=(PLOT_W, PLOT_H))
    fig.subplots_adjust(hspace=0.55, wspace=0.35)

    # ── (0,0) Frame composition stacked bars ─────────────────────────────
    ax = axes[0, 0]
    bar_labels = []
    vcl_means, filler_means, overhead_means = [], [], []
    for build_label in [BASELINE, IMPROVED]:
        bf = build_frames[build_label]
        if bf is None:
            continue
        for ft_label, mask in [("I-frame", bf["is_kf"]),
                               ("P-frame", bf["is_kf"] == False)]:
            t = bf["total"][mask]
            v = bf["vcl"][mask]
            f = bf["filler"][mask]
            if len(t) == 0:
                continue
            bar_labels.append(f"{ft_label}\n{LABELS[build_label]}")
            vcl_means.append(np.mean(v) / 1024)
            filler_means.append(np.mean(f) / 1024)
            overhead_means.append(np.mean(t - v - f) / 1024)

    if bar_labels:
        x = np.arange(len(bar_labels))
        w = 0.5
        ax.bar(x, vcl_means, w, label="VCL (coded video)", color=BLUE, alpha=0.7)
        ax.bar(x, filler_means, w, bottom=vcl_means, label="Filler (padding)", color=RED, alpha=0.7)
        bot2 = [a + b for a, b in zip(vcl_means, filler_means)]
        ax.bar(x, overhead_means, w, bottom=bot2, label="Overhead (SPS/PPS/SEI)", color=ORANGE, alpha=0.5)
        ax.set_xticks(x)
        ax.set_xticklabels(bar_labels, fontsize=9)
        ax.set_ylabel("Mean frame size (KB)")
        place_legend(ax, ncol=3)
    ax.set_title("Frame Byte Composition")

    # ── (0,1) Filler ratio distribution ──────────────────────────────────
    ax = axes[0, 1]
    for build_label, color in [(BASELINE, BASELINE_COLOR), (IMPROVED, IMPROVED_COLOR)]:
        bf = build_frames[build_label]
        if bf is None:
            continue
        safe_total = np.where(bf["total"] > 0, bf["total"], 1)
        ratio = bf["filler"] / safe_total * 100.0
        if len(ratio) == 0:
            continue
        hi = max(np.percentile(ratio, 99.5), 1.0)
        bins = np.linspace(0, hi, 60)
        ax.hist(ratio, bins=bins, alpha=0.5, color=color, density=True,
                label=f"{LABELS[build_label]} (med={np.median(ratio):.2f}%)")
    ax.set_xlabel("Filler / Total (%)")
    ax.set_ylabel("Density")
    ax.set_title("Filler Ratio Distribution")
    if ax.get_legend_handles_labels()[1]:
        place_legend(ax)

    # ── (1,0) VCL bytes per frame type boxplot ───────────────────────────
    ax = axes[1, 0]
    box_data, box_labels = [], []
    for build_label in [BASELINE, IMPROVED]:
        bf = build_frames[build_label]
        if bf is None:
            continue
        for ft_label, mask in [("I-frame", bf["is_kf"]),
                               ("P-frame", bf["is_kf"] == False)]:
            v = bf["vcl"][mask] / 1024.0
            if len(v) == 0:
                continue
            cap = np.percentile(v, 99)
            box_data.append(np.clip(v, 0, cap))
            box_labels.append(f"{ft_label}\n{LABELS[build_label]}")
    if box_data:
        bp = compat_boxplot(ax, box_data, tick_labels=box_labels,
                            patch_artist=True, widths=0.45,
                            medianprops=dict(color=DARK, linewidth=1.5))
        colors = []
        for lbl in box_labels:
            colors.append(RED if "I-frame" in lbl else BLUE)
        for idx, c in enumerate(colors):
            bp["boxes"][idx].set_facecolor(c)
            bp["boxes"][idx].set_alpha(0.5)
    ax.set_ylabel("VCL bytes (KB)")
    ax.set_title("VCL Bytes per Frame Type (P99 cap)")

    # ── (1,1) Filler bytes vs total frame size scatter ───────────────────
    ax = axes[1, 1]
    for build_label, color in [(BASELINE, BASELINE_COLOR), (IMPROVED, IMPROVED_COLOR)]:
        bf = build_frames[build_label]
        if bf is None:
            continue
        t_kb = bf["total"] / 1024.0
        f_kb = bf["filler"] / 1024.0
        if len(t_kb) == 0:
            continue
        ax.scatter(t_kb, f_kb, s=1, alpha=0.15, color=color,
                   label=LABELS[build_label], rasterized=True)
    ax.set_xlabel("Total frame size (KB)")
    ax.set_ylabel("Filler bytes (KB)")
    ax.set_title("Filler vs Frame Size")
    if ax.get_legend_handles_labels()[1]:
        place_legend(ax)

    p = out_dir / "bitrate_efficiency.pdf"
    save_fig(fig, p)
    plots["bitrate_efficiency"] = p


def _plot_latency_timeseries(ax, entry, title):
    """Plot per-frame pairwise latency vs frame index with FB overlay for one trial."""
    df = entry["csv"]
    r_col, w_col = "rtsp-0_arrival_us", "webrtc-0_arrival_us"
    r_kf_col = "rtsp-0_is_keyframe"
    if not all(c in df.columns for c in (r_col, w_col, r_kf_col)):
        return
    m = df.dropna(subset=[r_col, w_col]).reset_index(drop=True)
    if m.empty:
        return
    lat_ms = (m[w_col] - m[r_col]).values / 1000.0
    valid = (lat_ms >= 0) & (lat_ms <= LATENCY_MAX_MS)
    lat_ms, m = lat_ms[valid], m.iloc[valid].reset_index(drop=True)
    if m.empty:
        return
    is_kf = m[r_kf_col].values.astype(int) == 1
    arrivals = m[w_col].values.astype(float)
    idx = np.arange(len(lat_ms))

    cap = np.percentile(lat_ms, 99.5)
    lat_plot = np.clip(lat_ms, None, cap)
    ax.plot(idx, lat_plot, linewidth=0.3, alpha=0.5, color=BLUE)

    win = min(50, len(lat_ms) // 4) if len(lat_ms) > 8 else 1
    if win > 1:
        rolling_med = pd.Series(lat_plot).rolling(win, center=True, min_periods=1).median()
        ax.plot(idx, rolling_med, linewidth=1.2, color=DARK, label="Rolling median")

    kf_idx = idx[is_kf]
    ymin, ymax = ax.get_ylim()
    ax.vlines(kf_idx, ymin=ymin, ymax=ymin + (ymax - ymin) * 0.05,
              colors=RED, linewidth=0.4, alpha=0.5, label="I-frames")

    fb_events = entry["event_records"].get("webrtc-0", {}).get("freeze_bursts", [])
    for fb in fb_events:
        ts_start = fb["timestamp_us"]
        dur = fb["duration_us"]
        i_start = np.argmin(np.abs(arrivals - ts_start))
        i_end = np.argmin(np.abs(arrivals - (ts_start + dur)))
        i_end = max(i_end, i_start + 1)
        ax.axvspan(i_start, i_end, alpha=0.25, color=ORANGE, zorder=0)

    if fb_events:
        ax.axvspan(0, 0, alpha=0.25, color=ORANGE, label="Freeze-burst")
    ax.set_xlabel("Frame index")
    ax.set_ylabel("Latency (ms)")
    ax.set_title(title)
    place_legend(ax, ncol=3)


def _plot_latency_all_trials(ax, entries):
    """Superimpose normalised latency traces from all trials with FB positions."""
    grand_lat = []
    grand_pos = []
    fb_positions = []

    for e in entries:
        df = e["csv"]
        r_col, w_col = "rtsp-0_arrival_us", "webrtc-0_arrival_us"
        if not all(c in df.columns for c in (r_col, w_col)):
            continue
        m = df.dropna(subset=[r_col, w_col]).reset_index(drop=True)
        if len(m) < 10:
            continue
        lat_ms = (m[w_col] - m[r_col]).values / 1000.0
        valid = (lat_ms >= 0) & (lat_ms <= LATENCY_MAX_MS)
        lat_ms, m = lat_ms[valid], m.iloc[valid].reset_index(drop=True)
        if len(m) < 10:
            continue
        norm_pos = np.linspace(0, 1, len(lat_ms))
        cap = np.percentile(lat_ms, 99.5)
        lat_plot = np.clip(lat_ms, None, cap)
        ax.plot(norm_pos, lat_plot, linewidth=0.2, alpha=0.15, color=BLUE)
        grand_lat.append(lat_plot)
        grand_pos.append(norm_pos)

        arrivals = m[w_col].values.astype(float)
        total_dur = arrivals[-1] - arrivals[0]
        if total_dur <= 0:
            continue
        for fb in e["event_records"].get("webrtc-0", {}).get("freeze_bursts", []):
            pos = (fb["timestamp_us"] - arrivals[0]) / total_dur
            if 0 <= pos <= 1:
                fb_positions.append(pos)

    if grand_lat:
        n_bins = 200
        bin_edges = np.linspace(0, 1, n_bins + 1)
        bin_mids = (bin_edges[:-1] + bin_edges[1:]) / 2
        all_lat = np.concatenate(grand_lat)
        all_pos = np.concatenate(grand_pos)
        digitized = np.digitize(all_pos, bin_edges) - 1
        digitized = np.clip(digitized, 0, n_bins - 1)
        med_line = np.array([np.median(all_lat[digitized == b])
                             if np.any(digitized == b) else np.nan
                             for b in range(n_bins)])
        ax.plot(bin_mids, med_line, linewidth=1.5, color=DARK, label="Grand median")

    for pos in fb_positions:
        ax.axvline(pos, color=ORANGE, alpha=0.15, linewidth=0.5)
    if fb_positions:
        ax.axvline(fb_positions[0], color=ORANGE, alpha=0.15, linewidth=0.5,
                   label=f"FB events (n={len(fb_positions)})")

    ax.set_xlabel("Normalised trial position")
    ax.set_ylabel("Latency (ms)")
    ax.set_title(f"All Trials Superimposed ({LABELS[IMPROVED]})")
    place_legend(ax, ncol=3)


# ── LaTeX document generation ────────────────────────────────────────────────

DOWN = "$\\downarrow$"  # lower is better
UP = "$\\uparrow$"      # higher is better

METRICS_DEF = [
    ("RTSP FPS",              lambda e: extract_client(e,"rtsp-0")["fps"],                            UP),
    ("WebRTC FPS",            lambda e: extract_client(e,"webrtc-0")["fps"],                          UP),
    ("Latency Mean (ms)",     lambda e: extract_pair(e)["delta_mean_us"]/1000,                        DOWN),
    ("Latency P50 (ms)",      lambda e: extract_pair(e)["delta_p50_us"]/1000,                         DOWN),
    ("Latency P95 (ms)",      lambda e: extract_pair(e)["delta_p95_us"]/1000,                         DOWN),
    ("Latency P99 (ms)",      lambda e: extract_pair(e)["delta_p99_us"]/1000,                         DOWN),
    ("Latency Stddev (ms)",   lambda e: extract_pair(e)["delta_stddev_us"]/1000,                      DOWN),
    ("I-frame Lat. Mean (ms)",lambda e: extract_pair(e).get("delta_keyframe_mean_us",0)/1000,         DOWN),
    ("I-frame Lat. P50 (ms)", lambda e: extract_pair(e).get("delta_keyframe_p50_us",0)/1000,          DOWN),
    ("I-frame Lat. P95 (ms)", lambda e: extract_pair(e).get("delta_keyframe_p95_us",0)/1000,          DOWN),
    ("P-frame Lat. Mean (ms)",lambda e: extract_pair(e).get("delta_pframe_mean_us",0)/1000,           DOWN),
    ("P-frame Lat. P50 (ms)", lambda e: extract_pair(e).get("delta_pframe_p50_us",0)/1000,            DOWN),
    ("P-frame Lat. P95 (ms)", lambda e: extract_pair(e).get("delta_pframe_p95_us",0)/1000,            DOWN),
    ("I-frame Lat. Premium (ms)", lambda e: (extract_pair(e).get("delta_keyframe_mean_us",0) - extract_pair(e).get("delta_pframe_mean_us",0))/1000, DOWN),
    ("RTSP Jitter (ms)",      lambda e: extract_client(e,"rtsp-0")["jitter_stddev_us"]/1000,          DOWN),
    ("WebRTC Jitter (ms)",    lambda e: extract_client(e,"webrtc-0")["jitter_stddev_us"]/1000,        DOWN),
    ("RTSP True Drops",       lambda e: extract_client(e,"rtsp-0")["stutters"]["true_drop_events"],   DOWN),
    ("WebRTC True Drops",     lambda e: extract_client(e,"webrtc-0")["stutters"]["true_drop_events"], DOWN),
    ("RTSP Iso. Stutters",    lambda e: extract_client(e,"rtsp-0")["stutters"]["isolated_stutter_events"],  DOWN),
    ("WebRTC Iso. Stutters",  lambda e: extract_client(e,"webrtc-0")["stutters"]["isolated_stutter_events"],DOWN),
    ("Missed Frames (RTSP)",  lambda e: extract_client(e,"rtsp-0")["stutters"]["estimated_missed_frames"],  DOWN),
    ("Missed Frames (WebRTC)",lambda e: extract_client(e,"webrtc-0")["stutters"]["estimated_missed_frames"],DOWN),
    ("Freeze-Bursts (RTSP)",  lambda e: extract_freeze_bursts(e, "rtsp-0"),                           DOWN),
    ("Freeze-Bursts (WebRTC)",lambda e: extract_freeze_bursts(e, "webrtc-0"),                         DOWN),
    ("FB@Keyframe (RTSP)",    lambda e: extract_fb_at_keyframe(e, "rtsp-0"),                          DOWN),
    ("FB@Keyframe (WebRTC)",  lambda e: extract_fb_at_keyframe(e, "webrtc-0"),                        DOWN),
    ("FB@Delta (RTSP)",       lambda e: extract_fb_at_delta(e, "rtsp-0"),                             DOWN),
    ("FB@Delta (WebRTC)",     lambda e: extract_fb_at_delta(e, "webrtc-0"),                           DOWN),
    ("Episodes (RTSP)",       lambda e: extract_disruption_episodes(e, "rtsp-0"),                     DOWN),
    ("Episodes (WebRTC)",     lambda e: extract_disruption_episodes(e, "webrtc-0"),                   DOWN),
    ("Med. Drop Dur. RTSP (ms)",    lambda e: _trial_median_event_duration(e, "rtsp-0", "true_drops"),       DOWN),
    ("Med. Drop Dur. WebRTC (ms)",  lambda e: _trial_median_event_duration(e, "webrtc-0", "true_drops"),    DOWN),
    ("Med. FB Dur. RTSP (ms)",      lambda e: _trial_median_event_duration(e, "rtsp-0", "freeze_bursts"),   DOWN),
    ("Med. FB Dur. WebRTC (ms)",    lambda e: _trial_median_event_duration(e, "webrtc-0", "freeze_bursts"), DOWN),
    ("Med. Ep. Dur. RTSP (ms)",     lambda e: _trial_median_event_duration(e, "rtsp-0", "episodes"),        DOWN),
    ("Med. Ep. Dur. WebRTC (ms)",   lambda e: _trial_median_event_duration(e, "webrtc-0", "episodes"),      DOWN),
    ("Med. Inter-Drop RTSP (s)",    lambda e: _trial_median_inter_event(e, "rtsp-0", "true_drops"),         UP),
    ("Med. Inter-Drop WebRTC (s)",  lambda e: _trial_median_inter_event(e, "webrtc-0", "true_drops"),       UP),
    ("Med. Inter-FB RTSP (s)",      lambda e: _trial_median_inter_event(e, "rtsp-0", "freeze_bursts"),      UP),
    ("Med. Inter-FB WebRTC (s)",    lambda e: _trial_median_inter_event(e, "webrtc-0", "freeze_bursts"),    UP),
    ("Med. Inter-Ep. RTSP (s)",     lambda e: _trial_median_inter_event(e, "rtsp-0", "episodes"),           UP),
    ("Med. Inter-Ep. WebRTC (s)",   lambda e: _trial_median_inter_event(e, "webrtc-0", "episodes"),         UP),
    ("Frame Match \\%",       lambda e: extract_pair(e)["match_pct"],                                 UP),
    ("System CPU \\%",        lambda e: _stats_trial_mean(e, "sys_cpu_pct"),                          DOWN),
    ("MCM CPU \\%",           lambda e: _stats_trial_mean(e, "mcm_cpu_pct"),                          DOWN),
    ("MCM RSS (MB)",          lambda e: _stats_trial_mean(e, "mcm_rss_mb"),                           DOWN),
    ("CPU Temp (°C)",         lambda e: _stats_trial_mean(e, "cpu_temp_c"),                           DOWN),
    # ── Camera SoC metrics ──
    ("Cam Temp (°C)",         lambda e: _cam_trial_mean(e, "temp_c"),                                DOWN),
    ("Cam CPU \\%",           lambda e: _cam_trial_mean(e, "cam_cpu_pct"),                           DOWN),
    ("Cam Mem Used (MB)",     lambda e: _cam_trial_mean(e, "cam_mem_used_mb"),                       DOWN),
    ("Cam TX Rate (Mbps)",    lambda e: _cam_trial_mean(e, "cam_tx_rate_mbps"),                      "---"),
    ("Cam RX Rate (Mbps)",    lambda e: _cam_trial_mean(e, "cam_rx_rate_mbps"),                      "---"),
    ("Cam Core Volt (mV)",    lambda e: _cam_trial_mean(e, "core_volt"),                             "---"),
    ("Cam CPU Volt (mV)",     lambda e: _cam_trial_mean(e, "cpu_volt"),                              "---"),
    ("Cam NPU Volt (mV)",     lambda e: _cam_trial_mean(e, "npu_volt"),                              "---"),
    ("Cam Core TComp",        lambda e: _cam_trial_mean(e, "core_temp_comp"),                        "---"),
    ("Cam CPU TComp",         lambda e: _cam_trial_mean(e, "cpu_temp_comp"),                         "---"),
    ("Cam NPU TComp",         lambda e: _cam_trial_mean(e, "npu_temp_comp"),                         "---"),
]


def _stats_trial_mean(entry, col):
    """Return per-trial mean of a stats column (used by METRICS_DEF)."""
    df = entry.get("stats", pd.DataFrame())
    if col not in df.columns:
        return None
    s = pd.to_numeric(df[col][WARMUP_ROWS:], errors="coerce").dropna()
    return float(s.mean()) if len(s) > 0 else None


def _cam_trial_mean(entry, col):
    """Return per-trial mean of a camera_soc column (used by METRICS_DEF)."""
    df = entry.get("camera_soc", pd.DataFrame())
    if col not in df.columns:
        return None
    s = pd.to_numeric(df[col][CAMERA_WARMUP_ROWS:], errors="coerce").dropna()
    return float(s.mean()) if len(s) > 0 else None


def _trial_median_event_duration(entry, client, event_type):
    """Per-trial median event duration in ms (used by METRICS_DEF)."""
    recs = entry.get("event_records", {}).get(client, {}).get(event_type, [])
    if not recs:
        return None
    durations = [r["duration_us"] / 1000.0 for r in recs]
    return float(np.median(durations))


def _trial_median_inter_event(entry, client, event_type):
    """Per-trial median inter-event interval in seconds (used by METRICS_DEF)."""
    recs = entry.get("event_records", {}).get(client, {}).get(event_type, [])
    if len(recs) < 2:
        return None
    ts = np.array([r["timestamp_us"] for r in recs])
    intervals = np.diff(ts) / 1e6
    return float(np.median(intervals))


def tex_esc(s):
    return s.replace("_", r"\_").replace("%", r"\%").replace("&", r"\&")

def build_table_rows(data):
    rows = []
    for name, fn, goal in METRICS_DEF:
        b = series(data[BASELINE], fn)
        i = series(data[IMPROVED], fn)
        p, _ = mwu(b, i)
        sig = sigmark(p)
        bs = f"{np.mean(b):.2f} $\\pm$ {np.std(b):.2f}" if len(b) else "N/A"
        is_ = f"{np.mean(i):.2f} $\\pm$ {np.std(i):.2f}" if len(i) else "N/A"
        if len(b)>0 and len(i)>0 and np.mean(b)!=0:
            d = 100*(np.mean(i)-np.mean(b))/abs(np.mean(b))
            ds = f"{d:+.1f}\\%"
        else:
            ds = "N/A"
        ps = f"{p:.4f}" if not np.isnan(p) else "N/A"
        hl = r"\rowcolor{orange!10}" if sig == "**" else (r"\rowcolor{yellow!10}" if sig == "*" else "")
        rows.append(f"    {hl} {name} & {goal} & {bs} & {is_} & {ds} & {ps} & {sig} \\\\")
    return "\n".join(rows)

def build_latex(data, data_all, outlier_details, plots, out_dir):
    n_clean = len(data[BASELINE])
    n_all = len(data_all[BASELINE])
    n_outliers = n_all - n_clean
    base_lbl = LABELS[BASELINE]
    impr_lbl = LABELS[IMPROVED]

    def fig_cmd(key):
        if key in plots:
            return (f"\\includegraphics[width=\\textwidth,"
                    f"height=0.72\\textheight,keepaspectratio]"
                    f"{{{plots[key].name}}}")
        return ""

    clean_table = build_table_rows(data)
    all_table = build_table_rows(data_all)

    outlier_tex = ""
    if outlier_details:
        lines = []
        for d in outlier_details:
            lines.append(
                f"    {tex_esc(d['trial'])} "
                f"& {d['baseline_dur']:.0f} & {d['baseline_stats']} "
                f"& {d['improved_dur']:.0f} & {d['improved_stats']} "
                f"& {tex_esc(d['flagged_sides'])} \\\\"
            )
        outlier_tex = (
            r"\medskip" "\n"
            r"\textbf{Outlier trials excluded} ("
            + str(n_outliers) + " paired trial"
            + ("s" if n_outliers != 1 else "")
            + r" removed --- duration $<$ "
            + str(int(OUTLIER_MIN_DURATION_RATIO * 100))
            + r"\% of median, stats $<$ "
            + str(OUTLIER_MIN_STATS_ROWS)
            + r" rows, WebRTC match $<$ "
            + str(int(OUTLIER_MATCH_THRESHOLD))
            + r"\%, or WebRTC frames $<$ "
            + str(int(OUTLIER_FRAME_RATIO * 100))
            + r"\% of RTSP frames, or RTSP bitrate $<$ "
            + f"{OUTLIER_MIN_RTSP_BITRATE_MBPS:.1f}"
            + r" Mbps (80\% of configured 32.768 Mbps camera bitrate)):" "\n\n"
            r"\smallskip" "\n"
            r"\begin{tabular}{l r r r r l}" "\n"
            r"\toprule" "\n"
            r" & \multicolumn{2}{c}{\textbf{Baseline}} & \multicolumn{2}{c}{\textbf{Dev}} & \\" "\n"
            r"\cmidrule(lr){2-3} \cmidrule(lr){4-5}" "\n"
            r"\textbf{Trial} & \textbf{Duration} & \textbf{Stats} & \textbf{Duration} & \textbf{Stats} & \textbf{Flagged Side(s)} \\" "\n"
            r"\midrule" "\n"
            + "\n".join(lines) + "\n"
            r"\bottomrule" "\n"
            r"\end{tabular}" "\n\n"
            r"\smallskip" "\n"
            r"These trials had incomplete data (short duration or insufficient stats samples) "
            r"or WebRTC connection failures. "
            r"They are excluded from the summary tables and statistical plots above, "
            r"but \textbf{remain visible} in the per-trial evolution plots (marked with red X)."
        )

    tex = r"""\documentclass[a4paper,landscape,11pt]{article}
\usepackage[landscape,margin=2cm]{geometry}
\usepackage{graphicx}
\usepackage{booktabs}
\usepackage{longtable}
\usepackage[table]{xcolor}
\usepackage{parskip}
\usepackage{microtype}
\usepackage{lmodern}
\usepackage[T1]{fontenc}
\usepackage[utf8]{inputenc}
\pagestyle{empty}

\begin{document}

% ── Page 1: Executive Summary (clean) ─────────────────────────────
\begin{center}
{\LARGE\bfseries Overnight A/B Test Report}

\medskip
{\large Baseline: """ + tex_esc(base_lbl) + r""" \quad$\mid$\quad Improvement: """ + tex_esc(impr_lbl) + r""" \quad$\mid$\quad $n = """ + str(n_clean) + r"""$ paired trials (""" + str(n_outliers) + r""" outliers excluded)}

\smallskip
""" + f"João Antônio Cardoso --- {date.today().strftime('%B %d, %Y')}" + r"""
\end{center}

\medskip

This report compares two BlueOS builds running on a Raspberry Pi~4 (armv7).
An automated overnight test alternated between the two images in randomized order.
Before each $\sim$15-minute trial the Pi was fully rebooted to ensure a clean start.
A client on the local network simultaneously received the camera stream via RTSP
(directly from the IP camera) and WebRTC (relayed through MCM), measuring per-frame
latency, FPS, jitter, drops, and stutters.
System CPU, memory, and temperature were collected every second on the Pi.
Statistical comparisons use the Mann--Whitney~U test.

\medskip

\small
\begin{longtable}{l c r r r r c}
\caption*{\textbf{Summary --- outliers excluded} ($n = """ + str(n_clean) + r"""$ clean paired trials)} \\
\toprule
\textbf{Metric} & \textbf{Goal} & \textbf{""" + tex_esc(base_lbl) + r""" (baseline)} & \textbf{""" + tex_esc(impr_lbl) + r""" (new)} & \textbf{Delta} & \textbf{p-value} & \textbf{Sig} \\
\midrule
\endfirsthead
\multicolumn{7}{l}{\small\itshape Summary (outliers excluded) --- continued} \\
\toprule
\textbf{Metric} & \textbf{Goal} & \textbf{""" + tex_esc(base_lbl) + r""" (baseline)} & \textbf{""" + tex_esc(impr_lbl) + r""" (new)} & \textbf{Delta} & \textbf{p-value} & \textbf{Sig} \\
\midrule
\endhead
\midrule
\multicolumn{7}{r}{\small\itshape continued on next page} \\
\endfoot
\bottomrule
\endlastfoot
""" + clean_table + r"""
\end{longtable}
\normalsize

\smallskip
\textit{How to read: each row is one metric averaged across clean trials (mean $\pm$ std~dev).
``Goal'' shows the desired direction: $\downarrow$ = lower is better, $\uparrow$ = higher is better.
``Delta'' = percentage change from baseline to new (negative = new is lower).
``p-value'' = confidence the difference is real, not luck.
* = likely real ($p < 0.05$), ** = very likely real ($p < 0.01$).
Highlighted rows are statistically significant.}

""" + outlier_tex + r"""

% ── Page 1b: Full summary (all data including outliers) ───────────
\newpage
\section*{Reference: All Data Including Outliers ($n = """ + str(n_all) + r"""$)}

This table includes all trials, including the """ + str(n_outliers) + r""" outlier trials where
WebRTC had connection failures. Compare with the clean summary on the previous page
to see the impact of the outliers on the averages.

\small
\begin{longtable}{l c r r r r c}
\caption*{\textbf{Summary --- all trials} ($n = """ + str(n_all) + r"""$, including outliers)} \\
\toprule
\textbf{Metric} & \textbf{Goal} & \textbf{""" + tex_esc(base_lbl) + r""" (baseline)} & \textbf{""" + tex_esc(impr_lbl) + r""" (new)} & \textbf{Delta} & \textbf{p-value} & \textbf{Sig} \\
\midrule
\endfirsthead
\multicolumn{7}{l}{\small\itshape Summary (all trials) --- continued} \\
\toprule
\textbf{Metric} & \textbf{Goal} & \textbf{""" + tex_esc(base_lbl) + r""" (baseline)} & \textbf{""" + tex_esc(impr_lbl) + r""" (new)} & \textbf{Delta} & \textbf{p-value} & \textbf{Sig} \\
\midrule
\endhead
\midrule
\multicolumn{7}{r}{\small\itshape continued on next page} \\
\endfoot
\bottomrule
\endlastfoot
""" + all_table + r"""
\end{longtable}
\normalsize

% ── Page 2: Bar Charts – Latency & Jitter ─────────────────────────
\newpage
\section*{Summary: Latency \& Jitter}

Bar height = average across trials; error bars = $\pm$1 standard deviation.
Annotation above each pair shows statistical significance:
** = very likely real ($p<0.01$), * = likely real ($p<0.05$), n.s.\ = not significant.
Lower bars are better for all metrics on this page (less delay, less variability).

\vfill
\begin{center}
""" + fig_cmd("bar_latency") + r"""
\end{center}
\vfill

% ── Page 3: Bar Charts – Frame Delivery ───────────────────────────
\newpage
\section*{Summary: Frame Delivery}

These bars compare reliability metrics. Lower bars are better (fewer problems).
Drops = frames that never arrived. Stutters = visible pauses in the video stream.
Missed Frames = estimated total frames lost.
Each group shows RTSP and WebRTC side by side for both versions.

\vfill
\begin{center}
""" + fig_cmd("bar_delivery") + r"""
\end{center}
\vfill

% ── Page 4: Latency Distributions ─────────────────────────────────
\newpage
\section*{Pairwise Latency: RTSP $\rightarrow$ WebRTC}

Latency = how long it takes for the same video frame to travel from RTSP to WebRTC (lower is better).
Top-left: most frames fall here (trims outliers). Top-right: full range on a log scale to reveal rare spikes.
Bottom: box plots compare per-trial median (P50) and 95th-percentile (P95) latency.
The box shows the middle 50\% of trials; the line is the median; whiskers show the range.

\vfill
\begin{center}
""" + fig_cmd("latency_dist") + r"""
\end{center}
\vfill

% ── I-frame vs P-frame Latency ───────────────────────────────────
\newpage
\section*{I-frame vs P-frame Latency Analysis}

Testing hypotheses about MCM WebRTC freeze/burst amplification.
\textbf{H1}: I-frames have higher processing cost $\Rightarrow$ higher latency.
\textbf{H4}: latency correlates with frame size regardless of type (throughput bottleneck).
Top-left: latency distributions split by frame type.
Top-right: per-build comparison.
Bottom-left: scatter of frame size vs latency (Pearson~$r$ shown).
Bottom-right: summary statistics by frame type and build.

\vfill
\begin{center}
""" + fig_cmd("frame_type_latency") + r"""
\end{center}
\vfill

% ── RTP Fragmentation ────────────────────────────────────────────
""" + (r"""
\newpage
\section*{RTP Fragmentation Analysis}

Testing hypotheses about RTP reassembly overhead.
\textbf{H2}: I-frames require more RTP packets; MCM waits for all before forwarding.
\textbf{H3}: more packets per frame = more jitter compounding $\Rightarrow$ higher frame latency.
Top-left: RTP packet count distributions by frame type.
Top-right: packet count vs latency scatter (Pearson~$r$).
Bottom-left: RTP reassembly span (first$\rightarrow$last packet) by frame type and transport.
Bottom-right: reassembly span vs latency scatter.

\vfill
\begin{center}
""" + fig_cmd("rtp_fragmentation") + r"""
\end{center}
\vfill
""" if "rtp_fragmentation" in plots else "") + (r"""

% ── Temporal / Burst-Position Analysis ────────────────────────────
\newpage
\section*{Temporal / Burst-Position Analysis}

\textbf{Goal}: determine whether freeze-burst events are the \emph{tail} of the
steady-state I-frame latency premium (explanation~A) or caused by
intermittent processing spikes (explanation~B).

Top-left: per-frame latency time-series for the worst trial, with I-frame ticks
and freeze-burst windows shaded.
Top-right: all trials superimposed (normalised position), grand median, FB markers.
Middle-left: I-frame wake profile --- latency of frames 0..+5 after I-frames vs
P-frame control; tests head-of-line blocking.
Middle-right: FB trigger frame latency distribution vs population.
Bottom-left: spike composition by frame type.
Bottom-right: conditional probability summary (Wilson 95\% CI).

\vfill
\begin{center}
""" + fig_cmd("burst_position") + r"""
\end{center}
\vfill
""" if "burst_position" in plots else "") + (r"""

% ── Page: Bitrate Efficiency / NAL Composition ────────────────────
\newpage
\section*{Bitrate Efficiency / NAL Composition}

Per-frame byte breakdown into VCL (coded video), filler (H.264 NAL type 12 padding),
and overhead (SPS/PPS/SEI).
Top-left: average frame composition stacked by frame type and build.
Top-right: filler ratio distribution.
Bottom-left: VCL-only bytes per frame type.
Bottom-right: filler bytes vs total frame size.

\vfill
\begin{center}
""" + fig_cmd("bitrate_efficiency") + r"""
\end{center}
\vfill
""" if "bitrate_efficiency" in plots else "") + r"""

% ── Page 5: FPS and Frame Delivery ────────────────────────────────
\newpage
\section*{FPS and Frame Delivery}

FPS = frames per second (higher is better; 30 is the camera target).
Frame Match \% = how many frames received via RTSP were also received via WebRTC (100\% = no loss).
Drops = frames that never arrived (gaps in sequence).
Freeze-Bursts = a long gap (freeze) immediately followed by a short gap (catch-up burst) ---
the temporal pattern viewers perceive as a visible stutter.
FB@Keyframe = freeze-bursts where the post-freeze frame is an I-frame (camera-inherent).
FB@Delta = freeze-bursts where the post-freeze frame is a P/B-frame (pipeline/system stall).
Episodes = disruption episodes: consecutive abnormal gaps grouped into a single event.
Fewer drops, freeze-bursts, and episodes mean a smoother video experience.

\vfill
\begin{center}
""" + fig_cmd("fps_delivery") + r"""
\end{center}
\vfill

% ── Page 6: Jitter ────────────────────────────────────────────────
\newpage
\section*{Jitter and Inter-Arrival Timing}

Jitter = how inconsistent the timing between frames is (lower is better). High jitter causes jerky video.
Inter-arrival time = the gap between consecutive frames arriving. At 30~FPS the ideal gap is $\sim$33~ms.
P95/P99 show worst-case gaps: P95 means 95\% of gaps were shorter than this value.
Lower values mean the stream is more consistent and steady.

\vfill
\begin{center}
""" + fig_cmd("jitter") + r"""
\end{center}
\vfill

% ── Page 7: Resources ─────────────────────────────────────────────
\newpage
\section*{System Resource Usage (Pi)}

These metrics were collected every second from the Raspberry Pi during each trial.
System CPU~\% = total CPU load across all cores (lower means more headroom).
MCM CPU~\% = CPU used only by the camera manager process and its children.
MCM RSS = memory (RAM) used by the camera manager (lower is better).
CPU Temperature = Pi chip temperature; sustained high temps can throttle performance.

\vfill
\begin{center}
""" + fig_cmd("resources") + r"""
\end{center}
\vfill

% ── Camera Resources ─────────────────────────────────────────────
\newpage
\section*{Camera SoC: System Resources}

These metrics were collected every $\sim$2 seconds from the IP camera's SoC during each trial.
Camera Temp = SoC temperature (high temperatures may cause encoder throttling).
Camera CPU~\% = total CPU utilisation on the camera processor.
Camera Memory = RAM used on the camera.
Camera TX Rate = outbound network bitrate from the camera (should match the configured stream bitrate).

\vfill
\begin{center}
""" + fig_cmd("camera_resources") + r"""
\end{center}
\vfill

% ── Camera Voltages ──────────────────────────────────────────────
\newpage
\section*{Camera SoC: Voltages \& RX Rate}

Core, CPU, and NPU supply voltages reported by the camera SoC.
Voltage drops under load may indicate power-supply limitations.
RX Rate = inbound network traffic to the camera (control/feedback traffic).

\vfill
\begin{center}
""" + fig_cmd("camera_voltages") + r"""
\end{center}
\vfill

% ── Camera Thermal Detail ────────────────────────────────────────
\newpage
\section*{Camera SoC: Temperature Compensation}

The camera SoC reports per-domain temperature compensation offsets
(core, CPU, NPU). Larger negative values indicate more aggressive
thermal throttling applied by the SoC firmware.

\vfill
\begin{center}
""" + fig_cmd("camera_thermal") + r"""
\end{center}
\vfill

% ── Page 8: Time Series ──────────────────────────────────────────
\newpage
\section*{Time Series (averaged across """ + str(n_clean) + r""" clean trials)}

Each line is the mean value across all trials; the shaded band is $\pm$1 standard deviation.
MCM CPU and temperature were sampled every second; per-frame latency is binned into 5-second windows.
A flat line means stable behaviour; widening bands indicate trial-to-trial variability at that time point.

\vfill
\begin{center}
""" + fig_cmd("timeseries") + r"""
\end{center}
\vfill

% ── Camera Time Series ───────────────────────────────────────────
\newpage
\section*{Camera SoC Time Series (averaged across """ + str(n_clean) + r""" clean trials)}

Camera SoC temperature, CPU usage, and TX bitrate averaged across all clean trials.
The TX bitrate plot is especially important: it shows whether the camera maintained
the configured output bitrate throughout each trial.
A flat line means stable output; drops indicate encoder throttling or stream interruptions.

\vfill
\begin{center}
""" + fig_cmd("camera_timeseries") + r"""
\end{center}
\vfill

% ── Camera Voltage Time Series ───────────────────────────────────
\newpage
\section*{Camera SoC Voltage Time Series (averaged across """ + str(n_clean) + r""" clean trials)}

Core, CPU, and NPU supply voltages over time.
Downward trends may correlate with thermal throttling or power-rail droop under sustained load.

\vfill
\begin{center}
""" + fig_cmd("camera_voltage_ts") + r"""
\end{center}
\vfill

% ── Evolution pages ──────────────────────────────────────────────
\newpage
\section*{Per-Trial Evolution: Latency}

Each point is one complete $\sim$15-minute trial, plotted in chronological order (trial~1 ran first).
Flat lines = consistent behaviour across the night; trends may indicate
thermal, network, or software effects that build up over time.

\vfill
\begin{center}
""" + fig_cmd("evo_latency") + r"""
\end{center}
\vfill

\newpage
\section*{Per-Trial Evolution: FPS \& Stream Quality}

Latency variability (stddev), frame matching, and throughput (FPS) per trial.
Higher FPS and match~\% are better; lower stddev is better.

\vfill
\begin{center}
""" + fig_cmd("evo_quality") + r"""
\end{center}
\vfill

\newpage
\section*{Per-Trial Evolution: Jitter}

Jitter (stddev of inter-arrival) and worst-case inter-arrival gaps (P95) per trial.
Lower values mean a steadier stream.

\vfill
\begin{center}
""" + fig_cmd("evo_jitter") + r"""
\end{center}
\vfill

\newpage
\section*{Per-Trial Evolution: Drops \& Stutters}

Drop events = frames that never arrived; stutter events = visible pauses.
These tend to correlate with network or thermal stress. Lower is better.

\vfill
\begin{center}
""" + fig_cmd("evo_drops") + r"""
\end{center}
\vfill

\newpage
\section*{Per-Trial Evolution: Missed Frames \& Freeze-Bursts}

Estimated total frames lost and freeze-burst events per trial for each client.
Freeze-bursts detect the freeze-then-catch-up pattern (gap $>$ 1.5$\times$ expected
followed by gap $<$ 0.5$\times$ expected). Large values indicate sustained delivery problems.

\vfill
\begin{center}
""" + fig_cmd("evo_missed") + r"""
\end{center}
\vfill

\newpage
\section*{Per-Trial Evolution: Freeze-Burst Attribution \& Episodes}

Freeze-bursts split by cause: \textbf{FB@Keyframe} = camera I-frame delivery spike (inherent to
hardware encoder); \textbf{FB@Delta} = pipeline or system stall (fixable in MCM).
\textbf{Episodes} group consecutive abnormal gaps into single disruption events,
avoiding over-counting from multi-frame cascades.

\vfill
\begin{center}
""" + fig_cmd("evo_fb_attribution") + r"""
\end{center}
\vfill

% ── Stutter Duration Distributions (WebRTC) ──────────────────────
\newpage
\section*{Stutter \& Freeze Duration Distributions (WebRTC)}

How long does each disruption event last?
These histograms pool all individual events across clean trials for the WebRTC client.
\textbf{True Drops} = disruption windows where frames were genuinely lost (expected frames $>$ received gaps).
\textbf{Freeze-Bursts} = disruption windows where frames were delayed then caught up (no frame deficit, burst gaps present).
\textbf{Disruption Episodes} = every disruption window (consecutive abnormal gaps) regardless of classification.
The CDF subplot (bottom-right) compares all three event types on a single axis.
Shorter durations and fewer events mean less visible impact on the video stream.

\vfill
\begin{center}
""" + fig_cmd("stutter_duration_dist_webrtc") + r"""
\end{center}
\vfill

% ── Stutter Frequency Distributions (WebRTC) ─────────────────────
\newpage
\section*{Stutter \& Freeze Frequency Distributions (WebRTC)}

How much time passes between consecutive disruption events of the same type?
These histograms show the inter-event interval --- the time gap between one event ending
and the next event of the same type starting.
Longer intervals mean fewer disruptions per unit time (better quality).
The box plot (bottom-right) summarises the inter-event intervals across all three event types
for both builds.

\vfill
\begin{center}
""" + fig_cmd("stutter_frequency_dist_webrtc") + r"""
\end{center}
\vfill

% ── Stutter Duration Distributions (RTSP) ────────────────────────
\newpage
\section*{Stutter \& Freeze Duration Distributions (RTSP)}

Same analysis as the previous WebRTC page, but for the RTSP client (direct camera feed).
RTSP disruptions reflect camera-side or network issues before MCM processing.

\vfill
\begin{center}
""" + fig_cmd("stutter_duration_dist_rtsp") + r"""
\end{center}
\vfill

% ── Stutter Frequency Distributions (RTSP) ───────────────────────
\newpage
\section*{Stutter \& Freeze Frequency Distributions (RTSP)}

Inter-event interval distributions for the RTSP client.
Longer intervals between disruptions indicate a more stable direct camera feed.

\vfill
\begin{center}
""" + fig_cmd("stutter_frequency_dist_rtsp") + r"""
\end{center}
\vfill

% ── Stutter Temporal Distributions (WebRTC) ──────────────────────
\newpage
\section*{Stutter \& Freeze Runtime Temporal Distribution (WebRTC)}

When during the trial do disruptions occur?
These histograms show event timestamps normalised to percentage of total trial runtime
for the WebRTC client, pooled across all clean trials.
A uniform distribution suggests random, uncorrelated events; clusters near specific
runtime positions suggest systematic triggers (e.g.\ warmup artefacts, thermal
throttling, or resource exhaustion).
The density subplot (bottom-right) overlays all three event types on a single axis.

\vfill
\begin{center}
""" + fig_cmd("stutter_temporal_dist_webrtc") + r"""
\end{center}
\vfill

% ── Stutter Temporal Distributions (RTSP) ────────────────────────
\newpage
\section*{Stutter \& Freeze Runtime Temporal Distribution (RTSP)}

Same temporal analysis as the previous WebRTC page, but for the RTSP client
(direct camera feed).  Temporal clustering here points to camera-side or
network-level patterns rather than MCM processing issues.

\vfill
\begin{center}
""" + fig_cmd("stutter_temporal_dist_rtsp") + r"""
\end{center}
\vfill

\newpage
\section*{Per-Trial Evolution: Bitrate and Frame Size}

These plots show how much compressed video data each client received per trial.
A sudden bitrate or frame-size drop usually means the camera encoder or scene
became easier to compress, which can directly reduce missed frames.

\vfill
\begin{center}
""" + fig_cmd("evo_bitrate") + r"""
\end{center}
\vfill

\newpage
\section*{Per-Trial Evolution: System Resources}

Per-trial averages of CPU load, memory usage, and temperature on the Raspberry~Pi.
Trends here often explain trends in the stream-quality metrics above.

\vfill
\begin{center}
""" + fig_cmd("evo_resources") + r"""
\end{center}
\vfill

% ── Evolution: Camera SoC ────────────────────────────────────────
\newpage
\section*{Per-Trial Evolution: Camera SoC Resources}

Per-trial averages of the camera SoC temperature, CPU load, memory usage,
and outbound TX bitrate. TX bitrate stability is critical: a drop means
the camera reduced its encoding rate, which directly affects stream quality.

\vfill
\begin{center}
""" + fig_cmd("evo_camera") + r"""
\end{center}
\vfill

% ── Evolution: Camera Voltages ───────────────────────────────────
\newpage
\section*{Per-Trial Evolution: Camera Voltages \& RX Rate}

Per-trial averages of the camera's core, CPU, and NPU supply voltages
and the inbound RX rate. Voltage trends across the night may reveal
thermal- or power-related behaviour changes.

\vfill
\begin{center}
""" + fig_cmd("evo_camera_voltage") + r"""
\end{center}
\vfill

% ── P-value Convergence ──────────────────────────────────────────
\newpage
\section*{P-value Convergence}

Each subplot shows how the Mann--Whitney~U p-value evolves as trials accumulate (using all data, including outliers).
A p-value that \textbf{drops and stays flat} below the dashed lines indicates a robust, real effect.
A p-value that \textbf{rises} after an initial dip suggests early significance was a fluke.
Oscillating p-values indicate the metric is too noisy to conclude at the current sample size.

\vfill
\begin{center}
""" + fig_cmd("pvalue_convergence") + r"""
\end{center}
\vfill

\end{document}
"""
    tex_path = out_dir / "report.tex"
    tex_path.write_text(tex)
    return tex_path

# ── Console summary ──────────────────────────────────────────────────────────

def print_summary(data):
    print("\n" + "=" * 80)
    print("  OVERNIGHT A/B TEST SUMMARY")
    print(f"  Baseline: {LABELS[BASELINE]}  |  Improvement: {LABELS[IMPROVED]}")
    print("=" * 80)
    n = len(data[BASELINE])
    print(f"  Paired trials: {n}\n")
    hb, hi = LABELS[BASELINE], LABELS[IMPROVED]
    print(f"  {'Metric':<30} {hb:>14} {hi:>14} {'Delta':>8} {'p-val':>8}")
    print("  " + "-" * 74)
    for name, fn, _goal in METRICS_DEF:
        name = name.replace("\\%", "%")
        b = series(data[BASELINE], fn); i = series(data[IMPROVED], fn)
        p, _ = mwu(b, i); sig = sigmark(p)
        bs = f"{np.mean(b):.2f}" if len(b) else "N/A"
        is_ = f"{np.mean(i):.2f}" if len(i) else "N/A"
        if len(b)>0 and len(i)>0 and np.mean(b)!=0:
            d = 100*(np.mean(i)-np.mean(b))/abs(np.mean(b))
            ds = f"{d:+.1f}%"
        else: ds = "N/A"
        ps = f"{p:.4f}{sig}" if not np.isnan(p) else "N/A"
        print(f"  {name:<30} {bs:>14} {is_:>14} {ds:>8} {ps:>8}")
    print("=" * 80 + "\n")

# ── Main ─────────────────────────────────────────────────────────────────────

def main():
    parser = argparse.ArgumentParser(description="Generate overnight A/B test PDF report (LaTeX)")
    parser.add_argument("data_dir", help="Path to overnight_tests/ directory")
    parser.add_argument("--output", "-o", default=None)
    args = parser.parse_args()

    global LABELS
    base = Path(args.data_dir)
    if not base.is_dir():
        print(f"Error: {base} is not a directory", file=sys.stderr); sys.exit(1)
    output = Path(args.output) if args.output else base / "report.pdf"

    meta_path = base / "metadata.json"
    if meta_path.exists():
        meta = json.loads(meta_path.read_text())
        def _version_from_image(img):
            return img.rsplit(":", 1)[-1] if ":" in img else img
        LABELS = {
            "next": _version_from_image(meta.get("image_next", "next")),
            "beta": _version_from_image(meta.get("image_beta", "beta")),
        }
        print(f"Loaded metadata: next={LABELS['next']}, beta={LABELS['beta']}")
    else:
        print("No metadata.json found; using default labels.")

    print(f"Loading data from {base}...")
    data_all = load_trial_data(base)
    print(f"  Loaded {len(data_all['next'])} next, {len(data_all['beta'])} beta trials")
    if not data_all["next"] or not data_all["beta"]:
        print("Error: need >= 1 trial per image", file=sys.stderr); sys.exit(1)
    data_all = balance_trial_counts(data_all)
    print(f"  Balanced to {len(data_all[BASELINE])} paired trials")

    data_clean, outlier_idx, outlier_details = split_outliers(data_all)
    if outlier_idx:
        print(f"  Flagged {len(outlier_idx)} outlier trial(s):")
        for d in outlier_details:
            print(f"    {d['trial']}: {d['flagged_sides']}")
    print(f"  Clean trials: {len(data_clean[BASELINE])}")

    print("\n--- CLEAN summary ---")
    print_summary(data_clean)
    print("--- ALL data summary ---")
    print_summary(data_all)

    work_dir = Path(tempfile.mkdtemp(prefix="mcm_report_"))
    print(f"Work dir: {work_dir}")

    print("Generating plots (using clean data for stats, all data for evolution)...")
    plots = gen_plots(data_clean, data_all, outlier_idx, work_dir)

    print("Building LaTeX document...")
    tex_path = build_latex(data_clean, data_all, outlier_details, plots, work_dir)

    print("Compiling PDF (pdflatex)...")
    for run in range(2):
        result = subprocess.run(
            ["pdflatex", "-interaction=nonstopmode", "-halt-on-error", tex_path.name],
            cwd=work_dir, capture_output=True, text=True)
        if result.returncode != 0 and run == 1:
            print("pdflatex FAILED:", file=sys.stderr)
            print(result.stdout[-3000:], file=sys.stderr)
            sys.exit(1)

    compiled = work_dir / "report.pdf"
    if not compiled.exists():
        print("Error: compiled PDF not found", file=sys.stderr); sys.exit(1)

    shutil.copy2(compiled, output)
    shutil.rmtree(work_dir, ignore_errors=True)
    print(f"Report saved to {output}")


if __name__ == "__main__":
    main()
