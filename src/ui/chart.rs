//! Candlestick chart drawn directly with egui's painter.
//!
//! A hand-drawn chart rather than a plotting library, because the things that
//! make a charting tool usable — a crosshair that snaps to bars, zooming around
//! the cursor, pattern overlays anchored to specific bars — all need control
//! over the bar↔pixel mapping that a generic plot widget hides.

use crate::model::Candle;
use crate::patterns::types::{Detection, Direction};
use crate::ta;
use egui::{Align2, Color32, FontId, Pos2, Rect, Sense, Stroke, Vec2};

const MIN_VISIBLE_BARS: usize = 15;
const PRICE_AXIS_WIDTH: f32 = 62.0;
const DATE_AXIS_HEIGHT: f32 = 22.0;
const VOLUME_PANE_FRACTION: f32 = 0.22;

/// Empty slots kept to the right of the newest candle, as a share of the
/// viewport. Price glued to the axis is hard to read and leaves nowhere for the
/// eye to project the next move, so the newest bar always floats clear of it.
const RIGHT_PAD_FRACTION: f32 = 0.08;

/// Moving averages drawn over price, with the colour each is rendered in.
pub const MA_PERIODS: [usize; 3] = [20, 50, 200];

pub struct ChartView {
    /// Index of the leftmost visible bar.
    pub first_visible: usize,
    pub visible_bars: usize,
    pub show_volume: bool,
    pub show_markers: bool,
    pub show_ma: bool,
    /// Bar the user last hovered, for the readout strip.
    pub hovered: Option<usize>,
}

impl Default for ChartView {
    fn default() -> Self {
        Self {
            first_visible: 0,
            visible_bars: 140,
            show_volume: true,
            show_markers: true,
            // Off by default: this is a price-action tool, and a clean chart is
            // worth more here than a default overlay nobody asked for.
            show_ma: false,
            hovered: None,
        }
    }
}

impl ChartView {
    /// Blank slots reserved after the newest candle.
    fn right_pad(&self) -> usize {
        ((self.visible_bars as f32 * RIGHT_PAD_FRACTION).ceil() as usize).clamp(2, 30)
    }

    /// Snap the viewport to the most recent bars — what you want on every
    /// symbol change, since this is an end-of-day tool. The newest candle lands
    /// one pad-width in from the right edge rather than against the axis.
    pub fn reset_to_latest(&mut self, total_bars: usize) {
        self.visible_bars = self.visible_bars.clamp(MIN_VISIBLE_BARS, total_bars.max(MIN_VISIBLE_BARS));
        self.first_visible = (total_bars + self.right_pad()).saturating_sub(self.visible_bars);
    }

    fn clamp(&mut self, total_bars: usize) {
        if total_bars == 0 {
            self.first_visible = 0;
            return;
        }
        self.visible_bars = self.visible_bars.clamp(MIN_VISIBLE_BARS.min(total_bars), total_bars);
        // Scrolling may run past the last candle by exactly the pad, never more.
        let max_start = (total_bars + self.right_pad()).saturating_sub(self.visible_bars);
        self.first_visible = self.first_visible.min(max_start);
    }

    /// Last slot the viewport covers, which may sit beyond the final candle.
    fn last_slot(&self) -> usize {
        self.first_visible + self.visible_bars
    }
}

struct Palette {
    bull: Color32,
    bear: Color32,
    grid: Color32,
    dim_text: Color32,
    crosshair: Color32,
    background: Color32,
    bullish_marker: Color32,
    bearish_marker: Color32,
    neutral_marker: Color32,
    ma: [Color32; 3],
    last_price: Color32,
}

