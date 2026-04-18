//! Search results (tracker.php) parser.
//!
//! The per-row field extraction (`topic_id`, `title`, `author`, `size`, `seeds`, `leeches`) lives
//! in [`crate::row::parse_topic_row`] so `viewforum.php` and `tracker.php` share one code path.
//! This module adds only the page-level chrome (row selector, `search_id`) and the
//! search-specific `category` column.

use crate::{row::parse_topic_row, Error, Result, SearchPage, SearchResult};
use regex::Regex;
use scraper::{ElementRef, Html, Selector};

const PER_PAGE_DEFAULT: u32 = 50;

pub fn parse_search_page(html: &str) -> Result<SearchPage> {
    let doc = Html::parse_document(html);

    let tbody_sel = Selector::parse("table#tor-tbl tbody tr").unwrap();
    // Category cell: `td.f-name-col a` / `a.gen` (both match in current HTML).
    let cat_sel = Selector::parse("td.f-name-col a, a.gen").unwrap();
    let pg_sel = Selector::parse("a.pg").unwrap();

    let mut results = Vec::new();
    for row in doc.select(&tbody_sel) {
        let Some(common) = parse_topic_row(&row) else {
            continue;
        };
        let category = first_text(&row, &cat_sel).trim().to_string();

        results.push(SearchResult {
            topic_id: common.topic_id,
            title: common.title,
            size: common.size,
            seeds: common.seeds,
            leeches: common.leeches,
            author: common.author,
            category,
        });
    }

    let search_id = extract_search_id(&doc, &pg_sel);

    Ok(SearchPage {
        results,
        page: 1,
        per_page: PER_PAGE_DEFAULT,
        total_results: None,
        search_id,
    })
}

fn first_text(row: &ElementRef<'_>, sel: &Selector) -> String {
    row.select(sel)
        .next()
        .map(|e| e.text().collect::<String>())
        .unwrap_or_default()
}

fn extract_search_id(doc: &Html, pg_sel: &Selector) -> Option<String> {
    let re = Regex::new(r"search_id=([^&]+)").ok()?;
    for link in doc.select(pg_sel) {
        let Some(href) = link.value().attr("href") else {
            continue;
        };
        if let Some(m) = re.captures(href).and_then(|c| c.get(1)) {
            return Some(m.as_str().to_string());
        }
    }
    None
}

#[allow(dead_code)]
fn _unused_err_type_proof() -> Result<()> {
    Err(Error::MissingElement("placeholder"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use encoding_rs::WINDOWS_1251;

    const FORUM_FIXTURE: &[u8] = include_bytes!("../tests/fixtures/forum-sample.html");

    fn fixture_html() -> String {
        let (cow, _, _) = WINDOWS_1251.decode(FORUM_FIXTURE);
        cow.into_owned()
    }

    #[test]
    fn test_row_count_50_and_search_id_present() {
        let page = parse_search_page(&fixture_html()).unwrap();
        assert_eq!(
            page.results.len(),
            50,
            "expected 50 rows from forum-sample.html"
        );
        assert!(
            page.search_id.is_some(),
            "expected search_id from pagination links"
        );
    }

    /// Phase 1 regression test for the preexisting `a.u-link` author selector defect.
    /// Known-good first-row author is recorded in `tests/fixtures/topic-sample-expected.json`.
    #[test]
    fn test_row0_author_matches_known_value() {
        let page = parse_search_page(&fixture_html()).unwrap();
        let expected = include_str!("../tests/fixtures/topic-sample-expected.json");
        let expected_json: serde_json::Value = serde_json::from_str(expected).unwrap();
        let expected_author = expected_json["search"]["row0_author"].as_str().unwrap();
        assert_eq!(page.results[0].author, expected_author);
    }

    #[test]
    fn test_row0_title_non_empty() {
        let page = parse_search_page(&fixture_html()).unwrap();
        assert!(!page.results[0].title.is_empty());
    }

    #[test]
    fn test_row0_topic_id_nonzero() {
        let page = parse_search_page(&fixture_html()).unwrap();
        assert!(page.results[0].topic_id > 0);
    }
}
