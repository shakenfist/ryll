#!/usr/bin/env bash
# Kasm Workspaces startup script for the ryll image (tools/kasm/Dockerfile).
# Kasm's kasm_startup.sh runs this once the desktop is up. It keeps one
# maximised ryll running for the life of the session, restarting it if
# it exits, in the same shape as Kasm's stock remmina script.
#
# The connection comes from, in order:
#   - a URL Kasm hands in (KASM_URL or LAUNCH_URL at startup, or --url
#     on a later exec into a running session), passed to ryll as --url,
#     so it must point at a .vv file;
#   - RYLL_KASM_DIRECT, a HOST:PORT (or HOST:PORT:TLS_PORT) passed as
#     --direct, for a workspace pinned to one SPICE server. If that
#     server wants a password, RYLL_KASM_PASSWORD_FILE names a file in
#     the container holding it (mount it with a Kasm volume mapping),
#     passed as --password-file.
# APP_ARGS, Kasm's usual variable for extra application arguments, is
# appended to either. Set the variables in the workspace's Docker Run
# Config Override; see docs/kasm-workspace.md.

set -euo pipefail

START_COMMAND="ryll"
PGREP="ryll"
# Read by maximize_window.sh, which matches it against `wmctrl -l`.
export MAXIMIZE="true"
export MAXIMIZE_NAME="Ryll - SPICE Client"
MAXIMIZE_SCRIPT="${STARTUPDIR}/maximize_window.sh"

read -r -a EXTRA_ARGS <<< "${APP_ARGS:-}"

# Print the ryll command line for a connection URL ($1, may be empty)
# one argument per line, or nothing when there is no connection to make.
ryll_args() {
    local url=$1
    if [ -n "${url}" ]; then
        printf '%s\n' --url "${url}"
    elif [ -n "${RYLL_KASM_DIRECT:-}" ]; then
        printf '%s\n' --direct "${RYLL_KASM_DIRECT}"
        if [ -n "${RYLL_KASM_PASSWORD_FILE:-}" ]; then
            printf '%s\n' --password-file "${RYLL_KASM_PASSWORD_FILE}"
        fi
    else
        return
    fi
    if [ ${#EXTRA_ARGS[@]} -gt 0 ]; then
        printf '%s\n' "${EXTRA_ARGS[@]}"
    fi
}

# Kasm execs into a running session with --go or --assign when a user
# opens a URL in it. Open that URL in a second ryll alongside the first.
kasm_exec() {
    local url=$1
    if [ -z "${url}" ]; then
        echo "No URL specified for exec command. Doing nothing."
        return
    fi
    local -a args
    mapfile -t args < <(ryll_args "${url}")
    /usr/bin/filter_ready
    /usr/bin/desktop_ready
    "${START_COMMAND}" "${args[@]}"
}

kasm_startup() {
    local url=${KASM_URL:-${LAUNCH_URL:-}}
    local -a args
    mapfile -t args < <(ryll_args "${url}")
    if [ ${#args[@]} -eq 0 ]; then
        echo "ryll: no connection configured. Set RYLL_KASM_DIRECT in the" \
            "workspace's Docker Run Config Override, or launch it with a" \
            "kasm_url pointing at a .vv file." >&2
        exit 1
    fi

    if [ -n "${DISABLE_CUSTOM_STARTUP:-}" ]; then
        return
    fi

    /usr/bin/filter_ready
    /usr/bin/desktop_ready
    # maximize_window.sh loops forever re-maximising the window by
    # title, so one copy covers every ryll the loop below starts.
    bash "${MAXIMIZE_SCRIPT}" &

    echo "Entering process startup loop"
    while true; do
        if ! pgrep -x "${PGREP}" > /dev/null; then
            "${START_COMMAND}" "${args[@]}" || true
        fi
        sleep 1
    done
}

options=$(getopt -o gau: -l go,assign,url: -n "$0" -- "$@")
eval set -- "${options}"

GO=""
OPT_URL=""
while [[ $1 != -- ]]; do
    case $1 in
        -g|--go|-a|--assign) GO="true"; shift 1;;
        -u|--url) OPT_URL=$2; shift 2;;
        *) echo "bad option: $1" >&2; exit 1;;
    esac
done
shift

if [ -n "${GO}" ]; then
    kasm_exec "${OPT_URL:-${1:-}}"
else
    kasm_startup
fi
