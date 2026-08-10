//! Daily bhavcopy from NSE and BSE — the authoritative end-of-day record.
//!
//! Upstox is used for building history, but it cannot be trusted for the *tail*.
//! Measured on RELIANCE within a single minute, the same endpoint answered a
//! 5-day window with Friday's close, a 30-day window with Monday's, and a
//! 400-day window with Friday's again. The newest session is exactly the one
//! this app is about, so it comes from the exchanges instead.
//!
//! One file per exchange per day covers every scrip that traded: no per-symbol
//! requests, no rate limits, no cache to be served a stale copy from. Both
//! exchanges publish the same UDiFF layout, and both carry ISIN — which is what
//! this app keys instruments on, so matching is exact rather than by name.

use crate::model::{Candle, Exchange};
use anyhow::{Context, Result};
use chrono::NaiveDate;
use std::collections::HashMap;
use std::io::Read;

/// A day's closing record, keyed by Upstox-style instrument key
/// (`NSE_EQ|<ISIN>` / `BSE_EQ|<ISIN>`) so it drops straight into the store.
pub type DayRecord = HashMap<String, Candle>;

fn nse_url(date: NaiveDate) -> String {
    format!(
        "https://nsearchives.nseindia.com/content/cm/BhavCopy_NSE_CM_0_0_0_{}_F_0000.csv.zip",
        date.format("%Y%m%d")
    )
}

fn bse_url(date: NaiveDate) -> String {
    format!(
        "https://www.bseindia.com/download/BhavCopy/Equity/BhavCopy_BSE_CM_0_0_0_{}_F_0000.CSV",
        date.format("%Y%m%d")
    )
}

/// Both exchanges reject requests that do not look like a browser.
fn request(client: &reqwest::Client, url: &str, referer: &str) -> reqwest::RequestBuilder {
    client
        .get(url)
        .header("User-Agent", "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36")
        .header("Referer", referer)
        .header("Accept", "*/*")
        .header("Accept-Language", "en-US,en;q=0.9")
}

/// Pull one CSV out of a zip archive.
fn first_csv_in_zip(bytes: &[u8]) -> Result<String> {
    let mut archive = zip::ZipArchive::new(std::io::Cursor::new(bytes))
        .context("bhavcopy archive was not a readable zip")?;
    for i in 0..archive.len() {
        let mut entry = archive.by_index(i)?;
        if entry.name().to_ascii_lowercase().ends_with(".csv") {
            let mut out = String::new();
            entry.read_to_string(&mut out).context("bhavcopy csv was not valid text")?;
            return Ok(out);
        }
    }
    anyhow::bail!("no csv inside the bhavcopy archive")
}

/// Parse a UDiFF bhavcopy into candles, keeping only rows that look like an
/// ordinary traded equity for `exchange`.
fn parse(csv_text: &str, exchange: Exchange, date: NaiveDate) -> Result<DayRecord> {
    let mut reader = csv::ReaderBuilder::new().flexible(true).from_reader(csv_text.as_bytes());
    let headers = reader.headers()?.clone();
    let col = |name: &str| headers.iter().position(|h| h.trim().eq_ignore_ascii_case(name));

    let (Some(isin_c), Some(o_c), Some(h_c), Some(l_c), Some(c_c), Some(v_c)) = (
        col("ISIN"),
        col("OpnPric"),
        col("HghPric"),
        col("LwPric"),
        col("ClsPric"),
        col("TtlTradgVol"),
    ) else {
        anyhow::bail!("bhavcopy is missing the expected UDiFF columns");
    };
    let type_c = col("FinInstrmTp");
    let date_c = col("TradDt");

    let segment = match exchange {
        Exchange::Nse => "NSE_EQ",
        Exchange::Bse => "BSE_EQ",
    };

    let mut out = DayRecord::new();
    for record in reader.records() {
        let record = record?;

        // Derivatives share the file; only cash-segment stock rows are wanted.
        if let Some(i) = type_c {
            if !matches!(record.get(i).map(|s| s.trim()), Some("STK") | None) {
                continue;
            }
        }
        // A stale or mislabelled file must not be written under today's date.
        if let Some(i) = date_c {
            if let Some(traded) = record.get(i) {
                if traded.trim() != date.to_string() {
                    continue;
                }
            }
        }

        let Some(isin) = record.get(isin_c).map(|s| s.trim().to_uppercase()) else { continue };
        if isin.len() != 12 {
            continue;
        }

        let num = |i: usize| record.get(i).and_then(|s| s.trim().parse::<f64>().ok());
        let (Some(open), Some(high), Some(low), Some(close)) =
            (num(o_c), num(h_c), num(l_c), num(c_c))
        else {
            continue;
        };
        if ![open, high, low, close].iter().all(|v| v.is_finite() && *v > 0.0) {
            continue;
        }
        let volume = record.get(v_c).and_then(|s| s.trim().parse::<f64>().ok()).unwrap_or(0.0) as i64;

        out.insert(
            format!("{segment}|{isin}"),
            Candle { date, open, high, low, close, volume },
        );
    }
    Ok(out)
}

