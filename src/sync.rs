//! The background worker: login, universe refresh, history backfill and scans.
//!
//! The UI never blocks on I/O. It pushes a [`Command`] down a channel and reads
//! [`Event`]s back; bulk data lands in [`SharedState`], which the UI only ever
//! read-locks. The whole daily series set is a few tens of megabytes, so keeping
//! it resident makes chart switching and re-scanning instant.

use crate::config::{Secrets, Settings};
use crate::model::{Candle, Instrument};
use crate::patterns::types::{Detection, Direction};
use crate::store;
use crate::universe::{self, MarketCaps, UniverseStats};
use crate::upstox::{auth, history, instruments as instr, http_client};
use crate::writelock::WriteLock;
use anyhow::Result;
use chrono::{Duration, FixedOffset, NaiveDate, Utc};
use futures::stream::{self, StreamExt};
use rayon::prelude::*;
use std::collections::HashMap;
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, RwLock};

/// Overlap re-requested on every top-up. Wide on purpose — see the note in
/// `backfill`; narrow tail requests come back stale.
const TAIL_OVERLAP_DAYS: i64 = 30;

/// How long a write operation waits for another process to finish before
/// giving up. Generous: a full backfill is minutes, and a scheduled run that
/// waits is far better than one that collides.
const WRITE_LOCK_WAIT: std::time::Duration = std::time::Duration::from_secs(20 * 60);

/// Fewest stored bars at which a stock counts as having its history. This is
/// also the minimum `decide_inclusion` needs to list a stock at all.
const MIN_HISTORY_BARS: i64 = 30;

/// The app's own startup scan waits only briefly: whatever is on disk is
/// already displayable, so blocking the window for twenty minutes to redo a
/// scan somebody else is doing would be the wrong trade.
const STARTUP_LOCK_WAIT: std::time::Duration = std::time::Duration::from_secs(45);

/// Commands that rewrite the database and therefore need the cross-process lock.
fn rewrites_database(cmd: &Command) -> bool {
    matches!(
        cmd,
        Command::RefreshUniverse
            | Command::Backfill
            | Command::ScanAll
            | Command::ScanOne(_)
            | Command::FullSync
            | Command::TailUpdate
    )
}

/// Calendar days of tail re-taken from the exchanges' own bhavcopy files after
/// every backfill. Ten covers a long weekend plus a holiday and still costs
/// only a handful of file downloads.
const BHAVCOPY_TAIL_DAYS: i64 = 10;

/// Trading day in India, so a late-evening run still means "today".
pub fn today_ist() -> NaiveDate {
    let ist = FixedOffset::east_opt(5 * 3600 + 30 * 60).expect("IST offset is valid");
    Utc::now().with_timezone(&ist).date_naive()
}

// ---------------------------------------------------------------------------
// Shared state
// ---------------------------------------------------------------------------

/// Everything worth knowing about one stock's newest candle, in one place.
///
/// This is the row a trader actually reasons about at night: not "a hammer
/// fired", but "three independent patterns agree, on 3× volume, 4% off the
/// 52-week high". Screeners give the first; almost none give the second.
#[derive(Debug, Clone, Default)]
pub struct LatestSummary {
    pub bullish: usize,
    pub bearish: usize,
    pub neutral: usize,
    /// Strongest single detection on the candle, 0..1.
    pub best_score: f64,
    /// Labels of the detections, strongest first.
    pub patterns: Vec<String>,
    pub close: f64,
    pub change_pct: f64,
    /// Session volume divided by its 50-day average.
    pub volume_ratio: f64,
    /// Distance below the 52-week (or as much as is held) high, as a fraction.
    pub below_52w_high: f64,
    pub rsi: f64,
}

impl LatestSummary {
    pub fn total(&self) -> usize {
        self.bullish + self.bearish + self.neutral
    }

    /// Signed conviction in roughly -1..=1. Positive is bullish.
    ///
    /// Three ingredients, deliberately kept interpretable: how strong the best
    /// pattern was, how much the patterns *agree* with each other, and how many
    /// there are. One 80% hammer should not outrank four patterns all pointing
    /// the same way, and two patterns in opposite directions should cancel.
    pub fn conviction(&self) -> f64 {
        let total = self.total();
        if total == 0 {
            return 0.0;
        }
        let net = self.bullish as f64 - self.bearish as f64;
        if net == 0.0 {
            return 0.0;
        }
        let agreement = net.abs() / total as f64; // 1.0 when every pattern agrees
        let depth = (total as f64 / 3.0).min(1.0); // three or more counts as full
        net.signum() * self.best_score * (0.4 + 0.6 * agreement) * (0.5 + 0.5 * depth)
    }
}

#[derive(Debug, Clone)]
pub struct ScanRow {
    pub instrument_key: String,
    pub symbol: String,
    pub name: String,
    pub date: NaiveDate,
    pub detection: Detection,
}

