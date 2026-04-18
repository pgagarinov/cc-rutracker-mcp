#!/usr/bin/env bash
# Manual release-gate soak test — Phase M6.
#
# Two-pass contract for `rutracker mirror sync`:
#   1. Initial sync against a fresh mirror root writes at least 6 topic files
#      (≥ 3 topics × 2 forums by default).
#   2. A second sync against the same root writes 0 new topic files (idempotent
#      steady state — delta detection + 5-consecutive-older-and-known streak).
#
# Exits 0 iff both conditions hold. Commits a soak-mirror-<date>.log with the
# release. Not run by `cargo test`: requires live network, auth cookies, and the
# `rutracker` binary on PATH.
#
# Environment:
#   SOAK_FORUMS      space-separated forum ids (default: "252 251")
#   SOAK_MAX_TOPICS  topics-per-forum cap (default: 5)
#   SOAK_MIN_FIRST   minimum topic files after the first sync (default: 6)
#   SOAK_BIN         rutracker binary (default: rutracker)
#   SOAK_ROOT        mirror root (default: an auto-cleaned temp dir)

set -euo pipefail

FORUMS="${SOAK_FORUMS:-252 251}"
MAX_TOPICS="${SOAK_MAX_TOPICS:-5}"
MIN_FIRST="${SOAK_MIN_FIRST:-6}"
BIN="${SOAK_BIN:-rutracker}"
LOG="soak-mirror-$(date +%Y-%m-%d-%H%M%S).log"

command -v "$BIN" >/dev/null 2>&1 || {
    echo "error: $BIN not on PATH. Run 'cargo install --path crates/cli --locked' first." >&2
    exit 2
}

if [ -n "${SOAK_ROOT:-}" ]; then
    ROOT="$SOAK_ROOT"
    KEEP_ROOT=1
else
    ROOT="$(mktemp -d -t rutracker-mirror-soak.XXXXXX)"
    KEEP_ROOT=0
    trap '[ "$KEEP_ROOT" = "0" ] && rm -rf "$ROOT"' EXIT
fi

echo "soak-mirror: root=$ROOT forums=$FORUMS max_topics=$MAX_TOPICS" | tee "$LOG"

count_topic_jsons() {
    local r="$1"
    if [ ! -d "$r/forums" ]; then
        echo 0
        return
    fi
    find "$r/forums" -type f -name '*.json' 2>/dev/null | wc -l | tr -d ' '
}

echo "soak-mirror: init" | tee -a "$LOG"
"$BIN" mirror init --root "$ROOT" >> "$LOG" 2>&1

echo "soak-mirror: structure" | tee -a "$LOG"
"$BIN" mirror structure --root "$ROOT" >> "$LOG" 2>&1

for FID in $FORUMS; do
    echo "soak-mirror: watch add $FID" | tee -a "$LOG"
    "$BIN" mirror watch add "$FID" --root "$ROOT" >> "$LOG" 2>&1
done

BEFORE_FIRST=$(count_topic_jsons "$ROOT")

echo "soak-mirror: first sync (max-topics=$MAX_TOPICS per forum)" | tee -a "$LOG"
SYNC_ARGS=(--max-topics "$MAX_TOPICS" --root "$ROOT")
for FID in $FORUMS; do
    SYNC_ARGS+=(--forum "$FID")
done
"$BIN" mirror sync "${SYNC_ARGS[@]}" >> "$LOG" 2>&1

AFTER_FIRST=$(count_topic_jsons "$ROOT")
FIRST_WRITTEN=$((AFTER_FIRST - BEFORE_FIRST))
echo "soak-mirror: first sync wrote $FIRST_WRITTEN topic files (threshold: >= $MIN_FIRST)" | tee -a "$LOG"

if [ "$FIRST_WRITTEN" -lt "$MIN_FIRST" ]; then
    echo "soak-mirror: FAILED — first sync produced $FIRST_WRITTEN files (need >= $MIN_FIRST)" | tee -a "$LOG"
    exit 1
fi

echo "soak-mirror: second sync (expect 0 new files)" | tee -a "$LOG"
"$BIN" mirror sync "${SYNC_ARGS[@]}" >> "$LOG" 2>&1

AFTER_SECOND=$(count_topic_jsons "$ROOT")
SECOND_WRITTEN=$((AFTER_SECOND - AFTER_FIRST))
echo "soak-mirror: second sync wrote $SECOND_WRITTEN topic files (threshold: == 0)" | tee -a "$LOG"

if [ "$SECOND_WRITTEN" -ne 0 ]; then
    echo "soak-mirror: FAILED — second sync wrote $SECOND_WRITTEN files (expected 0)" | tee -a "$LOG"
    exit 1
fi

echo "soak-mirror: PASSED. log=$LOG" | tee -a "$LOG"
exit 0
