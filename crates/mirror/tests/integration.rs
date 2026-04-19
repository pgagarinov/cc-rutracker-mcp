//! Integration test placeholder — real end-to-end wiremock scenarios land in M4/M5.
//! Kept here so `cargo test -p rutracker-mirror` exercises the top-level API surface.

use rutracker_mirror::Mirror;
use tempfile::TempDir;

#[test]
fn test_init_then_open_roundtrip() {
    let td = TempDir::new().unwrap();
    {
        let _m = Mirror::init(td.path()).unwrap();
    }
    let m = Mirror::open(td.path(), None).unwrap();
    assert_eq!(m.root(), td.path());
    assert_eq!(m.state().schema_version().unwrap(), 2);
}

#[test]
fn test_init_creates_expected_files() {
    let td = TempDir::new().unwrap();
    let _m = Mirror::init(td.path()).unwrap();
    assert!(td.path().join("structure.json").exists());
    assert!(td.path().join("watchlist.json").exists());
    assert!(td.path().join("state.db").exists());
    assert!(td.path().join("forums").is_dir());
}
