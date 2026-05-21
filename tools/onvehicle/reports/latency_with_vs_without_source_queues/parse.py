#!/usr/bin/env python3
"""Parse stream_latency output files and produce a OLD vs NEW comparison."""
import re
import sys
from pathlib import Path

BASE = Path(__file__).resolve().parent
SCENARIOS = ["baseline", "cpu_stress", "net_impair", "combined"]
TRANSPORTS = ["rtsp-0", "rtsp-1", "webrtc-0"]

# Convert any "X.Yms" / "Xus" / "X.Ys" string back to microseconds.
def parse_us(s):
    s = s.strip()
    if s.endswith("us"):
        return float(s[:-2])
    if s.endswith("ms"):
        return float(s[:-2]) * 1_000
    if s.endswith("s"):
        return float(s[:-1]) * 1_000_000
    return float(s)

def fmt_us(us):
    if abs(us) >= 1_000_000:
        return f"{us/1_000_000:.2f}s"
    if abs(us) >= 1_000:
        return f"{us/1_000:.1f}ms"
    return f"{us:.0f}us"

PER_CLIENT_RE = re.compile(
    r"  (?P<name>\S+): (?P<n>\d+) frames, (?P<fps>[\d.]+) fps, (?P<mbps>[\d.]+) Mbps,"
    r" (?P<kb>[\d.]+) KB/frame, jitter\(stddev\)=(?P<jitter>\S+),"
    r" inter-arrival p50=(?P<p50>\S+) p95=(?P<p95>\S+) p99=(?P<p99>\S+) max=(?P<max>\S+)"
)
CLASS_RE = re.compile(
    r"      (?P<cls>I|P)-frames: (?P<n>\d+) frames, size avg=(?P<avg_kb>[\d.]+)KB min=(?P<min_kb>[\d.]+)KB max=(?P<max_kb>[\d.]+)KB,"
    r" inter-arrival p50=(?P<p50>\S+) p95=(?P<p95>\S+) p99=(?P<p99>\S+) max=(?P<max>\S+) stddev=(?P<stddev>\S+)"
)
PAIR_RE = re.compile(
    r"  (?P<a>\S+) -> (?P<b>\S+) \((?P<matched>\d+)/(?P<total>\d+) matched, (?P<pct>\d+)%\):"
    r"  min=(?P<min>\S+)  avg=(?P<avg>\S+)  p50=(?P<p50>\S+)  p95=(?P<p95>\S+)  p99=(?P<p99>\S+)  max=(?P<max>\S+)"
    r"  stddev=(?P<stddev>\S+)  rfc_jitter=(?P<jitter>\S+)"
)

def parse_file(p):
    text = p.read_text()
    res = {"per_client": {}, "pairs": {}}
    cur_client = None
    for line in text.splitlines():
        m = PER_CLIENT_RE.match(line)
        if m:
            cur_client = m["name"]
            res["per_client"][cur_client] = {
                "n": int(m["n"]),
                "fps": float(m["fps"]),
                "mbps": float(m["mbps"]),
                "kb": float(m["kb"]),
                "jitter_us": parse_us(m["jitter"]),
                "p50": parse_us(m["p50"]),
                "p95": parse_us(m["p95"]),
                "p99": parse_us(m["p99"]),
                "max": parse_us(m["max"]),
                "I": None,
                "P": None,
            }
            continue
        m = CLASS_RE.match(line)
        if m and cur_client is not None:
            res["per_client"][cur_client][m["cls"]] = {
                "n": int(m["n"]),
                "avg_kb": float(m["avg_kb"]),
                "p50": parse_us(m["p50"]),
                "p95": parse_us(m["p95"]),
                "p99": parse_us(m["p99"]),
                "max": parse_us(m["max"]),
                "stddev": parse_us(m["stddev"]),
            }
            continue
        m = PAIR_RE.match(line)
        if m:
            key = (m["a"], m["b"])
            res["pairs"][key] = {
                "matched": int(m["matched"]),
                "total": int(m["total"]),
                "pct": int(m["pct"]),
                "p50": parse_us(m["p50"]),
                "p95": parse_us(m["p95"]),
                "p99": parse_us(m["p99"]),
                "max": parse_us(m["max"]),
                "stddev": parse_us(m["stddev"]),
                "rfc_jitter": parse_us(m["jitter"]),
            }
    return res

