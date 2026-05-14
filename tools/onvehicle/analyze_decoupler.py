#!/usr/bin/env python3
"""Analyse a run_decoupler_matrix.sh results directory.

For every (variant, condition) cell aggregates per-rep stream_latency.csv
and cpu.csv files into:
  - per-protocol latency percentiles (median, p95, p99, max) with bootstrap
    95% CIs across the per-frame samples;
  - jitter (stddev of inter-arrival times) in microseconds;
  - CPU mean and p95 (% of all cores) for MCM;
  - v4l2 drops aggregated from mcm.log;
  - pairwise Mann-Whitney U tests + Cliff's delta vs the lowest-median
    variant within the same condition, Bonferroni-corrected.

Emits:
  <results_dir>/summary.md   human-readable comparison
  <results_dir>/summary.csv  flat table for downstream tooling

Usage:
  tools/onvehicle/analyze_decoupler.py <results_dir>

Inputs expected per cell directory:
  <results_dir>/<variant>_<condition>_<rep>/
    stream_latency.csv      content_hash, <proto>_pts_ms, <proto>_arrival_us, <proto>_bytes
    cpu.csv                 ts_iso, ..., mcm_total_pct, sys_*_pct
    mcm.log                 mcm_inst log lines for the cell window
    meta.json               variant, condition, rep
"""

from __future__ import annotations

import csv
import json
import math
import re
import sys
from collections import defaultdict
from pathlib import Path

try:
    import numpy as np
except ImportError:
    sys.exit("analyze_decoupler.py needs numpy: `pip install --user numpy scipy`")

try:
    from scipy import stats
except ImportError:
    sys.exit("analyze_decoupler.py needs scipy: `pip install --user numpy scipy`")


PROTOCOLS = ["rtsp-0", "udp-0", "webrtc-0"]
PAIRS = [("rtsp-0", "udp-0"), ("rtsp-0", "webrtc-0"), ("udp-0", "webrtc-0")]


def percentile(arr: np.ndarray, q: float) -> float:
    if arr.size == 0:
        return float("nan")
    return float(np.percentile(arr, q))


def bootstrap_ci(arr: np.ndarray, stat_fn, n_boot=2000, alpha=0.05) -> tuple[float, float]:
    if arr.size == 0:
        return (float("nan"), float("nan"))
    rng = np.random.default_rng(0xC0FFEE)
    idx = rng.integers(0, arr.size, size=(n_boot, arr.size))
    boots = np.fromiter((stat_fn(arr[i]) for i in idx), dtype=float, count=n_boot)
    lo = float(np.percentile(boots, 100 * alpha / 2))
    hi = float(np.percentile(boots, 100 * (1 - alpha / 2)))
    return (lo, hi)


def cliffs_delta(a: np.ndarray, b: np.ndarray) -> float:
    """Cliff's delta = P(A > B) - P(A < B). Range [-1, 1]."""
    if a.size == 0 or b.size == 0:
        return float("nan")
    # Subsample large arrays for tractable O(NM) comparison; the estimate
    # is still well-behaved at 5000x5000.
    cap = 5000
    if a.size > cap:
        a = np.random.default_rng(1).choice(a, cap, replace=False)
    if b.size > cap:
        b = np.random.default_rng(2).choice(b, cap, replace=False)
    gt = (a[:, None] > b[None, :]).sum()
    lt = (a[:, None] < b[None, :]).sum()
    n = a.size * b.size
    return (gt - lt) / n


def cliffs_label(d: float) -> str:
    a = abs(d)
    if a < 0.147:
        return "negligible"
    if a < 0.33:
        return "small"
    if a < 0.474:
        return "medium"
    return "large"


