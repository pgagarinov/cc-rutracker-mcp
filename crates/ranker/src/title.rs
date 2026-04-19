//! Rutracker title parser + deterministic film identity.
//!
//! Rutracker release titles follow a predictable template (plan §2.1):
//!
//! ```text
//! {ru_title} / {en_title}[/ {alt_title}] ({directors…}) [{year}, {countries}, {genres}, …, {format}] {dub_info}
//! ```
//!
//! Unparseable titles surface as [`TitleParseError`] — never a panic. The
//! film-identity pair `film_key` + `film_id` (plan §2.2) uses an ASCII `\x1f`
//! unit-separator so that titles containing a literal `|` still get distinct
//! ids when they differ in any of the four key fields.
//!
//! No fuzzy matching in v1: two titles match iff all of
//! (normalised ru title, normalised en title, year, normalised director)
//! agree exactly. See `test_film_key_tolerates_pipe_in_title` for the
//! adversarial case.

use regex::Regex;
use sha2::{Digest, Sha256};
use std::sync::OnceLock;
use thiserror::Error;

/// Byte used to glue `film_key` fields. Unit-separator (`\x1f`) cannot occur in
/// a normalised rutracker title, so it is collision-free even when titles
/// contain `|`, `/` or other punctuation.
pub const KEY_SEP: char = '\u{1f}';

/// Recognised rip-format tokens, in match-priority order. Longer / more
/// specific tokens go first so `WEB-DLRip-AVC` beats `WEB-DLRip`.
const FORMAT_TOKENS: &[&str] = &[
    "WEB-DLRip-AVC",
    "WEB-DLRip",
    "HDTVRip",
    "UHDRip",
    "BDRip",
    "HDRip",
    "WEBRip-AVC",
    "WEBRip",
    "DVDRip",
    "CAMRip",
    "HEVC",
    "CAM",
    "TS",
];

/// Structured view of a parsed rutracker title.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedTitle {
    pub title_ru: String,
    pub title_en: Option<String>,
    pub title_alt: Option<String>,
    pub director: Option<String>,
    pub year: Option<u16>,
    pub countries: Vec<String>,
    pub genres: Vec<String>,
    pub format: String,
    pub dub_info: String,
}

/// Errors emitted by [`parse_title`]. A single catch-all variant carries both
/// the offending input and a short diagnostic; downstream code logs these but
/// never panics.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum TitleParseError {
    #[error("title is empty")]
    Empty,

    #[error("no `[…]` bracketed metadata block found in title: {0:?}")]
    NoBracketBlock(String),

    #[error("bracketed block has fewer than the required fields (year, country, genre, …, format): {0:?}")]
    TooFewBracketFields(String),

    #[error("ru title part is missing (no text before the first ` / ` separator): {0:?}")]
    MissingRuTitle(String),
}

/// Parse one rutracker release title into its structured fields. Returns
/// [`TitleParseError`] for titles that do not match the template; never panics.
pub fn parse_title(input: &str) -> Result<ParsedTitle, TitleParseError> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err(TitleParseError::Empty);
    }

    // Split off the bracketed `[…]` metadata block — it's the pivot for
    // splitting the title half from the dub/release suffix.
    let (pre_bracket, bracket, post_bracket) = split_bracket_block(trimmed)
        .ok_or_else(|| TitleParseError::NoBracketBlock(trimmed.to_string()))?;

    // Everything before the `(directors)` parenthesis is the title chain.
    // The `(…)` group and anything between it and `[…]` is the director list.
    let (titles_part, director_part) = split_directors(pre_bracket);

    let titles = split_title_chain(titles_part);
    if titles.is_empty() || titles[0].trim().is_empty() {
        return Err(TitleParseError::MissingRuTitle(trimmed.to_string()));
    }
    let title_ru = titles[0].trim().to_string();
    let title_en = titles
        .get(1)
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let title_alt = titles
        .get(2)
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());

    let director = director_part
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());

    // Bracket block fields: year, countries..., genres..., format(s).
    // Rutracker uses `, ` as the separator. `year` is always first; `format` is
    // the last token matching one of our known rip tokens. Everything in
    // between that parses as a country or genre is treated as countries/genres.
    let fields: Vec<String> = bracket
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();

    if fields.len() < 2 {
        return Err(TitleParseError::TooFewBracketFields(trimmed.to_string()));
    }

    let year = fields[0]
        .chars()
        .take(4)
        .collect::<String>()
        .parse::<u16>()
        .ok();

    // Find the last field that matches a known format token (case-sensitive
    // against the canonical list). If none found, fall back to the last field.
    let format_idx = (1..fields.len())
        .rev()
        .find(|&i| FORMAT_TOKENS.iter().any(|tok| fields[i] == *tok))
        .unwrap_or(fields.len() - 1);
    let format = fields[format_idx].clone();

    // Country / genre split: rutracker always lists countries before genres.
    // We use a short whitelist of country names; everything else in the gap
    // between year and format is a genre.
    let (countries, genres) = split_countries_genres(&fields[1..format_idx]);

    let dub_info = post_bracket.trim().to_string();

    Ok(ParsedTitle {
        title_ru,
        title_en,
        title_alt,
        director,
        year,
        countries,
        genres,
        format,
        dub_info,
    })
}