/// Fetch one exchange's bhavcopy for `date`.
///
/// `Ok(None)` means the exchange has no file for that date — a holiday, a
/// weekend, or a session whose file has not been published yet. That is normal
/// and must not be treated as a failure.
pub async fn fetch_exchange(
    client: &reqwest::Client,
    exchange: Exchange,
    date: NaiveDate,
) -> Result<Option<DayRecord>> {
    let (url, referer) = match exchange {
        Exchange::Nse => (nse_url(date), "https://www.nseindia.com/"),
        Exchange::Bse => (bse_url(date), "https://www.bseindia.com/"),
    };

    let response = request(client, &url, referer)
        .send()
        .await
        .with_context(|| format!("requesting {url}"))?;

    if response.status() == reqwest::StatusCode::NOT_FOUND
        || response.status() == reqwest::StatusCode::FORBIDDEN
    {
        return Ok(None);
    }
    let response = response.error_for_status().with_context(|| format!("bad status from {url}"))?;
    let bytes = response.bytes().await?;

    // NSE ships a zip, BSE a bare CSV.
    let text = if url.to_ascii_lowercase().ends_with(".zip") {
        first_csv_in_zip(&bytes)?
    } else {
        String::from_utf8_lossy(&bytes).into_owned()
    };

    let parsed = parse(&text, exchange, date)?;
    Ok(if parsed.is_empty() { None } else { Some(parsed) })
}

/// Both exchanges for one session, merged. NSE wins where a stock is on both,
/// matching the universe's own preference.
pub async fn fetch_day(client: &reqwest::Client, date: NaiveDate) -> Result<DayRecord> {
    let (nse, bse) = tokio::join!(
        fetch_exchange(client, Exchange::Nse, date),
        fetch_exchange(client, Exchange::Bse, date),
    );

    let mut merged = DayRecord::new();
    // Insert BSE first so NSE overwrites on the (rare) key collision.
    if let Ok(Some(rows)) = bse {
        merged.extend(rows);
    }
    if let Ok(Some(rows)) = nse {
        merged.extend(rows);
    }
    Ok(merged)
}

#[cfg(test)]
mod tests {
    use super::*;

    const HEADER: &str = "TradDt,BizDt,Sgmt,Src,FinInstrmTp,FinInstrmId,ISIN,TckrSymb,SctySrs,\
                          XpryDt,FininstrmActlXpryDt,StrkPric,OptnTp,FinInstrmNm,OpnPric,HghPric,\
                          LwPric,ClsPric,LastPric,PrvsClsgPric,UndrlygPric,SttlmPric,OpnIntrst,\
                          ChngInOpnIntrst,TtlTradgVol,TtlTrfVal,TtlNbOfTxsExctd,SsnId,NewBrdLotQty,Rmks";

    fn row(date: &str, kind: &str, isin: &str, o: &str, h: &str, l: &str, c: &str, vol: &str) -> String {
        format!(
            "{date},{date},CM,NSE,{kind},1234,{isin},SYM,EQ,,,0,,Name,{o},{h},{l},{c},{c},{c},,,0,0,{vol},100,50,S1,1,\n"
        )
    }

    fn day() -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 8, 10).unwrap()
    }

    #[test]
    fn parses_an_equity_row_into_a_candle() {
        let csv = format!(
            "{HEADER}\n{}",
            row("2026-08-10", "STK", "INE002A01018", "1330.10", "1332.90", "1321.30", "1327.30", "8207102")
        );
        let out = parse(&csv, Exchange::Nse, day()).unwrap();
        let candle = &out["NSE_EQ|INE002A01018"];
        assert_eq!(candle.date, day());
        assert_eq!(candle.close, 1327.30);
        assert_eq!(candle.volume, 8_207_102);
    }

    #[test]
    fn keys_are_built_per_exchange() {
        let csv = format!("{HEADER}\n{}", row("2026-08-10", "STK", "INE002A01018", "1", "2", "0.5", "1.5", "10"));
        assert!(parse(&csv, Exchange::Bse, day()).unwrap().contains_key("BSE_EQ|INE002A01018"));
        assert!(parse(&csv, Exchange::Nse, day()).unwrap().contains_key("NSE_EQ|INE002A01018"));
    }

    #[test]
    fn skips_derivatives_and_other_instrument_types() {
        let csv = format!(
            "{HEADER}\n{}{}",
            row("2026-08-10", "FUTSTK", "INE002A01018", "1", "2", "0.5", "1.5", "10"),
            row("2026-08-10", "STK", "INE009A01021", "1", "2", "0.5", "1.5", "10"),
        );
        let out = parse(&csv, Exchange::Nse, day()).unwrap();
        assert_eq!(out.len(), 1);
        assert!(out.contains_key("NSE_EQ|INE009A01021"));
    }

    #[test]
    fn refuses_rows_from_a_different_session() {
        // Guards against a mislabelled or cached file being written under the
        // date we asked for — the exact failure this module exists to avoid.
        let csv = format!(
            "{HEADER}\n{}{}",
            row("2026-08-07", "STK", "INE002A01018", "1", "2", "0.5", "1.5", "10"),
            row("2026-08-10", "STK", "INE009A01021", "1", "2", "0.5", "1.5", "10"),
        );
        let out = parse(&csv, Exchange::Nse, day()).unwrap();
        assert_eq!(out.len(), 1, "only the requested session may be kept");
        assert!(out.contains_key("NSE_EQ|INE009A01021"));
    }

    #[test]
    fn drops_zero_and_malformed_prices() {
        let csv = format!(
            "{HEADER}\n{}{}",
            row("2026-08-10", "STK", "INE002A01018", "0", "0", "0", "0", "10"),
            row("2026-08-10", "STK", "INE009A01021", "1", "2", "0.5", "abc", "10"),
        );
        assert!(parse(&csv, Exchange::Nse, day()).unwrap().is_empty());
    }

    #[test]
    fn rejects_a_file_without_the_expected_columns() {
        assert!(parse("foo,bar\n1,2\n", Exchange::Nse, day()).is_err());
    }
}
