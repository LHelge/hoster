#!/usr/bin/env bash
# Exercises install.sh end to end against fake releases served from file:// URLs
# (HOSTER_INSTALL_RELEASES_URL), with a stub `hoster` binary per version:
#   - a bad sysctl file and a --dry-run on a fresh host change nothing;
#   - an install creates the user, linger, sysctl, the versioned layout, the unit, the dirs and
#     the config, and starts nothing;
#   - a re-run changes nothing (and keeps an edited config);
#   - newer releases switch `current` and prune to current + previous;
#   - a downgrade, a checksum mismatch and a rolled-back version are refused or kept;
#   - a user manager without the cpu controller gets the delegation drop-in and a restart while
#     hoster is idle; an existing drop-in is left alone; with hoster enabled, a hoster container
#     running or podman failing, it gets only a notice.
#
# It changes the host for real (user `hoster`, /etc/sysctl.d, linger), so it only runs on a
# disposable machine, as root, with HOSTER_INSTALL_TEST_DISPOSABLE=1 (CI's runner does that).
# To run it locally, start it in a throwaway systemd container instead:
#
#   scripts/test-install.sh --container [image]   # default docker.io/library/debian:trixie
#
# which needs podman and network access for the image's packages (systemd, curl, podman).
set -euo pipefail

root=$(cd "$(dirname "$0")/.." && pwd)

if [ "${1:-}" = --container ]; then
    image=${2:-docker.io/library/debian:trixie}
    name=hoster-install-test-$$
    trap 'podman rm -f "$name" > /dev/null 2>&1 || true' EXIT
    # Rootless containers cannot write /proc/sys, so the sysctl is set from outside and
    # install.sh finds it already in place.
    podman run -d --name "$name" --systemd=always --privileged \
        --sysctl net.ipv4.ip_unprivileged_port_start=80 \
        -v "$root:/src:ro,Z" "$image" \
        sh -c 'if command -v apt-get >/dev/null; then
                   apt-get update -q && DEBIAN_FRONTEND=noninteractive apt-get install -y -q systemd systemd-sysv curl ca-certificates dbus >/dev/null
               else
                   dnf install -y -q systemd curl >/dev/null
               fi && exec /sbin/init' > /dev/null
    for _ in $(seq 1 300); do
        if podman exec "$name" test -d /run/systemd/system 2> /dev/null &&
            podman exec "$name" systemctl is-system-running --wait > /dev/null 2>&1; then
            break
        fi
        if podman exec "$name" test -d /run/systemd/system 2> /dev/null &&
            [ "$(podman exec "$name" systemctl is-system-running 2> /dev/null)" = degraded ]; then
            break
        fi
        sleep 1
    done
    podman exec "$name" test -d /run/systemd/system || {
        podman logs "$name" | tail -20
        echo "error: systemd did not come up in $image" >&2
        exit 1
    }
    podman exec -e HOSTER_INSTALL_TEST_DISPOSABLE=1 "$name" bash /src/scripts/test-install.sh
    exit
fi

[ "$(id -u)" = 0 ] || {
    echo "error: run as root on a disposable host, or use --container" >&2
    exit 1
}
[ "${HOSTER_INSTALL_TEST_DISPOSABLE:-}" = 1 ] || {
    echo "error: this creates the hoster user and changes /etc; set HOSTER_INSTALL_TEST_DISPOSABLE=1 on a disposable host, or use --container" >&2
    exit 1
}
if getent passwd hoster > /dev/null; then
    echo "error: user hoster already exists; this test needs a fresh host" >&2
    exit 1
fi

case $(uname -m) in
    x86_64) target=x86_64-unknown-linux-musl ;;
    aarch64) target=aarch64-unknown-linux-musl ;;
    *) echo "error: unsupported $(uname -m)" >&2 && exit 1 ;;
esac

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
releases=$work/releases
# The hoster user downloads the binary, the unit and the config from these file:// paths.
chmod 0755 "$work"

failures=0
fail() {
    echo "FAIL: $*" >&2
    failures=$((failures + 1))
}
pass() { echo "ok: $*"; }
check() {
    local what=$1
    shift
    if "$@"; then pass "$what"; else fail "$what"; fi
}

# A release directory as release.yml lays it out, with a stub binary that reports $1. The
# install.sh in it is stamped with the version, as release.yml does.
make_release() {
    local tag=$1 dir=$releases/download/$1
    mkdir -p "$dir"
    cat > "$dir/hoster-$target" << EOF
#!/bin/sh
case "\$1" in
    version) printf '{"schema":1,"target":"$target","version":"${tag#v}"}\n' ;;
    doctor) echo "stub doctor for $tag"; exit 1 ;;
