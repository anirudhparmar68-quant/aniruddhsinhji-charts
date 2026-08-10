//! Shared technical primitives: moving averages, RSI, ADX, trend context and
//! line fitting.
//!
//! Everything here is index-based over an ascending slice of daily candles.
//! Series-returning functions always yield one entry per input bar, using
//! `None` for the warm-up period so indices never shift.

use crate::model::Candle;

// ---------------------------------------------------------------------------
// Moving averages and volatility
// ---------------------------------------------------------------------------

pub fn sma(values: &[f64], period: usize) -> Vec<Option<f64>> {
    let mut out = vec![None; values.len()];
    if period == 0 || values.len() < period {
        return out;
    }
    let mut sum: f64 = values[..period].iter().sum();
    out[period - 1] = Some(sum / period as f64);
    for i in period..values.len() {
        sum += values[i] - values[i - period];
        out[i] = Some(sum / period as f64);
    }
    out
}

pub fn true_range(candles: &[Candle]) -> Vec<f64> {
    let mut out = Vec::with_capacity(candles.len());
    for (i, c) in candles.iter().enumerate() {
        if i == 0 {
            out.push(c.range());
        } else {
            let pc = candles[i - 1].close;
            out.push(c.range().max((c.high - pc).abs()).max((c.low - pc).abs()));
        }
    }
    out
}

pub fn rsi(values: &[f64], period: usize) -> Vec<Option<f64>> {
    let mut out = vec![None; values.len()];
    if period == 0 || values.len() <= period {
        return out;
    }
    let (mut gain, mut loss) = (0.0, 0.0);
    for i in 1..=period {
        let ch = values[i] - values[i - 1];
        if ch >= 0.0 { gain += ch } else { loss -= ch }
    }
    let (mut avg_gain, mut avg_loss) = (gain / period as f64, loss / period as f64);
    out[period] = Some(rsi_from(avg_gain, avg_loss));
    for i in period + 1..values.len() {
        let ch = values[i] - values[i - 1];
        let (g, l) = if ch >= 0.0 { (ch, 0.0) } else { (0.0, -ch) };
        avg_gain = (avg_gain * (period as f64 - 1.0) + g) / period as f64;
        avg_loss = (avg_loss * (period as f64 - 1.0) + l) / period as f64;
        out[i] = Some(rsi_from(avg_gain, avg_loss));
    }
    out
}

fn rsi_from(avg_gain: f64, avg_loss: f64) -> f64 {
    if avg_loss <= f64::EPSILON {
        return 100.0;
    }
    let rs = avg_gain / avg_loss;
    100.0 - 100.0 / (1.0 + rs)
}

/// Wilder's smoothing: seed with a plain sum of the first `period` values, then
/// `next = prev - prev/period + value`. Shared by ADX's three running totals.
fn wilder_smooth(values: &[f64], period: usize) -> Vec<Option<f64>> {
    let mut out = vec![None; values.len()];
    if period == 0 || values.len() < period {
        return out;
    }
    let mut acc: f64 = values[..period].iter().sum();
    out[period - 1] = Some(acc);
    for i in period..values.len() {
        acc = acc - acc / period as f64 + values[i];
        out[i] = Some(acc);
    }
    out
}

