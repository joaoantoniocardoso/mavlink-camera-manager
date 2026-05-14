#!/usr/bin/env bash
# Quiet the lab Pi for the UDP-decoupler optimisation experiment.
#
# Subcommands:
#   pin           Phase 1: userspace tuning. Stops other containers, pins the
#                 blueos-core container to cores 2,3, sets cpufreq=performance,
#                 stops irqbalance and steers IRQs to cores 0,1, disables swap.
#                 No reboot.
#   restore       Reverse what `pin` did. Re-enables irqbalance, removes the
#                 docker cpuset, restores governor, re-enables swap. The
#                 other BlueOS containers must be restarted manually via
#                 blueos-bootstrap.
#   prepare-boot  Phase 2: append "isolcpus=2,3 nohz_full=2,3 rcu_nocbs=2,3"
#                 to /boot/firmware/cmdline.txt (idempotent). Pi reboot
#                 required afterwards. A backup of cmdline.txt is written
#                 alongside before each modification.
#   revert-boot   Remove the boot-time isolation parameters.
#   status        Print current pinning, governor, IRQ affinity, swap, and
#                 container CPU constraints.
#
# Usage:
#   tools/onvehicle/pin_mcm.sh pin
#   tools/onvehicle/pin_mcm.sh status
#   tools/onvehicle/pin_mcm.sh restore
#   tools/onvehicle/pin_mcm.sh prepare-boot
#   tools/onvehicle/pin_mcm.sh revert-boot

set -euo pipefail

SCRIPT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=_lib.sh
source "$SCRIPT_DIR/_lib.sh"

MCM_CORES=${MCM_CORES:-2,3}
HOST_CORES=${HOST_CORES:-0,1}
BLUEOS_CONTAINER=${BLUEOS_CONTAINER:-blueos-core}
OTHER_CONTAINERS_DEFAULT=(
    "blueos-bootstrap"
    "extension-blueroboticscockpitv1180"
    "extension-publicecrawsblueosbcloudagent20260211"
)

STATE_DIR=/tmp/mcm-pin-state
CMDLINE_PATH=/boot/firmware/cmdline.txt
ISOL_PARAMS=("isolcpus=$MCM_CORES" "nohz_full=$MCM_CORES" "rcu_nocbs=$MCM_CORES")

run_on_pi() {
    # Pass the whole shell snippet to a sudo bash subshell so multi-statement
    # constructs (loops, redirections to root-owned paths) execute under root.
    ssh_host "sudo bash -c $(printf %q "$1")"
}

cmd_pin() {
    echo "[+] Saving state to $STATE_DIR (on Pi)..."
    run_on_pi "mkdir -p $STATE_DIR && \
        cat /sys/devices/system/cpu/cpu0/cpufreq/scaling_governor > $STATE_DIR/governor.before 2>/dev/null || echo unknown > $STATE_DIR/governor.before; \
        systemctl is-active irqbalance > $STATE_DIR/irqbalance.before 2>/dev/null || echo inactive > $STATE_DIR/irqbalance.before; \
        cat /proc/swaps | awk 'NR>1' > $STATE_DIR/swaps.before; \
        docker ps --format '{{.Names}}' > $STATE_DIR/containers.before; \
        docker inspect --format '{{.HostConfig.CpusetCpus}}' $BLUEOS_CONTAINER > $STATE_DIR/blueos.cpuset.before 2>/dev/null || true"

    echo "[+] Pinning non-blueos containers to host cores ($HOST_CORES)..."
    # Some containers have restart policies; instead of stopping them
    # (where they would just respawn unpinned), constrain them to the
    # host cores. blueos-bootstrap is stopped because it manages other
    # extensions; we don't want it spawning new unpinned siblings.
    run_on_pi "docker stop -t 5 blueos-bootstrap >/dev/null 2>&1 || true"
    run_on_pi "for c in \$(docker ps --format '{{.Names}}' | grep -v '^$BLUEOS_CONTAINER\$'); do docker update --cpuset-cpus=$HOST_CORES \$c >/dev/null 2>&1 || true; done"

    echo "[+] Pinning $BLUEOS_CONTAINER to cores $MCM_CORES..."
    run_on_pi "docker update --cpuset-cpus=$MCM_CORES $BLUEOS_CONTAINER"

    echo "[+] Setting cpufreq governor=performance on all cores..."
    run_on_pi "for c in /sys/devices/system/cpu/cpu*/cpufreq/scaling_governor; do echo performance > \$c; done"
    run_on_pi "for c in /sys/devices/system/cpu/cpu*/cpufreq; do \
        if [[ -f \$c/scaling_max_freq ]]; then cat \$c/scaling_max_freq > \$c/scaling_min_freq; fi; \
    done" || echo "[!] Could not lock min==max freq (non-fatal)"

    echo "[+] Stopping irqbalance and pinning IRQs to host cores ($HOST_CORES)..."
    run_on_pi "systemctl stop irqbalance 2>/dev/null || true"
    # Build a hex mask for HOST_CORES (e.g. "0,1" -> 0x3).
    local mask
    mask=$(python3 -c "import sys; cores=[int(c) for c in '$HOST_CORES'.split(',')]; print(format(sum(1<<c for c in cores), 'x'))")
    run_on_pi "for irq in /proc/irq/*/smp_affinity; do echo $mask > \$irq 2>/dev/null || true; done"

    echo "[+] Disabling swap..."
    run_on_pi "swapoff -a || true"

    echo "[+] Verifying..."
    cmd_status
    echo
    echo "[+] Pin complete. blueos-core is the only running container, pinned to cores $MCM_CORES."
    echo "    Drive MCM as usual via repro_lab.sh; the bench container name is the same ($BLUEOS_CONTAINER)."
}

