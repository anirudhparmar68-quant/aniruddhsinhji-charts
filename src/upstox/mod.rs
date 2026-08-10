//! Everything that talks to Upstox: OAuth, the instrument master, and daily history.

pub mod auth;
pub mod history;
pub mod instruments;

use std::time::Duration;

/// Shared HTTP client. One connection pool for the whole app so the backfill
/// reuses TLS sessions instead of renegotiating thousands of times.
pub fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .user_agent("spider-charts/0.1 (+self-hosted)")
        .timeout(Duration::from_secs(45))
        .connect_timeout(Duration::from_secs(15))
        .pool_max_idle_per_host(16)
        .build()
        .expect("HTTP client construction cannot fail with these options")
}
