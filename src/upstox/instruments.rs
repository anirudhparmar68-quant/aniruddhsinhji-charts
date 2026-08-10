//! Upstox instrument master → the tradable NSE + BSE equity list.
//!
//! Upstox publishes one gzipped JSON per exchange containing every segment
//! (EQ / FO / INDEX / COM). We keep only cash-segment equities, drop ETFs and
//! other non-company instruments, and collapse dual-listed names by ISIN.

use crate::config::{data_dir, is_modified_today};
use crate::model::{Exchange, Instrument};
use anyhow::{Context, Result};
use serde::Deserialize;
use std::collections::HashMap;
use std::io::Read;

const NSE_URL: &str = "https://assets.upstox.com/market-quote/instruments/exchange/NSE.json.gz";
const BSE_URL: &str = "https://assets.upstox.com/market-quote/instruments/exchange/BSE.json.gz";

/// Substrings that mark an instrument as "not an operating company".
/// Matched against trading symbol and name, upper-cased.
const NON_EQUITY_MARKERS: &[&str] = &[
    "ETF", "BEES", "IETF", "GOLDCASE", "SILVERCASE", "MUTUAL FUND", "MUTUALFUND",
    "INDEX FUND", "LIQUIDCASE", "GILT", "SGB", "SOV GOLD", "SOVEREIGN GOLD",
    "TBILL", "T-BILL", "GOI LOAN", "STATE DEV", " SDL ", "NCD", "BOND",
    "DEBENTURE", "INVIT", "REIT", "PARTLY PAID", "RIGHTS ENT",
];

/// Trading-symbol suffixes for rights entitlements / partly paid / when-issued
/// scrips. These are separate lines of the same company, not the company itself.
const NON_EQUITY_SUFFIXES: &[&str] = &["-RE", "-PP", "-WI", "-BL", "-E1", "-E2", "-RT"];

/// Default BSE groups treated as mainboard equity.
///
/// BSE puts its *group code* where NSE puts an instrument type, so a plain
/// `instrument_type == "EQ"` test silently drops every BSE row. Measured against
/// a live dump: of 4,835 BSE equity-series scrips, group A (697) is 99.9%
/// dual-listed on NSE and group B (1,372) is 94% dual-listed, whereas T, X, XT,
/// Z, M, MT, MS, P, TS and ZP — 2,766 scrips — are *entirely* BSE-only. That
/// tail is the surveillance, illiquid and SME tiers, which is exactly what an
/// overnight-positional tool should not be charting.
pub const DEFAULT_BSE_GROUPS: &[&str] = &["A", "B"];

#[derive(Debug, Deserialize)]
struct RawInstrument {
    #[serde(default)]
    segment: String,
    #[serde(default)]
    instrument_key: String,
    #[serde(default)]
    trading_symbol: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    short_name: String,
    #[serde(default)]
    instrument_type: String,
    #[serde(default)]
    security_type: String,
    #[serde(default)]
    isin: String,
}

async fn download_exchange(client: &reqwest::Client, exchange: Exchange) -> Result<Vec<u8>> {
    let (url, file) = match exchange {
        Exchange::Nse => (NSE_URL, "NSE.json.gz"),
        Exchange::Bse => (BSE_URL, "BSE.json.gz"),
    };
    let path = data_dir().join(file);

    if path.is_file() && is_modified_today(&path) {
        return std::fs::read(&path).with_context(|| format!("reading cached {}", path.display()));
    }

    let bytes = client
        .get(url)
        .send()
        .await
        .with_context(|| format!("downloading {url}"))?
        .error_for_status()
        .with_context(|| format!("bad status from {url}"))?
        .bytes()
        .await
        .with_context(|| format!("reading body of {url}"))?;

    std::fs::write(&path, &bytes).with_context(|| format!("caching {}", path.display()))?;
    Ok(bytes.to_vec())
}

fn gunzip(bytes: &[u8]) -> Result<String> {
    // Upstox serves a real .gz payload, so this is file-level gzip rather than
    // HTTP Content-Encoding — reqwest's transparent gzip does not apply here.
    let mut decoder = flate2::read::GzDecoder::new(bytes);
    let mut out = String::new();
    decoder
        .read_to_string(&mut out)
        .context("instrument dump was not valid gzip")?;
    Ok(out)
}

/// Indian ISINs encode the security type in characters 8–9: `01` marks an
/// equity share while `07`/`08`/`09` are debt series, and an `INF` issuer
/// prefix is a mutual fund. This is what separates a company's shares from its
/// own bonds — BSE lists both in the same cash segment, and its debt rows
/// (6,522 in group F, 1,125 in G) outnumber its equities.
fn is_equity_series_isin(isin: &str) -> bool {
    let b = isin.as_bytes();
    b.len() == 12 && b[0] == b'I' && b[1] == b'N' && b[2] != b'F' && b[7] == b'0' && b[8] == b'1'
}

