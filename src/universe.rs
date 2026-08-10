//! Universe construction: which stocks the app tracks at all.
//!
//! Neither Upstox nor the exchanges publish market cap in a machine-readable
//! feed keyed by symbol, so this module resolves it from two sources:
//!
//!   1. AMFI's half-yearly "average market capitalisation of listed companies"
//!      workbook, matched to instruments by normalised company name.
//!   2. `data/mcap_overrides.csv` — a plain `isin,mcap_cr` file the user can
//!      edit, which always wins over AMFI.
//!
//! Anything still unresolved is *not* silently dropped. It is kept as
//! `mcap_cr = None` and judged on traded value instead, and the UI reports how
//! many names took that path so the gap is visible rather than hidden.

use crate::config::{data_dir, Settings};
use crate::model::{Candle, Instrument};
use anyhow::{Context, Result};
use std::collections::HashMap;

const OVERRIDES_FILE: &str = "mcap_overrides.csv";

// ---------------------------------------------------------------------------
// Name normalisation
// ---------------------------------------------------------------------------

/// Legal-form words that differ between AMFI and exchange spellings.
const NOISE_WORDS: &[&str] = &[
    "LIMITED", "LTD", "THE", "PVT", "PRIVATE", "INCORPORATED", "INC", "PLC",
];

/// Reduce a company name to a comparable key.
///
/// "Reliance Industries Ltd." and "RELIANCE INDUSTRIES LIMITED" must collapse to
/// the same string, without being so aggressive that distinct companies merge.
pub fn normalise_name(name: &str) -> String {
    let cleaned: String = name
        .to_uppercase()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { ' ' })
        .collect();
    cleaned
        .split_whitespace()
        .filter(|w| !NOISE_WORDS.contains(w))
        .collect::<Vec<_>>()
        .join(" ")
}

// ---------------------------------------------------------------------------
// Market-cap table
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Clone)]
pub struct MarketCaps {
    by_name: HashMap<String, f64>,
    by_isin: HashMap<String, f64>,
    by_symbol: HashMap<String, f64>,
    pub source_note: String,
}

impl MarketCaps {
    /// Read `data/mcap_overrides.csv` if present.
    ///
    /// Needs a market-cap column plus **either** `isin` or `symbol`, so a
    /// screener export (which gives symbols, not ISINs) can be dropped in
    /// unchanged. `#` lines are comments. A missing file is not an error.
    pub fn load_overrides(&mut self) -> Result<usize> {
        let path = data_dir().join(OVERRIDES_FILE);
        if !path.is_file() {
            return Ok(0);
        }
        let raw = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        self.parse_overrides(&raw)
            .with_context(|| format!("parsing {}", path.display()))
    }

    /// Split out from file loading so the column sniffing is unit-testable.
    pub fn parse_overrides(&mut self, data: &str) -> Result<usize> {
        let mut reader = csv::ReaderBuilder::new()
            .flexible(true)
            .comment(Some(b'#'))
            .from_reader(data.as_bytes());

        let headers = reader.headers()?.clone();
        let col = |want: &str| headers.iter().position(|h| h.trim().eq_ignore_ascii_case(want));
        let isin_col = col("isin");
        let symbol_col = col("symbol").or_else(|| col("nsecode")).or_else(|| col("sm"));
        let mcap_col = col("mcap_cr")
            .or_else(|| col("mcap"))
            .or_else(|| col("market_cap_cr"))
            .or_else(|| col("market cap"))
            .or_else(|| col("market capitalization"));

        let Some(mcap_col) = mcap_col else {
            anyhow::bail!("needs a market-cap column (mcap_cr / mcap / market cap)");
        };
        if isin_col.is_none() && symbol_col.is_none() {
            anyhow::bail!("needs an `isin` or `symbol` column");
        }

        let mut count = 0;
        for record in reader.records() {
            let record = record?;
            let Some(raw) = record.get(mcap_col) else { continue };
            let Ok(mcap) = raw.trim().replace(',', "").parse::<f64>() else { continue };
            if mcap <= 0.0 {
                continue;
            }

            let mut stored = false;
            if let Some(isin) = isin_col.and_then(|c| record.get(c)) {
                let isin = isin.trim().to_uppercase();
                if !isin.is_empty() {
                    self.by_isin.insert(isin, mcap);
                    stored = true;
                }
            }
            if let Some(symbol) = symbol_col.and_then(|c| record.get(c)) {
                let symbol = symbol.trim().to_uppercase();
                if !symbol.is_empty() {
                    self.by_symbol.insert(symbol, mcap);
                    stored = true;
                }
            }
            if stored {
                count += 1;
            }
        }
        Ok(count)
    }

