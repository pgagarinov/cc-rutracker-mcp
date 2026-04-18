//! rutracker-parser — pure HTML parsing for rutracker.org (TorrentPier phpBB fork).
//!
//! No I/O, no async. All inputs are `&str` (cp1251-decoded HTML). All outputs are `Result<T>`
//! with `Error` from [`error::Error`].

pub mod error;
pub mod forum_index;
pub mod metadata;
pub mod models;
pub mod search;
pub mod text_format;
pub mod topic;

pub use error::{Error, Result};
pub use models::{
    CategoryGroup, Comment, ForumCategory, SearchPage, SearchResult, TopicDetails, TopicMetadata,
};
