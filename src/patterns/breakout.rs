//! Multi-week chart structures: bases, reversals and continuations.
//!
//! These are defined on *swing structure*, not on individual bars, so almost
//! everything here works from the zig-zag pivot series rather than raw candles.
//! The zig-zag threshold adapts to the stock's own volatility — a 4% swing means
//! something very different in a utility than in a smallcap.

use crate::model::Candle;
use crate::patterns::types::{Detection, PatternKind as K};
use crate::ta::{self, Pivot, PivotKind};
use serde::{Deserialize, Serialize};

/// Chart patterns need a meaningful history before they mean anything.
const MIN_BARS: usize = 40;

/// Tunables for the cup-and-handle recogniser.
///
/// Every other structure in this file uses fixed thresholds, because there is
/// little honest disagreement about what a double top or a 52-week breakout is.
/// A cup is different: how round is round, how deep is deep, how long a handle
/// may drift — reasonable traders answer differently. So these are exposed
/// rather than baked in.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct CupParams {
    pub min_cup_bars: usize,
    pub max_cup_bars: usize,
    /// Maximum difference between the two rims, as a fraction.
    pub max_rim_asymmetry: f64,
    pub min_depth: f64,
    pub max_depth: f64,
    /// How much flatter the base must be than the cup's legs, 0..1.
    pub min_roundness: f64,
    pub min_handle_bars: usize,
    pub max_handle_bars: usize,
    /// Handle pullback as a fraction of cup depth.
    pub max_handle_retrace: f64,
    pub min_handle_depth: f64,
    pub max_handle_depth: f64,
}

