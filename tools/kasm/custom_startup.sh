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
#   - the workspace's launch form (see tools/kasm/launch_config.json),
#     whose answers Kasm writes to RYLL_KASM_LAUNCH_SELECTIONS: a
#     `connection` that is either an http(s) .vv URL (--url) or
#     HOST:PORT[:TLS_PORT] (--direct), and an optional `password`;
#   - RYLL_KASM_DIRECT, a HOST:PORT (or HOST:PORT:TLS_PORT) passed as
#     --direct, for a workspace pinned to one SPICE server. Set
#     RYLL_KASM_LAUNCH_FORM as well if the workspace also has a launch
#     form, so that the script waits for its answers.
# For --direct, a password typed into the launch form wins; otherwise
# RYLL_KASM_PASSWORD_FILE names a file in the container holding it
# (mount it with a Kasm volume mapping), passed as --password-file.
# APP_ARGS, Kasm's usual variable for extra application arguments, is
# appended to any of them. Set the variables in the workspace's Docker
# Run Config Override; see docs/kasm-workspace.md.

set -euo pipefail

START_COMMAND="ryll"
PGREP="ryll"
# Read by maximize_window.sh, which matches it against `wmctrl -l`.
export MAXIMIZE="true"
export MAXIMIZE_NAME="Ryll - SPICE Client"
MAXIMIZE_SCRIPT="${STARTUPDIR}/maximize_window.sh"

# Kasm's default destination for launch form answers.
LAUNCH_SELECTIONS=${RYLL_KASM_LAUNCH_SELECTIONS:-/tmp/launch_selections.json}
# How long to wait for the launch form answers when they are the only
# possible source of a connection.
LAUNCH_SELECTIONS_WAIT=30

read -r -a EXTRA_ARGS <<< "${APP_ARGS:-}"

# Print one launch form answer ($1 is its key), or nothing if the form
# was not used or left it empty.
launch_selection() {
    if [ -r "${LAUNCH_SELECTIONS}" ]; then
        jq -r --arg key "$1" '.[$key] // empty' "${LAUNCH_SELECTIONS}"
    fi
}

# Print the --direct password arguments: a password typed into the
# launch form, written to a private file because ryll only takes a
# password from a file, or else RYLL_KASM_PASSWORD_FILE. The file is
# kept out of $HOME, which Kasm's persistent profiles save when the
# session ends, and lives as long as the container does, because the
# restart loop hands ryll the same file each time.
password_args() {
    local password
    password=$(launch_selection password)
    if [ -n "${password}" ]; then
        local dir=${XDG_RUNTIME_DIR:-/dev/shm}
        if [ ! -d "${dir}" ] || [ ! -w "${dir}" ]; then
            dir=/tmp
        fi
        local file
        # mktemp creates the file readable by its owner only.
        file=$(mktemp "${dir}/ryll-password.XXXXXX")
        printf '%s' "${password}" > "${file}"
        printf '%s\n' --password-file "${file}"
    elif [ -n "${RYLL_KASM_PASSWORD_FILE:-}" ]; then
        printf '%s\n' --password-file "${RYLL_KASM_PASSWORD_FILE}"
    fi
}

# Print the ryll command line for a connection URL ($1, may be empty)
# one argument per line, or nothing when there is no connection to make.
ryll_args() {
    local url=$1
    local connection
    connection=$(launch_selection connection)
    if [ -n "${url}" ]; then
        printf '%s\n' --url "${url}"
    elif [[ ${connection} =~ ^https?:// ]]; then
        printf '%s\n' --url "${connection}"
    elif [ -n "${connection}" ]; then
        printf '%s\n' --direct "${connection}"
        password_args
    elif [ -n "${RYLL_KASM_DIRECT:-}" ]; then
        printf '%s\n' --direct "${RYLL_KASM_DIRECT}"
        password_args
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
    if [ -n "${DISABLE_CUSTOM_STARTUP:-}" ]; then
        return
    fi

    local url=${KASM_URL:-${LAUNCH_URL:-}}

    # Give Kasm a moment to deliver the launch form answers when they
    # are the only way to get a connection, or when RYLL_KASM_LAUNCH_FORM
    # says the workspace has a form whose answers override
    # RYLL_KASM_DIRECT and RYLL_KASM_PASSWORD_FILE.
    if [ -z "${url}" ] && { [ -z "${RYLL_KASM_DIRECT:-}" ] || [ -n "${RYLL_KASM_LAUNCH_FORM:-}" ]; }; then
        local waited=0
        while ! jq -e . "${LAUNCH_SELECTIONS}" > /dev/null 2>&1 \
                && [ ${waited} -lt ${LAUNCH_SELECTIONS_WAIT} ]; do
            sleep 1
            waited=$((waited + 1))
        done
    fi
    if [ -r "${LAUNCH_SELECTIONS}" ] && ! jq -e . "${LAUNCH_SELECTIONS}" > /dev/null 2>&1; then
        echo "ryll: cannot parse the launch form answers in" \
            "${LAUNCH_SELECTIONS}." >&2
        exit 1
    fi

    local -a args
    mapfile -t args < <(ryll_args "${url}")
    if [ ${#args[@]} -eq 0 ]; then
        echo "ryll: no connection configured. Give the workspace a launch" \
            "form (see docs/kasm-workspace.md), set RYLL_KASM_DIRECT in its" \
            "Docker Run Config Override, or launch it with a kasm_url" \
            "pointing at a .vv file." >&2
        exit 1
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
