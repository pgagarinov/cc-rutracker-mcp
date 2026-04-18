//! `watchlist.json` — user-edited list of forums to sync.
//!
//! Pure in-memory mutators (`add`, `remove`, `list`) plus disk I/O (`load`, `save`).
//! `add` consults a parsed [`Structure`] to resolve the forum name and to reject
//! unknown ids (see plan §6 / §M2).

use std::path::Path;

use chrono::Utc;

use crate::config::{Watchlist, WatchlistEntry};
use crate::structure::Structure;
use crate::{topic_io, Error, Result};

const WATCHLIST_FILE: &str = "watchlist.json";

/// Read `watchlist.json` from `root`; returns an empty default if the file is missing.
pub fn load(root: &Path) -> Result<Watchlist> {
    let path = root.join(WATCHLIST_FILE);
    if !path.exists() {
        return Ok(Watchlist::default());
    }
    let bytes = std::fs::read(&path)?;
    let wl: Watchlist = serde_json::from_slice(&bytes)?;
    Ok(wl)
}

/// Persist `wl` to `<root>/watchlist.json` atomically.
pub fn save(root: &Path, wl: &Watchlist) -> Result<()> {
    let path = root.join(WATCHLIST_FILE);
    topic_io::write_json_atomic(&path, wl)
}

/// Add `forum_id` to `wl`, looking up the canonical name in `structure`.
/// Idempotent: adding a duplicate is a no-op. Unknown ids raise [`Error::UnknownForum`].
pub fn add(wl: &mut Watchlist, structure: &Structure, forum_id: &str) -> Result<()> {
    if wl.forums.iter().any(|e| e.forum_id == forum_id) {
        return Ok(());
    }
    let name = lookup_forum_name(structure, forum_id)
        .ok_or_else(|| Error::UnknownForum(forum_id.to_string()))?;
    wl.forums.push(WatchlistEntry {
        forum_id: forum_id.to_string(),
        name,
        added_at: Utc::now().to_rfc3339(),
    });
    Ok(())
}

/// Remove `forum_id` from `wl` if present; no-op otherwise.
pub fn remove(wl: &mut Watchlist, forum_id: &str) {
    wl.forums.retain(|e| e.forum_id != forum_id);
}

/// Read-only view of the current watchlist entries.
pub fn list(wl: &Watchlist) -> &[WatchlistEntry] {
    &wl.forums
}

fn lookup_forum_name(structure: &Structure, forum_id: &str) -> Option<String> {
    for group in &structure.groups {
        for forum in &group.forums {
            if forum.forum_id == forum_id {
                return Some(forum.name.clone());
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use rutracker_parser::{CategoryGroup, ForumCategory};

    fn fixture_structure() -> Structure {
        Structure {
            schema_version: 1,
            groups: vec![CategoryGroup {
                group_id: "1".into(),
                title: "Кино".into(),
                forums: vec![
                    ForumCategory {
                        forum_id: "252".into(),
                        name: "Зарубежные фильмы".into(),
                        parent_id: None,
                    },
                    ForumCategory {
                        forum_id: "7".into(),
                        name: "Наше кино".into(),
                        parent_id: None,
                    },
                ],
            }],
            fetched_at: None,
        }
    }

    #[test]
    fn test_add_then_list_returns_forum() {
        let mut wl = Watchlist::default();
        let structure = fixture_structure();
        add(&mut wl, &structure, "252").unwrap();
        let entries = list(&wl);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].forum_id, "252");
        assert_eq!(entries[0].name, "Зарубежные фильмы");
        assert!(!entries[0].added_at.is_empty());
    }

    #[test]
    fn test_remove_removes_entry() {
        let mut wl = Watchlist::default();
        let structure = fixture_structure();
        add(&mut wl, &structure, "252").unwrap();
        add(&mut wl, &structure, "7").unwrap();
        remove(&mut wl, "252");
        let entries = list(&wl);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].forum_id, "7");
    }

    #[test]
    fn test_add_duplicate_is_idempotent() {
        let mut wl = Watchlist::default();
        let structure = fixture_structure();
        add(&mut wl, &structure, "252").unwrap();
        add(&mut wl, &structure, "252").unwrap();
        assert_eq!(list(&wl).len(), 1);
    }

    #[test]
    fn test_add_unknown_forum_id_errors() {
        let mut wl = Watchlist::default();
        let structure = fixture_structure();
        let err = add(&mut wl, &structure, "99999").unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("unknown forum id"),
            "error message missing 'unknown forum id': {msg}"
        );
        assert!(list(&wl).is_empty());
    }
}
