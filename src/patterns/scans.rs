//! Screener-style scans ported from the user's own Chartink setups.
//!
//! These differ from the geometric recognisers in `breakout.rs`: instead of
//! measuring a shape, they apply a checklist of indicator conditions to each
//! bar. That trades shape fidelity for objectivity — there is nothing to argue
//! about once the thresholds are fixed — which is exactly why they are kept
//! side by side with the geometric detectors rather than replacing them.

use crate::model::Candle;
use crate::patterns::types::{Detection, PatternKind as K};
use crate::ta;
use serde::{Deserialize, Serialize};

/// Thresholds for the "cup and handle breakout" Chartink scan.
///
/// Defaults reproduce the scan exactly as the user runs it:
///
/// ```text
/// Close crossed above Max(30, 1 day ago Close)
/// Min(20, Close)        <  Max(60, Close) * 0.85
/// Close                 >  Sma(Close, 50)
/// Sma(Volume, 5)        >  Sma(Volume, 50)
/// Rsi(14)               >  55
/// Adx(14)               >  20
/// Volume                >  1.5 * Sma(Volume, 20)
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct CupBreakoutParams {
    /// Closing-high window the breakout must clear.
    pub breakout_lookback: usize,
    /// Window for the recent low that establishes the drawdown.
    pub depth_window: usize,
    /// Window for the prior high the drawdown is measured against.
    pub base_window: usize,
    /// Recent low must sit below `base high × this`. 0.85 ⇒ at least a 15% dip.
    pub depth_ratio: f64,
    pub trend_sma: usize,
    pub volume_fast_sma: usize,
    pub volume_slow_sma: usize,
    pub rsi_period: usize,
    pub rsi_min: f64,
    pub adx_period: usize,
    pub adx_min: f64,
    /// Today's volume must exceed `this × Sma(Volume, volume_sma)`.
    pub volume_multiple: f64,
    pub volume_sma: usize,
}

impl Default for CupBreakoutParams {
    fn default() -> Self {
        Self {
            breakout_lookback: 30,
            depth_window: 20,
            base_window: 60,
            depth_ratio: 0.85,
            trend_sma: 50,
            volume_fast_sma: 5,
            volume_slow_sma: 50,
            rsi_period: 14,
            rsi_min: 55.0,
            adx_period: 14,
            adx_min: 20.0,
            volume_multiple: 1.5,
            volume_sma: 20,
        }
    }
}

/// Every bar where the full checklist passes.
pub fn cup_breakout(candles: &[Candle], p: &CupBreakoutParams) -> Vec<Detection> {
    // Longest warm-up of any condition decides where scanning can start.
    let warmup = p
        .base_window
        .max(p.trend_sma)
        .max(p.volume_slow_sma)
        .max(p.breakout_lookback + 2)
        .max(p.adx_period * 2)
        + 2;
    if candles.len() <= warmup {
        return Vec::new();
    }

    let closes: Vec<f64> = candles.iter().map(|c| c.close).collect();
    let volumes: Vec<f64> = candles.iter().map(|c| c.volume as f64).collect();

    let sma_close = ta::sma(&closes, p.trend_sma);
    let sma_close_short = ta::sma(&closes, p.volume_sma);
    let sma_vol_fast = ta::sma(&volumes, p.volume_fast_sma);
    let sma_vol_slow = ta::sma(&volumes, p.volume_slow_sma);
    let sma_vol_ref = ta::sma(&volumes, p.volume_sma);
    let rsi = ta::rsi(&closes, p.rsi_period);
    let adx = ta::adx(candles, p.adx_period);
    let _ = sma_close_short; // kept so the literal Chartink line stays easy to restore

    // `Max(30, 1 day ago Close)` — the highest close over the 30 bars *before* i.
    let prior_high = |i: usize| -> Option<f64> {
        if i == 0 {
            return None;
        }
        ta::highest_close(candles, i - 1, p.breakout_lookback)
    };
    let above = |i: usize| -> bool {
        matches!((closes.get(i), prior_high(i)), (Some(c), Some(h)) if *c > h)
    };

    let mut out = Vec::new();
    for i in warmup..candles.len() {
        // "Crossed above", not merely "is above": yesterday must not have qualified.
        if !above(i) || above(i - 1) {
            continue;
        }

        let (Some(recent_low), Some(base_high)) = (
            ta::lowest_close(candles, i, p.depth_window),
            ta::highest_close(candles, i, p.base_window),
        ) else {
            continue;
        };
        if recent_low >= base_high * p.depth_ratio {
            continue;
        }

        let (Some(trend), Some(vf), Some(vs), Some(vref), Some(r), Some(a)) = (
            sma_close[i],
            sma_vol_fast[i],
            sma_vol_slow[i],
            sma_vol_ref[i],
            rsi[i],
            adx[i],
        ) else {
            continue;
        };

        if closes[i] <= trend || vf <= vs || r <= p.rsi_min || a <= p.adx_min {
            continue;
        }
        if volumes[i] <= p.volume_multiple * vref {
            continue;
        }

        let depth = 1.0 - recent_low / base_high.max(f64::EPSILON);
        let vol_x = volumes[i] / vref.max(f64::EPSILON);
        // Score the margin by which the discretionary conditions were beaten,
        // so the strongest breakouts sort to the top of the scanner.
        let score = 0.60
            + ((depth - (1.0 - p.depth_ratio)) / 0.25).clamp(0.0, 1.0) * 0.15
            + ((vol_x - p.volume_multiple) / 3.0).clamp(0.0, 1.0) * 0.15
            + ((a - p.adx_min) / 25.0).clamp(0.0, 1.0) * 0.10;

        out.push(
            Detection::new(K::ChartinkCupBreakout, i.saturating_sub(p.base_window), i, score)
                .with_detail(format!(
                    "broke the {}-day closing high after a {:.0}% dip · RSI {:.0} · ADX {:.0} · {:.1}× avg volume",
                    p.breakout_lookback,
                    depth * 100.0,
                    r,
                    a,
                    vol_x
                )),
        );
    }
    out
}