esac
EOF
    chmod 0755 "$dir/hoster-$target"
    cp "$root/deploy/systemd/hoster.service" "$root/deploy/sysctl/50-hoster.conf" \
        "$root/deploy/hoster.example.toml" "$dir/"
    sed "s/^local RELEASE_VERSION=\"\"\$/local RELEASE_VERSION=\"$tag\"/" "$root/install.sh" \
        > "$dir/install.sh"
    grep -q "^local RELEASE_VERSION=\"$tag\"\$" "$dir/install.sh" || {
        echo "error: could not stamp install.sh" >&2
        exit 1
    }
    (cd "$dir" && sha256sum -- * > SHA256SUMS)
}

set_latest() {
    rm -rf "$releases/latest"
    mkdir -p "$releases/latest"
    cp -r "$releases/download/$1" "$releases/latest/download"
    chmod -R a+rX "$releases"
}

export HOSTER_INSTALL_RELEASES_URL=file://$releases

# Runs install.sh the way the README says (curl | bash), output in $work/out. Returns its
# status.
install_from() {
    local url=$1
    shift
    set +e
    curl -fsSL "$url" | bash -s -- "$@" > "$work/out" 2>&1
    local status=$?
    set -e
    cat "$work/out"
    return "$status"
}

home=/home/hoster
lib=$home/.local/lib/hoster
as_hoster() {
    local uid
    uid=$(id -u hoster)
    runuser -u hoster -- env XDG_RUNTIME_DIR="/run/user/$uid" "$@"
}
current() { readlink "$lib/current" || true; }
snapshot() {
    find "$home" /etc/sysctl.d/50-hoster.conf /var/lib/systemd/linger \
        -printf '%p %m %u %s %T@ %l\n' 2> /dev/null | sort
    grep '^hoster:' /etc/subuid /etc/subgid /etc/passwd
}
not() { ! "$@"; }
out_has() { grep -qF -- "$1" "$work/out"; }

for tag in v0.1.0 v0.1.1 v0.1.2 v0.1.3 v0.1.4; do make_release "$tag"; done
set_latest v0.1.1
# v0.1.3's binary no longer matches its SHA256SUMS.
echo tampered >> "$releases/download/v0.1.3/hoster-$target"
# v0.1.4's sysctl file, which root downloads and checks before changing anything, does not.
echo 'kernel.core_pattern = |/tmp/x' >> "$releases/download/v0.1.4/50-hoster.conf"
chmod -R a+rX "$releases"

echo "--- a bad sysctl file on a fresh host changes nothing"
if install_from "$HOSTER_INSTALL_RELEASES_URL/download/v0.1.4/install.sh"; then
    fail "fresh install with a tampered sysctl file refused"
else
    pass "fresh install with a tampered sysctl file refused"
fi
check "fresh mismatch reported" out_has "checksum mismatch for 50-hoster.conf"
check "fresh mismatch creates no user" not getent passwd hoster
check "fresh mismatch installs no sysctl file" test ! -e /etc/sysctl.d/50-hoster.conf

echo "--- dry run on a fresh host"
check "dry run succeeds" install_from "$HOSTER_INSTALL_RELEASES_URL/latest/download/install.sh" --dry-run
check "dry run prints the user it would create" out_has "would run: useradd"
check "dry run creates no user" not getent passwd hoster
check "dry run installs no sysctl file" test ! -e /etc/sysctl.d/50-hoster.conf
check "dry run enables no linger" test ! -e /var/lib/systemd/linger/hoster

