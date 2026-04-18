//! Search results (tracker.php) parser.

use crate::{Error, Result, SearchPage, SearchResult};
use regex::Regex;
use scraper::{ElementRef, Html, Selector};

const PER_PAGE_DEFAULT: u32 = 50;

pub fn parse_search_page(html: &str) -> Result<SearchPage> {
    let doc = Html::parse_document(html);

    let tbody_sel = Selector::parse("table#tor-tbl tbody tr").unwrap();
    let title_link_sel = Selector::parse("a.tLink").unwrap();
    let seed_sel =
        Selector::parse("b.seedmed, td.seedmed, b.seed, td.seed, td.row4.nowrap").unwrap();
    let leech_sel = Selector::parse("td.leechmed, b.leechmed, td.leech, b.leech").unwrap();
    let size_sel = Selector::parse("td.tor-size u, td.tor-size").unwrap();
    // Author cell: `td.u-name-col` holds `<div class="u-name"><a class="med ts-text">…</a></div>`.
    // The preexisting Python selector `a.u-link` was stale — this replacement is the Phase 1 fix.
    let author_sel = Selector::parse("td.u-name-col a").unwrap();
    // Category cell: `td.f-name-col a` / `a.gen` (both match in current HTML).
    let cat_sel = Selector::parse("td.f-name-col a, a.gen").unwrap();
    let pg_sel = Selector::parse("a.pg").unwrap();

    let mut results = Vec::new();
    for row in doc.select(&tbody_sel) {
        let Some(link) = row.select(&title_link_sel).next() else {
            continue;
        };
        let href = link.value().attr("href").unwrap_or("");
        let Some(topic_id) = extract_topic_id(href) else {
            continue;
        };

        let title = link.text().collect::<String>().trim().to_string();
        let size = first_text(&row, &size_sel).trim().to_string();
        let seeds = first_text(&row, &seed_sel)
            .trim()
            .parse::<u32>()
            .unwrap_or(0);
        let leeches = first_text(&row, &leech_sel)
            .trim()
            .parse::<u32>()
            .unwrap_or(0);
        let author = first_text(&row, &author_sel).trim().to_string();
        let category = first_text(&row, &cat_sel).trim().to_string();

        results.push(SearchResult {
            topic_id,
            title,
            size,
            seeds,
            leeches,
            author,
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

fn extract_topic_id(href: &str) -> Option<u64> {
    let re = Regex::new(r"t=(\d+)").ok()?;
    re.captures(href)?.get(1)?.as_str().parse::<u64>().ok()
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
