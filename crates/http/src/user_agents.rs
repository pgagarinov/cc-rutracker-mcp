use std::time::{SystemTime, UNIX_EPOCH};

pub const POOL: &[&str] = &[
    "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/133.0.0.0 Safari/537.36",
    "Mozilla/5.0 (Macintosh; Intel Mac OS X 14.4; rv:124.0) Gecko/20100101 Firefox/124.0",
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/133.0.0.0 Safari/537.36",
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64; rv:124.0) Gecko/20100101 Firefox/124.0",
];

pub fn pick_by_hour() -> &'static str {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let hour = seconds / 3_600;
    POOL[(hour as usize) % POOL.len()]
}

#[cfg(test)]
mod tests {
    use super::{pick_by_hour, POOL};

    #[test]
    fn test_pool_has_at_least_four_entries() {
        assert!(POOL.len() >= 4);
    }

    #[test]
    fn test_pick_by_hour_returns_pool_entry() {
        assert!(POOL.contains(&pick_by_hour()));
    }
}
