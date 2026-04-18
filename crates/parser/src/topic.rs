//! Topic page (viewtopic.php) parser — first post + comments + metadata.

use crate::metadata::parse_topic_metadata;
use crate::{Comment, Result, TopicDetails};
use regex::Regex;
use scraper::{ElementRef, Html, Selector};

pub fn parse_topic_page(html: &str) -> Result<TopicDetails> {
    let doc = Html::parse_document(html);

    let title_sel = Selector::parse("#topic-title").unwrap();
    let magnet_sel = Selector::parse("a.magnet-link").unwrap();
    // Try dedicated size span first; dl-stub label is a dead-last fallback.
    let size_humn_sel = Selector::parse("span#tor-size-humn").unwrap();
    let dl_stub_sel = Selector::parse("a.dl-stub").unwrap();
    // Seeds/leeches: the inner <b> holds the number; the outer span also contains
    // surrounding label text. Must prefer the inner <b>.
    let seed_b_sel = Selector::parse("span.seed b, span#seed-counter b").unwrap();
    let leech_b_sel = Selector::parse("span.leech b, span#leech-counter b").unwrap();
    let post_row_sel = Selector::parse("tbody[id^='post_']").unwrap();
    let post_body_sel = Selector::parse("div.post_body").unwrap();
    let canonical_sel = Selector::parse(r#"link[rel="canonical"]"#).unwrap();

    let title = doc
        .select(&title_sel)
        .next()
        .map(|e| e.text().collect::<String>().trim().to_string())
        .unwrap_or_default();

    let magnet_link = doc
        .select(&magnet_sel)
        .next()
        .and_then(|e| e.value().attr("href"))
        .unwrap_or("")
        .to_string();

    let size = doc
        .select(&size_humn_sel)
        .next()
        .map(|e| e.text().collect::<String>().trim().to_string())
        .or_else(|| {
            doc.select(&dl_stub_sel)
                .next()
                .map(|e| e.text().collect::<String>().trim().to_string())
        })
        .unwrap_or_default();

    let seeds = doc
        .select(&seed_b_sel)
        .next()
        .and_then(|e| e.text().collect::<String>().trim().parse::<u32>().ok())
        .unwrap_or(0);

    let leeches = doc
        .select(&leech_b_sel)
        .next()
        .and_then(|e| e.text().collect::<String>().trim().parse::<u32>().ok())
        .unwrap_or(0);

    let mut posts = doc.select(&post_row_sel);
    let opening = posts.next();

    // Opening post: description + metadata.
    let (description, metadata) = if let Some(op) = opening {
        let desc = op
            .select(&post_body_sel)
            .next()
            .map(|b| b.text().collect::<Vec<_>>().join("\n"))
            .unwrap_or_default();
        let meta = op.select(&post_body_sel).next().map(parse_topic_metadata);
        (desc, meta)
    } else {
        (String::new(), None)
    };

    // Remaining posts are comments.
    let mut comments = Vec::new();
    for post in posts {
        if let Some(c) = parse_comment(post) {
            comments.push(c);
        }
    }

    let comment_pages_total = count_topic_pages(&doc);

    let topic_id = doc
        .select(&canonical_sel)
        .next()
        .and_then(|e| e.value().attr("href"))
        .and_then(extract_topic_id)
        .unwrap_or(0);

    Ok(TopicDetails {
        topic_id,
        title,
        magnet_link,
        size,
        seeds,
        leeches,
        description,
        file_list: Vec::new(),
        metadata,
        comments,
        comment_pages_fetched: 1,
        comment_pages_total,
    })
}

fn parse_comment(post: ElementRef<'_>) -> Option<Comment> {
    let nick_sel = Selector::parse("p.nick").unwrap();
    let date_sel = Selector::parse("a.p-link.small, a.p-link, span.post-time a").unwrap();
    let body_sel = Selector::parse("div.post_body").unwrap();

    let id_attr = post.value().attr("id")?;
    let post_id = id_attr.strip_prefix("post_")?.parse::<u64>().ok()?;

    let author = post
        .select(&nick_sel)
        .next()
        .map(|e| e.text().collect::<String>().trim().to_string())
        .unwrap_or_default();

    let date = post
        .select(&date_sel)
        .next()
        .map(|e| e.text().collect::<String>().trim().to_string())
        .unwrap_or_default();

    let text = post
        .select(&body_sel)
        .next()
        .map(|e| e.text().collect::<Vec<_>>().join("\n"))
        .unwrap_or_default();

    Some(Comment {
        post_id,
        author,
        date,
        text,
    })
}

fn count_topic_pages(doc: &Html) -> u32 {
    let sel = Selector::parse("a.pg").unwrap();
    let re = Regex::new(r"start=(\d+)").unwrap();
    let mut max_start = 0u32;
    for link in doc.select(&sel) {
        let Some(href) = link.value().attr("href") else {
            continue;
        };
        if let Some(n) = re
            .captures(href)
            .and_then(|c| c.get(1))
            .and_then(|m| m.as_str().parse::<u32>().ok())
        {
            max_start = max_start.max(n);
        }
    }
    // rutracker paginates in 30-post chunks.
    max_start / 30 + 1
}

fn extract_topic_id(href: &str) -> Option<u64> {
    let re = Regex::new(r"t=(\d+)").ok()?;
    re.captures(href)?.get(1)?.as_str().parse::<u64>().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use encoding_rs::WINDOWS_1251;

    const TOPIC_FIXTURE: &[u8] = include_bytes!("../tests/fixtures/topic-sample.html");

    fn fixture_html() -> String {
        let (cow, _, _) = WINDOWS_1251.decode(TOPIC_FIXTURE);
        cow.into_owned()
    }

    #[test]
    fn test_topic_title_non_empty() {
        let td = parse_topic_page(&fixture_html()).unwrap();
        assert!(!td.title.is_empty(), "topic title must be non-empty");
    }

    #[test]
    fn test_topic_magnet_link_present() {
        let td = parse_topic_page(&fixture_html()).unwrap();
        assert!(
            td.magnet_link.starts_with("magnet:?"),
            "expected magnet URI, got {:?}",
            td.magnet_link
        );
    }

    #[test]
    fn test_topic_description_contains_label() {
        let td = parse_topic_page(&fixture_html()).unwrap();
        assert!(
            td.description.contains("Год выпуска"),
            "description should contain year-label, got prefix {:?}",
            &td.description.chars().take(80).collect::<String>()
        );
    }

    #[test]
    fn test_parse_comments_count_29() {
        let td = parse_topic_page(&fixture_html()).unwrap();
        assert_eq!(
            td.comments.len(),
            29,
            "topic-sample.html has 30 posts — opening + 29 comments"
        );
    }

    #[test]
    fn test_comment0_matches_expected_json() {
        let td = parse_topic_page(&fixture_html()).unwrap();
        let expected = include_str!("../tests/fixtures/topic-sample-expected.json");
        let expected_json: serde_json::Value = serde_json::from_str(expected).unwrap();
        let expected_author = expected_json["topic"]["first_comment"]["author"]
            .as_str()
            .unwrap();
        let expected_post_id = expected_json["topic"]["first_comment"]["post_id"]
            .as_u64()
            .unwrap();
        let expected_snippet = expected_json["topic"]["first_comment"]["text_snippet_80"]
            .as_str()
            .unwrap();
        let c0 = &td.comments[0];
        assert_eq!(c0.author, expected_author);
        assert_eq!(c0.post_id, expected_post_id);
        assert!(
            c0.text.contains(expected_snippet),
            "comment text should contain snippet; text = {:?}",
            c0.text
        );
    }

    #[test]
    fn test_metadata_populated() {
        let td = parse_topic_page(&fixture_html()).unwrap();
        let meta = td.metadata.expect("metadata should be populated");
        assert_eq!(meta.year, Some(2026));
        assert!(meta.kinopoisk_url.is_some());
    }
}
