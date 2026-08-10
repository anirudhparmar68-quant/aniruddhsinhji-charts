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
                kind: PatternKind::Doji, // replaced below; placeholder keeps the row shape
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

        replace_detections(
            &mut conn,
            "NSE_EQ|X",
            &candles,
            &[Detection::new(PatternKind::Doji, 0, 0, 0.6)],
        )
        .unwrap();
        replace_detections(
            &mut conn,
            "NSE_EQ|X",
            &candles,
            &[Detection::new(PatternKind::Hammer, 1, 1, 0.7)],
        )
        .unwrap();

        let found = detections_since(&conn, NaiveDate::from_ymd_opt(2025, 1, 1).unwrap()).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].kind, PatternKind::Hammer);
    }
}
