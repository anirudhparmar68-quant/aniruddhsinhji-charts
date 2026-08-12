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

/// Kept as a one-variant enum rather than deleted: it is what the storage
/// layer and the scanner's grouping are written against, and the catalogue has
/// carried several families before. Adding one back should not be a refactor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Family {
    /// Screener-style rule sets evaluated per bar.
    Chart,
}

impl Family {
    pub fn as_str(self) -> &'static str {
        match self {
            Family::Chart => "Chart",
        }
    }
}

#[allow(unused_imports)]
use Direction::{Bearish, Bullish, Neutral};
use Family::Chart;

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
    // The whole catalogue. Candlestick recognisers (75 of them) and geometric
    // chart structures (24) were both built, tested, and then removed on
    // request — the app now reports exactly one thing, and it is the one whose
    // rules are not a matter of opinion. Both sets live in git history.
    ChartinkCupBreakout => ("Cup & Handle Breakout (Chartink)", Chart, Bullish),
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
    fn the_catalogue_is_exactly_the_chartink_scan() {
        // Candlesticks and geometric structures were both removed on request.
        // If either ever comes back, this is the line that will say so.
        assert_eq!(PatternKind::ALL, &[PatternKind::ChartinkCupBreakout]);
        assert!(PatternKind::ALL.iter().all(|k| k.family() == Chart));
    }
}