/// Immutable snapshot of everything the UI reads.
///
/// The heavy fields are behind `Arc` so cloning a `Shared` is a handful of
/// refcount bumps. That matters because of how it is published: the worker
/// swaps a whole new snapshot in under a momentary lock, and the UI clones the
/// pointer out under an equally momentary lock and then renders with **no lock
/// held at all**.
///
/// The earlier design had the UI hold read guards across rendering while the
/// worker took a write guard. On Windows `std::sync::RwLock` is an SRWLOCK,
/// which is neither fair nor safe against a second read on a thread that
/// already holds one while a writer is queued — and it deadlocked the entire
/// process, every thread parked, zero CPU. Snapshots remove the whole class.
#[derive(Default, Clone)]
pub struct Shared {
    pub instruments: Arc<Vec<Instrument>>,
    pub candles: Arc<HashMap<String, Vec<Candle>>>,
    pub detections: Arc<HashMap<String, Vec<Detection>>>,
    pub scanner: Arc<Vec<ScanRow>>,
    pub stats: UniverseStats,
    pub last_sync: Option<String>,
    /// The startup load of the database has finished, whether or not it found
    /// anything. Until then an empty snapshot means "not read yet", not "no
    /// data", and the window must not tell an existing user that nothing is
    /// downloaded.
    pub loaded: bool,
    /// Newest trading session present in the data.
    ///
    /// Deliberately not "today": on a weekend, a holiday, or before the evening
    /// sync the newest bar is an earlier session. Every "what fired on the
    /// latest candle" view keys off this, never off the calendar, otherwise the
    /// app would show an empty screen every Sunday.
    pub latest_session: Option<NaiveDate>,
    /// instrument_key → what happened on `latest_session`.
    pub latest_hits: Arc<HashMap<String, LatestSummary>>,
    /// Included stocks with no bar on `latest_session`.
    ///
    /// Since the tail comes from bhavcopy, and a bhavcopy lists only the scrips
    /// that actually traded, a missing bar means the stock **did not trade** —
    /// normal for illiquid names, several of which skip days routinely. It is
    /// worth showing so the Today counts add up, but it is not a fault and the
    /// user cannot fix it by re-syncing.
    pub stale_count: usize,
}

impl Shared {
    /// Instruments in the universe, alphabetically — the sidebar's source list.
    pub fn included(&self) -> Vec<&Instrument> {
        self.instruments.iter().filter(|i| i.included).collect()
    }
}

pub type SharedState = Arc<RwLock<Arc<Shared>>>;

/// Take a cheap snapshot. The lock is held only for the pointer clone, never
/// across rendering — see the note on [`Shared`].
pub fn snapshot(state: &SharedState) -> Arc<Shared> {
    match state.read() {
        Ok(guard) => Arc::clone(&guard),
        // A poisoned lock means a worker panicked mid-update. An empty snapshot
        // renders an empty app, which beats taking the UI down with it.
        Err(poisoned) => Arc::clone(&poisoned.into_inner()),
    }
}

// ---------------------------------------------------------------------------
// Channel protocol
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub enum Command {
    /// Make sure we hold a working access token (may open a browser).
    Login,
    /// Instrument master + market caps, then decide what to download.
    RefreshUniverse,
    /// Download whatever daily history is missing.
    Backfill,
    /// Re-run pattern detection over everything held in memory.
    ScanAll,
    /// Re-run detection for a single instrument (used when a chart is opened).
    ScanOne(String),
    /// Load whatever is already in the database, without touching the network.
    LoadFromDisk,
    /// Refresh → backfill → scan, in order.
    FullSync,
    /// Bring the newest sessions up to date from bhavcopy alone, then rescan.
    /// Needs no Upstox token, so it never has to open a browser.
    TailUpdate,
    /// Replace the worker's copy after the user edits settings. Without this the
    /// worker would keep scanning with whatever was loaded at start-up.
    UpdateSettings(Box<Settings>),
    Shutdown,
}

#[derive(Debug, Clone)]
pub enum Event {
    Status(String),
    Progress { done: usize, total: usize, label: String },
    Busy(bool),
    DataChanged,
    ScannerChanged,
    Error(String),
}

// ---------------------------------------------------------------------------
// Worker
// ---------------------------------------------------------------------------

pub struct Worker {
    pub commands: Sender<Command>,
    pub events: Receiver<Event>,
    pub state: SharedState,
}

/// Spawn the worker thread and hand back its channels.
pub fn spawn(settings: Settings) -> Worker {
    let (cmd_tx, cmd_rx) = std::sync::mpsc::channel::<Command>();
    let (evt_tx, evt_rx) = std::sync::mpsc::channel::<Event>();
    let state: SharedState = Arc::new(RwLock::new(Arc::new(Shared::default())));
    let worker_state = Arc::clone(&state);

    std::thread::Builder::new()
        .name("spider-worker".into())
        .spawn(move || {
            let runtime = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
                Ok(r) => r,
                Err(e) => {
                    let _ = evt_tx.send(Event::Error(format!("could not start the async runtime: {e}")));
                    return;
                }
            };
            let mut ctx = WorkerCtx {
                client: http_client(),
                token: None,
                settings,
                state: worker_state,
                events: evt_tx,
                last_failed: 0,
            };
            runtime.block_on(ctx.run(cmd_rx));
        })
        .expect("spawning the worker thread");

    Worker { commands: cmd_tx, events: evt_rx, state }
}

struct WorkerCtx {
    client: reqwest::Client,
    token: Option<String>,
    settings: Settings,
    state: SharedState,
    events: Sender<Event>,
    /// Downloads that did not land on the last backfill. A sync that leaves
    /// these behind must not be reported as complete.
    last_failed: usize,
}

impl WorkerCtx {
    fn status(&self, msg: impl Into<String>) {
        let _ = self.events.send(Event::Status(msg.into()));
    }

    fn progress(&self, done: usize, total: usize, label: impl Into<String>) {
        let _ = self.events.send(Event::Progress { done, total, label: label.into() });
    }

    fn fail(&self, context: &str, e: anyhow::Error) {
        let _ = self.events.send(Event::Error(format!("{context}: {e:#}")));
    }

    /// Publish a new snapshot. The write lock is held for one pointer swap and
    /// nothing else — no allocation, no I/O, no user code.
    fn publish(&self, mutate: impl FnOnce(&mut Shared)) {
        let mut next = (*snapshot(&self.state)).clone();
        mutate(&mut next);
        let next = Arc::new(next);
        match self.state.write() {
            Ok(mut guard) => *guard = next,
            Err(poisoned) => *poisoned.into_inner() = next,
        }
    }

