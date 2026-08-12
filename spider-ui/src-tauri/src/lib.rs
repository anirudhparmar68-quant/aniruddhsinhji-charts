//! Tauri bridge between the Spider Charts engine and the React front end.
//!
//! This layer holds no logic of its own on purpose. It owns the worker, turns
//! engine types into JSON the browser can render, and forwards commands. Any
//! rule about what a pattern *is* or which stock belongs in the universe stays
//! in the engine, where it is unit-tested and shared with the headless jobs.

use std::collections::HashMap;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};

use chrono::NaiveDate;
use serde::Serialize;
use tauri::Emitter;

use spider_charts::config::Settings;
use spider_charts::model::Candle;
use spider_charts::patterns::types::Detection;
use spider_charts::sync::{self, Command, Event, LatestSummary, SharedState};

// ---------------------------------------------------------------------------
// Wire types
// ---------------------------------------------------------------------------

#[derive(Serialize, Clone, Default)]
#[serde(rename_all = "camelCase")]
pub struct Status {
    pub busy: bool,
    pub message: String,
    pub last_sync: Option<String>,
    pub latest_session: Option<String>,
    pub universe_count: usize,
    pub total_instruments: usize,
    pub hit_count: usize,
    pub stale_count: usize,
    pub progress_done: usize,
    pub progress_total: usize,
    pub progress_label: String,
    /// Newest error, kept until the next command starts so it cannot be missed
    /// by a user who was looking at another tab when it happened.
    pub error: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StockDto {
    pub key: String,
    pub symbol: String,
    pub name: String,
    pub exchange: String,
    pub mcap_cr: Option<f64>,
    pub close: f64,
    pub change_pct: f64,
    /// Session volume over its 50-day average. 0 when unknown.
    pub volume_ratio: f64,
    pub rsi: f64,
    /// Fired the scan on the newest session.
    pub hit: bool,
    pub score: f64,
    pub bars: usize,
    /// No bar on the newest session — the stock simply did not trade.
    pub stale: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CandleDto {
    /// `YYYY-MM-DD`, the form lightweight-charts takes for daily series.
    pub time: String,
    pub open: f64,
    pub high: f64,
    pub low: f64,
    pub close: f64,
    pub volume: f64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MarkerDto {
    pub time: String,
    pub start_time: String,
    pub pattern: String,
    pub direction: String,
    pub score: f64,
    pub detail: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ChartDto {
    pub key: String,
    pub symbol: String,
    pub name: String,
    pub exchange: String,
    pub candles: Vec<CandleDto>,
    pub markers: Vec<MarkerDto>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ScanDto {
    pub key: String,
    pub symbol: String,
    pub name: String,
    pub exchange: String,
    pub date: String,
    pub pattern: String,
    pub direction: String,
    pub score: f64,
    pub detail: String,
    pub close: f64,
    pub change_pct: f64,
    pub volume_ratio: f64,
    pub rsi: f64,
}

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

pub struct AppState {
    /// `mpsc::Sender` is `Send` but not `Sync`, and Tauri state must be both.
    commands: Mutex<Sender<Command>>,
    shared: SharedState,
    status: Arc<Mutex<Status>>,
    /// The UI's copy of the settings. The worker holds its own, which is why
    /// saving has to both persist *and* send `UpdateSettings` — otherwise the
    /// next scan would still run with whatever was loaded at start-up.
    settings: Mutex<Settings>,
}

impl AppState {
    fn send(&self, cmd: Command) {
        if let Ok(tx) = self.commands.lock() {
            let _ = tx.send(cmd);
        }
    }
}

fn ymd(d: NaiveDate) -> String {
    d.format("%Y-%m-%d").to_string()
}

/// Guard every float that crosses into JSON.
///
/// `serde_json` writes NaN and infinity as `null`, not as an error. The front
/// end types these fields as numbers, so one bad value would reach
/// `(null).toFixed(2)`, take the render down, and leave a blank window with
/// nothing in the status bar to explain it. Zero is wrong too, but it is
/// visibly wrong in one cell instead of invisibly fatal for the whole list.
#[inline]
fn finite(x: f64) -> f64 {
    if x.is_finite() { x } else { 0.0 }
}

/// Percentage move of bar `i` against the one before it.
fn change_pct_at(candles: &[Candle], i: usize) -> f64 {
    if i == 0 || i >= candles.len() {
        return 0.0;
    }
    let prev = candles[i - 1].close;
    if prev <= 0.0 {
        0.0
    } else {
        (candles[i].close - prev) / prev * 100.0
    }
}

fn change_pct(candles: &[Candle]) -> f64 {
    change_pct_at(candles, candles.len().saturating_sub(1))
}

/// Which bar a scan row is talking about.
///
/// A detection carries a bar *index*, and an index is only meaningful against
/// the exact series it was computed over. Pruning old sessions shifts every
/// index by the number of bars dropped, so a stored index can quietly address a
/// different day. The row also carries the session *date*, which cannot drift,
/// so that is what is trusted; the index is only the fallback for a date that
/// is genuinely absent from the series.
fn bar_for(candles: &[Candle], date: NaiveDate, index_hint: usize) -> usize {
    match candles.binary_search_by(|c| c.date.cmp(&date)) {
        Ok(i) => i,
        Err(_) => index_hint.min(candles.len().saturating_sub(1)),
    }
}

/// Volume ratio and RSI **as they stood on bar `i`**.
///
/// The stored `latest_hits` summary only describes the newest session, so
/// reusing it for a hit from three days ago would print today's RSI beside a
/// setup that fired on Tuesday — a number that reads as evidence and is not.
fn context_at(candles: &[Candle], i: usize) -> (f64, f64) {
    if i >= candles.len() {
        return (0.0, 0.0);
    }
    let avg = spider_charts::ta::avg_volume_before(candles, i, 50);
    let volume_ratio = if avg > 0.0 { candles[i].volume as f64 / avg } else { 0.0 };

    let closes: Vec<f64> = candles[..=i].iter().map(|c| c.close).collect();
    let rsi = spider_charts::ta::rsi(&closes, 14)
        .last()
        .copied()
        .flatten()
        .unwrap_or(0.0);

    (volume_ratio, rsi)
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

#[tauri::command]
fn get_status(state: tauri::State<'_, AppState>) -> Status {
    let mut status = state.status.lock().map(|s| s.clone()).unwrap_or_default();
    let snap = sync::snapshot(&state.shared);
    status.universe_count = snap.included().len();
    status.total_instruments = snap.instruments.len();
    status.hit_count = snap.latest_hits.len();
    status.stale_count = snap.stale_count;
    status.last_sync = snap.last_sync.as_deref().map(friendly_time);
    status.latest_session = snap.latest_session.map(ymd);
    status
}

/// The store keeps `last_sync` as a UTC RFC 3339 stamp, which is the right
/// thing on disk and unreadable in a status bar. Shown in IST, since that is
/// the only market this app covers.
fn friendly_time(raw: &str) -> String {
    match chrono::DateTime::parse_from_rfc3339(raw) {
        Ok(t) => {
            let ist = chrono::FixedOffset::east_opt(5 * 3600 + 30 * 60).expect("IST offset is valid");
            t.with_timezone(&ist).format("%d %b, %H:%M").to_string()
        }
        // Never invent a time: an unparseable stamp is shown as it was stored.
        Err(_) => raw.to_string(),
    }
}

/// The full universe, alphabetically — the list the app navigates with arrows.
#[tauri::command]
fn get_universe(state: tauri::State<'_, AppState>) -> Vec<StockDto> {
    universe_rows(&sync::snapshot(&state.shared))
}

/// The command bodies live as plain functions over a snapshot so they can be
/// tested without a window, an event loop or a database. The `#[tauri::command]`
/// wrappers above them do nothing but take the snapshot.
fn universe_rows(snap: &sync::Shared) -> Vec<StockDto> {
    let latest = snap.latest_session;
    let empty: Vec<Candle> = Vec::new();

    let mut rows: Vec<StockDto> = snap
        .included()
        .into_iter()
        .map(|inst| {
            let candles = snap.candles.get(&inst.instrument_key).unwrap_or(&empty);
            let summary = snap.latest_hits.get(&inst.instrument_key);
            let last = candles.last();
            StockDto {
                key: inst.instrument_key.clone(),
                symbol: inst.symbol.clone(),
                name: inst.name.clone(),
                exchange: inst.exchange.as_str().to_string(),
                mcap_cr: inst.mcap_cr.map(finite),
                close: finite(last.map(|c| c.close).unwrap_or(0.0)),
                change_pct: finite(change_pct(candles)),
                volume_ratio: finite(summary.map(|s| s.volume_ratio).unwrap_or(0.0)),
                rsi: finite(summary.map(|s: &LatestSummary| s.rsi).unwrap_or(0.0)),
                hit: summary.is_some(),
                score: finite(summary.map(|s| s.best_score).unwrap_or(0.0)),
                bars: candles.len(),
                // No bar on the newest session — including no bars at all,
                // which is what a newly listed scrip looks like before its
                // first backfill. Both mean "there is nothing to read here
                // today", and calling the second one current would put a ₹0.00
                // close in the list with nothing to explain it.
                stale: match (latest, last) {
                    (Some(session), Some(c)) => c.date < session,
                    (Some(_), None) => true,
                    _ => false,
                },
            }
        })
        .collect();

    // Alphabetical, as the user asked for from the start. Done here rather than
    // in the front end so every consumer gets the same order.
    rows.sort_by(|a, b| a.symbol.cmp(&b.symbol));
    rows
}

/// Everything the chart pane needs for one stock, in a single round trip.
#[tauri::command]
fn get_chart(key: String, state: tauri::State<'_, AppState>) -> Option<ChartDto> {
    chart_for(&sync::snapshot(&state.shared), &key)
}

fn chart_for(snap: &sync::Shared, key: &str) -> Option<ChartDto> {
    let inst = snap.instruments.iter().find(|i| i.instrument_key == key)?;
    let candles = snap.candles.get(key).cloned().unwrap_or_default();

    let markers = snap
        .detections
        .get(key)
        .map(|dets| {
            // The engine hands detections back newest-first. Two things need
            // them the other way round: lightweight-charts requires markers in
            // ascending time order, and the chart's signal card reads the last
            // one as "the most recent" — which, unsorted, showed a setup from
            // six months ago beside a stock that fired today.
            let mut dets: Vec<&Detection> = dets.iter().collect();
            dets.sort_by_key(|d| d.end);
            dets.into_iter()
                .filter_map(|d| {
                    // A detection indexes bars, so a stale index would silently
                    // draw a marker on the wrong candle; drop it instead.
                    let end = candles.get(d.end)?;
                    let start = candles.get(d.start.min(d.end))?;
                    Some(MarkerDto {
                        time: ymd(end.date),
                        start_time: ymd(start.date),
                        pattern: d.kind.label().to_string(),
                        direction: d.direction.as_str().to_string(),
                        score: finite(d.score),
                        detail: d.detail.clone(),
                    })
                })
                .collect()
        })
        .unwrap_or_default();

    Some(ChartDto {
        key: key.to_string(),
        symbol: inst.symbol.clone(),
        name: inst.name.clone(),
        exchange: inst.exchange.as_str().to_string(),
        candles: candles
            .iter()
            .map(|c| CandleDto {
                time: ymd(c.date),
                open: finite(c.open),
                high: finite(c.high),
                low: finite(c.low),
                close: finite(c.close),
                volume: finite(c.volume as f64),
            })
            .collect(),
        markers,
    })
}

/// Scan hits, newest first. `days` counts back from the newest session in the
/// data rather than from the calendar — on a Sunday "today" holds nothing.
#[tauri::command]
fn get_scanner(days: Option<i64>, state: tauri::State<'_, AppState>) -> Vec<ScanDto> {
    scan_rows(&sync::snapshot(&state.shared), days)
}

fn scan_rows(snap: &sync::Shared, days: Option<i64>) -> Vec<ScanDto> {
    let cutoff = match (snap.latest_session, days) {
        (Some(latest), Some(d)) => Some(latest - chrono::Duration::days(d)),
        _ => None,
    };

    let by_key: HashMap<&str, &spider_charts::model::Instrument> = snap
        .instruments
        .iter()
        .map(|i| (i.instrument_key.as_str(), i))
        .collect();
    let empty: Vec<Candle> = Vec::new();

    let mut rows: Vec<ScanDto> = snap
        .scanner
        .iter()
        .filter(|r| cutoff.map(|c| r.date >= c).unwrap_or(true))
        .map(|r| {
            let candles = snap.candles.get(&r.instrument_key).unwrap_or(&empty);
            let bar = bar_for(candles, r.date, r.detection.end);
            let (volume_ratio, rsi) = context_at(candles, bar);
            ScanDto {
                key: r.instrument_key.clone(),
                symbol: r.symbol.clone(),
                name: r.name.clone(),
                exchange: by_key
                    .get(r.instrument_key.as_str())
                    .map(|i| i.exchange.as_str().to_string())
                    .unwrap_or_default(),
                date: ymd(r.date),
                pattern: r.detection.kind.label().to_string(),
                direction: r.detection.direction.as_str().to_string(),
                score: finite(r.detection.score),
                detail: r.detection.detail.clone(),
                // Everything below describes the bar the scan fired on, not the
                // newest one, so a row from an older window stays self-consistent.
                close: finite(candles.get(bar).map(|c| c.close).unwrap_or(0.0)),
                change_pct: finite(change_pct_at(candles, bar)),
                volume_ratio: finite(volume_ratio),
                rsi: finite(rsi),
            }
        })
        .collect();

    rows.sort_by(|a, b| {
        b.date
            .cmp(&a.date)
            .then(b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal))
    });
    rows
}

/// Kick off a background job. Named rather than typed so the front end never
/// has to mirror the engine's `Command` enum.
#[tauri::command]
fn run_job(name: String, state: tauri::State<'_, AppState>) -> Result<(), String> {
    if let Ok(mut s) = state.status.lock() {
        s.error = None;
    }
    match name.as_str() {
        "tail" => state.send(Command::TailUpdate),
        "rescan" => state.send(Command::ScanAll),
        "universe" => state.send(Command::RefreshUniverse),
        "backfill" => state.send(Command::Backfill),
        "full" => state.send(Command::FullSync),
        "login" => state.send(Command::Login),
        // `LoadFromDisk` republishes instruments and candles but leaves the
        // detections alone, so on its own it would advance the session date and
        // the prices while the setup list still described the previous one.
        // "Reload" has to mean both halves or it means nothing.
        "reload" => {
            state.send(Command::LoadFromDisk);
            state.send(Command::ScanAll);
        }
        other => return Err(format!("unknown job: {other}")),
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Settings
// ---------------------------------------------------------------------------

#[tauri::command]
fn get_settings(state: tauri::State<'_, AppState>) -> Settings {
    state
        .settings
        .lock()
        .map(|s| s.clone())
        .unwrap_or_else(|p| p.into_inner().clone())
}

/// Persist, hand the worker its new copy, and optionally rescan.
///
/// The rescan is the caller's choice because the two kinds of change differ:
/// a pattern threshold only shows up after detection re-runs, while a download
/// setting does nothing until the next sync and rescanning for it would be
/// thirty wasted seconds.
#[tauri::command]
fn save_settings(
    settings: Settings,
    rescan: bool,
    state: tauri::State<'_, AppState>,
) -> Result<(), String> {
    settings.save().map_err(|e| e.to_string())?;
    if let Ok(mut held) = state.settings.lock() {
        *held = settings.clone();
    }
    state.send(Command::UpdateSettings(Box::new(settings)));
    if rescan {
        state.send(Command::ScanAll);
    }
    Ok(())
}

/// Restore defaults without writing them, so the user can look before saving.
#[tauri::command]
fn default_settings() -> Settings {
    Settings::default()
}

// ---------------------------------------------------------------------------
// Export
// ---------------------------------------------------------------------------

/// Write a CSV the front end has already rendered.
///
/// The rows are composed in the UI on purpose: an export should be exactly the
/// list on screen — same filter, same sort, same columns — and regenerating it
/// here would mean keeping a second copy of that logic in step forever.
///
/// The directory and the extension are fixed here, and the caller's name is
/// reduced to plain characters by [`export_stem`], so a bad name cannot escape
/// `data/`.
#[tauri::command]
fn export_csv(name: String, contents: String) -> Result<String, String> {
    let stem = export_stem(&name)?;
    let path = spider_charts::config::data_dir().join(format!("{stem}.csv"));
    // A BOM so Excel opens it as UTF-8; without it, company names with
    // accented characters arrive mangled and look like a data fault.
    let mut bytes = Vec::with_capacity(contents.len() + 3);
    bytes.extend_from_slice(&[0xEF, 0xBB, 0xBF]);
    bytes.extend_from_slice(contents.as_bytes());

    std::fs::write(&path, bytes).map_err(|e| format!("writing {}: {e}", path.display()))?;
    Ok(path.display().to_string())
}

/// Reduce a requested export name to something that can only ever name a file
/// inside `data/`. Anything outside `[A-Za-z0-9_-]` becomes an underscore, so
/// separators, drive letters and `..` cannot survive.
fn export_stem(name: &str) -> Result<String, String> {
    let stem: String = name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect();
    let stem = stem.trim_matches('_').to_string();
    if stem.is_empty() {
        return Err("that export needs a name".into());
    }
    Ok(stem)
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

#[cfg_attr(mobile, tauri::mobile_entry_point)]
/// Settle WebView2 into what a single-page local tool needs.
///
/// Measured, not assumed: these flags are worth about 14 MB of the roughly
/// 410 MB the window costs, so they are **not** an answer to the memory
/// question — that number is Chromium's baseline and no flag removes it. They
/// are kept because a tool that reads a local SQLite file has no business
/// running background networking or a sync client, not because they made it
/// small. If the footprint ever matters more than the interface, the egui app
/// is still in this repo and still builds.
///
/// The GPU process is deliberately left alone: the chart is a canvas, and
/// taking its acceleration away to save a process would cost the smooth pan and
/// zoom the rewrite was for.
///
/// Appended to whatever is already in the environment rather than replacing it,
/// so `WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS=--remote-debugging-port=…` still
/// works — which is how this window gets inspected, there being no devtools in
/// a release build.
///
/// Must run before the webview is created, hence the first line of `run`.
fn tune_webview() {
    const KEY: &str = "WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS";
    const OURS: &str = "--disable-background-networking --disable-sync \
                        --disable-features=msWebOOUI,msPdfOOUI";

    let merged = match std::env::var(KEY) {
        Ok(existing) if !existing.trim().is_empty() => format!("{existing} {OURS}"),
        _ => OURS.to_string(),
    };
    // SAFETY: called once, at the top of `run`, before any thread is spawned.
    unsafe { std::env::set_var(KEY, merged) };
}

pub fn run() {
    tune_webview();
    let settings = Settings::load();
    let worker = sync::spawn(settings.clone());
    let status = Arc::new(Mutex::new(Status {
        message: "Starting".into(),
        ..Default::default()
    }));

    let state = AppState {
        commands: Mutex::new(worker.commands.clone()),
        shared: Arc::clone(&worker.state),
        status: Arc::clone(&status),
        settings: Mutex::new(settings),
    };

    // No start-up command is sent: the worker already loads the database and
    // scans it before it takes its first command, so the window has data as
    // soon as it appears. Asking again would re-read 115 MB for nothing.

    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .manage(state)
        .setup(move |app| {
            // The worker talks over an mpsc channel, which cannot be shared, so
            // one thread owns the receiver and republishes as Tauri events.
            let handle = app.handle().clone();
            let events = worker.events;
            let status = Arc::clone(&status);
            std::thread::Builder::new()
                .name("spider-events".into())
                .spawn(move || {
                    while let Ok(event) = events.recv() {
                        if let Ok(mut s) = status.lock() {
                            match &event {
                                Event::Status(msg) => s.message = msg.clone(),
                                Event::Busy(b) => {
                                    s.busy = *b;
                                    if !*b {
                                        s.progress_done = 0;
                                        s.progress_total = 0;
                                        s.progress_label.clear();
                                    }
                                }
                                Event::Progress { done, total, label } => {
                                    s.progress_done = *done;
                                    s.progress_total = *total;
                                    s.progress_label = label.clone();
                                }
                                Event::Error(msg) => {
                                    s.error = Some(msg.clone());
                                    s.message = msg.clone();
                                }
                                Event::DataChanged | Event::ScannerChanged => {}
                            }
                        }
                        let topic = match event {
                            Event::DataChanged => "engine:data",
                            Event::ScannerChanged => "engine:scanner",
                            _ => "engine:status",
                        };
                        let _ = handle.emit(topic, ());
                    }
                })
                .expect("spawning the event bridge");
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            get_status,
            get_universe,
            get_chart,
            get_scanner,
            run_job,
            get_settings,
            save_settings,
            default_settings,
            export_csv
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

#[cfg(test)]
mod tests {
    use super::*;
    use spider_charts::model::{Exchange, Instrument};
    use spider_charts::patterns::types::{Detection, PatternKind};
    use spider_charts::sync::{ScanRow, Shared};

    fn day(i: usize) -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 6, 1).expect("valid date") + chrono::Duration::days(i as i64)
    }

    /// 60 bars that drift gently upward, with one loud bar at index 50 and a
    /// much louder one at the end. Anything that quotes the newest bar when it
    /// was asked about bar 50 shows up immediately.
    fn series() -> Vec<Candle> {
        (0..60)
            .map(|i| {
                let close = match i {
                    50 => 110.0,
                    59 => 200.0,
                    _ => 100.0 + i as f64 * 0.1,
                };
                let volume = match i {
                    50 => 3_000,
                    59 => 9_000,
                    _ => 1_000,
                };
                Candle { date: day(i), open: close - 0.5, high: close + 1.0, low: close - 1.0, close, volume }
            })
            .collect()
    }

    fn instrument(symbol: &str, included: bool) -> Instrument {
        Instrument {
            instrument_key: format!("NSE_EQ|{symbol}"),
            symbol: symbol.into(),
            name: format!("{symbol} Ltd"),
            isin: format!("INE{symbol}01011"),
            exchange: Exchange::Nse,
            mcap_cr: Some(500.0),
            included,
        }
    }

    fn shared_with(instruments: Vec<Instrument>, candles: Vec<(String, Vec<Candle>)>) -> Shared {
        let candles: HashMap<String, Vec<Candle>> = candles.into_iter().collect();
        let latest = candles.values().filter_map(|s| s.last().map(|c| c.date)).max();
        Shared {
            instruments: Arc::new(instruments),
            candles: Arc::new(candles),
            latest_session: latest,
            ..Default::default()
        }
    }

    #[test]
    fn the_universe_is_alphabetical_and_excludes_what_the_engine_excluded() {
        let mut snap = shared_with(
            vec![instrument("ZYDUS", true), instrument("ABB", true), instrument("HIDDEN", false)],
            vec![
                ("NSE_EQ|ZYDUS".into(), series()),
                ("NSE_EQ|ABB".into(), series()),
                ("NSE_EQ|HIDDEN".into(), series()),
            ],
        );
        snap.stats = Default::default();

        let rows = universe_rows(&snap);
        let symbols: Vec<&str> = rows.iter().map(|r| r.symbol.as_str()).collect();
        assert_eq!(symbols, ["ABB", "ZYDUS"], "alphabetical, and excluded names must not appear");
        assert!(rows.iter().all(|r| !r.hit), "no summaries were supplied, so nothing fired");
    }

    #[test]
    fn a_stock_with_no_bar_on_the_newest_session_is_marked_but_not_dropped() {
        // ABB stops one session early — it simply did not trade that day.
        let short: Vec<Candle> = series().into_iter().take(59).collect();
        let snap = shared_with(
            vec![instrument("ABB", true), instrument("ZYDUS", true)],
            vec![("NSE_EQ|ABB".into(), short), ("NSE_EQ|ZYDUS".into(), series())],
        );

        let rows = universe_rows(&snap);
        assert_eq!(rows.len(), 2, "an untraded day is not a reason to hide a stock");
        assert!(rows[0].stale, "ABB has no bar on the newest session");
        assert!(!rows[1].stale);
    }

    /// The regression this exists for: the scanner used to read its volume and
    /// RSI out of `latest_hits`, which only ever describes the newest session.
    /// A setup from nine sessions ago then displayed today's numbers.
    #[test]
    fn a_scan_row_describes_the_bar_the_setup_fired_on() {
        let candles = series();
        let mut snap = shared_with(vec![instrument("ABB", true)], vec![("NSE_EQ|ABB".into(), candles.clone())]);
        snap.scanner = Arc::new(vec![ScanRow {
            instrument_key: "NSE_EQ|ABB".into(),
            symbol: "ABB".into(),
            name: "ABB Ltd".into(),
            date: day(50),
            detection: Detection::new(PatternKind::ChartinkCupBreakout, 20, 50, 0.8),
        }]);

        let rows = scan_rows(&snap, None);
        assert_eq!(rows.len(), 1);
        let r = &rows[0];

        assert_eq!(r.date, "2026-07-21", "the row is dated by its own bar");
        assert_eq!(r.close, candles[50].close, "close comes from bar 50, not the last bar");
        assert_ne!(r.close, candles[59].close);

        // Bar 50 traded 3,000 against a 1,000 average; the last bar traded 9,000.
        assert!((r.volume_ratio - 3.0).abs() < 1e-9, "got {}", r.volume_ratio);

        // Bar 50 rose ~5%; the last bar rose ~90%. Quoting the wrong one is loud.
        assert!(r.change_pct > 4.0 && r.change_pct < 6.0, "got {}", r.change_pct);

        // Pinned against the *other* bar's value. `r.rsi > 0` alone would pass
        // whether or not the fix is present, which is how the regression got in.
        let (_, rsi_at_bar_50) = context_at(&candles, 50);
        let (_, rsi_at_last) = context_at(&candles, candles.len() - 1);
        assert_ne!(rsi_at_bar_50, rsi_at_last, "the fixture must distinguish the two bars");
        assert_eq!(r.rsi, rsi_at_bar_50, "the row's RSI is bar 50's");
        assert_ne!(r.rsi, rsi_at_last, "and specifically not the newest bar's");
    }

    /// Pruning old sessions shifts every stored bar index. The row's own date
    /// cannot drift, so that is what decides which bar it describes.
    #[test]
    fn a_scan_row_follows_its_date_when_stored_indices_have_shifted() {
        // Five sessions pruned off the front: what was bar 50 is now bar 45.
        let pruned: Vec<Candle> = series().into_iter().skip(5).collect();
        let mut snap = shared_with(vec![instrument("ABB", true)], vec![("NSE_EQ|ABB".into(), pruned.clone())]);
        snap.scanner = Arc::new(vec![ScanRow {
            instrument_key: "NSE_EQ|ABB".into(),
            symbol: "ABB".into(),
            name: "ABB Ltd".into(),
            date: day(50),
            // The index the detection was stored with, now five bars out.
            detection: Detection::new(PatternKind::ChartinkCupBreakout, 20, 50, 0.8),
        }]);

        let rows = scan_rows(&snap, None);
        assert_eq!(rows[0].date, "2026-07-21");
        assert_eq!(rows[0].close, 110.0, "the loud bar, found by date rather than by a stale index");
        assert!((rows[0].volume_ratio - 3.0).abs() < 1e-9, "got {}", rows[0].volume_ratio);
        assert_ne!(rows[0].close, pruned[50].close, "index 50 now points somewhere else entirely");
    }

    #[test]
    fn a_stock_with_no_history_at_all_is_marked_rather_than_shown_as_current() {
        let snap = shared_with(
            vec![instrument("ABB", true), instrument("NEW", true)],
            vec![("NSE_EQ|ABB".into(), series()), ("NSE_EQ|NEW".into(), Vec::new())],
        );
        let rows = universe_rows(&snap);
        let newly_listed = rows.iter().find(|r| r.symbol == "NEW").expect("listed");
        assert_eq!(newly_listed.bars, 0);
        assert!(newly_listed.stale, "never downloaded is not the same as up to date");
    }

    #[test]
    fn the_window_counts_back_from_the_newest_session_not_from_today() {
        let candles = series();
        let row = |bar: usize| ScanRow {
            instrument_key: "NSE_EQ|ABB".into(),
            symbol: "ABB".into(),
            name: "ABB Ltd".into(),
            date: day(bar),
            detection: Detection::new(PatternKind::ChartinkCupBreakout, bar - 20, bar, 0.7),
        };
        let mut snap = shared_with(vec![instrument("ABB", true)], vec![("NSE_EQ|ABB".into(), candles)]);
        snap.scanner = Arc::new(vec![row(50), row(59)]);

        // These dates are in the past relative to any real clock, which is the
        // point: "latest" must mean the newest *session*, never today's date.
        assert_eq!(scan_rows(&snap, Some(0)).len(), 1, "only the newest session");
        assert_eq!(scan_rows(&snap, Some(30)).len(), 2, "both fall inside a month");
        assert_eq!(scan_rows(&snap, None).len(), 2, "no window means everything");
        assert_eq!(scan_rows(&snap, Some(0))[0].date, "2026-07-30");
    }

    #[test]
    fn markers_that_point_past_the_series_are_dropped_rather_than_misplaced() {
        let candles = series();
        let mut detections = HashMap::new();
        detections.insert(
            "NSE_EQ|ABB".to_string(),
            vec![
                Detection::new(PatternKind::ChartinkCupBreakout, 20, 50, 0.8),
                // Stale index, e.g. history was pruned under a cached detection.
                Detection::new(PatternKind::ChartinkCupBreakout, 900, 999, 0.9),
            ],
        );
        let mut snap = shared_with(vec![instrument("ABB", true)], vec![("NSE_EQ|ABB".into(), candles)]);
        snap.detections = Arc::new(detections);

        let chart = chart_for(&snap, "NSE_EQ|ABB").expect("the stock exists");
        assert_eq!(chart.candles.len(), 60);
        assert_eq!(chart.markers.len(), 1, "a marker with no bar to sit on must not be drawn");
        assert_eq!(chart.markers[0].time, "2026-07-21");
        assert_eq!(chart.markers[0].start_time, "2026-06-21");
    }

    /// The engine returns detections newest-first. The chart needs the opposite:
    /// lightweight-charts requires ascending markers, and the signal card reads
    /// the last one as "most recent" — which showed a six-month-old setup beside
    /// a stock that had fired that morning.
    #[test]
    fn markers_come_back_oldest_first_whatever_order_the_engine_used() {
        let mut detections = HashMap::new();
        detections.insert(
            "NSE_EQ|ABB".to_string(),
            vec![
                Detection::new(PatternKind::ChartinkCupBreakout, 40, 59, 0.7),
                Detection::new(PatternKind::ChartinkCupBreakout, 30, 50, 0.8),
                Detection::new(PatternKind::ChartinkCupBreakout, 5, 25, 0.6),
            ],
        );
        let mut snap = shared_with(vec![instrument("ABB", true)], vec![("NSE_EQ|ABB".into(), series())]);
        snap.detections = Arc::new(detections);

        let chart = chart_for(&snap, "NSE_EQ|ABB").expect("the stock exists");
        let times: Vec<&str> = chart.markers.iter().map(|m| m.time.as_str()).collect();
        assert_eq!(times, ["2026-06-26", "2026-07-21", "2026-07-30"]);
        assert_eq!(
            chart.markers.last().expect("three markers").time,
            "2026-07-30",
            "the newest marker must be last — the chart's signal card reads it from there"
        );
    }

    #[test]
    fn a_sync_time_is_shown_in_ist_and_a_broken_one_is_not_invented() {
        // 18:15 UTC is 23:45 IST the same day.
        assert_eq!(friendly_time("2026-08-12T18:15:30.568613500+00:00"), "12 Aug, 23:45");
        // And past midnight IST the date must roll forward, not stay on the UTC day.
        assert_eq!(friendly_time("2026-08-12T20:00:00+00:00"), "13 Aug, 01:30");
        assert_eq!(friendly_time("not a timestamp"), "not a timestamp");
    }

    #[test]
    fn an_unknown_instrument_returns_nothing_rather_than_an_empty_chart() {
        let snap = shared_with(vec![instrument("ABB", true)], vec![("NSE_EQ|ABB".into(), series())]);
        assert!(chart_for(&snap, "NSE_EQ|NOPE").is_none());
    }

    #[test]
    fn export_names_cannot_escape_the_data_folder() {
        assert_eq!(export_stem("scanner_export").unwrap(), "scanner_export");
        assert_eq!(export_stem("../../Windows/System32/evil").unwrap(), "Windows_System32_evil");
        // Both the colon and the separator are replaced, hence two underscores.
        assert_eq!(export_stem("C:\\Users\\admin\\notes").unwrap(), "C__Users_admin_notes");
        assert_eq!(export_stem("a b.csv").unwrap(), "a_b_csv");
        for bad in ["../secret", "C:\\Users\\admin\\notes", "a/b", "a\\b", "..\\..\\x"] {
            let stem = export_stem(bad).unwrap();
            assert!(
                !stem.contains(['/', '\\', ':', '.']),
                "{bad} reduced to {stem}, which can still address another folder"
            );
        }
        assert!(export_stem("...").is_err(), "a name that reduces to nothing is refused");
        assert!(export_stem("").is_err());
    }

    /// A stock can hold one bar, or none at all, during a first backfill.
    #[test]
    fn short_and_empty_series_do_not_panic() {
        for n in [0usize, 1, 2, 3] {
            let candles: Vec<Candle> = series().into_iter().take(n).collect();
            let snap = shared_with(vec![instrument("ABB", true)], vec![("NSE_EQ|ABB".into(), candles)]);

            let rows = universe_rows(&snap);
            assert_eq!(rows.len(), 1, "n={n}");
            assert_eq!(rows[0].bars, n, "n={n}");
            if n < 2 {
                assert_eq!(rows[0].change_pct, 0.0, "n={n}: with no prior bar there is no move to report");
            }

            let chart = chart_for(&snap, "NSE_EQ|ABB").unwrap_or_else(|| panic!("n={n}"));
            assert_eq!(chart.candles.len(), n, "n={n}");

            // And the scanner path, which indexes by the detection's own bar.
            let mut snap = snap;
            if n > 0 {
                snap.scanner = Arc::new(vec![ScanRow {
                    instrument_key: "NSE_EQ|ABB".into(),
                    symbol: "ABB".into(),
                    name: "ABB Ltd".into(),
                    date: day(n - 1),
                    detection: Detection::new(PatternKind::ChartinkCupBreakout, 0, n - 1, 0.6),
                }]);
                assert_eq!(scan_rows(&snap, None).len(), 1, "n={n}");
            }
        }
    }
}