/// Where the parts of a cup breakout sit, for drawing it on a chart.
///
/// The scan never tests shape, so these points are chosen afterwards, from
/// closes only, inside the scan's own `base_window`:
///
/// - left rim, bottom and right rim: the trio that makes the deepest cup,
///   measured from the lower rim down to the bottom, where the bottom is the
///   lowest close between the rims;
/// - handle: the lowest close between the right rim and the breakout, if any;
/// - neckline: `Max(breakout_lookback, 1 day ago Close)`, the level the scan
///   says was broken. It often equals the right rim, but not always.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CupGeometry {
    pub left_rim: usize,
    pub bottom: usize,
    pub right_rim: usize,
    pub handle_low: Option<usize>,
    pub breakout: usize,
    pub neckline: f64,
}

/// The cup drawn for a breakout on bar `i`, or `None` when the base holds no
/// dip below both rims to draw.
pub fn cup_geometry(candles: &[Candle], i: usize, p: &CupBreakoutParams) -> Option<CupGeometry> {
    if i >= candles.len() || p.base_window < 3 || i + 1 < p.base_window || i < p.breakout_lookback {
        return None;
    }
    let close = |k: usize| candles[k].close;
    let start = i + 1 - p.base_window;

    // For each right rim, walk the left rim back from it while keeping the
    // lowest close in between, so every (left, bottom, right) trio is seen once.
    let mut best: Option<(f64, usize, usize, usize)> = None;
    for right in start + 2..i {
        let mut bottom = right - 1;
        for left in (start..right - 1).rev() {
            if close(left + 1) < close(bottom) {
                bottom = left + 1;
            }
            let depth = close(left).min(close(right)) - close(bottom);
            // Depth is capped by the lower rim, so many left rims can tie. The
            // highest of them is the real peak; any other sits on the way down.
            let better = match best {
                None => true,
                Some((d, l, ..)) => depth > d || (depth == d && close(left) > close(l)),
            };
            if better {
                best = Some((depth, left, bottom, right));
            }
        }
    }
    let (depth, left_rim, bottom, right_rim) = best?;
    if depth <= 0.0 {
        return None;
    }

    // Latest bar wins a tie, as Chartink's "1 day ago" window reads it.
    let handle_low = (right_rim + 1..i).min_by(|&a, &b| close(a).total_cmp(&close(b)).then(b.cmp(&a)));
    let neckline = ta::highest_close(candles, i - 1, p.breakout_lookback)?;

    Some(CupGeometry { left_rim, bottom, right_rim, handle_low, breakout: i, neckline })
}

