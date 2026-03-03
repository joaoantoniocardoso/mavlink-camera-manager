#!/usr/bin/env python3
import csv, os, signal, subprocess, sys, time

CLK_TCK = os.sysconf("SC_CLK_TCK")
csv_file = None

def cleanup(*_):
    if csv_file: csv_file.close()
    sys.exit(0)

signal.signal(signal.SIGTERM, cleanup)
signal.signal(signal.SIGINT, cleanup)

def read_cpu_times():
    with open("/proc/stat") as f:
        vals = list(map(int, f.readline().split()[1:]))
    return sum(vals), vals[3] + vals[4]

def read_mem():
    info = {}
    with open("/proc/meminfo") as f:
        for line in f:
            if line.startswith(("MemTotal:", "MemAvailable:", "MemFree:")):
                k, v, *_ = line.split()
                info[k.rstrip(":")] = int(v)
    total = info["MemTotal"]
    avail = info.get("MemAvailable", info.get("MemFree", 0))
    return (total - avail) / 1024.0, total / 1024.0

def read_cpu_temp():
    try:
        with open("/sys/class/thermal/thermal_zone0/temp") as f:
            return int(f.read().strip()) / 1000.0
    except Exception:
        return None

def find_mcm_root_pids():
    try:
        r = subprocess.run(["pidof", "mavlink-camera-manager"], capture_output=True, text=True)
        if r.returncode == 0 and r.stdout.strip():
            return [int(p) for p in r.stdout.strip().split()]
    except Exception:
        pass
    pids = []
    for e in os.listdir("/proc"):
        if not e.isdigit(): continue
        try:
            with open(f"/proc/{e}/comm") as f:
                if f.read().strip().startswith("mavlink-camera-"): pids.append(int(e))
        except Exception: continue
    return pids

def find_all_descendant_pids(root_pids):
    """Find root_pids + all their descendants by scanning /proc ppid fields."""
    if not root_pids:
        return []
    ppid_map = {}
    for e in os.listdir("/proc"):
        if not e.isdigit(): continue
        try:
            with open(f"/proc/{e}/stat") as f:
                rest = f.read().split(") ", 1)[1].split()
            ppid_map[int(e)] = int(rest[1])
        except Exception: continue

    all_pids = set(root_pids)
    queue = list(root_pids)
    children_of = {}
    for pid, ppid in ppid_map.items():
        children_of.setdefault(ppid, []).append(pid)
    while queue:
        pid = queue.pop(0)
        for child in children_of.get(pid, []):
            if child not in all_pids:
                all_pids.add(child)
                queue.append(child)
    return sorted(all_pids)

def read_pid_ticks(pid):
    try:
        with open(f"/proc/{pid}/stat") as f:
            rest = f.read().split(") ", 1)[1].split()
        return int(rest[11]) + int(rest[12])
    except Exception:
        return 0

def read_pid_rss_kb(pid):
    try:
        with open(f"/proc/{pid}/status") as f:
            for line in f:
                if line.startswith("VmRSS:"):
                    return int(line.split()[1])
    except Exception: pass
    return 0

def main():
    global csv_file
    if len(sys.argv) != 2 or sys.argv[1] in ("-h", "--help"):
        print(f"usage: {os.path.basename(sys.argv[0])} output_csv", file=sys.stderr)
        sys.exit(0 if len(sys.argv) == 2 else 1)

    path = sys.argv[1]
    csv_file = open(path, "w", newline="")
    writer = csv.writer(csv_file)
    writer.writerow(["timestamp", "sys_cpu_pct", "sys_mem_used_mb", "sys_mem_total_mb",
                      "cpu_temp_c", "mcm_cpu_pct", "mcm_rss_mb", "mcm_num_procs"])
    csv_file.flush()
    print(f"Stats collector started, writing to {path}", file=sys.stderr)

    prev_total, prev_idle = read_cpu_times()
    prev_mcm_ticks = None
    prev_mcm_pids = []
    time.sleep(1)

    while True:
        now = time.time()

        total, idle = read_cpu_times()
        dt, di = total - prev_total, idle - prev_idle
        sys_cpu = 100.0 * (dt - di) / dt if dt else 0.0
        prev_total, prev_idle = total, idle

        mem_used, mem_total = read_mem()
        temp = read_cpu_temp()
        temp_str = f"{temp:.1f}" if temp is not None else ""

        root_pids = find_mcm_root_pids()
        mcm_pids = find_all_descendant_pids(root_pids)

        mcm_cpu, mcm_rss, mcm_nprocs = "", "", ""
        if mcm_pids:
            if set(mcm_pids) != set(prev_mcm_pids):
                print(f"MCM PIDs: {mcm_pids} ({len(mcm_pids)} processes)", file=sys.stderr)
                prev_mcm_ticks = None

            cur_ticks = sum(read_pid_ticks(p) for p in mcm_pids)
            if prev_mcm_ticks is not None:
                mcm_cpu = f"{100.0 * (cur_ticks - prev_mcm_ticks) / CLK_TCK:.1f}"
            prev_mcm_ticks = cur_ticks

            total_rss_kb = sum(read_pid_rss_kb(p) for p in mcm_pids)
            mcm_rss = f"{total_rss_kb / 1024.0:.1f}"
            mcm_nprocs = str(len(mcm_pids))
        else:
            if prev_mcm_pids:
                print("MCM processes lost, will re-discover", file=sys.stderr)
            prev_mcm_ticks = None

        prev_mcm_pids = mcm_pids

        writer.writerow([f"{now:.3f}", f"{sys_cpu:.1f}", f"{mem_used:.1f}",
                          f"{mem_total:.1f}", temp_str, mcm_cpu, mcm_rss, mcm_nprocs])
        csv_file.flush()
        time.sleep(1)

if __name__ == "__main__":
    main()
