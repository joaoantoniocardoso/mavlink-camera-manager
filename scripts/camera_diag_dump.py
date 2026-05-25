#!/usr/bin/env python3
"""
Comprehensive camera SoC diagnostic dump via telnet.

Captures everything an embedded Linux engineer would need to diagnose
degraded camera behavior: kernel logs, proc/sys filesystem state,
HiSilicon media pipeline internals, network state, process info, etc.
"""

import json
import os
import socket
import sys
import time

sys.path.insert(0, os.path.join(os.path.dirname(__file__), "..", "examples", "stream_latency"))
from camera_monitor import CameraTelnet


def dump_cmd(tn, command, timeout=5):
    """Run a command and return clean output (strip echo + prompt)."""
    raw = tn.cmd(command, timeout=timeout)
    lines = raw.splitlines()
    if lines and command in lines[0]:
        lines = lines[1:]
    if lines and "# " in lines[-1] and len(lines[-1].strip()) < 10:
        lines = lines[:-1]
    return "\n".join(lines)


def collect_section(tn, label, commands, timeout=5):
    """Collect multiple commands into a single section string."""
    parts = [f"{'='*72}", f"  {label}", f"{'='*72}", ""]
    for cmd in commands:
        parts.append(f"--- {cmd} ---")
        try:
            out = dump_cmd(tn, cmd, timeout=timeout)
            parts.append(out)
        except Exception as e:
            parts.append(f"[ERROR: {e}]")
        parts.append("")
    return "\n".join(parts)


