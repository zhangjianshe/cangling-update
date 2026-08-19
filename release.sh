#!/usr/bin/env bash
# 将 Cargo.toml 版本号 +1，提交并推送 tag，触发 GitHub Actions 发布。
# Usage:
#   ./release.sh          # 修订号 +1（0.1.0 -> 0.1.1）
#   ./release.sh patch
#   ./release.sh minor    # 0.1.3 -> 0.2.0
#   ./release.sh major    # 1.2.3 -> 2.0.0
#   ./release.sh --dry-run
set -euo pipefail

cd "$(dirname "$0")"

part="patch"
dry_run=0
for arg in "$@"; do
  case "$arg" in
    --dry-run|-n) dry_run=1 ;;
    major|minor|patch) part="$arg" ;;
    -h|--help)
      grep -E '^# ' "$0" | sed 's/^# //'
      exit 0
      ;;
    *)
      echo "usage: $0 [major|minor|patch] [--dry-run]" >&2
      exit 2
      ;;
  esac
done

if [[ ! -f Cargo.toml ]]; then
  echo "Cargo.toml not found" >&2
  exit 1
fi

old="$(sed -n 's/^version = "\([0-9]\+\.[0-9]\+\.[0-9]\+\)"/\1/p' Cargo.toml | head -n1)"
if [[ ! "$old" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
  echo "无法从 Cargo.toml 读取 semver 版本号" >&2
  exit 1
fi

IFS=. read -r major minor patch <<< "$old"
case "$part" in
  major) major=$((major + 1)); minor=0; patch=0 ;;
  minor) minor=$((minor + 1)); patch=0 ;;
  patch) patch=$((patch + 1)) ;;
esac
ver="${major}.${minor}.${patch}"
tag="v${ver}"

if git rev-parse "$tag" >/dev/null 2>&1; then
  echo "tag 已存在：$tag" >&2
  exit 1
fi

if [[ ! -d .git ]]; then
  echo "当前目录不是 git 仓库" >&2
  exit 1
fi

if ! git remote get-url origin >/dev/null 2>&1; then
  echo "没有配置 origin 远程仓库" >&2
  exit 1
fi

echo "$old -> $ver  ($tag)"

if [[ "$dry_run" -eq 1 ]]; then
  echo "[dry-run] 将修改 Cargo.toml / Cargo.lock，提交，并 git push origin HEAD $tag"
  exit 0
fi

tmp="$(mktemp)"
awk -v new="$ver" '
  BEGIN { done = 0 }
  !done && $0 ~ /^version = "[0-9]+\.[0-9]+\.[0-9]+"/ {
    print "version = \"" new "\""
    done = 1
    next
  }
  { print }
' Cargo.toml > "$tmp"
mv "$tmp" Cargo.toml

if [[ -f Cargo.lock ]]; then
  tmp="$(mktemp)"
  awk -v new="$ver" '
    $0 == "name = \"cangling-update\"" { hit = 1 }
    hit && $0 ~ /^version = "/ {
      print "version = \"" new "\""
      hit = 0
      next
    }
    { print }
  ' Cargo.lock > "$tmp"
  mv "$tmp" Cargo.lock
fi

if command -v cargo >/dev/null 2>&1; then
  cargo metadata --offline --format-version 1 >/dev/null
fi

git add Cargo.toml
if [[ -f Cargo.lock ]]; then
  git add Cargo.lock
fi

if git diff --cached --quiet; then
  echo "版本文件没有变化" >&2
  exit 1
fi

git commit -m "release: ${tag}"
git tag -a "$tag" -m "release ${tag}"
git push origin HEAD
git push origin "$tag"

echo
echo "已推送 $tag，GitHub Actions 将编译并发布："
echo "  cangling-update-linux-amd64"
echo "  cangling-update-linux-arm64"
remote="$(git remote get-url origin)"
echo "  $remote"