/// Wilder's Average Directional Index.
///
/// Returns one value per bar; `None` through the warm-up. ADX needs two
/// smoothing passes, so the first real value lands at bar `2 * period - 1`.
pub fn adx(candles: &[Candle], period: usize) -> Vec<Option<f64>> {
    let n = candles.len();
    let mut out = vec![None; n];
    if period == 0 || n < period * 2 {
        return out;
    }

    // Directional movement, defined from bar 1 onwards.
    let mut plus_dm = vec![0.0; n];
    let mut minus_dm = vec![0.0; n];
    for i in 1..n {
        let up = candles[i].high - candles[i - 1].high;
        let down = candles[i - 1].low - candles[i].low;
        // Only the larger of the two counts, and only when positive.
        plus_dm[i] = if up > down && up > 0.0 { up } else { 0.0 };
        minus_dm[i] = if down > up && down > 0.0 { down } else { 0.0 };
    }

    let tr = true_range(candles);
    // Bar 0's "true range" is just its own range and has no directional move to
    // pair with, so all three series are smoothed over the same 1.. window.
    let sm_tr = wilder_smooth(&tr[1..], period);
    let sm_plus = wilder_smooth(&plus_dm[1..], period);
    let sm_minus = wilder_smooth(&minus_dm[1..], period);

    let mut dx = Vec::with_capacity(n);
    let mut dx_index = Vec::with_capacity(n);
    for k in 0..sm_tr.len() {
        let (Some(t), Some(p), Some(m)) = (sm_tr[k], sm_plus[k], sm_minus[k]) else { continue };
        if t <= f64::EPSILON {
            continue;
        }
        let di_plus = 100.0 * p / t;
        let di_minus = 100.0 * m / t;
        let sum = di_plus + di_minus;
        if sum <= f64::EPSILON {
            continue;
        }
        dx.push(100.0 * (di_plus - di_minus).abs() / sum);
        dx_index.push(k + 1); // shift back to candle indices
    }

    if dx.len() < period {
        return out;
    }
    // ADX is a Wilder *average* of DX, not a running sum.
    let mut adx_value = dx[..period].iter().sum::<f64>() / period as f64;
    out[dx_index[period - 1]] = Some(adx_value);
    for k in period..dx.len() {
        adx_value = (adx_value * (period as f64 - 1.0) + dx[k]) / period as f64;
        out[dx_index[k]] = Some(adx_value);
    }
    out
}

/// Highest close over the `window` bars ending at `i` (inclusive).
pub fn highest_close(candles: &[Candle], i: usize, window: usize) -> Option<f64> {
    if window == 0 || i + 1 < window {
        return None;
    }
    Some(candles[i + 1 - window..=i].iter().fold(f64::MIN, |a, c| a.max(c.close)))
}

/// Lowest close over the `window` bars ending at `i` (inclusive).
pub fn lowest_close(candles: &[Candle], i: usize, window: usize) -> Option<f64> {
    if window == 0 || i + 1 < window {
        return None;
    }
    Some(candles[i + 1 - window..=i].iter().fold(f64::MAX, |a, c| a.min(c.close)))
}

// ---------------------------------------------------------------------------
// Local statistics used by the candlestick rules
// ---------------------------------------------------------------------------

/// Mean body size of the `lookback` bars *before* `i`. Candlestick rules are
/// relative ("a long body"), so they need a local yardstick rather than an
/// absolute rupee amount.
pub fn avg_body_before(candles: &[Candle], i: usize, lookback: usize) -> f64 {
    let start = i.saturating_sub(lookback);
    let slice = &candles[start..i];
    if slice.is_empty() {
        return candles[i].body().max(f64::EPSILON);
    }
    let mean = slice.iter().map(|c| c.body()).sum::<f64>() / slice.len() as f64;
    mean.max(f64::EPSILON)
}

pub fn avg_range_before(candles: &[Candle], i: usize, lookback: usize) -> f64 {
    let start = i.saturating_sub(lookback);
    let slice = &candles[start..i];
    if slice.is_empty() {
        return candles[i].range().max(f64::EPSILON);
    }
    let mean = slice.iter().map(|c| c.range()).sum::<f64>() / slice.len() as f64;
    mean.max(f64::EPSILON)
}

pub fn avg_volume_before(candles: &[Candle], i: usize, lookback: usize) -> f64 {
    let start = i.saturating_sub(lookback);
    let slice = &candles[start..i];
    if slice.is_empty() {
        return candles[i].volume as f64;
    }
    slice.iter().map(|c| c.volume as f64).sum::<f64>() / slice.len() as f64
}

pub fn highest_high(candles: &[Candle], from: usize, to: usize) -> f64 {
    candles[from..=to.min(candles.len() - 1)]
        .iter()
        .fold(f64::MIN, |acc, c| acc.max(c.high))
}

// ---------------------------------------------------------------------------
// Trend context
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trend {
    Up,
    Down,
    Side,
}

