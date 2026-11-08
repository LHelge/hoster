#!/usr/bin/env bash
# Installs hoster on a Linux host with systemd, or brings an existing install up to date
# (ADR-0052, docs/install.md):
#
#   curl -fsSL https://github.com/LHelge/hoster/releases/latest/download/install.sh | sudo bash
#   curl -fsSL .../install.sh | sudo bash -s -- --version v0.2.0 --dry-run
#
# As root it installs podman (apt or dnf) if missing, creates the `hoster` system user with
# subordinate uid/gid ranges, enables linger and installs the low-port sysctl, which it downloads
# and checks against the release's SHA256SUMS itself before changing anything. As `hoster` it
# downloads the binary for this architecture and checks it against the release's SHA256SUMS,
# installs it as ~/.local/lib/hoster/versions/vX.Y.Z and switches `current` to it, enables
# podman.socket, installs the user unit, creates the state and config directories, writes the
# example config if there is none and runs `hoster doctor`. It never starts or restarts the
# service, and re-running it changes nothing unless a newer version is to be installed.
#
# Testing only: HOSTER_INSTALL_RELEASES_URL replaces https://github.com/LHelge/hoster/releases
# (file:// and http:// URLs are accepted then), laid out as <url>/download/vX.Y.Z/<file> and
# <url>/latest/download/<file>. scripts/test-install.sh uses it.
#
# Everything is inside one function that the last line calls, so a truncated download defines
# nothing and runs nothing.

