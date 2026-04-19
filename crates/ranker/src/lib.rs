//! rutracker-ranker — objective quality ranker for rutracker film releases.
//!
//! See `.omc/plans/quality-ranker.md` for the full two-stage pipeline design.
//! This crate owns stages A / C / D / E (pure Rust, no network). Stage B (the
//! NLP comment scanner) is a Claude Code subagent living in
//! `.claude/agents/rutracker-film-scanner.md`.

pub mod agent_sha;
pub mod aggregator;
pub mod rip;
pub mod rip_metadata;
pub mod scan_io;
pub mod scan_prepare;
pub mod skill_contract;
pub mod title;

pub use agent_sha::{agent_sha_current, agent_sha_of};
pub use aggregator::{aggregate_film, FilmScore, FilmTopic, K_PRIOR, MU_0};
pub use rip::{rank_rips, RipCandidate, RipRanking, RipRationale};
pub use rip_metadata::{parse_size_bytes, RipMetadata};
pub use scan_io::{
    is_cached, read_scan, scan_is_failed, ScanError, ScanFile, TechQuality, TopicAnalysis,
};
pub use scan_prepare::{scan_prepare, PrepareReport, ScanPrepareError, ScanPrepareOpts};
pub use title::{film_id, film_key, parse_title, ParsedTitle, TitleParseError};
