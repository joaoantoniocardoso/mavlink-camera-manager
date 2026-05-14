#!/usr/bin/env bash
# Sample MCM CPU usage and context-switch counters at 1 Hz, emitting a
# CSV with the following columns:
#
#   ts_iso,uptime_ns,
#   mcm_total_pct,mcm_user_pct,mcm_system_pct,
#   mcm_rss_kb,mcm_threads,mcm_vol_ctx,mcm_invol_ctx,
#   sys_user_pct,sys_system_pct,sys_iowait_pct,sys_idle_pct,
#   cpu0_pct,cpu1_pct,cpu2_pct,cpu3_pct
#
# Runs entirely inside the container so /proc reflects the same view MCM
# sees. Targets the first matching `mavlink-camera-manager` PID. Exits
# when MCM dies or when sent SIGINT/SIGTERM.
#
# Usage (on the Pi, inside the blueos-core container):
#   tools/onvehicle/sample_cpu.sh [duration_seconds] [out.csv]
#
# Usage (from lab PC, executed inside the container via repro_lab.sh):
#   ssh pi 'docker exec blueos-core /tools/onvehicle/sample_cpu.sh 120 /tmp/cpu.csv'
#   scp pi:/var/lib/docker/.../tmp/cpu.csv ./out/

set -euo pipefail

DURATION=${1:-0}
OUT=${2:-/dev/stdout}

PID=$(pgrep -f mavlink-camera-manager | head -1 || true)
if [[ -z "$PID" ]]; then
    echo "sample_cpu.sh: no mavlink-camera-manager process found" >&2
    exit 1
fi

HZ=$(getconf CLK_TCK)

ncpus=$(nproc)
declare -a cpu_columns
cpu_columns=()
for ((i = 0; i < ncpus; i++)); do
    cpu_columns+=("cpu${i}_pct")
done
cpu_header=$(IFS=,; echo "${cpu_columns[*]}")

{
    echo "ts_iso,uptime_ns,mcm_total_pct,mcm_user_pct,mcm_system_pct,mcm_rss_kb,mcm_threads,mcm_vol_ctx,mcm_invol_ctx,sys_user_pct,sys_system_pct,sys_iowait_pct,sys_idle_pct,$cpu_header"
} > "$OUT"

read_stat_field() {
    # /proc/<pid>/stat field N (1-indexed). Field 2 (comm) may contain
    # parentheses and spaces; strip it before splitting.
    local pid=$1 n=$2
    local raw stripped
    raw=$(cat /proc/$pid/stat 2>/dev/null) || return 1
    stripped=$(echo "$raw" | sed -E 's/^[0-9]+ \([^)]*\) //')
    # field n in the original stat is field (n - 2) in `stripped`.
    awk -v idx=$((n - 2)) '{print $idx}' <<<"$stripped"
}

read_proc_stat() {
    # Return: total_jiffies, user, sys, iowait, idle for the aggregate "cpu" line.
    # NB: `system` and `printf` are awk builtins, hence the short field names.
    awk 'NR==1 {
        u=$2; ni=$3; sy=$4; id=$5; io=$6; ir=$7; so=$8; st=$9;
        tot=u+ni+sy+id+io+ir+so+st;
        print tot, u+ni, sy+ir+so+st, io, id;
    }' /proc/stat
}

read_proc_stat_per_cpu() {
    awk -v n=$ncpus '
        /^cpu[0-9]/ {
            cidx=substr($1,4)+0;
            if (cidx < n) {
                u=$2; ni=$3; sy=$4; id=$5; io=$6; ir=$7; so=$8; st=$9;
                tot=u+ni+sy+id+io+ir+so+st;
                busy=u+ni+sy+ir+so+st;
                print cidx, tot, busy;
            }
        }
    ' /proc/stat
}

# Prime the previous-sample state.
prev_pid_total=$(read_stat_field $PID 14 || echo 0)
prev_pid_total=${prev_pid_total:-0}
prev_pid_utime=$(read_stat_field $PID 14 || echo 0)
prev_pid_stime=$(read_stat_field $PID 15 || echo 0)

read prev_total prev_user prev_system prev_iowait prev_idle <<<"$(read_proc_stat)"

declare -a prev_cpu_total prev_cpu_busy
mapfile -t per_cpu_lines < <(read_proc_stat_per_cpu)
for line in "${per_cpu_lines[@]}"; do
    read idx t b <<<"$line"
    prev_cpu_total[$idx]=$t
    prev_cpu_busy[$idx]=$b
done

start_ns=$(date +%s%N)
end_ns=0
if (( DURATION > 0 )); then
    end_ns=$(( start_ns + DURATION * 1000000000 ))
