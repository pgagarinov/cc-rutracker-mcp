//! Structured metadata extraction from the opening post of a topic.
//!
//! rutracker's TorrentPier theme renders field labels as `<span class="post-b">Label</span>`
//! with the value following as a text node. This module converts those label/value pairs
//! plus embedded IMDb/Kinopoisk anchors into a [`TopicMetadata`].

use crate::TopicMetadata;
use regex::Regex;
use scraper::{ElementRef, Selector};

pub fn parse_topic_metadata(op_body: ElementRef<'_>) -> TopicMetadata {
    let label_sel = Selector::parse("span.post-b").unwrap();
    let anchor_sel = Selector::parse("a[href]").unwrap();

    let mut meta = TopicMetadata::default();

    // Collect label -> value via adjacent text nodes.
    for label_el in op_body.select(&label_sel) {
        let label_raw = label_el.text().collect::<String>();
        let label = label_raw.trim();
        // Walk forward siblings, stopping when we hit another post-b span or a block break.
        let mut value = String::new();
        let mut node = label_el.next_sibling();
        while let Some(n) = node {
            if let Some(text) = n.value().as_text() {
                value.push_str(text);
            } else if let Some(elem) = n.value().as_element() {
                if elem.name() == "span" && elem.classes().any(|c| c == "post-b") {
                    break;
                }
                if elem.name() == "br" {
                    break;
                }
                // Include inline text from non-break elements (e.g. links/bold inside a field).
                if let Some(child_ref) = ElementRef::wrap(n) {
                    value.push_str(&child_ref.text().collect::<String>());
                }
            }
            node = n.next_sibling();
        }
        apply_field(&mut meta, label, value.trim_start_matches(':').trim());
    }

    // IMDb / Kinopoisk URL scan.
    for anchor in op_body.select(&anchor_sel) {
        let Some(href) = anchor.value().attr("href") else {
            continue;
        };
        if meta.imdb_url.is_none() && href.contains("imdb.com") {
            meta.imdb_url = Some(href.to_string());
        }
        if meta.kinopoisk_url.is_none() && href.contains("kinopoisk.ru") {
            meta.kinopoisk_url = Some(href.to_string());
        }
    }

    meta
}

fn apply_field(meta: &mut TopicMetadata, label: &str, value: &str) {
    let value = value.trim();
    if value.is_empty() {
        return;
    }
    match label {
        "Год выпуска" => {
            if let Some(year) = Regex::new(r"(\d{4})")
                .unwrap()
                .captures(value)
                .and_then(|c| c.get(1))
                .and_then(|m| m.as_str().parse::<u16>().ok())
            {
                meta.year = Some(year);
            }
        }
        "Страна" => {
            meta.countries = split_list(value);
        }
        "Жанр" => {
            meta.genres = split_list(value);
        }
        "Режиссер" | "Режиссёр" => {
            meta.director = value.to_string();
        }
        "В ролях" => {
            meta.cast = split_list(value);
        }
        "Продолжительность" => {
            meta.duration = value.to_string();
        }
        "Тип релиза" | "Качество видео" => {
            if meta.release_type.is_empty() {
                meta.release_type = value.to_string();
            }
        }
        "Видео" => {
            if meta.video.is_empty() {
                meta.video = value.to_string();
            }
        }
        _ => {
            // Аудио, Аудио 1, Аудио 2, ...
            if label == "Аудио" || label.starts_with("Аудио") {
                meta.audio.push(value.to_string());
            }
        }
    }
}

