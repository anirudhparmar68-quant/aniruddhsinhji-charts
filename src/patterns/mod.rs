//! Pattern recognition: Japanese candlesticks and multi-week chart structures.

pub mod candlesticks;
pub mod scans;
pub mod types;

use crate::model::Candle;
use serde::{Deserialize, Serialize};
use types::Detection;

/// Tunables for the recognisers that genuinely need them.
///
/// Candlestick rules use fixed thresholds — there is little honest disagreement
/// about what an engulfing bar is. Screener scans are where traders differ, so
/// those get knobs and nothing else clutters the settings file.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct PatternParams {
    pub chartink: scans::CupBreakoutParams,
}

/// Run every recogniser over one series.
///
/// Detections are de-duplicated on `(kind, end bar)` keeping the strongest
/// score — several windows can legitimately match the same structure, and the
/// user wants one row per pattern occurrence, not one per window that saw it.
pub fn detect_all(candles: &[Candle], params: &PatternParams) -> Vec<Detection> {
    let mut all = candlesticks::detect(candles);
    all.extend(scans::cup_breakout(candles, &params.chartink));

    all.sort_by(|a, b| {
        (a.kind.key(), a.end)
            .cmp(&(b.kind.key(), b.end))
            .then(b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal))
    });
    all.dedup_by(|a, b| a.kind == b.kind && a.end == b.end);

    // Present newest first, strongest first within a bar.
    all.sort_by(|a, b| {
        b.end
            .cmp(&a.end)
            .then(b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal))
    });
    all
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;

    fn candles(n: usize) -> Vec<Candle> {
        (0..n)
            .map(|i| {
                let p = 100.0 + (i as f64 * 0.7).sin() * 8.0 + i as f64 * 0.2;
                Candle {
                    date: NaiveDate::from_ymd_opt(2024, 1, 1).unwrap() + chrono::Duration::days(i as i64),
                    open: p,
                    high: p * 1.02,
                    low: p * 0.98,
                    close: p * 1.005,
                    volume: 50_000,
                }
            })
            .collect()
    }

    #[test]
    fn no_duplicate_kind_at_the_same_bar() {
        let dets = detect_all(&candles(220), &PatternParams::default());
        let mut seen = std::collections::HashSet::new();
        for d in &dets {
            assert!(seen.insert((d.kind.key(), d.end)), "duplicate {:?} at bar {}", d.kind, d.end);
        }
    }

    #[test]
    fn results_are_newest_first() {
        let dets = detect_all(&candles(220), &PatternParams::default());
        for w in dets.windows(2) {
            assert!(w[0].end >= w[1].end, "not sorted newest-first");
        }
    }

    #[test]
    fn empty_and_tiny_series_are_safe() {
        assert!(detect_all(&[], &PatternParams::default()).is_empty());
        assert!(detect_all(&candles(3), &PatternParams::default()).len() < 10);
    }

    #[test]
    fn only_candlesticks_and_the_chartink_scan_are_produced() {
        use types::{Family, PatternKind};
        let dets = detect_all(&candles(220), &PatternParams::default());
        for d in &dets {
            assert!(
                d.kind.family() == Family::Candlestick || d.kind == PatternKind::ChartinkCupBreakout,
                "geometric chart structures were removed, but {:?} appeared",
                d.kind
            );
        }
    }
}
