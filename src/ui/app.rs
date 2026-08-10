//! The eframe application: stock list, chart, scanner and settings.

use crate::config::Settings;
use crate::model::Instrument;
use crate::patterns::types::{Detection, Direction, Family, PatternKind};
use crate::sync::{self, Command, Event, Worker};
use crate::ui::chart::{self, ChartView};
use chrono::{Duration, NaiveDate};
use egui::{Color32, RichText};

#[derive(PartialEq, Eq, Clone, Copy)]
enum Tab {
    Chart,
    Scanner,
    Settings,
}

/// How far back the scanner looks, measured from the newest session in the
/// data rather than from the calendar — on a Sunday "today" holds nothing.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Window {
    Latest,
    Days3,
    Week,
    Month,
    All,
}

impl Window {
    const ALL: [Window; 5] = [Window::Latest, Window::Days3, Window::Week, Window::Month, Window::All];

    fn label(self) -> &'static str {
        match self {
            Window::Latest => "Latest candle",
            Window::Days3 => "3 days",
            Window::Week => "1 week",
            Window::Month => "1 month",
            Window::All => "Everything",
        }
    }

    fn cutoff(self, latest: NaiveDate) -> NaiveDate {
        match self {
            Window::Latest => latest,
            Window::Days3 => latest - Duration::days(3),
            Window::Week => latest - Duration::days(7),
            Window::Month => latest - Duration::days(30),
            Window::All => NaiveDate::from_ymd_opt(1970, 1, 1).expect("epoch is a valid date"),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SortKey {
    Conviction,
    Score,
    Change,
    Volume,
    Rsi,
    Symbol,
    Date,
}

impl SortKey {
    fn label(self) -> &'static str {
        match self {
            SortKey::Conviction => "Conf",
            SortKey::Score => "Score",
            SortKey::Change => "Chg%",
            SortKey::Volume => "Vol×",
            SortKey::Rsi => "RSI",
            SortKey::Symbol => "Symbol",
            SortKey::Date => "Date",
        }
    }
}

/// One rendered scanner line: a detection plus the price context of the stock
/// it fired on, so the table can be judged without opening every chart.
struct ScanDisplayRow {
    key: String,
    symbol: String,
    name: String,
    date: NaiveDate,
    pattern: &'static str,
    direction: Direction,
    score: f64,
    detail: String,
    change_pct: f64,
    volume_ratio: f64,
    rsi: f64,
    below_52w: f64,
    conviction: f64,
    agreement: (usize, usize),
}

#[derive(Clone)]
struct ScannerFilter {
    family: Option<Family>,
    direction: Option<Direction>,
    kind: Option<PatternKind>,
    min_score: f64,
    window: Window,
    symbol_search: String,
}

impl Default for ScannerFilter {
    fn default() -> Self {
        Self {
            family: None,
            direction: None,
            kind: None,
            min_score: 0.5,
            // The question this tool exists to answer is "what fired on the
            // most recent candle", so that is what it opens on.
            window: Window::Latest,
            symbol_search: String::new(),
        }
    }
}

/// One rendered line of the stock list. Materialised per frame so the shared
/// state lock is released before any drawing happens.
struct SidebarRow {
    key: String,
    symbol: String,
    name: String,
    mcap_cr: Option<f64>,
    hits: usize,
    /// Signed agreement of the newest candle's patterns, -1..=1.
    conviction: f64,
}

pub struct SpiderApp {
    worker: Worker,
    settings: Settings,
    settings_draft: Settings,
    tab: Tab,
    search: String,
    selected: Option<String>,
    chart: ChartView,
    status: String,
    busy: bool,
    progress: Option<(usize, usize, String)>,
    messages: Vec<String>,
    filter: ScannerFilter,
    /// Narrow the sidebar to stocks that printed a pattern on the newest session.
    only_latest_hits: bool,
    sort_key: SortKey,
    sort_desc: bool,
    /// Row the list should scroll into view on the next frame, set by keyboard
    /// navigation so the selection never disappears off-screen.
    scroll_target: Option<usize>,
}

impl SpiderApp {
    pub fn new(settings: Settings) -> Self {
        let worker = sync::spawn(settings.clone());
        Self {
            worker,
            settings_draft: settings.clone(),
            settings,
            tab: Tab::Chart,
            search: String::new(),
            selected: None,
            chart: ChartView::default(),
            status: "Ready".into(),
            busy: false,
            progress: None,
            messages: Vec::new(),
            filter: ScannerFilter::default(),
            only_latest_hits: false,
            sort_key: SortKey::Conviction,
            sort_desc: true,
            scroll_target: None,
        }
    }

    fn send(&self, cmd: Command) {
        let _ = self.worker.commands.send(cmd);
    }

    fn drain_events(&mut self, ctx: &egui::Context) {
        let mut changed = false;
        while let Ok(event) = self.worker.events.try_recv() {
            changed = true;
            match event {
                Event::Status(s) => {
                    self.status = s.clone();
                    self.messages.push(s);
                }
                Event::Progress { done, total, label } => self.progress = Some((done, total, label)),
                Event::Busy(b) => {
                    self.busy = b;
                    if !b {
                        self.progress = None;
                    }
                }
                Event::Error(e) => {
                    self.status = format!("⚠ {e}");
                    self.messages.push(format!("ERROR: {e}"));
                }
                Event::DataChanged | Event::ScannerChanged => {}
            }
        }
        // Keep the log bounded; it is a status trail, not an audit record.
        if self.messages.len() > 300 {
            let drop = self.messages.len() - 300;
            self.messages.drain(..drop);
        }
        if changed || self.busy {
            ctx.request_repaint_after(std::time::Duration::from_millis(120));
        }
    }

    /// Cheap pointer clone of the worker's latest published state.
    ///
    /// Everything the UI renders comes from one of these. No lock is ever held
    /// while drawing — that is what makes the worker's updates non-blocking.
    fn snapshot(&self) -> std::sync::Arc<sync::Shared> {
        sync::snapshot(&self.worker.state)
    }

    fn select(&mut self, key: String) {
        let bars = self.snapshot().candles.get(&key).map(|c| c.len()).unwrap_or(0);
        self.selected = Some(key);
        self.chart.reset_to_latest(bars);
        self.tab = Tab::Chart;
    }
}

impl eframe::App for SpiderApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        // eframe 0.36 hands the app a root `Ui` rather than a `Context`, and
        // panels are nested inside it. The context is cheap to clone (it is an
        // Arc handle) and we need it free of the `ui` borrow.
        let ctx = ui.ctx().clone();
        self.drain_events(&ctx);

        // Open on the strongest setup from the newest candle rather than an
        // empty pane. Fires once, as soon as a scan has produced something.
        if self.selected.is_none() {
            let pick = self
                .snapshot()
                .latest_hits
                .iter()
                .max_by(|a, b| {
                    a.1.conviction()
                        .abs()
                        .partial_cmp(&b.1.conviction().abs())
                        .unwrap_or(std::cmp::Ordering::Equal)
                })
                .map(|(key, _)| key.clone());
            if let Some(key) = pick {
                self.select(key);
            }
        }

        self.top_bar(ui);
        self.sidebar(ui);

        egui::CentralPanel::default().show(ui, |ui| match self.tab {
            Tab::Chart => self.chart_tab(ui),
            Tab::Scanner => self.scanner_tab(ui),
            Tab::Settings => self.settings_tab(ui),
        });
    }
}

