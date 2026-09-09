#!/usr/bin/env bash
# Build, deploy and *verify* an InPhase host on the Windows target.
#
# This exists because the manual procedure kept producing hosts nobody could
# identify. On 2026-09-07 the running host was serving `assets/main-B_ZS04pK.js`
# while the build directory held `main-CvBnt7N9.js` — the exe had been compiled
# at 01:03 and the web bundle rebuilt at 10:57 and never compiled in. The host
# was serving JavaScript that existed nowhere on disk, and no step in the manual
# flow could have caught it.
#
# So the contract here is: **a ship that cannot prove what it shipped fails.**
# Every stage verifies, none can be skipped, and the last stage asks the running
# host what build it is and refuses to call the deploy done until it answers
# with the id this script built. There is deliberately no --skip-tests and no
# --force.
#
# Usage:
#   tools/ship.sh                 # ship HEAD (requires a clean tree)
#   tools/ship.sh --dirty         # ship the working tree, id gets -dirty.<digest>
#   tools/ship.sh --no-install    # build + verify remotely, leave the live host alone
#   tools/ship.sh --host user@ip  # override the target
#
# Exit codes: 1 usage/preflight, 2 sync, 3 remote build, 4 verification.

set -euo pipefail

# NOTE: `grep -c` exits 1 on zero matches, which under `set -e` silently kills a
# pipeline mid-ship (this bit us repeatedly). Every grep below is either the
# subject of an `if`, or terminated with `|| true`. Do not "tidy" that away.

readonly REMOTE_DEFAULT="sambe@100.127.176.18"
# The single canonical build directory. C:\Users\sambe\{inphase,InPhase-build}
# are abandoned checkouts from earlier sessions (InPhase-build sits on 195a172,
# ~30 commits back) and are NOT to be built from — a deploy out of one of those
# is how a stale exe shipped twice. This one is canonical because the boot task
# already launches from it and its cargo/npm caches are warm.
readonly REMOTE_DIR='C:\Users\sambe\InPhase-wt'
readonly REMOTE_TASK="InPhaseWTStart"
readonly INSTALL_EXE='C:\Program Files\InPhase\InPhaseHost.exe'
readonly STATUS_URL="http://127.0.0.1:47800/api/v1/status"

REMOTE="$REMOTE_DEFAULT"
ALLOW_DIRTY=0
INSTALL=1

while [ $# -gt 0 ]; do
  case "$1" in
    --dirty)      ALLOW_DIRTY=1 ;;
    --no-install) INSTALL=0 ;;
    --host)       shift; REMOTE="${1:?--host needs an argument}" ;;
    -h|--help)    sed -n '2,28p' "$0"; exit 0 ;;
    *)            echo "ship: unknown argument '$1' (try --help)" >&2; exit 1 ;;
  esac
  shift
done

cd "$(dirname "$0")/.."
ROOT=$(pwd)
# Tracked + untracked-not-ignored files that actually exist on disk. `git
# ls-files` keeps listing a tracked file after it is deleted, which breaks both
# the content digest and the tarball; a refactor that moves a file would
# otherwise fail the ship for no reason. Pass -z for NUL-separated output.
ship_files() {
  if [ "${1:-}" = "-z" ]; then
    git ls-files -co --exclude-standard -z | while IFS= read -r -d '' f; do
      [ -f "$f" ] && printf '%s\0' "$f"
    done
  else
    git ls-files -co --exclude-standard | while IFS= read -r f; do
      [ -f "$f" ] && printf '%s\n' "$f"
    done
  fi
}

say() { printf '\n\033[1;36m== %s\033[0m\n' "$*"; }
ok()  { printf '   \033[32mok\033[0m %s\n' "$*"; }
die() { printf '\n\033[1;31mFAIL\033[0m %s\n' "$2" >&2; exit "$1"; }

# --- 1. build id ------------------------------------------------------------
# Identifies the source this binary came from. A clean tree ships its commit; a
# dirty tree ships the commit plus a digest over the exact file contents sent,
# so even an uncommitted experiment is distinguishable from every other build.
say "build id"
SHA=$(git rev-parse --short=12 HEAD)
if [ -z "$(git status --porcelain)" ]; then
  BUILD_ID="$SHA"
