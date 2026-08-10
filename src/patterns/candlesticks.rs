//! Japanese candlestick recognisers.
//!
//! Every rule is relative to local context, never to absolute rupees: "long
//! body" means long *compared with the last 14 bars of this stock*, so the same
//! thresholds work on a ₹18 scrip and an ₹80,000 one.
//!
//! Reversal patterns additionally require a prior trend — a hammer inside a
//! sideways drift is noise, and reporting it would bury the real signals.

use crate::model::Candle;
use crate::patterns::types::{Detection, PatternKind as K};
use crate::ta::{self, Trend};

/// Bars of local history used to judge "long" and "short" bodies.
const CTX_LOOKBACK: usize = 14;

/// Precomputed per-bar context so each recogniser stays O(1).
pub struct Ctx {
    pub avg_body: Vec<f64>,
    pub avg_range: Vec<f64>,
    pub trend: Vec<Trend>,
}

impl Ctx {
    pub fn build(candles: &[Candle]) -> Self {
        let n = candles.len();
        let mut avg_body = Vec::with_capacity(n);
        let mut avg_range = Vec::with_capacity(n);
        let mut trend = Vec::with_capacity(n);
        for i in 0..n {
            avg_body.push(ta::avg_body_before(candles, i, CTX_LOOKBACK));
            avg_range.push(ta::avg_range_before(candles, i, CTX_LOOKBACK));
            trend.push(ta::trend(candles, i));
        }
        Self { avg_body, avg_range, trend }
    }

    /// Tolerance for "equal" prices, scaled to the stock's own daily range.
    fn tol(&self, i: usize) -> f64 {
        (self.avg_range[i] * 0.08).max(f64::EPSILON)
    }
}

// ---------------------------------------------------------------------------
// Shape predicates
// ---------------------------------------------------------------------------

fn long_body(c: &Candle, avg: f64) -> bool {
    c.body() >= avg * 1.3
}

fn short_body(c: &Candle, avg: f64) -> bool {
    c.body() <= avg * 0.5
}

fn is_doji(c: &Candle) -> bool {
    c.is_doji_like(0.05)
}

fn near(a: f64, b: f64, tol: f64) -> bool {
    (a - b).abs() <= tol
}

/// A true price gap: no overlap of the full ranges.
fn gap_up(prev: &Candle, cur: &Candle) -> bool {
    cur.low > prev.high
}

fn gap_down(prev: &Candle, cur: &Candle) -> bool {
    cur.high < prev.low
}

/// A body gap: shadows may overlap but the real bodies do not.
fn body_gap_up(prev: &Candle, cur: &Candle) -> bool {
    cur.body_bottom() > prev.body_top()
}

fn body_gap_down(prev: &Candle, cur: &Candle) -> bool {
    cur.body_top() < prev.body_bottom()
}

/// Ratio of a value to a reference, clamped — the common scoring building block.
fn ratio_score(value: f64, reference: f64, cap: f64) -> f64 {
    if reference <= 0.0 {
        return 0.0;
    }
    (value / reference).min(cap) / cap
}

fn blend(base: f64, bonus: f64) -> f64 {
    (base + bonus).clamp(0.0, 1.0)
}