/// Every bar where yesterday was an inside bar and today closes above its high.
///
/// ```text
/// Daily 1 day ago High  <  2 days ago High
/// Daily 1 day ago Low   >  2 days ago Low
/// Daily Close           >  1 day ago High
/// ```
pub fn inside_bar_breakout(candles: &[Candle]) -> Vec<Detection> {
    let mut out = Vec::new();
    for i in 2..candles.len() {
        let (mother, inside, today) = (&candles[i - 2], &candles[i - 1], &candles[i]);
        if inside.high >= mother.high || inside.low <= mother.low {
            continue;
        }
        if today.close <= inside.high {
            continue;
        }

        // Score by how far the close cleared the inside bar's high; 3% or more is full marks.
        let margin = (today.close - inside.high) / inside.high;
        let score = 0.70 + (margin / 0.03).clamp(0.0, 1.0) * 0.30;

        out.push(
            Detection::new(K::ChartinkInsideBarBreakout, i - 2, i, score)
                .with_detail(format!("closed {:.1}% above the inside bar's high", margin * 100.0)),
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;

    fn bar(day: i64, high: f64, low: f64, close: f64) -> Candle {
        Candle {
            date: NaiveDate::from_ymd_opt(2024, 1, 1).unwrap() + chrono::Duration::days(day),
            open: close,
            high,
            low,
            close,
            volume: 100_000,
        }
    }

    #[test]
    fn inside_bar_breakout_fires_on_the_close_above_the_inside_high() {
        let candles = vec![
            bar(0, 102.0, 98.0, 100.0), // mother bar
            bar(1, 101.0, 99.0, 100.0), // inside bar
            bar(2, 103.0, 100.5, 101.5), // closes above 101
        ];
        let hits = inside_bar_breakout(&candles);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].kind, K::ChartinkInsideBarBreakout);
        assert_eq!((hits[0].start, hits[0].end), (0, 2));
    }

    #[test]
    fn inside_bar_breakout_needs_the_close_above_the_inside_high() {
        let candles = vec![
            bar(0, 102.0, 98.0, 100.0),
            bar(1, 101.0, 99.0, 100.0),
            bar(2, 103.0, 100.5, 100.5), // closes below 101
        ];
        assert!(inside_bar_breakout(&candles).is_empty());
    }

    #[test]
    fn inside_bar_breakout_needs_both_the_lower_high_and_the_higher_low() {
        // Yesterday's low undercuts the mother bar's low, so it is not an inside bar.
        let candles = vec![
            bar(0, 102.0, 98.0, 100.0),
            bar(1, 101.0, 97.5, 100.0),
            bar(2, 103.0, 100.5, 101.5),
        ];
        assert!(inside_bar_breakout(&candles).is_empty());
    }

    fn build(rows: &[(f64, i64)]) -> Vec<Candle> {
        rows.iter()
            .enumerate()
            .map(|(i, &(p, v))| Candle {
                date: NaiveDate::from_ymd_opt(2024, 1, 1).unwrap() + chrono::Duration::days(i as i64),
                open: p * 0.995,
                high: p * 1.012,
                low: p * 0.988,
                close: p,
                volume: v,
            })
            .collect()
    }

    /// Rally, sharp correction, a quiet base, then a fresh breakout on heavy
    /// volume — the shape the scan is meant to catch.
    ///
    /// The base deliberately stays well under the old high. A long *rising*
    /// recovery would set a new 30-day closing high partway up, so "crossed
    /// above" would fire mid-recovery and the real breakout bar would then be
    /// only a continuation, not a cross.
    fn breakout_series() -> Vec<Candle> {
        let mut rows: Vec<(f64, i64)> = Vec::new();
        // 80 bars rallying 100 → 150.
        for i in 0..80 {
            rows.push((100.0 + i as f64 * 0.625, 100_000));
        }
        // 10 bars correcting hard to ~118, i.e. 21% off the high.
        for i in 0..10 {
            rows.push((150.0 - i as f64 * 3.55, 150_000));
        }
        // 14 quiet bars basing 120 → 130, still far below the old high.
        for i in 0..14 {
            rows.push((120.0 + i as f64 * 0.77, 80_000));
        }
        // Breakout: clears the 30-day closing high on 5x volume.
        rows.push((152.0, 600_000));
        build(&rows)
    }

    /// Same breakout, but the pullback was only ~4% — the depth rule must veto it.
    fn shallow_dip_series() -> Vec<Candle> {
        let mut rows: Vec<(f64, i64)> = Vec::new();
        for i in 0..80 {
            rows.push((100.0 + i as f64 * 0.625, 100_000));
        }
        for i in 0..10 {
            rows.push((150.0 - i as f64 * 0.7, 150_000));
        }
        for i in 0..14 {
            rows.push((144.0 + i as f64 * 0.3, 80_000));
        }
        rows.push((152.0, 600_000));
        build(&rows)
    }

    #[test]
    fn fires_on_a_clean_breakout() {
        let candles = breakout_series();
        let hits = cup_breakout(&candles, &CupBreakoutParams::default());
        assert!(!hits.is_empty(), "expected the scan to fire");
        let last = hits.last().unwrap();
        assert_eq!(last.end, candles.len() - 1);
        assert_eq!(last.kind, K::ChartinkCupBreakout);
    }

    #[test]
    fn does_not_fire_without_the_drawdown() {
        let hits = cup_breakout(&shallow_dip_series(), &CupBreakoutParams::default());
        assert!(hits.is_empty(), "a 4% dip must fail the 15% depth rule: {hits:?}");
    }

    #[test]
    fn does_not_fire_on_a_steady_grind_up() {
        // Every bar is already a new 30-day closing high, so nothing ever *crosses*.
        let rows: Vec<(f64, i64)> = (0..140)
            .map(|i| (100.0 + i as f64 * 0.5, if i == 139 { 400_000 } else { 100_000 }))
            .collect();
        assert!(cup_breakout(&build(&rows), &CupBreakoutParams::default()).is_empty());
    }

    #[test]
    fn does_not_fire_without_the_volume_surge() {
        let mut candles = breakout_series();
        let last = candles.len() - 1;
        candles[last].volume = 100_000; // below 1.5x the 20-day average
        let hits = cup_breakout(&candles, &CupBreakoutParams::default());
        assert!(hits.iter().all(|h| h.end != last), "volume filter should reject the final bar");
    }

    #[test]
    fn requires_a_fresh_cross_not_a_standing_high() {
        let candles = breakout_series();
        let hits = cup_breakout(&candles, &CupBreakoutParams::default());
        // Extend with more new highs; those bars are already above the prior
        // high, so "crossed above" must not re-fire on every one of them.
        let mut extended: Vec<(f64, i64)> = candles.iter().map(|c| (c.close, c.volume)).collect();
        for i in 0..5 {
            extended.push((157.0 + i as f64, 400_000));
        }
        let more = cup_breakout(&build(&extended), &CupBreakoutParams::default());
        assert_eq!(
            more.len(),
            hits.len(),
            "a standing high must not re-fire on every subsequent bar"
        );
    }

    #[test]
    fn cup_geometry_finds_the_rims_bottom_and_neckline() {
        let candles = breakout_series();
        let i = candles.len() - 1;
        let g = cup_geometry(&candles, i, &CupBreakoutParams::default()).expect("a cup to draw");
        // Peak of the rally, low of the correction, top of the quiet base.
        assert_eq!((g.left_rim, g.bottom, g.right_rim, g.breakout), (80, 89, 103, i));
        // The base runs straight into the breakout, so there is no handle.
        assert_eq!(g.handle_low, None);
        // The level broken is the 30-day closing high, which here is the left rim.
        assert_eq!(g.neckline, candles[80].close);
    }

    #[test]
    fn cup_geometry_marks_the_handle_low() {
        let mut rows: Vec<(f64, i64)> = breakout_series().iter().map(|c| (c.close, c.volume)).collect();
        let breakout = rows.pop().unwrap();
        rows.extend([(126.0, 80_000), (124.0, 80_000), (127.0, 80_000)]);
        rows.push(breakout);
        let candles = build(&rows);
        let g = cup_geometry(&candles, candles.len() - 1, &CupBreakoutParams::default()).expect("a cup to draw");
        assert_eq!(g.right_rim, 103);
        assert_eq!(g.handle_low, Some(105), "the 124 close is the handle's low");
    }

    #[test]
    fn cup_geometry_needs_a_dip() {
        let rows: Vec<(f64, i64)> = (0..140).map(|i| (100.0 + i as f64 * 0.5, 100_000)).collect();
        let candles = build(&rows);
        assert_eq!(cup_geometry(&candles, 139, &CupBreakoutParams::default()), None);
        assert_eq!(cup_geometry(&candles, 10, &CupBreakoutParams::default()), None, "too early for a base");
    }

    #[test]
    fn short_series_is_safe() {
        assert!(cup_breakout(&build(&[(100.0, 1000); 20]), &CupBreakoutParams::default()).is_empty());
        assert!(cup_breakout(&[], &CupBreakoutParams::default()).is_empty());
    }

    #[test]
    fn thresholds_are_honoured() {
        let candles = breakout_series();
        let strict = CupBreakoutParams { rsi_min: 99.0, ..Default::default() };
        assert!(cup_breakout(&candles, &strict).is_empty(), "an impossible RSI floor must reject everything");

        let strict_adx = CupBreakoutParams { adx_min: 99.0, ..Default::default() };
        assert!(cup_breakout(&candles, &strict_adx).is_empty(), "an impossible ADX floor must reject everything");
    }
}
