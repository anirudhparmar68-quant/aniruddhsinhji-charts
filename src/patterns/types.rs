//! The pattern catalogue.
//!
//! Every recogniser in this crate emits a [`Detection`]. The catalogue is
//! declared once through the `patterns!` macro so the enum, its display labels,
//! its family and its round-trip string key can never drift apart.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Direction {
    Bullish,
    Bearish,
    Neutral,
}

impl Direction {
    pub fn as_str(self) -> &'static str {
        match self {
            Direction::Bullish => "Bullish",
            Direction::Bearish => "Bearish",
            Direction::Neutral => "Neutral",
        }
    }

    pub fn parse(s: &str) -> Self {
        match s {
            "Bullish" => Direction::Bullish,
            "Bearish" => Direction::Bearish,
            _ => Direction::Neutral,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Family {
    /// One to five bar Japanese candlestick formations.
    Candlestick,
    /// Multi-week price structures: bases, reversals, continuations.
    Chart,
}

impl Family {
    pub fn as_str(self) -> &'static str {
        match self {
            Family::Candlestick => "Candlestick",
            Family::Chart => "Chart",
        }
    }
}

use Direction::{Bearish, Bullish, Neutral};
use Family::{Candlestick, Chart};

macro_rules! patterns {
    ($( $variant:ident => ($label:expr, $family:expr, $dir:expr) ),* $(,)?) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
        pub enum PatternKind { $($variant),* }

        impl PatternKind {
            /// Every pattern the engine knows about, in catalogue order.
            pub const ALL: &'static [PatternKind] = &[ $(PatternKind::$variant),* ];

            /// Human-readable name shown in the UI.
            pub fn label(self) -> &'static str {
                match self { $(Self::$variant => $label),* }
            }

            pub fn family(self) -> Family {
                match self { $(Self::$variant => $family),* }
            }

            /// The bias the pattern carries by definition.
            pub fn bias(self) -> Direction {
                match self { $(Self::$variant => $dir),* }
            }

            /// Stable identifier used for storage and settings.
            pub fn key(self) -> &'static str {
                match self { $(Self::$variant => stringify!($variant)),* }
            }

            pub fn from_key(s: &str) -> Option<Self> {
                match s { $(stringify!($variant) => Some(Self::$variant),)* _ => None }
            }
        }
    };
}