impl Palette {
    fn for_ui(ui: &egui::Ui) -> Self {
        let dark = ui.visuals().dark_mode;
        Self {
            bull: if dark { Color32::from_rgb(38, 166, 109) } else { Color32::from_rgb(20, 138, 84) },
            bear: if dark { Color32::from_rgb(224, 72, 84) } else { Color32::from_rgb(198, 40, 52) },
            grid: if dark { Color32::from_gray(52) } else { Color32::from_gray(222) },
            dim_text: if dark { Color32::from_gray(150) } else { Color32::from_gray(105) },
            crosshair: if dark { Color32::from_gray(140) } else { Color32::from_gray(120) },
            background: ui.visuals().extreme_bg_color,
            bullish_marker: if dark { Color32::from_rgb(90, 210, 140) } else { Color32::from_rgb(24, 150, 92) },
            bearish_marker: if dark { Color32::from_rgb(240, 110, 120) } else { Color32::from_rgb(206, 52, 64) },
            neutral_marker: if dark { Color32::from_rgb(190, 170, 90) } else { Color32::from_rgb(160, 130, 40) },
            ma: if dark {
                [
                    Color32::from_rgb(240, 190, 90),  // 20
                    Color32::from_rgb(120, 190, 240), // 50
                    Color32::from_rgb(200, 130, 230), // 200
                ]
            } else {
                [
                    Color32::from_rgb(200, 140, 20),
                    Color32::from_rgb(30, 110, 190),
                    Color32::from_rgb(140, 60, 180),
                ]
            },
            last_price: if dark { Color32::from_gray(190) } else { Color32::from_gray(80) },
        }
    }

    fn marker(&self, dir: Direction) -> Color32 {
        match dir {
            Direction::Bullish => self.bullish_marker,
            Direction::Bearish => self.bearish_marker,
            Direction::Neutral => self.neutral_marker,
        }
    }
}

