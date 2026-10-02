//! Paths, secrets and user-tunable settings.
//!
//! Secrets live in `.env` (never persisted by the app). Everything the user can
//! change from the UI lives in `data/settings.json` so a rebuild never wipes it.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

/// Folder (under `%LOCALAPPDATA%`) an installed copy keeps its data in. Not
/// "Spider Charts": that is where the installer puts the program itself, and
/// an uninstall must never be able to take the user's database with it.
const INSTALLED_HOME: &str = "Spider Charts Data";

/// Everything `resolve_root` looks at, gathered in one place so every rule can
/// be exercised in a test with real folders instead of the real machine.
pub struct RootInputs {
    /// `SPIDER_HOME`, if set.
    pub spider_home: Option<String>,
    /// Path of the running exe.
    pub exe: Option<PathBuf>,
    /// The source checkout this binary was compiled from. Baked in at build
    /// time, so on any other PC it simply does not exist.
    pub source_dir: PathBuf,
    /// `%LOCALAPPDATA%`.
    pub local_app_data: Option<PathBuf>,
}

/// Project root: where `data/` and `.env` live.
///
/// Worked out once, at the first call (which is at launch, while the exe is
/// certainly on disk), and kept. Resolving again on every call would let the
/// answer change under a running process: `current_exe()` keeps reporting the
/// old path after the exe is renamed aside for a rebuild, the checkout rule then
/// stops matching, and the next write would start a second, empty database in
/// `%LOCALAPPDATA%`.
pub fn project_root() -> PathBuf {
    static ROOT: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    ROOT.get_or_init(|| {
        resolve_root(&RootInputs {
            spider_home: std::env::var("SPIDER_HOME").ok(),
            exe: std::env::current_exe().ok(),
            source_dir: PathBuf::from(env!("CARGO_MANIFEST_DIR")),
            local_app_data: std::env::var_os("LOCALAPPDATA").map(PathBuf::from),
        })
    })
    .clone()
}

/// Pick the root, first match wins:
/// 1. `SPIDER_HOME`, an explicit choice.
/// 2. The exe's own folder, when it holds `data/` or `.env` (a portable copy).
/// 3. The source checkout, when the exe was built inside it (`cargo run`,
///    `tauri dev`, the nightly `target\release` job): the developer's data.
/// 4. `%LOCALAPPDATA%\Spider Charts Data`: an installed copy. This is the case
///    the compile-time path used to break, because on another PC it points at
///    a folder that is not there.
/// 5. The exe's folder, if there is no `%LOCALAPPDATA%` at all.
pub fn resolve_root(i: &RootInputs) -> PathBuf {
    if let Some(home) = i.spider_home.as_deref().map(str::trim).filter(|h| !h.is_empty()) {
        return PathBuf::from(home);
    }
    let exe_dir = i.exe.as_deref().and_then(Path::parent);
    if let Some(dir) = exe_dir {
        if dir.join("data").is_dir() || dir.join(".env").is_file() {
            return dir.to_path_buf();
        }
    }
    if let Some(exe) = &i.exe {
        if i.source_dir.join("Cargo.toml").is_file() && is_inside(exe, &i.source_dir) {
            return i.source_dir.clone();
        }
    }
    if let Some(base) = i.local_app_data.as_ref().filter(|b| !b.as_os_str().is_empty()) {
        return base.join(INSTALLED_HOME);
    }
    exe_dir.map(Path::to_path_buf).unwrap_or_else(|| i.source_dir.clone())
}

