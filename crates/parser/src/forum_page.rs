//! `viewforum.php` listing parser.
//!
//! Produces a [`ForumListing`] of [`TopicRow`] entries. Unlike `tracker.php`, `viewforum.php`
//! exposes a last-post anchor (`viewtopic.php?p=<id>#<id>`) and a replies counter; the row
//! parser captures both.

use crate::{row::parse_topic_row, Error, ForumListing, Result, TopicRow};
use regex::Regex;
use scraper::{ElementRef, Html, Selector};

const SANITY_BODY_MIN_BYTES: usize = 1024;

pub fn parse_forum_page(html: &str) -> Result<ForumListing> {
    let doc = Html::parse_document(html);

    let forum_id = extract_forum_id(&doc).unwrap_or_default();
    let total_pages = extract_total_pages(&doc);

    // viewforum.php uses `<table class="vf-table vf-tor ...">` with no explicit <tbody>;
    // topic rows carry `class="hl-tr"` and a `data-topic_id` attribute.
    let row_sel = Selector::parse("table.vf-tor tr.hl-tr").unwrap();
    let mut topics = Vec::new();
    for row in doc.select(&row_sel) {
        let Some(common) = parse_topic_row(&row) else {
            continue;
        };
        let downloads = extract_downloads(&row);
        let reply_count = extract_reply_count(&row);
        let (last_post_id, last_post_at) = extract_last_post(&row);

        if last_post_id == 0 {
            continue;
        }

        topics.push(TopicRow {
            topic_id: common.topic_id,
            title: common.title,
            author: common.author,
            size: common.size,
            seeds: common.seeds,
            leeches: common.leeches,
            downloads,
            reply_count,
            last_post_id,
            last_post_at,
        });
    }

    // ParseSanity: forum page yielded 0 rows but the body is non-trivial → HTML shape likely
    // changed under us. §11 scenario 3 / M4 consumer. A legitimately empty forum would also
    // produce 0 rows, but such pages are a fraction of a KB — the size guard separates them
    // from the drift case.
    if topics.is_empty() && html.len() > SANITY_BODY_MIN_BYTES {
        return Err(Error::ParseSanityFailed(
            "viewforum.php returned 0 rows from a non-empty body",
        ));
    }

    Ok(ForumListing {
        forum_id,
        topics,
        total_pages,
    })
}