// ---------------------------------------------------------------------------
// Top bar
// ---------------------------------------------------------------------------

impl SpiderApp {
    fn top_bar(&mut self, ui: &mut egui::Ui) {
        egui::Panel::top("top").show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.heading("Spider Charts");
                ui.separator();
                ui.selectable_value(&mut self.tab, Tab::Chart, "Chart");
                ui.selectable_value(&mut self.tab, Tab::Scanner, "Scanner");
                ui.selectable_value(&mut self.tab, Tab::Settings, "Settings");
                ui.separator();

                ui.add_enabled_ui(!self.busy, |ui| {
                    if ui.button("Full sync").on_hover_text(
                        "Instrument master → market caps → daily history → pattern scan",
                    ).clicked() {
                        self.send(Command::FullSync);
                    }
                    if ui.button("Universe").on_hover_text("Refresh the NSE + BSE list and market caps").clicked() {
                        self.send(Command::RefreshUniverse);
                    }
                    if ui.button("History").on_hover_text("Download only the missing sessions").clicked() {
                        self.send(Command::Backfill);
                    }
                    if ui.button("Rescan").on_hover_text("Re-run every pattern over stored data").clicked() {
                        self.send(Command::ScanAll);
                    }
                });

                ui.separator();
                let stats = self.snapshot().stats;
                ui.label(
                    RichText::new(format!(
                        "{} in universe · {} mcap resolved · {} on liquidity",
                        stats.passed_on_mcap + stats.passed_on_liquidity,
                        stats.mcap_resolved,
                        stats.passed_on_liquidity
                    ))
                    .small(),
                );

                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let dark = ui.visuals().dark_mode;
                    if ui
                        .button(if dark { "Light" } else { "Dark" })
                        .on_hover_text("Switch theme — the chart repaints for either")
                        .clicked()
                    {
                        ui.ctx().set_theme(if dark {
                            egui::ThemePreference::Light
                        } else {
                            egui::ThemePreference::Dark
                        });
                    }
                });
            });

            ui.horizontal(|ui| {
                if self.busy {
                    ui.spinner();
                }
                ui.label(RichText::new(&self.status).small());
                if let Some((done, total, label)) = &self.progress {
                    let fraction = *done as f32 / (*total).max(1) as f32;
                    ui.add(
                        egui::ProgressBar::new(fraction)
                            .desired_width(240.0)
                            .text(format!("{done}/{total}  {label}")),
                    );
                }
            });
        });
    }

    // -----------------------------------------------------------------------
    // Sidebar: the alphabetical stock list
    // -----------------------------------------------------------------------

    fn sidebar(&mut self, ui: &mut egui::Ui) {
        egui::Panel::left("stocks")
            .resizable(true)
            .default_size(250.0)
            .show(ui, |ui| {
                ui.add_space(4.0);
                let mut search_focused = false;
                ui.horizontal(|ui| {
                    ui.label("🔍");
                    let field = ui.add(
                        egui::TextEdit::singleline(&mut self.search)
                            .hint_text("Symbol or company   (Ctrl+K)")
                            .desired_width(f32::INFINITY),
                    );
                    if ui.input(|i| i.modifiers.command && i.key_pressed(egui::Key::K)) {
                        field.request_focus();
                    }
                    search_focused = field.has_focus();
                });

                let needle = self.search.trim().to_uppercase();
                // The list is already alphabetical from the store; filtering preserves that.
                let (rows, total_hits) = {
                    let snap = self.snapshot();
                    let hits = &snap.latest_hits;
                    let rows: Vec<SidebarRow> = snap
                        .included()
                        .into_iter()
                        .filter(|i| {
                            needle.is_empty()
                                || i.symbol.contains(&needle)
                                || i.name.to_uppercase().contains(&needle)
                        })
                        .filter(|i| !self.only_latest_hits || hits.contains_key(&i.instrument_key))
                        .map(|i| {
                            let summary = hits.get(&i.instrument_key);
                            SidebarRow {
                                key: i.instrument_key.clone(),
                                symbol: i.symbol.clone(),
                                name: i.name.clone(),
                                mcap_cr: i.mcap_cr,
                                hits: summary.map(|s| s.total()).unwrap_or(0),
                                conviction: summary.map(|s| s.conviction()).unwrap_or(0.0),
                            }
                        })
                        .collect();
                    (rows, hits.len())
                };

                ui.add_space(2.0);
                ui.horizontal(|ui| {
                    ui.label(RichText::new(format!("{} stocks", rows.len())).small().weak());
                    if total_hits > 0 {
                        ui.checkbox(&mut self.only_latest_hits, RichText::new(format!("today ({total_hits})")).small())
                            .on_hover_text("Only stocks that printed a pattern on the newest candle");
                    }
                });
                ui.separator();

                if rows.is_empty() {
                    ui.add_space(8.0);
                    ui.label(
                        RichText::new("Nothing loaded yet.\nPress “Full sync” to build the universe.")
                            .small()
                            .weak(),
                    );
                    return;
                }

                let row_height = ui.text_style_height(&egui::TextStyle::Body) + 6.0;
                let mut clicked = None;

                // Up/Down walk the list with the chart following instantly.
                // The chart claims Left/Right for panning, so the two never
                // fight; and while the search box has focus it keeps the keys.
                if !search_focused {
                    let (down, up) = ui.input(|i| {
                        (i.key_pressed(egui::Key::ArrowDown), i.key_pressed(egui::Key::ArrowUp))
                    });
                    if (down || up) && !rows.is_empty() {
                        let current = self
                            .selected
                            .as_deref()
                            .and_then(|key| rows.iter().position(|r| r.key == key));
                        let next = match current {
                            Some(i) if down => (i + 1).min(rows.len() - 1),
                            Some(i) => i.saturating_sub(1),
                            None => 0,
                        };
                        clicked = Some(rows[next].key.clone());
                        self.scroll_target = Some(next);
                    }
                }

                let mut area = egui::ScrollArea::vertical().auto_shrink([false, false]);
                if let Some(target) = self.scroll_target.take() {
                    // Keep the selection roughly a third down the viewport
                    // rather than pinned to an edge.
                    let offset = (target as f32 * row_height - ui.available_height() / 3.0).max(0.0);
                    area = area.vertical_scroll_offset(offset);
                }
                area.show_rows(
                    ui,
                    row_height,
                    rows.len(),
                    |ui, range| {
                        for idx in range {
                            let row = &rows[idx];
                            let is_selected = self.selected.as_deref() == Some(row.key.as_str());

                            // Stocks that printed something on the newest candle
                            // are tinted and carry a count, so the list itself
                            // answers "where should I look tonight".
                            let mut text = RichText::new(if row.hits > 0 {
                                format!("{}  ({})", row.symbol, row.hits)
                            } else {
                                row.symbol.clone()
                            })
                            .monospace()
                            .strong();
                            // Colour carries the direction the night's patterns
                            // agree on, so the list reads at a glance.
                            if row.hits > 0 {
                                text = text.color(if row.conviction > 0.15 {
                                    Color32::from_rgb(70, 200, 130)
                                } else if row.conviction < -0.15 {
                                    Color32::from_rgb(235, 100, 110)
                                } else {
                                    Color32::from_rgb(150, 165, 185)
                                });
                            }

                            let mcap = match row.mcap_cr {
                                Some(m) => format!("Market cap ≈ ₹{m:.0} cr"),
                                None => "Market cap unresolved — included on traded value".to_string(),
                            };
                            let tip = if row.hits > 0 {
                                format!("{}\n{mcap}\n{} pattern(s) on the latest candle", row.name, row.hits)
                            } else {
                                format!("{}\n{mcap}", row.name)
                            };

                            if ui.selectable_label(is_selected, text).on_hover_text(tip).clicked() {
                                clicked = Some(row.key.clone());
                            }
                        }
                    },
                );
                if let Some(key) = clicked {
                    self.select(key);
                }
            });
    }

    // -----------------------------------------------------------------------
    // Chart tab
    // -----------------------------------------------------------------------

    fn chart_tab(&mut self, ui: &mut egui::Ui) {
        let Some(key) = self.selected.clone() else {
            ui.centered_and_justified(|ui| {
                ui.label(RichText::new("Pick a stock from the list on the left").weak());
            });
            return;
        };

        let snap = self.snapshot();
        let instrument = snap.instruments.iter().find(|i| i.instrument_key == key).cloned();
        let candles = snap.candles.get(&key).cloned().unwrap_or_default();
        let detections = snap.detections.get(&key).cloned().unwrap_or_default();
        drop(snap);

        if let Some(inst) = &instrument {
            ui.horizontal(|ui| {
                ui.heading(&inst.symbol);
                ui.label(RichText::new(&inst.name).weak());
                ui.label(RichText::new(inst.exchange.as_str()).small().weak());
                if let Some(m) = inst.mcap_cr {
                    ui.label(RichText::new(format!("₹{m:.0} cr")).small().weak());
                }
                if let Some(last) = candles.last() {
                    let prev = candles.get(candles.len().saturating_sub(2)).map(|c| c.close).unwrap_or(last.close);
                    let change = if prev > 0.0 { (last.close - prev) / prev * 100.0 } else { 0.0 };
                    let colour = if change >= 0.0 { Color32::from_rgb(30, 150, 90) } else { Color32::from_rgb(200, 60, 70) };
                    ui.label(RichText::new(format!("{:.2}", last.close)).monospace().strong());
                    ui.label(RichText::new(format!("{change:+.2}%")).monospace().color(colour));
                    ui.label(RichText::new(last.date.format("%d %b %Y").to_string()).small().weak());
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.checkbox(&mut self.chart.show_markers, "Markers");
                    ui.checkbox(&mut self.chart.show_ma, "MA");
                    ui.checkbox(&mut self.chart.show_volume, "Volume");
                });
            });

            // Timeframe presets: one click for the spans people actually use.
            ui.horizontal(|ui| {
                let total = candles.len();
                for (label, bars) in
                    [("1M", 22usize), ("3M", 66), ("6M", 132), ("1Y", 250), ("All", usize::MAX)]
                {
                    if ui.small_button(label).clicked() && total > 0 {
                        self.chart.visible_bars = bars.min(total);
                        self.chart.reset_to_latest(total);
                    }
                }
                ui.separator();
                ui.label(
                    RichText::new("scroll = zoom · drag = pan · ← → pan · + − zoom · Home/End")
                        .small()
                        .weak(),
                );
            });
        }

        // What fired on the newest candle. This is the question the tool exists
        // to answer, so it sits above the chart rather than buried in the table.
        if let Some(last) = candles.last() {
            let last_idx = candles.len() - 1;
            let todays: Vec<&Detection> = detections.iter().filter(|d| d.end == last_idx).collect();
            egui::Frame::group(ui.style()).show(ui, |ui| {
                ui.horizontal_wrapped(|ui| {
                    ui.label(
                        RichText::new(format!("{} ·", last.date.format("%a %d %b %Y")))
                            .small()
                            .strong(),
                    );
                    if todays.is_empty() {
                        ui.label(RichText::new("no pattern completed on this candle").small().weak());
                    } else {
                        for det in &todays {
                            ui.label(
                                RichText::new(format!(
                                    "{} · {:.0}%",
                                    det.kind.label(),
                                    det.score * 100.0
                                ))
                                .small()
                                .strong()
                                .color(direction_colour(det.direction)),
                            )
                            .on_hover_text(&det.detail);
                        }
                    }
                });
            });
        }

        // Readout strip for the hovered bar.
        let hovered_text = match self.chart.hovered.and_then(|i| candles.get(i).map(|c| (i, c))) {
            Some((idx, c)) => {
                let prev = idx
                    .checked_sub(1)
                    .and_then(|p| candles.get(p))
                    .map(|p| p.close)
                    .unwrap_or(c.open);
                let change = if prev > 0.0 { (c.close - prev) / prev * 100.0 } else { 0.0 };
                // Naming the patterns on the hovered bar means the markers are
                // readable without cross-referencing the table below.
                let names: Vec<&str> = detections
                    .iter()
                    .filter(|d| d.end == idx)
                    .map(|d| d.kind.label())
                    .collect();
                let mut line = format!(
                    "{}   O {:.2}  H {:.2}  L {:.2}  C {:.2}  ({:+.2}%)  Vol {}",
                    c.date.format("%d %b %Y"),
                    c.open,
                    c.high,
                    c.low,
                    c.close,
                    change,
                    format_volume(c.volume)
                );
                if !names.is_empty() {
                    line.push_str("   ▸ ");
                    line.push_str(&names.join(" · "));
                }
                line
            }
            None => "Hover a candle for its OHLC and patterns".to_string(),
        };
        ui.label(RichText::new(hovered_text).monospace().small().weak());

        // Pattern list for this stock, below the chart.
        let list_height = 150.0_f32.min(ui.available_height() * 0.32);
        let chart_height = (ui.available_height() - list_height - 8.0).max(120.0);

        let mut jump_to: Option<usize> = None;
        ui.allocate_ui(egui::vec2(ui.available_width(), chart_height), |ui| {
            // The hovered bar is stored on the view; the return value is only
            // useful to callers that want it immediately.
            let _ = chart::draw(ui, &candles, &detections, &mut self.chart);
        });

        ui.separator();
        ui.label(RichText::new(format!("{} patterns detected", detections.len())).small().strong());
        egui::ScrollArea::vertical()
            .id_salt("pattern_list")
            .max_height(list_height)
            .auto_shrink([false, false])
            .show(ui, |ui| {
                egui::Grid::new("patterns").striped(true).num_columns(5).show(ui, |ui| {
                    let last_idx = candles.len().saturating_sub(1);
                    for det in &detections {
                        let Some(bar) = candles.get(det.end) else { continue };
                        let date = RichText::new(bar.date.format("%d %b %y").to_string())
                            .monospace()
                            .small();
                        // Latest-candle rows are tinted so they stand out from
                        // the historical ones scrolling below them.
                        ui.label(if det.end == last_idx {
                            date.strong().color(Color32::from_rgb(90, 180, 250))
                        } else {
                            date
                        });
                        ui.label(RichText::new(det.kind.label()).small().color(direction_colour(det.direction)));
                        ui.label(RichText::new(det.kind.family().as_str()).small().weak());
                        ui.label(RichText::new(format!("{:.0}%", det.score * 100.0)).monospace().small());
                        if ui
                            .add(egui::Label::new(RichText::new(&det.detail).small().weak()).sense(egui::Sense::click()))
                            .clicked()
                        {
                            jump_to = Some(det.start);
                        }
                        ui.end_row();
                    }
                });
            });

        if let Some(start) = jump_to {
            // Centre the formation in the viewport.
            self.chart.first_visible = start.saturating_sub(self.chart.visible_bars / 6);
        }
    }

    // -----------------------------------------------------------------------
    // Scanner tab
    // -----------------------------------------------------------------------

    fn scanner_tab(&mut self, ui: &mut egui::Ui) {
        ui.horizontal_wrapped(|ui| {
            egui::ComboBox::from_id_salt("family")
                .selected_text(match self.filter.family {
                    None => "All families",
                    Some(Family::Candlestick) => "Candlestick",
                    Some(Family::Chart) => "Chart",
                })
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut self.filter.family, None, "All families");
                    ui.selectable_value(&mut self.filter.family, Some(Family::Candlestick), "Candlestick");
                    ui.selectable_value(&mut self.filter.family, Some(Family::Chart), "Chart");
                });

            egui::ComboBox::from_id_salt("direction")
                .selected_text(match self.filter.direction {
                    None => "Any direction",
                    Some(d) => d.as_str(),
                })
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut self.filter.direction, None, "Any direction");
                    ui.selectable_value(&mut self.filter.direction, Some(Direction::Bullish), "Bullish");
                    ui.selectable_value(&mut self.filter.direction, Some(Direction::Bearish), "Bearish");
                    ui.selectable_value(&mut self.filter.direction, Some(Direction::Neutral), "Neutral");
                });

            egui::ComboBox::from_id_salt("kind")
                .width(230.0)
                .selected_text(self.filter.kind.map(|k| k.label()).unwrap_or("All patterns"))
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut self.filter.kind, None, "All patterns");
                    ui.separator();
                    for &kind in PatternKind::ALL {
                        if self.filter.family.map(|f| f != kind.family()).unwrap_or(false) {
                            continue;
                        }
                        ui.selectable_value(&mut self.filter.kind, Some(kind), kind.label());
                    }
                });

            ui.add(egui::Slider::new(&mut self.filter.min_score, 0.0..=1.0).text("min score"));
            ui.add(
                egui::TextEdit::singleline(&mut self.filter.symbol_search)
                    .hint_text("symbol")
                    .desired_width(90.0),
            );
        });

        let latest = self.snapshot().latest_session;
        ui.horizontal(|ui| {
            ui.label(RichText::new("Show:").small().weak());
            for window in Window::ALL {
                ui.selectable_value(&mut self.filter.window, window, window.label());
            }
            if let Some(latest) = latest {
                ui.separator();
                ui.label(
                    RichText::new(format!("latest candle: {}", latest.format("%a %d %b %Y")))
                        .small()
                        .weak(),
                );
            }
        });
        ui.separator();

        let Some(latest) = latest else {
            ui.add_space(12.0);
            ui.label(RichText::new("No price data yet — run a sync.").weak());
            return;
        };
        let cutoff = self.filter.window.cutoff(latest);
        let needle = self.filter.symbol_search.trim().to_uppercase();

        let snap = self.snapshot();
        let mut rows: Vec<ScanDisplayRow> = {
            let state = &snap;
            state
                .scanner
                .iter()
                .filter(|r| r.date >= cutoff)
                .filter(|r| r.detection.score >= self.filter.min_score)
                .filter(|r| self.filter.family.map(|f| r.detection.kind.family() == f).unwrap_or(true))
                .filter(|r| self.filter.direction.map(|d| r.detection.direction == d).unwrap_or(true))
                .filter(|r| self.filter.kind.map(|k| r.detection.kind == k).unwrap_or(true))
                .filter(|r| needle.is_empty() || r.symbol.contains(&needle))
                .take(4000)
                .map(|r| {
                    // Context is measured on the newest candle. Rows from older
                    // sessions still carry it, but they are rendered dimmed so
                    // nobody reads today's volume as that day's volume.
                    let s = state.latest_hits.get(&r.instrument_key);
                    ScanDisplayRow {
                        key: r.instrument_key.clone(),
                        symbol: r.symbol.clone(),
                        name: r.name.clone(),
                        date: r.date,
                        pattern: r.detection.kind.label(),
                        direction: r.detection.direction,
                        score: r.detection.score,
                        detail: r.detection.detail.clone(),
                        change_pct: s.map(|s| s.change_pct).unwrap_or(0.0),
                        volume_ratio: s.map(|s| s.volume_ratio).unwrap_or(0.0),
                        rsi: s.map(|s| s.rsi).unwrap_or(0.0),
                        below_52w: s.map(|s| s.below_52w_high).unwrap_or(0.0),
                        conviction: s.map(|s| s.conviction()).unwrap_or(0.0),
                        agreement: s.map(|s| (s.bullish, s.bearish)).unwrap_or((0, 0)),
                    }
                })
                .collect()
        };
        sort_rows(&mut rows, self.sort_key, self.sort_desc);

        let distinct: std::collections::HashSet<&str> =
            rows.iter().map(|r| r.symbol.as_str()).collect();
        ui.horizontal(|ui| {
            ui.label(
                RichText::new(format!(
                    "{} patterns across {} stocks · {}{}",
                    rows.len(),
                    distinct.len(),
                    if self.filter.window == Window::Latest {
                        format!("{}", latest.format("%d %b %Y"))
                    } else {
                        format!("since {}", cutoff.format("%d %b %Y"))
                    },
                    if rows.len() == 4000 { " · showing the first 4000" } else { "" }
                ))
                .small()
                .strong(),
            );
            if ui
                .small_button("Export CSV")
                .on_hover_text("Write exactly this view — filters, sort and all — to data/scan_export.csv")
                .clicked()
            {
                self.status = match export_scan_csv(&rows) {
                    Ok(path) => format!("Exported {} rows to {path}", rows.len()),
                    Err(e) => format!("⚠ export failed: {e:#}"),
                };
            }
        });

        let mut clicked = None;
        let mut new_sort = None;
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            egui::Grid::new("scanner").striped(true).num_columns(10).show(ui, |ui| {
                // Clickable headers; the active one carries the sort arrow.
                let mut header = |ui: &mut egui::Ui, key: SortKey| {
                    let arrow = if self.sort_key == key {
                        if self.sort_desc { " ▼" } else { " ▲" }
                    } else {
                        ""
                    };
                    if ui
                        .add(
                            egui::Label::new(
                                RichText::new(format!("{}{arrow}", key.label())).small().strong(),
                            )
                            .sense(egui::Sense::click()),
                        )
                        .clicked()
                    {
                        new_sort = Some(key);
                    }
                };
                header(ui, SortKey::Date);
                header(ui, SortKey::Symbol);
                ui.label(RichText::new("Pattern").small().strong());
                ui.label(RichText::new("Bias").small().strong());
                header(ui, SortKey::Score);
                header(ui, SortKey::Change);
                header(ui, SortKey::Volume);
                header(ui, SortKey::Rsi);
                header(ui, SortKey::Conviction);
                ui.label(RichText::new("Detail").small().strong());
                ui.end_row();

                for row in &rows {
                    let stale = row.date != latest;
                    let dim = |t: RichText| if stale { t.weak() } else { t };

                    ui.label(RichText::new(row.date.format("%d %b %y").to_string()).monospace().small());
                    if ui
                        .add(
                            egui::Label::new(RichText::new(&row.symbol).monospace().strong())
                                .sense(egui::Sense::click()),
                        )
                        .on_hover_text(&row.name)
                        .clicked()
                    {
                        clicked = Some(row.key.clone());
                    }
                    ui.label(RichText::new(row.pattern).small());
                    ui.label(
                        RichText::new(row.direction.as_str())
                            .small()
                            .color(direction_colour(row.direction)),
                    );
                    ui.label(RichText::new(format!("{:.0}%", row.score * 100.0)).monospace().small());

                    let change_colour = if row.change_pct >= 0.0 {
                        Color32::from_rgb(60, 170, 110)
                    } else {
                        Color32::from_rgb(210, 80, 90)
                    };
                    ui.label(dim(
                        RichText::new(format!("{:+.1}", row.change_pct)).monospace().small().color(change_colour),
                    ));
                    ui.label(dim(RichText::new(format!("{:.1}×", row.volume_ratio)).monospace().small()));
                    ui.label(dim(RichText::new(format!("{:.0}", row.rsi)).monospace().small()));

                    let (bull, bear) = row.agreement;
                    ui.label(
                        dim(RichText::new(format!("{:+.2}", row.conviction))
                            .monospace()
                            .small()
                            .color(if row.conviction > 0.15 {
                                Color32::from_rgb(70, 200, 130)
                            } else if row.conviction < -0.15 {
                                Color32::from_rgb(235, 100, 110)
                            } else {
                                Color32::from_gray(150)
                            })),
                    )
                    .on_hover_text(format!(
                        "{bull} bullish / {bear} bearish patterns on the latest candle\n\
                         {:.1}% below the 52-week high",
                        row.below_52w * 100.0
                    ));

                    ui.label(RichText::new(&row.detail).small().weak());
                    ui.end_row();
                }
            });
        });

        if let Some(key) = new_sort {
            // Clicking the active column flips direction; a new column starts
            // descending, which is what "best first" means for every metric here.
            if self.sort_key == key {
                self.sort_desc = !self.sort_desc;
            } else {
                self.sort_key = key;
                self.sort_desc = true;
            }
        }
        if let Some(key) = clicked {
            self.select(key);
        }
    }

    // -----------------------------------------------------------------------
    // Settings tab
    // -----------------------------------------------------------------------

    fn settings_tab(&mut self, ui: &mut egui::Ui) {
        ui.heading("Universe");
        egui::Grid::new("settings_universe").num_columns(2).spacing([16.0, 8.0]).show(ui, |ui| {
            ui.label("Minimum market cap (₹ crore)");
            ui.add(egui::DragValue::new(&mut self.settings_draft.min_mcap_cr).speed(10.0).range(0.0..=1_000_000.0));
            ui.end_row();

            ui.label("Keep stocks with unresolved market cap");
            ui.checkbox(&mut self.settings_draft.include_unknown_mcap, "")
                .on_hover_text("They are judged on traded value instead of being dropped silently");
            ui.end_row();

            ui.label("Minimum median turnover (₹ crore/day)");
            ui.add(egui::DragValue::new(&mut self.settings_draft.min_median_turnover_cr).speed(0.05).range(0.0..=500.0));
            ui.end_row();

            ui.label("Minimum price (₹)");
            ui.add(egui::DragValue::new(&mut self.settings_draft.min_price).speed(1.0).range(0.0..=10_000.0));
            ui.end_row();

            ui.label("Prefer NSE for dual-listed stocks");
            ui.checkbox(&mut self.settings_draft.prefer_nse, "");
            ui.end_row();
        });

        ui.add_space(12.0);
        ui.heading("Data");
        egui::Grid::new("settings_data").num_columns(2).spacing([16.0, 8.0]).show(ui, |ui| {
            ui.label("History to keep (calendar days)");
            ui.add(egui::DragValue::new(&mut self.settings_draft.history_days).speed(5.0).range(90..=3650));
            ui.end_row();

            ui.label("Parallel downloads");
            ui.add(egui::Slider::new(&mut self.settings_draft.max_concurrency, 1..=32))
                .on_hover_text("How many requests may be in flight at once");
            ui.end_row();

            ui.label("Requests per second");
            ui.add(egui::Slider::new(&mut self.settings_draft.requests_per_second, 0.5..=20.0).step_by(0.5))
                .on_hover_text(
                    "Global rate cap. Upstox enforces per-minute and per-30-minute quotas that \
                     concurrency alone cannot respect — lower this if downloads start failing.",
                );
            ui.end_row();
        });

        ui.add_space(12.0);
        ui.heading("Patterns");
        egui::Grid::new("settings_patterns").num_columns(2).spacing([16.0, 8.0]).show(ui, |ui| {
            ui.label("Minimum score to report");
            ui.add(egui::Slider::new(&mut self.settings_draft.min_pattern_score, 0.0..=1.0));
            ui.end_row();
        });

        ui.add_space(12.0);
        ui.collapsing("Cup & Handle Breakout — your Chartink scan", |ui| {
            ui.label(
                RichText::new(
                    "A faithful port of the Chartink screener, and the only non-candlestick \
                     pattern in the app. It is a momentum base-breakout checklist rather than a \
                     shape test, which is exactly why it stayed when the geometric structures went.",
                )
                .small()
                .weak(),
            );
            ui.add_space(6.0);
            let ck = &mut self.settings_draft.patterns.chartink;
            egui::Grid::new("settings_chartink").num_columns(2).spacing([16.0, 8.0]).show(ui, |ui| {
                ui.label("Breakout lookback (closing high)");
                ui.add(egui::DragValue::new(&mut ck.breakout_lookback).speed(1.0).range(5..=250));
                ui.end_row();

                ui.label("Dip window / base window");
                ui.horizontal(|ui| {
                    ui.add(egui::DragValue::new(&mut ck.depth_window).speed(1.0).range(5..=120));
                    ui.add(egui::DragValue::new(&mut ck.base_window).speed(1.0).range(10..=250));
                });
                ui.end_row();

                ui.label("Depth ratio");
                ui.add(egui::Slider::new(&mut ck.depth_ratio, 0.5..=0.99))
                    .on_hover_text("Recent low must sit below base high × this. 0.85 means at least a 15% dip.");
                ui.end_row();

                ui.label("Trend SMA");
                ui.add(egui::DragValue::new(&mut ck.trend_sma).speed(1.0).range(5..=250));
                ui.end_row();

                ui.label("Volume SMAs (fast / slow)");
                ui.horizontal(|ui| {
                    ui.add(egui::DragValue::new(&mut ck.volume_fast_sma).speed(1.0).range(2..=60));
                    ui.add(egui::DragValue::new(&mut ck.volume_slow_sma).speed(1.0).range(5..=250));
                });
                ui.end_row();

                ui.label("RSI period / minimum");
                ui.horizontal(|ui| {
                    ui.add(egui::DragValue::new(&mut ck.rsi_period).speed(1.0).range(2..=100));
                    ui.add(egui::DragValue::new(&mut ck.rsi_min).speed(1.0).range(0.0..=100.0));
                });
                ui.end_row();

                ui.label("ADX period / minimum");
                ui.horizontal(|ui| {
                    ui.add(egui::DragValue::new(&mut ck.adx_period).speed(1.0).range(2..=100));
                    ui.add(egui::DragValue::new(&mut ck.adx_min).speed(1.0).range(0.0..=100.0));
                });
                ui.end_row();

                ui.label("Volume surge");
                ui.horizontal(|ui| {
                    ui.add(egui::DragValue::new(&mut ck.volume_multiple).speed(0.1).range(0.0..=20.0).prefix("× "));
                    ui.label("of Sma(Volume,");
                    ui.add(egui::DragValue::new(&mut ck.volume_sma).speed(1.0).range(2..=250));
                    ui.label(")");
                });
                ui.end_row();
            });
        });

        ui.add_space(16.0);
        ui.horizontal(|ui| {
            if ui.button("Save settings").clicked() {
                self.settings = self.settings_draft.clone();
                match self.settings.save() {
                    Ok(()) => self.status = "Settings saved — run a rescan to apply them".into(),
                    Err(e) => self.status = format!("⚠ could not save settings: {e:#}"),
                }
                // The worker holds its own copy; without this it would keep
                // scanning with the values loaded at start-up.
                self.send(Command::UpdateSettings(Box::new(self.settings.clone())));
            }
            if ui.button("Revert").clicked() {
                self.settings_draft = self.settings.clone();
            }
            if ui.button("Re-login to Upstox").clicked() {
                crate::upstox::auth::clear_cached_token();
                self.send(Command::Login);
            }
        });

        ui.add_space(8.0);
        ui.label(
            RichText::new(
                "Settings apply to the next sync or scan. Changing the market-cap floor only \
                 takes effect after a universe refresh.",
            )
            .small()
            .weak(),
        );

        ui.add_space(16.0);
        ui.collapsing("Activity log", |ui| {
            egui::ScrollArea::vertical().max_height(220.0).stick_to_bottom(true).show(ui, |ui| {
                for line in &self.messages {
                    ui.label(RichText::new(line).monospace().small());
                }
            });
        });
    }
}