/// Is this an ordinary listed company share on the given exchange?
fn looks_like_company_share(raw: &RawInstrument, exchange: Exchange, bse_groups: &[String]) -> bool {
    let kind = raw.instrument_type.trim().to_ascii_uppercase();
    let accepted = match exchange {
        Exchange::Nse => kind == "EQ",
        Exchange::Bse => bse_groups.iter().any(|g| g.eq_ignore_ascii_case(&kind)),
    };
    if !accepted {
        return false;
    }
    if !is_equity_series_isin(&isin_of(raw)) {
        return false;
    }
    let symbol = raw.trading_symbol.to_ascii_uppercase();
    let haystack = format!(
        "{} {} {} {}",
        symbol,
        raw.name.to_ascii_uppercase(),
        raw.short_name.to_ascii_uppercase(),
        raw.security_type.to_ascii_uppercase()
    );
    if NON_EQUITY_MARKERS.iter().any(|m| haystack.contains(m)) {
        return false;
    }
    if NON_EQUITY_SUFFIXES.iter().any(|s| symbol.ends_with(s)) {
        return false;
    }
    true
}

/// Equity instrument keys are `NSE_EQ|<ISIN>`, so the key is a reliable
/// fallback when the dump omits the field.
fn isin_of(raw: &RawInstrument) -> String {
    if !raw.isin.trim().is_empty() {
        return raw.isin.trim().to_ascii_uppercase();
    }
    raw.instrument_key
        .split('|')
        .nth(1)
        .unwrap_or_default()
        .trim()
        .to_ascii_uppercase()
}

fn parse_exchange(json: &str, exchange: Exchange, bse_groups: &[String]) -> Result<Vec<Instrument>> {
    let wanted_segment = match exchange {
        Exchange::Nse => "NSE_EQ",
        Exchange::Bse => "BSE_EQ",
    };
    let raws: Vec<RawInstrument> =
        serde_json::from_str(json).context("instrument dump was not the expected JSON array")?;

    Ok(raws
        .into_iter()
        .filter(|r| r.segment == wanted_segment)
        .filter(|r| looks_like_company_share(r, exchange, bse_groups))
        .map(|r| {
            let isin = isin_of(&r);
            let name = if r.name.trim().is_empty() {
                r.short_name.clone()
            } else {
                r.name.clone()
            };
            Instrument {
                instrument_key: r.instrument_key,
                symbol: r.trading_symbol.to_ascii_uppercase(),
                name: name.trim().to_string(),
                isin,
                exchange,
                mcap_cr: None,
                included: false,
            }
        })
        .filter(|i| !i.instrument_key.is_empty() && !i.symbol.is_empty())
        .collect())
}

/// Collapse dual-listed companies to one row per ISIN.
///
/// When a company trades on both exchanges we keep a single line — NSE by
/// default, since its daily data is deeper and its symbol is the one traders
/// recognise. BSE-only companies are retained as-is.
fn dedupe_by_isin(all: Vec<Instrument>, prefer_nse: bool) -> Vec<Instrument> {
    let preferred = if prefer_nse { Exchange::Nse } else { Exchange::Bse };
    let mut by_isin: HashMap<String, Instrument> = HashMap::new();

    for inst in all {
        match by_isin.get(&inst.isin) {
            Some(existing) if existing.exchange == preferred => {}
            Some(_) if inst.exchange != preferred => {}
            _ => {
                by_isin.insert(inst.isin.clone(), inst);
            }
        }
    }

    let mut out: Vec<Instrument> = by_isin.into_values().collect();
    sort_alphabetically(&mut out);
    out
}

/// The user browses this list by name, so alphabetical order by symbol is the
/// canonical ordering everywhere in the app.
pub fn sort_alphabetically(list: &mut [Instrument]) {
    list.sort_by(|a, b| {
        a.symbol
            .cmp(&b.symbol)
            .then_with(|| a.exchange.as_str().cmp(b.exchange.as_str()))
    });
}