def read_stream_latency_csv(path: Path) -> dict[str, np.ndarray]:
    """Return per-protocol arrays of pairwise relative arrival times (us).

    Latency proxy: for every protocol P, deltas vs the first protocol that
    has a value for the same content_hash. We return raw arrival_us per
    protocol so callers can compute jitter and pairwise deltas separately.
    """
    out: dict[str, list[int]] = {p: [] for p in PROTOCOLS}
    if not path.exists():
        return {p: np.array([], dtype=np.int64) for p in PROTOCOLS}
    with path.open() as f:
        reader = csv.DictReader(f)
        if not reader.fieldnames:
            return {p: np.array([], dtype=np.int64) for p in PROTOCOLS}
        for row in reader:
            for proto in PROTOCOLS:
                key = f"{proto}_arrival_us"
                v = row.get(key, "")
                if v != "":
                    try:
                        out[proto].append(int(v))
                    except ValueError:
                        pass
    return {p: np.array(out[p], dtype=np.int64) for p in PROTOCOLS}


def read_pairwise_deltas(path: Path) -> dict[tuple[str, str], np.ndarray]:
    """Pairwise arrival_us delta = b - a (us), per row that has both."""
    out: dict[tuple[str, str], list[int]] = {pair: [] for pair in PAIRS}
    if not path.exists():
        return {pair: np.array([], dtype=np.int64) for pair in PAIRS}
    with path.open() as f:
        reader = csv.DictReader(f)
        if not reader.fieldnames:
            return {pair: np.array([], dtype=np.int64) for pair in PAIRS}
        for row in reader:
            for a, b in PAIRS:
                va = row.get(f"{a}_arrival_us", "")
                vb = row.get(f"{b}_arrival_us", "")
                if va != "" and vb != "":
                    try:
                        out[(a, b)].append(int(vb) - int(va))
                    except ValueError:
                        pass
    return {pair: np.array(out[pair], dtype=np.int64) for pair in PAIRS}


def jitter_us(arrivals_us: np.ndarray) -> float:
    """stddev of inter-arrival times in microseconds."""
    if arrivals_us.size < 2:
        return float("nan")
    diffs = np.diff(np.sort(arrivals_us))
    return float(np.std(diffs))


def read_cpu_csv(path: Path) -> dict[str, np.ndarray]:
    cols: dict[str, list[float]] = defaultdict(list)
    if not path.exists():
        return {}
    with path.open() as f:
        reader = csv.DictReader(f)
        if not reader.fieldnames:
            return {}
        for row in reader:
            for k, v in row.items():
                if k in ("ts_iso", "uptime_ns"):
                    continue
                try:
                    cols[k].append(float(v))
                except (TypeError, ValueError):
                    pass
    return {k: np.asarray(v, dtype=np.float64) for k, v in cols.items()}


V4L2_DROPS_RE = re.compile(
    r'mcm_inst="v4l2_drops".*?pipeline_id=([0-9a-f-]+).*?'
    r'buffers_1s=(\d+) drops_1s=(\d+) buffers_total=(\d+) drops_total=(\d+)'
)
# `fetch_logs_for_window` greps the entire current-hour log, which carries
# markers from prior cells in the same hour. We re-filter here by the cell's
# (start_iso, end_iso) ISO timestamps and the cell's `producer_id`
# (pipeline_id). The first ~`WARMUP_DROP_GUARD_S` seconds after `start_iso`
# also get masked off because the v4l2src startup transient routinely
# shows drops_1s=8-9 for the first 1-2 windows before steady state.
TS_RE = re.compile(r'^(\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(?:\.\d+)?)Z')
WARMUP_DROP_GUARD_S = 3.0


def _parse_iso(s: str) -> float:
    """Accepts the variations we actually see:
       - mcm.log line prefix:  2026-05-20T19:20:42.129266Z   (UTC, no offset)
       - meta.json start_iso:  2026-05-20T16:24:18-03:00     (local + offset)
    Returns absolute seconds for ordering across the two."""
    from datetime import datetime, timezone

    # ``fromisoformat`` accepts both forms in 3.11+. Normalise the trailing
    # "Z" to "+00:00" so older interpreters cope too.
    s = s.replace("Z", "+00:00")
    dt = datetime.fromisoformat(s)
    if dt.tzinfo is None:
        dt = dt.replace(tzinfo=timezone.utc)
    return dt.timestamp()


