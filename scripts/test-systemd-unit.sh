#!/usr/bin/env bash
# Starts deploy/systemd/hoster.service for real in the calling user's systemd instance, with a
# stub in place of the hoster binary, and reports which of its protections took effect
# (ADR-0023). `systemd-analyze verify` (scripts/check-systemd-units.sh) cannot catch a unit that
# verifies but fails to start, such as a capability drop on a host that restricts unprivileged
# user namespaces (218/CAPABILITIES on Ubuntu >= 23.10).
#
# Checks:
#   - the unit starts and stays active;
#   - NoNewPrivileges and the seccomp filters apply;
#   - LimitNOFILE= sets the soft open file limit (ADR-0065);
#   - the file-system sandbox (a private mount namespace) applies, unless
#     kernel.apparmor_restrict_unprivileged_userns=1, where systemd skips it (reported; accepted, ADR-0064).
#
# Needs a running user manager (linger or a login session) and XDG_RUNTIME_DIR. The unit runs
# as `hoster-unit-test.service`, without the podman.socket dependency and with Type=exec (the
# stub sends no READY=1); everything it installs is removed again. The directories in
# ReadWritePaths= must exist for the unit to start; the ones this script creates are removed
# afterwards if still empty.
#
# Usage: scripts/test-systemd-unit.sh
set -euo pipefail

root=$(cd "$(dirname "$0")/.." && pwd)
unit_src="$root/deploy/systemd/hoster.service"
name=hoster-unit-test.service
units="${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user"
# Under $HOME, not /tmp: PrivateTmp= hides the host's /tmp from the unit.
stub_dir="$HOME/.cache/hoster-unit-test"
created_dirs=()

fail() {
    echo "error: $*" >&2
    exit 1
}

cleanup() {
    systemctl --user stop "$name" > /dev/null 2>&1 || true
    systemctl --user reset-failed "$name" > /dev/null 2>&1 || true
    rm -f "$units/$name"
    systemctl --user daemon-reload > /dev/null 2>&1 || true
    rm -rf "$stub_dir"
    # Deepest first, so parents created along the way go too (only if still empty).
    local i
    for ((i = ${#created_dirs[@]} - 1; i >= 0; i--)); do
        rmdir "${created_dirs[i]}" 2> /dev/null || true
    done
}

# mkdir -p that records every directory it creates, for cleanup.
make_dir() {
    local missing=() dir=$1
    while [ ! -d "$dir" ]; do
        missing=("$dir" "${missing[@]}")
        dir=$(dirname "$dir")
    done
    for dir in "${missing[@]}"; do
        mkdir "$dir"
        created_dirs+=("$dir")
    done
}
trap cleanup EXIT

[ -n "${XDG_RUNTIME_DIR:-}" ] || fail "XDG_RUNTIME_DIR is not set; needs a user manager"
systemctl --user is-system-running > /dev/null 2>&1 ||
    [ "$(systemctl --user is-system-running 2> /dev/null || true)" = degraded ] ||
    fail "no running user manager (enable linger or log in)"

# The stub: stays up like hoster does.
make_dir "$stub_dir"
printf '#!/bin/sh\nexec sleep infinity\n' > "$stub_dir/hoster"
chmod 0755 "$stub_dir/hoster"

# ReadWritePaths= entries must exist, or the unit fails with 226/NAMESPACE where namespaces work.
for dir in "$HOME/.local/share/hoster" "${XDG_CONFIG_HOME:-$HOME/.config}/hoster" \
    "$HOME/.local/lib/hoster"; do
    make_dir "$dir"
done

make_dir "$units"
sed -e "s|^ExecStart=%h/.local/lib/hoster/current/hoster |ExecStart=$stub_dir/hoster |" \
    -e 's/^Type=notify$/Type=exec/' \
    -e '/^Requires=podman\.socket$/d' \
    -e '/^After=podman\.socket$/d' \
    "$unit_src" > "$units/$name"
grep -q "^ExecStart=$stub_dir/hoster " "$units/$name" || fail "could not point ExecStart= at the stub"
grep -q '^Type=exec$' "$units/$name" || fail "could not switch Type= to exec"
systemctl --user daemon-reload

if ! systemctl --user start "$name"; then
    systemctl --user status "$name" --no-pager >&2 || true
    journalctl --user -u "$name" --no-pager -n 20 >&2 || true
    fail "$unit_src does not start"
fi
sleep 1
[ "$(systemctl --user is-active "$name")" = active ] || fail "$name did not stay active"
pid=$(systemctl --user show -p MainPID --value "$name")
[ "$pid" -gt 0 ] || fail "no MainPID for $name"
echo "ok: the unit starts (pid $pid)"

status_field() {
    sed -n "s/^$1:[[:space:]]*//p" "/proc/$pid/status"
}
[ "$(status_field NoNewPrivs)" = 1 ] || fail "NoNewPrivileges= is not in effect"
echo "ok: NoNewPrivileges in effect"
[ "$(status_field Seccomp)" = 2 ] || fail "no seccomp filter (SystemCallFilter= and friends)"
echo "ok: seccomp filters in effect ($(status_field Seccomp_filters) filters)"
want_files=$(sed -n 's/^LimitNOFILE=//p' "$unit_src")
soft_files=$(awk '/^Max open files/ { print $4 }' "/proc/$pid/limits")
[ "$soft_files" = "$want_files" ] || fail "soft open file limit is $soft_files, not LimitNOFILE=$want_files"
echo "ok: open file limit $soft_files"

restricted=$(cat /proc/sys/kernel/apparmor_restrict_unprivileged_userns 2> /dev/null || echo 0)
if [ "$(readlink "/proc/$pid/ns/mnt")" != "$(readlink /proc/self/ns/mnt)" ]; then
    echo "ok: file-system sandbox in effect (private mount namespace)"
elif [ "$restricted" = 1 ]; then
    echo "warn: file-system sandbox skipped: kernel.apparmor_restrict_unprivileged_userns=1" \
        "(ProtectSystem, ProtectHome, ReadWritePaths, PrivateTmp are not applied; accepted, ADR-0064)"
else
    fail "file-system sandbox not applied although user namespaces are unrestricted"
fi
echo "all unit checks passed"