patterns! {
    // ---- single candle -----------------------------------------------------
    Doji                    => ("Doji", Candlestick, Neutral),
    LongLeggedDoji          => ("Long-Legged Doji", Candlestick, Neutral),
    DragonflyDoji           => ("Dragonfly Doji", Candlestick, Bullish),
    GravestoneDoji          => ("Gravestone Doji", Candlestick, Bearish),
    FourPriceDoji           => ("Four Price Doji", Candlestick, Neutral),
    Hammer                  => ("Hammer", Candlestick, Bullish),
    InvertedHammer          => ("Inverted Hammer", Candlestick, Bullish),
    HangingMan              => ("Hanging Man", Candlestick, Bearish),
    ShootingStar            => ("Shooting Star", Candlestick, Bearish),
    BullishMarubozu         => ("Bullish Marubozu", Candlestick, Bullish),
    BearishMarubozu         => ("Bearish Marubozu", Candlestick, Bearish),
    SpinningTop             => ("Spinning Top", Candlestick, Neutral),
    HighWave                => ("High Wave", Candlestick, Neutral),
    BullishBeltHold         => ("Bullish Belt Hold", Candlestick, Bullish),
    BearishBeltHold         => ("Bearish Belt Hold", Candlestick, Bearish),

    // ---- two candles -------------------------------------------------------
    BullishEngulfing        => ("Bullish Engulfing", Candlestick, Bullish),
    BearishEngulfing        => ("Bearish Engulfing", Candlestick, Bearish),
    BullishHarami           => ("Bullish Harami", Candlestick, Bullish),
    BearishHarami           => ("Bearish Harami", Candlestick, Bearish),
    BullishHaramiCross      => ("Bullish Harami Cross", Candlestick, Bullish),
    BearishHaramiCross      => ("Bearish Harami Cross", Candlestick, Bearish),
    PiercingLine            => ("Piercing Line", Candlestick, Bullish),
    DarkCloudCover          => ("Dark Cloud Cover", Candlestick, Bearish),
    TweezerBottom           => ("Tweezer Bottom", Candlestick, Bullish),
    TweezerTop              => ("Tweezer Top", Candlestick, Bearish),
    BullishKicker           => ("Bullish Kicker", Candlestick, Bullish),
    BearishKicker           => ("Bearish Kicker", Candlestick, Bearish),
    BullishSeparatingLines  => ("Bullish Separating Lines", Candlestick, Bullish),
    BearishSeparatingLines  => ("Bearish Separating Lines", Candlestick, Bearish),
    MatchingLow             => ("Matching Low", Candlestick, Bullish),
    MatchingHigh            => ("Matching High", Candlestick, Bearish),
    OnNeck                  => ("On-Neck Line", Candlestick, Bearish),
    InNeck                  => ("In-Neck Line", Candlestick, Bearish),
    Thrusting               => ("Thrusting Line", Candlestick, Bearish),
    BullishCounterattack    => ("Bullish Counterattack", Candlestick, Bullish),
    BearishCounterattack    => ("Bearish Counterattack", Candlestick, Bearish),
    HomingPigeon            => ("Homing Pigeon", Candlestick, Bullish),
    DescendingHawk          => ("Descending Hawk", Candlestick, Bearish),

    // ---- three candles -----------------------------------------------------
    MorningStar             => ("Morning Star", Candlestick, Bullish),
    EveningStar             => ("Evening Star", Candlestick, Bearish),
    MorningDojiStar         => ("Morning Doji Star", Candlestick, Bullish),
    EveningDojiStar         => ("Evening Doji Star", Candlestick, Bearish),
    BullishAbandonedBaby    => ("Bullish Abandoned Baby", Candlestick, Bullish),
    BearishAbandonedBaby    => ("Bearish Abandoned Baby", Candlestick, Bearish),
    ThreeWhiteSoldiers      => ("Three White Soldiers", Candlestick, Bullish),
    ThreeBlackCrows         => ("Three Black Crows", Candlestick, Bearish),
    IdenticalThreeCrows     => ("Identical Three Crows", Candlestick, Bearish),
    ThreeInsideUp           => ("Three Inside Up", Candlestick, Bullish),
    ThreeInsideDown         => ("Three Inside Down", Candlestick, Bearish),
    ThreeOutsideUp          => ("Three Outside Up", Candlestick, Bullish),
    ThreeOutsideDown        => ("Three Outside Down", Candlestick, Bearish),
    BullishTriStar          => ("Bullish Tri-Star", Candlestick, Bullish),
    BearishTriStar          => ("Bearish Tri-Star", Candlestick, Bearish),
    StickSandwich           => ("Stick Sandwich", Candlestick, Bullish),
    UniqueThreeRiverBottom  => ("Unique Three River Bottom", Candlestick, Bullish),
    ThreeStarsInTheSouth    => ("Three Stars in the South", Candlestick, Bullish),
    AdvanceBlock            => ("Advance Block", Candlestick, Bearish),
    Deliberation            => ("Deliberation", Candlestick, Bearish),
    TwoCrows                => ("Two Crows", Candlestick, Bearish),
    UpsideGapTwoCrows       => ("Upside Gap Two Crows", Candlestick, Bearish),
    BullishTasukiGap        => ("Upside Tasuki Gap", Candlestick, Bullish),
    BearishTasukiGap        => ("Downside Tasuki Gap", Candlestick, Bearish),
    SideBySideWhiteLines    => ("Side-by-Side White Lines", Candlestick, Bullish),

    // ---- four and five candles --------------------------------------------
    RisingThreeMethods      => ("Rising Three Methods", Candlestick, Bullish),
    FallingThreeMethods     => ("Falling Three Methods", Candlestick, Bearish),
    MatHold                 => ("Mat Hold", Candlestick, Bullish),
    BullishThreeLineStrike  => ("Bullish Three Line Strike", Candlestick, Bullish),
    BearishThreeLineStrike  => ("Bearish Three Line Strike", Candlestick, Bearish),
    ConcealingBabySwallow   => ("Concealing Baby Swallow", Candlestick, Bullish),
    LadderBottom            => ("Ladder Bottom", Candlestick, Bullish),
    BullishBreakaway        => ("Bullish Breakaway", Candlestick, Bullish),
    BearishBreakaway        => ("Bearish Breakaway", Candlestick, Bearish),
    BullishHikkake          => ("Bullish Hikkake", Candlestick, Bullish),
    BearishHikkake          => ("Bearish Hikkake", Candlestick, Bearish),

    // ---- screener scans ported from the user's Chartink setups -------------
    //
    // The only non-candlestick entry. The geometric chart-structure recognisers
    // (cup shapes, double tops, head and shoulders, triangles, wedges, flags,
    // Darvas boxes, VCP, rounding bottoms, base breakouts) were removed on
    // request — they are the family where "did it really form?" is a matter of
    // opinion, and this scan answers the same question with rules that are not.
    ChartinkCupBreakout     => ("Cup & Handle Breakout (Chartink)", Chart, Bullish),
}

