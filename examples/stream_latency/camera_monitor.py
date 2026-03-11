#!/usr/bin/env python3
"""
Camera SoC Monitor - Telnet-based monitoring of HiSilicon camera internals.

Connects to the camera via telnet and periodically samples:
  - SoC temperature and voltage (from /proc/umap/pm)
  - CPU usage (from /proc/stat)
  - Memory usage (from /proc/meminfo)
  - Network counters (from /proc/net/dev)

Outputs NDJSON snapshots to a file for later analysis.
"""

import json
import os
import re
import socket
import sys
import time


class CameraTelnet:
    """Minimal telnet client for HiSilicon cameras (no telnetlib needed)."""

    def __init__(self, host, port=23, timeout=5):
        self.sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        self.sock.settimeout(timeout)
        self.sock.connect((host, port))

    def _drain(self, timeout=1.0):
        """Read and discard any pending data."""
        end = time.time() + timeout
        data = b""
        self.sock.setblocking(False)
        while time.time() < end:
            try:
                chunk = self.sock.recv(4096)
                if not chunk:
                    break
                data += chunk
            except (BlockingIOError, socket.timeout):
                time.sleep(0.05)
        self.sock.setblocking(True)
        return data.decode(errors="replace")

    def read_until(self, marker, timeout=5):
        data = b""
        end = time.time() + timeout
        while time.time() < end:
            try:
                chunk = self.sock.recv(4096)
                if not chunk:
                    break
                data += chunk
                if marker.encode() in data:
                    break
            except socket.timeout:
                break
        return data.decode(errors="replace")

    def cmd(self, command, marker="# ", timeout=3):
        """Send a command and wait for the shell prompt."""
        self.sock.sendall((command + "\n").encode())
        data = b""
        end = time.time() + timeout
        while time.time() < end:
            try:
                chunk = self.sock.recv(4096)
                if not chunk:
                    break
                data += chunk
                if marker.encode() in data:
                    break
            except socket.timeout:
                break
        return data.decode(errors="replace")

    def login(self, user, password):
        self.read_until("login:", timeout=5)
        self.sock.sendall((user + "\n").encode())
        self.read_until("assword:", timeout=3)
        self.sock.sendall((password + "\n").encode())
        resp = self.read_until("# ", timeout=5)
        return "#" in resp or "Welcome" in resp

    def close(self):
        try:
            self.sock.close()
        except Exception:
            pass


def parse_pm(text):
    """Parse /proc/umap/pm output for temperature and voltages."""
    result = {}
    # cur_temp:        58     core_cur_volt:       888 ...
    m = re.search(r"cur_temp:\s+(-?\d+)", text)
    if m:
        result["temp_c"] = int(m.group(1))
    for name in ("core_cur_volt", "cpu_cur_volt", "npu_cur_volt"):
        m = re.search(rf"{name}:\s+(\d+)", text)
        if m:
            result[name.replace("_cur_", "_")] = int(m.group(1))
    # Compensation values
    for name in ("core_temp_comp", "cpu_temp_comp", "npu_temp_comp"):
        m = re.search(rf"{name}:\s+(-?\d+)", text)
        if m:
            result[name] = int(m.group(1))
    return result


def parse_stat(text):
    """Parse /proc/stat cpu line."""
    for line in text.splitlines():
        if line.startswith("cpu "):
            parts = line.split()
            vals = [int(x) for x in parts[1:]]
            total = sum(vals[:8]) if len(vals) >= 8 else sum(vals)
            idle = vals[3] + (vals[4] if len(vals) > 4 else 0)  # idle + iowait
            return {"cpu_total": total, "cpu_busy": total - idle}
    return {}


def parse_meminfo(text):
    """Parse /proc/meminfo for key fields."""
    result = {}
    for line in text.splitlines():
        for key in ("MemTotal", "MemFree", "MemAvailable", "Buffers", "Cached"):
            if line.startswith(key + ":"):
                parts = line.split()
                if len(parts) >= 2:
                    try:
                        result[f"mem_{key.lower()}_kb"] = int(parts[1])
                    except ValueError:
                        pass
    return result


