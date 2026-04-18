//! `structure.json` — top-level forum-tree snapshot refreshed on demand.

use std::path::Path;

use chrono::Utc;
use rutracker_http::{urls, Client};
use rutracker_parser::{forum_index::parse_forum_index, CategoryGroup};
use serde::{Deserialize, Serialize};

use crate::topic_io;
use crate::Result;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Structure {
    pub schema_version: u32,
    pub groups: Vec<CategoryGroup>,
    pub fetched_at: Option<String>,
}

impl Structure {
    /// Empty skeleton written by `Mirror::init` before the first refresh.
    pub fn empty() -> Self {
        Self {
            schema_version: 1,
            groups: Vec::new(),
            fetched_at: None,
        }
    }
}

/// Fetch `index.php`, parse the forum tree, and persist `structure.json`.
pub async fn refresh_structure(root: &Path, client: &Client) -> Result<Structure> {
    let html = client.get_text(urls::INDEX_PHP, &[]).await?;
    write_structure_from_html(root, &html)
}

/// Parse a caller-supplied `index.php` HTML string and persist `structure.json`.
/// Split out from [`refresh_structure`] so tests can exercise the writer without
/// a network stub.
pub fn write_structure_from_html(root: &Path, html: &str) -> Result<Structure> {
    let groups = parse_forum_index(html)?;
    let structure = Structure {
        schema_version: 1,
        groups,
        fetched_at: Some(Utc::now().to_rfc3339()),
    };
    let path = root.join("structure.json");
    topic_io::write_json_atomic(&path, &structure)?;
    Ok(structure)
}

#[cfg(test)]
mod tests {
    use super::*;
    use encoding_rs::WINDOWS_1251;
    use tempfile::TempDir;

    const INDEX_FIXTURE: &[u8] = include_bytes!("../../parser/tests/fixtures/index-sample.html");

    fn fixture_html() -> String {
        let (cow, _, _) = WINDOWS_1251.decode(INDEX_FIXTURE);
        cow.into_owned()
    }

    #[test]
    fn test_structure_json_contains_at_least_26_groups() {
        let td = TempDir::new().unwrap();
        let written = write_structure_from_html(td.path(), &fixture_html()).unwrap();
        assert!(
            written.groups.len() >= 26,
            "writer returned fewer than 26 groups: {}",
            written.groups.len()
        );

        let bytes = std::fs::read(td.path().join("structure.json")).unwrap();
        let loaded: Structure = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(loaded.schema_version, 1);
        assert!(
            loaded.groups.len() >= 26,
            "loaded structure.json has fewer than 26 groups: {}",
            loaded.groups.len()
        );
        assert!(loaded.fetched_at.is_some());
    }
}