    async fn run(&mut self, commands: Receiver<Command>) {
        // Show whatever is already on disk before any network call, then scan it.
        //
        // Detections live in SQLite but the in-memory view is what the UI reads,
        // so without this the app would open with a full stock list and an empty
        // scanner until the user thought to press Rescan. Re-running is cheap
        // (rayon over a few thousand series) and guarantees the results match
        // the current settings rather than whatever they were last night.
        if let Err(e) = self.load_from_disk() {
            self.fail("loading cached data", e);
        } else {
            match WriteLock::acquire(STARTUP_LOCK_WAIT, "desktop app startup") {
                Ok(Some(_lock)) => {
                    if let Err(e) = self.scan_all() {
                        self.fail("scanning cached data", e);
                    }
                }
                // Whatever is on disk is already loaded and displayable.
                Ok(None) => self.status(
                    "Another process is updating the database — showing the last saved scan.",
                ),
                Err(e) => self.fail("taking the write lock", e),
            }
        }

        while let Ok(cmd) = commands.recv() {
            let _ = self.events.send(Event::Busy(true));

            // One writer at a time across processes — the nightly job runs
            // whether or not this window is open.
            let lock = if rewrites_database(&cmd) {
                match WriteLock::acquire(WRITE_LOCK_WAIT, "desktop app") {
                    Ok(Some(lock)) => Some(lock),
                    Ok(None) => {
                        let _ = self.events.send(Event::Error(
                            "Another Spider Charts process is updating the database. \
                             Nothing was changed — try again once it finishes."
                                .to_string(),
                        ));
                        let _ = self.events.send(Event::Busy(false));
                        continue;
                    }
                    Err(e) => {
                        self.fail("taking the write lock", e);
                        let _ = self.events.send(Event::Busy(false));
                        continue;
                    }
                }
            } else {
                None
            };

            match cmd {
                Command::Shutdown => break,
                Command::Login => {
                    if let Err(e) = self.ensure_token().await {
                        self.fail("login", e);
                    }
                }
                Command::LoadFromDisk => {
                    if let Err(e) = self.load_from_disk() {
                        self.fail("loading cached data", e);
                    }
                }
                Command::RefreshUniverse => {
                    if let Err(e) = self.refresh_universe().await {
                        self.fail("refreshing the universe", e);
                    }
                }
                Command::Backfill => {
                    if let Err(e) = self.backfill().await {
                        self.fail("backfilling history", e);
                    }
                }
                Command::ScanAll => {
                    if let Err(e) = self.scan_all() {
                        self.fail("scanning", e);
                    }
                }
                Command::ScanOne(key) => {
                    if let Err(e) = self.scan_one(&key) {
                        self.fail("scanning one instrument", e);
                    }
                }
                Command::FullSync => {
                    if let Err(e) = self.full_sync().await {
                        self.fail("full sync", e);
                    }
                }
                Command::TailUpdate => {
                    if let Err(e) = self.tail_update().await {
                        self.fail("tail update", e);
                    }
                }
                Command::UpdateSettings(settings) => {
                    self.settings = *settings;
                    self.status("Settings applied — rescan to see them take effect");
                }
            }
            drop(lock);
            let _ = self.events.send(Event::Busy(false));
        }
    }

    async fn full_sync(&mut self) -> Result<()> {
        self.history_token().await?;
        self.refresh_universe().await?;
        self.backfill().await?;
        self.scan_all()?;
        // A first sync that lost stocks to throttling or a dropped network is
        // not finished, so it is not stamped: the "Continue download" bar stays
        // up until a run gets through. A copy that has synced before keeps
        // stamping as it always did.
        if self.last_failed == 0 || snapshot(&self.state).last_sync.is_some() {
            self.stamp_last_sync()?;
        }
        if self.last_failed > 0 {
            self.status(format!(
                "Sync finished, but {} stocks are still missing history — press History to retry them",
                self.last_failed
            ));
        } else {
            self.status("Sync complete");
        }
        Ok(())
    }

    /// Catch up on the newest sessions using only the exchanges' bhavcopy.
    ///
    /// The Upstox token dies at 03:30 IST every day and renewing it means a
    /// browser. A daily update should never need that — and it does not, because
    /// bhavcopy is public. Upstox is only required to *build* history; keeping
    /// it current is the exchanges' own files.
    async fn tail_update(&mut self) -> Result<()> {
        let targets: Vec<Instrument> = snapshot(&self.state).instruments.as_ref().clone();
        if targets.is_empty() {
            self.status("Nothing downloaded yet — run a Full sync first (no account needed, about 15 minutes).");
            return Ok(());
        }

        let written = self.bhavcopy_topup(&targets, today_ist()).await?;
        self.load_from_disk()?;
        self.scan_all()?;
        self.stamp_last_sync()?;
        self.status(format!("Tail update complete — {written} bars refreshed"));
        Ok(())
    }

    /// Record that a sync finished, in the database and in the snapshot the UI
    /// reads. The snapshot only learns `last_sync` when it reloads from disk, so
    /// stamping the database alone left it stale until the next launch, and a
    /// fresh install looked as if its first sync had never finished.
    fn stamp_last_sync(&self) -> Result<()> {
        let now = Utc::now().to_rfc3339();
        let conn = store::open()?;
        store::set_meta(&conn, "last_sync", &now)?;
        self.publish(move |shared| shared.last_sync = Some(now));
        Ok(())
    }

    async fn ensure_token(&mut self) -> Result<()> {
        if self.token.is_some() {
            return Ok(());
        }
        self.status("Checking the Upstox access token…");
        let secrets = Secrets::load()?;
        let token = auth::ensure_token(&self.client, &secrets).await?;
        self.token = Some(token);
        self.status("Upstox token ready");
        Ok(())
    }

    /// What to send with history requests: the user's token when they have
    /// Upstox keys, otherwise nothing.
    ///
    /// Upstox serves daily candles to anonymous callers (it ignores the
    /// `Authorization` header on that endpoint), so a copy installed on a PC
    /// with no API keys can still build its own history. A user who *does*
    /// have keys is unaffected: the token is still fetched, and renewed through
    /// the browser, exactly as before.
    async fn history_token(&mut self) -> Result<String> {
        if let Some(token) = &self.token {
            return Ok(token.clone());
        }
        if Secrets::load().is_err() {
            self.status("No Upstox login set up — using Upstox's public history feed");
            return Ok(String::new());
        }
        self.ensure_token().await?;
        Ok(self.token.clone().unwrap_or_default())
    }

