//! Forum index (index.php) parser — category groups + subforum links.

use crate::{CategoryGroup, ForumCategory, Result};
use regex::Regex;
use scraper::{Html, Selector};

pub fn parse_forum_index(html: &str) -> Result<Vec<CategoryGroup>> {
    let doc = Html::parse_document(html);
    let category_sel = Selector::parse("div.category").unwrap();
    let title_sel = Selector::parse("h3.cat_title, span.cat_title, h4.cat_title").unwrap();
    let forum_link_sel = Selector::parse(r#"a[href*="viewforum.php?f="]"#).unwrap();
    let forum_id_re = Regex::new(r"viewforum\.php\?f=(\d+)").unwrap();

    let mut groups = Vec::new();
    for cat in doc.select(&category_sel) {
        let group_id = cat.value().attr("id").unwrap_or("").to_string();
        let title = cat
            .select(&title_sel)
            .next()
            .map(|t| t.text().collect::<String>().trim().to_string())
            .unwrap_or_default();

        let mut forums = Vec::new();
        for link in cat.select(&forum_link_sel) {
            let Some(href) = link.value().attr("href") else {
                continue;
            };
            let Some(captures) = forum_id_re.captures(href) else {
                continue;
            };
            let forum_id = captures.get(1).unwrap().as_str().to_string();
            let name = link.text().collect::<String>().trim().to_string();
            if name.is_empty() {
                continue;
            }
            forums.push(ForumCategory {
                forum_id,
                name,
                parent_id: Some(group_id.clone()),
            });
        }

        groups.push(CategoryGroup {
            group_id,
            title,
            forums,
        });
    }

    Ok(groups)
}

#[cfg(test)]
mod tests {
    use super::*;
    use encoding_rs::WINDOWS_1251;

    const INDEX_FIXTURE: &[u8] = include_bytes!("../tests/fixtures/index-sample.html");

    fn fixture_html() -> String {
        let (cow, _, _) = WINDOWS_1251.decode(INDEX_FIXTURE);
        cow.into_owned()
    }

    #[test]
    fn test_group_count_26() {
        let groups = parse_forum_index(&fixture_html()).unwrap();
        assert_eq!(
            groups.len(),
            26,
            "expected 26 CategoryGroups from index-sample.html"
        );
    }

    /// Hierarchical forum count — forums nested under `div.category` groups.
    /// Raw `viewforum.php?f=` anchor count is 321 but includes top-nav shortcuts (~41) and
    /// cross-references (~5 duplicates). The 280 number is the true subforum hierarchy size.
    #[test]
    fn test_forum_count_gte_280() {
        let groups = parse_forum_index(&fixture_html()).unwrap();
        let total: usize = groups.iter().map(|g| g.forums.len()).sum();
        assert!(total >= 280, "expected ≥280 forums, got {total}");
    }

    #[test]
    fn test_forum_parent_id_matches_group_id() {
        let groups = parse_forum_index(&fixture_html()).unwrap();
        let first_group_with_forums = groups.iter().find(|g| !g.forums.is_empty()).unwrap();
        assert_eq!(
            first_group_with_forums.forums[0].parent_id.as_deref(),
            Some(first_group_with_forums.group_id.as_str())
        );
    }

    /// US-008: within a category, `<a href="viewforum.php?f=…">` links with
    /// malformed / missing `f=<digits>` captures must be silently skipped by
    /// the `let Some(captures) = … else { continue; }` branch (L29). Also
    /// covers L34 — anchor with empty link text must be skipped.
    #[test]
    fn test_category_skips_links_without_forum_id_regex_match() {
        let html = r#"<!DOCTYPE html>
<html><body>
<div class="category" id="cat-1">
  <h3 class="cat_title">Cat One</h3>
  <!-- matches the selector (href contains viewforum.php?f=) but the regex wants f=<digits> -->
  <a href="viewforum.php?f=NOT_A_NUMBER">bad-id</a>
  <!-- empty name — must be skipped via the `if name.is_empty()` guard -->
  <a href="viewforum.php?f=1"></a>
  <!-- good entry -->
  <a href="viewforum.php?f=2">Фильмы</a>
</div>
</body></html>"#;
        let groups = parse_forum_index(html).unwrap();
        assert_eq!(groups.len(), 1);
        let forums = &groups[0].forums;
        assert_eq!(
            forums.len(),
            1,
            "only the well-formed, non-empty-name link must be kept"
        );
        assert_eq!(forums[0].forum_id, "2");
        assert_eq!(forums[0].name, "Фильмы");
    }
}