def parse_netdev(text, iface="eth0"):
    """Parse /proc/net/dev for a specific interface's cumulative counters."""
    for line in text.splitlines():
        parts = line.strip().split()
        if not parts or not parts[0].startswith(iface):
            continue
        # "eth0: rx_bytes rx_packets rx_errs rx_drop ... tx_bytes tx_packets tx_errs tx_drop ..."
        label = parts[0].rstrip(":")
        vals = [int(x) for x in parts[1:]]
        if len(vals) < 16:
            continue
        return {
            f"{label}_rx_bytes": vals[0],
            f"{label}_rx_packets": vals[1],
            f"{label}_rx_errs": vals[2],
            f"{label}_rx_drop": vals[3],
            f"{label}_tx_bytes": vals[8],
            f"{label}_tx_packets": vals[9],
            f"{label}_tx_errs": vals[10],
            f"{label}_tx_drop": vals[11],
        }
    return {}


def parse_top_header(text):
    """Parse the top header lines for CPU% and Mem summary."""
    result = {}
    for line in text.splitlines():
        # CPU:  4.5% usr  0.0% sys  0.0% nic 95.4% idle ...
        m = re.search(
            r"CPU:\s+([\d.]+)%\s+usr\s+([\d.]+)%\s+sys\s+"
            r"([\d.]+)%\s+nic\s+([\d.]+)%\s+idle",
            line,
        )
        if m:
            result["top_cpu_usr"] = float(m.group(1))
            result["top_cpu_sys"] = float(m.group(2))
            result["top_cpu_nic"] = float(m.group(3))
            result["top_cpu_idle"] = float(m.group(4))
        # Mem: 106816K used, 256188K free, ...
        m = re.search(r"Mem:\s+(\d+)K\s+used,\s+(\d+)K\s+free", line)
        if m:
            result["top_mem_used_kb"] = int(m.group(1))
            result["top_mem_free_kb"] = int(m.group(2))
    return result


def query_system_info(tn):
    """One-shot query for static kernel/system info."""
    info = {}
    uname = tn.cmd("uname -a", timeout=2)
    for line in uname.splitlines():
        if line.startswith("Linux"):
            info["uname"] = line.strip()
            break
    uptime = tn.cmd("cat /proc/uptime", timeout=2)
    for line in uptime.splitlines():
        parts = line.strip().split()
        if len(parts) >= 1:
            try:
                info["uptime_s"] = float(parts[0])
                break
            except ValueError:
                continue
    return info


def snapshot(tn):
    """Take a single snapshot of the camera's SoC state."""
    snap = {"ts": time.time()}

    # Temperature + voltages from /proc/umap/pm
    pm_text = tn.cmd("cat /proc/umap/pm", timeout=2)
    snap.update(parse_pm(pm_text))

    # CPU counters from /proc/stat
    stat_text = tn.cmd("head -1 /proc/stat", timeout=2)
    snap.update(parse_stat(stat_text))

    # Memory from /proc/meminfo
    mem_text = tn.cmd(
        "grep -E '^(MemTotal|MemFree|MemAvailable|Buffers|Cached):' /proc/meminfo",
        timeout=2,
    )
    snap.update(parse_meminfo(mem_text))

    # Network counters from /proc/net/dev
    net_text = tn.cmd("cat /proc/net/dev", timeout=2)
    snap.update(parse_netdev(net_text))

    return snap


