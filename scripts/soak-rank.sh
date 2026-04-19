#!/usr/bin/env bash
# soak-rank.sh — Full-pipeline soak test for rutracker-ranker (Phase R5).
#
# Runs the three-stage ranking pipeline against a real mirror forum and
# asserts that at least 100 films were scored. Logs everything to a dated
# file so results can be committed as a release artefact.
#
# Usage:
#   scripts/soak-rank.sh [--root <mirror-root>] [--forum <id>] [--non-interactive]
#
# Options:
#   --root             Mirror root directory (default: $HOME/.rutracker/mirror)
#   --forum            Forum id to rank (default: 252)
#   --non-interactive  Skip the interactive pause for /rank-scan-run; instead
#                      proceed immediately if .scan.json files already exist.
#
# Exit codes:
#   0  Pipeline ran and film_score table has >= 100 rows
#   1  Assertion failed or pipeline error
#   2  Usage / environment error
#
# Not run by `cargo test`: requires live mirror data, auth cookies, sqlite3,
# and the `rutracker` binary on PATH.

set -euo pipefail

# ── Defaults ─────────────────────────────────────────────────────────────────
ROOT="${HOME}/.rutracker/mirror"
FORUM="252"
NON_INTERACTIVE=0
LOG="soak-rank-$(date +%Y%m%d-%H%M%S).log"
BIN="${SOAK_BIN:-rutracker}"

# ── Argument parsing ──────────────────────────────────────────────────────────
while [[ $# -gt 0 ]]; do
    case "$1" in
        --root)
            ROOT="$2"; shift 2 ;;
        --forum)
            FORUM="$2"; shift 2 ;;
        --non-interactive)
            NON_INTERACTIVE=1; shift ;;
        *)
            echo "Usage: $0 [--root <path>] [--forum <id>] [--non-interactive]" >&2
            exit 2 ;;
    esac
done

# ── Prerequisite checks ───────────────────────────────────────────────────────
command -v "$BIN" >/dev/null 2>&1 || {
    echo "error: $BIN not on PATH. Run 'cargo install --path crates/cli --locked' first." >&2
    exit 2
}

command -v sqlite3 >/dev/null 2>&1 || {
    echo "error: sqlite3 not on PATH. Install it (e.g. 'brew install sqlite')." >&2
    exit 2
}

DB="$ROOT/state.db"
if [[ ! -f "$DB" ]]; then
    echo "error: mirror database not found at $DB — run 'rutracker mirror init' first." >&2
    exit 2
fi

# ── Start logging (tee to file + stdout) ─────────────────────────────────────
exec > >(tee "$LOG") 2>&1

echo "soak-rank: root=$ROOT forum=$FORUM log=$LOG"
echo "soak-rank: $(date -u +%Y-%m-%dT%H:%M:%SZ)"

# ── Stage A — match titles ────────────────────────────────────────────────────
echo ""
echo "soak-rank: [Stage A] rutracker rank match --forum $FORUM"
"$BIN" rank match --forum "$FORUM" --root "$ROOT"

# ── Stage B.1 — prepare scan queue ───────────────────────────────────────────
echo ""
echo "soak-rank: [Stage B.1] rutracker rank scan-prepare --forum $FORUM"
"$BIN" rank scan-prepare --forum "$FORUM" --root "$ROOT"

# ── Stage B.2 — execute scans (Claude Code) ──────────────────────────────────
SCANS_DIR="$ROOT/forums/$FORUM/scans"
SCAN_COUNT=0
if [[ -d "$SCANS_DIR" ]]; then
    SCAN_COUNT=$(find "$SCANS_DIR" -name '*.scan.json' 2>/dev/null | wc -l | tr -d ' ')
fi

if [[ "$NON_INTERACTIVE" -eq 1 ]]; then
    if [[ "$SCAN_COUNT" -gt 0 ]]; then
        echo ""
        echo "soak-rank: [Stage B.2] non-interactive mode — $SCAN_COUNT .scan.json files found, skipping /rank-scan-run prompt"
    else
        echo ""
        echo "WARNING: --non-interactive set but no .scan.json files found in $SCANS_DIR"
        echo "         Proceeding anyway; aggregate may report missing scans."
    fi
else
    echo ""
    echo "soak-rank: [Stage B.2] Manual step required."
    echo "  In a Claude Code session, run:"
    echo "    /rank-scan-run --forum $FORUM"
    echo ""
    printf "Now run /rank-scan-run --forum %s in Claude Code; press enter when done: " "$FORUM"
    read -r _
fi

# ── Stage C — aggregate ───────────────────────────────────────────────────────
echo ""
echo "soak-rank: [Stage C] rutracker rank aggregate --forum $FORUM"
"$BIN" rank aggregate --forum "$FORUM" --root "$ROOT"

# ── Assert >= 100 films scored ────────────────────────────────────────────────
echo ""
echo "soak-rank: checking film_score count in $DB"
FILM_COUNT=$(sqlite3 "$DB" "SELECT COUNT(*) FROM film_score;")
echo "soak-rank: film_score rows = $FILM_COUNT (threshold: >= 100)"

if [[ "$FILM_COUNT" -lt 100 ]]; then
    echo "soak-rank: FAILED — only $FILM_COUNT films scored (need >= 100)"
    exit 1
fi

echo ""
echo "soak-rank: PASSED. film_count=$FILM_COUNT log=$LOG"
exit 0
