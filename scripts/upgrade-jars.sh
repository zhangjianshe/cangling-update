#!/usr/bin/env bash
set -Eeuo pipefail

DEPLOY_ROOT="${DEPLOY_ROOT:-/opt/cangling/jars}"
INCOMING_DIR="${INCOMING_DIR:-/opt/cangling/jar-deploy/incoming}"
TARGET_HOST="${TARGET_HOST:-root@10.142.183.19}"
LOCK_FILE="${LOCK_FILE:-/run/lock/cangling-jar-upgrade.lock}"
JARS=(cis-server-1.0.0.jar cis-map-1.0.0.jar cis-k8s-1.0.0.jar np4-base-server-1.0.0.jar)

log() { printf '[%s] %s\n' "$(date '+%F %T')" "$*"; }
fail() { log "ERROR: $*"; exit 1; }
wait_http() { local url=$1 code; for ((i=1; i<=${2:-45}; i++)); do code=$(curl -sS --max-time 5 -o /dev/null -w '%{http_code}' "$url" 2>/dev/null || true); [[ -n "$code" && "$code" != 000 ]] && return 0; sleep 2; done; return 1; }

remote_restart_and_check() {
  ssh "${SSH_OPTS[@]}" "$TARGET_HOST" 'set -eu; cd /opt/cangling/apps/cluster; docker compose up -d --force-recreate cis-map np4-base; for i in $(seq 1 45); do a=$(curl -sS --max-time 5 -o /dev/null -w "%{http_code}" http://127.0.0.1:8079/ 2>/dev/null || true); b=$(curl -sS --max-time 5 -o /dev/null -w "%{http_code}" http://127.0.0.1:7600/ 2>/dev/null || true); test -n "$a" && test "$a" != 000 && test -n "$b" && test "$b" != 000 && exit 0; sleep 2; done; exit 1'
}
local_restart_and_check() {
  (cd /opt/cangling/apps/cluster && docker compose up -d --force-recreate cis-map np4-base)
  (cd /opt/cangling/apps/core && docker compose up -d --force-recreate cis-server)
  (cd /opt/cangling/apps/cis-k8s && docker compose up -d --force-recreate cis-k8s)
  wait_http http://127.0.0.1:8079/; wait_http http://127.0.0.1:7600/; wait_http http://127.0.0.1:8078/; wait_http http://127.0.0.1:8077/
}
rollback_remote() { [[ -n "$1" ]] || return 0; log "Rolling back $TARGET_HOST to $1"; ssh "${SSH_OPTS[@]}" "$TARGET_HOST" "ln -sfn '$1' '$DEPLOY_ROOT/current'"; remote_restart_and_check; }
rollback_local() { [[ -n "$1" ]] || return 0; log "Rolling back local node to $1"; ln -sfn "$1" "$DEPLOY_ROOT/current"; local_restart_and_check; }
cleanup() { rm -rf "$LOCAL_STAGE"; ssh "${SSH_OPTS[@]}" "$TARGET_HOST" "rm -rf '$REMOTE_STAGE'" >/dev/null 2>&1 || true; }

[[ $# -le 1 ]] || fail "Usage: $0 [release-label]"
[[ $EUID -eq 0 ]] || fail "Run as root"
exec 9>"$LOCK_FILE"; flock -n 9 || fail "Another JAR upgrade is already running"
SSH_OPTS=(-o BatchMode=yes -o ConnectTimeout=10 -o StrictHostKeyChecking=accept-new)
VERSION="${1:-release}-$(date +%Y%m%d-%H%M%S)"
LOCAL_RELEASE="$DEPLOY_ROOT/releases/$VERSION"; REMOTE_RELEASE="$LOCAL_RELEASE"
LOCAL_STAGE="$DEPLOY_ROOT/releases/.${VERSION}.tmp.$$"; REMOTE_STAGE="$LOCAL_STAGE"

SELECTED=()
for jar in "${JARS[@]}"; do [[ -s "$INCOMING_DIR/$jar" ]] && SELECTED+=("$jar"); done
(( ${#SELECTED[@]} > 0 )) || fail "No supported JAR found in $INCOMING_DIR"
LOCAL_PREVIOUS=$(readlink -f "$DEPLOY_ROOT/current" 2>/dev/null || true)
[[ -d "$LOCAL_PREVIOUS" ]] || fail "Current local release does not exist: $LOCAL_PREVIOUS"
REMOTE_PREVIOUS=$(ssh "${SSH_OPTS[@]}" "$TARGET_HOST" "readlink -f '$DEPLOY_ROOT/current' 2>/dev/null || true")
[[ ! -e "$LOCAL_RELEASE" ]] || fail "Generated local release already exists: $LOCAL_RELEASE"
ssh "${SSH_OPTS[@]}" "$TARGET_HOST" "test ! -e '$REMOTE_RELEASE'" || fail "Generated remote release already exists: $REMOTE_RELEASE"

log "Staging ${#SELECTED[@]} JAR(s): ${SELECTED[*]}"
mkdir -p "$LOCAL_STAGE"; trap cleanup EXIT
for jar in "${JARS[@]}"; do install -m 0644 "$LOCAL_PREVIOUS/$jar" "$LOCAL_STAGE/$jar"; done
for jar in "${SELECTED[@]}"; do install -m 0644 "$INCOMING_DIR/$jar" "$LOCAL_STAGE/$jar"; done
(cd "$LOCAL_STAGE" && sha256sum "${JARS[@]}" > SHA256SUMS && sha256sum -c SHA256SUMS)
mv "$LOCAL_STAGE" "$LOCAL_RELEASE"

log "Synchronizing release to $TARGET_HOST"
ssh "${SSH_OPTS[@]}" "$TARGET_HOST" "mkdir -p '$REMOTE_STAGE'"
rsync -a --checksum -e "ssh ${SSH_OPTS[*]}" "$LOCAL_RELEASE/" "$TARGET_HOST:$REMOTE_STAGE/"
ssh "${SSH_OPTS[@]}" "$TARGET_HOST" "cd '$REMOTE_STAGE' && sha256sum -c SHA256SUMS && mv '$REMOTE_STAGE' '$REMOTE_RELEASE'"

log "Switching and upgrading $TARGET_HOST"
ssh "${SSH_OPTS[@]}" "$TARGET_HOST" "ln -sfn 'releases/$VERSION' '$DEPLOY_ROOT/current'"
if ! remote_restart_and_check; then rollback_remote "$REMOTE_PREVIOUS" || true; fail "Remote upgrade failed"; fi
log "Switching and upgrading local node"
ln -sfn "releases/$VERSION" "$DEPLOY_ROOT/current"
if ! local_restart_and_check; then rollback_local "$LOCAL_PREVIOUS" || true; rollback_remote "$REMOTE_PREVIOUS" || true; fail "Local upgrade failed"; fi

for jar in "${SELECTED[@]}"; do rm -f "$INCOMING_DIR/$jar"; done
log "Upgrade complete: $VERSION (${SELECTED[*]})"
