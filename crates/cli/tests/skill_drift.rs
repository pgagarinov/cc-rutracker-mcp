//! Bi-directional drift guard: fails if `.claude/skills/rutracker.md` falls out
//! of sync with the actual `rutracker` CLI surface in either direction.

use regex::Regex;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Commands present in the CLI but intentionally absent from the skill doc.
/// Add entries only when a command is genuinely not worth documenting.
const ALLOWLIST: &[&str] = &[
    // rationale: purely internal alias clap injects; never user-facing
];

// ---------------------------------------------------------------------------
// Skill file locator
// ---------------------------------------------------------------------------

/// Walk up from CARGO_MANIFEST_DIR until we find the file at `rel` below a
/// directory that also contains a `.claude/` subdirectory (repo root).
fn locate_repo_file(rel: &str) -> PathBuf {
    let start = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let mut cur: &Path = &start;
    loop {
        let candidate = cur.join(rel);
        if candidate.is_file() {
            return candidate;
        }
        cur = cur.parent().unwrap_or_else(|| {
            panic!("could not find {rel} walking up from {start:?}");
        });
    }
}

// ---------------------------------------------------------------------------
// Set A — commands extracted from the skill file
// ---------------------------------------------------------------------------

/// Parse every `` `rutracker <subcommand chain>` `` occurrence in `text`.
/// Returns normalised subcommand paths such as `"mirror watch add"`.
fn extract_skill_commands(text: &str) -> BTreeSet<String> {
    // Match inline code spans containing `rutracker ...`
    // Capture everything after "rutracker " up to the closing backtick.
    let re = Regex::new(r"`rutracker ([^`]+)`").unwrap();
    let mut out = BTreeSet::new();

    for cap in re.captures_iter(text) {
        let rest = cap[1].trim().to_string();
        // Split on whitespace and stop at the first token that:
        //   - starts with `-` (flag), `<` (placeholder), `"` (quoted string)
        //   - contains `/` (path) or `~` (home-relative path)
        //   - is purely numeric (positional id like `6843582`)
        //   - is `...` (ellipsis placeholder)
        //   - starts with `→` or `|` (inline table / arrow notation)
        let tokens: Vec<&str> = rest
            .split_whitespace()
            .take_while(|t| {
                !t.starts_with('-')
                    && !t.starts_with('<')
                    && !t.starts_with('"')
                    && !t.contains('/')
                    && !t.contains('~')
                    && *t != "..."
                    && !t.chars().all(|c| c.is_ascii_digit())
                    && !t.starts_with('→')
                    && !t.starts_with('|')
            })
            .collect();

        if tokens.is_empty() {
            continue;
        }
        out.insert(tokens.join(" "));
    }

    out
}

// ---------------------------------------------------------------------------
// Set B — commands extracted from the built binary's --help output
// ---------------------------------------------------------------------------

/// Recursively walk the clap help tree starting from `prefix`.
/// `prefix` is e.g. `&[]` for the root, `&["mirror"]` for subcommands of mirror.
/// Appends every *leaf* subcommand path (as a space-joined string) to `out`.
fn walk_cli(bin: &str, prefix: &[String], out: &mut BTreeSet<String>) {
    // Build argv: <bin> [prefix...] --help
    let mut argv: Vec<&str> = prefix.iter().map(String::as_str).collect();
    argv.push("--help");

    let output = Command::new(bin)
        .args(&argv)
        .output()
        .expect("failed to run rutracker binary");

    let text = String::from_utf8_lossy(&output.stdout).to_string()
        + &String::from_utf8_lossy(&output.stderr);

    // Extract the "Commands:" section — lines indented after that header.
    // clap 4.x prints:
    //   Commands:
    //     search   Search for torrents
    //     topic    ...
    let mut in_commands = false;
    let mut children: Vec<String> = Vec::new();

    for line in text.lines() {
        if line.trim_start() == line && line.starts_with("Commands:") {
            in_commands = true;
            continue;
        }
        if in_commands {
            // A non-indented non-empty line signals end of Commands section
            if !line.starts_with(' ') && !line.starts_with('\t') && !line.is_empty() {
                in_commands = false;
                continue;
            }
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            // First word is the subcommand name
            let sub = trimmed.split_whitespace().next().unwrap();
            // Skip the auto-generated `help` subcommand
            if sub == "help" {
                continue;
            }
            children.push(sub.to_string());
        }
    }

    if children.is_empty() {
        // This is a leaf — record the full path (skip root-only bare "rutracker")
        if !prefix.is_empty() {
            out.insert(prefix.join(" "));
        }
    } else {
        for child in children {
            let mut next_prefix: Vec<String> = prefix.to_vec();
            next_prefix.push(child);
            walk_cli(bin, &next_prefix, out);
        }
    }
}

// ---------------------------------------------------------------------------
// Test
// ---------------------------------------------------------------------------

#[test]
fn test_skill_commands_match_cli_surface() {
    let bin = env!("CARGO_BIN_EXE_rutracker");
    let skill_path = locate_repo_file(".claude/skills/rutracker.md");

    // --- Set A ---
    let skill_text = std::fs::read_to_string(&skill_path).expect("could not read skill file");
    let set_a = extract_skill_commands(&skill_text);

    // --- Set B ---
    let mut set_b = BTreeSet::new();
    walk_cli(bin, &[], &mut set_b);

    // --- Compute deltas ---
    let allowlisted: BTreeSet<String> = ALLOWLIST.iter().map(|s| s.to_string()).collect();

    let a_minus_b: Vec<String> = set_a.difference(&set_b).cloned().collect();

    let b_minus_a: Vec<String> = set_b
        .difference(&set_a)
        .filter(|cmd| !allowlisted.contains(*cmd))
        .cloned()
        .collect();

    let mut errors: Vec<String> = Vec::new();

    if !a_minus_b.is_empty() {
        errors.push(format!(
            "SKILL references commands not in CLI:\n  {}",
            a_minus_b.join("\n  ")
        ));
    }
    if !b_minus_a.is_empty() {
        errors.push(format!(
            "CLI has commands missing from SKILL (add to skill or allowlist):\n  {}",
            b_minus_a.join("\n  ")
        ));
    }

    assert!(
        errors.is_empty(),
        "\n\nSkill↔CLI drift detected:\n\n{}\n",
        errors.join("\n\n")
    );
}