    /// Download and parse a bulk market-cap workbook from `page_url`.
    ///
    /// There is no dependable free bulk source keyed by symbol: AMFI's
    /// categorisation page has moved and returns 404, and BSE's per-scrip API
    /// carries EPS, P/E and face value but not market cap. So this is opt-in —
    /// leave `mcap_source_url` empty (the default) and the overrides CSV is the
    /// single source of truth. If a workable page turns up, point this at it:
    /// the page is scraped for the first `.xls`/`.xlsx` link that looks like a
    /// market-cap workbook, so a versioned filename keeps working.
    pub async fn load_from_page(
        &mut self,
        client: &reqwest::Client,
        page_url: &str,
    ) -> Result<usize> {
        let page = client
            .get(page_url)
            .send()
            .await
            .with_context(|| format!("fetching {page_url}"))?
            .error_for_status()?
            .text()
            .await?;

        let re = regex::Regex::new(r#"(?i)href\s*=\s*["']([^"']*\.xlsx?)["']"#)?;
        let link = re
            .captures_iter(&page)
            .map(|c| c[1].to_string())
            .find(|href| {
                let h = href.to_uppercase();
                h.contains("MKTCAP") || h.contains("MARKETCAP") || h.contains("MARKET-CAP") || h.contains("AVGMKT")
            })
            .ok_or_else(|| anyhow::anyhow!("no market-cap workbook link found on {page_url}"))?;

        let url = if link.starts_with("http") {
            link
        } else {
            // Resolve against the page's own origin.
            let origin = page_url
                .split_once("://")
                .and_then(|(scheme, rest)| rest.split('/').next().map(|host| format!("{scheme}://{host}")))
                .unwrap_or_else(|| page_url.to_string());
            format!("{origin}/{}", link.trim_start_matches('/'))
        };

        let bytes = client
            .get(&url)
            .send()
            .await
            .with_context(|| format!("downloading {url}"))?
            .error_for_status()?
            .bytes()
            .await?;

        let cached = data_dir().join("mcap_workbook.xlsx");
        std::fs::write(&cached, &bytes).ok();

        let added = self.parse_workbook(&bytes)?;
        self.source_note = format!("workbook: {added} companies");
        Ok(added)
    }

    /// Parse the workbook loosely: find the company-name column and the numeric
    /// market-cap column per row rather than assuming a fixed layout, because
    /// AMFI reshuffles headers between releases.
    fn parse_workbook(&mut self, bytes: &[u8]) -> Result<usize> {
        use calamine::{Data, Reader};

        let cursor = std::io::Cursor::new(bytes.to_vec());
        let mut workbook =
            calamine::open_workbook_auto_from_rs(cursor).context("AMFI file was not a readable workbook")?;
        let Some(sheet_name) = workbook.sheet_names().first().cloned() else {
            anyhow::bail!("AMFI workbook had no sheets");
        };
        let range = workbook
            .worksheet_range(&sheet_name)
            .with_context(|| format!("reading sheet {sheet_name}"))?;

        let mut added = 0;
        for row in range.rows() {
            // A data row is any row with one text cell that looks like a company
            // name and one positive number that looks like a rupee-crore figure.
            let name = row.iter().find_map(|cell| match cell {
                Data::String(s) if s.trim().len() > 3 && s.chars().any(|c| c.is_alphabetic()) => {
                    Some(s.trim().to_string())
                }
                _ => None,
            });
            let mcap = row.iter().filter_map(|cell| match cell {
                Data::Float(f) if *f > 0.0 => Some(*f),
                Data::Int(i) if *i > 0 => Some(*i as f64),
                Data::String(s) => s.trim().replace(',', "").parse::<f64>().ok().filter(|v| *v > 0.0),
                _ => None,
            })
            // Serial numbers are small; market caps in crore are not. Take the
            // largest numeric cell to skip the "Sr. No." column.
            .fold(0.0_f64, f64::max);

            if let Some(name) = name {
                if mcap > 1.0 {
                    self.by_name.insert(normalise_name(&name), mcap);
                    added += 1;
                }
            }
        }
        if added == 0 {
            anyhow::bail!("AMFI workbook parsed but contained no usable rows");
        }
        Ok(added)
    }

    /// ISIN first (unambiguous), then ticker, then normalised company name.
    pub fn lookup(&self, instrument: &Instrument) -> Option<f64> {
        if let Some(v) = self.by_isin.get(&instrument.isin) {
            return Some(*v);
        }
        if let Some(v) = self.by_symbol.get(&instrument.symbol) {
            return Some(*v);
        }
        self.by_name.get(&normalise_name(&instrument.name)).copied()
    }

    pub fn is_empty(&self) -> bool {
        self.by_isin.is_empty() && self.by_symbol.is_empty() && self.by_name.is_empty()
    }
}

// ---------------------------------------------------------------------------
// Filtering
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Clone, Copy)]
pub struct UniverseStats {
    pub total: usize,
    pub mcap_resolved: usize,
    pub passed_on_mcap: usize,
    pub passed_on_liquidity: usize,
    pub excluded: usize,
}

