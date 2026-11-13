#!/usr/bin/env bash
# Make a release of FREEHOLD's drivers (FREEHOLD's DECISIONS.md, D-215).
#
#   tools/release.sh N            build and package from a clean checkout,
#                                 print the steps that publish release vN
#   tools/release.sh N --publish  also publish the zips and the unsigned
#                                 catalog as GitHub release vN, a prerelease
#
# A release is every driver's zip (NAME-VERSION.ZIP), CATALOG.TXT and
# SHA256SUMS, built from this repository's committed main and a clean
# sibling FREEHOLD checkout, so each zip's NAME.INF names the exact commits.
# The zips need no signature: FREEHOLD's build pins the basic ones by
# SHA-256 (tools/drivers.pin there), and a FREEHOLD machine checks every zip
# against the catalog's SHA-256. The catalog does: a machine trusts it only
# with CATALOG.SIG, its Ed25519 signature by the owner's release key, which
# only the owner holds (the same key and step as FREEHOLD's releases,
# D-212). So the release stays a prerelease — not what
# releases/latest/download/CATALOG.TXT serves — until the owner has signed
# its catalog and uploaded the signature (the steps printed below).
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"
N="${1:-}"
case "$N" in ''|*[!0-9]*) echo "usage: tools/release.sh N [--publish]" >&2; exit 1 ;; esac
PUBLISH="${2:-}"
REPO=Gerszcom/FREEHOLD-DRIVERS
TAG="v$N"
FREEHOLD="$(cd "$ROOT/.." && pwd)/freehold"

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
rm -rf "release/$TAG"
mkdir -p "release/$TAG"
cp dist/*.ZIP dist/CATALOG.TXT dist/SHA256SUMS "release/$TAG/"
echo "release: release/$TAG/ — $(ls "release/$TAG"/*.ZIP | wc -l) zip(s), CATALOG.TXT (unsigned), SHA256SUMS"
cat "release/$TAG/SHA256SUMS"

NOTES="FREEHOLD drivers, release $N: $(sed -n 's/^\([0-9A-F]*:[0-9A-F*]*\) [a-z0-9]* \([A-Z0-9_-]*\) \([^ ]*\) .*/\2 \3/p' "release/$TAG/CATALOG.TXT" | sort -u | paste -sd, | sed 's/,/, /g'). Built from $(git rev-parse --short HEAD) against FREEHOLD $(git -C "$FREEHOLD" rev-parse --short HEAD). The catalog is trusted once CATALOG.SIG, the owner's signature, is attached."
if [ "$PUBLISH" = --publish ]; then
    gh release create "$TAG" "release/$TAG"/*.ZIP "release/$TAG/CATALOG.TXT" "release/$TAG/SHA256SUMS" \
        --repo "$REPO" --target "$(git rev-parse HEAD)" --prerelease \
        --title "FREEHOLD drivers $N" --notes "$NOTES"
    echo "release: published $TAG as a prerelease (zips and unsigned catalog)"
fi

cat <<EOF

To make release $N trusted (the owner, with the key only he holds):

  1. Fetch the catalog exactly as published, and sign it:
       gh release download $TAG --repo $REPO --pattern CATALOG.TXT --dir sign-$TAG
       openssl pkeyutl -sign -inkey freehold-release.pem -rawin \\
           -in sign-$TAG/CATALOG.TXT -out sign-$TAG/CATALOG.SIG
  2. Check it against the public key FREEHOLD ships (C:\\FREEHOLD\\RELEASE.KEY):
       openssl pkeyutl -verify -pubin -inkey RELEASE.KEY -rawin \\
           -in sign-$TAG/CATALOG.TXT -sigfile sign-$TAG/CATALOG.SIG
  3. Attach the signature and make it the latest release:
       gh release upload $TAG sign-$TAG/CATALOG.SIG --repo $REPO
       gh release edit $TAG --prerelease=false --latest --repo $REPO
EOF
[ "$PUBLISH" = --publish ] || echo "  (first publish the release: tools/release.sh $N --publish)"