echo "--- install v0.1.0"
check "install succeeds" install_from "$HOSTER_INSTALL_RELEASES_URL/download/v0.1.0/install.sh"
[ "$failures" = 0 ] || {
    echo "aborting: the install failed" >&2
    exit 1
}
check "user exists" getent passwd hoster
check "subuid range" grep -q '^hoster:' /etc/subuid
check "subgid range" grep -q '^hoster:' /etc/subgid
check "linger" test -e /var/lib/systemd/linger/hoster
check "sysctl file" cmp -s /etc/sysctl.d/50-hoster.conf "$root/deploy/sysctl/50-hoster.conf"
check "current is v0.1.0" test "$(current)" = versions/v0.1.0
check "binary 0755 owned by hoster" test "$(stat -c '%a %U' "$lib/versions/v0.1.0/hoster")" = "755 hoster"
check "unit installed" cmp -s "$home/.config/systemd/user/hoster.service" "$root/deploy/systemd/hoster.service"
check "config 0600" test "$(stat -c '%a %U' "$home/.config/hoster/hoster.toml")" = "600 hoster"
check "state dir 0700" test "$(stat -c '%a %U' "$home/.local/share/hoster")" = "700 hoster"
check "config dir 0700" test "$(stat -c '%a %U' "$home/.config/hoster")" = "700 hoster"
check "hoster on PATH" test "$(readlink "$home/.local/bin/hoster")" = ../lib/hoster/current/hoster
check "podman.socket enabled" as_hoster systemctl --user -q is-enabled podman.socket
check "catatonit installed where podman looks" test -e /usr/libexec/podman/catatonit -o -e /usr/lib/podman/catatonit
check "hoster.service not started" not as_hoster systemctl --user -q is-active hoster.service
check "hoster.service not enabled" not as_hoster systemctl --user -q is-enabled hoster.service
check "doctor ran" out_has "stub doctor for v0.1.0"
check "manual steps printed" out_has "systemctl --user enable --now hoster"

echo "--- re-run changes nothing"
echo '# edited by the admin' >> "$home/.config/hoster/hoster.toml"
snapshot > "$work/before"
check "re-run succeeds" install_from "$HOSTER_INSTALL_RELEASES_URL/download/v0.1.0/install.sh"
snapshot > "$work/after"
check "re-run reports nothing to change" out_has "nothing to change"
check "re-run leaves files alone" diff -u "$work/before" "$work/after"
check "dry run on the installed host plans nothing" install_from "$HOSTER_INSTALL_RELEASES_URL/download/v0.1.0/install.sh" --dry-run
check "dry run reports 0 changes" out_has "0 change(s) would be made"

echo "--- latest (v0.1.1) updates"
check "update succeeds" install_from "$HOSTER_INSTALL_RELEASES_URL/latest/download/install.sh"
check "current is v0.1.1" test "$(current)" = versions/v0.1.1
check "v0.1.0 kept as previous" test -x "$lib/versions/v0.1.0/hoster"
check "update notice" out_has "Updated from v0.1.0 to v0.1.1"
check "edited config kept" grep -q '^# edited by the admin' "$home/.config/hoster/hoster.toml"

echo "--- v0.1.2 prunes v0.1.0"
set_latest v0.1.2
check "v0.1.2 succeeds" install_from "$HOSTER_INSTALL_RELEASES_URL/download/v0.1.2/install.sh" --version v0.1.2
check "current is v0.1.2" test "$(current)" = versions/v0.1.2
check "v0.1.1 kept as previous" test -x "$lib/versions/v0.1.1/hoster"
check "v0.1.0 pruned" test ! -e "$lib/versions/v0.1.0"

echo "--- refusals"
if install_from "$HOSTER_INSTALL_RELEASES_URL/latest/download/install.sh" --version v0.1.1; then
    fail "downgrade refused"
else
    pass "downgrade refused"
fi
check "downgrade names hoster rollback" out_has "hoster rollback"
check "current still v0.1.2" test "$(current)" = versions/v0.1.2
snapshot > "$work/before"
if install_from "$HOSTER_INSTALL_RELEASES_URL/latest/download/install.sh" --version v0.1.3; then
    fail "checksum mismatch refused"
else
    pass "checksum mismatch refused"
fi
check "mismatch reported" out_has "checksum mismatch"
check "v0.1.3 not installed" test ! -e "$lib/versions/v0.1.3"
check "current still v0.1.2 after mismatch" test "$(current)" = versions/v0.1.2
snapshot > "$work/after"
check "mismatch changes no file" diff -u "$work/before" "$work/after"

echo "--- a rolled-back version is not re-installed by a plain re-run"
runuser -u hoster -- ln -sfn versions/v0.1.1 "$lib/current"
check "re-run after rollback succeeds" install_from "$HOSTER_INSTALL_RELEASES_URL/latest/download/install.sh"
check "current stays v0.1.1" test "$(current)" = versions/v0.1.1
check "explicit --version switches forward" install_from "$HOSTER_INSTALL_RELEASES_URL/latest/download/install.sh" --version v0.1.2
check "current is v0.1.2 again" test "$(current)" = versions/v0.1.2