/// Build the deterministic identity key for a parsed title. Fields are joined
/// with [`KEY_SEP`] (unit separator, `\x1f`). Empty fields are emitted as empty
/// strings so position remains stable.
pub fn film_key(p: &ParsedTitle) -> String {
    let ru = normalize(&p.title_ru);
    let en = p.title_en.as_deref().map(normalize).unwrap_or_default();
    let yr = p.year.map(|y| y.to_string()).unwrap_or_default();
    let dir = p.director.as_deref().map(normalize).unwrap_or_default();
    format!("{ru}{KEY_SEP}{en}{KEY_SEP}{yr}{KEY_SEP}{dir}")
}

/// First 16 hex chars of SHA-256 of `key`. Stable, deterministic, not
/// cryptographically sensitive — only a collision bucket.
pub fn film_id(key: &str) -> String {
    let mut h = Sha256::new();
    h.update(key.as_bytes());
    let digest = h.finalize();
    let hex = digest
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();
    hex[..16].to_string()
}

/// Trim, collapse internal whitespace, and (optionally) case-fold. The core
/// loop is shared by [`normalize`] (with lowercasing) and
/// [`crate::aggregator::normalize_theme`] (also with lowercasing). Both callers
/// pass `to_lowercase()` output in, so the shared helper just trims and
/// collapses whitespace.
pub(crate) fn normalize_ws(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_ws = false;
    for ch in s.trim().chars() {
        if ch.is_whitespace() {
            if !prev_ws {
                out.push(' ');
            }
            prev_ws = true;
        } else {
            out.push(ch);
            prev_ws = false;
        }
    }
    out
}

/// Case-fold, trim, collapse internal whitespace. Used only for equality keys
/// (`film_key`), never for display.
fn normalize(s: &str) -> String {
    normalize_ws(&s.to_lowercase())
}

/// Split `Ru / En [/ Alt]` into up to 3 title segments.
fn split_title_chain(s: &str) -> Vec<String> {
    // The separator is ` / ` (space-slash-space) to avoid slashes inside tokens
    // like URLs. We allow up to 3 pieces; anything further is merged into alt.
    let parts: Vec<&str> = s.split(" / ").collect();
    if parts.len() <= 3 {
        parts.into_iter().map(|p| p.to_string()).collect()
    } else {
        let (first_two, rest) = parts.split_at(2);
        let mut out: Vec<String> = first_two.iter().map(|s| s.to_string()).collect();
        out.push(rest.join(" / "));
        out
    }
}

/// Splits `… titles … (directors)` into `(titles, Some(directors))`.
/// Returns `(input, None)` when there is no parenthesis group.
fn split_directors(s: &str) -> (&str, Option<&str>) {
    // Find the last `(…)` — rutracker can put role parentheses inside title
    // fields, so we want the rightmost group which should be the director one.
    if let Some(open) = s.rfind('(') {
        if let Some(close_rel) = s[open..].rfind(')') {
            let close = open + close_rel;
            if close > open + 1 {
                let titles = s[..open].trim_end();
                let directors = &s[open + 1..close];
                return (titles, Some(directors));
            }
        }
    }
    (s, None)
}

