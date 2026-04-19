//! Per-topic rip metadata extracted for the rip scorer.
//!
//! In Phase R1 the [`rutracker_mirror::topic_io::TopicFile`] struct does *not*
//! yet persist seeds / leeches / downloads / size fields — those land in
//! Phase R2 (US-002). This module compiles against the current `TopicFile`
//! shape by treating every such field as `None`; the `format_tag` is pulled
//! from the already-parsed title (plan §2.3 + US-002's MINOR-2 fix).

use rutracker_mirror::topic_io::TopicFile;

use crate::title::ParsedTitle;

/// Rip-level numbers the aggregator needs to rank releases within a film.
/// Every field is optional since older mirror JSONs predate the extended
/// schema.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RipMetadata {
    pub seeds: Option<u32>,
    pub leeches: Option<u32>,
    pub downloads: Option<u32>,
    pub size_bytes: Option<u64>,
    pub format_tag: Option<String>,
    pub fetched_at: Option<String>,
    /// Raw dub/voiceover metadata from the parsed title (e.g. `"Dub"`,
    /// `"MVO, Sub"`, `"Dub | Dub"`). Used by the rip ranker's audio-preference
    /// axis. `None` when the parsed title carried no post-bracket suffix.
    pub dub_info: Option<String>,
}

impl RipMetadata {
    /// Build the metadata record from a topic file + its parsed title.
    ///
    /// In Phase R1, `TopicFile` carries no seeds/leeches/downloads/size
    /// columns, so the only non-`None` outputs here are `format_tag`
    /// (from the parsed title) and `fetched_at` (copied verbatim). US-002
    /// extends `TopicFile` and this function will pick the new fields up
    /// with no call-site change.
    pub fn from_topic_file(topic_file: &TopicFile, parsed: &ParsedTitle) -> Self {
        Self {
            seeds: None,
            leeches: None,
            downloads: None,
            size_bytes: None,
            format_tag: Some(parsed.format.clone()).filter(|s| !s.is_empty()),
            fetched_at: Some(topic_file.fetched_at.clone()).filter(|s| !s.is_empty()),
            dub_info: Some(parsed.dub_info.clone()).filter(|s| !s.is_empty()),
        }
    }
}

/// Parse a rutracker size string like `"2.22 GB"`, `"1,46 Гбайт"`, `"502 Мбайт"`
/// into bytes. Accepts `.` or `,` as decimal, spaces as thousands separator,
/// and the English units `KB/MB/GB/TB` plus Russian `Кбайт/Мбайт/Гбайт/Тбайт`.
/// Returns `None` if the string is empty or cannot be parsed.
pub fn parse_size_bytes(s: &str) -> Option<u64> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }

    // Locate the first character that is neither digit, decimal nor whitespace.
    let (num_part, unit_part) = split_num_unit(s);
    let cleaned: String = num_part
        .chars()
        .filter(|c| !c.is_whitespace())
        .map(|c| if c == ',' { '.' } else { c })
        .collect();
    let value: f64 = cleaned.parse().ok()?;

    let unit = unit_part.trim().to_lowercase();
    let mult: f64 = match unit.as_str() {
        "b" | "байт" | "bytes" => 1.0,
        "kb" | "кб" | "кбайт" | "kib" => 1024.0,
        "mb" | "мб" | "мбайт" | "mib" => 1024.0 * 1024.0,
        "gb" | "гб" | "гбайт" | "gib" => 1024.0 * 1024.0 * 1024.0,
        "tb" | "тб" | "тбайт" | "tib" => 1024.0_f64.powi(4),
        _ => return None,
    };
    Some((value * mult) as u64)
}

/// Split a string of the shape `<number><ws><unit>` into `(number_str, unit_str)`.
/// The split point is the first alphabetic char after any digit / sep.
fn split_num_unit(s: &str) -> (&str, &str) {
    let idx = s
        .char_indices()
        .find(|(_, c)| c.is_alphabetic())
        .map(|(i, _)| i)
        .unwrap_or(s.len());
    (&s[..idx], &s[idx..])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::title::parse_title;

    fn sample_topic_file() -> TopicFile {
        TopicFile {
            schema_version: 1,
            topic_id: "6843582".to_string(),
            forum_id: "252".to_string(),
            title: "some title".to_string(),
            fetched_at: "2026-04-18T20:30:44.785551+00:00".to_string(),
            last_post_id: 0,
            last_post_at: String::new(),
            opening_post: Default::default(),
            comments: vec![],
            metadata: serde_json::Value::Null,
            size_bytes: None,
            seeds: None,
            leeches: None,
            downloads: None,
        }
    }

    #[test]
    fn test_rip_metadata_extracts_format_and_fetched_at() {
        let tf = sample_topic_file();
        let parsed =
            parse_title("Foo / Foo En (Director) [2026, США, драма, WEB-DLRip-AVC] Dub").unwrap();
        let rm = RipMetadata::from_topic_file(&tf, &parsed);
        assert_eq!(rm.format_tag.as_deref(), Some("WEB-DLRip-AVC"));
        assert_eq!(
            rm.fetched_at.as_deref(),
            Some("2026-04-18T20:30:44.785551+00:00")
        );
        assert_eq!(rm.dub_info.as_deref(), Some("Dub"));
        // US-002 will add these; Phase R1 leaves them None.
        assert_eq!(rm.seeds, None);
        assert_eq!(rm.leeches, None);
        assert_eq!(rm.downloads, None);
        assert_eq!(rm.size_bytes, None);
    }

    #[test]
    fn test_parse_size_bytes_gb_en() {
        let b = parse_size_bytes("2.22 GB").unwrap();
        // 2.22 * 1024^3 ≈ 2_384_032_235 (integer truncation of the float).
        assert!(
            (2_380_000_000..=2_390_000_000).contains(&b),
            "unexpected byte count: {b}"
        );
    }

    #[test]
    fn test_parse_size_bytes_ru() {
        let b = parse_size_bytes("1,46 Гбайт").unwrap();
        assert!((1_560_000_000..=1_580_000_000).contains(&b), "got {b}");

        let mb = parse_size_bytes("502 Мбайт").unwrap();
        assert_eq!(mb, 502 * 1024 * 1024);
    }

    #[test]
    fn test_parse_size_bytes_returns_none_on_junk() {
        assert_eq!(parse_size_bytes(""), None);
        assert_eq!(parse_size_bytes("not a size"), None);
    }
}
