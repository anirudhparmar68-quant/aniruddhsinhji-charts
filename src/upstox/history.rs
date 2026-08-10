//! Daily historical candles from the Upstox V3 API.
//!
//! `GET /v3/historical-candle/{instrument_key}/days/1/{to_date}/{from_date}`
//! returns `data.candles` as `[timestamp, open, high, low, close, volume, oi]`,
//! newest first. Daily data goes back to January 2000.

use crate::model::Candle;
use anyhow::{Context, Result};
use chrono::{DateTime, NaiveDate};
use std::time::Duration;

const BASE: &str = "https://api.upstox.com/v3/historical-candle";

/// Upstox enforces per-second, per-minute *and* per-30-minute quotas. Bounded
/// concurrency alone cannot respect the longer windows — 2,161 requests will
/// blow through a per-30-minute quota no matter how few run at once — so every
/// request also passes through a global pacer.
const MAX_ATTEMPTS: u32 = 6;

/// Paces request starts to a fixed global rate.
///
/// Each caller claims the next free slot and sleeps until it, so N concurrent
/// tasks still issue at most `1 / min_gap` requests per second in aggregate.
#[derive(Debug)]
pub struct Pacer {
    min_gap: Duration,
    next_slot: tokio::sync::Mutex<tokio::time::Instant>,
}

impl Pacer {
    pub fn per_second(rate: f64) -> Self {
        let rate = rate.clamp(0.2, 50.0);
        Self {
            min_gap: Duration::from_secs_f64(1.0 / rate),
            next_slot: tokio::sync::Mutex::new(tokio::time::Instant::now()),
        }
    }

    async fn acquire(&self) {
        let slot = {
            let mut next = self.next_slot.lock().await;
            let slot = (*next).max(tokio::time::Instant::now());
            *next = slot + self.min_gap;
            slot
        };
        tokio::time::sleep_until(slot).await;
    }

    /// Push every pending slot back — used when the server signals throttling,
    /// so the whole fleet slows down rather than just the request that was hit.
    async fn penalise(&self, delay: Duration) {
        let mut next = self.next_slot.lock().await;
        *next = (*next).max(tokio::time::Instant::now() + delay);
    }
}

/// Outcome of one instrument's fetch. `NoData` is a normal result for freshly
/// listed or long-suspended scrips and must not be treated as a failure.
#[derive(Debug)]
pub enum FetchOutcome {
    Candles(Vec<Candle>),
    NoData,
}

fn parse_candles(body: &serde_json::Value) -> Result<Vec<Candle>> {
    let rows = body
        .get("data")
        .and_then(|d| d.get("candles"))
        .and_then(|c| c.as_array())
        .ok_or_else(|| anyhow::anyhow!("response had no data.candles array"))?;

    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        let Some(cells) = row.as_array() else { continue };
        if cells.len() < 6 {
            continue;
        }
        let Some(ts) = cells[0].as_str() else { continue };
        let Ok(dt) = DateTime::parse_from_rfc3339(ts) else { continue };

        let num = |v: &serde_json::Value| v.as_f64();
        let (Some(open), Some(high), Some(low), Some(close)) =
            (num(&cells[1]), num(&cells[2]), num(&cells[3]), num(&cells[4]))
        else {
            continue;
        };
        // A zero/!finite bar is a data artefact; patterns would read it as a gap.
        if ![open, high, low, close].iter().all(|v| v.is_finite() && *v > 0.0) {
            continue;
        }

        out.push(Candle {
            date: dt.date_naive(),
            open,
            high,
            low,
            close,
            volume: cells[5].as_i64().unwrap_or_else(|| num(&cells[5]).unwrap_or(0.0) as i64),
        });
    }

    // Upstox returns newest-first; every consumer in this crate assumes ascending.
    out.sort_by_key(|c| c.date);
    out.dedup_by_key(|c| c.date);
    Ok(out)
}

