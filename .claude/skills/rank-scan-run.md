---
name: rank-scan-run
description: Consumes forums/<fid>/scan-queue.jsonl produced by 'rutracker rank scan-prepare' and writes scans/<tid>.scan.json per topic via the rutracker-film-scanner subagent.
---

Thin consumer skill — all cache/truncation logic already ran in Rust. You only dispatch the scanner and persist its JSON.

Steps:

1. Read `<root>/forums/<fid>/scan-queue.jsonl`. If missing or empty, print `"no topics queued — run 'rutracker rank scan-prepare --forum <fid>' first"` and exit.
2. Read `<root>/forums/<fid>/scan-queue.done.jsonl` if it exists; collect its `topic_id`s into a skip-set for resumability.
3. For each manifest line (one JSON object per line):
   a. Parse the line. Skip if `topic_id` is in the done skip-set.
   b. Call `Agent(subagent_type="rutracker-film-scanner", prompt=<JSON-stringified line.payload>)`.
   c. Parse the agent's final text response as JSON. On parse failure, retry ONCE with a short reminder. On second failure, atomic-write `<scan_path with trailing '.scan.json' replaced by '.scan.failed.json'>` containing `{"error": "parse_failed", "raw": <response>, "topic_id": ..., "last_post_id": ...}` and continue.
   d. On success, atomic-write `<scan_path>` (temp file + rename) with this exact shape:
      ```json
      {
        "schema": 1,
        "agent_sha": "<from manifest>",
        "scanned_at": "<ISO 8601 UTC>",
        "topic_id": "<from manifest>",
        "last_post_id": "<from manifest>",
        "analysis": <agent JSON verbatim>
      }
      ```
   e. Append the manifest line to `<root>/forums/<fid>/scan-queue.done.jsonl`.
4. Print final summary: `scanned=X failed=Y skipped_done=Z`.
