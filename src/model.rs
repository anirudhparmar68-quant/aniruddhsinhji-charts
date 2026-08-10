//! Core domain types shared by the loader, the pattern engine and the UI.

use chrono::NaiveDate;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Exchange {
    Nse,
    Bse,
}

impl Exchange {
    pub fn as_str(self) -> &'static str {
        match self {
            Exchange::Nse => "NSE",
            Exchange::Bse => "BSE",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_uppercase().as_str() {
            "NSE" => Some(Exchange::Nse),
            "BSE" => Some(Exchange::Bse),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Instrument {
    /// Upstox key, e.g. `NSE_EQ|INE848E01016`. Primary key everywhere.
    pub instrument_key: String,
    pub symbol: String,
    pub name: String,
    pub isin: String,
    pub exchange: Exchange,
    /// Market cap in rupees crore. `None` when we could not resolve it.
    pub mcap_cr: Option<f64>,
    /// Whether this instrument passed the universe filter.
    pub included: bool,
}

/// One daily OHLCV bar. Dates are trading sessions, so gaps are expected and
/// every index in this crate is a *bar index*, never a calendar offset.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Candle {
    pub date: NaiveDate,
    pub open: f64,
    pub high: f64,
    pub low: f64,
    pub close: f64,
    pub volume: i64,
}

impl Candle {
    #[inline]
    pub fn body(&self) -> f64 {
        (self.close - self.open).abs()
    }

    /// Signed body: positive on an up candle.
    #[inline]
    pub fn signed_body(&self) -> f64 {
        self.close - self.open
    }

    #[inline]
    pub fn range(&self) -> f64 {
        self.high - self.low
    }

    #[inline]
    pub fn upper_shadow(&self) -> f64 {
        self.high - self.open.max(self.close)
    }

    #[inline]
    pub fn lower_shadow(&self) -> f64 {
        self.open.min(self.close) - self.low
    }

    #[inline]
    pub fn body_top(&self) -> f64 {
        self.open.max(self.close)
    }

    #[inline]
    pub fn body_bottom(&self) -> f64 {
        self.open.min(self.close)
    }

    #[inline]
    pub fn is_bull(&self) -> bool {
        self.close > self.open
    }

    #[inline]
    pub fn is_bear(&self) -> bool {
        self.close < self.open
    }

    #[inline]
    pub fn mid(&self) -> f64 {
        (self.high + self.low) / 2.0
    }

    /// Body midpoint — used by piercing / dark-cloud style rules.
    #[inline]
    pub fn body_mid(&self) -> f64 {
        (self.open + self.close) / 2.0
    }

    /// Traded value in rupees crore, the liquidity measure used by the universe filter.
    #[inline]
    pub fn turnover_cr(&self) -> f64 {
        self.close * self.volume as f64 / 1e7
    }

    /// True when the candle has effectively no body relative to its own range.
    /// `frac` is the share of the range the body must stay under.
    #[inline]
    pub fn is_doji_like(&self, frac: f64) -> bool {
        let r = self.range();
        r > 0.0 && self.body() <= r * frac
    }
}