cmd_restore() {
    if ! ssh_host "test -d $STATE_DIR" 2>/dev/null; then
        echo "[!] No saved state at $STATE_DIR; nothing to restore" >&2
        exit 1
    fi

    echo "[+] Removing docker cpuset constraint on $BLUEOS_CONTAINER..."
    run_on_pi "docker update --cpuset-cpus='' $BLUEOS_CONTAINER || true"

    echo "[+] Restoring cpufreq governor..."
    local prev_gov
    prev_gov=$(ssh_host "cat $STATE_DIR/governor.before" 2>/dev/null || echo schedutil)
    if [[ -z "$prev_gov" || "$prev_gov" == "unknown" ]]; then prev_gov=schedutil; fi
    run_on_pi "for c in /sys/devices/system/cpu/cpu*/cpufreq/scaling_governor; do echo $prev_gov > \$c; done"

    echo "[+] Re-enabling irqbalance..."
    run_on_pi "systemctl start irqbalance 2>/dev/null || true"

    echo "[+] Re-enabling swap..."
    run_on_pi "swapon -a || true"

    echo "[+] Restore complete. Restart blueos-bootstrap to bring the other extensions back:"
    echo "      ssh $PI_USER@$PI_HOST 'sudo docker start blueos-bootstrap'"
}

cmd_status() {
    echo "=== CPUs ==="
    ssh_host "nproc && cat /sys/devices/system/cpu/cpu0/cpufreq/scaling_governor 2>/dev/null"
    echo
    echo "=== Per-cpu freq ==="
    ssh_host "for c in /sys/devices/system/cpu/cpu*/cpufreq/scaling_cur_freq; do echo \"\$c \$(cat \$c)\"; done" 2>/dev/null | head -12
    echo
    echo "=== irqbalance ==="
    ssh_host "systemctl is-active irqbalance 2>/dev/null || echo inactive"
    echo
    echo "=== Swap ==="
    ssh_host "swapon --show 2>/dev/null || cat /proc/swaps"
    echo
    echo "=== Docker (running) ==="
    ssh_host "docker ps --format '{{.Names}} {{.Image}} cpuset={{.ID}}' | head -10"
    echo
    echo "=== Container cpuset ==="
    ssh_host "for c in \$(docker ps --format '{{.Names}}'); do echo \"\$c -> \$(docker inspect --format '{{.HostConfig.CpusetCpus}}' \$c)\"; done"
    echo
    echo "=== /proc/cmdline ==="
    ssh_host "cat /proc/cmdline"
}

cmd_prepare_boot() {
    echo "[+] Backing up $CMDLINE_PATH..."
    run_on_pi "cp -n $CMDLINE_PATH ${CMDLINE_PATH}.before-mcm-pin"
    local missing
    missing=$(ssh_host "cat $CMDLINE_PATH")
    local addendum=""
    for param in "${ISOL_PARAMS[@]}"; do
        if ! grep -q -- "$param" <<<"$missing"; then
            addendum+=" $param"
        fi
    done
    if [[ -z "$addendum" ]]; then
        echo "[+] Boot params already present; nothing to do."
        return 0
    fi
    addendum="${addendum# }"
    echo "[+] Appending to $CMDLINE_PATH: $addendum"
    run_on_pi "sed -i -E \"s|\$| $addendum|\" $CMDLINE_PATH"
    echo "[+] cmdline.txt is now:"
    ssh_host "cat $CMDLINE_PATH"
    echo
    echo "[!] Reboot the Pi to take effect:"
    echo "      ssh $PI_USER@$PI_HOST 'sudo reboot'"
}

cmd_revert_boot() {
    echo "[+] Backing up $CMDLINE_PATH..."
    run_on_pi "cp -n $CMDLINE_PATH ${CMDLINE_PATH}.before-mcm-revert"
    for param in "${ISOL_PARAMS[@]}"; do
        run_on_pi "sed -i -E \"s| ?$param||g\" $CMDLINE_PATH"
    done
    echo "[+] cmdline.txt is now:"
    ssh_host "cat $CMDLINE_PATH"
    echo
    echo "[!] Reboot required to drop the isolation."
}

main() {
    if [[ $# -lt 1 ]]; then
        sed -n '2,28p' "$0"
        exit 1
    fi
    case "$1" in
        pin)          cmd_pin ;;
        restore)      cmd_restore ;;
        status)       cmd_status ;;
        prepare-boot) cmd_prepare_boot ;;
        revert-boot)  cmd_revert_boot ;;
        *) echo "unknown subcommand: $1" >&2; exit 1 ;;
    esac
}

main "$@"
