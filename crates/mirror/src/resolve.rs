//! Forum-name resolution — map a user-supplied string (numeric id or name) to
//! a canonical numeric `forum_id` string.

use rutracker_parser::CategoryGroup;

use crate::structure::Structure;

#[derive(Debug, thiserror::Error)]
pub enum ResolveError {
    #[error("no forum matches {0:?}")]
    NotFound(String),
    #[error("ambiguous forum name {query:?}: {matches:?}")]
    Ambiguous {
        query: String,
        matches: Vec<(String, String)>,
    }, // (forum_id, name)
    #[error("structure.json missing — run `rutracker mirror structure` first")]
    NoStructure,
}

/// Resolve a forum reference (numeric id or name) to a canonical forum_id string.
///
/// Rules:
/// - If `input` is all ASCII digits → returned verbatim (backward-compatible).
/// - Else: case-insensitive exact match on `forum.name` across all groups → single match wins.
/// - If no exact match, case-insensitive substring match.
///   - Exactly 1 hit → return it.
///   - 0 hits → `NotFound`.
///   - > 1 hit → `Ambiguous` with the list.
pub fn resolve_forum_ref(structure: &Structure, input: &str) -> Result<String, ResolveError> {
    // Numeric ids pass through verbatim — backward-compatible, zero cost.
    if input.chars().all(|c| c.is_ascii_digit()) {
        return Ok(input.to_string());
    }

    let input_lower = input.to_lowercase();

    let all_forums: Vec<&rutracker_parser::ForumCategory> = structure
        .groups
        .iter()
        .flat_map(|g: &CategoryGroup| g.forums.iter())
        .collect();

    // 1. Case-insensitive exact match.
    let exact: Vec<_> = all_forums
        .iter()
        .filter(|f| f.name.to_lowercase() == input_lower)
        .collect();

    match exact.len() {
        1 => return Ok(exact[0].forum_id.clone()),
        n if n > 1 => {
            let matches = exact
                .iter()
                .map(|f| (f.forum_id.clone(), f.name.clone()))
                .collect();
            return Err(ResolveError::Ambiguous {
                query: input.to_string(),
                matches,
            });
        }
        _ => {}
    }

    // 2. Case-insensitive substring match.
    let sub: Vec<_> = all_forums
        .iter()
        .filter(|f| f.name.to_lowercase().contains(&input_lower))
        .collect();

    match sub.len() {
        0 => Err(ResolveError::NotFound(input.to_string())),
        1 => Ok(sub[0].forum_id.clone()),
        _ => {
            let matches = sub
                .iter()
                .map(|f| (f.forum_id.clone(), f.name.clone()))
                .collect();
            Err(ResolveError::Ambiguous {
                query: input.to_string(),
                matches,
            })
        }
    }
}

// ---------- helpers for tests ----------

#[cfg(test)]
fn make_structure(forums: &[(&str, &str)]) -> Structure {
    use rutracker_parser::{CategoryGroup, ForumCategory};

    let forum_entries: Vec<ForumCategory> = forums
        .iter()
        .map(|(id, name)| ForumCategory {
            forum_id: id.to_string(),
            name: name.to_string(),
            parent_id: None,
        })
        .collect();

    Structure {
        schema_version: 1,
        groups: vec![CategoryGroup {
            group_id: "1".to_string(),
            title: "Тест".to_string(),
            forums: forum_entries,
        }],
        fetched_at: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_numeric_input_passes_through() {
        let structure = make_structure(&[("252", "Фильмы 2026")]);
        assert_eq!(resolve_forum_ref(&structure, "252").unwrap(), "252");
        assert_eq!(resolve_forum_ref(&structure, "999999").unwrap(), "999999");
    }

    #[test]
    fn test_exact_name_match() {
        let structure = make_structure(&[("252", "Фильмы 2026")]);
        assert_eq!(resolve_forum_ref(&structure, "Фильмы 2026").unwrap(), "252");
    }

    #[test]
    fn test_case_insensitive_match() {
        let structure = make_structure(&[("252", "Фильмы 2026")]);
        // Cyrillic lowercase of "Ф" is "ф"
        assert_eq!(resolve_forum_ref(&structure, "фильмы 2026").unwrap(), "252");
    }

    #[test]
    fn test_substring_match_unique() {
        // Single match — returns it. Use a non-numeric substring.
        let structure = make_structure(&[("252", "Фильмы 2026")]);
        assert_eq!(resolve_forum_ref(&structure, "Фильмы").unwrap(), "252");

        // Two matches containing the same substring — Ambiguous.
        let structure2 = make_structure(&[("252", "Фильмы 2026"), ("9999", "Фильмы 2021-2025")]);
        let err = resolve_forum_ref(&structure2, "Фильмы 20").unwrap_err();
        assert!(
            matches!(err, ResolveError::Ambiguous { .. }),
            "expected Ambiguous, got: {err:?}"
        );
        if let ResolveError::Ambiguous { matches, .. } = err {
            assert_eq!(matches.len(), 2);
        }
    }

    #[test]
    fn test_not_found_returns_error() {
        let structure = make_structure(&[("252", "Фильмы 2026")]);
        let err = resolve_forum_ref(&structure, "bogus").unwrap_err();
        assert!(matches!(err, ResolveError::NotFound(_)));
        assert_eq!(err.to_string(), r#"no forum matches "bogus""#);
    }

    #[test]
    fn test_ambiguous_substring_returns_error_with_candidates() {
        let structure = make_structure(&[
            ("252", "Фильмы 2026"),
            ("1950", "Фильмы 2021-2025"),
            ("2200", "Фильмы 2016-2020"),
        ]);
        let err = resolve_forum_ref(&structure, "Фильмы").unwrap_err();
        match err {
            ResolveError::Ambiguous { query, matches } => {
                assert_eq!(query, "Фильмы");
                assert_eq!(matches.len(), 3);
                let ids: Vec<&str> = matches.iter().map(|(id, _)| id.as_str()).collect();
                assert!(ids.contains(&"252"));
                assert!(ids.contains(&"1950"));
                assert!(ids.contains(&"2200"));
            }
            other => panic!("expected Ambiguous, got: {other:?}"),
        }
    }
}