/// Whether `path` lies under `dir`. Both are canonicalised first: on Windows
/// the OS reports the on-disk spelling, so `c:\users\...` from a scheduled
/// task and `C:\Users\...` from cargo compare equal. A path that does not
/// exist is, by definition, not inside anything.
fn is_inside(path: &Path, dir: &Path) -> bool {
    match (fs::canonicalize(path), fs::canonicalize(dir)) {
        (Ok(p), Ok(d)) => p.starts_with(d),
        _ => false,
    }
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
    ///
    /// Currently read by nothing: `patterns::detect_all` runs over every stored
    /// bar. Kept so an existing `settings.json` still parses, and so the field
    /// is not silently re-added with a different meaning later.
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A scratch folder that deletes itself.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new() -> Self {
            static N: AtomicUsize = AtomicUsize::new(0);
            let dir = std::env::temp_dir().join(format!(
                "spider-config-test-{}-{}",
                std::process::id(),
                N.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
        fn dir(&self, rel: &str) -> PathBuf {
            let p = self.0.join(rel);
            fs::create_dir_all(&p).unwrap();
            p
        }
        fn file(&self, rel: &str) -> PathBuf {
            let p = self.0.join(rel);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(&p, b"x").unwrap();
            p
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// A checkout with a real `Cargo.toml`, an installed-looking exe elsewhere,
    /// and a `%LOCALAPPDATA%`. Tests override whichever field they care about.
    fn inputs(s: &Scratch) -> RootInputs {
        s.file("src-tree/Cargo.toml");
        RootInputs {
            spider_home: None,
            exe: Some(s.file("installed/Spider Charts.exe")),
            source_dir: s.dir("src-tree"),
            local_app_data: Some(s.dir("local")),
        }
    }

    #[test]
    fn spider_home_beats_everything() {
        let s = Scratch::new();
        let mut i = inputs(&s);
        s.dir("installed/data");
        i.spider_home = Some(s.0.join("chosen").to_string_lossy().into_owned());
        assert_eq!(resolve_root(&i), s.0.join("chosen"));
    }

    #[test]
    fn blank_spider_home_is_ignored() {
        let s = Scratch::new();
        let mut i = inputs(&s);
        i.spider_home = Some("   ".into());
        assert_eq!(resolve_root(&i), s.0.join("local").join(INSTALLED_HOME));
    }

    #[test]
    fn exe_folder_with_data_is_a_portable_copy() {
        let s = Scratch::new();
        let i = inputs(&s);
        s.dir("installed/data");
        assert_eq!(resolve_root(&i), s.0.join("installed"));
    }

    #[test]
    fn exe_folder_with_env_file_is_a_portable_copy() {
        let s = Scratch::new();
        let i = inputs(&s);
        s.file("installed/.env");
        assert_eq!(resolve_root(&i), s.0.join("installed"));
    }

    #[test]
    fn exe_built_inside_the_checkout_uses_the_checkout() {
        let s = Scratch::new();
        let mut i = inputs(&s);
        i.exe = Some(s.file("src-tree/target/release/spider_charts.exe"));
        assert_eq!(resolve_root(&i), s.0.join("src-tree"));
    }

    #[test]
    fn exe_outside_the_checkout_uses_local_app_data() {
        let s = Scratch::new();
        let i = inputs(&s);
        assert_eq!(resolve_root(&i), s.0.join("local").join(INSTALLED_HOME));
    }

    /// Another PC: the compile-time path does not exist there.
    #[test]
    fn missing_checkout_uses_local_app_data() {
        let s = Scratch::new();
        let mut i = inputs(&s);
        i.source_dir = s.0.join("no-such-folder");
        assert_eq!(resolve_root(&i), s.0.join("local").join(INSTALLED_HOME));
    }

    #[test]
    fn a_folder_without_cargo_toml_is_not_a_checkout() {
        let s = Scratch::new();
        let mut i = inputs(&s);
        i.source_dir = s.dir("plain-folder");
        i.exe = Some(s.file("plain-folder/Spider Charts.exe"));
        assert_eq!(resolve_root(&i), s.0.join("local").join(INSTALLED_HOME));
    }

    #[test]
    fn no_local_app_data_falls_back_to_the_exe_folder() {
        let s = Scratch::new();
        let mut i = inputs(&s);
        i.local_app_data = None;
        assert_eq!(resolve_root(&i), s.0.join("installed"));
    }

    /// A scheduled task may launch the exe by a lower-case spelling of its path.
    #[cfg(windows)]
    #[test]
    fn checkout_match_ignores_path_case_on_windows() {
        let s = Scratch::new();
        let mut i = inputs(&s);
        let real = s.file("src-tree/target/release/spider_charts.exe");
        i.exe = Some(PathBuf::from(real.to_string_lossy().to_lowercase()));
        assert_eq!(resolve_root(&i), s.0.join("src-tree"));
    }
}