/// Download (or reuse today's cache of) both exchanges and return the merged,
/// de-duplicated, alphabetically sorted equity universe.
pub async fn load_equities(
    client: &reqwest::Client,
    prefer_nse: bool,
    bse_groups: &[String],
) -> Result<Vec<Instrument>> {
    let (nse_bytes, bse_bytes) = tokio::try_join!(
        download_exchange(client, Exchange::Nse),
        download_exchange(client, Exchange::Bse),
    )?;

    let mut all = parse_exchange(&gunzip(&nse_bytes)?, Exchange::Nse, bse_groups)?;
    all.extend(parse_exchange(&gunzip(&bse_bytes)?, Exchange::Bse, bse_groups)?);
    Ok(dedupe_by_isin(all, prefer_nse))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(symbol: &str, name: &str, itype: &str, isin: &str) -> RawInstrument {
        RawInstrument {
            segment: "NSE_EQ".into(),
            instrument_key: format!("NSE_EQ|{isin}"),
            trading_symbol: symbol.into(),
            name: name.into(),
            short_name: String::new(),
            instrument_type: itype.into(),
            security_type: "NORMAL".into(),
            isin: isin.into(),
        }
    }

    fn groups() -> Vec<String> {
        DEFAULT_BSE_GROUPS.iter().map(|s| s.to_string()).collect()
    }

    fn nse_ok(r: &RawInstrument) -> bool {
        looks_like_company_share(r, Exchange::Nse, &groups())
    }

    fn bse_ok(r: &RawInstrument) -> bool {
        looks_like_company_share(r, Exchange::Bse, &groups())
    }

    #[test]
    fn keeps_ordinary_shares() {
        assert!(nse_ok(&raw("RELIANCE", "RELIANCE INDUSTRIES LTD", "EQ", "INE002A01018")));
    }

    #[test]
    fn drops_etfs_and_rights_entitlements() {
        assert!(!nse_ok(&raw("NIFTYBEES", "NIPPON INDIA ETF NIFTY 50", "EQ", "INF204KB14I2")));
        assert!(!nse_ok(&raw("SOMECO-RE", "SOME COMPANY RIGHTS", "EQ", "INE123A01011")));
        assert!(!nse_ok(&raw("SOMEFUT", "SOME COMPANY", "FUT", "INE123A01011")));
    }

    #[test]
    fn bse_uses_group_codes_not_an_instrument_type() {
        // BSE never says "EQ" — it reports the group, so the NSE rule would
        // silently drop every BSE row.
        assert!(!bse_ok(&raw("SOMECO", "SOME CO LTD", "EQ", "INE123A01011")));
        assert!(bse_ok(&raw("RELIANCE", "RELIANCE INDUSTRIES LTD", "A", "INE002A01018")));
        assert!(bse_ok(&raw("JYOTIRES", "JYOTI RESINS & ADHESIVES LTD.", "B", "INE197L01017")));
    }

    #[test]
    fn bse_surveillance_and_sme_tiers_are_excluded() {
        for group in ["X", "XT", "T", "Z", "M", "MT", "MS", "P", "TS", "ZP"] {
            assert!(
                !bse_ok(&raw("RAJKSYN", "RAJKAMAL SYNTHETICS LTD.", group, "INE376L01013")),
                "group {group} should not be in the universe"
            );
        }
    }

    #[test]
    fn bse_debt_lines_are_excluded_by_isin() {
        // BSE lists a company's bonds in the same cash segment as its shares.
        // The ISIN security-type digits are what tell them apart.
        assert!(!bse_ok(&raw("775IRFC33", "IRFC-7.75%-15-4-33-PVT", "F", "INE053F08270")));
        // Even mislabelled into an equity group, the ISIN still rejects it.
        assert!(!bse_ok(&raw("775IRFC33", "IRFC-7.75%-15-4-33-PVT", "B", "INE053F08270")));
    }

    #[test]
    fn equity_series_isin_rule() {
        assert!(is_equity_series_isin("INE002A01018")); // Reliance shares
        assert!(!is_equity_series_isin("INE053F08270")); // a bond
        assert!(!is_equity_series_isin("INF204KB14I2")); // a mutual fund
        assert!(!is_equity_series_isin("INE002A0101")); // too short
        assert!(!is_equity_series_isin(""));
    }

    #[test]
    fn dedupe_prefers_nse_regardless_of_input_order() {
        let mk = |ex: Exchange, sym: &str| Instrument {
            instrument_key: format!("{}_EQ|INE123A01011", ex.as_str()),
            symbol: sym.into(),
            name: "SOME CO".into(),
            isin: "INE123A01011".into(),
            exchange: ex,
            mcap_cr: None,
            included: false,
        };

        for input in [
            vec![mk(Exchange::Bse, "SOMECO"), mk(Exchange::Nse, "SOMECO")],
            vec![mk(Exchange::Nse, "SOMECO"), mk(Exchange::Bse, "SOMECO")],
        ] {
            let out = dedupe_by_isin(input, true);
            assert_eq!(out.len(), 1);
            assert_eq!(out[0].exchange, Exchange::Nse);
        }
    }

    #[test]
    fn bse_only_names_survive_dedupe() {
        let only_bse = vec![Instrument {
            instrument_key: "BSE_EQ|INE999A01011".into(),
            symbol: "BSEONLY".into(),
            name: "BSE ONLY LTD".into(),
            isin: "INE999A01011".into(),
            exchange: Exchange::Bse,
            mcap_cr: None,
            included: false,
        }];
        assert_eq!(dedupe_by_isin(only_bse, true).len(), 1);
    }
}