def diff_pct(old, new):
    """Format a (old, new, delta_pct) cell.  Uses NEW-OLD delta and % change vs OLD."""
    if old is None or new is None:
        return f"{fmt_us(new) if new is not None else '-':>8}  (-)"
    delta = new - old
    pct = (delta / old * 100) if old != 0 else 0.0
    sign = "+" if delta >= 0 else ""
    return f"{fmt_us(new):>8} ({sign}{pct:.0f}%)"

data = {}
for scen in SCENARIOS:
    data[scen] = {
        "old": parse_file(BASE / f"old_{scen}.txt"),
        "new": parse_file(BASE / f"new_{scen}.txt"),
    }

# ------- Section 1: Sample sizes per scenario -------
print("=" * 96)
print("SAMPLE SIZES (NEW = no source queues, OLD = with source queues)")
print("=" * 96)
print(f"{'Scenario':<14}  {'Transport':<10}  {'OLD I':>5} {'NEW I':>5}  {'OLD P':>6} {'NEW P':>6}")
for scen in SCENARIOS:
    for t in TRANSPORTS:
        old_pc = data[scen]["old"]["per_client"].get(t, {})
        new_pc = data[scen]["new"]["per_client"].get(t, {})
        old_i = (old_pc.get("I") or {}).get("n", 0)
        new_i = (new_pc.get("I") or {}).get("n", 0)
        old_p = (old_pc.get("P") or {}).get("n", 0)
        new_p = (new_pc.get("P") or {}).get("n", 0)
        print(f"{scen:<14}  {t:<10}  {old_i:>5} {new_i:>5}  {old_p:>6} {new_p:>6}")

# ------- Section 2: P-frame inter-arrival -------
def section(title, getter):
    print()
    print("=" * 116)
    print(title)
    print("=" * 116)
    header = f"{'Scenario':<14}  {'Transport':<10}  " + "  ".join(f"{m:>18}" for m in ["p50 (OLD/NEW)", "p95 (OLD/NEW)", "p99 (OLD/NEW)", "stddev (OLD/NEW)"])
    print(header)
    for scen in SCENARIOS:
        for t in TRANSPORTS:
            old_obj = getter(data[scen]["old"]["per_client"].get(t, {}))
            new_obj = getter(data[scen]["new"]["per_client"].get(t, {}))
            if not old_obj or not new_obj:
                line = f"{scen:<14}  {t:<10}  " + "  ".join(["{:>18}".format("-")] * 4)
                print(line)
                continue
            cells = []
            for k in ("p50", "p95", "p99", "stddev"):
                cells.append("{:>8} / {:<8}".format(fmt_us(old_obj[k]), fmt_us(new_obj[k])))
            print(f"{scen:<14}  {t:<10}  " + "  ".join(cells))

section("P-FRAME INTER-ARRIVAL (OLD vs NEW)", lambda pc: pc.get("P") or {})
section("I-FRAME INTER-ARRIVAL (OLD vs NEW)", lambda pc: pc.get("I") or {})

# ------- Section 3: Pairwise latency  -------
print()
print("=" * 116)
print("PAIRWISE LATENCY DELTAS (OLD vs NEW) -- p50 / p95 / p99 / stddev / rfc_jitter")
print("=" * 116)
print(f"{'Scenario':<14}  {'Pair':<28}  {'matched% O/N':<14}  " + "  ".join(f"{m:>18}" for m in ["p50", "p95", "p99", "stddev", "rfc_jitter"]))
pairs = [("rtsp-0", "rtsp-1"), ("rtsp-0", "webrtc-0"), ("rtsp-1", "webrtc-0")]
for scen in SCENARIOS:
    for p in pairs:
        oldp = data[scen]["old"]["pairs"].get(p)
        newp = data[scen]["new"]["pairs"].get(p)
        if not oldp or not newp:
            continue
        pname = f"{p[0]} -> {p[1]}"
        matched = f"{oldp['pct']}%/{newp['pct']}%"
        cells = []
        for k in ("p50", "p95", "p99", "stddev", "rfc_jitter"):
            cells.append("{:>8} / {:<8}".format(fmt_us(oldp[k]), fmt_us(newp[k])))
        print(f"{scen:<14}  {pname:<28}  {matched:<14}  " + "  ".join(cells))

print()
print("=" * 96)
print("CONFIDENCE NOTES")
print("=" * 96)
print("- P-frame: ~8500 samples/scenario -> p50/p95/p99 statistically robust")
print("- I-frame: ~150 samples/scenario for non-impaired -> p50/p95 robust, p99 marginal (~1.5 samples in 1% tail)")
print("- I-frame WebRTC under net_impair / combined: very few samples (1-94) -> stats only directional")
print("- Pairwise: ~8000 matched samples/scenario -> all percentiles robust")
