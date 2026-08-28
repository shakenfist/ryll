#!/usr/bin/env bash
# measure-idle-cpu.sh — per-thread CPU sampler for a running ryll.
#
# Answers "how much CPU is ryll actually burning, and which threads
# are burning it".  Written for PLAN-idle-cpu-and-latency, whose
# phase 1 profiled a 6.24-core idle client and whose phase 2 had to
# show the fix worked; the numbers in both plan files came from this
# method.  Keeping it as a script rather than a recipe means the next
# person to make a CPU claim about ryll can reproduce it rather than
# quote it.
#
# Method: read utime + stime from /proc/<pid>/task/<tid>/stat for
# every thread, sleep, read them again, and convert the deltas to
# percent of one core using CLK_TCK.  This measures the whole
# process, including the Mesa llvmpipe rasteriser threads that
# dominate on a machine with no GPU -- which is the entire reason the
# plan exists, and the reason `top` on the main thread alone is
# misleading here.
#
# The interesting reading is idle: connect, then leave the client
# alone with no input and no display activity.  Compare against the
# same measurement while moving the mouse over the surface -- a low
# idle number means nothing if the client has stopped waking up.
#
# Linux only: it reads /proc.  Exits 3 elsewhere.
#
# Exit codes:
#   0  sampled successfully
#   1  bad usage
#   2  no such process, or it exited mid-sample
#   3  not Linux / no /proc
#
# Usage: tools/measure-idle-cpu.sh <pid> [seconds]
#        tools/measure-idle-cpu.sh "$(pgrep -f 'ryll --direct')" 60

set -u

readonly DEFAULT_WINDOW=30

usage() {
    echo "usage: ${0##*/} <pid> [seconds]  (default ${DEFAULT_WINDOW}s)" >&2
}

if [[ $# -lt 1 || $# -gt 2 ]]; then
    usage
    exit 1
fi

pid=$1
window=${2:-$DEFAULT_WINDOW}

if [[ ! $pid =~ ^[0-9]+$ ]] || [[ ! $window =~ ^[0-9]+$ ]] || [[ $window -lt 1 ]]; then
    usage
    exit 1
fi

if [[ ! -d /proc/self/task ]]; then
    echo "${0##*/}: needs Linux /proc" >&2
    exit 3
fi

if [[ ! -d /proc/$pid ]]; then
    echo "${0##*/}: no process $pid" >&2
    exit 2
fi

hz=$(getconf CLK_TCK)

# Thread name and jiffies out of one /proc stat line.  The comm field
# is parenthesised and may itself contain spaces and ')', so anchor on
# the last ')' rather than splitting on whitespace -- the classic
# /proc/pid/stat parsing trap.  After that, utime and stime are fields
# 12 and 13 of the remainder.
read_stat() {
    local line=$1 comm rest
    comm=${line#*(}
    comm=${comm%)*}
    rest=${line##*) }
    local -a f
    read -r -a f <<< "$rest"
    printf '%s\t%s\n' "$comm" "$(( f[11] + f[12] ))"
}

declare -A before
for task in /proc/"$pid"/task/*; do
    tid=${task##*/}
    line=$(cat "$task/stat" 2>/dev/null) || continue
    IFS=$'\t' read -r _comm jiffies < <(read_stat "$line")
    before[$tid]=$jiffies
done

if [[ ${#before[@]} -eq 0 ]]; then
    echo "${0##*/}: process $pid has no readable threads" >&2
    exit 2
fi

echo "sampling pid $pid for ${window}s (${#before[@]} threads)..." >&2
sleep "$window"

if [[ ! -d /proc/$pid ]]; then
    echo "${0##*/}: process $pid exited during the sample" >&2
    exit 2
fi

total=0
declare -A group
rows=""
for task in /proc/"$pid"/task/*; do
    tid=${task##*/}
    line=$(cat "$task/stat" 2>/dev/null) || continue
    IFS=$'\t' read -r comm jiffies < <(read_stat "$line")
    # A thread that appeared mid-sample has no baseline; count it from
    # zero rather than dropping it, so the total stays honest.
    delta=$(( jiffies - ${before[$tid]:-0} ))
    (( delta < 0 )) && delta=0
    total=$(( total + delta ))
    # Collapse the numeric suffix so llvmpipe-0..15 read as one group.
    key=${comm%%-[0-9]*}
    group[$key]=$(( ${group[$key]:-0} + delta ))
    rows+=$(printf '%s\t%s\t%s\n' "$delta" "$comm" "$tid")$'\n'
done

pct() { awk -v d="$1" -v w="$window" -v hz="$hz" 'BEGIN{printf "%.2f", 100*d/(w*hz)}'; }

echo
echo "By thread group:"
for key in "${!group[@]}"; do
    printf '%s\t%s\n' "${group[$key]}" "$key"
done | sort -rn | while IFS=$'\t' read -r jiffies key; do
    printf '  %-24s %8s jiffies  %7s%% of one core\n' "$key" "$jiffies" "$(pct "$jiffies")"
done

echo
echo "Busiest threads:"
printf '%s' "$rows" | sort -rn | head -10 | while IFS=$'\t' read -r delta comm tid; do
    [[ $delta -eq 0 ]] && continue
    printf '  %-20s %-8s %6s jiffies  %7s%% of one core\n' "$comm" "$tid" "$delta" "$(pct "$delta")"
done

rss=$(awk '/^VmRSS/{print $2" "$3}' /proc/"$pid"/status 2>/dev/null)
echo
echo "TOTAL $total jiffies over ${window}s = $(pct "$total")% of one core"
echo "RSS ${rss:-unknown}, threads ${#before[@]}, CLK_TCK $hz"
