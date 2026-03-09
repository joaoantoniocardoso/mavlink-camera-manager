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
LABELS = {"next": "1.4.4-next.7", "beta": "1.4.4-beta.1"}

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

# ── Data loading ─────────────────────────────────────────────────────────────

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
CAMERA_FIXED_BITRATE_MBPS = 32.768
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


def paired_boxplot(ax, bv, iv, ylabel, title):
    bp = ax.boxplot([bv, iv],
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
    bp = ax.boxplot([bv1, iv1, bv2, iv2],
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
        ("drop_events", r"Drop Events [$\downarrow$ lower is better]"),
        ("stutter_events", r"Stutter Events [$\downarrow$ lower is better]"),
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
          ("Drops",    lambda e: extract_client(e,"webrtc-0")["stutters"]["drop_events"]),
          ("Missed",   lambda e: extract_client(e,"webrtc-0")["stutters"]["estimated_missed_frames"])]
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
    bd_ = series(data[BASELINE], lambda e: extract_client(e,"webrtc-0")["stutters"]["drop_events"])
    id2 = series(data[IMPROVED], lambda e: extract_client(e,"webrtc-0")["stutters"]["drop_events"])
    paired_boxplot(ax, bd_, id2, "Count", r"WebRTC Drop Events [$\downarrow$]")
    ax = axes[1,1]
    bs_ = series(data[BASELINE], lambda e: extract_client(e,"webrtc-0")["stutters"]["stutter_events"])
    is2 = series(data[IMPROVED], lambda e: extract_client(e,"webrtc-0")["stutters"]["stutter_events"])
    paired_boxplot(ax, bs_, is2, "Count", r"WebRTC Stutter Events [$\downarrow$]")
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
            ("RTSP Drops" + D, "count", lambda e: extract_client(e, "rtsp-0")["stutters"]["drop_events"]),
            ("WebRTC Drops" + D, "count", lambda e: extract_client(e, "webrtc-0")["stutters"]["drop_events"]),
            ("RTSP Stutters" + D, "count", lambda e: extract_client(e, "rtsp-0")["stutters"]["stutter_events"]),
            ("WebRTC Stutters" + D, "count", lambda e: extract_client(e, "webrtc-0")["stutters"]["stutter_events"]),
        ]),
        ("evo_missed", [
            ("RTSP Missed Frames" + D, "count", lambda e: extract_client(e, "rtsp-0")["stutters"]["estimated_missed_frames"]),
            ("WebRTC Missed Frames" + D, "count", lambda e: extract_client(e, "webrtc-0")["stutters"]["estimated_missed_frames"]),
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

    # -- p-value convergence (single page) – uses data_all --
    CONVERGENCE_METRICS = [
        ("Latency Mean", lambda e: extract_pair(e)["delta_mean_us"] / 1000),
        ("Latency P50", lambda e: extract_pair(e)["delta_p50_us"] / 1000),
        ("WebRTC FPS", lambda e: extract_client(e, "webrtc-0")["fps"]),
        ("Frame Match %", lambda e: extract_pair(e)["match_pct"]),
        ("MCM CPU %", lambda e: _stats_trial_mean(e, "mcm_cpu_pct")),
        ("MCM RSS (MB)", lambda e: _stats_trial_mean(e, "mcm_rss_mb")),
        ("WebRTC Jitter", lambda e: extract_client(e, "webrtc-0")["jitter_stddev_us"] / 1000),
        ("WebRTC Drops", lambda e: extract_client(e, "webrtc-0")["stutters"]["drop_events"]),
    ]

    n_conv = len(CONVERGENCE_METRICS)
    nrows_c = (n_conv + 1) // 2
    fig, axes = plt.subplots(nrows_c, 2, figsize=(PLOT_W, PLOT_H * nrows_c / 2))
    fig.subplots_adjust(hspace=0.75, wspace=0.35)
    if nrows_c == 1:
        axes = axes.reshape(1, -1)

    n_all = min(len(data_all[BASELINE]), len(data_all[IMPROVED]))
    for idx, (metric_name, fn) in enumerate(CONVERGENCE_METRICS):
        ax = axes[idx // 2, idx % 2]
        ns, pvals = [], []
        for k in range(2, n_all + 1):
            b = series(data_all[BASELINE][:k], fn)
            i = series(data_all[IMPROVED][:k], fn)
            if len(b) >= 2 and len(i) >= 2:
                p, _ = mwu(b, i)
                ns.append(k)
                pvals.append(p if not np.isnan(p) else 1.0)
        if ns:
            ax.plot(ns, pvals, "o-", color=IMPROVED_COLOR, markersize=4, linewidth=1.2)
            ax.axhline(0.05, color=ORANGE, linestyle="--", linewidth=1, alpha=0.8, label="p = 0.05")
            ax.axhline(0.01, color=RED, linestyle="--", linewidth=1, alpha=0.8, label="p = 0.01")
            ax.set_yscale("log")
            ax.set_ylim(1e-5, 1.5)
            ax.set_xlabel("Paired trials (n)")
            ax.set_ylabel("p-value")
            ax.set_title(metric_name)
            ax.xaxis.set_major_locator(plt.MaxNLocator(integer=True))
            place_legend(ax, ncol=2)
    for idx in range(n_conv, nrows_c * 2):
        axes[idx // 2, idx % 2].set_visible(False)
    p = out_dir / "pvalue_convergence.pdf"
    save_fig(fig, p)
    plots["pvalue_convergence"] = p

    return plots

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
    ("RTSP Jitter (ms)",      lambda e: extract_client(e,"rtsp-0")["jitter_stddev_us"]/1000,          DOWN),
    ("WebRTC Jitter (ms)",    lambda e: extract_client(e,"webrtc-0")["jitter_stddev_us"]/1000,        DOWN),
    ("RTSP Drops",            lambda e: extract_client(e,"rtsp-0")["stutters"]["drop_events"],        DOWN),
    ("WebRTC Drops",          lambda e: extract_client(e,"webrtc-0")["stutters"]["drop_events"],      DOWN),
    ("RTSP Stutters",         lambda e: extract_client(e,"rtsp-0")["stutters"]["stutter_events"],     DOWN),
    ("WebRTC Stutters",       lambda e: extract_client(e,"webrtc-0")["stutters"]["stutter_events"],   DOWN),
    ("Missed Frames (RTSP)",  lambda e: extract_client(e,"rtsp-0")["stutters"]["estimated_missed_frames"],  DOWN),
    ("Missed Frames (WebRTC)",lambda e: extract_client(e,"webrtc-0")["stutters"]["estimated_missed_frames"],DOWN),
    ("Frame Match \\%",       lambda e: extract_pair(e)["match_pct"],                                 UP),
    ("System CPU \\%",        lambda e: _stats_trial_mean(e, "sys_cpu_pct"),                          DOWN),
    ("MCM CPU \\%",           lambda e: _stats_trial_mean(e, "mcm_cpu_pct"),                          DOWN),
    ("MCM RSS (MB)",          lambda e: _stats_trial_mean(e, "mcm_rss_mb"),                           DOWN),
    ("CPU Temp (°C)",         lambda e: _stats_trial_mean(e, "cpu_temp_c"),                           DOWN),
]


def _stats_trial_mean(entry, col):
    """Return per-trial mean of a stats column (used by METRICS_DEF)."""
    df = entry.get("stats", pd.DataFrame())
    if col not in df.columns:
        return None
    s = pd.to_numeric(df[col][WARMUP_ROWS:], errors="coerce").dropna()
    return float(s.mean()) if len(s) > 0 else None

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

\begin{table}[!htbp]
\centering
\small
\caption*{\textbf{Summary --- outliers excluded} ($n = """ + str(n_clean) + r"""$ clean paired trials)}
\begin{tabular}{l c r r r r c}
\toprule
\textbf{Metric} & \textbf{Goal} & \textbf{""" + tex_esc(base_lbl) + r""" (baseline)} & \textbf{""" + tex_esc(impr_lbl) + r""" (new)} & \textbf{Delta} & \textbf{p-value} & \textbf{Sig} \\
\midrule
""" + clean_table + r"""
\bottomrule
\end{tabular}
\end{table}

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

\begin{table}[!htbp]
\centering
\small
\caption*{\textbf{Summary --- all trials} ($n = """ + str(n_all) + r"""$, including outliers)}
\begin{tabular}{l c r r r r c}
\toprule
\textbf{Metric} & \textbf{Goal} & \textbf{""" + tex_esc(base_lbl) + r""" (baseline)} & \textbf{""" + tex_esc(impr_lbl) + r""" (new)} & \textbf{Delta} & \textbf{p-value} & \textbf{Sig} \\
\midrule
""" + all_table + r"""
\bottomrule
\end{tabular}
\end{table}

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

% ── Page 5: FPS and Frame Delivery ────────────────────────────────
\newpage
\section*{FPS and Frame Delivery}

FPS = frames per second (higher is better; 30 is the camera target).
Frame Match \% = how many frames received via RTSP were also received via WebRTC (100\% = no loss).
Drops = frames that never arrived (gaps in sequence).
Stutters = visible pauses (consecutive frames delayed).
Fewer drops and stutters mean a smoother video experience.

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
\section*{Per-Trial Evolution: Missed Frames}

Estimated total frames lost per trial for each client.
Large values indicate sustained delivery problems.

\vfill
\begin{center}
""" + fig_cmd("evo_missed") + r"""
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

    base = Path(args.data_dir)
    if not base.is_dir():
        print(f"Error: {base} is not a directory", file=sys.stderr); sys.exit(1)
    output = Path(args.output) if args.output else base / "report.pdf"

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