/// Draw the chart and return the bar index the pointer is over, if any.
pub fn draw(
    ui: &mut egui::Ui,
    candles: &[Candle],
    detections: &[Detection],
    view: &mut ChartView,
) -> Option<usize> {
    let available = ui.available_size();
    let (rect, response) = ui.allocate_exact_size(available, Sense::click_and_drag());
    let painter = ui.painter_at(rect);
    let palette = Palette::for_ui(ui);
    painter.rect_filled(rect, 0.0, palette.background);

    if candles.is_empty() {
        painter.text(
            rect.center(),
            Align2::CENTER_CENTER,
            // Deliberately does not say "run a sync": a backfill in progress
            // shows this same state, and telling the user to start the thing
            // they are already waiting on reads as a failure.
            "No price history stored for this stock yet.",
            FontId::proportional(13.0),
            palette.dim_text,
        );
        return None;
    }

    view.clamp(candles.len());

    // -- panes ---------------------------------------------------------------
    let plot_rect = Rect::from_min_max(
        rect.min,
        Pos2::new(rect.max.x - PRICE_AXIS_WIDTH, rect.max.y - DATE_AXIS_HEIGHT),
    );
    let volume_height = if view.show_volume { plot_rect.height() * VOLUME_PANE_FRACTION } else { 0.0 };
    let price_rect = Rect::from_min_max(
        plot_rect.min,
        Pos2::new(plot_rect.max.x, plot_rect.max.y - volume_height),
    );
    let volume_rect = Rect::from_min_max(
        Pos2::new(plot_rect.min.x, plot_rect.max.y - volume_height),
        plot_rect.max,
    );

    // -- interaction ---------------------------------------------------------
    handle_input(ui, &response, view, candles.len(), plot_rect);

    let start = view.first_visible;
    let end = view.last_slot().min(candles.len());
    if start >= end {
        return None;
    }
    let visible = &candles[start..end];

    // -- scales --------------------------------------------------------------
    let (mut lo, mut hi) = visible.iter().fold((f64::MAX, f64::MIN), |(lo, hi), c| {
        (lo.min(c.low), hi.max(c.high))
    });
    if !(lo.is_finite() && hi.is_finite()) || hi <= lo {
        let mid = if hi.is_finite() { hi } else { 1.0 };
        lo = mid * 0.99;
        hi = mid * 1.01;
    }
    let pad = (hi - lo) * 0.06;
    lo -= pad;
    hi += pad;

    // Divide by the slot count, not the candle count, so the reserved padding on
    // the right actually occupies space rather than stretching the bars.
    let bar_width = price_rect.width() / view.visible_bars.max(1) as f32;
    let x_of = |i: usize| price_rect.min.x + (i - start) as f32 * bar_width + bar_width / 2.0;
    let y_of = |price: f64| {
        let t = ((price - lo) / (hi - lo)) as f32;
        price_rect.max.y - t * price_rect.height()
    };

    let max_volume = visible.iter().map(|c| c.volume).max().unwrap_or(1).max(1) as f32;

    // -- grid and axes -------------------------------------------------------
    draw_price_grid(&painter, price_rect, rect, lo, hi, &palette, &y_of);
    draw_date_axis(&painter, price_rect, rect, visible, start, &palette, &x_of);

    // -- candles -------------------------------------------------------------
    // Capped so a handful of bars zoomed right in stay candle-shaped instead of
    // turning into fat slabs, and the wick thickens with them so it never looks
    // like a hair pinned to a brick.
    let body_width = (bar_width * 0.68).clamp(1.0, 16.0);
    let wick_width = (bar_width * 0.11).clamp(1.0, 3.0);
    for (offset, c) in visible.iter().enumerate() {
        let i = start + offset;
        let x = x_of(i);
        let colour = if c.close >= c.open { palette.bull } else { palette.bear };

        painter.line_segment(
            [Pos2::new(x, y_of(c.high)), Pos2::new(x, y_of(c.low))],
            Stroke::new(wick_width, colour),
        );

        let (top, bottom) = (y_of(c.body_top()), y_of(c.body_bottom()));
        // Doji bodies collapse to nothing; keep a visible 1px line.
        let body = Rect::from_min_max(
            Pos2::new(x - body_width / 2.0, top),
            Pos2::new(x + body_width / 2.0, bottom.max(top + 1.0)),
        );
        painter.rect_filled(body, 0.0, colour);

        if view.show_volume && volume_height > 2.0 {
            let h = (c.volume as f32 / max_volume) * (volume_rect.height() - 4.0);
            let vol_bar = Rect::from_min_max(
                Pos2::new(x - body_width / 2.0, volume_rect.max.y - h),
                Pos2::new(x + body_width / 2.0, volume_rect.max.y),
            );
            painter.rect_filled(vol_bar, 0.0, colour.gamma_multiply(0.45));
        }
    }

    if view.show_volume && volume_height > 2.0 {
        painter.line_segment(
            [Pos2::new(volume_rect.min.x, volume_rect.min.y), Pos2::new(volume_rect.max.x, volume_rect.min.y)],
            Stroke::new(1.0, palette.grid),
        );
    }

    // -- moving averages -----------------------------------------------------
    if view.show_ma {
        let closes: Vec<f64> = candles.iter().map(|c| c.close).collect();
        // Averages are computed over the whole series, not the visible window,
        // so the 200 DMA is correct at the left edge instead of warming up there.
        let clipped = painter.with_clip_rect(price_rect);
        for (slot, period) in MA_PERIODS.iter().enumerate() {
            let ma = ta::sma(&closes, *period);
            let points: Vec<Pos2> = (start..end)
                .filter_map(|i| ma.get(i).copied().flatten().map(|v| Pos2::new(x_of(i), y_of(v))))
                .collect();
            if points.len() > 1 {
                clipped.add(egui::Shape::line(points, Stroke::new(1.3, palette.ma[slot])));
            }
        }

        // Legend sits on the chart itself, so the colours never need explaining.
        let mut legend_x = price_rect.min.x + 6.0;
        for (slot, period) in MA_PERIODS.iter().enumerate() {
            let galley = painter.layout_no_wrap(
                format!("MA{period}"),
                FontId::monospace(10.0),
                palette.ma[slot],
            );
            let width = galley.size().x;
            painter.galley(Pos2::new(legend_x, price_rect.min.y + 4.0), galley, palette.ma[slot]);
            legend_x += width + 10.0;
        }
    }

    // -- last traded price ---------------------------------------------------
    if let Some(last) = candles.last() {
        let y = y_of(last.close);
        if price_rect.y_range().contains(y) {
            painter.extend(egui::Shape::dashed_line(
                &[Pos2::new(price_rect.min.x, y), Pos2::new(price_rect.max.x, y)],
                Stroke::new(1.0, palette.last_price.gamma_multiply(0.7)),
                4.0,
                4.0,
            ));
            let tag = Rect::from_min_size(
                Pos2::new(price_rect.max.x + 1.0, y - 8.0),
                Vec2::new(PRICE_AXIS_WIDTH - 2.0, 16.0),
            );
            painter.rect_filled(tag, 0.0, palette.last_price);
            painter.text(
                tag.center(),
                Align2::CENTER_CENTER,
                format_price(last.close),
                FontId::monospace(10.0),
                Color32::BLACK,
            );
        }
    }

    // -- pattern markers -----------------------------------------------------
    if view.show_markers {
        draw_markers(&painter, detections, candles, start, end, &palette, &x_of, &y_of, bar_width);
    }

    // -- crosshair -----------------------------------------------------------
    let mut hovered = None;
    if let Some(pos) = response.hover_pos() {
        if price_rect.contains(pos) || volume_rect.contains(pos) {
            let slot = (((pos.x - price_rect.min.x) / bar_width).floor() as isize).max(0) as usize
                + start;
            // The pad past the last candle still gets a crosshair, just no bar.
            hovered = (slot < end).then_some(slot);

            let x = x_of(slot.min(view.last_slot().saturating_sub(1)));
            let dashed = Stroke::new(1.0, palette.crosshair.gamma_multiply(0.7));
            painter.line_segment([Pos2::new(x, plot_rect.min.y), Pos2::new(x, plot_rect.max.y)], dashed);
            painter.line_segment(
                [Pos2::new(price_rect.min.x, pos.y), Pos2::new(price_rect.max.x, pos.y)],
                dashed,
            );

            // Date tag on the time axis, mirroring the price tag on the right.
            if let Some(candle) = candles.get(slot) {
                let text = candle.date.format("%d %b %y").to_string();
                let galley = painter.layout_no_wrap(text, FontId::monospace(10.0), Color32::BLACK);
                let tag = Rect::from_center_size(
                    Pos2::new(x, rect.max.y - DATE_AXIS_HEIGHT / 2.0),
                    Vec2::new(galley.size().x + 10.0, 15.0),
                );
                painter.rect_filled(tag, 2.0, palette.crosshair);
                painter.galley(
                    Pos2::new(tag.center().x - galley.size().x / 2.0, tag.center().y - galley.size().y / 2.0),
                    galley,
                    Color32::BLACK,
                );
            }

            // Price tag against the axis.
            if price_rect.y_range().contains(pos.y) {
                let t = (price_rect.max.y - pos.y) / price_rect.height();
                let price = lo + (hi - lo) * t as f64;
                let tag = Rect::from_min_size(
                    Pos2::new(price_rect.max.x + 1.0, pos.y - 8.0),
                    Vec2::new(PRICE_AXIS_WIDTH - 2.0, 16.0),
                );
                painter.rect_filled(tag, 0.0, palette.crosshair);
                painter.text(
                    tag.center(),
                    Align2::CENTER_CENTER,
                    format!("{price:.2}"),
                    FontId::monospace(10.0),
                    Color32::BLACK,
                );
            }
        }
    }
    view.hovered = hovered;
    hovered
}