/// Trend of the `lookback` bars ending just before `i`.
///
/// Reversal candlesticks are only meaningful against a prior move — a hammer in
/// a sideways drift is noise. We fit a line to the closes and express its total
/// rise over the window as a fraction of the mean price, which keeps the
/// threshold comparable across a ₹20 stock and a ₹20,000 one.
pub fn trend_before(candles: &[Candle], i: usize, lookback: usize, threshold: f64) -> Trend {
    if i == 0 || lookback < 3 {
        return Trend::Side;
    }
    let start = i.saturating_sub(lookback);
    let closes: Vec<f64> = candles[start..i].iter().map(|c| c.close).collect();
    if closes.len() < 3 {
        return Trend::Side;
    }
    let xs: Vec<f64> = (0..closes.len()).map(|x| x as f64).collect();
    let Some(fit) = linreg(&xs, &closes) else { return Trend::Side };

    let mean = closes.iter().sum::<f64>() / closes.len() as f64;
    if mean <= 0.0 {
        return Trend::Side;
    }
    let total_move = fit.slope * (closes.len() - 1) as f64 / mean;

    if total_move > threshold {
        Trend::Up
    } else if total_move < -threshold {
        Trend::Down
    } else {
        Trend::Side
    }
}

/// Default trend context: 10 bars, 3% net move.
pub fn trend(candles: &[Candle], i: usize) -> Trend {
    trend_before(candles, i, 10, 0.03)
}

// ---------------------------------------------------------------------------
// Line fitting
// ---------------------------------------------------------------------------

/// Only `slope` is consumed today (by [`trend_before`]); the other two are part
/// of what a least-squares fit *is* and are asserted by its test.
#[derive(Debug, Clone, Copy)]
#[allow(dead_code)]
pub struct LineFit {
    pub slope: f64,
    pub intercept: f64,
    /// Coefficient of determination, 0..=1.
    pub r2: f64,
}

