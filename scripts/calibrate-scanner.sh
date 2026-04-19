#!/usr/bin/env bash
# calibrate-scanner.sh — Validate scanner output against hand-labelled holdout.
#
# Purpose:
#   Compute Spearman rank correlation (ρ) between the ranker's `analysis.sentiment_score`
#   and human-assigned scores in a labels file. Used as a release gate: ρ ≥ 0.6 required.
#
# Expected labels file format — one JSON object per line (JSONL):
#   {"topic_id":"6843582","human_score":7.5,"note":"Good film, good rip"}
#   {"topic_id":"6844681","human_score":4.0,"note":"Average quality"}
#
#   Fields:
#     topic_id     string  rutracker topic id (must match a scanned topic)
#     human_score  float   0–10 inclusive
#     note         string  optional free-text annotation (ignored by this script)
#
#   Minimum 20 lines recommended for a meaningful Spearman ρ.
#
# Three-step workflow (must be run manually — requires a live Claude Code session):
#   1. Run scan-prepare for each forum referenced by the labels:
#        rutracker rank scan-prepare --forum <fid>
#   2. In Claude Code, run the scan skill:
#        /rank-scan-run --forum <fid>
#   3. Re-run this script to compute Spearman ρ.
#
# Usage:
#   scripts/calibrate-scanner.sh [--root <mirror-root>] [--labels <labels.jsonl>]
#
# Options:
#   --root    Mirror root directory (default: $HOME/.rutracker/mirror)
#   --labels  Path to labels JSONL file
#             (default: crates/ranker/tests/fixtures/ranker/labels.jsonl)
#
# Exit codes:
#   0  ρ ≥ 0.6 (release gate passed)
#   1  ρ < 0.6 (gate failed) or labels file missing or insufficient data
#   2  Usage / environment error

set -euo pipefail

# ── Defaults ────────────────────────────────────────────────────────────────
ROOT="${HOME}/.rutracker/mirror"
LABELS="crates/ranker/tests/fixtures/ranker/labels.jsonl"

# ── Argument parsing ─────────────────────────────────────────────────────────
while [[ $# -gt 0 ]]; do
    case "$1" in
        --root)
            ROOT="$2"; shift 2 ;;
        --labels)
            LABELS="$2"; shift 2 ;;
        *)
            echo "Usage: $0 [--root <path>] [--labels <path>]" >&2
            exit 2 ;;
    esac
done

# ── Validate labels file ──────────────────────────────────────────────────────
if [[ ! -f "$LABELS" ]]; then
    echo "ERROR: labels file not found at $LABELS"
    echo "Expected format (one JSON per line):"
    echo '  {"topic_id":"<tid>","human_score":<0-10 float>,"note":"<optional>"}'
    echo "Minimum 20 lines recommended for meaningful Spearman rho."
    exit 1
fi

LABEL_COUNT=$(grep -c '"topic_id"' "$LABELS" 2>/dev/null || echo 0)
if [[ "$LABEL_COUNT" -eq 0 ]]; then
    echo "ERROR: labels file $LABELS appears empty or has no valid entries."
    exit 1
fi

echo "calibrate-scanner: root=$ROOT labels=$LABELS entries=$LABEL_COUNT"

if [[ "$LABEL_COUNT" -lt 20 ]]; then
    echo "WARNING: only $LABEL_COUNT labels found; 20+ recommended for meaningful Spearman rho."
fi

# ── Collect (sentiment_score, human_score) pairs ─────────────────────────────
# Read each label line, find the corresponding .scan.json, extract sentiment_score.
PAIRS_FILE="$(mktemp /tmp/calibrate-pairs.XXXXXX)"
trap 'rm -f "$PAIRS_FILE"' EXIT

MISSING_SCANS=0