fn extract_forum_id(doc: &Html) -> Option<String> {
    // Canonical link: <link rel="canonical" href=".../viewforum.php?f=252">
    let canonical_sel = Selector::parse(r#"link[rel="canonical"]"#).ok()?;
    let re = Regex::new(r"[?&]f=(\d+)").ok()?;
    for link in doc.select(&canonical_sel) {
        if let Some(href) = link.value().attr("href") {
            if let Some(m) = re.captures(href).and_then(|c| c.get(1)) {
                return Some(m.as_str().to_string());
            }
        }
    }
    None
}

fn extract_total_pages(doc: &Html) -> usize {
    // Pagination block links look like `viewforum.php?f=252&start=500`. Max `start / per_page + 1`.
    let link_sel = Selector::parse(r#"a[href*="viewforum.php"]"#).ok();
    let Some(sel) = link_sel else {
        return 1;
    };
    let re = Regex::new(r"[?&]start=(\d+)").unwrap();
    let mut max_start: u64 = 0;
    for link in doc.select(&sel) {
        if let Some(href) = link.value().attr("href") {
            if let Some(m) = re.captures(href).and_then(|c| c.get(1)) {
                if let Ok(n) = m.as_str().parse::<u64>() {
                    if n > max_start {
                        max_start = n;
                    }
                }
            }
        }
    }
    if max_start == 0 {
        1
    } else {
        // rutracker default page size is 50.
        (max_start as usize / 50) + 1
    }
}

fn extract_downloads(row: &ElementRef<'_>) -> u32 {
    // Forum layout: <td class="vf-col-replies"> … <b>42,348</b></td>
    // Search layout: <td class="row4 small number-format">831</td>
    let selectors = [
        Selector::parse("td.vf-col-replies b").unwrap(),
        Selector::parse("td.number-format").unwrap(),
    ];
    for sel in &selectors {
        if let Some(el) = row.select(sel).next() {
            let raw = el.text().collect::<String>();
            return raw
                .trim()
                .replace([',', '\u{00A0}'], "")
                .parse::<u32>()
                .unwrap_or(0);
        }
    }
    0
}

fn extract_reply_count(row: &ElementRef<'_>) -> u32 {
    // <td class="vf-col-replies"><p><span title="Ответов">107</span></p>…</td>
    let sel = Selector::parse("td.vf-col-replies span").unwrap();
    if let Some(el) = row.select(&sel).next() {
        return el
            .text()
            .collect::<String>()
            .trim()
            .replace(',', "")
            .parse::<u32>()
            .unwrap_or(0);
    }
    0
}

fn extract_last_post(row: &ElementRef<'_>) -> (u64, String) {
    // <td class="vf-col-last-post">
    //   <p>2026-04-18 18:50</p>
    //   <p><a …>kiksu</a><a href="viewtopic.php?p=89084698#89084698">…</a></p>
    // </td>
    let cell_sel = Selector::parse("td.vf-col-last-post").unwrap();
    let anchor_sel = Selector::parse(r#"a[href*="viewtopic.php?p="]"#).unwrap();
    let date_sel = Selector::parse("p").unwrap();
    let re = Regex::new(r"[?&]p=(\d+)").unwrap();

    let Some(cell) = row.select(&cell_sel).next() else {
        return (0, String::new());
    };

    let post_id = cell
        .select(&anchor_sel)
        .next()
        .and_then(|a| a.value().attr("href"))
        .and_then(|href| re.captures(href).and_then(|c| c.get(1)))
        .and_then(|m| m.as_str().parse::<u64>().ok())
        .unwrap_or(0);

    let date = cell
        .select(&date_sel)
        .next()
        .map(|p| p.text().collect::<String>().trim().to_string())
        .unwrap_or_default();

    (post_id, date)
}

#[cfg(test)]
mod tests {
    use super::*;
    use encoding_rs::WINDOWS_1251;

    const FORUM_FIXTURE: &[u8] = include_bytes!("../tests/fixtures/viewforum-sample.html");

    fn decode(bytes: &[u8]) -> String {
        let (cow, _, _) = WINDOWS_1251.decode(bytes);
        cow.into_owned()
    }

    #[test]
    fn test_rows_and_last_post_ids() {
        let listing = parse_forum_page(&decode(FORUM_FIXTURE)).unwrap();
        assert_eq!(listing.forum_id, "252");
        assert!(
            listing.topics.len() >= 30,
            "expected >= 30 rows, got {}",
            listing.topics.len()
        );
        for row in &listing.topics {
            assert!(
                row.last_post_id > 0,
                "topic {} has zero last_post_id",
                row.topic_id
            );
        }
    }

    /// US-008: a forum row with a valid `topic_id` but whose `last_post`
    /// anchor is missing or malformed (yielding `last_post_id == 0`) must
    /// be skipped (L31–L33). Build a minimal `vf-tor` table with one
    /// satisfying row (good `data-topic_id` + proper `viewtopic.php?p=`
    /// anchor) and one row that has no `vf-col-last-post` cell at all —
    /// the second row must NOT appear in `topics`.
    #[test]
    fn test_forum_row_without_last_post_is_skipped() {
        // Padding >1KiB to bypass the parse-sanity check.
        let padding = "x".repeat(2048);
        let html = format!(
            r#"<!DOCTYPE html>
<html><head>
<link rel="canonical" href="https://rutracker.org/forum/viewforum.php?f=1">
<title>padding {padding}</title>
</head><body>
<table class="vf-tor vf-table">
  <tr class="hl-tr" data-topic_id="111">
    <td><a class="tt-text">Good row</a></td>
    <td class="u-name-col"><a>alice</a></td>
    <td class="tor-size"><u>1</u> GB</td>
    <td><b class="seedmed">10</b></td>
    <td class="leechmed">1</td>
    <td class="vf-col-last-post">
      <p>2026-04-18 18:50</p>
      <p><a href="viewtopic.php?p=12345#12345">x</a></p>
    </td>
    <td class="vf-col-replies"><p><span title="Ответов">5</span></p><b>100</b></td>
  </tr>
  <tr class="hl-tr" data-topic_id="222">
    <td><a class="tt-text">Row with no last-post cell</a></td>
    <td class="u-name-col"><a>bob</a></td>
    <td class="tor-size"><u>2</u> GB</td>
    <td><b class="seedmed">5</b></td>
    <td class="leechmed">1</td>
    <!-- no vf-col-last-post td — extract_last_post returns (0, "") -->
    <td class="vf-col-replies"><p><span title="Ответов">5</span></p><b>100</b></td>
  </tr>
</table>
</body></html>"#
        );
        let listing = parse_forum_page(&html).unwrap();
        // Only the good row (last_post_id = 12345) should be kept.
        let ids: Vec<u64> = listing.topics.iter().map(|t| t.topic_id).collect();
        assert_eq!(
            ids,
            vec![111u64],
            "row with missing vf-col-last-post must be filtered"
        );
        assert_eq!(listing.topics[0].last_post_id, 12345);
    }

    /// US-008: a forum row where `parse_topic_row` returns `None` (no
    /// `data-topic_id` and no `viewtopic.php?t=` anchor) must be filtered by
    /// the `let Some(common) = … else { continue; };` branch (L24–L25).
    #[test]
    fn test_forum_row_without_topic_id_is_skipped() {
        let padding = "x".repeat(2048);
        let html = format!(
            r#"<!DOCTYPE html>
<html><head>
<link rel="canonical" href="https://rutracker.org/forum/viewforum.php?f=1">
<title>padding {padding}</title>
</head><body>
<table class="vf-tor vf-table">
  <tr class="hl-tr" data-topic_id="111">
    <td><a class="tt-text">keeper</a></td>
    <td class="u-name-col"><a>a</a></td>
    <td class="tor-size"><u>1</u> GB</td>
    <td><b class="seedmed">1</b></td>
    <td class="leechmed">0</td>
    <td class="vf-col-last-post">
      <p>2026-04-18</p>
      <p><a href="viewtopic.php?p=9#9">x</a></p>
    </td>
    <td class="vf-col-replies"><span title="Ответов">1</span><b>1</b></td>
  </tr>
  <tr class="hl-tr">
    <!-- no data-topic_id, no viewtopic.php?t= anchor anywhere -->
    <td><a class="tt-text">skip me</a></td>
    <td class="u-name-col"><a>b</a></td>
    <td class="tor-size"><u>2</u> GB</td>
    <td><b class="seedmed">2</b></td>
    <td class="leechmed">0</td>
    <td class="vf-col-last-post">
      <p>2026-04-18</p>
      <p><a href="viewtopic.php?p=99#99">x</a></p>
    </td>
  </tr>
</table>
</body></html>"#
        );
        let listing = parse_forum_page(&html).unwrap();
        assert_eq!(listing.topics.len(), 1);
        assert_eq!(listing.topics[0].topic_id, 111);
    }

    /// US-008: a page with no `viewforum.php?start=N` pagination anchors
    /// must return `total_pages = 1` (L99–L101). A single-row listing with
    /// no pagination block exercises this.
    #[test]
    fn test_forum_without_pagination_anchors_yields_total_pages_1() {
        let padding = "x".repeat(2048);
        let html = format!(
            r#"<!DOCTYPE html>
<html><head>
<link rel="canonical" href="https://rutracker.org/forum/viewforum.php?f=7">
<title>padding {padding}</title>
</head><body>
<table class="vf-tor vf-table">
  <tr class="hl-tr" data-topic_id="500">
    <td><a class="tt-text">only row</a></td>
    <td class="u-name-col"><a>c</a></td>
    <td class="tor-size"><u>3</u> GB</td>
    <td><b class="seedmed">7</b></td>
    <td class="leechmed">0</td>
    <td class="vf-col-last-post">
      <p>2026-04-18</p>
      <p><a href="viewtopic.php?p=500#500">x</a></p>
    </td>
  </tr>
</table>
</body></html>"#
        );
        let listing = parse_forum_page(&html).unwrap();
        assert_eq!(listing.total_pages, 1, "no pagination => total_pages = 1");
        assert_eq!(listing.forum_id, "7");
    }

    #[test]
    fn test_empty_listing_raises_sanity_error() {
        // Build a document with the expected chrome but an empty tor-tbl tbody. Body size
        // must exceed the 1 KB sanity threshold — the padding string keeps it well above.
        let padding = "x".repeat(2048);
        let html = format!(
            r#"<!DOCTYPE html>
<html><head>
<link rel="canonical" href="https://rutracker.org/forum/viewforum.php?f=252">
<title>padding {padding}</title>
</head><body>
<table id="tor-tbl"><tbody></tbody></table>
</body></html>"#
        );
        let err = parse_forum_page(&html).expect_err("should fail sanity");
        assert!(matches!(err, Error::ParseSanityFailed(_)));
    }
}
