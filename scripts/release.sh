#!/usr/bin/env bash
# Cut a hoster release (ADR-0052 and its 2026-11-08 correction). main is branch-protected, so a
# release takes two steps:
#
#   scripts/release.sh prepare X.Y.Z [--trailer 'Key: value']...
#       From a clean main that matches origin/main: create release/vX.Y.Z, bump the version in
#       Cargo.toml and Cargo.lock, commit `chore(release): vX.Y.Z`, push the branch and open a PR
#       for it. `--trailer` adds a trailer to the commit message (repeatable), e.g. the
#       Co-Authored-By line an agent's commits carry. The release notes are rendered by the
#       release workflow from cliff.toml when the tag is pushed, so nothing here needs git-cliff.
#
#   scripts/release.sh tag X.Y.Z [--yes]
#       After that PR is rebase-merged and local main is pulled: create the annotated tag vX.Y.Z on
#       main's tip (the merged `chore(release): vX.Y.Z` commit) and push only the tag, which runs
#       .github/workflows/release.yml. Asks for confirmation unless --yes is given. The tag is made
#       after merging because GitHub's rebase merge rewrites the PR's commit SHAs.
#
# X.Y.Z may be given with or without the leading `v` and may carry a SemVer pre-release suffix
# (0.2.0-rc.1), which the release workflow publishes as a GitHub pre-release. It must be greater
# than the version in Cargo.toml, except that the version already in Cargo.toml may be released
# once if it was never tagged (the first release, v0.1.0); its release commit is then empty.
#
# Needs git, cargo and gh (authenticated).
set -euo pipefail

readonly remote=origin
readonly branch=main

die() {
    echo "error: $*" >&2
    exit 1
}

usage() {
    sed -n '2,/^set -euo/{/^set -euo/d;s/^# \{0,1\}//;p}' "$0" >&2
    exit 2
}

# ---- SemVer ---------------------------------------------------------------------------------

semver_re='^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(-((0|[1-9][0-9]*|[0-9]*[A-Za-z-][0-9A-Za-z-]*)(\.(0|[1-9][0-9]*|[0-9]*[A-Za-z-][0-9A-Za-z-]*))*))?$'

