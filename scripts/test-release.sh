#!/usr/bin/env bash
# Tests scripts/release.sh in a throwaway repository with a bare local remote. `gh` and
# `git-cliff` are stubbed, so nothing reaches GitHub and the test runs offline.
#
# Usage: scripts/test-release.sh
#
# Used by CI (.github/workflows/ci.yml).
set -euo pipefail

script=$(cd "$(dirname "$0")" && pwd)/release.sh
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

failures=0
pass() { echo "ok: $*"; }
fail() {
    echo "FAIL: $*" >&2
    failures=$((failures + 1))
}
# check WHAT ACTUAL EXPECTED: passes when the two strings are equal.
check() {
    if [[ $2 == "$3" ]]; then pass "$1"; else fail "$1: got '$2', expected '$3'"; fi
}
# check_grep WHAT PATTERN FILE: passes when FILE has a line matching the fixed string PATTERN.
check_grep() {
    if grep -qF -- "$2" "$3" 2>/dev/null; then pass "$1"; else fail "$1: '$2' not in $(cat "$3" 2>&1)"; fi
}

# Git without the user's or the system's configuration (signing, hooks, default branch).
export GIT_CONFIG_NOSYSTEM=1
export GIT_CONFIG_GLOBAL="$tmp/gitconfig"
cat >"$GIT_CONFIG_GLOBAL" <<'EOF'
[user]
    name = Release Test
    email = release-test@example.invalid
[init]
    defaultBranch = main
[advice]
    detachedHead = false
EOF