else
  [ "$ALLOW_DIRTY" -eq 1 ] || die 1 "working tree is dirty — commit, or ship with --dirty:
$(git status --short | sed 's/^/     /')"
  # Content digest over every file we are about to ship: deterministic, unlike
  # hashing the tarball (which carries mtimes). Deleted-but-tracked paths are
  # filtered out - `git ls-files` still lists a file removed from the working
  # tree, and hashing it aborts the ship.
  DIGEST=$(ship_files -z | sort -z | xargs -0r sha256sum | sha256sum | cut -c1-8)
  BUILD_ID="$SHA-dirty.$DIGEST"
fi
ok "$BUILD_ID"

# --- 2. local preflight -----------------------------------------------------
# The host crate is Windows-only and cannot be compiled here, so what we can
# check locally, we check locally — a wire-vector regression should never reach
# the remote build.
say "local preflight"
cargo test -q -p inphase-protocol >/dev/null 2>&1 || die 1 "protocol tests failed (run: cargo test -p inphase-protocol)"
ok "protocol tests"
# The host *lib* does build off-Windows (only the GStreamer pipeline is gated),
# so its pure logic - control laws, health assessment - is testable right here
# rather than only on the target.
cargo test -q -p inphase-host --lib >/dev/null 2>&1 || die 1 "host lib tests failed (run: cargo test -p inphase-host --lib)"
ok "host lib tests"
( cd web && npx tsc --noEmit ) || die 1 "web typecheck failed"
ok "web typecheck"
( cd web && npm test --silent >/dev/null 2>&1 ) || die 1 "web tests failed (run: npm --prefix web test)"
ok "web tests"

# --- 3. sync ----------------------------------------------------------------
# Ship tracked + untracked-not-ignored files, which excludes target/, node_modules
# and web/dist. The remote rebuilds the bundle rather than receiving one, so a
# stale local dist can never be what gets embedded.
say "sync to $REMOTE:$REMOTE_DIR"
TARBALL=$(mktemp -t inphase-ship-XXXXXX.tgz)
trap 'rm -f "$TARBALL"' EXIT
ship_files -z | tar --null -T - -czf "$TARBALL" || die 2 "could not build source tarball"
ok "$(du -h "$TARBALL" | cut -f1) tarball"

# PowerShell 5.1 reads a BOM-less .ps1 as ANSI, so a stray UTF-8 character
# (an em-dash in a comment is enough) becomes mojibake mid-string and the whole
# file fails to parse with errors pointing at unrelated lines. Keep it ASCII.
if grep -qP '[^\x00-\x7F]' "$ROOT/tools/remote-build.ps1"; then
  die 1 "tools/remote-build.ps1 contains non-ASCII, which PowerShell 5.1 will mis-decode:
$(grep -nP '[^\x00-\x7F]' "$ROOT/tools/remote-build.ps1" | sed 's/^/     /')"
fi

scp -q "$TARBALL" "$REMOTE:C:/Users/sambe/inphase-ship.tgz" || die 2 "scp of source tarball failed"
scp -q "$ROOT/tools/remote-build.ps1" "$REMOTE:C:/Users/sambe/inphase-remote-build.ps1" || die 2 "scp of remote build script failed"
ok "uploaded"

# --- 4/5/6. remote build, verify, install, restart, verify ------------------
# All of it in one PowerShell invocation from a *file*: passing a multi-step
# script through ssh + cmd + powershell quoting is how paths got eaten before.
say "remote build + verify"
set +e
ssh -o BatchMode=yes "$REMOTE" \
  "powershell -NoProfile -ExecutionPolicy Bypass -File C:\\Users\\sambe\\inphase-remote-build.ps1 -BuildId $BUILD_ID -Install $INSTALL"
RC=$?
set -e
case "$RC" in
  0) ;;
  3) die 3 "remote build failed (see output above)" ;;
  4) die 4 "remote verification failed — the deploy did NOT take (see output above)" ;;
  *) die 3 "remote step exited $RC" ;;
esac

say "shipped"
ok "build id  $BUILD_ID"
if [ "$INSTALL" -eq 1 ]; then
  ok "the running host reports this id; any page open against it will reload itself"
else
  ok "--no-install: built and verified remotely, live host untouched"
fi