    // -- universe ----------------------------------------------------------

    async fn refresh_universe(&mut self) -> Result<()> {
        self.status("Downloading the NSE + BSE instrument master…");
        let mut instruments =
            instr::load_equities(&self.client, self.settings.prefer_nse, &self.settings.bse_groups)
                .await?;
        self.status(format!("{} equities listed across NSE + BSE", instruments.len()));

        let mut caps = MarketCaps::default();
        universe::write_overrides_template().ok();
        let overrides = caps.load_overrides().unwrap_or(0);

        // Bulk market caps are opt-in; see Settings::mcap_source_url.
        let source = self.settings.mcap_source_url.trim().to_string();
        if !source.is_empty() {
            self.status("Fetching bulk market caps…");
            match caps.load_from_page(&self.client, &source).await {
                Ok(n) => self.status(format!("market caps loaded for {n} companies")),
                Err(e) => {
                    // Non-fatal by design: unresolved names fall through to the
                    // turnover test instead of vanishing from the universe.
                    let _ = self.events.send(Event::Error(format!(
                        "market-cap source unavailable ({e:#}) — falling back to the liquidity filter"
                    )));
                }
            }
        } else if caps.is_empty() {
            // Not an error: it is the normal state of a fresh install, and the
            // app works as designed. An error here sat in the status bar, in
            // red, for the whole of a first download.
            self.status(
                "No market-cap list, so the universe is filtered on exchange tier, price and \
                 traded value. To add a market-cap floor, put a screener export in \
                 data/mcap_overrides.csv",
            );
        }

        let resolved = universe::apply_market_caps(&mut instruments, &caps);
        let mut stats = UniverseStats { total: instruments.len(), mcap_resolved: resolved, ..Default::default() };
        if overrides > 0 {
            self.status(format!("{overrides} market caps came from your overrides file"));
        }

        // Provisional inclusion so the sidebar is usable before any history lands.
        for inst in instruments.iter_mut() {
            inst.included = universe::worth_downloading(inst, &self.settings);
        }
        stats.passed_on_mcap = instruments
            .iter()
            .filter(|i| i.mcap_cr.map(|m| m >= self.settings.min_mcap_cr).unwrap_or(false))
            .count();

        let mut conn = store::open()?;
        store::save_instruments(&mut conn, &instruments)?;

        self.publish(move |shared| {
            shared.instruments = Arc::new(instruments);
            shared.stats = stats;
        });
        let _ = self.events.send(Event::DataChanged);
        Ok(())
    }

    // -- history -----------------------------------------------------------

    async fn backfill(&mut self) -> Result<()> {
        let token = self.history_token().await?;

        let targets: Vec<Instrument> = snapshot(&self.state)
            .instruments
            .iter()
            .filter(|i| universe::worth_downloading(i, &self.settings))
            .cloned()
            .collect();
        if targets.is_empty() {
            self.status("No instruments to download — refresh the universe first.");
            return Ok(());
        }

        let to = today_ist();
        let earliest = to - Duration::days(self.settings.history_days);
        let conn = store::open()?;

        // Resume points, so a re-run only fetches the missing tail.
        let mut jobs = Vec::with_capacity(targets.len());
        for inst in &targets {
            let from = match store::last_candle_date(&conn, &inst.instrument_key)? {
                // A stock with only a few bars has not had its history
                // downloaded, whatever its newest date says. An Update before the
                // first download finished tops every stock up with the last week
                // from bhavcopy, which would otherwise pass for "already current"
                // here and leave it without its year for good.
                Some(_) if store::candle_count_for(&conn, &inst.instrument_key)? < MIN_HISTORY_BARS => {
                    earliest
                }
                Some(last) if last >= to => continue,
                // Deliberately a wide overlap rather than "resume from the last
                // stored bar". Upstox will answer a narrow tail request with a
                // stale copy that omits the newest session — measured on the
                // same instrument, the same minute: a 5-day window returned
                // Friday's close while a 30-day window returned Monday's.
                Some(last) => (last - Duration::days(TAIL_OVERLAP_DAYS)).max(earliest),
                None => earliest,
            };
            jobs.push((inst.clone(), from));
        }
        drop(conn);

        let total = jobs.len();
        if total == 0 {
            self.status("History already up to date");
            return Ok(());
        }
        self.status(format!("Downloading daily history for {total} stocks…"));

        let client = self.client.clone();
        let concurrency = self.settings.max_concurrency.clamp(1, 32);
        let pacer = Arc::new(history::Pacer::per_second(self.settings.requests_per_second));
        let mut stream = stream::iter(jobs.into_iter().map(|(inst, from)| {
            let client = client.clone();
            let token = token.clone();
            let pacer = Arc::clone(&pacer);
            async move {
                let result =
                    history::fetch_daily(&client, &token, &pacer, &inst.instrument_key, from, to).await;
                (inst, result)
            }
        }))
        .buffer_unordered(concurrency);

        // Writes stay on this task so SQLite only ever sees one writer.
        let mut conn = store::open()?;
        let (mut ok, mut empty, mut failed, mut done) = (0usize, 0usize, 0usize, 0usize);

        while let Some((inst, result)) = stream.next().await {
            done += 1;
            match result {
                Ok(history::FetchOutcome::Candles(candles)) => {
                    store::save_candles(&mut conn, &inst.instrument_key, &candles)?;
                    ok += 1;
                }
                Ok(history::FetchOutcome::NoData) => empty += 1,
                Err(_) => failed += 1,
            }
            if done % 25 == 0 || done == total {
                self.progress(done, total, format!("{} · {}", inst.symbol, inst.exchange.as_str()));
            }
        }

        drop(conn);
        // The exchanges get the last word on the tail.
        self.bhavcopy_topup(&targets, to).await?;

        let conn = store::open()?;
        store::prune_before(&conn, earliest)?;
        self.last_failed = failed;

        if failed > 0 {
            // Loud, because a quiet failure here reads as "these stocks have no
            // data" for the rest of the app — which is exactly what happened
            // the first time: 1,656 instruments came back empty and the sync
            // still announced itself as complete.
            let _ = self.events.send(Event::Error(format!(
                "{failed} of {total} downloads failed — this is almost always Upstox throttling, \
                 not missing data. Nothing is lost: press History again and only the missing \
                 stocks are retried. If it keeps happening, lower “Requests per second” in Settings."
            )));
        }
        self.status(format!(
            "History: {ok} downloaded, {empty} with no data, {failed} failed"
        ));

        self.load_from_disk()?;
        Ok(())
    }

