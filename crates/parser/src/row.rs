//! Shared row-level parser used by both `tracker.php` (search) and `viewforum.php` listings.
//!
//! `tracker.php` and `viewforum.php` ship different markup around the same six fields
//! (`topic_id`, `title`, `author`, `size`, `seeds`, `leeches`). This module owns a single
//! extraction routine so the two callers stay in sync.
//!
//! The implementation tries search-style selectors first and falls back to forum-style
//! selectors. That ordering preserves the pre-refactor search output bit-for-bit: whenever a
//! search-page row is in play the original selector fires, identical to the prior inline code.

use crate::RowCommon;
use regex::Regex;
use scraper::{ElementRef, Selector};

/// Parse the common subset of fields from a topic-row `<tr>` element.
///
/// Returns `None` if the row does not carry a recognisable `topic_id` — rows without one
/// (pagination stubs, announcements without a viewtopic link) are expected callers of a
/// simple `filter_map`.
pub fn parse_topic_row(row: &ElementRef<'_>) -> Option<RowCommon> {
    let topic_id = extract_topic_id(row)?;

    let title = match_first_text(row, &title_selectors()).trim().to_string();
    let author = match_first_text(row, &author_selectors())
        .trim()
        .to_string();
    let size = match_first_text(row, &size_selectors()).trim().to_string();
    let seeds = match_first_text(row, &seed_selectors())
        .trim()
        .parse::<u32>()
        .unwrap_or(0);
    let leeches = match_first_text(row, &leech_selectors())
        .trim()
        .parse::<u32>()
        .unwrap_or(0);

    Some(RowCommon {
        topic_id,
        title,
        author,
        size,
        seeds,
        leeches,
    })
}

fn match_first_text(row: &ElementRef<'_>, selectors: &[Selector]) -> String {
    for sel in selectors {
        if let Some(el) = row.select(sel).next() {
            return el.text().collect::<String>();
        }
    }
    String::new()
}

fn extract_topic_id(row: &ElementRef<'_>) -> Option<u64> {
    if let Some(raw) = row.value().attr("data-topic_id") {
        if let Ok(id) = raw.parse::<u64>() {
            return Some(id);
        }
    }
    let href_sel = Selector::parse(r#"a[href*="viewtopic.php?t="]"#).ok()?;
    let re = Regex::new(r"t=(\d+)").ok()?;
    for link in row.select(&href_sel) {
        if let Some(href) = link.value().attr("href") {
            if let Some(m) = re.captures(href).and_then(|c| c.get(1)) {
                if let Ok(id) = m.as_str().parse::<u64>() {
                    return Some(id);
                }
            }
        }
    }
    None
}

// Selector bundles (search-first, forum-fallback ordering).
// `Selector::parse` is infallible for these literals; unwrap is safe.

fn title_selectors() -> [Selector; 2] {
    [
        Selector::parse("a.tLink").unwrap(),
        Selector::parse("a.tt-text").unwrap(),
    ]
}

fn author_selectors() -> [Selector; 2] {
    [
        Selector::parse("td.u-name-col a").unwrap(),
        Selector::parse("a.topicAuthor").unwrap(),
    ]
}

fn size_selectors() -> [Selector; 3] {
    [
        Selector::parse("td.tor-size u").unwrap(),
        Selector::parse("td.tor-size").unwrap(),
        Selector::parse("a.f-dl").unwrap(),
    ]
}

fn seed_selectors() -> [Selector; 2] {
    // First entry mirrors the pre-refactor union literally so the search path is
    // byte-for-byte unchanged (including the known tor-size-td first-match quirk that yields
    // `seeds = 0` on row[0] of the search fixture — see `search-row-golden.json`).
    [
        Selector::parse("b.seedmed, td.seedmed, b.seed, td.seed, td.row4.nowrap").unwrap(),
        Selector::parse("span.seedmed").unwrap(),
    ]
}

fn leech_selectors() -> [Selector; 2] {
    // First entry mirrors the pre-refactor union literally; fallback for `viewforum.php`.
    [
        Selector::parse("td.leechmed, b.leechmed, td.leech, b.leech").unwrap(),
        Selector::parse("span.leechmed").unwrap(),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::forum_page::parse_forum_page;
    use encoding_rs::WINDOWS_1251;
    use pretty_assertions::assert_eq;
    use scraper::Html;

    const SEARCH_FIXTURE: &[u8] = include_bytes!("../tests/fixtures/forum-sample.html");
    const FORUM_FIXTURE: &[u8] = include_bytes!("../tests/fixtures/viewforum-sample.html");
    const GOLDEN: &str = include_str!("../tests/fixtures/search-row-golden.json");

    fn decode(bytes: &[u8]) -> String {
        let (cow, _, _) = WINDOWS_1251.decode(bytes);
        cow.into_owned()
    }

    #[test]
    fn test_parse_topic_row_matches_search_and_forum_fixtures() {
        // Search-page row: assert field-by-field equality with the pre-refactor golden.
        let html = decode(SEARCH_FIXTURE);
        let doc = Html::parse_document(&html);
        let row_sel = Selector::parse("table#tor-tbl tbody tr").unwrap();
        let row0 = doc
            .select(&row_sel)
            .next()
            .expect("at least one search row");
        let common = parse_topic_row(&row0).expect("search row parses");

        let expected: serde_json::Value = serde_json::from_str(GOLDEN).unwrap();
        assert_eq!(common.topic_id, expected["topic_id"].as_u64().unwrap());
        assert_eq!(common.title, expected["title"].as_str().unwrap());
        assert_eq!(common.author, expected["author"].as_str().unwrap());
        assert_eq!(common.size, expected["size"].as_str().unwrap());
        assert_eq!(common.seeds, expected["seeds"].as_u64().unwrap() as u32);
        assert_eq!(common.leeches, expected["leeches"].as_u64().unwrap() as u32);

        // Forum-page row: verify the same helper produces a non-empty RowCommon
        // (selector fallbacks fire correctly on `viewforum.php` markup).
        let forum_html = decode(FORUM_FIXTURE);
        let listing = parse_forum_page(&forum_html).expect("forum page parses");
        assert!(
            !listing.topics.is_empty(),
            "forum listing should contain rows"
        );
        let forum_row0 = &listing.topics[0];
        assert!(forum_row0.topic_id > 0);
        assert!(!forum_row0.title.is_empty());
        assert!(!forum_row0.author.is_empty());
        assert!(!forum_row0.size.is_empty());
        assert!(forum_row0.seeds > 0);
    }
}