/// Order the scanner table.
///
/// Conviction sorts on *magnitude*, so the strongest agreement surfaces whether
/// it is bullish or bearish — the Bias column says which way. Every sort is
/// tie-broken on symbol so the table never reshuffles between frames.
fn sort_rows(rows: &mut [ScanDisplayRow], key: SortKey, desc: bool) {
    use std::cmp::Ordering;
    let num = |x: f64, y: f64| x.partial_cmp(&y).unwrap_or(Ordering::Equal);

    rows.sort_by(|a, b| {
        let ordering = match key {
            SortKey::Conviction => num(a.conviction.abs(), b.conviction.abs()),
            SortKey::Score => num(a.score, b.score),
            SortKey::Change => num(a.change_pct, b.change_pct),
            SortKey::Volume => num(a.volume_ratio, b.volume_ratio),
            SortKey::Rsi => num(a.rsi, b.rsi),
            SortKey::Symbol => a.symbol.cmp(&b.symbol),
            SortKey::Date => a.date.cmp(&b.date),
        };
        let ordering = if desc { ordering.reverse() } else { ordering };
        ordering.then_with(|| a.symbol.cmp(&b.symbol)).then_with(|| a.pattern.cmp(b.pattern))
    });
}

fn csv_quote(value: &str) -> String {
    format!("\"{}\"", value.replace('"', "\"\""))
}

