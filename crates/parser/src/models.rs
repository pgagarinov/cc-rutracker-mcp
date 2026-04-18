//! Data types shared across parser submodules and consumed by downstream crates.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct SearchResult {
    pub topic_id: u64,
    pub title: String,
    pub size: String,
    pub seeds: u32,
    pub leeches: u32,
    pub author: String,
    pub category: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SearchPage {
    pub results: Vec<SearchResult>,
    pub page: u32,
    pub per_page: u32,
    pub total_results: Option<u32>,
    pub search_id: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Comment {
    pub post_id: u64,
    pub author: String,
    pub date: String,
    pub text: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct TopicMetadata {
    pub imdb_rating: Option<f32>,
    pub kinopoisk_rating: Option<f32>,
    pub imdb_url: Option<String>,
    pub kinopoisk_url: Option<String>,
    pub year: Option<u16>,
    pub countries: Vec<String>,
    pub genres: Vec<String>,
    pub director: String,
    pub cast: Vec<String>,
    pub duration: String,
    pub release_type: String,
    pub video: String,
    pub audio: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TopicDetails {
    pub topic_id: u64,
    pub title: String,
    pub magnet_link: String,
    pub size: String,
    pub seeds: u32,
    pub leeches: u32,
    pub description: String,
    pub file_list: Vec<String>,
    pub metadata: Option<TopicMetadata>,
    pub comments: Vec<Comment>,
    pub comment_pages_fetched: u32,
    pub comment_pages_total: u32,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct CategoryGroup {
    pub group_id: String,
    pub title: String,
    pub forums: Vec<ForumCategory>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ForumCategory {
    pub forum_id: String,
    pub name: String,
    pub parent_id: Option<String>,
}