impl Default for CupParams {
    fn default() -> Self {
        Self {
            min_cup_bars: 20,
            max_cup_bars: 200,
            max_rim_asymmetry: 0.10,
            min_depth: 0.10,
            max_depth: 0.55,
            min_roundness: 0.35,
            min_handle_bars: 3,
            max_handle_bars: 45,
            max_handle_retrace: 0.55,
            min_handle_depth: 0.02,
            max_handle_depth: 0.25,
        }
    }
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

pub fn detect(candles: &[Candle], cup: &CupParams) -> Vec<Detection> {
    if candles.len() < MIN_BARS {
        return Vec::new();
    }
    let threshold = swing_threshold(candles);
    let pivots = ta::zigzag(candles, threshold);

    let mut out = Vec::new();
    cup_and_handle(candles, &pivots, cup, &mut out);
    double_and_triple(candles, &pivots, &mut out);
    head_and_shoulders(candles, &pivots, &mut out);
    triangles_and_wedges(candles, &mut out);
    flags_and_pennants(candles, &mut out);
    rectangles(candles, &mut out);
    rounding(candles, &mut out);
    volatility_contraction(candles, &pivots, &mut out);
    base_breakouts(candles, &mut out);
    out
}

/// Swing size that counts as structure for this particular stock: roughly three
/// average daily ranges, clamped so noisy and sleepy names both behave.
fn swing_threshold(candles: &[Candle]) -> f64 {
    let atr = ta::atr(candles, 14);
    let mut ratios: Vec<f64> = atr
        .iter()
        .enumerate()
        .filter_map(|(i, a)| a.map(|a| a / candles[i].close.max(f64::EPSILON)))
        .filter(|r| r.is_finite() && *r > 0.0)
        .collect();
    if ratios.is_empty() {
        return 0.05;
    }
    ratios.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let median = ratios[ratios.len() / 2];
    (median * 3.0).clamp(0.03, 0.09)
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

fn pct_diff(a: f64, b: f64) -> f64 {
    let denom = a.abs().max(b.abs()).max(f64::EPSILON);
    (a - b).abs() / denom
}

/// Map a measured value onto 0..1, where `best` scores 1 and `worst` scores 0.
fn grade(value: f64, best: f64, worst: f64) -> f64 {
    if (best - worst).abs() < f64::EPSILON {
        return 0.0;
    }
    ((value - worst) / (best - worst)).clamp(0.0, 1.0)
}

fn last_close(candles: &[Candle]) -> f64 {
    candles[candles.len() - 1].close
}

/// Highs and lows of the pivot series, split by kind.
fn split_pivots(pivots: &[Pivot]) -> (Vec<Pivot>, Vec<Pivot>) {
    let highs = pivots.iter().copied().filter(|p| p.kind == PivotKind::High).collect();
    let lows = pivots.iter().copied().filter(|p| p.kind == PivotKind::Low).collect();
    (highs, lows)
}

// ---------------------------------------------------------------------------
// Cup and handle
// ---------------------------------------------------------------------------

/// O'Neil's cup with handle, plus its inverse.
///
/// Structure in pivots: rim high → rounded low → rim high → shallow handle low.
/// The shape test is what separates a real cup from a V-bottom: we require the
/// price to actually *spend time* near the lows rather than spike through them.
fn cup_and_handle(candles: &[Candle], pivots: &[Pivot], p: &CupParams, out: &mut Vec<Detection>) {
    if pivots.len() < 4 {
        return;
    }
    for w in pivots.windows(4) {
        let (a, b, c, d) = (w[0], w[1], w[2], w[3]);

        if a.kind == PivotKind::High && b.kind == PivotKind::Low {
            if let Some(det) = try_cup(candles, a, b, c, d, false, p) {
                out.push(det);
            }
        }
        if a.kind == PivotKind::Low && b.kind == PivotKind::High {
            if let Some(det) = try_cup(candles, a, b, c, d, true, p) {
                out.push(det);
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn try_cup(
    candles: &[Candle],
    left_rim: Pivot,
    bottom: Pivot,
    right_rim: Pivot,
    handle: Pivot,
    inverted: bool,
    p: &CupParams,
) -> Option<Detection> {
    let cup_len = right_rim.idx.checked_sub(left_rim.idx)?;
    if !(p.min_cup_bars..=p.max_cup_bars).contains(&cup_len) {
        return None;
    }

    let rim = (left_rim.price + right_rim.price) / 2.0;
    if rim <= 0.0 {
        return None;
    }
    // Rims must be roughly level, otherwise it is a trend leg, not a base.
    let symmetry = pct_diff(left_rim.price, right_rim.price);
    if symmetry > p.max_rim_asymmetry {
        return None;
    }

    let depth_abs = (rim - bottom.price).abs();
    let depth = depth_abs / rim;
    if !(p.min_depth..=p.max_depth).contains(&depth) {
        return None;
    }

    // Roundness is what separates a cup from a V — and it is *not* about how
    // many bars sit near the low. A straight-line V spends proportionally just
    // as long in the bottom of its range as a cosine-shaped cup does, so a
    // time-in-zone test cannot tell them apart. What actually differs is the
    // gradient: a cup flattens out across its base, while a V carries the same
    // slope straight through the turn. So compare the two.
    let leg_slope = depth_abs / (cup_len / 2).max(1) as f64;
    let pad = (cup_len / 6).max(2);
    let base_from = bottom.idx.saturating_sub(pad).max(left_rim.idx);
    let base_to = (bottom.idx + pad).min(right_rim.idx);
    if base_to <= base_from {
        return None;
    }
    let base_span =
        ta::highest_high(candles, base_from, base_to) - ta::lowest_low(candles, base_from, base_to);
    let base_slope = base_span / (base_to - base_from) as f64;
    let round_frac = 1.0 - (base_slope / leg_slope.max(f64::EPSILON)).min(1.0);
    if round_frac < p.min_roundness {
        return None;
    }

    // Handle: a shallow drift in the upper part of the cup, never more than half of it.
    let handle_len = handle.idx.checked_sub(right_rim.idx)?;
    if !(p.min_handle_bars..=p.max_handle_bars).contains(&handle_len) {
        return None;
    }
    let handle_move = (right_rim.price - handle.price).abs();
    let handle_retrace = handle_move / depth_abs.max(f64::EPSILON);
    if handle_retrace > p.max_handle_retrace {
        return None;
    }
    let handle_depth_pct = handle_move / rim;
    if !(p.min_handle_depth..=p.max_handle_depth).contains(&handle_depth_pct) {
        return None;
    }
    // The handle must lean the right way: down for a cup, up for an inverse.
    if inverted && handle.price < right_rim.price {
        return None;
    }
    if !inverted && handle.price > right_rim.price {
        return None;
    }

    let trigger = if inverted { handle.price.min(right_rim.price) } else { handle.price.max(right_rim.price) };
    let close = last_close(candles);
    let broken_out = if inverted { close < trigger } else { close > trigger };

    let score = 0.35
        + grade(1.0 - symmetry, 1.0, 1.0 - p.max_rim_asymmetry) * 0.15
        + grade(round_frac, (p.min_roundness + 0.40).min(1.0), p.min_roundness) * 0.20
        + grade(1.0 - handle_retrace, 1.0, 1.0 - p.max_handle_retrace) * 0.10
        + if broken_out { 0.20 } else { 0.0 };

    let kind = if inverted { K::InverseCupAndHandle } else { K::CupAndHandle };
    let state = if broken_out { "breakout confirmed" } else { "handle forming" };
    Some(
        Detection::new(kind, left_rim.idx, handle.idx, score).with_detail(format!(
            "depth {:.0}%, cup {} bars, handle {} bars, trigger {:.2} — {}",
            depth * 100.0,
            cup_len,
            handle_len,
            trigger,
            state
        )),
    )
}

// ---------------------------------------------------------------------------
// Double and triple tops / bottoms
// ---------------------------------------------------------------------------

fn double_and_triple(candles: &[Candle], pivots: &[Pivot], out: &mut Vec<Detection>) {
    let close = last_close(candles);

    for w in pivots.windows(3) {
        let (a, m, b) = (w[0], w[1], w[2]);
        if a.kind != b.kind || a.kind == m.kind {
            continue;
        }
        let separation = b.idx.saturating_sub(a.idx);
        if !(10..=180).contains(&separation) {
            continue;
        }
        // The two extremes must be level, and the middle move meaningful.
        let level = pct_diff(a.price, b.price);
        if level > 0.04 {
            continue;
        }
        let travel = (m.price - a.price).abs() / a.price.max(f64::EPSILON);
        if travel < 0.05 {
            continue;
        }

        let score_base = 0.4 + grade(0.04 - level, 0.04, 0.0) * 0.2 + grade(travel, 0.20, 0.05) * 0.15;
        match a.kind {
            PivotKind::Low => {
                let confirmed = close > m.price;
                out.push(
                    Detection::new(K::DoubleBottom, a.idx, b.idx, score_base + if confirmed { 0.2 } else { 0.0 })
                        .with_detail(format!(
                            "two lows {:.1}% apart, neckline {:.2}{}",
                            level * 100.0,
                            m.price,
                            if confirmed { " — broken" } else { "" }
                        )),
                );
            }
            PivotKind::High => {
                let confirmed = close < m.price;
                out.push(
                    Detection::new(K::DoubleTop, a.idx, b.idx, score_base + if confirmed { 0.2 } else { 0.0 })
                        .with_detail(format!(
                            "two highs {:.1}% apart, neckline {:.2}{}",
                            level * 100.0,
                            m.price,
                            if confirmed { " — broken" } else { "" }
                        )),
                );
            }
        }
    }

    // Triple: three same-kind extremes at the same level.
    for w in pivots.windows(5) {
        let (a, m1, b, m2, c) = (w[0], w[1], w[2], w[3], w[4]);
        if a.kind != b.kind || b.kind != c.kind || a.kind == m1.kind {
            continue;
        }
        let spread = pct_diff(a.price, b.price).max(pct_diff(b.price, c.price));
        if spread > 0.045 {
            continue;
        }
        let neckline = if a.kind == PivotKind::Low { m1.price.max(m2.price) } else { m1.price.min(m2.price) };
        let confirmed = if a.kind == PivotKind::Low { close > neckline } else { close < neckline };
        let kind = if a.kind == PivotKind::Low { K::TripleBottom } else { K::TripleTop };
        out.push(
            Detection::new(kind, a.idx, c.idx, 0.5 + grade(0.045 - spread, 0.045, 0.0) * 0.2 + if confirmed { 0.2 } else { 0.0 })
                .with_detail(format!(
                    "three tests within {:.1}%, neckline {:.2}{}",
                    spread * 100.0,
                    neckline,
                    if confirmed { " — broken" } else { "" }
                )),
        );
    }
}

// ---------------------------------------------------------------------------
// Head and shoulders
// ---------------------------------------------------------------------------

fn head_and_shoulders(candles: &[Candle], pivots: &[Pivot], out: &mut Vec<Detection>) {
    if pivots.len() < 5 {
        return;
    }
    let close = last_close(candles);

    for w in pivots.windows(5) {
        let (ls, t1, head, t2, rs) = (w[0], w[1], w[2], w[3], w[4]);
        if ls.kind != head.kind || head.kind != rs.kind || ls.kind == t1.kind {
            continue;
        }
        // Shoulders roughly level, head clearly beyond both.
        let shoulder_level = pct_diff(ls.price, rs.price);
        if shoulder_level > 0.08 {
            continue;
        }

        let (topping, neckline) = match ls.kind {
            PivotKind::High => (true, t1.price.min(t2.price)),
            PivotKind::Low => (false, t1.price.max(t2.price)),
        };
        let head_clear = if topping {
            head.price > ls.price * 1.02 && head.price > rs.price * 1.02
        } else {
            head.price < ls.price * 0.98 && head.price < rs.price * 0.98
        };
        if !head_clear {
            continue;
        }

        let confirmed = if topping { close < neckline } else { close > neckline };
        let kind = if topping { K::HeadAndShoulders } else { K::InverseHeadAndShoulders };
        let prominence = (head.price - (ls.price + rs.price) / 2.0).abs() / head.price.max(f64::EPSILON);

        out.push(
            Detection::new(kind, ls.idx, rs.idx, 0.45 + grade(prominence, 0.15, 0.02) * 0.2
                + grade(0.08 - shoulder_level, 0.08, 0.0) * 0.15
                + if confirmed { 0.2 } else { 0.0 })
                .with_detail(format!(
                    "shoulders within {:.1}%, neckline {:.2}{}",
                    shoulder_level * 100.0,
                    neckline,
                    if confirmed { " — broken" } else { "" }
                )),
        );
    }
}

// ---------------------------------------------------------------------------
// Triangles and wedges
// ---------------------------------------------------------------------------

/// Fit upper and lower trendlines through recent fractal pivots and classify by
/// their slopes. Converging lines that actually contain price are the test —
/// two arbitrary lines will always "converge" somewhere.
fn triangles_and_wedges(candles: &[Candle], out: &mut Vec<Detection>) {
    for window in [40usize, 60, 90] {
        if candles.len() < window + 5 {
            continue;
        }
        let start = candles.len() - window;
        let slice = &candles[start..];

        let pivots = ta::fractal_pivots(slice, 3, 3);
        let (highs, lows) = split_pivots(&pivots);
        if highs.len() < 3 || lows.len() < 3 {
            continue;
        }

        let hx: Vec<f64> = highs.iter().map(|p| p.idx as f64).collect();
        let hy: Vec<f64> = highs.iter().map(|p| p.price).collect();
        let lx: Vec<f64> = lows.iter().map(|p| p.idx as f64).collect();
        let ly: Vec<f64> = lows.iter().map(|p| p.price).collect();

        let (Some(upper), Some(lower)) = (ta::linreg(&hx, &hy), ta::linreg(&lx, &ly)) else {
            continue;
        };
        if upper.r2 < 0.5 || lower.r2 < 0.5 {
            continue;
        }

        let mean_price = slice.iter().map(|c| c.close).sum::<f64>() / slice.len() as f64;
        if mean_price <= 0.0 {
            continue;
        }
        // Express each slope as the total % move it implies over the window,
        // so "flat" means the same thing at any price level.
        let up_move = upper.slope * window as f64 / mean_price;
        let down_move = lower.slope * window as f64 / mean_price;
        let flat = 0.03;

        // Lines must be closing in on each other for a triangle or wedge.
        let start_gap = upper.at(0.0) - lower.at(0.0);
        let end_gap = upper.at(window as f64 - 1.0) - lower.at(window as f64 - 1.0);
        let converging = end_gap > 0.0 && start_gap > 0.0 && end_gap < start_gap * 0.75;

        let s = start;
        let e = candles.len() - 1;
        let close = last_close(candles);
        let res = upper.at((window - 1) as f64);
        let sup = lower.at((window - 1) as f64);

        if up_move.abs() <= flat && down_move > flat && converging {
            out.push(
                Detection::new(K::AscendingTriangle, s, e, 0.5 + if close > res { 0.25 } else { 0.0 })
                    .with_detail(format!("flat resistance {res:.2}, rising support{}", if close > res { " — broken out" } else { "" })),
            );
        } else if down_move.abs() <= flat && up_move < -flat && converging {
            out.push(
                Detection::new(K::DescendingTriangle, s, e, 0.5 + if close < sup { 0.25 } else { 0.0 })
                    .with_detail(format!("flat support {sup:.2}, falling resistance{}", if close < sup { " — broken down" } else { "" })),
            );
        } else if up_move < -flat && down_move > flat && converging {
            out.push(
                Detection::new(K::SymmetricalTriangle, s, e, 0.5)
                    .with_detail(format!("apex approaching, band {sup:.2}–{res:.2}")),
            );
        } else if up_move > flat && down_move > flat && converging && down_move > up_move {
            out.push(
                Detection::new(K::RisingWedge, s, e, 0.5 + if close < sup { 0.2 } else { 0.0 })
                    .with_detail("both boundaries rising, support steeper — momentum fading"),
            );
        } else if up_move < -flat && down_move < -flat && converging && up_move > down_move {
            out.push(
                Detection::new(K::FallingWedge, s, e, 0.5 + if close > res { 0.2 } else { 0.0 })
                    .with_detail("both boundaries falling, resistance steeper — selling drying up"),
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Flags and pennants
// ---------------------------------------------------------------------------

/// A sharp pole followed by a shallow counter-trend rest.
fn flags_and_pennants(candles: &[Candle], out: &mut Vec<Detection>) {
    let n = candles.len();
    for rest_len in 5..=25usize {
        for pole_len in 5..=20usize {
            if n < rest_len + pole_len + 2 {
                continue;
            }
            let pole_start = n - rest_len - pole_len;
            let pole_end = n - rest_len - 1;
            let rest = &candles[n - rest_len..];

            let pole_from = candles[pole_start].close;
            let pole_to = candles[pole_end].close;
            if pole_from <= 0.0 {
                continue;
            }
            let pole_move = (pole_to - pole_from) / pole_from;
            if pole_move.abs() < 0.15 {
                continue;
            }

            // The rest must be genuinely tight relative to the pole.
            let rest_high = rest.iter().fold(f64::MIN, |a, c| a.max(c.high));
            let rest_low = rest.iter().fold(f64::MAX, |a, c| a.min(c.low));
            let rest_range = (rest_high - rest_low) / pole_to.max(f64::EPSILON);
            if rest_range > pole_move.abs() * 0.5 {
                continue;
            }

            let xs: Vec<f64> = (0..rest.len()).map(|i| i as f64).collect();
            let closes: Vec<f64> = rest.iter().map(|c| c.close).collect();
            let Some(fit) = ta::linreg(&xs, &closes) else { continue };
            let drift = fit.slope * rest.len() as f64 / pole_to.max(f64::EPSILON);

            let highs: Vec<f64> = rest.iter().map(|c| c.high).collect();
            let lows: Vec<f64> = rest.iter().map(|c| c.low).collect();
            let (Some(hf), Some(lf)) = (ta::linreg(&xs, &highs), ta::linreg(&xs, &lows)) else { continue };
            let converging = (hf.at(rest.len() as f64 - 1.0) - lf.at(rest.len() as f64 - 1.0))
                < (hf.at(0.0) - lf.at(0.0)) * 0.7;

            let start = pole_start;
            let end = n - 1;
            if pole_move > 0.0 && drift <= 0.0 {
                let kind = if converging { K::BullishPennant } else { K::BullFlag };
                out.push(
                    Detection::new(kind, start, end, 0.55 + grade(pole_move, 0.4, 0.15) * 0.2)
                        .with_detail(format!("{:.0}% pole, {} bar rest", pole_move * 100.0, rest_len)),
                );
                return;
            }
            if pole_move < 0.0 && drift >= 0.0 {
                let kind = if converging { K::BearishPennant } else { K::BearFlag };
                out.push(
                    Detection::new(kind, start, end, 0.55 + grade(-pole_move, 0.4, 0.15) * 0.2)
                        .with_detail(format!("{:.0}% pole, {} bar rest", pole_move * 100.0, rest_len)),
                );
                return;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Rectangles and Darvas boxes
// ---------------------------------------------------------------------------

fn rectangles(candles: &[Candle], out: &mut Vec<Detection>) {
    let n = candles.len();
    for window in [30usize, 45, 60] {
        if n < window + 2 {
            continue;
        }
        let slice = &candles[n - window..];
        let top = slice.iter().fold(f64::MIN, |a, c| a.max(c.high));
        let bottom = slice.iter().fold(f64::MAX, |a, c| a.min(c.low));
        if bottom <= 0.0 {
            continue;
        }
        let height = (top - bottom) / bottom;
        if height > 0.18 {
            continue;
        }

        // Require real touches at both boundaries, not just a quiet drift.
        let band = (top - bottom) * 0.15;
        let top_touches = slice.iter().filter(|c| c.high >= top - band).count();
        let bottom_touches = slice.iter().filter(|c| c.low <= bottom + band).count();
        if top_touches < 2 || bottom_touches < 2 {
            continue;
        }

        let close = last_close(candles);
        let start = n - window;
        if close > top {
            out.push(
                Detection::new(K::DarvasBox, start, n - 1, 0.65 + grade(0.18 - height, 0.18, 0.0) * 0.2)
                    .with_detail(format!("{window}-bar box {bottom:.2}–{top:.2} broken to the upside")),
            );
        } else {
            out.push(
                Detection::new(K::Rectangle, start, n - 1, 0.45 + grade(0.18 - height, 0.18, 0.0) * 0.15)
                    .with_detail(format!("{window}-bar range {bottom:.2}–{top:.2}, {top_touches} top / {bottom_touches} bottom touches")),
            );
        }
        return;
    }
}

// ---------------------------------------------------------------------------
// Rounding bottoms and tops
// ---------------------------------------------------------------------------

/// Saucer shapes, detected without curve fitting: split the window in thirds and
/// require the middle third to be the extreme while the two ends stay level.
fn rounding(candles: &[Candle], out: &mut Vec<Detection>) {
    let n = candles.len();
    for window in [60usize, 90, 120] {
        if n < window + 2 {
            continue;
        }
        let slice = &candles[n - window..];
        let third = window / 3;
        let avg = |part: &[Candle]| part.iter().map(|c| c.close).sum::<f64>() / part.len() as f64;

        let first = avg(&slice[..third]);
        let middle = avg(&slice[third..third * 2]);
        let last = avg(&slice[third * 2..]);
        if first <= 0.0 {
            continue;
        }
        let ends_level = pct_diff(first, last) <= 0.10;
        if !ends_level {
            continue;
        }

        let depth = (first - middle) / first;
        let start = n - window;
        let close = last_close(candles);

        if depth > 0.08 {
            // The extreme must genuinely sit in the middle third.
            let lowest = slice.iter().map(|c| c.low).fold(f64::MAX, f64::min);
            let low_idx = slice.iter().position(|c| c.low == lowest).unwrap_or(0);
            if !(third..third * 2).contains(&low_idx) {
                continue;
            }
            let rim = slice.iter().take(third).fold(f64::MIN, |a, c| a.max(c.high));
            out.push(
                Detection::new(K::RoundingBottom, start, n - 1, 0.5 + grade(depth, 0.30, 0.08) * 0.2 + if close > rim { 0.2 } else { 0.0 })
                    .with_detail(format!("saucer {:.0}% deep over {window} bars, rim {rim:.2}", depth * 100.0)),
            );
            return;
        }
        if depth < -0.08 {
            let highest = slice.iter().map(|c| c.high).fold(f64::MIN, f64::max);
            let high_idx = slice.iter().position(|c| c.high == highest).unwrap_or(0);
            if !(third..third * 2).contains(&high_idx) {
                continue;
            }
            let rim = slice.iter().take(third).fold(f64::MAX, |a, c| a.min(c.low));
            out.push(
                Detection::new(K::RoundingTop, start, n - 1, 0.5 + grade(-depth, 0.30, 0.08) * 0.2 + if close < rim { 0.2 } else { 0.0 })
                    .with_detail(format!("dome {:.0}% tall over {window} bars, rim {rim:.2}", -depth * 100.0)),
            );
            return;
        }
    }
}

// ---------------------------------------------------------------------------
// Volatility contraction
// ---------------------------------------------------------------------------

/// Minervini's VCP: each pullback shallower than the one before it, on fading
/// volume, with price holding near the highs.
fn volatility_contraction(candles: &[Candle], pivots: &[Pivot], out: &mut Vec<Detection>) {
    if pivots.len() < 5 {
        return;
    }
    let tail = &pivots[pivots.len().saturating_sub(6)..];

    // Measure each high→low leg as a percentage pullback.
    let mut pullbacks: Vec<(usize, usize, f64)> = Vec::new();
    for w in tail.windows(2) {
        if w[0].kind == PivotKind::High && w[1].kind == PivotKind::Low && w[0].price > 0.0 {
            pullbacks.push((w[0].idx, w[1].idx, (w[0].price - w[1].price) / w[0].price));
        }
    }
    if pullbacks.len() < 2 {
        return;
    }

    // Each contraction must be meaningfully tighter than the last.
    let contracting = pullbacks
        .windows(2)
        .all(|p| p[1].2 < p[0].2 * 0.8);
    if !contracting {
        return;
    }
    let last_pullback = pullbacks[pullbacks.len() - 1].2;
    if last_pullback > 0.15 {
        return;
    }

    // Price must still be near the top of the structure, not basing after a fall.
    let start = pullbacks[0].0;
    let structure_high = ta::highest_high(candles, start, candles.len() - 1);
    let close = last_close(candles);
    if close < structure_high * 0.88 {
        return;
    }

    let early_vol = ta::avg_volume_before(candles, pullbacks[0].1, 20);
    let late_vol = ta::avg_volume_before(candles, candles.len() - 1, 10);
    let vol_dry = late_vol < early_vol * 0.9;

    out.push(
        Detection::new(K::VolatilityContraction, start, candles.len() - 1,
            0.5 + grade(0.15 - last_pullback, 0.15, 0.0) * 0.2 + if vol_dry { 0.15 } else { 0.0 })
            .with_detail(format!(
                "{} contractions, last {:.1}%{}",
                pullbacks.len(),
                last_pullback * 100.0,
                if vol_dry { ", volume drying up" } else { "" }
            )),
    );
}

// ---------------------------------------------------------------------------
// Base breakouts
// ---------------------------------------------------------------------------

fn base_breakouts(candles: &[Candle], out: &mut Vec<Detection>) {
    let n = candles.len();
    let close = last_close(candles);

    // 52-week (or as much as we hold) high breakout, measured excluding today.
    if n >= 60 {
        let lookback = 250.min(n - 1);
        let prior_high = ta::highest_high(candles, n - 1 - lookback, n - 2);
        if close > prior_high {
            let margin = (close - prior_high) / prior_high.max(f64::EPSILON);
            let vol = candles[n - 1].volume as f64;
            let avg_vol = ta::avg_volume_before(candles, n - 1, 50);
            let vol_surge = avg_vol > 0.0 && vol > avg_vol * 1.5;
            out.push(
                Detection::new(K::FiftyTwoWeekBreakout, n - 1 - lookback, n - 1,
                    0.6 + grade(margin, 0.05, 0.0) * 0.2 + if vol_surge { 0.2 } else { 0.0 })
                    .with_detail(format!(
                        "closed {:.1}% above the {lookback}-bar high of {prior_high:.2}{}",
                        margin * 100.0,
                        if vol_surge { ", on heavy volume" } else { "" }
                    )),
            );
        }
    }

    // Flat base: a tight shelf directly under resistance, then a close through it.
    for window in [25usize, 35, 50] {
        if n < window + 2 {
            continue;
        }
        let base = &candles[n - 1 - window..n - 1];
        let top = base.iter().fold(f64::MIN, |a, c| a.max(c.high));
        let bottom = base.iter().fold(f64::MAX, |a, c| a.min(c.low));
        if bottom <= 0.0 {
            continue;
        }
        let tightness = (top - bottom) / bottom;
        if tightness <= 0.15 && close > top {
            out.push(
                Detection::new(K::FlatBaseBreakout, n - 1 - window, n - 1,
                    0.55 + grade(0.15 - tightness, 0.15, 0.0) * 0.25)
                    .with_detail(format!("{window}-bar base only {:.1}% wide, broke {top:.2}", tightness * 100.0)),
            );
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;

    fn from_path(path: &[f64]) -> Vec<Candle> {
        path.iter()
            .enumerate()
            .map(|(i, &p)| Candle {
                date: NaiveDate::from_ymd_opt(2024, 1, 1).unwrap() + chrono::Duration::days(i as i64),
                open: p,
                high: p * 1.01,
                low: p * 0.99,
                close: p,
                volume: 100_000,
            })
            .collect()
    }

    fn ramp(from: f64, to: f64, steps: usize) -> Vec<f64> {
        (0..steps)
            .map(|i| from + (to - from) * i as f64 / (steps.max(2) - 1) as f64)
            .collect()
    }

    /// Rounded bottom: cosine-shaped so it lingers near the low like a real cup.
    fn cup_path(rim: f64, depth_pct: f64, bars: usize) -> Vec<f64> {
        (0..bars)
            .map(|i| {
                let t = i as f64 / (bars - 1) as f64 * std::f64::consts::PI * 2.0;
                rim - rim * depth_pct * (1.0 - t.cos()) / 2.0
            })
            .collect()
    }

    fn found(dets: &[Detection], kind: K) -> bool {
        dets.iter().any(|d| d.kind == kind)
    }

    #[test]
    fn detects_cup_and_handle() {
        let mut path = ramp(70.0, 100.0, 30); // prior advance to the left rim
        path.extend(cup_path(100.0, 0.28, 80)); // rounded cup back to the rim
        path.extend(ramp(100.0, 92.0, 10)); // handle drifts down
        path.extend(ramp(92.5, 106.0, 6)); // breaks out
        let dets = detect(&from_path(&path), &CupParams::default());
        assert!(found(&dets, K::CupAndHandle), "cup and handle missing: {:?}", kinds(&dets));
    }

    #[test]
    fn v_bottom_is_not_a_cup() {
        let mut path = ramp(70.0, 100.0, 30);
        path.extend(ramp(100.0, 72.0, 12)); // straight down
        path.extend(ramp(72.0, 100.0, 12)); // straight back up
        path.extend(ramp(100.0, 94.0, 8));
        path.extend(ramp(94.0, 104.0, 5));
        let dets = detect(&from_path(&path), &CupParams::default());
        assert!(!found(&dets, K::CupAndHandle), "V bottom wrongly matched: {:?}", kinds(&dets));
    }

    #[test]
    fn detects_double_bottom() {
        let mut path = ramp(120.0, 100.0, 15);
        path.extend(ramp(100.0, 80.0, 15)); // first low
        path.extend(ramp(80.0, 96.0, 15)); // neckline
        path.extend(ramp(96.0, 80.5, 15)); // second low
        path.extend(ramp(80.5, 102.0, 15)); // breaks the neckline
        let dets = detect(&from_path(&path), &CupParams::default());
        assert!(found(&dets, K::DoubleBottom), "double bottom missing: {:?}", kinds(&dets));
    }

    #[test]
    fn detects_head_and_shoulders() {
        let mut path = ramp(80.0, 110.0, 14); // left shoulder
        path.extend(ramp(110.0, 96.0, 10));
        path.extend(ramp(96.0, 128.0, 14)); // head
        path.extend(ramp(128.0, 96.0, 14));
        path.extend(ramp(96.0, 111.0, 12)); // right shoulder
        path.extend(ramp(111.0, 90.0, 12)); // breaks the neckline
        let dets = detect(&from_path(&path), &CupParams::default());
        assert!(found(&dets, K::HeadAndShoulders), "H&S missing: {:?}", kinds(&dets));
    }

    #[test]
    fn detects_52_week_breakout() {
        let mut path = ramp(100.0, 140.0, 200);
        path.extend(ramp(140.0, 120.0, 40));
        path.push(145.0); // new high close
        let dets = detect(&from_path(&path), &CupParams::default());
        assert!(found(&dets, K::FiftyTwoWeekBreakout), "breakout missing: {:?}", kinds(&dets));
    }

    #[test]
    fn flat_series_yields_no_reversal_structures() {
        let path: Vec<f64> = (0..200).map(|i| 100.0 + ((i % 3) as f64) * 0.05).collect();
        let dets = detect(&from_path(&path), &CupParams::default());
        assert!(!found(&dets, K::CupAndHandle));
        assert!(!found(&dets, K::HeadAndShoulders));
        assert!(!found(&dets, K::DoubleTop));
    }

    #[test]
    fn short_series_is_skipped_safely() {
        assert!(detect(&from_path(&ramp(100.0, 110.0, 10)), &CupParams::default()).is_empty());
    }

    #[test]
    fn all_detections_stay_in_bounds() {
        let mut path = ramp(70.0, 100.0, 30);
        path.extend(cup_path(100.0, 0.25, 70));
        path.extend(ramp(100.0, 93.0, 10));
        path.extend(ramp(93.0, 108.0, 8));
        let candles = from_path(&path);
        for d in detect(&candles, &CupParams::default()) {
            assert!(d.end < candles.len(), "{d:?}");
            assert!(d.start <= d.end, "{d:?}");
            assert!((0.0..=1.0).contains(&d.score), "{d:?}");
        }
    }

    fn kinds(dets: &[Detection]) -> Vec<&'static str> {
        dets.iter().map(|d| d.kind.label()).collect()
    }
}