/// Write the current scanner view to `data/scan_export.csv`.
fn export_scan_csv(rows: &[ScanDisplayRow]) -> anyhow::Result<String> {
    let path = crate::config::data_dir().join("scan_export.csv");
    let mut out = String::from(
        "date,symbol,name,pattern,bias,score,change_pct,volume_ratio,rsi,below_52w_pct,conviction,detail\n",
    );
    for r in rows {
        out.push_str(&format!(
            "{},{},{},{},{},{:.3},{:.2},{:.2},{:.1},{:.2},{:.3},{}\n",
            r.date,
            r.symbol,
            csv_quote(&r.name),
            csv_quote(r.pattern),
            r.direction.as_str(),
            r.score,
            r.change_pct,
            r.volume_ratio,
            r.rsi,
            r.below_52w * 100.0,
            r.conviction,
            csv_quote(&r.detail),
        ));
    }
    std::fs::write(&path, out)?;
    Ok(path.display().to_string())
}

fn direction_colour(direction: Direction) -> Color32 {
    match direction {
        Direction::Bullish => Color32::from_rgb(38, 160, 100),
        Direction::Bearish => Color32::from_rgb(206, 62, 74),
        Direction::Neutral => Color32::from_rgb(150, 130, 60),
    }
}

fn format_volume(volume: i64) -> String {
    let v = volume as f64;
    if v >= 1e7 {
        format!("{:.2} cr", v / 1e7)
    } else if v >= 1e5 {
        format!("{:.2} L", v / 1e5)
    } else {
        format!("{volume}")
    }
}