/// Extract `[…]` block. Returns `(pre, inside, post)`.
fn split_bracket_block(s: &str) -> Option<(&str, &str, &str)> {
    let open = s.find('[')?;
    // Find matching close bracket (rutracker does not nest `[`).
    let close_rel = s[open..].find(']')?;
    let close = open + close_rel;
    if close <= open {
        return None;
    }
    let pre = s[..open].trim_end();
    let inside = &s[open + 1..close];
    let post = &s[close + 1..];
    Some((pre, inside, post))
}

/// Split the flat middle region of the bracket block into
/// (countries, genres). Rutracker puts countries before genres. We use a
/// small country whitelist; any token matching it (prefix-insensitive) is a
/// country, everything else becomes a genre.
fn split_countries_genres(middle: &[String]) -> (Vec<String>, Vec<String>) {
    static COUNTRIES: OnceLock<Regex> = OnceLock::new();
    let re = COUNTRIES.get_or_init(|| {
        // A tiny allow-list of the most common country names appearing first in
        // rutracker bracket blocks. Parser stays v1; a follow-up can expand it.
        Regex::new(concat!(
            r"(?i)^(США|Россия|СССР|Великобритания|Канада|Германия|Франция|",
            r"Италия|Испания|Япония|Китай|Южная Корея|Индия|Австралия|",
            r"Нидерланды|Швеция|Норвегия|Дания|Финляндия|Польша|Бельгия|",
            r"Чехия|Мексика|Бразилия|Аргентина|Ирландия|Новая Зеландия|",
            r"Гонконг|Тайвань|Турция|Иран|ЮАР|Израиль|Украина|Беларусь)$"
        ))
        .expect("country regex is well-formed")
    });

    let mut countries = Vec::new();
    let mut genres = Vec::new();
    let mut in_country_zone = true;
    for token in middle {
        if in_country_zone && re.is_match(token) {
            countries.push(token.clone());
        } else {
            in_country_zone = false;
            genres.push(token.clone());
        }
    }
    (countries, genres)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    // Real titles copied verbatim from the local mirror (forum 252).
    const TITLE_2026_HAIL_MARY: &str = "Проект «Конец света» / Project Hail Mary (Фил Лорд / Phil Lord, Кристофер Миллер / Christopher Miller) [2026, США, фантастика, триллер, драма, WEBRip-AVC] Dub (MovieDalen)";

    const TITLE_VACATION_WEBDLRIP: &str = "Отпуск на двоих / People We Meet on Vacation (Бретт Хейли / Brett Haley) [2026, США, мелодрама, комедия, WEB-DLRip] Dub (Videofilm Ltd.)";

    const TITLE_VACATION_WEBDLRIP_AVC: &str = "Отпуск на двоих / People We Meet on Vacation (Бретт Хейли / Brett Haley) [2026, США, мелодрама, комедия, WEB-DLRip-AVC] Dub (Videofilm Ltd.)";

    #[test]
    fn test_parse_standard_2026_title() {
        let p = parse_title(TITLE_2026_HAIL_MARY).expect("should parse");
        assert_eq!(p.title_ru, "Проект «Конец света»");
        assert_eq!(p.title_en.as_deref(), Some("Project Hail Mary"));
        assert_eq!(p.year, Some(2026));
        assert!(
            p.director.as_deref().unwrap_or("").contains("Фил Лорд"),
            "director should contain 'Фил Лорд', got {:?}",
            p.director
        );
        assert_eq!(p.format, "WEBRip-AVC");
        assert_eq!(p.countries, vec!["США".to_string()]);
        assert!(
            p.genres.iter().any(|g| g == "фантастика"),
            "genres should include 'фантастика', got {:?}",
            p.genres
        );
    }

    #[test]
    fn test_film_key_groups_rips_of_same_film() {
        let a = parse_title(TITLE_VACATION_WEBDLRIP).unwrap();
        let b = parse_title(TITLE_VACATION_WEBDLRIP_AVC).unwrap();

        assert_eq!(
            film_id(&film_key(&a)),
            film_id(&film_key(&b)),
            "same film with different rip format must share film_id"
        );

        // Different year → different film.
        let mut c = a.clone();
        c.year = Some(2019);
        assert_ne!(
            film_id(&film_key(&a)),
            film_id(&film_key(&c)),
            "year change must produce a distinct film_id"
        );
    }

    #[test]
    fn test_film_key_tolerates_pipe_in_title() {
        // Titles with a literal '|' must not collide just because '|' used to
        // be the key separator. Plan §2.2 — the key uses `\x1f` now.
        let a = ParsedTitle {
            title_ru: "First|Last".to_string(),
            title_en: Some("Alpha".to_string()),
            title_alt: None,
            director: Some("Someone".to_string()),
            year: Some(2020),
            countries: vec![],
            genres: vec![],
            format: "WEBRip".to_string(),
            dub_info: String::new(),
        };
        let b = ParsedTitle {
            title_ru: "First".to_string(),
            title_en: Some("Last|Alpha".to_string()),
            title_alt: None,
            director: Some("Someone".to_string()),
            year: Some(2020),
            countries: vec![],
            genres: vec![],
            format: "WEBRip".to_string(),
            dub_info: String::new(),
        };
        // If we had used `|` as the separator, both keys would collapse to
        // "first|last|alpha|…|2020|someone" identically. With `\x1f`, they
        // differ.
        assert_ne!(film_key(&a), film_key(&b));
        assert_ne!(film_id(&film_key(&a)), film_id(&film_key(&b)));
    }

    #[test]
    fn test_parse_failures_are_logged_not_fatal() {
        // Empty input → structured error, no panic.
        let err = parse_title("").unwrap_err();
        assert!(matches!(err, TitleParseError::Empty));

        // No bracket block → structured error.
        let err = parse_title("Some film without metadata").unwrap_err();
        assert!(matches!(err, TitleParseError::NoBracketBlock(_)));

        // Bracket block too thin → structured error.
        let err = parse_title("Film / Film2 [2020]").unwrap_err();
        assert!(matches!(err, TitleParseError::TooFewBracketFields(_)));
    }

    /// US-008: a title with 4+ ` / ` title segments must have the trailing
    /// segments merged into the third (alt) position — covers the
    /// `split_at(2)` + `rest.join(" / ")` branch at L222–L226.
    #[test]
    fn test_split_title_chain_merges_extra_segments_into_alt() {
        // Four title parts: first two are ru/en, remainder joins into alt.
        let p = parse_title(
            "Русское / English / Alt One / Alt Two (Director) [2020, США, драма, WEBRip] Dub",
        )
        .expect("should parse");
        assert_eq!(p.title_ru, "Русское");
        assert_eq!(p.title_en.as_deref(), Some("English"));
        assert_eq!(
            p.title_alt.as_deref(),
            Some("Alt One / Alt Two"),
            "third slot must absorb everything past the second ` / `"
        );
    }

    /// US-008: a title containing `[` but no matching `]` is treated as
    /// having no bracket block — covers the `s[open..].find(']')?` (L252)
    /// returning None.
    #[test]
    fn test_title_with_unclosed_bracket_errors_with_no_bracket_block() {
        let err =
            parse_title("Russian / English (Director) [2020, США, драма, WEBRip").unwrap_err();
        assert!(
            matches!(err, TitleParseError::NoBracketBlock(_)),
            "unclosed bracket must surface as NoBracketBlock, got: {err:?}"
        );
    }

    /// US-008: a bracket block whose directors group is malformed (e.g.
    /// mis-matched parens — `)` before `(`) must fall through to the
    /// "no directors" path. The `split_directors` helper returns
    /// `(s, None)` in that case.
    #[test]
    fn test_title_without_directors_parens() {
        // No `(directors)` group at all — directors are absent in the output.
        let p = parse_title("Русское / English [2020, США, драма, WEBRip] Dub").expect("parse");
        assert!(
            p.director.is_none(),
            "title without `(…)` group must have director=None, got: {:?}",
            p.director
        );
    }
}