def read_v4l2_drops(
    path: Path,
    pipeline_id: str | None = None,
    start_iso: str | None = None,
    end_iso: str | None = None,
) -> dict[str, float | int]:
    if not path.exists():
        return {"windows": 0, "drops_1s_sum": 0, "drops_1s_max": 0, "windows_with_drops": 0}
    drops_sum = 0
    drops_max = 0
    windows = 0
    windows_with_drops = 0
    start_ts = _parse_iso(start_iso) if start_iso else None
    end_ts = _parse_iso(end_iso) if end_iso else None
    guard_until = start_ts + WARMUP_DROP_GUARD_S if start_ts is not None else None
    with path.open(errors="ignore") as f:
        for line in f:
            m = V4L2_DROPS_RE.search(line)
            if not m:
                continue
            pid, _b1, d1, _bt, _dt = m.group(1), *(int(x) for x in m.groups()[1:])
            if pipeline_id and pid != pipeline_id:
                continue
            ts_match = TS_RE.match(line)
            if ts_match and (start_ts is not None or end_ts is not None):
                ts = _parse_iso(ts_match.group(1))
                if start_ts is not None and ts < start_ts:
                    continue
                if end_ts is not None and ts > end_ts:
                    continue
                if guard_until is not None and ts < guard_until:
                    continue
            windows += 1
            drops_sum += d1
            if d1 > drops_max:
                drops_max = d1
            if d1 > 0:
                windows_with_drops += 1
    return {
        "windows": windows,
        "drops_1s_sum": drops_sum,
        "drops_1s_max": drops_max,
        "windows_with_drops": windows_with_drops,
    }


def cell_dirs(results_dir: Path) -> list[Path]:
    return sorted(p for p in results_dir.iterdir() if p.is_dir() and (p / "meta.json").exists())