/// Ordinary least squares. `None` when the x values are degenerate.
pub fn linreg(xs: &[f64], ys: &[f64]) -> Option<LineFit> {
    let n = xs.len();
    if n < 2 || n != ys.len() {
        return None;
    }
    let nf = n as f64;
    let mean_x = xs.iter().sum::<f64>() / nf;
    let mean_y = ys.iter().sum::<f64>() / nf;

    let mut sxx = 0.0;
    let mut sxy = 0.0;
    for k in 0..n {
        let dx = xs[k] - mean_x;
        sxx += dx * dx;
        sxy += dx * (ys[k] - mean_y);
    }
    if sxx.abs() < f64::EPSILON {
        return None;
    }
    let slope = sxy / sxx;
    let intercept = mean_y - slope * mean_x;

    let mut ss_res = 0.0;
    let mut ss_tot = 0.0;
    for k in 0..n {
        let pred = slope * xs[k] + intercept;
        ss_res += (ys[k] - pred).powi(2);
        ss_tot += (ys[k] - mean_y).powi(2);
    }
    // A perfectly flat series has no variance to explain; treat it as a clean fit.
    let r2 = if ss_tot.abs() < f64::EPSILON { 1.0 } else { 1.0 - ss_res / ss_tot };

    Some(LineFit { slope, intercept, r2 })
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;

    fn series(values: &[(f64, f64, f64, f64)]) -> Vec<Candle> {
        values
            .iter()
            .enumerate()
            .map(|(i, &(o, h, l, c))| Candle {
                date: NaiveDate::from_ymd_opt(2025, 1, 1).unwrap() + chrono::Duration::days(i as i64),
                open: o,
                high: h,
                low: l,
                close: c,
                volume: 1_000,
            })
            .collect()
    }

    /// Flat-then-ramp closes, handy for trend and MA checks.
    fn from_closes(closes: &[f64]) -> Vec<Candle> {
        let v: Vec<(f64, f64, f64, f64)> = closes
            .iter()
            .map(|&c| (c, c + 0.5, c - 0.5, c))
            .collect();
        series(&v)
    }

    #[test]
    fn sma_matches_hand_computation() {
        let out = sma(&[1.0, 2.0, 3.0, 4.0], 2);
        assert_eq!(out[0], None);
        assert_eq!(out[1], Some(1.5));
        assert_eq!(out[3], Some(3.5));
    }

    #[test]
    fn series_functions_never_shift_indices() {
        let closes = [1.0, 2.0, 3.0, 4.0, 5.0];
        assert_eq!(sma(&closes, 3).len(), closes.len());
        assert_eq!(rsi(&closes, 3).len(), closes.len());
        assert_eq!(adx(&from_closes(&closes), 3).len(), closes.len());
    }

    #[test]
    fn rsi_is_100_when_only_gains() {
        let out = rsi(&[1.0, 2.0, 3.0, 4.0, 5.0, 6.0], 3);
        assert_eq!(out[3], Some(100.0));
    }

    #[test]
    fn trend_detects_direction() {
        let up = from_closes(&[100.0, 102.0, 104.0, 106.0, 108.0, 110.0, 112.0, 114.0]);
        assert_eq!(trend_before(&up, 7, 7, 0.03), Trend::Up);

        let down = from_closes(&[114.0, 112.0, 110.0, 108.0, 106.0, 104.0, 102.0, 100.0]);
        assert_eq!(trend_before(&down, 7, 7, 0.03), Trend::Down);

        let flat = from_closes(&[100.0, 100.4, 99.8, 100.2, 100.0, 99.9, 100.1, 100.0]);
        assert_eq!(trend_before(&flat, 7, 7, 0.03), Trend::Side);
    }

    #[test]
    fn linreg_recovers_a_known_line() {
        let xs: Vec<f64> = (0..10).map(|i| i as f64).collect();
        let ys: Vec<f64> = xs.iter().map(|x| 3.0 * x + 7.0).collect();
        let fit = linreg(&xs, &ys).unwrap();
        assert!((fit.slope - 3.0).abs() < 1e-9);
        assert!((fit.intercept - 7.0).abs() < 1e-9);
        assert!((fit.r2 - 1.0).abs() < 1e-9);
    }

    #[test]
    fn adx_warms_up_then_reports() {
        let candles = from_closes(&(0..60).map(|i| 100.0 + i as f64).collect::<Vec<_>>());
        let out = adx(&candles, 14);
        assert_eq!(out.len(), candles.len());
        // Two smoothing passes, so nothing before bar 2*period - 1.
        assert!(out[..27].iter().all(|v| v.is_none()), "ADX reported during warm-up");
        assert!(out.last().unwrap().is_some(), "ADX never produced a value");
    }

    #[test]
    fn adx_separates_trend_from_chop() {
        let trending = from_closes(&(0..80).map(|i| 100.0 + i as f64 * 1.5).collect::<Vec<_>>());
        let choppy = from_closes(&(0..80).map(|i| 100.0 + ((i % 2) as f64) * 1.5).collect::<Vec<_>>());

        let t = adx(&trending, 14).last().copied().flatten().unwrap();
        let c = adx(&choppy, 14).last().copied().flatten().unwrap();
        assert!(t > 25.0, "a clean trend should read high, got {t:.1}");
        assert!(c < t, "chop ({c:.1}) must read below a trend ({t:.1})");
    }

    #[test]
    fn adx_is_bounded_and_safe_on_short_input() {
        let candles = from_closes(&[100.0, 101.0, 102.0]);
        assert!(adx(&candles, 14).iter().all(|v| v.is_none()));
        assert!(adx(&[], 14).is_empty());

        let long = from_closes(&(0..90).map(|i| 100.0 + (i as f64 * 0.3).sin() * 6.0).collect::<Vec<_>>());
        for v in adx(&long, 14).into_iter().flatten() {
            assert!((0.0..=100.0).contains(&v), "ADX out of range: {v}");
        }
    }

    #[test]
    fn rolling_close_extremes_respect_the_window() {
        let candles = from_closes(&[10.0, 12.0, 8.0, 15.0, 11.0]);
        // Window of 3 ending at bar 4 covers closes 8, 15, 11.
        assert_eq!(highest_close(&candles, 4, 3), Some(15.0));
        assert_eq!(lowest_close(&candles, 4, 3), Some(8.0));
        assert_eq!(highest_close(&candles, 4, 5), Some(15.0));
        // Not enough history yet.
        assert_eq!(highest_close(&candles, 1, 3), None);
        assert_eq!(lowest_close(&candles, 0, 2), None);
    }

}
