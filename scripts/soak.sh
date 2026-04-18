#!/usr/bin/env bash
# Manual release-gate soak test — Phase 6a.
#
# Fetches 20 random topic IDs from a rutracker category and asserts each parses without
# panic and returns non-empty title/description. Produces soak-<date>.log. Required to
# pass before Phase 6b (Python deletion) is merged.
#
# This script is NOT run by `cargo test` (live network, auth cookies, Keychain). It is
# the explicit manual gate that closes Phase 3 pre-mortem scenario 3 ("scraper rejects
# malformed phpBB HTML the way lxml tolerated it").

set -euo pipefail

CATEGORY="${SOAK_CATEGORY:-252}"      # default: Фильмы 2026
N="${SOAK_N:-20}"
LOG="soak-$(date +%Y-%m-%d-%H%M%S).log"
BIN="${SOAK_BIN:-rutracker}"

command -v "$BIN" >/dev/null 2>&1 || {
    echo "error: $BIN not on PATH. Run 'cargo install --path crates/cli --locked' first." >&2
    exit 2
}

echo "soak: fetching $N random topic IDs from category $CATEGORY" | tee "$LOG"

# 1. Grab a page of topic IDs from the category.
IDS=$("$BIN" browse "$CATEGORY" --format json 2>>"$LOG" \
    | jq -r '.results[].topic_id' \
    | shuf \
    | head -n "$N")

if [ -z "$IDS" ]; then
    echo "soak: FAILED — no topic IDs returned from category $CATEGORY" | tee -a "$LOG"
    exit 1
fi

FAIL=0
OK=0
while IFS= read -r TID; do
    [ -z "$TID" ] && continue
    echo "soak: fetching topic $TID" >> "$LOG"
    if OUTPUT=$("$BIN" topic "$TID" --format json 2>>"$LOG"); then
        TITLE=$(echo "$OUTPUT" | jq -r '.title // ""')
        DESC_LEN=$(echo "$OUTPUT" | jq -r '.description // ""' | wc -c)
        if [ -n "$TITLE" ] && [ "$DESC_LEN" -gt 50 ]; then
            OK=$((OK + 1))
            echo "  ok  $TID  $TITLE (desc $DESC_LEN chars)" >> "$LOG"
        else
            FAIL=$((FAIL + 1))
            echo "  FAIL $TID  empty title or short desc (title=$TITLE, desc=$DESC_LEN chars)" | tee -a "$LOG"
        fi
    else
        FAIL=$((FAIL + 1))
        echo "  FAIL $TID  fetch/parse errored (see log)" | tee -a "$LOG"
    fi
done <<< "$IDS"

echo "soak: $OK passed, $FAIL failed (of $N)" | tee -a "$LOG"

if [ "$FAIL" -eq 0 ] && [ "$OK" -eq "$N" ]; then
    echo "All $N topics parsed successfully" | tee -a "$LOG"
    exit 0
fi

echo "soak: FAILED — commit $LOG to the release and investigate before Phase 6b merges." | tee -a "$LOG"
exit 1