/// One recognised pattern occurrence.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Detection {
    pub kind: PatternKind,
    /// Bar index where the pattern completes — the bar it is drawn against.
    pub end: usize,
    /// Bar index where the structure begins (equals `end` for single candles).
    pub start: usize,
    pub direction: Direction,
    /// Confidence in 0.0..=1.0. Recognisers score how cleanly the rules were met.
    pub score: f64,
    /// Short human explanation, e.g. the measured cup depth or neckline level.
    pub detail: String,
}

impl Detection {
    pub fn new(kind: PatternKind, start: usize, end: usize, score: f64) -> Self {
        Self {
            kind,
            start,
            end,
            direction: kind.bias(),
            score: score.clamp(0.0, 1.0),
            detail: String::new(),
        }
    }

    pub fn with_detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = detail.into();
        self
    }

    pub fn with_direction(mut self, direction: Direction) -> Self {
        self.direction = direction;
        self
    }

    /// Number of bars the structure spans.
    pub fn span(&self) -> usize {
        self.end.saturating_sub(self.start) + 1
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn keys_round_trip_and_are_unique() {
        let mut seen = HashSet::new();
        for &kind in PatternKind::ALL {
            assert_eq!(PatternKind::from_key(kind.key()), Some(kind));
            assert!(seen.insert(kind.key()), "duplicate key {}", kind.key());
        }
    }

    #[test]
    fn labels_are_unique_and_non_empty() {
        let mut seen = HashSet::new();
        for &kind in PatternKind::ALL {
            assert!(!kind.label().is_empty());
            assert!(seen.insert(kind.label()), "duplicate label {}", kind.label());
        }
    }

    #[test]
    fn catalogue_is_candlesticks_plus_the_chartink_scan() {
        let candles = PatternKind::ALL.iter().filter(|k| k.family() == Candlestick).count();
        let chart = PatternKind::ALL.iter().filter(|k| k.family() == Chart).count();
        assert!(candles > 50, "the full classical candlestick set should still be here");
        // Geometric structures were removed on request; only the Chartink scan
        // remains outside the candlestick family.
        assert_eq!(chart, 1);
        assert_eq!(
            PatternKind::ALL.iter().find(|k| k.family() == Chart),
            Some(&PatternKind::ChartinkCupBreakout)
        );
    }
}
