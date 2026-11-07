#!/usr/bin/env bash
# Checks the systemd files in deploy/ (ADR-0023):
#   - `systemd-analyze --user verify` on deploy/systemd/*.service, with ExecStart= pointed at a
#     stub so the check does not need an installed hoster binary;
#   - deploy/sysctl/50-hoster.conf sets net.ipv4.ip_unprivileged_port_start = 80;
#   - TimeoutStopSec= in the unit is above the default proxy grace period plus graceful shutdown
#     timeout in deploy/hoster.example.toml.
# With --security it also prints `systemd-analyze --user security --offline=yes` for the unit.
#
# Usage: scripts/check-systemd-units.sh [--security]
set -euo pipefail

root=$(cd "$(dirname "$0")/.." && pwd)
unit="$root/deploy/systemd/hoster.service"
sysctl_conf="$root/deploy/sysctl/50-hoster.conf"
example="$root/deploy/hoster.example.toml"

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

# A stub binary in place of /usr/local/bin/hoster.
printf '#!/bin/sh\nexit 0\n' > "$work/hoster"
chmod +x "$work/hoster"
mkdir -p "$work/units"
sed "s|^ExecStart=/usr/local/bin/hoster |ExecStart=$work/hoster |" "$unit" > "$work/units/hoster.service"
if ! grep -q "^ExecStart=$work/hoster " "$work/units/hoster.service"; then
    echo "error: $unit has no 'ExecStart=/usr/local/bin/hoster ...' line" >&2
    exit 1
fi

# podman.socket is a dependency; give verify a stub when podman is not installed (CI runners).
if ! systemctl --user cat podman.socket > /dev/null 2>&1 &&
    [ ! -e /usr/lib/systemd/user/podman.socket ]; then
    printf '[Socket]\nListenStream=%%t/podman/podman.sock\n' > "$work/units/podman.socket"
    printf '[Service]\nExecStart=/bin/true\n' > "$work/units/podman.service"
fi

# Unit search path: our directory first, then the defaults (trailing colon).
export SYSTEMD_UNIT_PATH="$work/units:"
# `--user` needs a runtime directory for %t, which a CI runner without a login session lacks.
if [ -z "${XDG_RUNTIME_DIR:-}" ]; then
    mkdir -m 0700 "$work/runtime"
    export XDG_RUNTIME_DIR="$work/runtime"
fi
systemd-analyze --user verify "$work/units/hoster.service"
echo "ok: systemd-analyze verify $unit"

# The sysctl drop-in.
if ! grep -Eq '^net\.ipv4\.ip_unprivileged_port_start[[:space:]]*=[[:space:]]*80$' "$sysctl_conf"; then
    echo "error: $sysctl_conf must set net.ipv4.ip_unprivileged_port_start = 80" >&2
    exit 1
fi
echo "ok: $sysctl_conf"

# TimeoutStopSec= against the proxy shutdown settings (whole seconds, "10s" style).
seconds() {
    sed -n "s/^$1 = \"\([0-9]\+\)s\"$/\1/p" "$example"
}
stop=$(sed -n 's/^TimeoutStopSec=\([0-9]\+\)s$/\1/p' "$unit")
grace=$(seconds grace_period)
shutdown=$(seconds graceful_shutdown_timeout)
if [ -z "$stop" ] || [ -z "$grace" ] || [ -z "$shutdown" ]; then
    echo "error: could not read TimeoutStopSec= ($stop), grace_period ($grace) or" \
        "graceful_shutdown_timeout ($shutdown)" >&2
    exit 1
fi
if [ "$stop" -le $((grace + shutdown)) ]; then
    echo "error: TimeoutStopSec=${stop}s must exceed grace_period + graceful_shutdown_timeout" \
        "(${grace}s + ${shutdown}s)" >&2
    exit 1
fi
echo "ok: TimeoutStopSec=${stop}s > ${grace}s + ${shutdown}s"

if [ "${1:-}" = "--security" ]; then
    systemd-analyze --user security --offline=yes --no-pager "$work/units/hoster.service"
fi
