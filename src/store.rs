//! SQLite persistence: instruments, daily candles and scan results.
//!
//! `Connection` is not `Sync`, so every thread opens its own handle. WAL mode
//! lets the UI read while the background worker writes.

use crate::model::{Candle, Exchange, Instrument};
use crate::patterns::types::{Detection, Direction, PatternKind};
use anyhow::{Context, Result};
use chrono::NaiveDate;
use rusqlite::{params, Connection, OptionalExtension};

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS instruments (
    instrument_key TEXT PRIMARY KEY,
    symbol         TEXT NOT NULL,
    name           TEXT NOT NULL DEFAULT '',
    isin           TEXT NOT NULL DEFAULT '',
    exchange       TEXT NOT NULL,
    mcap_cr        REAL,
    included       INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS idx_instruments_symbol ON instruments(symbol);

CREATE TABLE IF NOT EXISTS candles (
    instrument_key TEXT NOT NULL,
    d              TEXT NOT NULL,
    o REAL NOT NULL, h REAL NOT NULL, l REAL NOT NULL, c REAL NOT NULL,
    v INTEGER NOT NULL,
    PRIMARY KEY (instrument_key, d)
) WITHOUT ROWID;

CREATE TABLE IF NOT EXISTS detections (
    instrument_key TEXT NOT NULL,
    d              TEXT NOT NULL,
    kind           TEXT NOT NULL,
    start_d        TEXT NOT NULL,
    direction      TEXT NOT NULL,
    score          REAL NOT NULL,
    detail         TEXT NOT NULL DEFAULT '',
    PRIMARY KEY (instrument_key, d, kind)
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS idx_detections_date ON detections(d);

CREATE TABLE IF NOT EXISTS meta (k TEXT PRIMARY KEY, v TEXT NOT NULL);

-- Splits, bonuses and flagged events NSE lists for a symbol. Prices in `candles`
-- stay raw; these are applied in memory when the data is loaded.
CREATE TABLE IF NOT EXISTS corp_actions (
    symbol  TEXT NOT NULL,
    ex_date TEXT NOT NULL,
    kind    TEXT NOT NULL,
    purpose TEXT NOT NULL,
    factor  REAL NOT NULL,
    PRIMARY KEY (symbol, ex_date, kind, purpose)
) WITHOUT ROWID;

-- Symbols NSE has been asked about, so each is looked up once and after that only
-- when a price gap suggests something new.
CREATE TABLE IF NOT EXISTS corp_checked (
    symbol     TEXT PRIMARY KEY,
    checked_on TEXT NOT NULL
) WITHOUT ROWID;
"#;

pub fn open() -> Result<Connection> {
    let path = crate::config::db_path();
    let conn = Connection::open(&path)
        .with_context(|| format!("opening database {}", path.display()))?;
    // `PRAGMA journal_mode` answers with a row, which `pragma_update` rejects —
    // run the pragmas as a batch so the returned value is simply discarded.
    conn.execute_batch(
        "PRAGMA journal_mode = WAL;
         PRAGMA synchronous = NORMAL;
         PRAGMA foreign_keys = ON;",
    )
    .context("applying SQLite pragmas")?;
    // Wait rather than fail when the background writer holds the lock.
    conn.busy_timeout(std::time::Duration::from_secs(20))?;
    conn.execute_batch(SCHEMA).context("creating schema")?;
    Ok(conn)
}

// ---------------------------------------------------------------------------
// Instruments
// ---------------------------------------------------------------------------

pub fn save_instruments(conn: &mut Connection, list: &[Instrument]) -> Result<()> {
    let tx = conn.transaction()?;
    {
        let mut stmt = tx.prepare(
            "INSERT INTO instruments (instrument_key, symbol, name, isin, exchange, mcap_cr, included)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(instrument_key) DO UPDATE SET
                symbol = excluded.symbol,
                name = excluded.name,
                isin = excluded.isin,
                exchange = excluded.exchange,
                mcap_cr = excluded.mcap_cr,
                included = excluded.included",
        )?;
        for i in list {
            stmt.execute(params![
                i.instrument_key,
                i.symbol,
                i.name,
                i.isin,
                i.exchange.as_str(),
                i.mcap_cr,
                i.included as i32,
            ])?;
        }
    }
    tx.commit()?;
    Ok(())
}

pub fn load_instruments(conn: &Connection, included_only: bool) -> Result<Vec<Instrument>> {
    let sql = if included_only {
        "SELECT instrument_key, symbol, name, isin, exchange, mcap_cr, included
         FROM instruments WHERE included = 1 ORDER BY symbol"
    } else {
        "SELECT instrument_key, symbol, name, isin, exchange, mcap_cr, included
         FROM instruments ORDER BY symbol"
    };
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt.query_map([], |row| {
        let exchange: String = row.get(4)?;
        Ok(Instrument {
            instrument_key: row.get(0)?,
            symbol: row.get(1)?,
            name: row.get(2)?,
            isin: row.get(3)?,
            exchange: Exchange::parse(&exchange).unwrap_or(Exchange::Nse),
            mcap_cr: row.get(5)?,
            included: row.get::<_, i32>(6)? != 0,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

// ---------------------------------------------------------------------------
// Candles
// ---------------------------------------------------------------------------

pub fn save_candles(conn: &mut Connection, key: &str, candles: &[Candle]) -> Result<usize> {
    if candles.is_empty() {
        return Ok(0);
    }
    let tx = conn.transaction()?;
    let mut written = 0usize;
    {
        let mut stmt = tx.prepare(
            "INSERT INTO candles (instrument_key, d, o, h, l, c, v)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(instrument_key, d) DO UPDATE SET
                o = excluded.o, h = excluded.h, l = excluded.l,
                c = excluded.c, v = excluded.v",
        )?;
        for c in candles {
            written += stmt.execute(params![
                key,
                c.date.to_string(),
                c.open,
                c.high,
                c.low,
                c.close,
                c.volume
            ])?;
        }
    }
    tx.commit()?;
    Ok(written)
}

/// Upsert one session across many instruments in a single transaction.
///
/// A bhavcopy covers a few thousand stocks at once; doing that as one
/// transaction per stock would mean thousands of fsyncs for one day's data.
pub fn save_session(conn: &mut Connection, rows: &[(String, Candle)]) -> Result<usize> {
    if rows.is_empty() {
        return Ok(0);
    }
    let tx = conn.transaction()?;
    let mut written = 0usize;
    {
        let mut stmt = tx.prepare(
            "INSERT INTO candles (instrument_key, d, o, h, l, c, v)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(instrument_key, d) DO UPDATE SET
                o = excluded.o, h = excluded.h, l = excluded.l,
                c = excluded.c, v = excluded.v",
        )?;
        for (key, candle) in rows {
            written += stmt.execute(params![
                key,
                candle.date.to_string(),
                candle.open,
                candle.high,
                candle.low,
                candle.close,
                candle.volume
            ])?;
        }
    }
    tx.commit()?;
    Ok(written)
}

pub fn load_candles(conn: &Connection, key: &str) -> Result<Vec<Candle>> {
    let mut stmt = conn.prepare(
        "SELECT d, o, h, l, c, v FROM candles WHERE instrument_key = ?1 ORDER BY d",
    )?;
    let rows = stmt.query_map([key], |row| {
        let d: String = row.get(0)?;
        Ok(Candle {
            date: d.parse().unwrap_or_else(|_| NaiveDate::from_ymd_opt(1970, 1, 1).unwrap()),
            open: row.get(1)?,
            high: row.get(2)?,
            low: row.get(3)?,
            close: row.get(4)?,
            volume: row.get(5)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Every stored series in one pass, keyed by instrument.
///
/// Loading per instrument means one query per stock — 2,161 round trips through
/// the statement machinery just to open the app. A single ordered scan grouped
/// in memory does the same work in one.
pub fn load_all_candles(conn: &Connection) -> Result<std::collections::HashMap<String, Vec<Candle>>> {
    let mut stmt = conn.prepare(
        "SELECT instrument_key, d, o, h, l, c, v FROM candles ORDER BY instrument_key, d",
    )?;
    let rows = stmt.query_map([], |row| {
        let key: String = row.get(0)?;
        let d: String = row.get(1)?;
        Ok((
            key,
            Candle {
                date: d.parse().unwrap_or_else(|_| NaiveDate::from_ymd_opt(1970, 1, 1).unwrap()),
                open: row.get(2)?,
                high: row.get(3)?,
                low: row.get(4)?,
                close: row.get(5)?,
                volume: row.get(6)?,
            },
        ))
    })?;

    let mut out: std::collections::HashMap<String, Vec<Candle>> = std::collections::HashMap::new();
    for row in rows {
        let (key, candle) = row?;
        out.entry(key).or_default().push(candle);
    }
    Ok(out)
}

/// Newest stored session for an instrument — the resume point for a top-up.
pub fn last_candle_date(conn: &Connection, key: &str) -> Result<Option<NaiveDate>> {
    let raw: Option<String> = conn
        .query_row(
            "SELECT MAX(d) FROM candles WHERE instrument_key = ?1",
            [key],
            |row| row.get(0),
        )
        .optional()?
        .flatten();
    Ok(raw.and_then(|s| s.parse().ok()))
}

// ---------------------------------------------------------------------------
// Corporate actions
// ---------------------------------------------------------------------------

/// Record what NSE listed for `symbol`, and note that it has been asked.
///
/// This adds to what is stored and never removes. A stored event is only replaced
/// by NSE's newer wording for the *same ex-date*; one the answer leaves out stays.
/// NSE's answers are not always complete (an empty one for a symbol it could not
/// look up is indistinguishable from a stock with no events), and wiping a stock's
/// stored splits on the strength of one would put a cliff back in its chart.
pub fn replace_corp_actions(
    conn: &mut Connection,
    symbol: &str,
    actions: &[crate::corp::Action],
    today: NaiveDate,
) -> Result<()> {
    let tx = conn.transaction()?;
    for a in actions {
        tx.execute(
            "DELETE FROM corp_actions WHERE symbol = ?1 AND ex_date = ?2",
            params![symbol, a.ex_date.to_string()],
        )?;
    }
    {
        let mut stmt = tx.prepare(
            "INSERT OR REPLACE INTO corp_actions (symbol, ex_date, kind, purpose, factor)
             VALUES (?1, ?2, ?3, ?4, ?5)",
        )?;
        for a in actions {
            stmt.execute(params![symbol, a.ex_date.to_string(), a.kind.as_str(), a.purpose, a.factor])?;
        }
    }
    tx.execute(
        "INSERT INTO corp_checked (symbol, checked_on) VALUES (?1, ?2)
         ON CONFLICT(symbol) DO UPDATE SET checked_on = excluded.checked_on",
        params![symbol, today.to_string()],
    )?;
    tx.commit()?;
    Ok(())
}

/// Everything stored, by symbol, oldest ex-date first.
pub fn load_corp_actions(
    conn: &Connection,
) -> Result<std::collections::HashMap<String, Vec<crate::corp::Action>>> {
    let mut stmt = conn.prepare(
        "SELECT symbol, ex_date, kind, purpose, factor FROM corp_actions ORDER BY symbol, ex_date",
    )?;
    let rows = stmt.query_map([], |row| {
        let date: String = row.get(1)?;
        let kind: String = row.get(2)?;
        Ok((
            row.get::<_, String>(0)?,
            date,
            crate::corp::Kind::parse(&kind),
            row.get::<_, String>(3)?,
            row.get::<_, f64>(4)?,
        ))
    })?;
    let mut out: std::collections::HashMap<String, Vec<crate::corp::Action>> = Default::default();
    for row in rows {
        let (symbol, date, kind, purpose, factor) = row?;
        // A row whose date cannot be read is skipped, not fatal: the rest of the
        // stock's events are still good.
        if let Ok(ex_date) = date.parse() {
            out.entry(symbol).or_default().push(crate::corp::Action { ex_date, kind, purpose, factor });
        }
    }
    Ok(out)
}

/// Symbols NSE was asked about on or after `since`. Anything not in here is due to
/// be asked again, which is how an event too mild to show as a price jump (a 1:10
/// bonus is a 9% fall) is found in the end.
pub fn corp_checked_since(conn: &Connection, since: NaiveDate) -> Result<std::collections::HashSet<String>> {
    let mut stmt = conn.prepare("SELECT symbol FROM corp_checked WHERE checked_on >= ?1")?;
    let rows = stmt.query_map([since.to_string()], |row| row.get::<_, String>(0))?;
    Ok(rows.collect::<std::result::Result<_, _>>()?)
}

/// The stored close of one instrument on one session, if there is one.
pub fn close_on(conn: &Connection, key: &str, date: NaiveDate) -> Result<Option<f64>> {
    Ok(conn
        .query_row(
            "SELECT c FROM candles WHERE instrument_key = ?1 AND d = ?2",
            params![key, date.to_string()],
            |row| row.get(0),
        )
        .optional()?)
}

/// NSE symbols of listed stocks whose close moved by at least `down` (a ratio such
/// as 0.9, meaning a 10% fall) or `up` (such as 1.5) from one session to the next,
/// on or after `since`. Those are the ones worth asking NSE about again.
pub fn gap_candidates(conn: &Connection, since: NaiveDate, down: f64, up: f64) -> Result<Vec<String>> {
    // The window function needs the session before `since` to measure the first
    // day's move, so it reads a little further back than it reports.
    let from = since - chrono::Duration::days(10);
    let mut stmt = conn.prepare(
        "SELECT DISTINCT i.symbol FROM (
             SELECT instrument_key, d, c,
                    LAG(c) OVER (PARTITION BY instrument_key ORDER BY d) AS prev_c
             FROM candles WHERE d >= ?1
         ) a JOIN instruments i ON i.instrument_key = a.instrument_key
         WHERE a.prev_c > 0 AND a.d >= ?2 AND i.exchange = 'NSE' AND i.included = 1
           AND (a.c / a.prev_c <= ?3 OR a.c / a.prev_c >= ?4)
         ORDER BY i.symbol",
    )?;
    let rows = stmt.query_map(params![from.to_string(), since.to_string(), down, up], |row| {
        row.get::<_, String>(0)
    })?;
    Ok(rows.collect::<std::result::Result<_, _>>()?)
}

/// How many sessions are stored for one instrument.
pub fn candle_count_for(conn: &Connection, key: &str) -> Result<i64> {
    Ok(conn.query_row(
        "SELECT COUNT(*) FROM candles WHERE instrument_key = ?1",
        [key],
        |r| r.get(0),
    )?)
}

pub fn candle_count(conn: &Connection) -> Result<i64> {
    Ok(conn.query_row("SELECT COUNT(*) FROM candles", [], |r| r.get(0))?)
}

/// Drop sessions older than the retention window so the file stays small.
pub fn prune_before(conn: &Connection, cutoff: NaiveDate) -> Result<usize> {
    Ok(conn.execute("DELETE FROM candles WHERE d < ?1", [cutoff.to_string()])?)
}

// ---------------------------------------------------------------------------
// Detections
// ---------------------------------------------------------------------------

/// A detection as persisted: bar indices are replaced by real dates because
/// indices are only meaningful within one in-memory series.
#[derive(Debug, Clone)]
pub struct StoredDetection {
    pub instrument_key: String,
    pub symbol: String,
    pub date: NaiveDate,
    pub start_date: NaiveDate,
    pub kind: PatternKind,
    pub direction: Direction,
    pub score: f64,
    pub detail: String,
}

/// Replace all stored detections for one instrument with a fresh scan.
pub fn replace_detections(
    conn: &mut Connection,
    key: &str,
    candles: &[Candle],
    detections: &[Detection],
) -> Result<()> {
    let tx = conn.transaction()?;
    tx.execute("DELETE FROM detections WHERE instrument_key = ?1", [key])?;
    {
        let mut stmt = tx.prepare(
            "INSERT INTO detections (instrument_key, d, kind, start_d, direction, score, detail)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(instrument_key, d, kind) DO UPDATE SET
                start_d = excluded.start_d, direction = excluded.direction,
                score = excluded.score, detail = excluded.detail",
        )?;
        for d in detections {
            let (Some(end), Some(start)) = (candles.get(d.end), candles.get(d.start)) else {
                continue;
            };
            stmt.execute(params![
                key,
                end.date.to_string(),
                d.kind.key(),
                start.date.to_string(),
                d.direction.as_str(),
                d.score,
                d.detail,
            ])?;
        }
    }
    tx.commit()?;
    Ok(())
}

/// Scanner feed: every detection at or after `since`, strongest first.
pub fn detections_since(conn: &Connection, since: NaiveDate) -> Result<Vec<StoredDetection>> {
    let mut stmt = conn.prepare(
        "SELECT d.instrument_key, i.symbol, d.d, d.start_d, d.kind, d.direction, d.score, d.detail
         FROM detections d
         JOIN instruments i ON i.instrument_key = d.instrument_key
         WHERE d.d >= ?1
         ORDER BY d.d DESC, d.score DESC, i.symbol",
    )?;
    let rows = stmt.query_map([since.to_string()], |row| {
        let kind_key: String = row.get(4)?;
        let direction: String = row.get(5)?;
        let date: String = row.get(2)?;
        let start_date: String = row.get(3)?;
        Ok((
            StoredDetection {
                instrument_key: row.get(0)?,
                symbol: row.get(1)?,
                date: date.parse().unwrap_or_else(|_| NaiveDate::from_ymd_opt(1970, 1, 1).unwrap()),
                start_date: start_date
                    .parse()
                    .unwrap_or_else(|_| NaiveDate::from_ymd_opt(1970, 1, 1).unwrap()),
                // Replaced below from the stored key; this only keeps the row shape.
                kind: PatternKind::ChartinkCupBreakout,
                direction: Direction::parse(&direction),
                score: row.get(6)?,
                detail: row.get(7)?,
            },
            kind_key,
        ))
    })?;

    let mut out = Vec::new();
    for row in rows {
        let (mut det, kind_key) = row?;
        // A catalogue entry can be renamed between versions; skip unknown rows
        // rather than guessing, they will be rewritten on the next scan.
        let Some(kind) = PatternKind::from_key(&kind_key) else { continue };
        det.kind = kind;
        out.push(det);
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Meta
// ---------------------------------------------------------------------------

pub fn set_meta(conn: &Connection, key: &str, value: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO meta (k, v) VALUES (?1, ?2)
         ON CONFLICT(k) DO UPDATE SET v = excluded.v",
        params![key, value],
    )?;
    Ok(())
}

pub fn get_meta(conn: &Connection, key: &str) -> Result<Option<String>> {
    Ok(conn
        .query_row("SELECT v FROM meta WHERE k = ?1", [key], |r| r.get(0))
        .optional()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn memory_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(SCHEMA).unwrap();
        conn
    }

    fn candle(day: u32, close: f64) -> Candle {
        Candle {
            date: NaiveDate::from_ymd_opt(2025, 8, day).unwrap(),
            open: close - 1.0,
            high: close + 1.0,
            low: close - 2.0,
            close,
            volume: 1_000,
        }
    }

    fn nse_stock(key: &str, symbol: &str, included: bool) -> Instrument {
        Instrument {
            instrument_key: key.into(),
            symbol: symbol.into(),
            name: symbol.into(),
            isin: String::new(),
            exchange: Exchange::Nse,
            mcap_cr: None,
            included,
        }
    }

    fn action(day: u32, kind: crate::corp::Kind, purpose: &str, factor: f64) -> crate::corp::Action {
        crate::corp::Action {
            ex_date: NaiveDate::from_ymd_opt(2025, 8, day).unwrap(),
            kind,
            purpose: purpose.into(),
            factor,
        }
    }

    #[test]
    fn corporate_actions_round_trip_and_are_replaced_not_piled_up() {
        use crate::corp::Kind;
        let mut conn = memory_db();
        let today = NaiveDate::from_ymd_opt(2025, 9, 1).unwrap();

        replace_corp_actions(&mut conn, "AASTHA", &[action(28, Kind::Bonus, "Bonus 1:1", 0.5)], today).unwrap();
        replace_corp_actions(&mut conn, "VEDL", &[action(30, Kind::Demerger, "Demerger", 1.0)], today).unwrap();
        let stored = load_corp_actions(&conn).unwrap();
        assert_eq!(stored["AASTHA"], vec![action(28, Kind::Bonus, "Bonus 1:1", 0.5)]);
        assert_eq!(stored["VEDL"][0].kind, Kind::Demerger);

        // An empty answer removes nothing: NSE's answers are not always complete.
        replace_corp_actions(&mut conn, "AASTHA", &[], today).unwrap();
        assert_eq!(load_corp_actions(&conn).unwrap()["AASTHA"].len(), 1);

        // Newer wording for the same ex-date replaces the old, not piles up.
        replace_corp_actions(&mut conn, "AASTHA", &[action(28, Kind::Bonus, "Bonus 1:1 (revised)", 0.5)], today)
            .unwrap();
        let aastha = &load_corp_actions(&conn).unwrap()["AASTHA"];
        assert_eq!(aastha.len(), 1);
        assert_eq!(aastha[0].purpose, "Bonus 1:1 (revised)");
        assert!(load_corp_actions(&conn).unwrap().contains_key("VEDL"), "other symbols are untouched");
    }

    #[test]
    fn symbols_are_due_again_when_they_were_last_asked_about_long_ago() {
        let mut conn = memory_db();
        let d = |m, day| NaiveDate::from_ymd_opt(2025, m, day).unwrap();
        replace_corp_actions(&mut conn, "OLD", &[], d(8, 1)).unwrap();
        replace_corp_actions(&mut conn, "NEW", &[], d(9, 1)).unwrap();
        let fresh = corp_checked_since(&conn, d(8, 25)).unwrap();
        assert!(fresh.contains("NEW") && !fresh.contains("OLD"));
    }

    #[test]
    fn a_stored_close_can_be_looked_up_by_date() {
        let mut conn = memory_db();
        save_candles(&mut conn, "K", &[candle(1, 100.0), candle(2, 101.0)]).unwrap();
        let d = |day| NaiveDate::from_ymd_opt(2025, 8, day).unwrap();
        assert_eq!(close_on(&conn, "K", d(2)).unwrap(), Some(101.0));
        assert_eq!(close_on(&conn, "K", d(9)).unwrap(), None);
        assert_eq!(close_on(&conn, "NOPE", d(1)).unwrap(), None);
    }

    #[test]
    fn a_symbol_with_no_actions_still_counts_as_checked() {
        let mut conn = memory_db();
        let today = NaiveDate::from_ymd_opt(2025, 9, 1).unwrap();
        assert!(corp_checked_since(&conn, today).unwrap().is_empty());
        replace_corp_actions(&mut conn, "TCS", &[], today).unwrap();
        assert!(corp_checked_since(&conn, today).unwrap().contains("TCS"));
    }

    #[test]
    fn gap_candidates_are_the_listed_nse_stocks_that_jumped_recently() {
        let mut conn = memory_db();
        save_instruments(
            &mut conn,
            &[
                nse_stock("K1", "HALVED", true),
                nse_stock("K2", "STEADY", true),
                nse_stock("K3", "OUTSIDE", false),
                nse_stock("K4", "OLDGAP", true),
                nse_stock("K5", "DOUBLED", true),
            ],
        )
        .unwrap();
        // Sessions 1..=10 of Aug 2025. HALVED falls 50% on the 9th, OLDGAP did on
        // the 3rd, DOUBLED rises 120% on the 10th, OUTSIDE is not in the universe.
        for (key, f) in [
            ("K1", (|d: u32| if d >= 9 { 50.0 } else { 100.0 }) as fn(u32) -> f64),
            ("K2", |_| 100.0),
            ("K3", |d| if d >= 9 { 50.0 } else { 100.0 }),
            ("K4", |d| if d >= 3 { 50.0 } else { 100.0 }),
            ("K5", |d| if d >= 10 { 220.0 } else { 100.0 }),
        ] {
            let bars: Vec<Candle> = (1..=10).map(|d| candle(d, f(d))).collect();
            save_candles(&mut conn, key, &bars).unwrap();
        }

        let since = NaiveDate::from_ymd_opt(2025, 8, 8).unwrap();
        let found = gap_candidates(&conn, since, 0.9, 1.5).unwrap();
        assert_eq!(found, vec!["DOUBLED".to_string(), "HALVED".to_string()]);

        // A wider window picks up the older gap too.
        let since = NaiveDate::from_ymd_opt(2025, 8, 1).unwrap();
        let found = gap_candidates(&conn, since, 0.9, 1.5).unwrap();
        assert_eq!(found, vec!["DOUBLED".to_string(), "HALVED".to_string(), "OLDGAP".to_string()]);
    }

    #[test]
    fn candles_are_counted_per_instrument() {
        let mut conn = memory_db();
        save_candles(&mut conn, "NSE_EQ|X", &[candle(1, 100.0), candle(2, 101.0), candle(3, 102.0)]).unwrap();
        save_candles(&mut conn, "NSE_EQ|Y", &[candle(1, 50.0)]).unwrap();
        assert_eq!(candle_count_for(&conn, "NSE_EQ|X").unwrap(), 3);
        assert_eq!(candle_count_for(&conn, "NSE_EQ|Y").unwrap(), 1);
        assert_eq!(candle_count_for(&conn, "NSE_EQ|NOPE").unwrap(), 0);
    }

    #[test]
    fn candles_round_trip_and_upsert() {
        let mut conn = memory_db();
        save_candles(&mut conn, "NSE_EQ|X", &[candle(1, 100.0), candle(2, 101.0)]).unwrap();
        // Re-saving the same session must update, not duplicate.
        save_candles(&mut conn, "NSE_EQ|X", &[candle(2, 999.0)]).unwrap();

        let got = load_candles(&conn, "NSE_EQ|X").unwrap();
        assert_eq!(got.len(), 2);
        assert_eq!(got[1].close, 999.0);
        assert_eq!(
            last_candle_date(&conn, "NSE_EQ|X").unwrap(),
            Some(NaiveDate::from_ymd_opt(2025, 8, 2).unwrap())
        );
    }

    #[test]
    fn bulk_load_groups_by_instrument_and_keeps_order() {
        let mut conn = memory_db();
        save_candles(&mut conn, "NSE_EQ|A", &[candle(3, 30.0), candle(1, 10.0), candle(2, 20.0)]).unwrap();
        save_candles(&mut conn, "NSE_EQ|B", &[candle(1, 99.0)]).unwrap();

        let all = load_all_candles(&conn).unwrap();
        assert_eq!(all.len(), 2);

        let a = &all["NSE_EQ|A"];
        assert_eq!(a.len(), 3);
        assert!(a.windows(2).all(|w| w[0].date < w[1].date), "series must be ascending");
        assert_eq!(a[0].close, 10.0);
        assert_eq!(all["NSE_EQ|B"].len(), 1);
    }

    #[test]
    fn bulk_load_matches_per_instrument_load() {
        let mut conn = memory_db();
        save_candles(&mut conn, "NSE_EQ|A", &[candle(1, 10.0), candle(2, 20.0)]).unwrap();
        let bulk = load_all_candles(&conn).unwrap();
        assert_eq!(bulk["NSE_EQ|A"], load_candles(&conn, "NSE_EQ|A").unwrap());
    }

    #[test]
    fn bulk_load_on_empty_db_is_empty() {
        assert!(load_all_candles(&memory_db()).unwrap().is_empty());
    }

    #[test]
    fn last_date_is_none_for_unknown_instrument() {
        let conn = memory_db();
        assert_eq!(last_candle_date(&conn, "NSE_EQ|NOPE").unwrap(), None);
    }

    #[test]
    fn detections_round_trip_through_dates() {
        let mut conn = memory_db();
        let candles = vec![candle(1, 100.0), candle(2, 101.0), candle(3, 102.0)];
        save_instruments(
            &mut conn,
            &[Instrument {
                instrument_key: "NSE_EQ|X".into(),
                symbol: "XCO".into(),
                name: "X CO".into(),
                isin: "INE000A01011".into(),
                exchange: Exchange::Nse,
                mcap_cr: Some(500.0),
                included: true,
            }],
        )
        .unwrap();

        let det = Detection::new(PatternKind::ChartinkCupBreakout, 0, 2, 0.8).with_detail("depth 20%");
        replace_detections(&mut conn, "NSE_EQ|X", &candles, &[det]).unwrap();

        let found = detections_since(&conn, NaiveDate::from_ymd_opt(2025, 8, 1).unwrap()).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].kind, PatternKind::ChartinkCupBreakout);
        assert_eq!(found[0].symbol, "XCO");
        assert_eq!(found[0].date, NaiveDate::from_ymd_opt(2025, 8, 3).unwrap());
        assert_eq!(found[0].start_date, NaiveDate::from_ymd_opt(2025, 8, 1).unwrap());
    }

    #[test]
    fn rescan_replaces_previous_detections() {
        let mut conn = memory_db();
        let candles = vec![candle(1, 100.0), candle(2, 101.0)];
        save_instruments(
            &mut conn,
            &[Instrument {
                instrument_key: "NSE_EQ|X".into(),
                symbol: "XCO".into(),
                name: String::new(),
                isin: String::new(),
                exchange: Exchange::Nse,
                mcap_cr: None,
                included: true,
            }],
        )
        .unwrap();

        // A rescan must replace, not accumulate: same instrument, same kind,
        // different bar — only the newer detection should survive.
        replace_detections(
            &mut conn,
            "NSE_EQ|X",
            &candles,
            &[Detection::new(PatternKind::ChartinkCupBreakout, 0, 0, 0.6)],
        )
        .unwrap();
        replace_detections(
            &mut conn,
            "NSE_EQ|X",
            &candles,
            &[Detection::new(PatternKind::ChartinkCupBreakout, 1, 1, 0.7)],
        )
        .unwrap();

        let found = detections_since(&conn, NaiveDate::from_ymd_opt(2025, 1, 1).unwrap()).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].score, 0.7);
        assert_eq!(found[0].date, NaiveDate::from_ymd_opt(2025, 8, 2).unwrap());
    }
}