fn trend_bonus(actual: Trend, wanted: Trend) -> f64 {
    if actual == wanted { 0.15 } else { 0.0 }
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

/// All candlestick detections across the series.
pub fn detect(candles: &[Candle]) -> Vec<Detection> {
    let ctx = Ctx::build(candles);
    let mut out = Vec::new();
    for i in 0..candles.len() {
        single(candles, i, &ctx, &mut out);
        two(candles, i, &ctx, &mut out);
        three(candles, i, &ctx, &mut out);
        multi(candles, i, &ctx, &mut out);
    }
    out
}

// ---------------------------------------------------------------------------
// One-candle patterns
// ---------------------------------------------------------------------------

fn single(c: &[Candle], i: usize, ctx: &Ctx, out: &mut Vec<Detection>) {
    let cur = &c[i];
    let range = cur.range();
    let avg_body = ctx.avg_body[i];
    let avg_range = ctx.avg_range[i];
    let trend = ctx.trend[i];
    let upper = cur.upper_shadow();
    let lower = cur.lower_shadow();

    // --- doji family: emit the most specific variant only ------------------
    if range <= f64::EPSILON || near(cur.high, cur.low, avg_range * 0.02) {
        out.push(
            Detection::new(K::FourPriceDoji, i, i, 0.6)
                .with_detail("open = high = low = close"),
        );
    } else if is_doji(cur) {
        let up_frac = upper / range;
        let low_frac = lower / range;
        let size = ratio_score(range, avg_range, 2.0);

        if low_frac >= 0.6 && up_frac <= 0.10 {
            out.push(
                Detection::new(K::DragonflyDoji, i, i, blend(0.55, size * 0.2 + trend_bonus(trend, Trend::Down)))
                    .with_detail(format!("lower shadow {:.0}% of range", low_frac * 100.0)),
            );
        } else if up_frac >= 0.6 && low_frac <= 0.10 {
            out.push(
                Detection::new(K::GravestoneDoji, i, i, blend(0.55, size * 0.2 + trend_bonus(trend, Trend::Up)))
                    .with_detail(format!("upper shadow {:.0}% of range", up_frac * 100.0)),
            );
        } else if up_frac >= 0.35 && low_frac >= 0.35 {
            out.push(
                Detection::new(K::LongLeggedDoji, i, i, blend(0.5, size * 0.3))
                    .with_detail("indecision, long shadows both sides"),
            );
        } else {
            out.push(Detection::new(K::Doji, i, i, blend(0.45, size * 0.25)).with_detail("indecision"));
        }
    }

    // --- hammer / hanging man ----------------------------------------------
    let hammer_shape = range > 0.0
        && lower >= cur.body() * 2.0
        && lower >= range * 0.5
        && upper <= range * 0.15
        && !is_doji(cur);
    if hammer_shape {
        let quality = ratio_score(lower, range * 0.5, 2.0);
        match trend {
            Trend::Down => out.push(
                Detection::new(K::Hammer, i, i, blend(0.6, quality * 0.25))
                    .with_detail(format!("lower shadow {:.1}x body", lower / cur.body().max(f64::EPSILON))),
            ),
            Trend::Up => out.push(
                Detection::new(K::HangingMan, i, i, blend(0.55, quality * 0.25))
                    .with_detail("hammer shape after an advance"),
            ),
            Trend::Side => {}
        }
    }

    // --- inverted hammer / shooting star ------------------------------------
    let inverted_shape = range > 0.0
        && upper >= cur.body() * 2.0
        && upper >= range * 0.5
        && lower <= range * 0.15
        && !is_doji(cur);
    if inverted_shape {
        let quality = ratio_score(upper, range * 0.5, 2.0);
        match trend {
            Trend::Down => out.push(
                Detection::new(K::InvertedHammer, i, i, blend(0.55, quality * 0.25))
                    .with_detail("long upper shadow after a decline"),
            ),
            Trend::Up => out.push(
                Detection::new(K::ShootingStar, i, i, blend(0.6, quality * 0.25))
                    .with_detail(format!("upper shadow {:.1}x body", upper / cur.body().max(f64::EPSILON))),
            ),
            Trend::Side => {}
        }
    }

    // --- marubozu -----------------------------------------------------------
    if range > 0.0 && long_body(cur, avg_body) && upper <= range * 0.03 && lower <= range * 0.03 {
        let size = ratio_score(cur.body(), avg_body, 3.0);
        let kind = if cur.is_bull() { K::BullishMarubozu } else { K::BearishMarubozu };
        out.push(
            Detection::new(kind, i, i, blend(0.6, size * 0.3))
                .with_detail("no meaningful shadows, one-sided session"),
        );
    }

    // --- spinning top / high wave -------------------------------------------
    if range > 0.0 && !is_doji(cur) && short_body(cur, avg_body) {
        let body = cur.body().max(f64::EPSILON);
        if upper >= body * 2.0 && lower >= body * 2.0 && range >= avg_range * 1.8 {
            out.push(
                Detection::new(K::HighWave, i, i, blend(0.5, ratio_score(range, avg_range, 3.0) * 0.3))
                    .with_detail("very wide range, tiny body"),
            );
        } else if upper >= body && lower >= body && cur.body() <= range * 0.35 {
            out.push(Detection::new(K::SpinningTop, i, i, 0.45).with_detail("small body, balanced shadows"));
        }
    }

    // --- belt hold ----------------------------------------------------------
    if range > 0.0 && long_body(cur, avg_body) {
        let size = ratio_score(cur.body(), avg_body, 3.0);
        if cur.is_bull() && lower <= range * 0.05 && trend == Trend::Down {
            out.push(
                Detection::new(K::BullishBeltHold, i, i, blend(0.5, size * 0.25))
                    .with_detail("opened on its low and closed strong"),
            );
        }
        if cur.is_bear() && upper <= range * 0.05 && trend == Trend::Up {
            out.push(
                Detection::new(K::BearishBeltHold, i, i, blend(0.5, size * 0.25))
                    .with_detail("opened on its high and closed weak"),
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Two-candle patterns
// ---------------------------------------------------------------------------

fn two(c: &[Candle], i: usize, ctx: &Ctx, out: &mut Vec<Detection>) {
    if i < 1 {
        return;
    }
    let (p, cur) = (&c[i - 1], &c[i]);
    let avg_body = ctx.avg_body[i];
    let trend = ctx.trend[i - 1];
    let tol = ctx.tol(i);

    // --- engulfing ----------------------------------------------------------
    let engulfs = cur.body_bottom() <= p.body_bottom()
        && cur.body_top() >= p.body_top()
        && cur.body() > p.body()
        && p.body() > f64::EPSILON;
    if engulfs {
        let size = ratio_score(cur.body(), p.body(), 3.0);
        if p.is_bear() && cur.is_bull() {
            out.push(
                Detection::new(K::BullishEngulfing, i - 1, i, blend(0.6, size * 0.2 + trend_bonus(trend, Trend::Down)))
                    .with_detail("up candle swallows the prior down body"),
            );
        } else if p.is_bull() && cur.is_bear() {
            out.push(
                Detection::new(K::BearishEngulfing, i - 1, i, blend(0.6, size * 0.2 + trend_bonus(trend, Trend::Up)))
                    .with_detail("down candle swallows the prior up body"),
            );
        }
    }

    // --- harami (and harami cross) ------------------------------------------
    let inside = cur.body_top() <= p.body_top()
        && cur.body_bottom() >= p.body_bottom()
        && p.body() > cur.body()
        && long_body(p, avg_body);
    if inside {
        let containment = 1.0 - (cur.body() / p.body().max(f64::EPSILON));
        if p.is_bear() && (cur.is_bull() || is_doji(cur)) {
            let kind = if is_doji(cur) { K::BullishHaramiCross } else { K::BullishHarami };
            out.push(
                Detection::new(kind, i - 1, i, blend(0.55, containment * 0.2 + trend_bonus(trend, Trend::Down)))
                    .with_detail("selling pressure contracted inside the prior body"),
            );
        } else if p.is_bull() && (cur.is_bear() || is_doji(cur)) {
            let kind = if is_doji(cur) { K::BearishHaramiCross } else { K::BearishHarami };
            out.push(
                Detection::new(kind, i - 1, i, blend(0.55, containment * 0.2 + trend_bonus(trend, Trend::Up)))
                    .with_detail("buying pressure contracted inside the prior body"),
            );
        }
        // Same-colour containment: continuation-flavoured cousins of the harami.
        if p.is_bear() && cur.is_bear() && trend == Trend::Down {
            out.push(
                Detection::new(K::HomingPigeon, i - 1, i, 0.5)
                    .with_detail("second down candle contained by the first"),
            );
        }
        if p.is_bull() && cur.is_bull() && trend == Trend::Up {
            out.push(
                Detection::new(K::DescendingHawk, i - 1, i, 0.5)
                    .with_detail("second up candle contained by the first"),
            );
        }
    }

    // --- piercing line / dark cloud cover ------------------------------------
    if long_body(p, avg_body) {
        if p.is_bear() && cur.is_bull() && cur.open < p.low && cur.close > p.body_mid() && cur.close < p.open {
            let penetration = (cur.close - p.close) / p.body().max(f64::EPSILON);
            out.push(
                Detection::new(K::PiercingLine, i - 1, i, blend(0.6, penetration * 0.2 + trend_bonus(trend, Trend::Down)))
                    .with_detail(format!("recovered {:.0}% of the prior body", penetration * 100.0)),
            );
        }
        if p.is_bull() && cur.is_bear() && cur.open > p.high && cur.close < p.body_mid() && cur.close > p.open {
            let penetration = (p.close - cur.close) / p.body().max(f64::EPSILON);
            out.push(
                Detection::new(K::DarkCloudCover, i - 1, i, blend(0.6, penetration * 0.2 + trend_bonus(trend, Trend::Up)))
                    .with_detail(format!("gave back {:.0}% of the prior body", penetration * 100.0)),
            );
        }
        // On-neck / in-neck / thrusting: failed rebounds after a long down candle.
        if p.is_bear() && cur.is_bull() && cur.open < p.low {
            if near(cur.close, p.low, tol) {
                out.push(
                    Detection::new(K::OnNeck, i - 1, i, blend(0.45, trend_bonus(trend, Trend::Down)))
                        .with_detail("rebound stalled exactly at the prior low"),
                );
            } else if cur.close > p.close && cur.close <= p.close + p.body() * 0.25 {
                out.push(
                    Detection::new(K::InNeck, i - 1, i, blend(0.45, trend_bonus(trend, Trend::Down)))
                        .with_detail("rebound barely entered the prior body"),
                );
            } else if cur.close > p.close && cur.close < p.body_mid() {
                out.push(
                    Detection::new(K::Thrusting, i - 1, i, blend(0.45, trend_bonus(trend, Trend::Down)))
                        .with_detail("rebound fell short of the prior body midpoint"),
                );
            }
        }
    }

    // --- tweezers -----------------------------------------------------------
    if near(cur.low, p.low, tol) && trend == Trend::Down && cur.is_bull() {
        out.push(
            Detection::new(K::TweezerBottom, i - 1, i, 0.5)
                .with_detail("two sessions rejected from the same low"),
        );
    }
    if near(cur.high, p.high, tol) && trend == Trend::Up && cur.is_bear() {
        out.push(
            Detection::new(K::TweezerTop, i - 1, i, 0.5)
                .with_detail("two sessions rejected from the same high"),
        );
    }

    // --- kicker -------------------------------------------------------------
    if p.is_bear() && cur.is_bull() && body_gap_up(p, cur) && long_body(cur, avg_body) {
        out.push(
            Detection::new(K::BullishKicker, i - 1, i, blend(0.7, ratio_score(cur.body(), avg_body, 3.0) * 0.2))
                .with_detail("gapped straight through the prior down body"),
        );
    }
    if p.is_bull() && cur.is_bear() && body_gap_down(p, cur) && long_body(cur, avg_body) {
        out.push(
            Detection::new(K::BearishKicker, i - 1, i, blend(0.7, ratio_score(cur.body(), avg_body, 3.0) * 0.2))
                .with_detail("gapped straight through the prior up body"),
        );
    }

    // --- separating lines (continuation) -------------------------------------
    if near(cur.open, p.open, tol) {
        if trend == Trend::Up && p.is_bear() && cur.is_bull() && long_body(cur, avg_body) {
            out.push(
                Detection::new(K::BullishSeparatingLines, i - 1, i, 0.5)
                    .with_detail("uptrend resumed from the same open"),
            );
        }
        if trend == Trend::Down && p.is_bull() && cur.is_bear() && long_body(cur, avg_body) {
            out.push(
                Detection::new(K::BearishSeparatingLines, i - 1, i, 0.5)
                    .with_detail("downtrend resumed from the same open"),
            );
        }
    }

    // --- matching low / high --------------------------------------------------
    if near(cur.close, p.close, tol) {
        if p.is_bear() && cur.is_bear() && trend == Trend::Down {
            out.push(
                Detection::new(K::MatchingLow, i - 1, i, 0.45)
                    .with_detail("two down sessions closing on the same level"),
            );
        }
        if p.is_bull() && cur.is_bull() && trend == Trend::Up {
            out.push(
                Detection::new(K::MatchingHigh, i - 1, i, 0.45)
                    .with_detail("two up sessions closing on the same level"),
            );
        }
    }

    // --- counterattack --------------------------------------------------------
    if long_body(p, avg_body) && long_body(cur, avg_body) && near(cur.close, p.close, tol) {
        if p.is_bear() && cur.is_bull() && cur.open < p.low && trend == Trend::Down {
            out.push(
                Detection::new(K::BullishCounterattack, i - 1, i, 0.5)
                    .with_detail("gapped down then closed back at the prior close"),
            );
        }
        if p.is_bull() && cur.is_bear() && cur.open > p.high && trend == Trend::Up {
            out.push(
                Detection::new(K::BearishCounterattack, i - 1, i, 0.5)
                    .with_detail("gapped up then closed back at the prior close"),
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Three-candle patterns
// ---------------------------------------------------------------------------

fn three(c: &[Candle], i: usize, ctx: &Ctx, out: &mut Vec<Detection>) {
    if i < 2 {
        return;
    }
    let (a, b, cc) = (&c[i - 2], &c[i - 1], &c[i]);
    let avg_body = ctx.avg_body[i];
    let trend = ctx.trend[i - 2];
    let tol = ctx.tol(i);

    // --- morning / evening star ----------------------------------------------
    let star_small = short_body(b, avg_body);
    if long_body(a, avg_body) && star_small {
        // Morning star: long down, small gap-down star, strong recovery.
        if a.is_bear() && cc.is_bull() && body_gap_down(a, b) && cc.close > a.body_mid() {
            let recovery = (cc.close - a.close) / a.body().max(f64::EPSILON);
            let kind = if is_doji(b) { K::MorningDojiStar } else { K::MorningStar };
            let mut det = Detection::new(kind, i - 2, i, blend(0.65, recovery * 0.2 + trend_bonus(trend, Trend::Down)))
                .with_detail(format!("closed back {:.0}% into the down body", recovery * 100.0));
            // A star fully detached on both sides is the abandoned baby.
            if is_doji(b) && gap_down(a, b) && gap_up(b, cc) {
                det = Detection::new(K::BullishAbandonedBaby, i - 2, i, 0.85)
                    .with_detail("doji island low, gapped on both sides");
            }
            out.push(det);
        }
        // Evening star: mirror.
        if a.is_bull() && cc.is_bear() && body_gap_up(a, b) && cc.close < a.body_mid() {
            let giveback = (a.close - cc.close) / a.body().max(f64::EPSILON);
            let kind = if is_doji(b) { K::EveningDojiStar } else { K::EveningStar };
            let mut det = Detection::new(kind, i - 2, i, blend(0.65, giveback * 0.2 + trend_bonus(trend, Trend::Up)))
                .with_detail(format!("closed back {:.0}% into the up body", giveback * 100.0));
            if is_doji(b) && gap_up(a, b) && gap_down(b, cc) {
                det = Detection::new(K::BearishAbandonedBaby, i - 2, i, 0.85)
                    .with_detail("doji island high, gapped on both sides");
            }
            out.push(det);
        }
    }

    // --- tri-star --------------------------------------------------------------
    if is_doji(a) && is_doji(b) && is_doji(cc) {
        if b.low < a.low && b.low < cc.low && trend == Trend::Down {
            out.push(Detection::new(K::BullishTriStar, i - 2, i, 0.6).with_detail("three dojis, middle one lowest"));
        }
        if b.high > a.high && b.high > cc.high && trend == Trend::Up {
            out.push(Detection::new(K::BearishTriStar, i - 2, i, 0.6).with_detail("three dojis, middle one highest"));
        }
    }

    // --- three soldiers / crows -------------------------------------------------
    let three_bull = a.is_bull() && b.is_bull() && cc.is_bull();
    let three_bear = a.is_bear() && b.is_bear() && cc.is_bear();
    let rising = b.close > a.close && cc.close > b.close;
    let falling = b.close < a.close && cc.close < b.close;

    if three_bull
        && rising
        && [a, b, cc].iter().all(|x| long_body(x, avg_body))
        && b.open > a.body_bottom() && b.open < a.body_top()
        && cc.open > b.body_bottom() && cc.open < b.body_top()
        && [a, b, cc].iter().all(|x| x.upper_shadow() <= x.body() * 0.4)
    {
        out.push(
            Detection::new(K::ThreeWhiteSoldiers, i - 2, i, blend(0.75, trend_bonus(trend, Trend::Down)))
                .with_detail("three strong up closes, each opening inside the last body"),
        );
    }

    if three_bear
        && falling
        && [a, b, cc].iter().all(|x| long_body(x, avg_body))
        && b.open < a.body_top() && b.open > a.body_bottom()
        && cc.open < b.body_top() && cc.open > b.body_bottom()
        && [a, b, cc].iter().all(|x| x.lower_shadow() <= x.body() * 0.4)
    {
        let identical = near(b.open, a.close, tol) && near(cc.open, b.close, tol);
        let kind = if identical { K::IdenticalThreeCrows } else { K::ThreeBlackCrows };
        out.push(
            Detection::new(kind, i - 2, i, blend(0.75, trend_bonus(trend, Trend::Up)))
                .with_detail("three heavy down closes in a row"),
        );
    }

    // --- three inside / outside --------------------------------------------------
    let harami_bull = a.is_bear() && b.body_top() <= a.body_top() && b.body_bottom() >= a.body_bottom() && long_body(a, avg_body);
    let harami_bear = a.is_bull() && b.body_top() <= a.body_top() && b.body_bottom() >= a.body_bottom() && long_body(a, avg_body);
    if harami_bull && cc.is_bull() && cc.close > a.open {
        out.push(
            Detection::new(K::ThreeInsideUp, i - 2, i, blend(0.65, trend_bonus(trend, Trend::Down)))
                .with_detail("harami confirmed by a close above the down body"),
        );
    }
    if harami_bear && cc.is_bear() && cc.close < a.open {
        out.push(
            Detection::new(K::ThreeInsideDown, i - 2, i, blend(0.65, trend_bonus(trend, Trend::Up)))
                .with_detail("harami confirmed by a close below the up body"),
        );
    }

    let engulf_bull = a.is_bear() && b.is_bull() && b.body_bottom() <= a.body_bottom() && b.body_top() >= a.body_top();
    let engulf_bear = a.is_bull() && b.is_bear() && b.body_bottom() <= a.body_bottom() && b.body_top() >= a.body_top();
    if engulf_bull && cc.is_bull() && cc.close > b.close {
        out.push(
            Detection::new(K::ThreeOutsideUp, i - 2, i, blend(0.7, trend_bonus(trend, Trend::Down)))
                .with_detail("engulfing followed through to a higher close"),
        );
    }
    if engulf_bear && cc.is_bear() && cc.close < b.close {
        out.push(
            Detection::new(K::ThreeOutsideDown, i - 2, i, blend(0.7, trend_bonus(trend, Trend::Up)))
                .with_detail("engulfing followed through to a lower close"),
        );
    }

    // --- stick sandwich ----------------------------------------------------------
    if a.is_bear() && b.is_bull() && cc.is_bear() && near(a.close, cc.close, tol) && b.close > a.close {
        out.push(
            Detection::new(K::StickSandwich, i - 2, i, blend(0.5, trend_bonus(trend, Trend::Down)))
                .with_detail("two identical closes sandwiching an up day"),
        );
    }

    // --- unique three river bottom -------------------------------------------------
    if trend == Trend::Down
        && a.is_bear() && long_body(a, avg_body)
        && b.is_bear() && b.low < a.low && b.close > a.close && b.lower_shadow() > b.body()
        && cc.is_bull() && short_body(cc, avg_body) && cc.close < b.close
    {
        out.push(
            Detection::new(K::UniqueThreeRiverBottom, i - 2, i, 0.6)
                .with_detail("new low rejected, then a quiet up day"),
        );
    }

    // --- three stars in the south ---------------------------------------------------
    if trend == Trend::Down
        && a.is_bear() && long_body(a, avg_body) && a.lower_shadow() > a.body() * 0.5
        && b.is_bear() && b.low > a.low && b.body() < a.body() && b.lower_shadow() > 0.0
        && cc.is_bear() && cc.body() < b.body() && cc.low >= b.low && cc.high <= b.high
        && cc.lower_shadow() <= cc.range() * 0.05 && cc.upper_shadow() <= cc.range() * 0.05
    {
        out.push(
            Detection::new(K::ThreeStarsInTheSouth, i - 2, i, 0.6)
                .with_detail("selling shrinking three sessions running"),
        );
    }

    // --- advance block / deliberation --------------------------------------------------
    if trend == Trend::Up && three_bull && rising {
        let shrinking = b.body() < a.body() && cc.body() < b.body();
        let long_wicks = b.upper_shadow() > b.body() * 0.5 && cc.upper_shadow() > cc.body() * 0.5;
        if shrinking && long_wicks {
            out.push(
                Detection::new(K::AdvanceBlock, i - 2, i, 0.55)
                    .with_detail("rally losing thrust, upper shadows growing"),
            );
        } else if long_body(a, avg_body) && long_body(b, avg_body) && short_body(cc, avg_body) {
            out.push(
                Detection::new(K::Deliberation, i - 2, i, 0.55)
                    .with_detail("two strong pushes then a stall"),
            );
        }
    }

    // --- two crows / upside gap two crows --------------------------------------------
    if trend == Trend::Up && a.is_bull() && long_body(a, avg_body) && b.is_bear() && cc.is_bear() {
        if body_gap_up(a, b) && cc.open > b.body_bottom() && cc.open <= b.body_top() && cc.close < a.close && cc.close > a.open {
            out.push(
                Detection::new(K::TwoCrows, i - 2, i, 0.6)
                    .with_detail("gap up sold off over two sessions"),
            );
        }
        if body_gap_up(a, b)
            && cc.body_top() >= b.body_top() && cc.body_bottom() <= b.body_bottom()
            && cc.close > a.close
        {
            out.push(
                Detection::new(K::UpsideGapTwoCrows, i - 2, i, 0.6)
                    .with_detail("gap still unfilled but sellers took control"),
            );
        }
    }

    // --- tasuki gaps and side-by-side white lines ---------------------------------------
    if a.is_bull() && b.is_bull() && body_gap_up(a, b) {
        if cc.is_bear() && cc.open > b.body_bottom() && cc.open < b.body_top() && cc.close > a.close {
            out.push(
                Detection::new(K::BullishTasukiGap, i - 2, i, blend(0.55, trend_bonus(trend, Trend::Up)))
                    .with_detail("pullback stopped before filling the gap"),
            );
        }
        if cc.is_bull() && near(cc.open, b.open, tol) && near(cc.close, b.close, tol) {
            out.push(
                Detection::new(K::SideBySideWhiteLines, i - 2, i, blend(0.5, trend_bonus(trend, Trend::Up)))
                    .with_detail("twin up candles above the gap"),
            );
        }
    }
    if a.is_bear() && b.is_bear() && body_gap_down(a, b)
        && cc.is_bull() && cc.open < b.body_top() && cc.open > b.body_bottom() && cc.close < a.close
    {
        out.push(
            Detection::new(K::BearishTasukiGap, i - 2, i, blend(0.55, trend_bonus(trend, Trend::Down)))
                .with_detail("bounce stopped before filling the gap"),
        );
    }
}

// ---------------------------------------------------------------------------
// Four- and five-candle patterns
// ---------------------------------------------------------------------------

fn multi(c: &[Candle], i: usize, ctx: &Ctx, out: &mut Vec<Detection>) {
    three_line_strike(c, i, ctx, out);
    rising_falling_three(c, i, ctx, out);
    concealing_baby_swallow(c, i, ctx, out);
    ladder_bottom(c, i, ctx, out);
    breakaway(c, i, ctx, out);
    hikkake(c, i, ctx, out);
}

/// Three same-colour candles then one that engulfs the whole run.
fn three_line_strike(c: &[Candle], i: usize, ctx: &Ctx, out: &mut Vec<Detection>) {
    if i < 3 {
        return;
    }
    let (a, b, cc, d) = (&c[i - 3], &c[i - 2], &c[i - 1], &c[i]);
    let avg_body = ctx.avg_body[i];

    if a.is_bull() && b.is_bull() && cc.is_bull()
        && b.close > a.close && cc.close > b.close
        && d.is_bear() && d.open > cc.close && d.close < a.open
    {
        out.push(
            Detection::new(K::BullishThreeLineStrike, i - 3, i, 0.6)
                .with_detail("one bar erased three up days but the trend held"),
        );
    }
    if a.is_bear() && b.is_bear() && cc.is_bear()
        && b.close < a.close && cc.close < b.close
        && d.is_bull() && d.open < cc.close && d.close > a.open
    {
        out.push(
            Detection::new(K::BearishThreeLineStrike, i - 3, i, 0.6)
                .with_detail("one bar erased three down days but the trend held"),
        );
    }
    let _ = avg_body;
}

/// Long candle, a short counter-trend rest inside its range, then continuation.
fn rising_falling_three(c: &[Candle], i: usize, ctx: &Ctx, out: &mut Vec<Detection>) {
    if i < 4 {
        return;
    }
    let a = &c[i - 4];
    let rest = &c[i - 3..i];
    let last = &c[i];
    let avg_body = ctx.avg_body[i];
    let trend = ctx.trend[i - 4];

    let rest_inside_up = rest.iter().all(|r| r.high <= a.high && r.low >= a.low);
    if a.is_bull() && long_body(a, avg_body)
        && rest.iter().all(|r| short_body(r, avg_body))
        && rest_inside_up
        && last.is_bull() && long_body(last, avg_body) && last.close > a.close
    {
        // Mat hold differs only by gapping away and resting in the upper half.
        let gapped = rest[0].open > a.close;
        let shallow = rest.iter().all(|r| r.low > a.body_mid());
        if gapped && shallow {
            out.push(
                Detection::new(K::MatHold, i - 4, i, blend(0.7, trend_bonus(trend, Trend::Up)))
                    .with_detail("shallow rest above the midpoint, then a new high close"),
            );
        } else {
            out.push(
                Detection::new(K::RisingThreeMethods, i - 4, i, blend(0.65, trend_bonus(trend, Trend::Up)))
                    .with_detail("three quiet days inside the range, then continuation"),
            );
        }
    }

    if a.is_bear() && long_body(a, avg_body)
        && rest.iter().all(|r| short_body(r, avg_body))
        && rest.iter().all(|r| r.high <= a.high && r.low >= a.low)
        && last.is_bear() && long_body(last, avg_body) && last.close < a.close
    {
        out.push(
            Detection::new(K::FallingThreeMethods, i - 4, i, blend(0.65, trend_bonus(trend, Trend::Down)))
                .with_detail("three quiet days inside the range, then a lower close"),
        );
    }
}

fn concealing_baby_swallow(c: &[Candle], i: usize, ctx: &Ctx, out: &mut Vec<Detection>) {
    if i < 3 {
        return;
    }
    let (a, b, cc, d) = (&c[i - 3], &c[i - 2], &c[i - 1], &c[i]);
    if ctx.trend[i - 3] != Trend::Down {
        return;
    }
    let marubozu = |x: &Candle| x.is_bear() && x.upper_shadow() <= x.range() * 0.05 && x.lower_shadow() <= x.range() * 0.05;

    if marubozu(a) && marubozu(b)
        && cc.is_bear() && cc.open < b.close && cc.upper_shadow() > 0.0 && cc.high > b.close
        && d.is_bear() && d.high >= cc.high && d.low <= cc.low
    {
        out.push(
            Detection::new(K::ConcealingBabySwallow, i - 3, i, 0.65)
                .with_detail("capitulation shape: last bar swallows the third"),
        );
    }
}

fn ladder_bottom(c: &[Candle], i: usize, ctx: &Ctx, out: &mut Vec<Detection>) {
    if i < 4 {
        return;
    }
    let (a, b, cc, d, e) = (&c[i - 4], &c[i - 3], &c[i - 2], &c[i - 1], &c[i]);
    if ctx.trend[i - 4] != Trend::Down {
        return;
    }
    if a.is_bear() && b.is_bear() && cc.is_bear()
        && b.close < a.close && cc.close < b.close
        && b.open < a.open && cc.open < b.open
        && d.is_bear() && d.upper_shadow() > d.body()
        && e.is_bull() && e.open > d.body_top()
    {
        out.push(
            Detection::new(K::LadderBottom, i - 4, i, 0.6)
                .with_detail("stair-step decline ended with a gap-up reversal"),
        );
    }
}

fn breakaway(c: &[Candle], i: usize, ctx: &Ctx, out: &mut Vec<Detection>) {
    if i < 4 {
        return;
    }
    let (a, b, cc, d, e) = (&c[i - 4], &c[i - 3], &c[i - 2], &c[i - 1], &c[i]);
    let avg_body = ctx.avg_body[i];

    // Bullish: accelerating decline with an unfilled gap, then one bar closes back into it.
    if a.is_bear() && long_body(a, avg_body)
        && body_gap_down(a, b)
        && cc.close < b.close && d.close < cc.close
        && e.is_bull() && long_body(e, avg_body)
        && e.close > b.body_top() && e.close < a.body_bottom()
    {
        out.push(
            Detection::new(K::BullishBreakaway, i - 4, i, 0.6)
                .with_detail("closed back inside the breakaway gap"),
        );
    }
    if a.is_bull() && long_body(a, avg_body)
        && body_gap_up(a, b)
        && cc.close > b.close && d.close > cc.close
        && e.is_bear() && long_body(e, avg_body)
        && e.close < b.body_bottom() && e.close > a.body_top()
    {
        out.push(
            Detection::new(K::BearishBreakaway, i - 4, i, 0.6)
                .with_detail("closed back inside the breakaway gap"),
        );
    }
}

/// Inside bar, a false break out of it, then a snap back through the other side.
fn hikkake(c: &[Candle], i: usize, ctx: &Ctx, out: &mut Vec<Detection>) {
    // Needs the inside pair, the fake-out bar, and at least one resolution bar.
    if i < 3 {
        return;
    }
    let tol = ctx.tol(i);
    let _ = tol;

    // Look back for an inside bar whose fake-out resolves at `i`.
    for back in 3..=5usize {
        if i < back {
            break;
        }
        let mother = &c[i - back];
        let inside = &c[i - back + 1];
        let fake = &c[i - back + 2];
        let cur = &c[i];

        let is_inside = inside.high <= mother.high && inside.low >= mother.low;
        if !is_inside {
            continue;
        }

        // Bullish: broke below the inside bar, then reclaimed its high.
        if fake.low < inside.low && cur.close > inside.high {
            out.push(
                Detection::new(K::BullishHikkake, i - back, i, 0.6)
                    .with_detail("false break below the inside bar, then reclaimed"),
            );
            return;
        }
        // Bearish: broke above, then lost the inside bar's low.
        if fake.high > inside.high && cur.close < inside.low {
            out.push(
                Detection::new(K::BearishHikkake, i - back, i, 0.6)
                    .with_detail("false break above the inside bar, then rejected"),
            );
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;

    fn c(o: f64, h: f64, l: f64, cl: f64) -> (f64, f64, f64, f64) {
        (o, h, l, cl)
    }

    fn build(rows: &[(f64, f64, f64, f64)]) -> Vec<Candle> {
        rows.iter()
            .enumerate()
            .map(|(i, &(o, h, l, cl))| Candle {
                date: NaiveDate::from_ymd_opt(2025, 1, 1).unwrap() + chrono::Duration::days(i as i64),
                open: o,
                high: h,
                low: l,
                close: cl,
                volume: 10_000,
            })
            .collect()
    }

    /// 18 sessions of steady decline, so reversal rules see a real downtrend.
    fn downtrend() -> Vec<(f64, f64, f64, f64)> {
        (0..18)
            .map(|i| {
                let base = 200.0 - i as f64 * 4.0;
                c(base, base + 1.5, base - 5.0, base - 3.5)
            })
            .collect()
    }

    fn uptrend() -> Vec<(f64, f64, f64, f64)> {
        (0..18)
            .map(|i| {
                let base = 100.0 + i as f64 * 4.0;
                c(base, base + 5.0, base - 1.5, base + 3.5)
            })
            .collect()
    }

    fn found(dets: &[Detection], kind: K) -> bool {
        dets.iter().any(|d| d.kind == kind)
    }

    #[test]
    fn detects_hammer_after_a_decline() {
        let mut rows = downtrend();
        // Small body at the top, long lower shadow. The body must clear the doji
        // threshold (5% of range) or this is a dragonfly doji, not a hammer.
        rows.push(c(130.0, 131.0, 118.0, 130.9));
        let dets = detect(&build(&rows));
        assert!(found(&dets, K::Hammer), "hammer missing: {dets:?}");
        assert!(!found(&dets, K::HangingMan), "hanging man must need an uptrend");
    }

    #[test]
    fn same_shape_after_a_rally_is_a_hanging_man() {
        let mut rows = uptrend();
        rows.push(c(180.0, 181.0, 168.0, 180.9));
        let dets = detect(&build(&rows));
        assert!(found(&dets, K::HangingMan), "hanging man missing: {dets:?}");
        assert!(!found(&dets, K::Hammer));
    }

    #[test]
    fn detects_bullish_engulfing() {
        let mut rows = downtrend();
        rows.push(c(135.0, 136.0, 130.0, 131.0)); // small down bar
        rows.push(c(129.0, 140.0, 128.0, 139.0)); // engulfs it
        let dets = detect(&build(&rows));
        assert!(found(&dets, K::BullishEngulfing), "engulfing missing: {dets:?}");
    }

    #[test]
    fn detects_morning_star() {
        let mut rows = downtrend();
        rows.push(c(135.0, 136.0, 120.0, 121.0)); // long down body
        rows.push(c(118.0, 119.5, 117.0, 118.5)); // small star, gapped down
        rows.push(c(120.0, 133.0, 119.5, 132.0)); // strong recovery
        let dets = detect(&build(&rows));
        assert!(
            found(&dets, K::MorningStar) || found(&dets, K::MorningDojiStar),
            "morning star missing: {dets:?}"
        );
    }

    #[test]
    fn detects_three_white_soldiers() {
        let mut rows = downtrend();
        rows.push(c(130.0, 142.0, 129.5, 141.0));
        rows.push(c(136.0, 153.0, 135.5, 152.0));
        rows.push(c(147.0, 164.0, 146.5, 163.0));
        let dets = detect(&build(&rows));
        assert!(found(&dets, K::ThreeWhiteSoldiers), "soldiers missing: {dets:?}");
    }

    #[test]
    fn detects_doji_variants_distinctly() {
        let mut rows = downtrend();
        rows.push(c(130.0, 130.4, 120.0, 130.1)); // dragonfly
        let dets = detect(&build(&rows));
        assert!(found(&dets, K::DragonflyDoji), "dragonfly missing: {dets:?}");
        // The generic doji must not double-report the same bar.
        let last = dets.iter().filter(|d| d.end == rows.len() - 1).collect::<Vec<_>>();
        assert_eq!(last.iter().filter(|d| d.kind == K::Doji).count(), 0);
    }

    #[test]
    fn detects_bullish_kicker() {
        let mut rows = downtrend();
        rows.push(c(135.0, 136.0, 128.0, 129.0)); // down body
        rows.push(c(140.0, 152.0, 139.0, 151.0)); // gaps clean above it
        let dets = detect(&build(&rows));
        assert!(found(&dets, K::BullishKicker), "kicker missing: {dets:?}");
    }

    #[test]
    fn detects_rising_three_methods() {
        let mut rows = uptrend();
        rows.push(c(170.0, 190.0, 169.0, 189.0)); // long up bar
        // The resting bars must be *short* relative to the local average body,
        // and must stay inside the long bar's range.
        rows.push(c(186.0, 187.0, 183.5, 185.0));
        rows.push(c(185.0, 186.0, 182.0, 183.5));
        rows.push(c(183.0, 184.5, 181.0, 182.0));
        rows.push(c(181.5, 200.0, 180.5, 198.0)); // breaks out
        let dets = detect(&build(&rows));
        assert!(found(&dets, K::RisingThreeMethods) || found(&dets, K::MatHold), "three methods missing: {dets:?}");
    }

    #[test]
    fn detects_bullish_hikkake() {
        let mut rows = downtrend();
        rows.push(c(130.0, 140.0, 120.0, 125.0)); // mother bar
        rows.push(c(128.0, 136.0, 124.0, 130.0)); // inside bar
        rows.push(c(129.0, 133.0, 121.0, 123.0)); // false break low
        rows.push(c(124.0, 139.0, 123.0, 138.0)); // reclaims inside high
        let dets = detect(&build(&rows));
        assert!(found(&dets, K::BullishHikkake), "hikkake missing: {dets:?}");
    }

    #[test]
    fn quiet_series_produces_no_reversal_signals() {
        let rows: Vec<_> = (0..30).map(|_| c(100.0, 100.6, 99.4, 100.05)).collect();
        let dets = detect(&build(&rows));
        assert!(!found(&dets, K::Hammer));
        assert!(!found(&dets, K::MorningStar));
        assert!(!found(&dets, K::ThreeWhiteSoldiers));
    }

    #[test]
    fn detection_indices_stay_in_bounds() {
        let mut rows = downtrend();
        rows.extend(uptrend());
        let candles = build(&rows);
        for d in detect(&candles) {
            assert!(d.end < candles.len(), "{d:?}");
            assert!(d.start <= d.end, "{d:?}");
            assert!((0.0..=1.0).contains(&d.score), "{d:?}");
        }
    }
}