def main():
    import argparse

    parser = argparse.ArgumentParser(description="Monitor camera SoC via telnet")
    parser.add_argument("host", help="Camera IP address")
    parser.add_argument("--user", default="root", help="Telnet username")
    parser.add_argument("--password", required=True, help="Telnet password")
    parser.add_argument("--output", help="Output NDJSON file path (for monitoring mode)")
    parser.add_argument(
        "--interval",
        type=float,
        default=2.0,
        help="Sampling interval in seconds",
    )
    parser.add_argument(
        "--duration",
        type=float,
        default=0,
        help="Total duration in seconds (0 = run until killed)",
    )
    parser.add_argument(
        "--dump-dmesg",
        metavar="PATH",
        help="Dump kernel ring buffer to PATH and exit (one-shot mode)",
    )
    args = parser.parse_args()

    if not args.dump_dmesg and not args.output:
        parser.error("--output is required for monitoring mode (or use --dump-dmesg)")

    print(f"       Connecting to camera at {args.host}...")
    try:
        tn = CameraTelnet(args.host)
    except (socket.timeout, ConnectionRefusedError, OSError) as e:
        print(f"       WARNING: Cannot connect to camera telnet: {e}")
        sys.exit(1)

    if not tn.login(args.user, args.password):
        print("       WARNING: Camera telnet login failed.")
        tn.close()
        sys.exit(1)

    if args.dump_dmesg:
        dmesg_text = tn.cmd("dmesg", timeout=10)
        lines = dmesg_text.splitlines()
        # Strip the command echo and trailing prompt
        if lines and lines[0].strip().startswith("dmesg"):
            lines = lines[1:]
        if lines and "# " in lines[-1]:
            lines = lines[:-1]
        os.makedirs(os.path.dirname(args.dump_dmesg) or ".", exist_ok=True)
        with open(args.dump_dmesg, "w") as f:
            f.write("\n".join(lines) + "\n")
        tn.close()
        print(f"       dmesg: {len(lines)} lines written to {args.dump_dmesg}")
        return

    print("       Camera telnet connected. Monitoring SoC...")

    sys_info = query_system_info(tn)
    if sys_info:
        print(f"       Kernel: {sys_info.get('uname', '?')}, uptime: {sys_info.get('uptime_s', '?')}s")

    def reconnect():
        """Reopen telnet + login.  Returns a new CameraTelnet or None on failure."""
        try:
            new_tn = CameraTelnet(args.host)
        except (socket.timeout, ConnectionRefusedError, OSError) as exc:
            print(f"       Camera monitor: reconnect connect failed: {exc}")
            return None
        if not new_tn.login(args.user, args.password):
            print("       Camera monitor: reconnect login failed.")
            new_tn.close()
            return None
        return new_tn

    start_time = time.time()
    samples = 0
    reconnects = 0
    consecutive_failures = 0

    try:
        with open(args.output, "w") as f:
            while True:
                loop_start = time.time()
                try:
                    snap = snapshot(tn)
                    consecutive_failures = 0
                except (BrokenPipeError, ConnectionResetError, socket.timeout, OSError) as exc:
                    consecutive_failures += 1
                    print(f"       Camera monitor: snapshot failed ({type(exc).__name__}: {exc}); attempting reconnect (#{reconnects + 1})...")
                    try:
                        tn.close()
                    except Exception:
                        pass
                    backoff = min(2.0 * consecutive_failures, 30.0)
                    time.sleep(backoff)
                    new_tn = reconnect()
                    if new_tn is not None:
                        tn = new_tn
                        reconnects += 1
                        print(f"       Camera monitor: reconnected (total reconnects: {reconnects}).")
                    if args.duration > 0 and (time.time() - start_time) >= args.duration:
                        break
                    continue

                if samples == 0:
                    snap["system_info"] = sys_info
                if reconnects > 0 and "reconnects" not in snap:
                    snap["reconnects"] = reconnects
                f.write(json.dumps(snap) + "\n")
                f.flush()
                samples += 1

                if args.duration > 0 and (time.time() - start_time) >= args.duration:
                    break

                elapsed = time.time() - loop_start
                sleep_time = max(0, args.interval - elapsed)
                if sleep_time > 0:
                    time.sleep(sleep_time)
    except KeyboardInterrupt:
        pass
    finally:
        try:
            tn.close()
        except Exception:
            pass

    print(f"       Camera monitor: {samples} samples written to {args.output} ({reconnects} reconnect(s))")


if __name__ == "__main__":
    main()