    /// Overwrite the recent tail from the exchanges' own daily files.
    ///
    /// This is the fix for the problem Upstox cannot be trusted on: its
    /// historical endpoint intermittently answers with a cached tail that omits
    /// the newest session, and which answer you get depends on the window you
    /// asked for. A bhavcopy is one file per exchange per session covering every
    /// scrip that traded, published by the exchange itself — nothing to cache
    /// wrong, nothing to rate-limit, and it carries ISIN so matching is exact.
    ///
    /// Days that return nothing are weekends, holidays, or a session not yet
    /// published; all three are normal and none is an error.
    async fn bhavcopy_topup(&mut self, targets: &[Instrument], to: NaiveDate) -> Result<usize> {
        let known: std::collections::HashSet<&str> =
            targets.iter().map(|i| i.instrument_key.as_str()).collect();

        let mut conn = store::open()?;
        let mut written = 0usize;
        let mut sessions = 0usize;

        self.status("Taking the recent sessions from NSE + BSE bhavcopy…");
        for back in 0..BHAVCOPY_TAIL_DAYS {
            let date = to - Duration::days(back);
            let day = match crate::bhavcopy::fetch_day(&self.client, date).await {
                Ok(day) => day,
                Err(e) => {
                    let _ = self
                        .events
                        .send(Event::Error(format!("bhavcopy for {date} unavailable: {e:#}")));
                    continue;
                }
            };
            if day.is_empty() {
                continue;
            }

            let rows: Vec<(String, Candle)> = day
                .into_iter()
                .filter(|(key, _)| known.contains(key.as_str()))
                .collect();
            if rows.is_empty() {
                continue;
            }
            sessions += 1;
            written += store::save_session(&mut conn, &rows)?;
            self.progress(
                sessions,
                BHAVCOPY_TAIL_DAYS as usize,
                format!("{date} · {} stocks", rows.len()),
            );
        }

        self.status(format!("Bhavcopy: {sessions} sessions confirmed, {written} bars written"));
        Ok(written)
    }

    // -- disk → memory ------------------------------------------------------

    fn load_from_disk(&mut self) -> Result<()> {
        let conn = store::open()?;
        let mut instruments = store::load_instruments(&conn, false)?;
        if instruments.is_empty() {
            self.publish(|shared| shared.loaded = true);
            return Ok(());
        }
        self.status("Loading price history…");

        let candles = store::load_all_candles(&conn)?;

        // Final inclusion now that we can see liquidity and price.
        let allowlist = universe::Allowlist::load().unwrap_or_default();
        if !allowlist.is_empty() {
            self.status(format!(
                "Universe restricted to your {} listed symbols",
                allowlist.len()
            ));
        }
        let mut stats = UniverseStats { total: instruments.len(), ..Default::default() };
        for inst in instruments.iter_mut() {
            if inst.mcap_cr.is_some() {
                stats.mcap_resolved += 1;
            }
            let series = candles.get(&inst.instrument_key).map(|v| v.as_slice()).unwrap_or(&[]);
            inst.included = universe::decide_inclusion(inst, series, &self.settings, &allowlist);
            if inst.included {
                if inst.mcap_cr.is_some() {
                    stats.passed_on_mcap += 1;
                } else {
                    stats.passed_on_liquidity += 1;
                }
            } else {
                stats.excluded += 1;
            }
        }

        let mut write_conn = store::open()?;
        store::save_instruments(&mut write_conn, &instruments)?;
        let last_sync = store::get_meta(&conn, "last_sync")?;
        let latest_session = candles
            .values()
            .filter_map(|series| series.last().map(|c| c.date))
            .max();

        let included = instruments.iter().filter(|i| i.included).count();
        let stale_count = instruments
            .iter()
            .filter(|i| i.included)
            .filter(|i| {
                candles
                    .get(&i.instrument_key)
                    .and_then(|series| series.last())
                    .map(|bar| Some(bar.date) != latest_session)
                    .unwrap_or(true)
            })
            .count();

        self.publish(move |shared| {
            shared.instruments = Arc::new(instruments);
            shared.candles = Arc::new(candles);
            shared.stats = stats;
            shared.last_sync = last_sync;
            shared.latest_session = latest_session;
            shared.stale_count = stale_count;
            shared.loaded = true;
        });
        let _ = self.events.send(Event::DataChanged);

        if stale_count > 0 {
            // Status, not an error: nothing failed and there is nothing to fix.
            self.status(format!(
                "{stale_count} of {included} stocks did not trade on {}",
                latest_session.map(|d| d.to_string()).unwrap_or_default()
            ));
        }
        self.status(format!("{included} stocks in the universe"));
        Ok(())
    }

    // -- scanning -----------------------------------------------------------

