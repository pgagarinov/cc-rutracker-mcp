use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("html structure missing expected element: {0}")]
    MissingElement(&'static str),

    #[error("html attribute missing: {0}")]
    MissingAttribute(&'static str),

    #[error("integer parse failed: {0}")]
    ParseInt(#[from] std::num::ParseIntError),

    #[error("regex compile failed: {0}")]
    Regex(#[from] regex::Error),

    #[error("parser sanity check failed: {0}")]
    ParseSanityFailed(&'static str),
}

pub type Result<T> = std::result::Result<T, Error>;