def main():
    import argparse

    parser = argparse.ArgumentParser(description="Full camera SoC diagnostic dump")
    parser.add_argument("host", help="Camera IP address")
    parser.add_argument("--user", default="root")
    parser.add_argument("--password", required=True)
    parser.add_argument("--output-dir", required=True, help="Directory for dump files")
    args = parser.parse_args()

    os.makedirs(args.output_dir, exist_ok=True)

    print(f"Connecting to {args.host}...")
    tn = CameraTelnet(args.host, timeout=10)
    if not tn.login(args.user, args.password):
        print("Login failed!")
        sys.exit(1)
    print("Connected. Starting diagnostic dump...\n")

    sections = {}
    all_output = []

    # --- 1. Kernel & Boot ---
    s = collect_section(tn, "KERNEL & BOOT", [
        "uname -a",
        "cat /proc/version",
        "cat /proc/cmdline",
        "uptime",
        "cat /proc/uptime",
        "date",
        "cat /proc/sys/kernel/hostname",
    ])
    all_output.append(s)
    print("  [1/14] Kernel & boot info")

    # --- 2. dmesg (full kernel ring buffer) ---
    s = collect_section(tn, "KERNEL RING BUFFER (dmesg)", ["dmesg"], timeout=15)
    all_output.append(s)
    with open(os.path.join(args.output_dir, "dmesg_full.log"), "w") as f:
        f.write(dump_cmd(tn, "dmesg", timeout=15))
    print("  [2/14] dmesg")

    # --- 3. CPU & Interrupts ---
    s = collect_section(tn, "CPU & INTERRUPTS", [
        "cat /proc/cpuinfo",
        "cat /proc/stat",
        "cat /proc/interrupts",
        "cat /proc/softirqs",
        "cat /proc/loadavg",
    ])
    all_output.append(s)
    print("  [3/14] CPU & interrupts")

    # --- 4. Memory ---
    s = collect_section(tn, "MEMORY", [
        "cat /proc/meminfo",
        "cat /proc/vmstat",
        "cat /proc/buddyinfo",
        "cat /proc/slabinfo",
        "cat /proc/sys/vm/min_free_kbytes",
        "cat /proc/sys/vm/overcommit_memory",
        "cat /proc/sys/fs/file-nr",
    ], timeout=8)
    all_output.append(s)
    print("  [4/14] Memory")

    # --- 5. Processes ---
    s = collect_section(tn, "PROCESSES", [
        "ps -ef",
        "ps aux",
        "top -b -n1",
        "cat /proc/sys/kernel/threads-max",
    ], timeout=10)
    all_output.append(s)
    print("  [5/14] Processes")

    # --- 6. HiSilicon /proc/umap (media pipeline internals) ---
    umap_files_raw = dump_cmd(tn, "ls /proc/umap/ 2>/dev/null", timeout=3)
    umap_files = [f.strip() for f in umap_files_raw.split() if f.strip()]

    umap_cmds = [f"cat /proc/umap/{f}" for f in umap_files if f]
    s = collect_section(tn, "HISILICON /proc/umap (MEDIA PIPELINE)", [
        "ls -la /proc/umap/",
    ] + umap_cmds, timeout=5)
    all_output.append(s)
    with open(os.path.join(args.output_dir, "proc_umap_full.txt"), "w") as f:
        f.write(s)
    print(f"  [6/14] HiSilicon /proc/umap ({len(umap_files)} entries)")

    # --- 7. Network ---
    s = collect_section(tn, "NETWORK", [
        "ifconfig -a",
        "cat /proc/net/dev",
        "cat /proc/net/tcp",
        "cat /proc/net/udp",
        "cat /proc/net/sockstat",
        "cat /proc/net/arp",
        "route -n",
        "cat /proc/net/netstat",
        "cat /proc/net/snmp",
        "cat /proc/sys/net/core/rmem_max",
        "cat /proc/sys/net/core/wmem_max",
        "cat /proc/sys/net/core/netdev_budget",
        "cat /proc/sys/net/ipv4/tcp_rmem",
        "cat /proc/sys/net/ipv4/tcp_wmem",
    ])
    all_output.append(s)
    print("  [7/14] Network state")

    # --- 8. Kernel Modules ---
    s = collect_section(tn, "KERNEL MODULES", [
        "lsmod",
        "cat /proc/modules",
    ], timeout=5)
    all_output.append(s)
    print("  [8/14] Kernel modules")

    # --- 9. Filesystem & Storage ---
    s = collect_section(tn, "FILESYSTEM & STORAGE", [
        "mount",
        "df -h",
        "cat /proc/mounts",
        "cat /proc/filesystems",
        "cat /proc/partitions",
        "cat /proc/mtd",
    ])
    all_output.append(s)
    print("  [9/14] Filesystem & storage")

    # --- 10. Thermal & Hardware ---
    s = collect_section(tn, "THERMAL & HARDWARE", [
        "cat /proc/umap/pm",
        "ls -la /sys/class/thermal/ 2>/dev/null",
        "cat /sys/class/thermal/thermal_zone0/temp 2>/dev/null",
        "cat /sys/class/thermal/thermal_zone0/type 2>/dev/null",
        "cat /proc/device-tree/model 2>/dev/null",
        "cat /proc/device-tree/compatible 2>/dev/null",
        "cat /sys/devices/system/cpu/cpu0/cpufreq/cpuinfo_cur_freq 2>/dev/null",
        "cat /sys/devices/system/cpu/cpu0/cpufreq/scaling_governor 2>/dev/null",
    ])
    all_output.append(s)
    print("  [10/14] Thermal & hardware")

    # --- 11. /sys exploration for HiSilicon SoC ---
    s = collect_section(tn, "SoC /sys EXPLORATION", [
        "find /sys/class -maxdepth 1 -type l 2>/dev/null",
        "ls -la /sys/class/video4linux/ 2>/dev/null",
        "ls -la /sys/class/gpio/ 2>/dev/null",
        "ls -la /dev/hi* 2>/dev/null",
        "ls -la /dev/mmz* 2>/dev/null",
        "cat /proc/media-mem 2>/dev/null",
        "cat /proc/umap/sys 2>/dev/null",
    ], timeout=8)
    all_output.append(s)
    print("  [11/14] SoC /sys exploration")

    # --- 12. Log files ---
    log_cmds = [
        "ls -la /var/log/ 2>/dev/null",
        "ls -la /tmp/*.log 2>/dev/null",
        "cat /var/log/messages 2>/dev/null",
        "cat /var/log/syslog 2>/dev/null",
    ]
    s = collect_section(tn, "LOG FILES", log_cmds, timeout=10)
    all_output.append(s)
    print("  [12/14] Log files")

    # --- 13. RTSP / streaming internals ---
    s = collect_section(tn, "RTSP / STREAMING STATE", [
        "ps -ef | grep -i rtsp",
        "ps -ef | grep -i stream",
        "ps -ef | grep -i ipc",
        "ps -ef | grep -i encoder",
        "netstat -tlnp 2>/dev/null",
        "netstat -ulnp 2>/dev/null",
        "cat /proc/net/tcp",
        "cat /proc/net/udp",
        "ls -la /dev/video* 2>/dev/null",
    ], timeout=5)
    all_output.append(s)
    print("  [13/14] RTSP/streaming state")

    # --- 14. Open files & FD state ---
    s = collect_section(tn, "OPEN FILES & FD STATE", [
        "cat /proc/sys/fs/file-nr",
        "cat /proc/sys/fs/inode-nr",
        "ls -la /proc/self/fd/ 2>/dev/null",
        "cat /proc/sys/kernel/pid_max",
    ])
    all_output.append(s)
    print("  [14/14] Open files & FD state")

    # --- Write combined report ---
    report_path = os.path.join(args.output_dir, "full_diagnostic_report.txt")
    with open(report_path, "w") as f:
        header = [
            f"Camera SoC Diagnostic Dump",
            f"Host: {args.host}",
            f"Timestamp: {time.strftime('%Y-%m-%d %H:%M:%S %Z')}",
            f"Unix timestamp: {time.time()}",
            "",
        ]
        f.write("\n".join(header) + "\n\n")
        f.write("\n\n".join(all_output))

    # --- Write machine-readable snapshot ---
    try:
        snap_data = {
            "host": args.host,
            "timestamp": time.time(),
            "timestamp_human": time.strftime("%Y-%m-%d %H:%M:%S %Z"),
        }
        snap_path = os.path.join(args.output_dir, "snapshot_metadata.json")
        with open(snap_path, "w") as f:
            json.dump(snap_data, f, indent=2)
    except Exception:
        pass

    tn.close()

    report_size = os.path.getsize(report_path)
    print(f"\nDiagnostic dump complete: {report_path} ({report_size // 1024}KB)")
    print(f"Additional files in: {args.output_dir}/")


if __name__ == "__main__":
    main()