# Stubs: gh records its arguments and the PR body; git-cliff prints fixed notes. STUB_FAIL makes
# `gh auth`, `gh pr` or git-cliff fail, for the failure paths.
mkdir "$tmp/bin"
cat >"$tmp/bin/gh" <<'EOF'
#!/usr/bin/env bash
[[ ${STUB_FAIL:-} != "gh $1" ]] || { echo "gh $1: stubbed failure" >&2; exit 1; }
[[ $1 != auth ]] || exit 0
printf '%s\n' "$*" >>"$STUB_DIR/gh.args"
while (($#)); do
    if [[ $1 == --body-file ]]; then cp "$2" "$STUB_DIR/pr-body.md"; fi
    shift
done
echo "https://github.invalid/pr/1"
EOF
cat >"$tmp/bin/git-cliff" <<'EOF'
#!/usr/bin/env bash
[[ ${STUB_FAIL:-} != git-cliff ]] || { echo "git-cliff: stubbed failure" >&2; exit 1; }
printf '%s\n' "$*" >>"$STUB_DIR/cliff.args"
printf '### Features\n\n- Stubbed release notes\n'
EOF
chmod +x "$tmp/bin/gh" "$tmp/bin/git-cliff"
export PATH="$tmp/bin:$PATH" STUB_DIR="$tmp"

# The remote, seeded with a minimal hoster crate at 0.1.0 that was never released.
git init --quiet --bare "$tmp/remote.git"
mkdir -p "$tmp/seed/src" "$tmp/seed/scripts"
cat >"$tmp/seed/Cargo.toml" <<'EOF'
[package]
name = "hoster"
version = "0.1.0"
edition = "2024"
publish = false

[dependencies]
EOF
echo 'fn main() {}' >"$tmp/seed/src/main.rs"
echo '/target/' >"$tmp/seed/.gitignore"
touch "$tmp/seed/cliff.toml"
cp "$script" "$tmp/seed/scripts/release.sh"
(
    cd "$tmp/seed"
    cargo generate-lockfile --offline --quiet
    git init --quiet
    git add -A
    git commit --quiet -m "feat: initial"
    git remote add origin "$tmp/remote.git"
    git push --quiet origin main
)

git clone --quiet "$tmp/remote.git" "$tmp/work"
git clone --quiet "$tmp/remote.git" "$tmp/github"
cd "$tmp/work"

release() { scripts/release.sh "$@" </dev/null; }

# Runs release.sh and expects it to fail with a message matching $1.
expect_refusal() {
    local what=$1 pattern=$2
    shift 2
    local out
    if out=$(release "$@" 2>&1); then
        fail "$what: release.sh $* succeeded"
    elif grep -Eq -- "$pattern" <<<"$out"; then
        pass "$what"
    else
        fail "$what: expected /$pattern/, got: $out"
    fi
}

remote_sha() { git --git-dir="$tmp/remote.git" rev-parse --verify --quiet "$1"; }
version_at() { git show "$1:Cargo.toml" | sed -n 's/^version = "\(.*\)"$/\1/p' | head -n1; }

# GitHub's rebase merge of a release PR: the same change as a new commit (new SHA) on main.
merge_pr() {
    (
        cd "$tmp/github"
        git fetch --quiet origin
        git switch --quiet main
        git reset --quiet --hard origin/main
        # GitHub commits it as itself, which is enough to give it a new SHA.
        GIT_COMMITTER_NAME=GitHub GIT_COMMITTER_EMAIL=noreply@github.invalid \
            git cherry-pick --quiet --allow-empty "origin/$1" >/dev/null
        git push --quiet origin main
    )
}

# ---- prepare: refusals ----------------------------------------------------------------------

expect_refusal "prepare rejects a non-SemVer version" "not a SemVer version" prepare 1.2
expect_refusal "prepare rejects build metadata" "not a SemVer version" prepare 1.2.3+build
expect_refusal "prepare rejects a leading zero" "not a SemVer version" prepare 01.2.3
expect_refusal "prepare rejects a lower version" "not greater than the current version 0.1.0" prepare 0.0.9
expect_refusal "prepare rejects a pre-release of the current version" "not greater" prepare 0.1.0-rc.1
expect_refusal "prepare needs a version" "release.sh prepare X.Y.Z" prepare

git switch --quiet -c topic
expect_refusal "prepare refuses off main" "not on main" prepare 0.2.0
git switch --quiet main

echo "// dirty" >>src/main.rs
expect_refusal "prepare refuses a dirty tree" "uncommitted changes" prepare 0.2.0
git checkout --quiet -- src/main.rs

git commit --quiet --allow-empty -m "feat: local only"
expect_refusal "prepare refuses main ahead of origin/main" "does not match origin/main" prepare 0.2.0
git reset --quiet --hard origin/main

STUB_FAIL="gh auth" expect_refusal "prepare refuses when gh is not logged in" "gh is not logged in" prepare 0.2.0

# ---- prepare: failure paths -----------------------------------------------------------------

# Fails after the bump is committed but before the push: everything local is undone.
STUB_FAIL=git-cliff expect_refusal "prepare reports a git-cliff failure" "rolled back" prepare 0.2.0
check "rollback returns to main" "$(git symbolic-ref --short HEAD)" main
check "rollback leaves main clean" "$(git status --porcelain --untracked-files=no)" ""
check "rollback leaves main at origin/main" "$(git rev-parse main)" "$(git rev-parse origin/main)"
check "rollback deletes the local release branch" \
    "$(git rev-parse --verify --quiet refs/heads/release/v0.2.0 || echo none)" none
check "rollback pushes nothing" "$(remote_sha refs/heads/release/v0.2.0 || echo none)" none

# Fails after the push: the remote branch stays and the message says how to finish or drop it.
STUB_FAIL="gh pr" expect_refusal "prepare explains a failed gh pr create" \
    "release/v0.2.0 is pushed but has no PR" prepare 0.2.0
check "a failed PR leaves the branch pushed" "$(remote_sha refs/heads/release/v0.2.0 >/dev/null && echo pushed)" pushed
git switch --quiet main
git branch --quiet -D release/v0.2.0
git push --quiet origin --delete release/v0.2.0
rm -f "$tmp/gh.args" "$tmp/pr-body.md" "$tmp/cliff.args"

# ---- prepare: the first release keeps the version already in Cargo.toml ---------------------

if out=$(release prepare v0.1.0 2>&1); then
    pass "prepare 0.1.0 (first release, version unchanged)"
    check "first release commit pushed" "$(git log -1 --format=%s origin/release/v0.1.0 2>&1)" "chore(release): v0.1.0"
    check "first release commit is empty" "$(git diff --name-only main origin/release/v0.1.0 2>&1)" ""
else
    fail "prepare 0.1.0: $out"
fi
# Drop that release again; the rest of the test releases 0.2.0.
git branch --quiet -D release/v0.1.0
git push --quiet origin --delete release/v0.1.0
rm -f "$tmp/gh.args" "$tmp/pr-body.md" "$tmp/cliff.args"

# ---- prepare: success -----------------------------------------------------------------------

main_before=$(remote_sha refs/heads/main)
if out=$(release prepare 0.2.0 --trailer "Co-Authored-By: Someone <someone@example.invalid>" 2>&1); then
    pass "prepare 0.2.0"
else
    fail "prepare 0.2.0: $out"
fi
rel=refs/heads/release/v0.2.0
check "prepare returns to main" "$(git symbolic-ref --short HEAD)" main
check "prepare does not push main" "$(remote_sha refs/heads/main)" "$main_before"
if remote_sha "$rel" >/dev/null; then
    pass "release branch pushed"
    check "release commit subject" "$(git log -1 --format=%s origin/release/v0.2.0)" "chore(release): v0.2.0"
    check "release commit carries the trailer" \
        "$(git log -1 --format='%(trailers:key=Co-Authored-By,valueonly)' origin/release/v0.2.0 | head -n1)" \
        "Someone <someone@example.invalid>"
    check "Cargo.toml bumped" "$(version_at origin/release/v0.2.0)" 0.2.0
    check "Cargo.lock bumped" \
        "$(git show origin/release/v0.2.0:Cargo.lock | grep -A1 '^name = "hoster"$' | tail -n1)" 'version = "0.2.0"'
    check "only Cargo.toml and Cargo.lock change" \
        "$(git diff --name-only main origin/release/v0.2.0 | sort | tr '\n' ' ')" "Cargo.lock Cargo.toml "
else
    fail "release branch not pushed"
fi
check_grep "gh pr create called" \
    "pr create --base main --head release/v0.2.0 --title chore(release): v0.2.0" "$tmp/gh.args"
check_grep "PR body is the git-cliff preview" "Stubbed release notes" "$tmp/pr-body.md"
check_grep "git-cliff previews the unreleased commits" "--unreleased --tag v0.2.0" "$tmp/cliff.args"

expect_refusal "prepare refuses an existing release branch" "already exists" prepare 0.2.0

# ---- tag ------------------------------------------------------------------------------------

expect_refusal "tag refuses before the PR is merged" "not 'chore\(release\): v0.2.0'" tag 0.2.0 --yes
merge_pr release/v0.2.0
expect_refusal "tag refuses a main behind origin/main" "does not match origin/main" tag 0.2.0 --yes
git pull --quiet --ff-only
expect_refusal "tag refuses another version" "not 'chore\(release\): v0.3.0'" tag 0.3.0 --yes
expect_refusal "tag asks for confirmation" "pass --yes" tag 0.2.0

main_before=$(remote_sha refs/heads/main)
branches_before=$(git --git-dir="$tmp/remote.git" for-each-ref --format='%(refname) %(objectname)' refs/heads)
if out=$(release tag 0.2.0 --yes 2>&1); then
    pass "tag 0.2.0"
else
    fail "tag 0.2.0: $out"
fi
if tag_obj=$(remote_sha refs/tags/v0.2.0); then
    check "tag is annotated" "$(git --git-dir="$tmp/remote.git" cat-file -t "$tag_obj")" tag
    tagged=$(remote_sha 'refs/tags/v0.2.0^{commit}')
    check "tag points at the merged commit on main" "$tagged" "$main_before"
    if [[ $tagged != $(remote_sha "$rel") ]]; then
        pass "tag is not on the PR branch commit"
    else
        fail "tag is on the PR branch commit"
    fi
else
    fail "tag not pushed"
fi
check "tag pushes only the tag" \
    "$(git --git-dir="$tmp/remote.git" for-each-ref --format='%(refname) %(objectname)' refs/heads)" "$branches_before"

expect_refusal "tag refuses an existing tag" "already exists" tag 0.2.0 --yes
expect_refusal "prepare rejects a pre-release below the released version" "not greater than the current version 0.2.0" prepare 0.2.0-rc.1
expect_refusal "prepare rejects the released version" "already exists" prepare 0.2.0

if ((failures)); then
    echo "$failures check(s) failed" >&2
    exit 1
fi
echo "all release.sh checks passed"