/// Stamp resolved market caps onto the instrument list.
pub fn apply_market_caps(instruments: &mut [Instrument], caps: &MarketCaps) -> usize {
    let mut resolved = 0;
    for inst in instruments.iter_mut() {
        inst.mcap_cr = caps.lookup(inst);
        if inst.mcap_cr.is_some() {
            resolved += 1;
        }
    }
    resolved
}

/// Should we bother downloading history for this name?
///
/// Deliberately generous: anything clearing the market-cap floor, plus anything
/// whose market cap we could not resolve (it gets judged on turnover later).
pub fn worth_downloading(inst: &Instrument, settings: &Settings) -> bool {
    match inst.mcap_cr {
        Some(mcap) => mcap >= settings.min_mcap_cr,
        None => settings.include_unknown_mcap,
    }
}

/// Median daily traded value in crore over the most recent `window` sessions.
pub fn median_turnover_cr(candles: &[Candle], window: usize) -> f64 {
    if candles.is_empty() {
        return 0.0;
    }
    let start = candles.len().saturating_sub(window);
    let mut values: Vec<f64> = candles[start..].iter().map(|c| c.turnover_cr()).collect();
    values.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    values[values.len() / 2]
}

/// Final inclusion decision once history is available.
pub fn decide_inclusion(inst: &Instrument, candles: &[Candle], settings: &Settings) -> bool {
    // No history means nothing to chart or scan, whatever the market cap.
    if candles.len() < 30 {
        return false;
    }
    if candles[candles.len() - 1].close < settings.min_price {
        return false;
    }
    match inst.mcap_cr {
        Some(mcap) => mcap >= settings.min_mcap_cr,
        None => {
            settings.include_unknown_mcap
                && median_turnover_cr(candles, 60) >= settings.min_median_turnover_cr
        }
    }
}