/// Kept for the sidebar's tooltip text and unit-tested independently of egui.
pub fn describe_instrument(inst: &Instrument) -> String {
    match inst.mcap_cr {
        Some(m) => format!("{} · ₹{m:.0} cr", inst.name),
        None => format!("{} · market cap unresolved", inst.name),
    }
}

/// Date floor used by the scanner's "days back" control.
pub fn scanner_cutoff(today: NaiveDate, days: i64) -> NaiveDate {
    today - Duration::days(days.max(0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Exchange;

    #[test]
    fn volume_formatting_uses_indian_units() {
        assert_eq!(format_volume(950), "950");
        assert_eq!(format_volume(250_000), "2.50 L");
        assert_eq!(format_volume(35_000_000), "3.50 cr");
    }

    #[test]
    fn describes_missing_market_cap_explicitly() {
        let mut inst = Instrument {
            instrument_key: "NSE_EQ|X".into(),
            symbol: "X".into(),
            name: "X Ltd".into(),
            isin: "INE1".into(),
            exchange: Exchange::Nse,
            mcap_cr: None,
            included: true,
        };
        assert!(describe_instrument(&inst).contains("unresolved"));
        inst.mcap_cr = Some(1234.0);
        assert!(describe_instrument(&inst).contains("1234"));
    }

    fn scan_row(symbol: &str, conviction: f64, score: f64, change: f64) -> ScanDisplayRow {
        ScanDisplayRow {
            key: format!("NSE_EQ|{symbol}"),
            symbol: symbol.into(),
            name: format!("{symbol} Ltd"),
            date: NaiveDate::from_ymd_opt(2026, 8, 7).unwrap(),
            pattern: "Hammer",
            direction: Direction::Bullish,
            score,
            detail: String::new(),
            change_pct: change,
            volume_ratio: 1.0,
            rsi: 50.0,
            below_52w: 0.1,
            conviction,
            agreement: (1, 0),
        }
    }

    #[test]
    fn conviction_sort_ranks_by_magnitude_not_sign() {
        // A strong bearish cluster is as interesting as a strong bullish one.
        let mut rows = vec![
            scan_row("MILD", 0.10, 0.5, 1.0),
            scan_row("BEAR", -0.80, 0.5, 1.0),
            scan_row("BULL", 0.60, 0.5, 1.0),
        ];
        sort_rows(&mut rows, SortKey::Conviction, true);
        assert_eq!(
            rows.iter().map(|r| r.symbol.as_str()).collect::<Vec<_>>(),
            ["BEAR", "BULL", "MILD"]
        );
    }

    #[test]
    fn sort_direction_flips() {
        let mut rows = vec![scan_row("A", 0.1, 0.9, 0.0), scan_row("B", 0.2, 0.3, 0.0)];
        sort_rows(&mut rows, SortKey::Score, true);
        assert_eq!(rows[0].symbol, "A");
        sort_rows(&mut rows, SortKey::Score, false);
        assert_eq!(rows[0].symbol, "B");
    }

    #[test]
    fn ties_break_on_symbol_so_the_table_is_stable() {
        let mut rows = vec![
            scan_row("ZED", 0.5, 0.5, 0.0),
            scan_row("ACE", 0.5, 0.5, 0.0),
            scan_row("MID", 0.5, 0.5, 0.0),
        ];
        sort_rows(&mut rows, SortKey::Conviction, true);
        assert_eq!(
            rows.iter().map(|r| r.symbol.as_str()).collect::<Vec<_>>(),
            ["ACE", "MID", "ZED"]
        );
    }

    #[test]
    fn change_sort_puts_biggest_gainers_first() {
        let mut rows = vec![
            scan_row("FLAT", 0.5, 0.5, 0.2),
            scan_row("UP", 0.5, 0.5, 9.4),
            scan_row("DOWN", 0.5, 0.5, -6.1),
        ];
        sort_rows(&mut rows, SortKey::Change, true);
        assert_eq!(rows[0].symbol, "UP");
        assert_eq!(rows[2].symbol, "DOWN");
    }

    #[test]
    fn csv_quoting_escapes_embedded_quotes() {
        assert_eq!(csv_quote("plain"), "\"plain\"");
        assert_eq!(csv_quote(r#"say "hi""#), r#""say ""hi""""#);
        assert_eq!(csv_quote("a,b"), "\"a,b\"");
    }

    #[test]
    fn scanner_cutoff_moves_backwards() {
        let today = NaiveDate::from_ymd_opt(2025, 8, 10).unwrap();
        assert_eq!(scanner_cutoff(today, 5), NaiveDate::from_ymd_opt(2025, 8, 5).unwrap());
        assert_eq!(scanner_cutoff(today, 0), today);
    }
}
