//! Paths, secrets and user-tunable settings.
//!
//! Secrets live in `.env` (never persisted by the app). Everything the user can
//! change from the UI lives in `data/settings.json` so a rebuild never wipes it.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

/// Project root. In dev this is the crate dir; for a shipped exe we prefer the
/// exe's own folder so the app stays portable next to its `data/`.
pub fn project_root() -> PathBuf {
    if let Ok(p) = std::env::var("SPIDER_HOME") {
        return PathBuf::from(p);
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            if dir.join("data").is_dir() || dir.join(".env").is_file() {
                return dir.to_path_buf();
            }
        }
    }
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

pub fn data_dir() -> PathBuf {
    let d = project_root().join("data");
    let _ = fs::create_dir_all(&d);
    d
}

pub fn db_path() -> PathBuf {
    data_dir().join("spider.db")
}

pub fn token_path() -> PathBuf {
    data_dir().join("token.json")
}

pub fn settings_path() -> PathBuf {
    data_dir().join("settings.json")
}

// ---------------------------------------------------------------------------
// Secrets
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct Secrets {
    pub api_key: String,
    pub api_secret: String,
    pub redirect_uri: String,
}

impl Secrets {
    pub fn load() -> Result<Self> {
        let env_file = project_root().join(".env");
        if env_file.is_file() {
            // Non-fatal: a malformed .env shouldn't stop the app, process env may still have the keys.
            let _ = dotenvy::from_path(&env_file);
        }
        let api_key = std::env::var("UPSTOX_API_KEY").unwrap_or_default();
        let api_secret = std::env::var("UPSTOX_API_SECRET").unwrap_or_default();
        let redirect_uri = std::env::var("UPSTOX_REDIRECT_URI")
            .unwrap_or_else(|_| "http://localhost:8765/callback".to_string());

        if api_key.trim().is_empty() || api_secret.trim().is_empty() {
            anyhow::bail!(
                "UPSTOX_API_KEY / UPSTOX_API_SECRET missing. Put them in {}",
                env_file.display()
            );
        }
        Ok(Self { api_key, api_secret, redirect_uri })
    }

    /// Host + port the OAuth redirect catcher must bind to.
    pub fn redirect_host_port(&self) -> (String, u16) {
        // Deliberately hand-parsed: the URI is a fixed localhost callback, and
        // pulling in a URL crate for one line isn't worth it.
        let after_scheme = self
            .redirect_uri
            .split("://")
            .nth(1)
            .unwrap_or("localhost:8765/callback");
        let authority = after_scheme.split('/').next().unwrap_or("localhost:8765");
        let mut parts = authority.split(':');
        let host = parts.next().unwrap_or("localhost").to_string();
        let port = parts.next().and_then(|p| p.parse().ok()).unwrap_or(8765);
        (host, port)
    }
}

// ---------------------------------------------------------------------------
// User settings
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Market-cap floor in rupees crore.
    pub min_mcap_cr: f64,
    /// Calendar days of daily history to keep (365 = ~248 trading sessions).
    pub history_days: i64,
    /// Parallel in-flight Upstox history requests.
    pub max_concurrency: usize,
    /// Global cap on how fast history requests are issued, across all workers.
    /// Concurrency alone cannot respect Upstox's per-minute and per-30-minute
    /// quotas; this is what actually keeps a 2,000-stock backfill inside them.
    pub requests_per_second: f64,
    /// Keep a stock whose market cap we could not resolve, provided it clears
    /// the liquidity floor below. Prevents silently dropping valid names.
    pub include_unknown_mcap: bool,
    /// Median daily traded value (crore) required when market cap is unknown.
    pub min_median_turnover_cr: f64,
    /// Optional page to scrape for a bulk market-cap workbook. Empty by
    /// default: no dependable free source keyed by symbol exists today, so
    /// `data/mcap_overrides.csv` is the supported route.
    pub mcap_source_url: String,
    /// Ignore any stock whose latest close is below this (penny-stock guard).
    pub min_price: f64,
    /// Candles a pattern scan looks back over.
    pub scan_lookback: usize,
    /// Minimum score (0..1) for a detection to be listed.
    pub min_pattern_score: f64,
    /// Exchange preference when the same ISIN trades on both.
    pub prefer_nse: bool,
    /// BSE group codes treated as mainboard equity. BSE reports a group where
    /// NSE reports an instrument type; A and B are the mainboard tiers, while
    /// T/X/XT/Z are surveillance or illiquid and M/MT/MS are the SME platform.
    pub bse_groups: Vec<String>,
    /// Thresholds for the recognisers that expose them (cups, Chartink scans).
    pub patterns: crate::patterns::PatternParams,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            min_mcap_cr: 100.0,
            history_days: 400,
            max_concurrency: 8,
            requests_per_second: 4.0,
            include_unknown_mcap: true,
            min_median_turnover_cr: 0.25,
            mcap_source_url: String::new(),
            min_price: 5.0,
            scan_lookback: 250,
            min_pattern_score: 0.5,
            prefer_nse: true,
            bse_groups: crate::upstox::instruments::DEFAULT_BSE_GROUPS
                .iter()
                .map(|s| s.to_string())
                .collect(),
            patterns: crate::patterns::PatternParams::default(),
        }
    }
}

impl Settings {
    pub fn load() -> Self {
        let path = settings_path();
        match fs::read_to_string(&path) {
            Ok(raw) => serde_json::from_str(&raw).unwrap_or_default(),
            Err(_) => Self::default(),
        }
    }

    pub fn save(&self) -> Result<()> {
        let path = settings_path();
        let raw = serde_json::to_string_pretty(self)?;
        fs::write(&path, raw).with_context(|| format!("writing {}", path.display()))?;
        Ok(())
    }
}

/// Small helper used by the downloader cache checks.
pub fn is_modified_today(path: &Path) -> bool {
    use chrono::{DateTime, Local};
    let Ok(meta) = fs::metadata(path) else { return false };
    let Ok(modified) = meta.modified() else { return false };
    let modified: DateTime<Local> = modified.into();
    modified.date_naive() == Local::now().date_naive()
}