/// Write a starter overrides file so the user can see the expected format.
pub fn write_overrides_template() -> Result<()> {
    let path = data_dir().join(OVERRIDES_FILE);
    if path.is_file() {
        return Ok(());
    }
    std::fs::write(
        &path,
        "# Market caps for the universe filter, in rupees crore.\n\
         # This file always wins over any other source.\n\
         #\n\
         # Easiest way to fill it: run a screener for \"Market Cap > 100\", export\n\
         # the result, and paste it below. Only a market-cap column plus either\n\
         # `symbol` or `isin` is required — extra columns are ignored, and lines\n\
         # starting with # are skipped.\n\
         symbol,isin,mcap_cr\n",
    )
    .with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Exchange;
    use chrono::NaiveDate;

    fn inst(name: &str, isin: &str, mcap: Option<f64>) -> Instrument {
        Instrument {
            instrument_key: format!("NSE_EQ|{isin}"),
            symbol: "TEST".into(),
            name: name.into(),
            isin: isin.into(),
            exchange: Exchange::Nse,
            mcap_cr: mcap,
            included: false,
        }
    }

    fn candles(n: usize, close: f64, volume: i64) -> Vec<Candle> {
        (0..n)
            .map(|i| Candle {
                date: NaiveDate::from_ymd_opt(2025, 1, 1).unwrap() + chrono::Duration::days(i as i64),
                open: close,
                high: close,
                low: close,
                close,
                volume,
            })
            .collect()
    }

    #[test]
    fn normalisation_collapses_legal_forms() {
        assert_eq!(normalise_name("Reliance Industries Ltd."), "RELIANCE INDUSTRIES");
        assert_eq!(normalise_name("RELIANCE INDUSTRIES LIMITED"), "RELIANCE INDUSTRIES");
        assert_ne!(normalise_name("Tata Motors"), normalise_name("Tata Steel"));
    }

    #[test]
    fn overrides_skip_comment_lines() {
        // The template ships with a comment block; the csv reader must not
        // mistake the first comment for the header row.
        let mut caps = MarketCaps::default();
        let n = caps
            .parse_overrides(
                "# market caps in rupees crore\n\
                 # exported from a screener\n\
                 symbol,isin,mcap_cr\n\
                 RELIANCE,INE002A01018,1850000\n",
            )
            .unwrap();
        assert_eq!(n, 1);
        assert_eq!(caps.lookup(&inst("RELIANCE INDUSTRIES", "INE002A01018", None)), Some(1850000.0));
    }

    #[test]
    fn overrides_accept_a_symbol_only_export() {
        // A screener export gives tickers, not ISINs.
        let mut caps = MarketCaps::default();
        let n = caps
            .parse_overrides("Sr,Symbol,Market Cap\n1,TATAMOTORS,320000\n2,JYOTIRES,410\n")
            .unwrap();
        assert_eq!(n, 2);

        let mut tata = inst("Tata Motors Ltd", "INE155A01022", None);
        tata.symbol = "TATAMOTORS".into();
        assert_eq!(caps.lookup(&tata), Some(320000.0));
    }

    #[test]
    fn overrides_reject_a_file_with_no_usable_columns() {
        let mut caps = MarketCaps::default();
        assert!(caps.parse_overrides("foo,bar\n1,2\n").is_err());
        assert!(caps.parse_overrides("symbol,industry\nRELIANCE,Energy\n").is_err());
    }

    #[test]
    fn overrides_ignore_unparseable_values() {
        let mut caps = MarketCaps::default();
        let n = caps
            .parse_overrides("symbol,mcap_cr\nAAA,\"1,234\"\nBBB,not-a-number\nCCC,500\n")
            .unwrap();
        assert_eq!(n, 2, "comma-grouped 1,234 and 500 load; the text value is skipped");
        let mut aaa = inst("A Ltd", "INE111A01011", None);
        aaa.symbol = "AAA".into();
        assert_eq!(caps.lookup(&aaa), Some(1234.0));
    }

    #[test]
    fn isin_override_beats_name_match() {
        let mut caps = MarketCaps::default();
        caps.by_name.insert(normalise_name("SOME CO LTD"), 50.0);
        caps.by_isin.insert("INE123A01011".into(), 900.0);
        assert_eq!(caps.lookup(&inst("SOME CO LTD", "INE123A01011", None)), Some(900.0));
    }

    #[test]
    fn median_turnover_is_robust_to_one_spike() {
        // 1 crore turnover on most days, one huge outlier.
        let mut c = candles(60, 100.0, 100_000); // 100 * 100000 / 1e7 = 1.0 cr
        c[30].volume = 100_000_000;
        assert!((median_turnover_cr(&c, 60) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn inclusion_respects_mcap_floor() {
        let s = Settings::default(); // 100 cr floor
        let c = candles(60, 100.0, 100_000);
        assert!(decide_inclusion(&inst("BIG", "I1", Some(500.0)), &c, &s));
        assert!(!decide_inclusion(&inst("SMALL", "I2", Some(50.0)), &c, &s));
    }

    #[test]
    fn unknown_mcap_falls_back_to_turnover() {
        let s = Settings::default(); // needs 0.25 cr median turnover
        let liquid = candles(60, 100.0, 100_000); // 1.0 cr/day
        let illiquid = candles(60, 100.0, 1_000); // 0.01 cr/day
        assert!(decide_inclusion(&inst("UNKNOWN", "I3", None), &liquid, &s));
        assert!(!decide_inclusion(&inst("UNKNOWN", "I4", None), &illiquid, &s));
    }

    #[test]
    fn unknown_mcap_can_be_excluded_outright() {
        let s = Settings { include_unknown_mcap: false, ..Settings::default() };
        let liquid = candles(60, 100.0, 100_000);
        assert!(!decide_inclusion(&inst("UNKNOWN", "I5", None), &liquid, &s));
        assert!(!worth_downloading(&inst("UNKNOWN", "I5", None), &s));
    }

    #[test]
    fn penny_stocks_and_stubs_are_excluded() {
        let s = Settings::default();
        assert!(!decide_inclusion(&inst("PENNY", "I6", Some(500.0)), &candles(60, 2.0, 10_000_000), &s));
        assert!(!decide_inclusion(&inst("NEW", "I7", Some(500.0)), &candles(10, 100.0, 100_000), &s));
    }
}
