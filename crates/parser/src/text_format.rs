//! Text formatters reused by both binaries (CLI `--format text` and MCP tool output).
//!
//! Produces byte-identical output to the legacy Python MCP's formatters. Snapshot-tested
//! against `tests/fixtures/legacy-snapshots/legacy-{search,get-topic}.txt`.

use crate::{SearchResult, TopicDetails};

/// Python-equivalent `server.search` formatter:
///
/// ```text
/// Found {N} results:
///
/// [id] title | Size: ... | Seeds: s | Leeches: l | Category: ... | Author: ...
///
/// [next row]
/// ```
pub fn format_search_legacy(results: &[SearchResult]) -> String {
    if results.is_empty() {
        return "No results found.".to_string();
    }

    let rows: Vec<String> = results
        .iter()
        .map(|r| {
            let mut parts = vec![format!("[{}] {}", r.topic_id, r.title)];
            if !r.size.is_empty() {
                parts.push(format!("Size: {}", r.size));
            }
            parts.push(format!("Seeds: {} | Leeches: {}", r.seeds, r.leeches));
            if !r.category.is_empty() {
                parts.push(format!("Category: {}", r.category));
            }
            if !r.author.is_empty() {
                parts.push(format!("Author: {}", r.author));
            }
            parts.join(" | ")
        })
        .collect();

    format!("Found {} results:\n\n{}", results.len(), rows.join("\n\n"))
}

/// Python-equivalent `server.get_topic` formatter.
pub fn format_topic_legacy(td: &TopicDetails) -> String {
    let mut lines = vec![format!("Title: {}", td.title)];
    if !td.size.is_empty() {
        lines.push(format!("Size: {}", td.size));
    }
    lines.push(format!("Seeds: {} | Leeches: {}", td.seeds, td.leeches));
    if !td.magnet_link.is_empty() {
        lines.push(format!("Magnet: {}", td.magnet_link));
    }
    if !td.description.is_empty() {
        lines.push(format!("\nDescription:\n{}", td.description));
    }
    if !td.file_list.is_empty() {
        lines.push(format!("\nFiles ({}):", td.file_list.len()));
        for f in td.file_list.iter().take(50) {
            lines.push(format!("  - {}", f));
        }
        if td.file_list.len() > 50 {
            lines.push(format!("  ... and {} more files", td.file_list.len() - 50));
        }
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{search::parse_search_page, topic::parse_topic_page};
    use encoding_rs::WINDOWS_1251;

    const FORUM_FIXTURE: &[u8] = include_bytes!("../tests/fixtures/forum-sample.html");
    const TOPIC_FIXTURE: &[u8] = include_bytes!("../tests/fixtures/topic-sample.html");
    const LEGACY_SEARCH: &str =
        include_str!("../tests/fixtures/legacy-snapshots/legacy-search.txt");
    const LEGACY_TOPIC: &str =
        include_str!("../tests/fixtures/legacy-snapshots/legacy-get-topic.txt");

    fn decode(bytes: &[u8]) -> String {
        let (cow, _, _) = WINDOWS_1251.decode(bytes);
        cow.into_owned()
    }

    #[test]
    fn test_legacy_search_byte_equal() {
        // legacy-search.txt is a snapshot of format_search_legacy applied to
        // forum-sample.html.  Byte-equal comparison catches any formatting regression.
        let html = decode(FORUM_FIXTURE);
        let page = parse_search_page(&html).unwrap();
        let rendered = format_search_legacy(&page.results);
        assert_eq!(
            rendered.as_bytes(),
            LEGACY_SEARCH.as_bytes(),
            "search formatter output differs from committed snapshot"
        );
    }

    #[test]
    fn test_empty_search_returns_no_results_hint() {
        let out = format_search_legacy(&[]);
        assert_eq!(out, "No results found.");
    }

    #[test]
    fn test_search_row_omits_empty_fields() {
        // Row with empty size, empty category, empty author should still render
        // but omit those sections.
        let results = vec![SearchResult {
            topic_id: 42,
            title: "Minimal".into(),
            size: String::new(),
            seeds: 0,
            leeches: 0,
            category: String::new(),
            author: String::new(),
        }];
        let out = format_search_legacy(&results);
        assert!(out.contains("[42] Minimal"));
        assert!(out.contains("Seeds: 0 | Leeches: 0"));
        assert!(!out.contains("Size: "));
        assert!(!out.contains("Category: "));
        assert!(!out.contains("Author: "));
    }

    #[test]
    fn test_topic_omits_empty_sections_and_truncates_files() {
        let td = TopicDetails {
            topic_id: 1,
            title: "Topic Title".into(),
            magnet_link: String::new(),
            size: String::new(),
            seeds: 1,
            leeches: 0,
            description: String::new(),
            file_list: (0..60).map(|i| format!("file-{i}.mkv")).collect(),
            metadata: None,
            comments: Vec::new(),
            comment_pages_fetched: 1,
            comment_pages_total: 1,
        };
        let out = format_topic_legacy(&td);
        assert!(out.contains("Title: Topic Title"));
        assert!(!out.contains("Size: "));
        assert!(!out.contains("Magnet: "));
        assert!(!out.contains("\nDescription:\n"));
        assert!(out.contains("Files (60):"));
        assert!(
            out.contains("... and 10 more files"),
            "expected truncation summary, got: {out}"
        );
    }

    #[test]
    fn test_legacy_get_topic_byte_equal() {
        // legacy-get-topic.txt is a snapshot of format_topic_legacy applied to
        // topic-sample.html.  Byte-equal comparison catches any formatting regression.
        let html = decode(TOPIC_FIXTURE);
        let td = parse_topic_page(&html).unwrap();
        let rendered = format_topic_legacy(&td);
        assert_eq!(
            rendered.as_bytes(),
            LEGACY_TOPIC.as_bytes(),
            "topic formatter output differs from committed snapshot"
        );
    }
}