fi

while true; do
    sleep 1
    now_ns=$(date +%s%N)
    if (( end_ns > 0 && now_ns >= end_ns )); then break; fi

    if ! kill -0 $PID 2>/dev/null; then
        echo "sample_cpu.sh: PID $PID gone, exiting" >&2
        break
    fi

    pid_utime=$(read_stat_field $PID 14 || echo 0); pid_utime=${pid_utime:-0}
    pid_stime=$(read_stat_field $PID 15 || echo 0); pid_stime=${pid_stime:-0}
    pid_threads=$(read_stat_field $PID 20 || echo 0); pid_threads=${pid_threads:-0}
    pid_rss_pages=$(read_stat_field $PID 24 || echo 0); pid_rss_pages=${pid_rss_pages:-0}
    pid_rss_kb=$(( pid_rss_pages * 4 ))
    vol_ctx=$(awk '/voluntary_ctxt_switches/ {print $2}' /proc/$PID/status 2>/dev/null | head -1)
    invol_ctx=$(awk '/nonvoluntary_ctxt_switches/ {print $2}' /proc/$PID/status 2>/dev/null | head -1)
    vol_ctx=${vol_ctx:-0}; invol_ctx=${invol_ctx:-0}

    read cur_total cur_user cur_system cur_iowait cur_idle <<<"$(read_proc_stat)"
    dtot=$(( cur_total - prev_total ))
    duser=$(( cur_user - prev_user ))
    dsys=$(( cur_system - prev_system ))
    diowait=$(( cur_iowait - prev_iowait ))
    didle=$(( cur_idle - prev_idle ))
    if (( dtot <= 0 )); then dtot=1; fi

    # Per-process percent of one core's worth of CPU time.
    dpid_total=$(( (pid_utime - prev_pid_utime) + (pid_stime - prev_pid_stime) ))
    dpid_user=$(( pid_utime - prev_pid_utime ))
    dpid_sys=$(( pid_stime - prev_pid_stime ))
    # CPU% as fraction of all cores: jiffies / dtot * ncpus * 100.
    mcm_total_pct=$(awk -v j=$dpid_total -v t=$dtot -v n=$ncpus 'BEGIN {printf "%.2f", (j/t)*n*100}')
    mcm_user_pct=$(awk -v j=$dpid_user -v t=$dtot -v n=$ncpus 'BEGIN {printf "%.2f", (j/t)*n*100}')
    mcm_system_pct=$(awk -v j=$dpid_sys -v t=$dtot -v n=$ncpus 'BEGIN {printf "%.2f", (j/t)*n*100}')

    sys_user_pct=$(awk -v j=$duser -v t=$dtot 'BEGIN {printf "%.2f", (j/t)*100}')
    sys_system_pct=$(awk -v j=$dsys -v t=$dtot 'BEGIN {printf "%.2f", (j/t)*100}')
    sys_iowait_pct=$(awk -v j=$diowait -v t=$dtot 'BEGIN {printf "%.2f", (j/t)*100}')
    sys_idle_pct=$(awk -v j=$didle -v t=$dtot 'BEGIN {printf "%.2f", (j/t)*100}')

    declare -a cpu_vals
    cpu_vals=()
    mapfile -t per_cpu_lines < <(read_proc_stat_per_cpu)
    for line in "${per_cpu_lines[@]}"; do
        read idx t b <<<"$line"
        prev_t=${prev_cpu_total[$idx]}
        prev_b=${prev_cpu_busy[$idx]}
        dt=$(( t - prev_t )); db=$(( b - prev_b ))
        if (( dt <= 0 )); then dt=1; fi
        cpu_vals[$idx]=$(awk -v b=$db -v t=$dt 'BEGIN {printf "%.2f", (b/t)*100}')
        prev_cpu_total[$idx]=$t
        prev_cpu_busy[$idx]=$b
    done
    cpu_csv=$(IFS=,; echo "${cpu_vals[*]}")

    ts_iso=$(date -Iseconds)
    echo "$ts_iso,$now_ns,$mcm_total_pct,$mcm_user_pct,$mcm_system_pct,$pid_rss_kb,$pid_threads,$vol_ctx,$invol_ctx,$sys_user_pct,$sys_system_pct,$sys_iowait_pct,$sys_idle_pct,$cpu_csv" >> "$OUT"

    prev_total=$cur_total; prev_user=$cur_user; prev_system=$cur_system
    prev_iowait=$cur_iowait; prev_idle=$cur_idle
    prev_pid_utime=$pid_utime; prev_pid_stime=$pid_stime
done