/// Fetch daily candles for one instrument, retrying on throttling and transient
/// server errors with exponential backoff.
pub async fn fetch_daily(
    client: &reqwest::Client,
    token: &str,
    pacer: &Pacer,
    instrument_key: &str,
    from: NaiveDate,
    to: NaiveDate,
) -> Result<FetchOutcome> {
    let url = format!(
        "{BASE}/{}/days/1/{}/{}",
        urlencoding::encode(instrument_key),
        to.format("%Y-%m-%d"),
        from.format("%Y-%m-%d"),
    );

    let mut last_err = None;
    for attempt in 0..MAX_ATTEMPTS {
        if attempt > 0 {
            // 2s, 6s, 18s, 54s, 120s. A throttle can be a per-minute or even a
            // per-30-minute quota, so the tail of this schedule has to be long
            // enough to actually outlast one — a few seconds is useless.
            let secs = (2u64 * 3u64.saturating_pow(attempt - 1)).min(120);
            tokio::time::sleep(Duration::from_secs(secs)).await;
        }
        pacer.acquire().await;

        let resp = match client
            .get(&url)
            .bearer_auth(token)
            .header("Accept", "application/json")
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) => {
                last_err = Some(anyhow::anyhow!("request failed: {e}"));
                continue;
            }
        };

        let status = resp.status();
        // 404 here means "this instrument has no daily series", not an error.
        if status == reqwest::StatusCode::NOT_FOUND {
            return Ok(FetchOutcome::NoData);
        }

        // 403 is included deliberately: under load Upstox's edge answers with
        // Cloudflare "error 1010" rather than a 429, and treating that as fatal
        // is what silently turned a throttle into 1,656 empty instruments.
        let throttled = status == reqwest::StatusCode::TOO_MANY_REQUESTS
            || status == reqwest::StatusCode::FORBIDDEN
            || status.is_server_error();
        if throttled {
            let retry_after = resp
                .headers()
                .get(reqwest::header::RETRY_AFTER)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<u64>().ok())
                .map(Duration::from_secs)
                .unwrap_or_else(|| Duration::from_secs(5 * (attempt as u64 + 1)));
            // Slow the whole fleet, not just this task.
            pacer.penalise(retry_after).await;
            last_err = Some(anyhow::anyhow!("upstream returned {status}"));
            continue;
        }

        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("Upstox returned {status} for {instrument_key}: {body}");
        }

        let body: serde_json::Value = resp
            .json()
            .await
            .with_context(|| format!("non-JSON history response for {instrument_key}"))?;
        let candles = parse_candles(&body)
            .with_context(|| format!("parsing history for {instrument_key}"))?;

        return Ok(if candles.is_empty() {
            FetchOutcome::NoData
        } else {
            FetchOutcome::Candles(candles)
        });
    }

    Err(last_err.unwrap_or_else(|| anyhow::anyhow!("history fetch failed for {instrument_key}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_sorts_ascending() {
        let body = serde_json::json!({
            "status": "success",
            "data": { "candles": [
                ["2025-08-08T00:00:00+05:30", 101.0, 105.0, 100.0, 104.0, 5000, 0],
                ["2025-08-07T00:00:00+05:30", 100.0, 102.0,  99.0, 101.0, 4000, 0]
            ]}
        });
        let candles = parse_candles(&body).unwrap();
        assert_eq!(candles.len(), 2);
        assert_eq!(candles[0].date, NaiveDate::from_ymd_opt(2025, 8, 7).unwrap());
        assert_eq!(candles[1].close, 104.0);
    }

    #[test]
    fn drops_zero_and_malformed_rows() {
        let body = serde_json::json!({
            "data": { "candles": [
                ["2025-08-08T00:00:00+05:30", 0.0, 0.0, 0.0, 0.0, 0, 0],
                ["not-a-date", 1.0, 2.0, 0.5, 1.5, 10, 0],
                ["2025-08-07T00:00:00+05:30", 100.0, 102.0, 99.0, 101.0, 4000, 0]
            ]}
        });
        let candles = parse_candles(&body).unwrap();
        assert_eq!(candles.len(), 1);
        assert_eq!(candles[0].close, 101.0);
    }

    #[test]
    fn missing_candles_key_is_an_error() {
        assert!(parse_candles(&serde_json::json!({"status": "success"})).is_err());
    }

    #[tokio::test]
    async fn pacer_enforces_a_global_rate() {
        let pacer = Pacer::per_second(20.0); // one slot every 50 ms
        let start = tokio::time::Instant::now();
        for _ in 0..5 {
            pacer.acquire().await;
        }
        // Slots 0..4 sit at 0, 50, 100, 150 and 200 ms.
        assert!(
            start.elapsed() >= Duration::from_millis(180),
            "rate not enforced, elapsed {:?}",
            start.elapsed()
        );
    }

    #[tokio::test]
    async fn pacer_penalty_delays_everyone() {
        let pacer = Pacer::per_second(50.0);
        pacer.acquire().await;
        pacer.penalise(Duration::from_millis(300)).await;

        let start = tokio::time::Instant::now();
        pacer.acquire().await;
        assert!(
            start.elapsed() >= Duration::from_millis(250),
            "throttle penalty ignored, elapsed {:?}",
            start.elapsed()
        );
    }

    #[test]
    fn pacer_rate_is_clamped_to_something_sane() {
        assert!(Pacer::per_second(0.0).min_gap <= Duration::from_secs(5));
        assert!(Pacer::per_second(1e9).min_gap >= Duration::from_millis(20));
    }
}
