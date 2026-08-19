#!/usr/bin/env bash
# Bump VERSION, build cangling-test:<ver>, and save a .tar.gz for the updater UI.
# Usage:
#   ./bump.sh current  # build/save the VERSION file as-is (first package: 1.0.0)
#   ./bump.sh          # increment patch (1.0.0 -> 1.0.1)
#   ./bump.sh patch
#   ./bump.sh minor    # 1.0.3 -> 1.1.0
#   ./bump.sh major    # 1.2.3 -> 2.0.0
set -euo pipefail

cd "$(dirname "$0")"

part="${1:-patch}"
case "$part" in
  current|major|minor|patch) ;;
  *)
    echo "usage: $0 [current|major|minor|patch]" >&2
    exit 2
    ;;
esac

if [[ ! -f VERSION ]]; then
  echo "1.0.0" > VERSION
fi

old="$(tr -d '[:space:]' < VERSION)"
if [[ ! "$old" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
  echo "VERSION must be semver like 1.0.0, got: $old" >&2
  exit 1
fi

IFS=. read -r major minor patch <<< "$old"
case "$part" in
  current) ;;
  major) major=$((major + 1)); minor=0; patch=0 ;;
  minor) minor=$((minor + 1)); patch=0 ;;
  patch) patch=$((patch + 1)) ;;
esac
ver="${major}.${minor}.${patch}"
echo "$ver" > VERSION

image="cangling-test:${ver}"
mkdir -p dist
out="dist/cangling-test-${ver}.tar.gz"

echo "building ${image} (was ${old})"
docker build --build-arg "VERSION=${ver}" -t "$image" .
docker save "$image" | gzip > "$out"

echo
echo "VERSION  ${old} -> ${ver}"
echo "image    ${image}"
echo "archive  $(pwd)/${out}"
echo
echo "Upload ${out} in the 苍岭更新 UI."
echo "After docker load + retag, compose uses cangling-test:latest"
echo "Open http://<host>:8088  — the page should show ${ver}"