echo "--- cgroup delegation (ADR-0059)"
uid=$(id -u hoster)
unit=user@$uid.service
controllers=/sys/fs/cgroup/user.slice/user-$uid.slice/$unit/cgroup.controllers
dropin=/etc/systemd/system/$unit.d/hoster-delegate.conf
undelegate=/etc/systemd/system/user@.service.d/00-test-undelegate.conf
manager_pid() { systemctl show -p MainPID --value "$unit"; }
has_cpu() { grep -qw cpu "$controllers"; }
# Restarts the user manager, whose template's delegation is cut to `pids memory` (Ubuntu
# 22.04's default) while $undelegate exists; a drop-in named later still adds to it.
restart_manager() {
    systemctl daemon-reload
    # The checks restart it more often than its start limit allows.
    systemctl reset-failed "$unit"
    systemctl restart "$unit"
    for _ in $(seq 1 60); do
        [ -S "/run/user/$uid/systemd/private" ] && return
        sleep 0.5
    done
    echo "error: $unit did not come back" >&2
    exit 1
}
undelegate_cpu() {
    mkdir -p "${undelegate%/*}"
    printf '[Service]\nDelegate=\nDelegate=pids memory\n' > "$undelegate"
}
# Rootless podman cannot run inside this test's container (nested user namespaces), so
# install.sh's `podman ps` goes through a stand-in that answers as $podman_mode says: no
# containers, a running hoster-* container, or a failure (explicit, since real podman works on
# a VM such as the CI runner); anything else goes to the real podman.
podman_mode=/run/test-podman-mode
cat > /usr/local/bin/podman << SHIM
#!/bin/sh
if [ "\$1" = ps ]; then
    case \$(cat $podman_mode 2> /dev/null) in
        none) exit 0 ;;
        running) echo 0123456789ab && exit 0 ;;
        fail) echo 'Error: unable to connect to Podman socket' >&2 && exit 125 ;;
    esac
fi
exec /usr/bin/podman "\$@"
SHIM
chmod 0755 /usr/local/bin/podman
# Runs install.sh with cpu taken away from the user manager, the drop-in as $1 says (absent, or
# the admin's own) and podman answering $2. Sets before_pid to the manager's PID before it.
undelegated_install() {
    undelegate_cpu
    rm -f "$dropin"
    if [ "$1" = own ]; then
        mkdir -p "${dropin%/*}"
        echo "# the admin's own" > "$dropin"
    fi
    echo "$2" > "$podman_mode"
    restart_manager
    before_pid=$(manager_pid)
    install_from "$HOSTER_INSTALL_RELEASES_URL/latest/download/install.sh"
}
undelegate_cpu
restart_manager
if has_cpu; then
    echo "note: cannot take the cpu controller away from $unit here; skipping the delegation checks"
else
    check "install while hoster is idle succeeds" undelegated_install absent none
    check "drop-in written" grep -qx 'Delegate=cpu memory pids' "$dropin"
    check "drop-in is 0644 root" test "$(stat -c '%a %U' "$dropin")" = "644 root"
    check "idle user manager restarted" test "$(manager_pid)" != "$before_pid"
    check "cpu delegated after the restart" has_cpu
    check "podman.socket listening after the restart" as_hoster systemctl --user -q is-active podman.socket

    check "install with an existing drop-in succeeds" undelegated_install own none
    check "existing drop-in left alone" grep -qx "# the admin's own" "$dropin"
    check "existing drop-in reported" out_has "$dropin exists and is left alone"
    check "a drop-in that adds nothing is warned about" out_has "its configuration does not add them"
    check "no restart for a drop-in that adds nothing" test "$(manager_pid)" = "$before_pid"

    as_hoster systemctl --user enable hoster.service > /dev/null 2>&1
    check "install with hoster enabled succeeds" undelegated_install absent none
    as_hoster systemctl --user disable hoster.service > /dev/null 2>&1
    check "drop-in written while hoster is enabled" grep -qx 'Delegate=cpu memory pids' "$dropin"
    check "no restart while hoster is enabled" test "$(manager_pid)" = "$before_pid"
    check "restart notice printed" out_has "delegates cpu once it restarts"
    check "hoster.service still not started" not as_hoster systemctl --user -q is-active hoster.service

    check "install with a hoster container running succeeds" undelegated_install absent running
    check "no restart while a hoster container runs" test "$(manager_pid)" = "$before_pid"
    check "drop-in written while a container runs" test -f "$dropin"

    check "install when podman cannot tell succeeds" undelegated_install absent fail
    check "no restart when podman cannot tell" test "$(manager_pid)" = "$before_pid"
fi
rm -f "$undelegate" /usr/local/bin/podman "$podman_mode"
restart_manager

if [ "$failures" -gt 0 ]; then
    echo "$failures check(s) failed" >&2
    exit 1
fi
echo "all install.sh checks passed"
