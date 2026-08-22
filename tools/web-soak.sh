#!/bin/bash
#
# Sample a running `ryll --web` process while driving the guest, and
# print numbers comparable with the webrtc-rs 0.20 upgrade's 0.17
# baseline.
#
# The baseline lives in the "Baseline" section of
# docs/plans/PLAN-webrtc-0.20-upgrade-phase-01-prework.md. It was
# captured with an ad-hoc sampler that was never committed, which is
# why this script exists: the conditions it describes are only
# reproducible if the sampling is written down rather than
# reconstructed from prose. Change the method here and the numbers
# stop being comparable with the ones already recorded.
#
# What it samples, per interval:
#
#   RSS         VmRSS from /proc/<pid>/status
#   CPU         utime+stime summed across every thread, from
#               /proc/<pid>/task/*/stat fields 14 and 15
#   host busy%  derived from two reads of /proc/stat
#   load        1-minute figure from /proc/loadavg
#
# The host figures are per-sample and deliberate: these soaks run on a
# shared machine, and phase 01's second run caught an external spike to
# 29% busy. Recording it inline means contamination is visible in the
# data rather than folded into the result.
#
# Usage:
#   tools/web-soak.sh --pid <ryll-pid> [options]
#   tools/web-soak.sh --pidfile /tmp/ryll.pid [options]
#
# Options:
#   --duration SECONDS   total run length (default 1200, i.e. 20 min)
#   --interval SECONDS   sample and keypress cadence (default 30)
#   --qmp PATH           QEMU QMP socket; drives the guest with
#                        `sendkey` once per interval. `make test-qemu`
#                        creates one at /tmp/ryll-test-qemu-qmp.sock.
#                        Omit to sample without driving the guest.
#   --key KEY            QMP key to send (default `spc`)
#   --csv PATH           where to write the per-sample CSV
#                        (default ./web-soak-<pid>.csv)
#
set -euo pipefail

PID=""
PIDFILE=""
DURATION=1200
INTERVAL=30
QMP=""
KEY="spc"
CSV=""

usage() {
    sed -n '3,45p' "$0" | sed 's/^# \{0,1\}//'
    exit "${1:-0}"
}

while [ $# -gt 0 ]; do
    case "$1" in
        --pid)      PID="$2";      shift 2 ;;
        --pidfile)  PIDFILE="$2";  shift 2 ;;
        --duration) DURATION="$2"; shift 2 ;;
        --interval) INTERVAL="$2"; shift 2 ;;
        --qmp)      QMP="$2";      shift 2 ;;
        --key)      KEY="$2";      shift 2 ;;
        --csv)      CSV="$2";      shift 2 ;;
        -h|--help)  usage 0 ;;
        *) echo "unknown argument: $1" >&2; usage 1 ;;
    esac
done

if [ -n "$PIDFILE" ]; then
    [ -r "$PIDFILE" ] || { echo "cannot read pidfile $PIDFILE" >&2; exit 1; }
    PID="$(cat "$PIDFILE")"
fi
if [ -z "$PID" ]; then
    echo "one of --pid or --pidfile is required" >&2
    usage 1
fi
if [ ! -d "/proc/$PID" ]; then
    echo "no process $PID" >&2
    exit 1
fi
if [ "$INTERVAL" -lt 1 ] || [ "$DURATION" -lt "$INTERVAL" ]; then
    echo "--duration must be at least one --interval, and --interval at least 1s" >&2
    exit 1
fi
CSV="${CSV:-./web-soak-$PID.csv}"

# Kernel clock ticks per second: /proc/*/stat reports CPU time in
# these, not in seconds.
CLK_TCK="$(getconf CLK_TCK)"

if [ -n "$QMP" ]; then
    if ! command -v socat >/dev/null 2>&1; then
        echo "socat is required to drive the guest over QMP; install it or omit --qmp" >&2
        exit 1
    fi
    [ -S "$QMP" ] || { echo "no QMP socket at $QMP" >&2; exit 1; }
fi

# Send one QMP command and return the reply. A fresh connection per
# command: QMP needs a capabilities handshake per session, and one
# command every 30 s does not justify holding a session open.
qmp_send() {
    printf '%s\n%s\n' \
        '{"execute":"qmp_capabilities"}' \
        "$1" \
    | socat - "UNIX-CONNECT:$QMP" 2>/dev/null || true
}

# The `sendkey` payload, built once so the pre-flight check below and
# the sampling loop cannot drift apart.
qmp_sendkey_cmd() {
    echo "{\"execute\":\"send-key\",\"arguments\":{\"keys\":[{\"type\":\"qcode\",\"data\":\"$KEY\"}]}}"
}

# Total and idle jiffies from /proc/stat's aggregate line.
host_cpu_totals() {
    awk '/^cpu / {
        total = 0
        for (i = 2; i <= NF; i++) { total += $i }
        # Fields 5 and 6 after "cpu" are idle and iowait.
        print total, $5 + $6
    }' /proc/stat
}

