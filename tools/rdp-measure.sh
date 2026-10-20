#!/usr/bin/env bash
# rdp-measure.sh — CPU and bandwidth sampler for ryll's RDP mode.
#
# Meant for the three workloads of the RDP manual test: typing in a
# terminal, scrolling a browser page, and playing a video.  Start
# ryll with --rdp, connect an RDP client, begin the workload, then run
# this against the ryll process and the RDP listener's port.  The
# numbers it prints are recorded in the phase plan by the management
# session.
#
# Method: once a second, read utime + stime from /proc/PID/stat
# (converted with CLK_TCK, as tools/measure-idle-cpu.sh does) and the
# cumulative bytes_sent of the established TCP connection(s) whose
# local port is PORT, from `ss -tin`.  The accepted connection shares
# the listener's port, so filtering on the local port finds it.  No
# root is needed: ss reads /proc/net via netlink for all sockets.
#
# CPU percentages are over the sampling interval, where 100 percent is
# one core.  If the process exits or the connection disappears mid-run,
# sampling stops and the summary covers what was collected.
#
# Exit codes:
#   0  sampled (possibly cut short; the summary says how many seconds)
#   1  bad usage
#   2  no such process, no connection at the start, or nothing sampled
#   3  not Linux / no /proc
#
# Usage: tools/rdp-measure.sh --pid PID --port PORT [--seconds N]
#        (N defaults to 30)
#
# Output: one line on stdout,
#   seconds=N avg_cpu=X% peak_cpu=Y% bytes_sent=B avg_bps=R

set -euo pipefail

readonly DEFAULT_SECONDS=30

usage() {
    echo "usage: ${0##*/} --pid PID --port PORT [--seconds N]  (default ${DEFAULT_SECONDS}s)" >&2
}

pid=""
port=""
seconds=$DEFAULT_SECONDS
while [[ $# -gt 0 ]]; do
    case $1 in
        --pid) pid=${2:-}; shift 2 || { usage; exit 1; } ;;
        --port) port=${2:-}; shift 2 || { usage; exit 1; } ;;
        --seconds) seconds=${2:-}; shift 2 || { usage; exit 1; } ;;
        *) usage; exit 1 ;;
    esac
done

if [[ ! $pid =~ ^[0-9]+$ || ! $port =~ ^[0-9]+$ || ! $seconds =~ ^[0-9]+$ || $seconds -lt 1 ]]; then
    usage
    exit 1
fi

if [[ ! -d /proc/self/task ]]; then
    echo "${0##*/}: needs Linux /proc" >&2
    exit 3
fi

if [[ ! -r /proc/$pid/stat ]]; then
    echo "${0##*/}: no process $pid" >&2
    exit 2
fi

hz=$(getconf CLK_TCK)

# Total utime + stime jiffies of the process; fails if it has gone.
# The comm field is parenthesised and may contain spaces and ')', so
# anchor on the last ')'.  After that, utime and stime are fields 12
# and 13 of the remainder.
cpu_jiffies() {
    local line rest
    local -a f
    line=$(cat "/proc/$pid/stat" 2>/dev/null) || return 1
    rest=${line##*) }
    read -r -a f <<< "$rest"
    echo $(( f[11] + f[12] ))
}

# Sum of bytes_sent over established connections with local port
# $port; fails if there is no such connection.
bytes_sent() {
    ss -H -tin state established "( sport = :$port )" 2>/dev/null | awk '
        match($0, /bytes_sent:[0-9]+/) {
            sum += substr($0, RSTART + 11, RLENGTH - 11); found = 1
        }
        END { if (!found) exit 1; print sum }'
}

now() { echo "${EPOCHREALTIME/./}"; }   # microseconds

prev_cpu=$(cpu_jiffies)
if ! base_bytes=$(bytes_sent); then
    echo "${0##*/}: no established connection on local port $port" >&2
    exit 2
fi
prev_bytes=$base_bytes
prev_t=$(now)
start_t=$prev_t

echo "sampling pid $pid, port $port for ${seconds}s..." >&2

samples=0
cpu_total=0   # jiffies
cpu_peak=0    # hundredths of a percent
for (( i = 0; i < seconds; i++ )); do
    sleep 1
    cur_cpu=$(cpu_jiffies) || { echo "${0##*/}: process $pid exited" >&2; break; }
    cur_bytes=$(bytes_sent) || { echo "${0##*/}: connection on port $port closed" >&2; break; }
    t=$(now)
    dt=$(( t - prev_t ))
    djif=$(( cur_cpu - prev_cpu ))
    # percent * 100, from jiffies over microseconds
    centi=$(( djif * 100 * 100 * 1000000 / (hz * dt) ))
    (( centi > cpu_peak )) && cpu_peak=$centi
    cpu_total=$(( cpu_total + djif ))
    prev_cpu=$cur_cpu
    prev_bytes=$cur_bytes
    prev_t=$t
    samples=$(( samples + 1 ))
done

if [[ $samples -eq 0 ]]; then
    echo "${0##*/}: no samples collected" >&2
    exit 2
fi

elapsed_us=$(( prev_t - start_t ))
total_bytes=$(( prev_bytes - base_bytes ))
(( total_bytes < 0 )) && total_bytes=0

awk -v n="$samples" -v jif="$cpu_total" -v hz="$hz" -v us="$elapsed_us" \
    -v peak="$cpu_peak" -v bytes="$total_bytes" 'BEGIN {
    s = us / 1000000
    printf "seconds=%d avg_cpu=%.1f%% peak_cpu=%.1f%% bytes_sent=%d avg_bps=%.0f\n",
        n, 100 * jif / (hz * s), peak / 100, bytes, bytes * 8 / s
}'