# Prints the version with any leading `v` removed, or fails if it isn't SemVer (without build
# metadata, which tags can't usefully carry).
parse_version() {
    local v=${1#v}
    [[ $v =~ $semver_re ]] || die "'$1' is not a SemVer version (X.Y.Z or X.Y.Z-pre)"
    printf '%s\n' "$v"
}

# Compares two pre-release identifiers per SemVer 2.0.0 §11: prints -1, 0 or 1.
cmp_ident() {
    # SemVer orders alphanumeric identifiers by ASCII, not by the locale's collation.
    local LC_ALL=C a=$1 b=$2
    if [[ $a =~ ^[0-9]+$ && $b =~ ^[0-9]+$ ]]; then
        if ((10#$a < 10#$b)); then echo -1; elif ((10#$a > 10#$b)); then echo 1; else echo 0; fi
    elif [[ $a =~ ^[0-9]+$ ]]; then
        echo -1
    elif [[ $b =~ ^[0-9]+$ ]]; then
        echo 1
    elif [[ $a < $b ]]; then
        echo -1
    elif [[ $a > $b ]]; then
        echo 1
    else
        echo 0
    fi
}

# Compares two SemVer versions by precedence: prints -1, 0 or 1.
semver_cmp() {
    local a=$1 b=$2 a_core b_core a_pre="" b_pre="" i c
    a_core=${a%%-*} b_core=${b%%-*}
    [[ $a == *-* ]] && a_pre=${a#*-}
    [[ $b == *-* ]] && b_pre=${b#*-}
    local -a an bn
    IFS=. read -r -a an <<<"$a_core"
    IFS=. read -r -a bn <<<"$b_core"
    for i in 0 1 2; do
        if ((an[i] < bn[i])); then echo -1; return; fi
        if ((an[i] > bn[i])); then echo 1; return; fi
    done
    # A version without a pre-release ranks above the same version with one.
    if [[ -z $a_pre && -z $b_pre ]]; then echo 0; return; fi
    if [[ -z $a_pre ]]; then echo 1; return; fi
    if [[ -z $b_pre ]]; then echo -1; return; fi
    local -a ap bp
    IFS=. read -r -a ap <<<"$a_pre"
    IFS=. read -r -a bp <<<"$b_pre"
    for ((i = 0; i < ${#ap[@]} && i < ${#bp[@]}; i++)); do
        c=$(cmp_ident "${ap[i]}" "${bp[i]}")
        if [[ $c != 0 ]]; then echo "$c"; return; fi
    done
    if ((${#ap[@]} < ${#bp[@]})); then echo -1; elif ((${#ap[@]} > ${#bp[@]})); then echo 1; else echo 0; fi
}

# ---- Repository checks ----------------------------------------------------------------------

# The `version` of [package] in a Cargo.toml read from stdin.
package_version() {
    awk '
        /^\[/ { in_pkg = ($0 == "[package]") }
        in_pkg && /^version[ \t]*=/ {
            sub(/^version[ \t]*=[ \t]*"/, ""); sub(/".*$/, ""); print; exit
        }'
}

require_tools() {
    local t
    for t in "$@"; do
        command -v "$t" >/dev/null || die "$t is not installed"
    done
}

# Fetches main and the tags from the remote and fails unless local main is exactly origin/main.
require_main_in_sync() {
    git fetch --quiet --tags "$remote" "$branch" || die "could not fetch $branch from $remote"
    local local_sha remote_sha
    local_sha=$(git rev-parse --verify --quiet "refs/heads/$branch") || die "there is no local $branch branch"
    remote_sha=$(git rev-parse --verify --quiet "refs/remotes/$remote/$branch") ||
        die "there is no $remote/$branch"
    [[ $local_sha == "$remote_sha" ]] ||
        die "local $branch ($local_sha) does not match $remote/$branch ($remote_sha); pull or push first"
}

require_untagged() {
    local tag=$1
    ! git rev-parse --verify --quiet "refs/tags/$tag" >/dev/null || die "tag $tag already exists"
    [[ -z $(git ls-remote --tags "$remote" "refs/tags/$tag") ]] || die "tag $tag already exists on $remote"
}

# Undoes a `prepare` that failed before anything was pushed: back on main, release branch gone.
# After the push only the remote branch is left, and the message says how to finish or drop it.
# Globals, so the EXIT trap still sees them after cmd_prepare returns.
release_body="" undo_branch="" pushed_branch=""
on_exit() {
    local status=$?
    [[ -z $release_body ]] || rm -f "$release_body"
    ((status != 0)) || return 0
    if [[ -n $undo_branch ]]; then
        rm -f Cargo.toml.release-tmp
        if git switch --quiet --force "$branch" && git branch --quiet -D "$undo_branch"; then
            echo "rolled back: back on $branch, local branch $undo_branch deleted" >&2
        else
            echo "could not roll back; switch to $branch and delete $undo_branch by hand" >&2
        fi
    elif [[ -n $pushed_branch ]]; then
        echo "$pushed_branch is pushed but has no PR; open it with" \
            "'gh pr create --base $branch --head $pushed_branch', or drop it with" \
            "'git push $remote --delete $pushed_branch && git branch -D $pushed_branch'" >&2
    fi
}
trap on_exit EXIT

# ---- Commands -------------------------------------------------------------------------------

cmd_prepare() {
    local version="" arg
    local -a trailers=()
    while (($#)); do
        arg=$1
        shift
        case $arg in
            --trailer)
                (($#)) || die "--trailer needs a value"
                trailers+=(--trailer "$1")
                shift
                ;;
            -h | --help) usage ;;
            -*) die "unknown option $arg" ;;
            *)
                [[ -z $version ]] || die "only one version may be given"
                version=$(parse_version "$arg")
                ;;
        esac
    done
    [[ -n $version ]] || usage
    local tag="v$version" rel_branch="release/v$version"

    require_tools git cargo gh
    gh auth status >/dev/null 2>&1 || die "gh is not logged in; run 'gh auth login' first"
    [[ $(git symbolic-ref --quiet --short HEAD || true) == "$branch" ]] || die "not on $branch"
    [[ -z $(git status --porcelain --untracked-files=no) ]] || die "the working tree has uncommitted changes"
    require_main_in_sync

    local current cmp
    current=$(package_version <Cargo.toml)
    [[ -n $current ]] || die "no [package] version in Cargo.toml"
    [[ $current =~ $semver_re ]] || die "the version '$current' in Cargo.toml is not SemVer"
    cmp=$(semver_cmp "$version" "$current")
    if [[ $cmp == 0 ]]; then
        # Only the very first release: Cargo.toml already says it and it was never tagged.
        echo "Cargo.toml already says $current; releasing it as the first release of that version"
    elif [[ $cmp != 1 ]]; then
        die "$version is not greater than the current version $current"
    fi
    require_untagged "$tag"
    ! git rev-parse --verify --quiet "refs/heads/$rel_branch" >/dev/null || die "branch $rel_branch already exists"
    [[ -z $(git ls-remote --heads "$remote" "refs/heads/$rel_branch") ]] ||
        die "branch $rel_branch already exists on $remote"

    git switch --quiet -c "$rel_branch"
    undo_branch=$rel_branch

    if [[ $cmp == 1 ]]; then
        # Only the [package] version line changes; dependency versions are left alone.
        awk -v v="$version" '
            /^\[/ { in_pkg = ($0 == "[package]") }
            in_pkg && !done && /^version[ \t]*=/ { print "version = \"" v "\""; done = 1; next }
            { print }' Cargo.toml >Cargo.toml.release-tmp
        mv Cargo.toml.release-tmp Cargo.toml
        # Rewrites hoster's own entry in Cargo.lock without touching any dependency.
        cargo update --workspace --quiet
        [[ $(package_version <Cargo.toml) == "$version" ]] || die "failed to bump Cargo.toml"
        # Exactly one line (hoster's own version) in each of the two files, nothing else.
        local changed
        changed=$(git diff --numstat | sort -k3 | tr '\t\n' '  ')
        [[ $changed == "1 1 Cargo.lock 1 1 Cargo.toml " ]] ||
            die "the version bump changed more than the version lines: $changed"
        git add Cargo.toml Cargo.lock
    fi
    git commit --quiet --allow-empty -m "chore(release): $tag" "${trailers[@]}"

    release_body=$(mktemp)
    {
        echo "Release $tag."
        echo
        echo "Rebase-merge this PR, then on an up-to-date \`$branch\` run \`scripts/release.sh tag $version\`"
        echo "to tag the merged commit and publish the release. The release workflow renders the"
        echo "release notes from cliff.toml when the tag is pushed."
    } >"$release_body"

    # From the push on, a failure leaves the branch on the remote; on_exit says what to do.
    undo_branch=""
    pushed_branch=$rel_branch
    git push --quiet --set-upstream "$remote" "$rel_branch"
    gh pr create --base "$branch" --head "$rel_branch" --title "chore(release): $tag" --body-file "$release_body"
    pushed_branch=""
    git switch --quiet "$branch"
    echo "opened the release PR for $tag; after it is merged: git pull --ff-only && scripts/release.sh tag $version"
}

cmd_tag() {
    local version="" yes=0 arg
    while (($#)); do
        arg=$1
        shift
        case $arg in
            -y | --yes) yes=1 ;;
            -h | --help) usage ;;
            -*) die "unknown option $arg" ;;
            *)
                [[ -z $version ]] || die "only one version may be given"
                version=$(parse_version "$arg")
                ;;
        esac
    done
    [[ -n $version ]] || usage
    local tag="v$version"

    require_tools git
    require_main_in_sync

    local sha subject toml_version
    sha=$(git rev-parse "refs/heads/$branch")
    subject=$(git log -1 --format=%s "$sha")
    [[ $subject == "chore(release): $tag" ]] ||
        die "the tip of $branch is '$subject', not 'chore(release): $tag'; is the release PR merged and pulled?"
    toml_version=$(git show "$sha:Cargo.toml" | package_version)
    [[ $toml_version == "$version" ]] || die "Cargo.toml on $branch says $toml_version, not $version"
    require_untagged "$tag"

    echo "about to tag $(git log -1 --format='%h %s' "$sha") as $tag and push the tag to $remote"
    if ((!yes)); then
        local answer
        [[ -t 0 ]] || die "not a terminal; pass --yes to tag without confirming"
        read -r -p "continue? [y/N] " answer
        [[ $answer == [yY] || $answer == [yY][eE][sS] ]] || die "aborted"
    fi

    git tag -a "$tag" -m "hoster $tag" "$sha"
    git push --quiet "$remote" "refs/tags/$tag"
    echo "pushed $tag; the release workflow publishes it"
}

main() {
    (($#)) || usage
    local cmd=$1
    shift
    # Run from the repository root so Cargo.toml resolves.
    cd "$(git rev-parse --show-toplevel)"
    case $cmd in
        prepare) cmd_prepare "$@" ;;
        tag) cmd_tag "$@" ;;
        -h | --help | help) usage ;;
        *) die "unknown command '$cmd' (expected prepare or tag)" ;;
    esac
}

main "$@"