    fn scan_all(&mut self) -> Result<()> {
        // Arc clones — the whole candle set is shared, never copied.
        let snap = snapshot(&self.state);
        let instruments = Arc::clone(&snap.instruments);
        let candles = Arc::clone(&snap.candles);
        drop(snap);

        let targets: Vec<&Instrument> = instruments.iter().filter(|i| i.included).collect();
        if targets.is_empty() {
            self.status("Nothing to scan yet — download history first.");
            return Ok(());
        }
        self.status(format!("Scanning {} stocks for patterns…", targets.len()));

        // Pure CPU work over independent series: rayon saturates every core.
        let results: Vec<(String, Vec<Detection>)> = targets
            .par_iter()
            .filter_map(|inst| {
                let series = candles.get(&inst.instrument_key)?;
                Some((
                    inst.instrument_key.clone(),
                    crate::patterns::detect_all(series, &self.settings.patterns),
                ))
            })
            .collect();

        let min_score = self.settings.min_pattern_score;
        let mut conn = store::open()?;
        let mut per_instrument: HashMap<String, Vec<Detection>> = HashMap::with_capacity(results.len());
        let mut total = 0usize;

        for (key, detections) in results {
            let kept: Vec<Detection> = detections.into_iter().filter(|d| d.score >= min_score).collect();
            if let Some(series) = candles.get(&key) {
                store::replace_detections(&mut conn, &key, series, &kept)?;
            }
            total += kept.len();
            per_instrument.insert(key, kept);
        }

        let scanner = self.build_scanner_rows(&instruments, &candles, &per_instrument);

        // Per-stock count of what fired on the newest session, so the sidebar
        // can surface "something happened here today" without re-scanning.
        let latest_session = candles
            .values()
            .filter_map(|series| series.last().map(|c| c.date))
            .max();
        let latest_hits = match latest_session {
            Some(latest) => build_latest_summaries(&scanner, &candles, latest),
            None => HashMap::new(),
        };

        self.publish(move |shared| {
            shared.detections = Arc::new(per_instrument);
            shared.scanner = Arc::new(scanner);
            shared.latest_session = latest_session;
            shared.latest_hits = Arc::new(latest_hits);
        });
        let _ = self.events.send(Event::ScannerChanged);
        self.status(format!("Scan complete — {total} patterns across {} stocks", targets.len()));
        Ok(())
    }

    fn scan_one(&mut self, key: &str) -> Result<()> {
        let series = snapshot(&self.state).candles.get(key).cloned();
        let Some(series) = series else { return Ok(()) };

        let detections: Vec<Detection> = crate::patterns::detect_all(&series, &self.settings.patterns)
            .into_iter()
            .filter(|d| d.score >= self.settings.min_pattern_score)
            .collect();

        let mut conn = store::open()?;
        store::replace_detections(&mut conn, key, &series, &detections)?;

        let key = key.to_string();
        self.publish(move |shared| {
            Arc::make_mut(&mut shared.detections).insert(key, detections);
        });
        let _ = self.events.send(Event::DataChanged);
        Ok(())
    }

    /// Flatten per-instrument detections into the scanner table, newest first.
    fn build_scanner_rows(
        &self,
        instruments: &[Instrument],
        candles: &HashMap<String, Vec<Candle>>,
        detections: &HashMap<String, Vec<Detection>>,
    ) -> Vec<ScanRow> {
        let mut rows = Vec::new();
        for inst in instruments.iter().filter(|i| i.included) {
            let (Some(series), Some(dets)) =
                (candles.get(&inst.instrument_key), detections.get(&inst.instrument_key))
            else {
                continue;
            };
            for d in dets {
                let Some(bar) = series.get(d.end) else { continue };
                rows.push(ScanRow {
                    instrument_key: inst.instrument_key.clone(),
                    symbol: inst.symbol.clone(),
                    name: inst.name.clone(),
                    date: bar.date,
                    detection: d.clone(),
                });
            }
        }
        rows.sort_by(|a, b| {
            b.date
                .cmp(&a.date)
                .then(b.detection.score.partial_cmp(&a.detection.score).unwrap_or(std::cmp::Ordering::Equal))
                .then(a.symbol.cmp(&b.symbol))
        });
        rows
    }
}

/// Roll every detection on the newest session up into one summary per stock,
/// and attach the price context a trader needs to act on it.
fn build_latest_summaries(
    scanner: &[ScanRow],
    candles: &HashMap<String, Vec<Candle>>,
    latest: NaiveDate,
) -> HashMap<String, LatestSummary> {
    let mut out: HashMap<String, LatestSummary> = HashMap::new();

    for row in scanner.iter().filter(|r| r.date == latest) {
        let entry = out.entry(row.instrument_key.clone()).or_default();
        match row.detection.direction {
            Direction::Bullish => entry.bullish += 1,
            Direction::Bearish => entry.bearish += 1,
            Direction::Neutral => entry.neutral += 1,
        }
        entry.best_score = entry.best_score.max(row.detection.score);
        entry.patterns.push(row.detection.kind.label().to_string());
    }

    for (key, summary) in out.iter_mut() {
        let Some(series) = candles.get(key) else { continue };
        let Some(last) = series.last() else { continue };
        // A stock whose newest bar predates the session has stale context; leave
        // it at zero rather than quoting yesterday's numbers as today's.
        if last.date != latest || series.len() < 2 {
            continue;
        }
        let end = series.len() - 1;
        summary.close = last.close;

        let prev = series[end - 1].close;
        if prev > 0.0 {
            summary.change_pct = (last.close - prev) / prev * 100.0;
        }

        let avg_volume = crate::ta::avg_volume_before(series, end, 50);
        if avg_volume > 0.0 {
            summary.volume_ratio = last.volume as f64 / avg_volume;
        }

        let lookback = 250.min(end);
        let high = crate::ta::highest_high(series, end - lookback, end);
        if high > 0.0 {
            summary.below_52w_high = ((high - last.close) / high).max(0.0);
        }

        let closes: Vec<f64> = series.iter().map(|c| c.close).collect();
        summary.rsi = crate::ta::rsi(&closes, 14).last().copied().flatten().unwrap_or(0.0);
    }
    out
}