fn handle_input(
    ui: &egui::Ui,
    response: &egui::Response,
    view: &mut ChartView,
    total: usize,
    plot_rect: Rect,
) {
    // Drag to pan, one bar per bar-width of movement.
    if response.dragged() {
        let bar_width = plot_rect.width() / view.visible_bars.max(1) as f32;
        let shift = (response.drag_delta().x / bar_width.max(0.5)).round() as isize;
        if shift != 0 {
            view.first_visible = (view.first_visible as isize - shift).max(0) as usize;
        }
    }

    // Wheel zooms around the pointer so the bar under the cursor stays put.
    let scroll = ui.input(|i| i.smooth_scroll_delta.y);
    if scroll.abs() > 0.1 && response.hovered() {
        let anchor = response
            .hover_pos()
            .map(|p| ((p.x - plot_rect.min.x) / plot_rect.width()).clamp(0.0, 1.0))
            .unwrap_or(0.5);
        let anchor_bar = view.first_visible as f32 + anchor * view.visible_bars as f32;

        let factor = if scroll > 0.0 { 0.85 } else { 1.18 };
        let new_visible = ((view.visible_bars as f32 * factor).round() as usize)
            .clamp(MIN_VISIBLE_BARS.min(total.max(1)), total.max(1));
        view.visible_bars = new_visible;
        view.first_visible = (anchor_bar - anchor * new_visible as f32).max(0.0) as usize;
    }

    // Keyboard, but only while the pointer is over the chart — otherwise the
    // arrow keys would fight the stock list the user is browsing.
    if response.hovered() {
        ui.input(|i| {
            let step = (view.visible_bars / 8).max(1);
            if i.key_pressed(egui::Key::ArrowLeft) {
                view.first_visible = view.first_visible.saturating_sub(step);
            }
            if i.key_pressed(egui::Key::ArrowRight) {
                view.first_visible = view.first_visible.saturating_add(step);
            }
            if i.key_pressed(egui::Key::Home) {
                view.first_visible = 0;
            }
            if i.key_pressed(egui::Key::End) {
                view.first_visible = total.saturating_sub(view.visible_bars);
            }
            if i.key_pressed(egui::Key::Plus) {
                view.visible_bars = (view.visible_bars * 4 / 5).max(MIN_VISIBLE_BARS.min(total.max(1)));
            }
            if i.key_pressed(egui::Key::Minus) {
                view.visible_bars = (view.visible_bars * 5 / 4 + 1).min(total.max(1));
            }
        });
    }

    view.clamp(total);
}

