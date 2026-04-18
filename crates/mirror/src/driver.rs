use std::time::Duration;

use chrono::{DateTime, Utc};
use rusqlite::params;
use rutracker_http::Client;

use crate::engine::{SyncEngine, SyncOpts};
use crate::{Error, Mirror, Result};

pub struct SyncDriver<'a> {
    mirror: &'a mut Mirror,
    client: Client,
}

#[derive(Debug, thiserror::Error)]
pub enum DriverError {
    #[error("gave up on forum {forum_id} after {attempts} attempts")]
    GaveUp { forum_id: String, attempts: u32 },
    #[error(transparent)]
    Inner(#[from] Error),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForumSummary {
    pub forum_id: String,
    pub topics_count: usize,
    pub attempts: u32,
    pub gave_up: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SyncSummary {
    pub forums_ok: Vec<ForumSummary>,
    pub forums_failed: Vec<ForumSummary>,
}

impl<'a> SyncDriver<'a> {
    pub fn new(mirror: &'a mut Mirror, client: Client) -> Self {
        Self { mirror, client }
    }

    pub async fn run_until_done(
        &mut self,
        forum_id: &str,
        opts: SyncOpts,
    ) -> std::result::Result<ForumSummary, DriverError> {
        for attempt in 1..=opts.max_attempts_per_forum {
            tracing::info!(
                target: "rutracker_mirror::driver",
                event = "forum_start",
                forum_id,
                attempt,
                max_attempts = opts.max_attempts_per_forum
            );

            let mut engine = SyncEngine::new(self.mirror, self.client.clone());
            let report = engine.sync_forum(forum_id, opts.clone()).await?;

            if report.forums_rate_limited == 0 {
                let topics_count = forum_topics_count(self.mirror, forum_id)?;
                tracing::info!(
                    target: "rutracker_mirror::driver",
                    event = "forum_complete",
                    forum_id,
                    topics_count,
                    attempts = attempt
                );
                return Ok(ForumSummary {
                    forum_id: forum_id.to_string(),
                    topics_count,
                    attempts: attempt,
                    gave_up: false,
                });
            }

            if !opts.cooldown_wait {
                break;
            }

            let wait = cooldown_wait(self.mirror, forum_id, opts.cooldown_multiplier)?;
            tracing::info!(
                target: "rutracker_mirror::driver",
                event = "rate_limit_sleep",
                forum_id,
                sleep_seconds = wait.as_secs_f64(),
                attempt,
                max_attempts = opts.max_attempts_per_forum
            );
            if !wait.is_zero() {
                tokio::time::sleep(wait).await;
            }
        }

        tracing::info!(
            target: "rutracker_mirror::driver",
            event = "forum_gave_up",
            forum_id,
            attempts = opts.max_attempts_per_forum
        );
        Err(DriverError::GaveUp {
            forum_id: forum_id.to_string(),
            attempts: opts.max_attempts_per_forum,
        })
    }

    pub async fn run_until_done_all(
        &mut self,
        forum_ids: &[String],
        opts: SyncOpts,
    ) -> std::result::Result<SyncSummary, DriverError> {
        let mut summary = SyncSummary::default();

        for forum_id in forum_ids {
            match self.run_until_done(forum_id, opts.clone()).await {
                Ok(forum_summary) => summary.forums_ok.push(forum_summary),
                Err(DriverError::GaveUp { forum_id, attempts }) => {
                    summary.forums_failed.push(ForumSummary {
                        topics_count: forum_topics_count(self.mirror, &forum_id)?,
                        forum_id,
                        attempts,
                        gave_up: true,
                    });
                }
                Err(err) => return Err(err),
            }
        }

        tracing::info!(
            target: "rutracker_mirror::driver",
            event = "sync_complete",
            forums_ok = summary.forums_ok.len(),
            forums_failed = summary.forums_failed.len()
        );

        Ok(summary)
    }
}

fn forum_topics_count(mirror: &Mirror, forum_id: &str) -> Result<usize> {
    let count: i64 = mirror.state().conn().query_row(
        "SELECT COUNT(*) FROM topic_index WHERE forum_id = ?1",
        params![forum_id],
        |row| row.get(0),
    )?;
    Ok(count.max(0) as usize)
}

fn cooldown_wait(mirror: &Mirror, forum_id: &str, multiplier: f32) -> Result<Duration> {
    let cooldown_until: Option<String> = mirror
        .state()
        .conn()
        .query_row(
            "SELECT cooldown_until FROM forum_state WHERE forum_id = ?1",
            params![forum_id],
            |row| row.get(0),
        )
        .ok()
        .flatten();

    let now = Utc::now();
    let base = cooldown_until
        .as_deref()
        .and_then(parse_ts)
        .map(|cooldown| cooldown.signed_duration_since(now))
        .unwrap_or_else(|| chrono::Duration::seconds(1));
    let base = base.max(chrono::Duration::seconds(1));
    let base = base.to_std().unwrap_or_else(|_| Duration::from_secs(1));
    Ok(base.mul_f32(multiplier.max(0.0)))
}

fn parse_ts(raw: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(raw)
        .ok()
        .map(|dt| dt.with_timezone(&Utc))
}