/// Diagnostic: build the universe and report on it without touching the broker
/// account. Both the instrument master and the AMFI workbook are public, so
/// this needs no access token and places no orders — it exists to prove the
/// parsing, filtering and market-cap matching work against live data.
pub async fn run_universe_check(settings: Settings) -> Result<()> {
    let client = http_client();

    println!("Downloading the NSE + BSE instrument master…");
    let mut instruments = instr::load_equities(&client, settings.prefer_nse, &settings.bse_groups).await?;
    let nse = instruments.iter().filter(|i| i.exchange.as_str() == "NSE").count();
    let bse = instruments.len() - nse;
    println!("  {} equities after de-duplication  ({nse} NSE, {bse} BSE-only)", instruments.len());

    let mut caps = MarketCaps::default();
    universe::write_overrides_template().ok();
    match caps.load_overrides() {
        Ok(n) if n > 0 => println!("  {n} market caps from data/mcap_overrides.csv"),
        Ok(_) => {}
        Err(e) => println!("  overrides file unusable: {e:#}"),
    }

    let source = settings.mcap_source_url.trim().to_string();
    if !source.is_empty() {
        println!("Fetching bulk market caps from {source} …");
        match caps.load_from_page(&client, &source).await {
            Ok(n) => println!("  {n} companies in the workbook"),
            Err(e) => println!("  unavailable: {e:#}"),
        }
    }

    let resolved = universe::apply_market_caps(&mut instruments, &caps);
    let passing = instruments
        .iter()
        .filter(|i| i.mcap_cr.map(|m| m >= settings.min_mcap_cr).unwrap_or(false))
        .count();
    let unknown = instruments.len() - resolved;

    println!("\nUniverse at a ₹{:.0} crore floor:", settings.min_mcap_cr);
    println!("  market cap resolved   : {resolved}");
    println!("  clears the floor      : {passing}");
    println!("  unresolved market cap : {unknown}  (judged on traded value after a backfill)");
    println!(
        "  would be downloaded   : {}",
        instruments.iter().filter(|i| universe::worth_downloading(i, &settings)).count()
    );

    if resolved == 0 {
        println!(
            "\nNo market caps are loaded, so the ₹ floor is not being applied yet.\n\
             The universe above is filtered on exchange tier (NSE cash / BSE groups {}),\n\
             and will additionally be filtered on price and median traded value once\n\
             history is downloaded.\n\n\
             To apply a real market-cap floor: export a screener list with a market-cap\n\
             column into data/mcap_overrides.csv. A `symbol` column is enough — ISIN is\n\
             optional and extra columns are ignored.",
            settings.bse_groups.join("+")
        );
    }

    println!("\nFirst 12 alphabetically:");
    for inst in instruments.iter().take(12) {
        let mcap = match inst.mcap_cr {
            Some(m) => format!("₹{m:>12.0} cr"),
            None => "        unresolved".to_string(),
        };
        println!("  {:<14} {:<4} {mcap}  {}", inst.symbol, inst.exchange.as_str(), inst.name);
    }
    Ok(())
}

/// Headless full sync, for running nightly from Task Scheduler.
/// A worker wired to a stdout + file logger, for the two headless modes.
fn headless_ctx(settings: Settings) -> WorkerCtx {
    let (evt_tx, evt_rx) = std::sync::mpsc::channel::<Event>();
    std::thread::spawn(move || {
        // Release builds have no console, so a scheduled nightly run would
        // otherwise be silent. Everything also goes to a file that survives it.
        let log_path = crate::config::data_dir().join("sync.log");
        let mut file = std::fs::OpenOptions::new().create(true).append(true).open(&log_path).ok();

        let mut emit = |line: String| {
            println!("{line}");
            if let Some(f) = file.as_mut() {
                use std::io::Write;
                let _ = writeln!(f, "{line}");
                let _ = f.flush();
            }
        };

        emit(format!("[spider] ---- run started {} ----", Utc::now().to_rfc3339()));
        while let Ok(evt) = evt_rx.recv() {
            match evt {
                Event::Status(s) => emit(format!("[spider] {s}")),
                Event::Progress { done, total, label } => {
                    emit(format!("[spider] {done}/{total}  {label}"))
                }
                Event::Error(e) => emit(format!("[spider] ERROR {e}")),
                _ => {}
            }
        }
    });

    WorkerCtx {
        client: http_client(),
        token: None,
        settings,
        state: Arc::new(RwLock::new(Arc::new(Shared::default()))),
        events: evt_tx,
        last_failed: 0,
    }
}

/// Full sync: instrument master, market caps, Upstox history, bhavcopy tail,
/// scan. With Upstox keys configured it uses a token and will open a browser
/// when it has none; without keys it uses Upstox's public history feed.
pub async fn run_headless(settings: Settings) -> Result<()> {
    let mut ctx = headless_ctx(settings);
    let Some(_lock) = WriteLock::acquire(WRITE_LOCK_WAIT, "--sync")? else {
        ctx.status("Another Spider Charts process is writing — skipping this run.");
        return Ok(());
    };
    ctx.load_from_disk()?;
    ctx.full_sync().await
}

