#!/usr/bin/env python3
"""Dump all /proc/umap/* entries from a HiSilicon camera via telnet."""

import os
import sys
import time

sys.path.insert(0, os.path.join(os.path.dirname(__file__), "..", "examples", "stream_latency"))
from camera_monitor import CameraTelnet


def clean_cmd(tn, command, timeout=5):
    raw = tn.cmd(command, timeout=timeout)
    lines = raw.splitlines()
    if lines and command[:20] in lines[0]:
        lines = lines[1:]
    if lines and "# " in lines[-1] and len(lines[-1].strip()) < 10:
        lines = lines[:-1]
    return "\n".join(lines)


def main():
    import argparse
    parser = argparse.ArgumentParser()
    parser.add_argument("host")
    parser.add_argument("--user", default="root")
    parser.add_argument("--password", required=True)
    parser.add_argument("--output-dir", required=True)
    args = parser.parse_args()

    os.makedirs(args.output_dir, exist_ok=True)

    tn = CameraTelnet(args.host, timeout=10)
    if not tn.login(args.user, args.password):
        print("Login failed!")
        sys.exit(1)

    # Get clean file list without color codes
    listing = clean_cmd(tn, "ls /proc/umap/ 2>/dev/null | cat", timeout=3)
    entries = [e.strip() for e in listing.split() if e.strip() and not e.startswith("[")]

    print(f"Found {len(entries)} /proc/umap entries: {', '.join(entries)}")

    all_output = []
    all_output.append(f"Camera SoC /proc/umap Dump")
    all_output.append(f"Host: {args.host}")
    all_output.append(f"Timestamp: {time.strftime('%Y-%m-%d %H:%M:%S %Z')}")
    all_output.append(f"Entries: {len(entries)}")
    all_output.append("")

    for entry in entries:
        path = f"/proc/umap/{entry}"
        print(f"  Reading {path}...")
        content = clean_cmd(tn, f"cat {path} 2>&1", timeout=8)
        all_output.append(f"{'='*72}")
        all_output.append(f"  {path}")
        all_output.append(f"{'='*72}")
        all_output.append(content)
        all_output.append("")

        # Save individual files too
        with open(os.path.join(args.output_dir, f"umap_{entry}.txt"), "w") as f:
            f.write(content + "\n")

    # Additional useful commands for embedded engineers
    extra_cmds = [
        ("cat /proc/media-mem 2>&1", "media_mem.txt"),
        ("cat /proc/interrupts", "interrupts.txt"),
        ("cat /proc/softirqs", "softirqs.txt"),
        ("cat /proc/meminfo", "meminfo.txt"),
        ("cat /proc/slabinfo 2>&1", "slabinfo.txt"),
        ("cat /proc/vmstat", "vmstat.txt"),
        ("cat /proc/buddyinfo", "buddyinfo.txt"),
        ("cat /proc/net/dev", "net_dev.txt"),
        ("cat /proc/net/snmp", "net_snmp.txt"),
        ("cat /proc/net/netstat", "net_netstat.txt"),
        ("cat /proc/net/tcp", "net_tcp.txt"),
        ("cat /proc/net/udp", "net_udp.txt"),
        ("cat /proc/cpuinfo", "cpuinfo.txt"),
        ("top -b -n1 2>&1", "top.txt"),
        ("ps -ef", "ps.txt"),
        ("lsmod", "lsmod.txt"),
        ("mount", "mount.txt"),
        ("df -h 2>&1", "df.txt"),
        ("ifconfig -a 2>&1", "ifconfig.txt"),
        ("netstat -tlnp 2>&1", "netstat_tcp.txt"),
        ("netstat -ulnp 2>&1", "netstat_udp.txt"),
        ("dmesg 2>&1", "dmesg.txt"),
        ("cat /proc/cmdline", "cmdline.txt"),
        ("uptime", "uptime.txt"),
        ("free 2>&1", "free.txt"),
    ]

    for cmd, fname in extra_cmds:
        print(f"  Running {cmd[:40]}...")
        content = clean_cmd(tn, cmd, timeout=10)
        with open(os.path.join(args.output_dir, fname), "w") as f:
            f.write(content + "\n")
        all_output.append(f"{'='*72}")
        all_output.append(f"  {cmd}")
        all_output.append(f"{'='*72}")
        all_output.append(content)
        all_output.append("")

    report_path = os.path.join(args.output_dir, "full_dump.txt")
    with open(report_path, "w") as f:
        f.write("\n".join(all_output))

    tn.close()
    print(f"\nDone. Combined report: {report_path}")
    print(f"Individual files: {args.output_dir}/")


if __name__ == "__main__":
    main()