def main(argv: list[str]) -> int:
    if len(argv) != 2:
        print(__doc__, file=sys.stderr)
        return 2
    results_dir = Path(argv[1]).resolve()
    if not results_dir.is_dir():
        print(f"Not a directory: {results_dir}", file=sys.stderr)
        return 2

    # Per-cell aggregated data.
    cells: dict[tuple[str, str], dict] = defaultdict(
        lambda: {
            "per_proto_arrivals": defaultdict(list),
            "per_pair_deltas": defaultdict(list),
            "cpu_mcm_total": [],
            "cpu_sys_user": [],
            "cpu_sys_iowait": [],
            "v4l2_drops_1s_max": [],
            "v4l2_drops_1s_sum": [],
            "v4l2_windows_with_drops": [],
            "reps": 0,
        }
    )

    for cd in cell_dirs(results_dir):
        meta = json.loads((cd / "meta.json").read_text())
        key = (meta["variant"], meta["condition"])
        arrivals = read_stream_latency_csv(cd / "stream_latency.csv")
        deltas = read_pairwise_deltas(cd / "stream_latency.csv")
        cpu = read_cpu_csv(cd / "cpu.csv")
        drops = read_v4l2_drops(
            cd / "mcm.log",
            pipeline_id=meta.get("producer_id"),
            start_iso=meta.get("start_iso"),
            end_iso=meta.get("end_iso"),
        )

        c = cells[key]
        for p, arr in arrivals.items():
            if arr.size:
                c["per_proto_arrivals"][p].append(arr)
        for pair, arr in deltas.items():
            if arr.size:
                c["per_pair_deltas"][pair].append(arr)
        for col_key, cell_key in (
            ("mcm_total_pct", "cpu_mcm_total"),
            ("sys_user_pct", "cpu_sys_user"),
            ("sys_iowait_pct", "cpu_sys_iowait"),
        ):
            arr = cpu.get(col_key)
            if arr is not None and arr.size:
                c[cell_key].append(arr)
        c["v4l2_drops_1s_max"].append(drops["drops_1s_max"])
        c["v4l2_drops_1s_sum"].append(drops["drops_1s_sum"])
        c["v4l2_windows_with_drops"].append(drops["windows_with_drops"])
        c["reps"] += 1

    if not cells:
        print(f"No cells found under {results_dir}", file=sys.stderr)
        return 1

    # Per-cell summary stats.
    rows: list[dict[str, object]] = []
    pair_deltas_by_cell: dict[tuple[str, str, tuple[str, str]], np.ndarray] = {}
    for (variant, condition), c in cells.items():
        for pair in PAIRS:
            arrs = c["per_pair_deltas"].get(pair, [])
            if arrs:
                pair_deltas_by_cell[(variant, condition, pair)] = np.concatenate(arrs)

        row: dict[str, object] = {
            "variant": variant,
            "condition": condition,
            "reps": c["reps"],
        }
        for p in PROTOCOLS:
            arrs = c["per_proto_arrivals"].get(p, [])
            arr_all = np.concatenate(arrs) if arrs else np.array([], dtype=np.int64)
            row[f"{p}_jitter_us"] = jitter_us(arr_all)
        for pair in PAIRS:
            label = f"{pair[1]}_minus_{pair[0]}_us"
            arr = pair_deltas_by_cell.get((variant, condition, pair), np.array([], dtype=np.int64))
            row[f"{label}_median"] = percentile(arr, 50)
            row[f"{label}_p95"] = percentile(arr, 95)
            row[f"{label}_p99"] = percentile(arr, 99)
            row[f"{label}_max"] = percentile(arr, 100)
            ci_lo, ci_hi = bootstrap_ci(arr, lambda x: float(np.median(x)))
            row[f"{label}_median_ci_lo"] = ci_lo
            row[f"{label}_median_ci_hi"] = ci_hi
        cpu_all = np.concatenate(c["cpu_mcm_total"]) if c["cpu_mcm_total"] else np.array([])
        row["cpu_mcm_mean"] = float(np.mean(cpu_all)) if cpu_all.size else float("nan")
        row["cpu_mcm_p95"] = percentile(cpu_all, 95)
        row["cpu_mcm_max"] = percentile(cpu_all, 100)
        sys_user = np.concatenate(c["cpu_sys_user"]) if c["cpu_sys_user"] else np.array([])
        row["cpu_sys_user_mean"] = float(np.mean(sys_user)) if sys_user.size else float("nan")
        row["v4l2_drops_1s_max"] = int(max(c["v4l2_drops_1s_max"] or [0]))
        row["v4l2_drops_1s_sum"] = int(sum(c["v4l2_drops_1s_sum"] or [0]))
        row["v4l2_windows_with_drops"] = int(sum(c["v4l2_windows_with_drops"] or [0]))
        rows.append(row)

    # Cross-variant comparisons within each condition.
    comparisons: list[dict[str, object]] = []
    conditions = sorted({c for _, c in cells.keys()})
    variants = sorted({v for v, _ in cells.keys()})
    for condition in conditions:
        for pair in PAIRS:
            label = f"{pair[1]}_minus_{pair[0]}_us"
            per_variant_arr = {
                v: pair_deltas_by_cell.get((v, condition, pair), np.array([], dtype=np.int64))
                for v in variants
            }
            best = None
            best_med = math.inf
            for v in variants:
                arr = per_variant_arr[v]
                if arr.size and float(np.median(arr)) < best_med:
                    best_med = float(np.median(arr))
                    best = v
            if best is None:
                continue
            for v in variants:
                if v == best:
                    continue
                a = per_variant_arr[best]
                b = per_variant_arr[v]
                if a.size == 0 or b.size == 0:
                    continue
                mw = stats.mannwhitneyu(a, b, alternative="two-sided")
                p_bonf = min(1.0, float(mw.pvalue) * (len(variants) - 1))
                cd_val = cliffs_delta(b, a)
                comparisons.append(
                    {
                        "condition": condition,
                        "pair": label,
                        "best_variant": best,
                        "vs_variant": v,
                        "best_median_us": best_med,
                        "vs_median_us": float(np.median(b)),
                        "p_bonferroni": p_bonf,
                        "cliffs_delta": cd_val,
                        "effect": cliffs_label(cd_val),
                    }
                )

    # Write summary.csv (flat rows).
    out_csv = results_dir / "summary.csv"
    fields = sorted({k for r in rows for k in r.keys()})
    with out_csv.open("w", newline="") as f:
        w = csv.DictWriter(f, fieldnames=fields)
        w.writeheader()
        for r in rows:
            w.writerow(r)

    # Write summary.md.
    out_md = results_dir / "summary.md"
    with out_md.open("w") as f:
        f.write(f"# UDP decoupler experiment summary\n\n")
        f.write(f"Source: `{results_dir}`\n\n")
        f.write(f"Cells: {len(cells)} (variants {variants}, conditions {conditions})\n\n")

        f.write("## Per-cell pairwise arrival deltas (us)\n\n")
        f.write("| variant | condition | reps | pair | median | p95 | p99 | max | 95% CI(median) |\n")
        f.write("|---|---|---:|---|---:|---:|---:|---:|---|\n")
        for r in sorted(rows, key=lambda r: (r["condition"], r["variant"])):
            for pair in PAIRS:
                label = f"{pair[1]}_minus_{pair[0]}_us"
                med = r.get(f"{label}_median")
                p95 = r.get(f"{label}_p95")
                p99 = r.get(f"{label}_p99")
                mx = r.get(f"{label}_max")
                lo = r.get(f"{label}_median_ci_lo")
                hi = r.get(f"{label}_median_ci_hi")
                f.write(
                    f"| {r['variant']} | {r['condition']} | {r['reps']} | "
                    f"{pair[1]} - {pair[0]} | "
                    f"{med:.1f} | {p95:.1f} | {p99:.1f} | {mx:.1f} | "
                    f"[{lo:.1f}, {hi:.1f}] |\n"
                )

        f.write("\n## CPU and drops\n\n")
        f.write("| variant | condition | reps | cpu_mcm_mean(%) | cpu_mcm_p95(%) | sys_user_mean(%) | drops_max | drops_sum | windows_w/drops |\n")
        f.write("|---|---|---:|---:|---:|---:|---:|---:|---:|\n")
        for r in sorted(rows, key=lambda r: (r["condition"], r["variant"])):
            f.write(
                f"| {r['variant']} | {r['condition']} | {r['reps']} | "
                f"{r['cpu_mcm_mean']:.2f} | {r['cpu_mcm_p95']:.2f} | {r['cpu_sys_user_mean']:.2f} | "
                f"{r['v4l2_drops_1s_max']} | {r['v4l2_drops_1s_sum']} | {r['v4l2_windows_with_drops']} |\n"
            )

        if comparisons:
            f.write("\n## Cross-variant comparison (within condition)\n\n")
            f.write("Best variant per (condition, pair) is the one with the lowest median pairwise arrival delta.\n")
            f.write("Mann-Whitney U test (Bonferroni-corrected across variants). Cliff's delta is signed: >0 means the\n")
            f.write("comparison variant has *larger* (worse) deltas than the best.\n\n")
            f.write("| condition | pair | best | vs | best_median(us) | vs_median(us) | p_bonf | Cliff's d | effect |\n")
            f.write("|---|---|---|---|---:|---:|---:|---:|---|\n")
            for c in comparisons:
                f.write(
                    f"| {c['condition']} | {c['pair']} | {c['best_variant']} | {c['vs_variant']} | "
                    f"{c['best_median_us']:.1f} | {c['vs_median_us']:.1f} | "
                    f"{c['p_bonferroni']:.4g} | {c['cliffs_delta']:+.3f} | {c['effect']} |\n"
                )

    print(f"Wrote {out_md}")
    print(f"Wrote {out_csv}")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