/// Daily update: newest sessions from bhavcopy, then rescan.
///
/// This is what a scheduled job should run. It touches no broker API, so it
/// cannot be blocked by an expired token or stall waiting on a login window.
pub async fn run_tail(settings: Settings) -> Result<()> {
    let mut ctx = headless_ctx(settings);
    // Waits rather than fails: a scheduled run that starts twenty minutes late
    // is fine, one that collides with the open app is not.
    let Some(_lock) = WriteLock::acquire(WRITE_LOCK_WAIT, "--tail")? else {
        ctx.status("Another Spider Charts process is writing — skipping this run.");
        return Ok(());
    };
    ctx.load_from_disk()?;
    ctx.tail_update().await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn summary(bullish: usize, bearish: usize, neutral: usize, best: f64) -> LatestSummary {
        LatestSummary { bullish, bearish, neutral, best_score: best, ..Default::default() }
    }

    #[test]
    fn no_patterns_means_no_conviction() {
        assert_eq!(summary(0, 0, 0, 0.0).conviction(), 0.0);
    }

    #[test]
    fn opposing_patterns_cancel_out() {
        assert_eq!(summary(2, 2, 0, 0.9).conviction(), 0.0);
        assert_eq!(summary(0, 0, 4, 1.0).conviction(), 0.0);
    }

    #[test]
    fn agreement_outranks_one_strong_pattern() {
        // The whole point of confluence: four patterns agreeing at 75% should
        // beat a single 95% signal standing alone.
        let lone = summary(1, 0, 0, 0.95);
        let crowd = summary(4, 0, 0, 0.75);
        assert!(crowd.conviction() > lone.conviction(), "{crowd:?} vs {lone:?}");
    }

    #[test]
    fn sign_follows_the_majority_direction() {
        assert!(summary(3, 1, 0, 0.8).conviction() > 0.0);
        assert!(summary(1, 3, 0, 0.8).conviction() < 0.0);
    }

    #[test]
    fn disagreement_dilutes_conviction() {
        let clean = summary(3, 0, 0, 0.8);
        let muddy = summary(3, 0, 3, 0.8);
        assert!(clean.conviction() > muddy.conviction());
    }

    #[test]
    fn conviction_stays_within_range() {
        for (b, s, n) in [(1, 0, 0), (5, 0, 0), (0, 5, 0), (3, 2, 1), (0, 0, 4)] {
            let c = summary(b, s, n, 1.0).conviction();
            assert!((-1.0..=1.0).contains(&c), "conviction {c} out of range for {b}/{s}/{n}");
        }
    }

    #[test]
    fn totals_count_every_direction() {
        assert_eq!(summary(2, 1, 3, 0.5).total(), 6);
    }

    fn empty_state() -> SharedState {
        Arc::new(RwLock::new(Arc::new(Shared::default())))
    }

    #[test]
    fn a_held_snapshot_does_not_block_a_publisher() {
        // The regression test for the deadlock that parked every thread: a
        // reader holds its view for a whole render pass while the worker
        // publishes. With snapshots this is simply two short, disjoint locks.
        let state = empty_state();
        let held = snapshot(&state);
        *state.write().expect("write must not block on a held snapshot") =
            Arc::new(Shared::default());
        drop(held);
    }

    #[test]
    fn nested_reads_on_one_thread_are_safe() {
        // The other half of the old hazard: taking a second read while already
        // holding one. Snapshots are plain `Arc`s, so this cannot deadlock.
        let state = empty_state();
        let outer = snapshot(&state);
        let inner = snapshot(&state);
        assert_eq!(outer.instruments.len(), inner.instruments.len());
    }

    #[test]
    fn a_snapshot_is_isolated_from_later_publishes() {
        let state = empty_state();
        let before = snapshot(&state);

        let mut next = (*snapshot(&state)).clone();
        next.latest_session = NaiveDate::from_ymd_opt(2026, 8, 7);
        *state.write().unwrap() = Arc::new(next);

        assert!(before.latest_session.is_none(), "an old snapshot must not mutate underneath its reader");
        assert!(snapshot(&state).latest_session.is_some());
    }

    #[test]
    fn stale_stocks_are_counted_not_hidden() {
        use crate::model::{Candle, Exchange, Instrument};
        use chrono::NaiveDate;

        let day = |d: u32| NaiveDate::from_ymd_opt(2026, 8, d).unwrap();
        let bar = |d: u32| Candle {
            date: day(d),
            open: 100.0,
            high: 101.0,
            low: 99.0,
            close: 100.5,
            volume: 1_000,
        };
        let inst = |key: &str| Instrument {
            instrument_key: key.into(),
            symbol: key.into(),
            name: String::new(),
            isin: String::new(),
            exchange: Exchange::Nse,
            mcap_cr: None,
            included: true,
        };

        let instruments = vec![inst("FRESH"), inst("STALE"), inst("ALSO_STALE")];
        let mut candles: HashMap<String, Vec<Candle>> = HashMap::new();
        candles.insert("FRESH".into(), vec![bar(7), bar(10)]);
        candles.insert("STALE".into(), vec![bar(6), bar(7)]);
        candles.insert("ALSO_STALE".into(), vec![bar(7)]);

        let latest = candles.values().filter_map(|s| s.last().map(|c| c.date)).max();
        assert_eq!(latest, Some(day(10)));

        let stale = instruments
            .iter()
            .filter(|i| i.included)
            .filter(|i| {
                candles
                    .get(&i.instrument_key)
                    .and_then(|s| s.last())
                    .map(|b| Some(b.date) != latest)
                    .unwrap_or(true)
            })
            .count();
        assert_eq!(stale, 2, "both stocks stuck on the 7th must be reported");
    }

    #[test]
    fn cloning_shared_only_bumps_refcounts() {
        // Cheap cloning is what makes publish-by-swap affordable; if a heavy
        // field ever loses its Arc this assertion is the tripwire.
        let mut original = Shared::default();
        Arc::make_mut(&mut original.candles).insert("NSE_EQ|X".into(), Vec::new());
        let copy = original.clone();
        assert!(Arc::ptr_eq(&original.candles, &copy.candles));
        assert!(Arc::ptr_eq(&original.scanner, &copy.scanner));
        assert!(Arc::ptr_eq(&original.detections, &copy.detections));
        assert!(Arc::ptr_eq(&original.instruments, &copy.instruments));
        assert!(Arc::ptr_eq(&original.latest_hits, &copy.latest_hits));
    }
}
