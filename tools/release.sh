#!/usr/bin/env bash
# Make a release of FREEHOLD's drivers (FREEHOLD's DECISIONS.md, D-215).
#
#   tools/release.sh N            build and package from a clean checkout
#                                 into releases/vN/, to be committed
#   tools/release.sh N --publish  publish releases/vN/ (built before, or
#                                 now) as GitHub release vN too (the
#                                 owner's step: automated sessions may not
#                                 create releases)
#
# A release is every driver's zip (NAME-VERSION.ZIP), CATALOG.TXT and
# SHA256SUMS, built from this repository's committed main and a clean
# sibling FREEHOLD checkout, so each zip's NAME.INF names the exact commits.
# releases/vN/ is committed: its files are published by the push, at
# https://raw.githubusercontent.com/Gerszcom/FREEHOLD-DRIVERS/<commit>/releases/vN/
# (as FREEHOLD-APPS serves its zips from the repository), and a GitHub
# release of the same files is the same bytes, so a pin holds for both.
# A published release is never rebuilt: the next one is N+1.
#
# The zips need no signature: FREEHOLD's build pins the basic ones by
# SHA-256 (tools/drivers.pin there), and a FREEHOLD machine checks every zip
# against the catalog's SHA-256. The catalog does: a machine trusts it only
# with CATALOG.SIG, its Ed25519 signature by the owner's release key, which
# only the owner holds (the same key and step as FREEHOLD's releases,
# D-212). The owner signs releases/vN/CATALOG.TXT and commits
# releases/vN/CATALOG.SIG beside it (the steps printed below). A GitHub
# release stays a prerelease until it carries CATALOG.SIG.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"
N="${1:-}"
case "$N" in ''|*[!0-9]*) echo "usage: tools/release.sh N [--publish]" >&2; exit 1 ;; esac
PUBLISH="${2:-}"
REPO=Gerszcom/FREEHOLD-DRIVERS
TAG="v$N"
FREEHOLD="$(cd "$ROOT/.." && pwd)/freehold"

if [ -e "releases/$TAG" ] && [ "$PUBLISH" != --publish ]; then
    echo "release: releases/$TAG exists; a published release is never rebuilt (raise N)" >&2
    exit 1
fi
if [ ! -e "releases/$TAG" ]; then
    if [ -n "$(git status --porcelain --untracked-files=no)" ]; then
        echo "release: commit your changes first (a release names its commit)" >&2
        exit 1
    fi
    if [ -n "$(git -C "$FREEHOLD" status --porcelain --untracked-files=no)" ]; then
        echo "release: $FREEHOLD has changes; a release is built against a clean FREEHOLD commit" >&2
        git -C "$FREEHOLD" status --short --untracked-files=no >&2
        exit 1
    fi
    bash tools/build.sh --quiet
    mkdir -p "releases/$TAG"
    cp dist/*.ZIP dist/CATALOG.TXT dist/SHA256SUMS "releases/$TAG/"
    echo "release: releases/$TAG/ — $(ls "releases/$TAG"/*.ZIP | wc -l) zip(s), CATALOG.TXT (unsigned), SHA256SUMS"
    cat "releases/$TAG/SHA256SUMS"
    echo
    echo "Commit releases/$TAG/ and push: that publishes it."
fi

if [ "$PUBLISH" = --publish ]; then
    WHAT="$(sed -n 's/^[0-9A-F]*:[0-9A-F*]* [a-z0-9]* \([A-Z0-9_-]*\) \([^ ]*\) .*/\1 \2/p' \
        "releases/$TAG/CATALOG.TXT" | sort -u | paste -sd, | sed 's/,/, /g')"
    FILES=("releases/$TAG"/*.ZIP "releases/$TAG/CATALOG.TXT" "releases/$TAG/SHA256SUMS")
    PRE=(--prerelease)
    if [ -f "releases/$TAG/CATALOG.SIG" ]; then
        FILES+=("releases/$TAG/CATALOG.SIG")
        PRE=(--latest)
    fi
    gh release create "$TAG" "${FILES[@]}" --repo "$REPO" "${PRE[@]}" \
        --title "FREEHOLD drivers $N" \
        --notes "FREEHOLD drivers, release $N: $WHAT. Each zip's NAME.INF names the commits it was built from."
    echo "release: published $TAG on GitHub (${PRE[*]})"
fi

if [ ! -f "releases/$TAG/CATALOG.SIG" ]; then
    cat <<EOF

To make release $N's catalog trusted (the owner, with the key only he holds):

  1. Sign the catalog exactly as committed:
       openssl pkeyutl -sign -inkey freehold-release.pem -rawin \\
           -in releases/$TAG/CATALOG.TXT -out releases/$TAG/CATALOG.SIG
  2. Check it against the public key FREEHOLD ships (C:\\FREEHOLD\\RELEASE.KEY):
       openssl pkeyutl -verify -pubin -inkey RELEASE.KEY -rawin \\
           -in releases/$TAG/CATALOG.TXT -sigfile releases/$TAG/CATALOG.SIG
  3. Commit and push releases/$TAG/CATALOG.SIG. To have the release on
     GitHub too: tools/release.sh $N --publish (or, if $TAG is there
     already, gh release upload $TAG releases/$TAG/CATALOG.SIG --repo $REPO
     and gh release edit $TAG --prerelease=false --latest --repo $REPO).
EOF
fi