# utime+stime across every thread of the process, in jiffies.
process_cpu_jiffies() {
    local total=0 t
    for t in "/proc/$PID/task"/*/stat; do
        [ -r "$t" ] || continue
        # The comm field can contain spaces and brackets, so count
        # fields back from the closing paren rather than forward from
        # the start: utime and stime are the 12th and 13th fields
        # after it.
        total=$((total + $(awk '{
            s = $0
            sub(/^[^)]*\) /, "", s)
            split(s, f, " ")
            print f[12] + f[13]
        }' "$t")))
    done
    echo "$total"
}

rss_kb() {
    awk '/^VmRSS:/ { print $2 }' "/proc/$PID/status"
}

cat <<BANNER
web-soak: sampling pid $PID every ${INTERVAL}s for ${DURATION}s
          CSV -> $CSV
BANNER
if [ -n "$QMP" ]; then
    cat <<BANNER
          driving the guest with '$KEY' via $QMP

          Note: the uefi-latency-guest advances a fixed eight-colour
          cycle on any keypress, and one step in eight is black. A
          viewer legitimately shows about ${INTERVAL}s of black every
          $((INTERVAL * 8))s. That is the guest, not a fault.
BANNER
fi
echo

# Send one key before committing to the run. A mistyped --key is
# accepted by the socket and rejected by QEMU, which would otherwise
# leave the guest idle for the whole soak and the result worthless --
# and a soak is expensive enough to redo that it is worth one probe.
if [ -n "$QMP" ]; then
    reply="$(qmp_send "$(qmp_sendkey_cmd)")"
    case "$reply" in
        *'"error"'*)
            echo "web-soak: QEMU rejected the '$KEY' keypress -- refusing to run a soak that" >&2
            echo "          would not touch the guest. QMP said: $reply" >&2
            exit 1
            ;;
        '')
            echo "web-soak: no reply from the QMP socket at $QMP" >&2
            exit 1
            ;;
    esac
fi

echo "elapsed_s,rss_kb,proc_cpu_jiffies,host_busy_pct,load1" > "$CSV"

read -r host_total_prev host_idle_prev <<<"$(host_cpu_totals)"
cpu_start="$(process_cpu_jiffies)"
rss_start="$(rss_kb)"
rss_max="$rss_start"
elapsed=0

while [ "$elapsed" -lt "$DURATION" ]; do
    sleep "$INTERVAL"
    elapsed=$((elapsed + INTERVAL))

    if [ ! -d "/proc/$PID" ]; then
        echo "web-soak: process $PID exited after ${elapsed}s -- stopping" >&2
        break
    fi

    rss="$(rss_kb)"
    cpu="$(process_cpu_jiffies)"
    [ "$rss" -gt "$rss_max" ] && rss_max="$rss"

    read -r host_total host_idle <<<"$(host_cpu_totals)"
    busy="$(awk -v t="$host_total" -v tp="$host_total_prev" \
                -v i="$host_idle" -v ip="$host_idle_prev" \
        'BEGIN { d = t - tp; if (d <= 0) { print "0.0" } else { printf "%.1f", 100 * (1 - (i - ip) / d) } }')"
    host_total_prev="$host_total"
    host_idle_prev="$host_idle"

    load1="$(awk '{ print $1 }' /proc/loadavg)"

    echo "$elapsed,$rss,$cpu,$busy,$load1" >> "$CSV"
    printf 'web-soak: t=%-5s rss=%s MB  host busy=%s%%  load=%s\n' \
        "${elapsed}s" "$((rss / 1024))" "$busy" "$load1"

    if [ -n "$QMP" ]; then
        qmp_send "$(qmp_sendkey_cmd)" >/dev/null
    fi
done

if [ -d "/proc/$PID" ]; then
    rss_end="$(rss_kb)"
    cpu_end="$(process_cpu_jiffies)"
else
    rss_end="$rss_max"
    cpu_end="$(awk -F, 'END { print $3 }' "$CSV")"
fi

# CPU as a percentage of one core across the whole run, which is how
# the baseline table states it.
cpu_pct="$(awk -v a="$cpu_start" -v b="$cpu_end" -v secs="$elapsed" -v tck="$CLK_TCK" \
    'BEGIN { if (secs <= 0) { print "n/a" } else { printf "%.2f", 100 * ((b - a) / tck) / secs } }')"
host_mean="$(awk -F, 'NR > 1 { s += $4; n++ } END { if (n) printf "%.1f", s / n; else print "n/a" }' "$CSV")"
host_max="$(awk -F, 'NR > 1 && $4 > m { m = $4 } END { printf "%.1f", m }' "$CSV")"

cat <<SUMMARY

web-soak summary (compare with the Baseline table in
docs/plans/PLAN-webrtc-0.20-upgrade-phase-01-prework.md)

  Duration                     ${elapsed}s
  RSS start -> end             $((rss_start / 1024)) -> $((rss_end / 1024)) MB
  RSS max                      $((rss_max / 1024)) MB
  CPU, all threads, whole run  ${cpu_pct}% of one core
  Host CPU busy%, mean (max)   ${host_mean} (${host_max})

  Per-sample data: $CSV

Not sampled here: video and audio pump drop counts and reaper events,
which ryll logs at debug -- run ryll with --verbose and read them out
of the session log. Do that in a short separate session rather than
in the one being measured: --verbose is debug for the whole
dependency tree, and webrtc-rs at that level is enough log to move
the numbers.

ryll does not read RUST_LOG.
SUMMARY