fn split_list(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use encoding_rs::WINDOWS_1251;
    use scraper::{Html, Selector};

    const TOPIC_FIXTURE: &[u8] = include_bytes!("../tests/fixtures/topic-sample.html");

    fn op_body_html() -> String {
        let (cow, _, _) = WINDOWS_1251.decode(TOPIC_FIXTURE);
        cow.into_owned()
    }

    fn meta_from_fixture() -> TopicMetadata {
        let html = op_body_html();
        let doc = Html::parse_document(&html);
        let sel = Selector::parse("tbody[id^='post_'] div.post_body").unwrap();
        let op = doc.select(&sel).next().expect("opening post body");
        parse_topic_metadata(op)
    }

    #[test]
    fn test_extracts_year_kp_video_audio() {
        let meta = meta_from_fixture();
        assert_eq!(meta.year, Some(2026), "year should be 2026");
        assert!(
            meta.kinopoisk_url.is_some(),
            "kinopoisk_url must be Some(…), got {:?}",
            meta.kinopoisk_url
        );
        assert!(
            !meta.video.is_empty(),
            "video string must be non-empty, got {:?}",
            meta.video
        );
        assert!(
            !meta.audio.is_empty(),
            "audio must have ≥1 entry, got {:?}",
            meta.audio
        );
    }

    #[test]
    fn test_extracts_duration_and_director() {
        let meta = meta_from_fixture();
        assert!(
            meta.duration.contains(':'),
            "duration should look like HH:MM:SS, got {:?}",
            meta.duration
        );
        assert!(
            !meta.director.is_empty(),
            "director should be non-empty, got {:?}",
            meta.director
        );
    }

    #[test]
    fn test_countries_and_genres_split() {
        let meta = meta_from_fixture();
        assert!(
            !meta.countries.is_empty(),
            "countries list should not be empty, got {:?}",
            meta.countries
        );
        assert!(
            !meta.genres.is_empty(),
            "genres list should not be empty, got {:?}",
            meta.genres
        );
    }

    /// US-008: anchors whose `href` attribute contains `imdb.com` populate
    /// `imdb_url` (L49). Covers the previously-only-kinopoisk branch. Also
    /// confirms that `<a>` tags without `href` are tolerated via the L46
    /// `else { continue; }` branch.
    #[test]
    fn test_imdb_url_populated_and_anchors_without_href_skipped() {
        let html = r#"<html><body><table><tbody id="post_1"><tr><td><div class="post_body">
<a name="bookmark"></a>
<a href="https://www.imdb.com/title/tt1234567/">IMDb</a>
<a href="https://rutracker.org/forum/viewtopic.php?t=1">other</a>
</div></td></tr></tbody></table></body></html>"#;
        let doc = Html::parse_document(html);
        let sel = Selector::parse("tbody[id^='post_'] div.post_body").unwrap();
        let op = doc.select(&sel).next().unwrap();
        let meta = parse_topic_metadata(op);
        assert_eq!(
            meta.imdb_url.as_deref(),
            Some("https://www.imdb.com/title/tt1234567/"),
            "imdb_url must be populated from the imdb.com anchor"
        );
        assert!(
            meta.kinopoisk_url.is_none(),
            "no kinopoisk.ru link => kinopoisk_url must be None"
        );
    }

    /// US-008: when the opening post body contains label-value pairs broken
    /// by a `<br>` tag, the walker must stop at the `<br>` (L31–L33 of
    /// metadata.rs). Assert that a second `post-b` label after a `<br>` is
    /// picked up cleanly.
    #[test]
    fn test_break_tag_terminates_value_capture() {
        let html = r#"<html><body><table><tbody id="post_1"><tr><td><div class="post_body">
<span class="post-b">Год выпуска</span> 2024<br>
<span class="post-b">Жанр</span> драма, триллер
</div></td></tr></tbody></table></body></html>"#;
        let doc = Html::parse_document(html);
        let sel = Selector::parse("tbody[id^='post_'] div.post_body").unwrap();
        let op = doc.select(&sel).next().unwrap();
        let meta = parse_topic_metadata(op);
        assert_eq!(
            meta.year,
            Some(2024),
            "year must be captured before the <br>"
        );
        assert_eq!(
            meta.genres,
            vec!["драма".to_string(), "триллер".to_string()],
            "second label after <br> must be captured independently"
        );
    }
}