hoster_install() {
set -euo pipefail
# Command substitutions such as `current=$(hoster_binary ...)` stop at the first failure too.
shopt -s inherit_errexit

# Set to the release's tag when release.yml publishes this file, so the script installs the
# binary it was released with. Empty in a checkout: then it resolves the latest stable release.
local RELEASE_VERSION=""

local -r SERVICE_USER=hoster
local -r DEFAULT_RELEASES_URL=https://github.com/LHelge/hoster/releases
local -r SYSCTL_DEST=/etc/sysctl.d/50-hoster.conf

# ---------------------------------------------------------------------------------------------
# Helpers, shared with the part that runs as hoster (passed over with `declare -f`). Root never
# installs a file or writes through a path the hoster user could have changed: a compromised
# hoster service must not be able to feed root the sysctl drop-in (kernel.core_pattern runs a
# program as root) or a symlinked change log while the script runs.

say() { printf 'hoster-install: %s\n' "$*" >&2; }
ok() { say "ok: $*"; }
die() {
    say "error: $*"
    exit 1
}

# Runs a command that changes the host, or only prints it under --dry-run. Every change is
# counted in $CHANGES (a file), so the summary can tell whether anything happened.
run() {
    local quoted
    quoted=$(printf '%q ' "$@")
    if [ "${DRY_RUN:-0}" = 1 ]; then
        say "would run: ${quoted% }"
    else
        say "+ ${quoted% }"
        # Explicit, because errexit does not apply inside `if` and `||` contexts.
        "$@" || die "failed: ${quoted% }"
    fi
    if [ -n "${CHANGES:-}" ]; then
        printf '%s\n' "${quoted% }" >> "$CHANGES"
    fi
}

# curl options for every request to the releases: HTTPS only, redirects (GitHub's downloads
# redirect to its object storage) included, unless the testing override is in use.
curl_opts() {
    if [ "$RELEASES_URL" = "$DEFAULT_RELEASES_URL" ]; then
        printf '%s\n' --proto '=https' --proto-redir '=https' --tlsv1.2
    fi
}

# Fetches release file $1 into directory $2. No release file comes near the size limit; it
# only bounds what a misbehaving server can make the script write.
fetch() {
    local url="$RELEASE_BASE/$1" proto
    mapfile -t proto < <(curl_opts)
    curl -fsSL --retry 3 --max-filesize 512M "${proto[@]}" -o "$2/$1" "$url" ||
        die "could not download $url"
}

# Checks file $1 in directory $2 against $2/SHA256SUMS; aborts on a missing or wrong sum.
verify() {
    local sums
    sums=$(grep -E "^[0-9a-f]{64} [ *]$1\$" "$2/SHA256SUMS" || true)
    [ "$(printf '%s' "$sums" | grep -c .)" = 1 ] ||
        die "SHA256SUMS of $RELEASE_TAG lists $1 $(printf '%s' "$sums" | grep -c .) times, expected once"
    (cd "$2" && printf '%s\n' "$sums" | sha256sum --check --status) ||
        die "checksum mismatch for $1 from $RELEASE_BASE: refusing to install it"
}

# Prints -1, 0 or 1 as SemVer version $1 is lower than, equal to or higher than $2 (with or
# without the leading v; build metadata is ignored).
semver_cmp() {
    # SemVer orders pre-release identifiers in ASCII, not in the locale's collation.
    local LC_ALL=C
    local a=${1#v} b=${2#v}
    a=${a%%+*}
    b=${b%%+*}
    local a_core=${a%%-*} b_core=${b%%-*} a_pre="" b_pre=""
    [ "$a" != "$a_core" ] && a_pre=${a#*-}
    [ "$b" != "$b_core" ] && b_pre=${b#*-}
    local -a x y
    IFS=. read -ra x <<< "$a_core"
    IFS=. read -ra y <<< "$b_core"
    local i
    for i in 0 1 2; do
        if ((10#${x[i]} < 10#${y[i]})); then echo -1 && return; fi
        if ((10#${x[i]} > 10#${y[i]})); then echo 1 && return; fi
    done
    # A version without a pre-release is higher than the same version with one.
    if [ -z "$a_pre" ] && [ -z "$b_pre" ]; then echo 0 && return; fi
    if [ -z "$a_pre" ]; then echo 1 && return; fi
    if [ -z "$b_pre" ]; then echo -1 && return; fi
    IFS=. read -ra x <<< "$a_pre"
    IFS=. read -ra y <<< "$b_pre"
    for ((i = 0; i < ${#x[@]} || i < ${#y[@]}; i++)); do
        if [ "$i" -ge "${#x[@]}" ]; then echo -1 && return; fi
        if [ "$i" -ge "${#y[@]}" ]; then echo 1 && return; fi
        local p=${x[i]} q=${y[i]} pn=0 qn=0
        [[ $p =~ ^[0-9]+$ ]] && pn=1
        [[ $q =~ ^[0-9]+$ ]] && qn=1
        if [ "$pn$qn" = 11 ]; then
            if ((10#$p < 10#$q)); then echo -1 && return; fi
            if ((10#$p > 10#$q)); then echo 1 && return; fi
        elif [ "$pn$qn" = 10 ]; then
            echo -1 && return
        elif [ "$pn$qn" = 01 ]; then
            echo 1 && return
        elif [[ $p < $q ]]; then
            echo -1 && return
        elif [[ $p > $q ]]; then
            echo 1 && return
        fi
    done
    echo 0
}

is_semver_tag() {
    [[ $1 =~ ^v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(-[0-9A-Za-z.-]+)?(\+[0-9A-Za-z.-]+)?$ ]]
}

# ---------------------------------------------------------------------------------------------
# The part that runs as hoster, with its own user bus (XDG_RUNTIME_DIR). Under --dry-run on a
# host without the user yet it runs as root against the would-be home, and only prints.

# True when the user manager can be asked about units (not on a dry run for a missing user).
user_bus() {
    [ "${NO_USER:-0}" = 0 ] && [ -S "${XDG_RUNTIME_DIR:-/nonexistent}/systemd/private" ]
}

# The canonical vX.Y.Z version directories in $1, one per line.
installed_versions() {
    local dir name
    for dir in "$1"/v*/; do
        [ -f "$dir/hoster" ] || continue
        name=$(basename "$dir")
        if is_semver_tag "$name"; then printf '%s\n' "$name"; fi
    done
    return 0
}

# The version `current` points at, if it is an installed version.
current_version() {
    local link=$1/current target
    target=$(readlink "$link" 2>/dev/null) || return 0
    case $target in
        versions/v*) ;;
        *) die "$link points outside $1/versions: $target" ;;
    esac
    target=${target#versions/}
    if is_semver_tag "$target" && [ -f "$1/versions/$target/hoster" ]; then
        printf '%s\n' "$target"
    fi
}

# Installs the binary, switches `current` and keeps only the current and previous version.
# Prints the version that is current afterwards on stdout.
hoster_binary() {
    local staging=$1 lib=$HOME/.local/lib/hoster
    local current
    current=$(current_version "$lib")
    local dest=$lib/versions/$RELEASE_TAG

    if [ -n "$current" ]; then
        case $(semver_cmp "$RELEASE_TAG" "$current") in
            0)
                ok "$RELEASE_TAG is installed and current"
                printf '%s\n' "$current"
                return
                ;;
            1)
                # Installed but not current: `hoster rollback` went back from it. Only switch
                # forward again when asked to.
                if [ -f "$dest/hoster" ] && [ "$EXPLICIT_VERSION" = 0 ]; then
                    ok "keeping $current: $RELEASE_TAG is installed but was rolled back from; pass --version $RELEASE_TAG to switch to it"
                    printf '%s\n' "$current"
                    return
                fi
                ;;
            -1)
                if [ "$EXPLICIT_VERSION" = 1 ]; then
                    die "$RELEASE_TAG is older than the current $current; switching back would not downgrade the database schema. Use \`hoster rollback\` (docs/install.md)."
                fi
                ok "keeping $current, which is newer than the latest stable release $RELEASE_TAG"
                printf '%s\n' "$current"
                return
                ;;
        esac
    fi

    if [ -f "$dest/hoster" ] && cmp -s "$dest/hoster" "$staging/hoster-$TARGET"; then
        ok "$dest/hoster is already installed"
    else
        run install -d -m 0755 "$lib" "$lib/versions" "$dest"
        # A temporary name and a rename, so a crash never leaves a partial binary (as
        # `hoster update` does).
        run install -m 0755 "$staging/hoster-$TARGET" "$dest/.hoster.tmp-$$"
        run sync "$dest/.hoster.tmp-$$"
        run mv -f "$dest/.hoster.tmp-$$" "$dest/hoster"
        run sync "$dest"
    fi

    if [ "${DRY_RUN:-0}" = 0 ]; then
        local info reported
        info=$("$dest/hoster" version --json) ||
            die "$dest/hoster does not run; is $HOME mounted noexec?"
        reported=v$(printf '%s' "$info" | sed -n 's/.*"version" *: *"\([^"]*\)".*/\1/p')
        if [ "$reported" != "$RELEASE_TAG" ]; then
            run rm -rf "$dest"
            die "the $RELEASE_TAG binary reports version $reported; not installing it"
        fi
        case $info in
            *"\"$TARGET\""*) ;;
            *) die "the $RELEASE_TAG binary for $TARGET reports another target: $info" ;;
        esac
    fi

    # Atomic switch: a new relative symlink renamed over `current`.
    run ln -sfn "versions/$RELEASE_TAG" "$lib/.current.tmp-$$"
    run mv -T "$lib/.current.tmp-$$" "$lib/current"
    run sync "$lib"

    # Keep the new current and the previous one: the newest version older than it.
    local version previous=""
    while IFS= read -r version; do
        [ "$(semver_cmp "$version" "$RELEASE_TAG")" = -1 ] || continue
        if [ -z "$previous" ] || [ "$(semver_cmp "$version" "$previous")" = 1 ]; then
            previous=$version
        fi
    done < <(installed_versions "$lib/versions")
    while IFS= read -r version; do
        [ "$version" = "$RELEASE_TAG" ] || [ "$version" = "$previous" ] && continue
        run rm -rf "$lib/versions/$version"
    done < <(installed_versions "$lib/versions")

    printf '%s\n' "$RELEASE_TAG"
}

# Installs file $1 as $2 with mode $3 unless it is already identical. Sets INSTALLED to 1 when
# it installed the file, 0 when it was up to date.
install_file() {
    INSTALLED=0
    if [ -f "$2" ] && cmp -s "$1" "$2"; then
        ok "$2 is up to date"
        return 0
    fi
    run install -m "$3" "$1" "$2"
    INSTALLED=1
}

install_dir() {
    if [ -d "$1" ]; then
        ok "$1 exists"
    else
        run install -d -m "$2" "$1"
    fi
}

# Everything the hoster user does. $1 is the staging directory with the verified files.
hoster_user() {
    local staging=$1
    cd "$HOME" 2>/dev/null || cd /

    local current
    current=$(hoster_binary "$staging")
    local updated=0
    [ "$current" = "$RELEASE_TAG" ] && updated=1

    # A stable name on PATH for `hoster update`, `hoster rollback` and `hoster doctor`.
    local bin=$HOME/.local/bin/hoster
    if [ -L "$bin" ] && [ "$(readlink "$bin")" = ../lib/hoster/current/hoster ]; then
        ok "$bin links to the current version"
    elif [ -e "$bin" ] || [ -L "$bin" ]; then
        say "note: $bin exists and is left alone; the current binary is ~/.local/lib/hoster/current/hoster"
    else
        install_dir "$HOME/.local/bin" 0755
        run ln -s ../lib/hoster/current/hoster "$bin"
    fi

    # The rootless podman API socket (ADR-0002).
    if user_bus && systemctl --user -q is-enabled podman.socket 2>/dev/null &&
        systemctl --user -q is-active podman.socket 2>/dev/null; then
        ok "podman.socket is enabled and listening"
    else
        run systemctl --user enable --now podman.socket
    fi

    # The unit, from the release whose binary is current. A newer release's unit is not
    # installed while an older binary stays current.
    local units=$HOME/.config/systemd/user
    if [ "$updated" = 1 ]; then
        install_dir "$HOME/.config" 0755
        install_dir "$HOME/.config/systemd" 0755
        install_dir "$units" 0755
        local INSTALLED
        install_file "$staging/hoster.service" "$units/hoster.service" 0644
        if [ "$INSTALLED" = 1 ]; then
            run systemctl --user daemon-reload
        fi
    fi

    # State and configuration (ADR-0023, ADR-0024). Both must exist before the first start
    # because the unit makes everything else read-only.
    install_dir "$HOME/.local/share/hoster" 0700
    install_dir "$HOME/.config/hoster" 0700
    local config=$HOME/.config/hoster/hoster.toml
    if [ -e "$config" ]; then
        ok "$config exists and is left alone"
    else
        run install -m 0600 "$staging/hoster.example.toml" "$config"
    fi

    if [ "${DRY_RUN:-0}" = 1 ]; then
        say "would run: $HOME/.local/lib/hoster/current/hoster doctor"
        return
    fi
    # The handle from the unit's drop-in, if the admin has set it, so doctor sees what the
    # service will.
    local word words=() handle=()
    if user_bus; then
        read -ra words <<< "$(systemctl --user show hoster.service -p Environment --value 2>/dev/null || true)"
        for word in "${words[@]}"; do
            case $word in HOSTER_ADMIN_GITHUB=*) handle=("$word") ;; esac
        done
    fi
    say "running hoster doctor; failures are expected until the steps below are done"
    env "${handle[@]}" "$HOME/.local/lib/hoster/current/hoster" doctor >&2 ||
        say "hoster doctor reported failures (see above)"
}

# ---------------------------------------------------------------------------------------------
# The root part.

usage() {
    cat << 'EOF'
Usage: curl -fsSL https://github.com/LHelge/hoster/releases/latest/download/install.sh | sudo bash
       sudo bash install.sh [--version vX.Y.Z] [--dry-run]

Installs hoster as a systemd user service of the `hoster` user, or updates an existing install
to a newer release. Safe to re-run. Never starts or restarts the service.

Options:
  --version vX.Y.Z  install this release (pre-releases only this way); default: this script's
                    own release, or the latest stable release
  --dry-run         print every action instead of doing it (downloads into a temporary
                    directory to check the release, changes nothing)
  -h, --help        show this help

See https://github.com/LHelge/hoster/blob/main/docs/install.md
EOF
}

# Downloads SHA256SUMS and release files $2... into directory $1 and verifies each of them.
fetch_release() {
    local dir=$1 file
    shift
    fetch SHA256SUMS "$dir"
    for file in "$@"; do
        fetch "$file" "$dir"
        verify "$file" "$dir"
    done
    ok "downloaded $* from $RELEASE_TAG and verified it against SHA256SUMS"
}

# Runs function $1 (with the remaining arguments) as the hoster user with its user bus. Its
# changes are logged to $USER_CHANGES, a file the hoster user owns.
as_hoster() {
    local uid home
    uid=$(id -u "$SERVICE_USER")
    home=$(getent passwd "$SERVICE_USER" | cut -d: -f6)
    local functions
    functions=$(declare -f say ok die run curl_opts fetch verify fetch_release semver_cmp \
        is_semver_tag user_bus installed_versions current_version hoster_binary install_file \
        install_dir hoster_user)
    runuser -u "$SERVICE_USER" -- env -i \
        HOME="$home" USER="$SERVICE_USER" LOGNAME="$SERVICE_USER" SHELL=/bin/bash \
        PATH=/usr/local/bin:/usr/bin:/bin:/usr/local/sbin:/usr/sbin:/sbin \
        LANG=C.UTF-8 XDG_RUNTIME_DIR="/run/user/$uid" \
        DBUS_SESSION_BUS_ADDRESS="unix:path=/run/user/$uid/bus" \
        DRY_RUN="$DRY_RUN" CHANGES="$USER_CHANGES" NO_USER=0 TARGET="$TARGET" \
        RELEASE_TAG="$RELEASE_TAG" RELEASE_BASE="$RELEASE_BASE" RELEASES_URL="$RELEASES_URL" \
        DEFAULT_RELEASES_URL="$DEFAULT_RELEASES_URL" EXPLICIT_VERSION="$EXPLICIT_VERSION" \
        bash -c "set -euo pipefail; shopt -s inherit_errexit; $functions"$'\n''"$@"' \
        hoster-install "$@"
}

check_host() {
    [ "$(uname -s)" = Linux ] || die "hoster runs on Linux only, not $(uname -s)"
    case $(uname -m) in
        x86_64 | amd64) TARGET=x86_64-unknown-linux-musl ;;
        aarch64 | arm64) TARGET=aarch64-unknown-linux-musl ;;
        *) die "no hoster binary for $(uname -m); releases cover x86_64 and aarch64" ;;
    esac
    [ "$(id -u)" = 0 ] || die "run this as root, e.g. \`curl -fsSL .../install.sh | sudo bash\`"
    [ -d /run/systemd/system ] || die "systemd is not running as the init system"
    local cmd
    for cmd in curl sha256sum getent loginctl systemctl cmp; do
        command -v "$cmd" > /dev/null || die "$cmd is missing"
    done
}

# Picks the release: --version, this script's own release, or the latest stable one.
choose_release() {
    if [ -n "$RELEASE_TAG" ]; then
        EXPLICIT_VERSION=1
    elif [ -n "$RELEASE_VERSION" ]; then
        RELEASE_TAG=$RELEASE_VERSION
    else
        # A checkout's copy: ask GitHub where /latest redirects to (…/releases/tag/vX.Y.Z).
        local effective proto
        mapfile -t proto < <(curl_opts)
        effective=$(curl -fsSL "${proto[@]}" -o /dev/null -w '%{url_effective}' \
            "$RELEASES_URL/latest") ||
            die "could not resolve the latest release at $RELEASES_URL/latest; pass --version"
        RELEASE_TAG=${effective##*/}
    fi
    is_semver_tag "$RELEASE_TAG" || die "not a release version: $RELEASE_TAG (expected vX.Y.Z)"
    RELEASE_BASE=$RELEASES_URL/download/$RELEASE_TAG
    say "installing hoster $RELEASE_TAG ($TARGET) from $RELEASE_BASE"
}

# Installs podman, newuidmap/newgidmap and sysctl with apt or dnf when any is missing.
install_packages() {
    local missing=() apt=() dnf=()
    if ! command -v podman > /dev/null; then
        missing+=(podman) apt+=(podman) dnf+=(podman)
    fi
    if ! command -v newuidmap > /dev/null; then
        missing+=(newuidmap) apt+=(uidmap) dnf+=(shadow-utils)
    fi
    if ! command -v sysctl > /dev/null; then
        missing+=(sysctl) apt+=(procps) dnf+=(procps-ng)
    fi
    # Base tools minimal images can lack.
    if ! command -v runuser > /dev/null; then
        missing+=(runuser) apt+=(util-linux) dnf+=(util-linux)
    fi
    if ! command -v useradd > /dev/null || ! command -v usermod > /dev/null; then
        missing+=(useradd) apt+=(passwd) dnf+=(shadow-utils)
    fi
    # systemctl --user and podman's systemd cgroup manager need a user D-Bus (Debian splits it
    # out; Fedora ships it).
    if command -v apt-get > /dev/null && [ ! -e /usr/lib/systemd/user/dbus.socket ]; then
        missing+=(dbus-user-session) apt+=(dbus-user-session)
    fi
    if [ "${#missing[@]}" = 0 ]; then
        ok "podman $(podman --version | awk '{print $NF}') is installed"
        return
    fi
    if command -v apt-get > /dev/null; then
        run apt-get update -q
        run env DEBIAN_FRONTEND=noninteractive apt-get install -y -q "${apt[@]}"
    elif command -v dnf > /dev/null; then
        run dnf install -y -q "${dnf[@]}"
    else
        die "${missing[*]} missing and neither apt-get nor dnf found: install the podman package (with newuidmap from uidmap or shadow-utils, and sysctl from procps) with your package manager, then re-run"
    fi
}

# The first free start of a 65536-wide range in subordinate id file $1.
free_subid_start() {
    [ -s "$1" ] || {
        echo 100000
        return
    }
    awk -F: 'BEGIN { m = 100000 } NF >= 3 { e = $2 + $3; if (e > m) m = e } END { print m }' \
        "$1" 2>/dev/null || echo 100000
}

create_user() {
    if getent passwd "$SERVICE_USER" > /dev/null; then
        ok "user $SERVICE_USER exists"
    else
        run useradd --system --user-group --create-home --home-dir "/home/$SERVICE_USER" \
            --shell /bin/bash --comment "hoster deployment orchestrator" "$SERVICE_USER"
    fi
    local file option start
    for file in /etc/subuid /etc/subgid; do
        if grep -q "^$SERVICE_USER:" "$file" 2>/dev/null; then
            ok "$SERVICE_USER has a range in $file"
            continue
        fi
        option=--add-subuids
        [ "$file" = /etc/subgid ] && option=--add-subgids
        start=$(free_subid_start "$file")
        run usermod "$option" "$start-$((start + 65535))" "$SERVICE_USER"
    done
}

enable_linger() {
    if [ -e "/var/lib/systemd/linger/$SERVICE_USER" ]; then
        ok "linger is enabled for $SERVICE_USER"
    else
        run loginctl enable-linger "$SERVICE_USER"
    fi
    [ "$DRY_RUN" = 1 ] && return
    # The user manager (and its runtime directory) must be up for the user part.
    local uid
    uid=$(id -u "$SERVICE_USER")
    systemctl start "user@$uid.service"
    local i
    for i in $(seq 1 60); do
        [ -S "/run/user/$uid/systemd/private" ] && return
        [ "$i" = 60 ] || sleep 0.5
    done
    die "the systemd user manager of $SERVICE_USER did not start (systemctl status user@$uid.service)"
}

install_sysctl() {
    local key=net.ipv4.ip_unprivileged_port_start
    local INSTALLED
    install_file "$1/50-hoster.conf" "$SYSCTL_DEST" 0644
    if [ "$(sysctl -n "$key" 2>/dev/null)" = 80 ]; then
        ok "$key is 80"
    else
        run sysctl -q -p "$SYSCTL_DEST"
    fi
}

next_steps() {
    local home=$1 current=$2 before=$3
    cat << EOF

hoster $current is installed in $home/.local/lib/hoster. The script never starts the service;
finish the setup as the hoster user, in a session with its own user bus:

  sudo machinectl shell $SERVICE_USER@        # or: ssh $SERVICE_USER@<host>

1. The admin's GitHub handle (ADR-0021), in a drop-in that unit updates leave alone:
     systemctl --user edit hoster
   and enter:
     [Service]
     Environment=HOSTER_ADMIN_GITHUB=<your-github-handle>
2. A GitHub OAuth App with the callback https://<admin_host>/auth/github/callback. Save its
   client id and a client secret as 0600 files in ~/.config/hoster and add to the drop-in:
     LoadCredential=github_client_id:%h/.config/hoster/github_client_id
     LoadCredential=github_client_secret:%h/.config/hoster/github_client_secret
3. In ~/.config/hoster/hoster.toml set admin_host (DNS pointing here) and the ACME contact
   ([acme] contact).
4. Check, then start:
     HOSTER_ADMIN_GITHUB=<your-github-handle> ~/.local/lib/hoster/current/hoster doctor
     systemctl --user enable --now hoster

See https://github.com/LHelge/hoster/blob/main/docs/install.md
EOF
    if [ -n "$before" ] && [ "$before" != "$current" ]; then
        cat << EOF

Updated from $before to $current. A running hoster keeps running $before until it restarts:
  systemctl --user restart hoster
(\`hoster update\` does the same from the admin UI or CLI and snapshots the database first.)
EOF
    fi
}

main() {
    local DRY_RUN=0 RELEASE_TAG="" EXPLICIT_VERSION=0 TARGET RELEASE_BASE
    local RELEASES_URL=${HOSTER_INSTALL_RELEASES_URL:-$DEFAULT_RELEASES_URL}
    RELEASES_URL=${RELEASES_URL%/}
    while [ "$#" -gt 0 ]; do
        case $1 in
            --version)
                [ "$#" -ge 2 ] || die "--version needs a value such as v0.2.0"
                RELEASE_TAG=v${2#v}
                shift 2
                ;;
            --version=*)
                RELEASE_TAG=${1#--version=}
                RELEASE_TAG=v${RELEASE_TAG#v}
                shift
                ;;
            --dry-run)
                DRY_RUN=1
                shift
                ;;
            -h | --help)
                usage
                return 0
                ;;
            *)
                usage >&2
                die "unknown argument: $1"
                ;;
        esac
    done
    # The script can arrive on stdin (curl | bash); nothing it runs may read the rest of it.
    exec < /dev/null

    check_host
    choose_release
    [ "$DRY_RUN" = 1 ] && say "dry run: nothing is changed"

    # The staging directory is root's (mktemp makes it 0700). root/ holds what root installs
    # (the sysctl file) and `changes` root's log, out of the hoster user's reach. user/ is the
    # hoster user's: it downloads and verifies the files it installs there.
    local staging
    staging=$(mktemp -d "${TMPDIR:-/tmp}/hoster-install.XXXXXX")
    # shellcheck disable=SC2064 # expand now: the local is gone when the trap runs
    trap "rm -rf $(printf '%q' "$staging")" EXIT
    local CHANGES=$staging/changes USER_CHANGES=""
    : > "$CHANGES"
    install -d -m 0700 "$staging/root"
    fetch_release "$staging/root" 50-hoster.conf

    install_packages
    create_user

    local user_exists=0 home
    local -a user_files=("hoster-$TARGET" hoster.service hoster.example.toml)
    if getent passwd "$SERVICE_USER" > /dev/null; then
        user_exists=1
        home=$(getent passwd "$SERVICE_USER" | cut -d: -f6)
        local -a own
        own=(-o "$(id -u "$SERVICE_USER")" -g "$(id -g "$SERVICE_USER")")
        chmod 0711 "$staging"
        install -d "${own[@]}" -m 0700 "$staging/user"
        USER_CHANGES=$staging/user/changes
        install "${own[@]}" -m 0600 /dev/null "$USER_CHANGES"
        # The downloads run as hoster, with no root rights.
        as_hoster fetch_release "$staging/user" "${user_files[@]}"
    else
        # Dry run only: the user does not exist yet.
        home=/home/$SERVICE_USER
        install -d -m 0700 "$staging/user"
        fetch_release "$staging/user" "${user_files[@]}"
    fi
    enable_linger
    install_sysctl "$staging/root"

    local before=""
    before=$(current_version "$home/.local/lib/hoster" 2>/dev/null || true)
    if [ "$user_exists" = 1 ]; then
        as_hoster hoster_user "$staging/user"
    else
        # Dry run only: print what the user part would do in a fresh home.
        (HOME=$home NO_USER=1 hoster_user "$staging/user")
    fi
    local current
    current=$(current_version "$home/.local/lib/hoster" 2>/dev/null || true)

    local changes user_changes=0
    changes=$(grep -c . "$CHANGES" || true)
    if [ -n "$USER_CHANGES" ]; then
        # Read as hoster: the file is hoster's and could have been swapped for a symlink.
        user_changes=$(runuser -u "$SERVICE_USER" -- grep -c . "$USER_CHANGES" || true)
    fi
    changes=$((changes + ${user_changes:-0}))
    if [ "$DRY_RUN" = 1 ]; then
        say "dry run: $changes change(s) would be made"
        return
    fi
    if [ "$changes" = 0 ]; then
        say "nothing to change: hoster $current is installed and up to date"
    else
        say "made $changes change(s)"
    fi
    next_steps "$home" "${current:-$RELEASE_TAG}" "$before"
}

main "$@"
}
hoster_install "$@"