fn draw_price_grid(
    painter: &egui::Painter,
    price_rect: Rect,
    full: Rect,
    lo: f64,
    hi: f64,
    palette: &Palette,
    y_of: &impl Fn(f64) -> f32,
) {
    for level in nice_levels(lo, hi, 6) {
        let y = y_of(level);
        if !price_rect.y_range().contains(y) {
            continue;
        }
        painter.line_segment(
            [Pos2::new(price_rect.min.x, y), Pos2::new(price_rect.max.x, y)],
            Stroke::new(1.0, palette.grid),
        );
        painter.text(
            Pos2::new(full.max.x - 4.0, y),
            Align2::RIGHT_CENTER,
            format_price(level),
            FontId::monospace(10.0),
            palette.dim_text,
        );
    }
}

fn draw_date_axis(
    painter: &egui::Painter,
    price_rect: Rect,
    full: Rect,
    visible: &[Candle],
    start: usize,
    palette: &Palette,
    x_of: &impl Fn(usize) -> f32,
) {
    let ticks = 6.min(visible.len());
    if ticks == 0 {
        return;
    }
    let step = (visible.len() / ticks).max(1);
    for offset in (0..visible.len()).step_by(step) {
        let x = x_of(start + offset);
        // Time gridlines sit well behind the price ones; they orient the eye
        // without competing with the candles.
        painter.line_segment(
            [Pos2::new(x, price_rect.min.y), Pos2::new(x, price_rect.max.y)],
            Stroke::new(1.0, palette.grid.gamma_multiply(0.35)),
        );
        painter.text(
            Pos2::new(x, full.max.y - DATE_AXIS_HEIGHT / 2.0),
            Align2::CENTER_CENTER,
            visible[offset].date.format("%d %b %y").to_string(),
            FontId::monospace(9.5),
            palette.dim_text,
        );
    }
}

