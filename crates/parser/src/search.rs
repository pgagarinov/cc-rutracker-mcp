//! Search results (tracker.php) parser.
//!
//! The per-row field extraction (`topic_id`, `title`, `author`, `size`, `seeds`, `leeches`) lives
//! in [`crate::row::parse_topic_row`] so `viewforum.php` and `tracker.php` share one code path.
//! This module adds only the page-level chrome (row selector, `search_id`) and the
//! search-specific `category` column.

use crate::{row::parse_topic_row, Result, SearchPage, SearchResult};
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Error;
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

    /// US-008 coverage: a table with rows that carry no `topic_id` (neither
    /// `data-topic_id` nor a `viewtopic.php?t=` anchor) must be filtered out
    /// by the `let Some(common) = … else { continue; };` branch in
    /// `parse_search_page`. The returned page must have `results.len() == 0`
    /// and `search_id` must be `None` (no `a.pg` pagination links either).
    #[test]
    fn test_rows_without_topic_id_are_filtered_and_no_search_id() {
        let html = r#"<!DOCTYPE html>
<html><body>
<table id="tor-tbl"><tbody>
  <tr><td class="f-name-col"><a>Category</a></td><td><a href="http://other/unrelated">no id here</a></td></tr>
  <tr><td class="f-name-col"><a>X</a></td></tr>
</tbody></table>
</body></html>"#;
        let page = parse_search_page(html).unwrap();
        assert_eq!(
            page.results.len(),
            0,
            "rows without topic_id must be filtered out"
        );
        assert!(
            page.search_id.is_none(),
            "search_id must be None when there are no a.pg links"
        );
    }

    /// US-008 coverage: pagination `<a class="pg">` links without an `href`
    /// attribute must be skipped (the `let Some(href) = … else { continue };`
    /// branch). Likewise, pagination anchors whose hrefs lack `search_id=`
    /// must be tolerated (the end-of-loop `None` branch). We build a page with
    /// a single `<a class="pg">` that has no href and assert `search_id` is
    /// `None` rather than panicking or mis-extracting.
    #[test]
    fn test_pg_anchor_without_href_and_without_search_id_yields_none() {
        let html = r#"<!DOCTYPE html>
<html><body>
<table id="tor-tbl"><tbody></tbody></table>
<!-- anchor with no href — hits `let Some(href) = … else { continue; };` -->
<a class="pg">1</a>
<!-- anchor with href that has no search_id= — falls through to `None` -->
<a class="pg" href="tracker.php?start=50">2</a>
</body></html>"#;
        let page = parse_search_page(html).unwrap();
        assert!(
            page.search_id.is_none(),
            "search_id must be None when no a.pg href contains search_id="
        );
    }

    /// US-008 coverage: confirm `Error::MissingElement` remains displayable.
    /// Keeps the variant alive by constructing and asserting its Display.
    #[test]
    fn test_missing_element_error_display_contains_detail() {
        let e = Error::MissingElement("foo");
        let s = e.to_string();
        assert!(
            s.contains("foo"),
            "Display of MissingElement must include the field name, got: {s}"
        );
        assert!(
            s.contains("missing"),
            "Display of MissingElement must mention 'missing', got: {s}"
        );
    }
}