while IFS= read -r line; do
    [[ -z "$line" ]] && continue

    # Extract topic_id and human_score using parameter expansion / awk
    TOPIC_ID=$(printf '%s' "$line" | python3 -c "import sys,json; d=json.loads(sys.stdin.read()); print(d['topic_id'])")
    HUMAN_SCORE=$(printf '%s' "$line" | python3 -c "import sys,json; d=json.loads(sys.stdin.read()); print(d['human_score'])")

    # Locate the scan file — search all forum subdirs
    SCAN_FILE=$(find "$ROOT/forums" -path "*/scans/${TOPIC_ID}.scan.json" 2>/dev/null | head -1)

    if [[ -z "$SCAN_FILE" ]]; then
        echo "  MISSING scan for topic $TOPIC_ID — run scan-prepare + /rank-scan-run first"
        MISSING_SCANS=$((MISSING_SCANS + 1))
        continue
    fi

    SENTIMENT=$(python3 -c "import sys,json; d=json.load(open('$SCAN_FILE')); print(d['analysis']['sentiment_score'])")
    printf '%s\t%s\n' "$SENTIMENT" "$HUMAN_SCORE" >> "$PAIRS_FILE"
done < "$LABELS"

if [[ "$MISSING_SCANS" -gt 0 ]]; then
    echo ""
    echo "Invocation procedure (run these steps, then rerun this script):"
    echo "  a. For each forum referenced by your labels run:"
    echo "       rutracker rank scan-prepare --forum <fid>"
    echo "  b. In a Claude Code session run:"
    echo "       /rank-scan-run --forum <fid>"
    echo "  c. Then rerun: scripts/calibrate-scanner.sh"
fi

PAIR_COUNT=$(wc -l < "$PAIRS_FILE" | tr -d ' ')
if [[ "$PAIR_COUNT" -lt 2 ]]; then
    echo "ERROR: need at least 2 matched pairs to compute Spearman rho (got $PAIR_COUNT)."
    exit 1
fi

echo "calibrate-scanner: $PAIR_COUNT matched pairs found (${MISSING_SCANS} missing scans skipped)"

# ── Compute Spearman ρ (via python3) ─────────────────────────────────────────
RHO=$(python3 - "$PAIRS_FILE" <<'PYEOF'
import sys, math

pairs_file = sys.argv[1]
pairs = []
with open(pairs_file) as f:
    for line in f:
        line = line.strip()
        if not line:
            continue
        sentiment, human = line.split('\t')
        pairs.append((float(sentiment), float(human)))

n = len(pairs)

def rank_with_ties(values):
    """Return average ranks for a list of values (1-based)."""
    indexed = sorted(enumerate(values), key=lambda x: x[1])
    ranks = [0.0] * n
    i = 0
    while i < n:
        j = i
        while j < n and indexed[j][1] == indexed[i][1]:
            j += 1
        avg_rank = (i + 1 + j) / 2.0
        for k in range(i, j):
            ranks[indexed[k][0]] = avg_rank
        i = j
    return ranks

sentiments = [p[0] for p in pairs]
humans     = [p[1] for p in pairs]

r_s = rank_with_ties(sentiments)
r_h = rank_with_ties(humans)

d2_sum = sum((r_s[i] - r_h[i]) ** 2 for i in range(n))

rho = 1.0 - 6.0 * d2_sum / (n * (n * n - 1)) if n * (n * n - 1) != 0 else 0.0

print(f"{rho:.4f}")
PYEOF
)

echo "spearman_rho=${RHO} n=${PAIR_COUNT}"

# ── Gate ──────────────────────────────────────────────────────────────────────
THRESHOLD="0.6"
PASS=$(python3 -c "import sys; print('1' if float('${RHO}') >= float('${THRESHOLD}') else '0')")

if [[ "$PASS" == "1" ]]; then
    echo "calibrate-scanner: PASSED (rho=${RHO} >= ${THRESHOLD})"
    exit 0
else
    echo "calibrate-scanner: FAILED (rho=${RHO} < ${THRESHOLD}) — tighten the scanner prompt and re-scan"
    exit 1
fi