/// Small triangles above/below the bar where a pattern completes.
#[allow(clippy::too_many_arguments)]
fn draw_markers(
    painter: &egui::Painter,
    detections: &[Detection],
    candles: &[Candle],
    start: usize,
    end: usize,
    palette: &Palette,
    x_of: &impl Fn(usize) -> f32,
    y_of: &impl Fn(f64) -> f32,
    bar_width: f32,
) {
    // Stack markers when several patterns land on the same bar.
    let mut stack: std::collections::HashMap<usize, usize> = std::collections::HashMap::new();
    let size = (bar_width * 0.35).clamp(3.0, 7.0);

    for det in detections.iter().filter(|d| d.end >= start && d.end < end) {
        let Some(candle) = candles.get(det.end) else { continue };
        let level = stack.entry(det.end).or_insert(0);
        let offset = (*level as f32) * (size * 2.2 + 2.0);
        *level += 1;

        let x = x_of(det.end);
        let colour = palette.marker(det.direction);
        let (tip, base_y) = match det.direction {
            Direction::Bearish => {
                let y = y_of(candle.high) - 6.0 - offset;
                (Pos2::new(x, y + size), y - size)
            }
            _ => {
                let y = y_of(candle.low) + 6.0 + offset;
                (Pos2::new(x, y - size), y + size)
            }
        };
        painter.add(egui::Shape::convex_polygon(
            vec![tip, Pos2::new(x - size, base_y), Pos2::new(x + size, base_y)],
            colour,
            Stroke::NONE,
        ));
    }
}

/// Round grid levels a human would pick (1, 2, 2.5, 5 × 10^n).
fn nice_levels(lo: f64, hi: f64, target: usize) -> Vec<f64> {
    if !(lo.is_finite() && hi.is_finite()) || hi <= lo || target == 0 {
        return Vec::new();
    }
    let raw = (hi - lo) / target as f64;
    let magnitude = 10f64.powf(raw.log10().floor());
    let normalised = raw / magnitude;
    let step = magnitude
        * if normalised <= 1.0 {
            1.0
        } else if normalised <= 2.0 {
            2.0
        } else if normalised <= 2.5 {
            2.5
        } else if normalised <= 5.0 {
            5.0
        } else {
            10.0
        };

    let mut out = Vec::new();
    let mut level = (lo / step).ceil() * step;
    while level <= hi && out.len() < target * 3 {
        out.push(level);
        level += step;
    }
    out
}

fn format_price(p: f64) -> String {
    if p >= 10_000.0 {
        format!("{p:.0}")
    } else if p >= 100.0 {
        format!("{p:.1}")
    } else {
        format!("{p:.2}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nice_levels_are_ordered_and_inside_range() {
        let levels = nice_levels(97.3, 154.8, 6);
        assert!(!levels.is_empty());
        for w in levels.windows(2) {
            assert!(w[1] > w[0]);
        }
        assert!(levels.iter().all(|l| *l >= 97.3 && *l <= 154.8));
    }

    #[test]
    fn nice_levels_handles_degenerate_input() {
        assert!(nice_levels(100.0, 100.0, 5).is_empty());
        assert!(nice_levels(f64::NAN, 10.0, 5).is_empty());
        assert!(nice_levels(1.0, 2.0, 0).is_empty());
    }

    #[test]
    fn view_clamps_to_available_bars() {
        let mut v = ChartView::default();
        v.visible_bars = 500;
        v.first_visible = 900;
        v.clamp(120);
        assert!(v.visible_bars <= 120);
        // The viewport may run past the last candle by the pad, never further.
        assert!(v.first_visible + v.visible_bars <= 120 + v.right_pad());
    }

    #[test]
    fn reset_leaves_breathing_room_after_the_last_candle() {
        let mut v = ChartView { visible_bars: 60, ..Default::default() };
        v.reset_to_latest(300);
        let pad = v.right_pad();
        assert!(pad >= 2, "there must always be some space on the right");
        assert_eq!(v.first_visible + v.visible_bars, 300 + pad);
        assert!(v.first_visible < 300, "the newest candle must still be on screen");
    }

    #[test]
    fn padding_scales_with_zoom_but_stays_bounded() {
        let tight = ChartView { visible_bars: 20, ..Default::default() };
        let wide = ChartView { visible_bars: 400, ..Default::default() };
        assert!(tight.right_pad() >= 2);
        assert!(wide.right_pad() <= 30, "padding must not eat the chart when zoomed out");
        assert!(wide.right_pad() >= tight.right_pad());
    }

    #[test]
    fn moving_averages_are_off_by_default() {
        assert!(!ChartView::default().show_ma, "a clean chart is the default");
    }

    #[test]
    fn clamp_on_empty_series_is_safe() {
        let mut v = ChartView::default();
        v.clamp(0);
        assert_eq!(v.first_visible, 0);
    }
}
