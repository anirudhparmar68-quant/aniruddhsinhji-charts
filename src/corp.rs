//! Splits, bonuses and other corporate actions, from NSE's own MCP service.
//!
//! Both price sources the app uses, Upstox and the exchange bhavcopy, are *raw*.
//! The day a company splits its shares or issues a bonus, the price falls by the
//! ratio and nothing in the data says why: a 1:1 bonus is a one-day fall of 50%
//! that never happened to anyone's money. On a chart it is a cliff, and the cup
//! scan reads it as a deep dip.
//!
//! NSE's MCP service (https://www.nseindia.com/nse-mcp, no login, no key) lists
//! those events with the factor to correct the earlier prices by. Three rules
//! shape this module:
//!
//! * The database keeps the raw prices. Adjustments are applied in memory when the
//!   data is loaded, so nothing is ever destroyed and an adjustment can be turned
//!   off again.
//! * An event is applied only if the data actually shows the jump it describes. A
//!   mis-dated event, or a price series that was already adjusted, is skipped
//!   instead of being corrected twice.
//! * A demerger or a rights issue has no factor in NSE's data, so it is flagged on
//!   the chart and not adjusted. Guessing a ratio would be inventing data.
//!
//! NSE describes the data as informational and educational, not for commercial
//! use or for training AI. The app is free, and each copy asks NSE itself for the
//! few thousand small answers it needs; nothing from NSE is redistributed.

use crate::model::Candle;
use anyhow::{bail, Context, Result};
use chrono::NaiveDate;
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::Duration;

pub const NSE_MCP_URL: &str = "https://mcp.nseindia.in/bhavcopy/cm/mcp";

/// A one-day move smaller than this is not worth flagging as an event.
const FLAG_BELOW: f64 = 0.90;
const FLAG_ABOVE: f64 = 1.10;

/// How many bars before an ex-date `first_raw_bar` looks for the seam between
/// corrected and raw prices. The raw stretch is the exchanges' last ten calendar
/// days, about seven sessions, so a dozen leaves room.
const SEAM_WINDOW: usize = 12;
/// The largest factor whose seam is looked for: 0.6, so a 1:1 bonus and bigger.
const SEAM_MAX_FACTOR: f64 = 0.6;
/// How far from the expected size a seam's step may be, to allow for the stock's
/// own move on that day.
const SEAM_BAND_LOW: f64 = 0.75;
const SEAM_BAND_HIGH: f64 = 1.25;

// ---------------------------------------------------------------------------
// What an action is
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Split,
    Bonus,
    Demerger,
    Rights,
    Other,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Split => "SPLIT",
            Kind::Bonus => "BONUS",
            Kind::Demerger => "DEMERGER",
            Kind::Rights => "RIGHTS",
            Kind::Other => "OTHER",
        }
    }

    pub fn parse(s: &str) -> Kind {
        match s {
            "SPLIT" => Kind::Split,
            "BONUS" => Kind::Bonus,
            "DEMERGER" => Kind::Demerger,
            "RIGHTS" => Kind::Rights,
            _ => Kind::Other,
        }
    }
}

/// One corporate action, as NSE lists it.
#[derive(Debug, Clone, PartialEq)]
pub struct Action {
    pub ex_date: NaiveDate,
    pub kind: Kind,
    /// NSE's own wording, e.g. "Bonus 1:1".
    pub purpose: String,
    /// What to multiply every earlier price by. 0.5 for a 1:1 bonus. 1.0 means
    /// NSE gives no factor (a demerger, a dividend).
    pub factor: f64,
}

/// Something worth drawing on a chart at an ex-date.
#[derive(Debug, Clone, PartialEq)]
pub struct Event {
    pub date: NaiveDate,
    pub label: String,
    /// The earlier prices were corrected for it. False for an event that is only
    /// flagged because NSE gives no factor for it.
    pub adjusted: bool,
}

/// Whether a factor is one to apply: a real number, not 1, and not absurd.
pub fn adjusts(factor: f64) -> bool {
    factor.is_finite() && (factor - 1.0).abs() > 1e-6 && (0.01..=100.0).contains(&factor)
}

// ---------------------------------------------------------------------------
// Reading NSE's reply
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct RawReply {
    #[serde(default)]
    actions: Vec<RawAction>,
}

#[derive(Deserialize)]
struct RawAction {
    #[serde(rename = "exDate", default)]
    ex_date: String,
    #[serde(rename = "actionType", default)]
    action_type: Option<String>,
    #[serde(default)]
    purpose: Option<String>,
    #[serde(rename = "adjustmentFactor", default)]
    adjustment_factor: Option<f64>,
}

/// Turn the JSON text of a `get_corporate_actions` answer into the actions worth
/// keeping: splits and bonuses with a factor, and demergers and rights issues to
/// flag. Dividends, buy-backs and meetings are dropped.
pub fn parse_actions(text: &str) -> Result<Vec<Action>> {
    let reply: RawReply =
        serde_json::from_str(text).context("reading NSE's corporate-actions answer")?;
    let mut out: Vec<Action> = Vec::new();
    for raw in reply.actions {
        let Ok(ex_date) = NaiveDate::parse_from_str(raw.ex_date.trim(), "%Y-%m-%d") else {
            continue;
        };
        let factor = raw.adjustment_factor.unwrap_or(1.0);
        let purpose = raw.purpose.unwrap_or_default().trim().to_string();
        let Some(kind) = classify(raw.action_type.as_deref().unwrap_or(""), &purpose, factor)
        else {
            continue;
        };
        let action = Action { ex_date, kind, purpose, factor };
        if !out.contains(&action) {
            out.push(action);
        }
    }
    out.sort_by_key(|a| a.ex_date);
    Ok(out)
}

fn classify(action_type: &str, purpose: &str, factor: f64) -> Option<Kind> {
    let kind = action_type.trim().to_ascii_uppercase();
    let wording = purpose.to_ascii_lowercase();
    if wording.contains("demerger") {
        return Some(Kind::Demerger);
    }
    if wording.contains("rights") {
        return Some(Kind::Rights);
    }
    match kind.as_str() {
        "SPLIT" => Some(Kind::Split),
        "BONUS" => Some(Kind::Bonus),
        "DIVIDEND" => None,
        _ if adjusts(factor) => Some(Kind::Other),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Applying them to a price series
// ---------------------------------------------------------------------------

/// Whether the move across an ex-date is the one the factor describes: nearer to
/// the factor than to "nothing happened", and not wildly off it. This is what
/// stops a series that is already adjusted, or an event with the wrong date, from
/// being corrected a second time.
fn showing_the_jump(ratio: f64, factor: f64) -> bool {
    let (moved, expected) = (ratio.ln(), factor.ln());
    (moved - expected).abs() < moved.abs() && (0.65..=1.55).contains(&(ratio / factor))
}

fn trim_number(x: f64) -> String {
    if (x - x.round()).abs() < 0.05 {
        format!("{}", x.round() as i64)
    } else {
        format!("{x:.1}")
    }
}

fn ratio_label(name: &str, factor: f64) -> String {
    if factor < 1.0 {
        format!("{name} 1:{}", trim_number(1.0 / factor))
    } else {
        format!("{name} {}:1", trim_number(factor))
    }
}

/// A short name for a chart marker.
pub fn label(action: &Action) -> String {
    match action.kind {
        Kind::Bonus if !action.purpose.is_empty() => action.purpose.chars().take(40).collect(),
        Kind::Bonus => ratio_label("Bonus", action.factor),
        Kind::Split => ratio_label("Split", action.factor),
        Kind::Demerger => "Demerger".to_string(),
        Kind::Rights => "Rights issue".to_string(),
        Kind::Other => ratio_label("Adjustment", action.factor),
    }
}

/// Correct `series` (ascending by date, raw prices) for the splits and bonuses in
/// `actions`, in place, and return the events to draw.
///
/// Every bar before an ex-date has its prices multiplied by the factor and its
/// volume divided by it, so a 1:1 bonus halves the old prices and doubles the old
/// volumes: the shape of the chart is kept and the cliff goes. Several events
/// compound. Bars from the ex-date on are never touched.
pub fn apply(series: &mut [Candle], actions: &[Action]) -> Vec<Event> {
    let mut events = Vec::new();
    if series.len() < 2 || actions.is_empty() {
        return events;
    }

    // Actions that share an ex-date move the price together (a split and a bonus
    // on the same day), so they are judged against the price move as one.
    let mut sorted: Vec<&Action> = actions.iter().collect();
    sorted.sort_by_key(|a| a.ex_date);
    let mut by_bar: Vec<(usize, Vec<&Action>)> = Vec::new();
    for action in sorted {
        // The first bar on or after the ex-date. An ex-date before the data starts
        // has nothing to adjust, and one after it has not happened yet.
        let idx = series.partition_point(|c| c.date < action.ex_date);
        if idx == 0 || idx >= series.len() {
            continue;
        }
        match by_bar.last_mut() {
            Some((last, group)) if *last == idx => group.push(action),
            _ => by_bar.push((idx, vec![action])),
        }
    }

    // First decide which events the data actually shows; each applied one then
    // corrects the raw bars [first, idx), worked out below.
    let mut applied: Vec<(usize, f64)> = Vec::new();
    for (idx, group) in by_bar {
        let (before, after) = (series[idx - 1].close, series[idx].close);
        if !(before > 0.0 && after > 0.0) {
            continue;
        }
        let ratio = after / before;
        let date = series[idx].date;

        let adjusting: Vec<&&Action> = group.iter().filter(|a| adjusts(a.factor)).collect();
        if !adjusting.is_empty() {
            let factor: f64 = adjusting.iter().map(|a| a.factor).product();
            if showing_the_jump(ratio, factor) {
                applied.push((idx, factor));
                let name = adjusting.iter().map(|a| label(a)).collect::<Vec<_>>().join(" + ");
                events.push(Event { date, label: name, adjusted: true });
            }
        } else if ratio <= FLAG_BELOW || ratio >= FLAG_ABOVE {
            if let Some(action) =
                group.iter().find(|a| matches!(a.kind, Kind::Demerger | Kind::Rights))
            {
                events.push(Event { date, label: label(action), adjusted: false });
            }
        }
    }

    // Compound every event onto the bars it corrects, then apply once. Events a
    // few bars apart share one raw stretch and so one seam, which carries the
    // combined step of all of them.
    let mut scale = vec![1.0_f64; series.len()];
    for &(idx, factor) in &applied {
        let neighbours: f64 = applied
            .iter()
            .filter(|&&(other, _)| other != idx && other.abs_diff(idx) <= SEAM_WINDOW)
            .map(|&(_, f)| f)
            .product();
        let first = first_raw_bar(series, idx, factor, neighbours);
        for s in &mut scale[first..idx] {
            *s *= factor;
        }
    }
    for (bar, s) in series.iter_mut().zip(scale) {
        if s != 1.0 {
            bar.open *= s;
            bar.high *= s;
            bar.low *= s;
            bar.close *= s;
            bar.volume = (bar.volume as f64 / s).round() as i64;
        }
    }
    events
}

/// Bars before an ex-date that are still raw begin here.
///
/// The prices are a patchwork. Upstox's history is already corrected for many
/// splits and bonuses, while the last ten or so days come from the exchanges'
/// bhavcopy, which never is. A bonus whose ex-date falls in that recent stretch
/// therefore leaves *two* jumps: the cliff at the ex-date, and an upward step a few
/// bars earlier where Upstox's corrected bars end and the raw bars begin. Only the
/// bars between the two are raw; the older ones are already right, and correcting
/// them again would halve a year of history a second time.
///
/// This looks a short way back from the ex-date for that upward step (the mirror
/// image of the factor) and returns the bar it lands on. With none, the history is
/// raw all the way and the answer is the first bar.
///
/// Be strict about what counts as the step, because a wrong answer corrects too
/// little and leaves a false cliff. Only a large factor (a 1:1 bonus or bigger) has
/// a step an ordinary day cannot imitate, so milder ones are never searched for: a
/// seam for a 1:10 bonus is a +10% day, and stocks have those all the time. The
/// step must also land within a quarter of the expected size (a little more on the low side). `neighbours` is the
/// product of the other events in the same raw stretch, whose steps stack on this
/// one's.
fn first_raw_bar(series: &[Candle], idx: usize, factor: f64, neighbours: f64) -> usize {
    let mirrors: Vec<f64> = [factor, factor * neighbours]
        .into_iter()
        .filter(|&f| f <= SEAM_MAX_FACTOR)
        .map(|f| 1.0 / f)
        .collect();
    let mut best: Option<(usize, f64)> = None;
    for j in idx.saturating_sub(SEAM_WINDOW).max(1)..idx {
        let (earlier, later) = (series[j - 1].close, series[j].close);
        if !(earlier > 0.0 && later > 0.0) {
            continue;
        }
        let step = later / earlier;
        for mirror in &mirrors {
            let off = step / mirror;
            if (SEAM_BAND_LOW..=SEAM_BAND_HIGH).contains(&off) {
                let miss = off.ln().abs();
                if best.map_or(true, |(_, m)| miss < m) {
                    best = Some((j, miss));
                }
            }
        }
    }
    best.map_or(0, |(j, _)| j)
}

// ---------------------------------------------------------------------------
// Asking NSE
// ---------------------------------------------------------------------------

/// A client for NSE's MCP service. It speaks the protocol's JSON-RPC over HTTP
/// directly: one `initialize` handshake, then `tools/call`.
pub struct NseMcp {
    client: reqwest::Client,
    url: String,
    session: tokio::sync::Mutex<Option<String>>,
    next_id: AtomicU64,
    /// Attempts that failed in a row. At `BREAKER` the service is treated as down
    /// and every further question fails at once, so one that hangs costs about a
    /// minute instead of tying up a sync for a quarter of an hour.
    failures: AtomicU32,
    /// The pause before the first retry; it doubles for each one after.
    pause: Duration,
}

/// Failed attempts in a row after which the service is given up on.
const BREAKER: u32 = 6;

impl NseMcp {
    pub fn new(client: reqwest::Client) -> Self {
        Self::with_url(client, NSE_MCP_URL)
    }

    pub fn with_url(client: reqwest::Client, url: &str) -> Self {
        Self {
            client,
            url: url.to_string(),
            session: tokio::sync::Mutex::new(None),
            next_id: AtomicU64::new(1),
            failures: AtomicU32::new(0),
            pause: Duration::from_secs(1),
        }
    }

    /// Shorten the pause between retries (for tests).
    pub fn with_pause(mut self, pause: Duration) -> Self {
        self.pause = pause;
        self
    }

    /// Whether so many attempts in a row have failed that the service is being
    /// left alone.
    pub fn is_down(&self) -> bool {
        self.failures.load(Ordering::Relaxed) >= BREAKER
    }

    /// The splits, bonuses and flagged events NSE lists for one symbol. An unknown
    /// symbol is not an error: NSE answers it with an empty list.
    pub async fn actions(&self, symbol: &str) -> Result<Vec<Action>> {
        let mut last = None;
        for attempt in 0..3u32 {
            if self.is_down() {
                bail!("NSE's service is not answering, so it is being left alone for now");
            }
            if attempt > 0 {
                tokio::time::sleep(self.pause * (1u32 << attempt)).await;
            }
            match self.call_once(symbol).await {
                Ok(actions) => {
                    self.failures.store(0, Ordering::Relaxed);
                    return Ok(actions);
                }
                Err(e) => {
                    self.failures.fetch_add(1, Ordering::Relaxed);
                    // A fresh session on the next try: the usual reason for a
                    // failure here is one that expired.
                    *self.session.lock().await = None;
                    last = Some(e);
                }
            }
        }
        Err(last.unwrap_or_else(|| anyhow::anyhow!("NSE did not answer for {symbol}")))
    }

    async fn call_once(&self, symbol: &str) -> Result<Vec<Action>> {
        let session = self.session().await?;
        let symbol = wire_symbol(symbol);
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let body = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "tools/call",
            "params": { "name": "get_corporate_actions", "arguments": { "symbol": symbol } }
        });
        let (_, raw) = self.post(&body, Some(&session)).await?;
        let message = parse_message(&raw)?;
        if let Some(error) = message.get("error") {
            bail!("NSE refused the request: {error}");
        }
        let result = message.get("result").context("NSE's answer had no result")?;
        let text: String = result
            .get("content")
            .and_then(Value::as_array)
            .map(|parts| parts.iter().filter_map(|p| p.get("text").and_then(Value::as_str)).collect())
            .unwrap_or_default();
        if result.get("isError").and_then(Value::as_bool).unwrap_or(false) {
            bail!("NSE could not answer for {symbol}: {text}");
        }
        parse_actions(&text)
    }

    /// The session id, shaking hands with the server the first time.
    async fn session(&self) -> Result<String> {
        let mut held = self.session.lock().await;
        if let Some(id) = held.as_ref() {
            return Ok(id.clone());
        }
        let init = json!({
            "jsonrpc": "2.0",
            "id": self.next_id.fetch_add(1, Ordering::Relaxed),
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-03-26",
                "capabilities": {},
                "clientInfo": { "name": "spider-charts", "version": env!("CARGO_PKG_VERSION") }
            }
        });
        let (id, _) = self.post(&init, None).await?;
        let id = id.context("NSE did not start a session")?;
        let hello = json!({ "jsonrpc": "2.0", "method": "notifications/initialized" });
        self.post(&hello, Some(&id)).await?;
        *held = Some(id.clone());
        Ok(id)
    }

    async fn post(&self, body: &Value, session: Option<&str>) -> Result<(Option<String>, String)> {
        let mut request = self
            .client
            .post(&self.url)
            .header("Accept", "application/json, text/event-stream")
            .timeout(Duration::from_secs(15))
            .json(body);
        if let Some(id) = session {
            request = request.header("Mcp-Session-Id", id);
        }
        let response = request.send().await.context("reaching NSE")?;
        let status = response.status();
        let session_id = response
            .headers()
            .get("mcp-session-id")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let text = response.text().await.context("reading NSE's answer")?;
        if !status.is_success() {
            let brief: String = text.chars().take(160).collect();
            bail!("NSE answered {status}: {brief}");
        }
        Ok((session_id, text))
    }
}

/// The symbol as NSE's service wants to receive it. It passes the symbol on to
/// NSE's own site without encoding it, so an ampersand (M&M, M&MFIN) would end the
/// query early and come back empty; written as %26 it works.
fn wire_symbol(symbol: &str) -> String {
    symbol.trim().to_ascii_uppercase().replace('&', "%26")
}

/// An MCP answer is either plain JSON or one server-sent event whose `data:` line
/// carries the JSON.
pub fn parse_message(raw: &str) -> Result<Value> {
    let trimmed = raw.trim();
    if trimmed.starts_with('{') {
        return serde_json::from_str(trimmed).context("reading NSE's answer");
    }
    for line in trimmed.lines() {
        if let Some(data) = line.strip_prefix("data:") {
            return serde_json::from_str(data.trim()).context("reading NSE's answer");
        }
    }
    bail!("NSE's answer was in a form this app does not read");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn day(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).expect("valid date")
    }

    fn bar(date: NaiveDate, close: f64, volume: i64) -> Candle {
        Candle { date, open: close, high: close * 1.01, low: close * 0.99, close, volume }
    }

    fn action(ex: NaiveDate, kind: Kind, purpose: &str, factor: f64) -> Action {
        Action { ex_date: ex, kind, purpose: purpose.to_string(), factor }
    }

    /// Six sessions of a stock that issued a 1:1 bonus on the fifth: the raw price
    /// halves overnight, exactly as the exchanges' files record it.
    fn bonus_series() -> Vec<Candle> {
        vec![
            bar(day(2026, 9, 22), 100.0, 1000),
            bar(day(2026, 9, 23), 102.0, 1100),
            bar(day(2026, 9, 24), 98.0, 900),
            bar(day(2026, 9, 25), 100.0, 1000),
            bar(day(2026, 9, 28), 50.0, 2000),
            bar(day(2026, 9, 29), 51.0, 2100),
        ]
    }

    fn bonus() -> Action {
        action(day(2026, 9, 28), Kind::Bonus, "Bonus 1:1", 0.5)
    }

    #[test]
    fn a_bonus_halves_every_earlier_bar_and_doubles_its_volume() {
        let mut series = bonus_series();
        let events = apply(&mut series, &[bonus()]);

        let closes: Vec<f64> = series.iter().map(|c| c.close).collect();
        assert_eq!(closes, vec![50.0, 51.0, 49.0, 50.0, 50.0, 51.0], "the cliff is gone");
        let volumes: Vec<i64> = series.iter().map(|c| c.volume).collect();
        assert_eq!(volumes, vec![2000, 2200, 1800, 2000, 2000, 2100]);
        assert!((series[0].high - 50.5).abs() < 1e-9, "highs and lows scale with the close");

        assert_eq!(events.len(), 1);
        assert_eq!(events[0].date, day(2026, 9, 28));
        assert_eq!(events[0].label, "Bonus 1:1");
        assert!(events[0].adjusted);
    }

    #[test]
    fn bars_from_the_ex_date_on_are_never_touched() {
        let mut series = bonus_series();
        let before = series.clone();
        apply(&mut series, &[bonus()]);
        assert_eq!(series[4..], before[4..]);
    }

    #[test]
    fn applying_to_freshly_loaded_data_is_repeatable() {
        // The database is never changed, so every load starts from raw prices and
        // must land in the same place.
        let (mut a, mut b) = (bonus_series(), bonus_series());
        apply(&mut a, &[bonus()]);
        apply(&mut b, &[bonus()]);
        assert_eq!(a, b);
    }

    #[test]
    fn two_events_compound() {
        let mut series = vec![
            bar(day(2026, 1, 5), 400.0, 100),
            bar(day(2026, 1, 6), 400.0, 100),
            bar(day(2026, 2, 2), 200.0, 200),
            bar(day(2026, 2, 3), 200.0, 200),
            bar(day(2026, 3, 2), 100.0, 400),
            bar(day(2026, 3, 3), 100.0, 400),
        ];
        let events = apply(
            &mut series,
            &[
                action(day(2026, 2, 2), Kind::Bonus, "Bonus 1:1", 0.5),
                action(day(2026, 3, 2), Kind::Split, "Split", 0.5),
            ],
        );
        let closes: Vec<f64> = series.iter().map(|c| c.close).collect();
        assert_eq!(closes, vec![100.0, 100.0, 100.0, 100.0, 100.0, 100.0]);
        assert_eq!(series[0].volume, 400);
        assert_eq!(events.len(), 2);
    }

    #[test]
    fn a_split_and_a_bonus_on_the_same_day_are_judged_together() {
        // 1:1 bonus and a 1:5 split together: the price falls to a tenth.
        let mut series = vec![
            bar(day(2026, 5, 4), 1000.0, 10),
            bar(day(2026, 5, 5), 100.0, 100),
            bar(day(2026, 5, 6), 101.0, 100),
        ];
        let events = apply(
            &mut series,
            &[
                action(day(2026, 5, 5), Kind::Bonus, "Bonus 1:1", 0.5),
                action(day(2026, 5, 5), Kind::Split, "Split", 0.2),
            ],
        );
        assert!((series[0].close - 100.0).abs() < 1e-9);
        assert_eq!(events.len(), 1, "one ex-date, one marker");
        assert!(events[0].label.contains('+'));
    }

    /// What a real stock looked like on 3 Oct 2026 (AASTHA, 1:1 bonus ex 28 Sep):
    /// Upstox's history was already corrected, the exchanges' last few days were
    /// raw. Closes are the real ones; the older bars sit at the corrected level.
    fn patchwork_series() -> Vec<Candle> {
        vec![
            bar(day(2026, 9, 17), 38.39, 446_704),
            bar(day(2026, 9, 18), 38.12, 480_496),
            bar(day(2026, 9, 21), 37.10, 486_392),
            bar(day(2026, 9, 22), 40.81, 2_669_652),
            bar(day(2026, 9, 23), 44.88, 5_969_480),
            // The seam: the exchanges' raw bars start here, at the old price.
            bar(day(2026, 9, 24), 80.80, 3_696_534),
            bar(day(2026, 9, 25), 72.72, 4_769_681),
            // The ex-date.
            bar(day(2026, 9, 28), 39.30, 2_200_477),
            bar(day(2026, 9, 29), 37.53, 689_221),
        ]
    }

    #[test]
    fn history_that_is_already_corrected_is_not_corrected_twice() {
        let before = patchwork_series();
        let mut series = patchwork_series();
        let events = apply(&mut series, &[bonus()]);

        // Only the two raw bars are halved. Everything older was already right.
        assert_eq!(series[..5], before[..5], "the corrected older bars must be left alone");
        assert!((series[5].close - 40.40).abs() < 1e-9);
        assert!((series[6].close - 36.36).abs() < 1e-9);
        assert_eq!(series[5].volume, 7_393_068, "volume of a raw bar is doubled");
        assert_eq!(series[7..], before[7..]);
        assert_eq!(events.len(), 1, "one event, drawn once");

        // And the result has no cliff left anywhere.
        let worst = series.windows(2).map(|w| w[1].close / w[0].close).fold(f64::MAX, f64::min);
        assert!(worst > 0.85, "no one-day fall bigger than a normal move remains, got {worst}");
    }

    #[test]
    fn a_fully_raw_history_is_corrected_all_the_way_back() {
        // No seam anywhere: every bar before the ex-date is raw.
        let mut series = bonus_series();
        apply(&mut series, &[bonus()]);
        assert_eq!(series[0].close, 50.0);
    }

    #[test]
    fn an_ordinary_rally_is_not_mistaken_for_the_seam() {
        // +30% a few bars before the ex-date is not the 2x step of a 1:1 bonus's seam.
        let mut series = vec![
            bar(day(2026, 9, 21), 100.0, 100),
            bar(day(2026, 9, 22), 130.0, 100),
            bar(day(2026, 9, 23), 135.0, 100),
            bar(day(2026, 9, 28), 66.0, 200),
        ];
        apply(&mut series, &[bonus()]);
        assert_eq!(series[0].close, 50.0, "the whole raw history is halved");
    }

    #[test]
    fn a_mild_bonus_after_a_rally_is_still_corrected_all_the_way_back() {
        // A 1:10 bonus (factor 0.909) in a history that is raw all the way, after a
        // +6% day. That day is an ordinary rally, not a seam: a mild bonus's seam
        // cannot be told from one, so it is never searched for.
        let mut series = vec![
            bar(day(2026, 9, 14), 100.0, 100),
            bar(day(2026, 9, 15), 100.0, 100),
            bar(day(2026, 9, 16), 106.0, 100),
            bar(day(2026, 9, 17), 106.0, 100),
            bar(day(2026, 9, 18), 106.0, 100),
            bar(day(2026, 9, 21), 96.36, 110),
            bar(day(2026, 9, 22), 96.5, 110),
        ];
        let ex = action(day(2026, 9, 21), Kind::Bonus, "Bonus 1:10", 1.0 / 1.1);
        apply(&mut series, &[ex]);
        assert!((series[0].close - 90.909).abs() < 0.01, "the oldest bar is corrected too");
        assert!((series[2].close - 96.36).abs() < 0.01);
    }

    #[test]
    fn a_seam_is_still_found_when_the_stock_fell_on_the_seam_day() {
        // The raw bars start with a 15% fall on top of the 2x seam: step 1.7.
        let mut series = vec![
            bar(day(2026, 9, 14), 40.0, 100),
            bar(day(2026, 9, 15), 41.0, 100),
            bar(day(2026, 9, 16), 69.7, 100), // 41 * 2 * 0.85
            bar(day(2026, 9, 17), 70.0, 100),
            bar(day(2026, 9, 18), 35.0, 200), // ex-date
            bar(day(2026, 9, 21), 35.5, 200),
        ];
        let ex = action(day(2026, 9, 18), Kind::Bonus, "Bonus 1:1", 0.5);
        apply(&mut series, &[ex]);
        assert_eq!(series[1].close, 41.0, "the corrected older bars are left alone");
        assert!((series[2].close - 34.85).abs() < 1e-9 && (series[3].close - 35.0).abs() < 1e-9);
    }

    #[test]
    fn a_step_that_is_not_the_size_of_the_seam_is_not_taken_for_one() {
        // A 1:1 bonus; a +40% day four bars earlier is well short of the 2x a seam has.
        let mut series = vec![
            bar(day(2026, 9, 14), 100.0, 100),
            bar(day(2026, 9, 15), 140.0, 100),
            bar(day(2026, 9, 16), 140.0, 100),
            bar(day(2026, 9, 17), 140.0, 100),
            bar(day(2026, 9, 18), 70.0, 200),
        ];
        apply(&mut series, &[action(day(2026, 9, 18), Kind::Bonus, "Bonus 1:1", 0.5)]);
        assert_eq!(series[0].close, 50.0, "no seam, so the whole history is raw");
    }

    #[test]
    fn two_events_in_one_raw_stretch_share_one_seam() {
        // Upstox's corrected bars (10), then the exchanges' raw ones at the level
        // from before both events (40), a 1:1 bonus, then a 1:1 split. The seam's
        // step is 4x, the product of the two.
        let mut series = vec![
            bar(day(2026, 9, 1), 10.0, 400),
            bar(day(2026, 9, 2), 10.0, 400),
            bar(day(2026, 9, 3), 10.0, 400),
            bar(day(2026, 9, 4), 40.0, 100),
            bar(day(2026, 9, 7), 40.0, 100),
            bar(day(2026, 9, 8), 20.0, 200), // bonus ex-date
            bar(day(2026, 9, 9), 20.0, 200),
            bar(day(2026, 9, 10), 10.0, 400), // split ex-date
            bar(day(2026, 9, 11), 10.0, 400),
        ];
        let events = apply(
            &mut series,
            &[
                action(day(2026, 9, 8), Kind::Bonus, "Bonus 1:1", 0.5),
                action(day(2026, 9, 10), Kind::Split, "Split", 0.5),
            ],
        );
        let closes: Vec<f64> = series.iter().map(|c| c.close).collect();
        assert_eq!(closes, vec![10.0; 9], "every bar ends on the same footing");
        assert_eq!(events.len(), 2);
    }

    #[tokio::test]
    async fn a_service_that_does_not_answer_is_given_up_on_quickly() {
        // Nothing listens on port 9, so every attempt is refused.
        let nse = NseMcp::with_url(reqwest::Client::new(), "http://127.0.0.1:9/mcp")
            .with_pause(Duration::from_millis(1));
        assert!(nse.actions("TCS").await.is_err()); // three attempts
        assert!(!nse.is_down());
        assert!(nse.actions("INFY").await.is_err()); // three more: now six in a row
        assert!(nse.is_down());

        let started = std::time::Instant::now();
        assert!(nse.actions("WIPRO").await.is_err());
        assert!(
            started.elapsed() < Duration::from_millis(100),
            "once it is given up on, a question fails at once"
        );
    }

    #[test]
    fn data_that_already_shows_no_jump_is_left_alone() {
        // The event is real but this series is already adjusted (or the event is
        // mis-dated): correcting it would halve the history a second time.
        let mut series = vec![
            bar(day(2026, 9, 25), 50.0, 2000),
            bar(day(2026, 9, 28), 50.5, 2000),
            bar(day(2026, 9, 29), 51.0, 2100),
        ];
        let before = series.clone();
        let events = apply(&mut series, &[bonus()]);
        assert_eq!(series, before);
        assert!(events.is_empty());
    }

    #[test]
    fn an_event_before_the_data_or_after_it_changes_nothing() {
        let mut series = bonus_series();
        let before = series.clone();
        let events = apply(
            &mut series,
            &[
                action(day(2026, 1, 1), Kind::Bonus, "Bonus 1:1", 0.5),
                action(day(2026, 12, 1), Kind::Bonus, "Bonus 1:1", 0.5),
            ],
        );
        assert_eq!(series, before);
        assert!(events.is_empty());
    }

    #[test]
    fn an_ex_date_on_a_holiday_lands_on_the_next_session() {
        // Ex-date Saturday 27 Sep: the first bar at or after it is Monday 28th.
        let mut series = bonus_series();
        let events = apply(&mut series, &[action(day(2026, 9, 27), Kind::Bonus, "Bonus 1:1", 0.5)]);
        assert_eq!(series[0].close, 50.0);
        assert_eq!(events[0].date, day(2026, 9, 28));
    }

    #[test]
    fn a_reverse_split_scales_the_old_prices_up() {
        let mut series = vec![
            bar(day(2026, 6, 1), 5.0, 1000),
            bar(day(2026, 6, 2), 50.0, 100),
            bar(day(2026, 6, 3), 51.0, 100),
        ];
        let events = apply(&mut series, &[action(day(2026, 6, 2), Kind::Split, "Consolidation", 10.0)]);
        assert!((series[0].close - 50.0).abs() < 1e-9);
        assert_eq!(series[0].volume, 100);
        assert_eq!(events[0].label, "Split 10:1");
    }

    #[test]
    fn a_demerger_is_flagged_but_never_adjusted() {
        let mut series = vec![
            bar(day(2026, 4, 29), 773.0, 1000),
            bar(day(2026, 4, 30), 271.0, 3000),
            bar(day(2026, 5, 4), 275.0, 2000),
        ];
        let before = series.clone();
        let events = apply(&mut series, &[action(day(2026, 4, 30), Kind::Demerger, "Demerger", 1.0)]);
        assert_eq!(series, before, "NSE gives no ratio, so no price is touched");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].label, "Demerger");
        assert!(!events[0].adjusted);
    }

    #[test]
    fn a_demerger_with_no_price_move_is_not_drawn() {
        let mut series = vec![
            bar(day(2026, 4, 29), 100.0, 1000),
            bar(day(2026, 4, 30), 99.0, 1000),
            bar(day(2026, 5, 4), 100.0, 1000),
        ];
        let events = apply(&mut series, &[action(day(2026, 4, 30), Kind::Demerger, "Demerger", 1.0)]);
        assert!(events.is_empty());
    }

    #[test]
    fn nonsense_prices_do_not_panic() {
        let mut series = vec![
            bar(day(2026, 9, 25), 0.0, 10),
            bar(day(2026, 9, 28), 50.0, 10),
            bar(day(2026, 9, 29), f64::NAN, 10),
        ];
        let events = apply(&mut series, &[bonus()]);
        assert!(events.is_empty());
        assert!(apply(&mut [], &[bonus()]).is_empty());
        assert!(apply(&mut series, &[]).is_empty());
    }

    #[test]
    fn the_jump_check_accepts_the_real_thing_and_rejects_its_absence() {
        assert!(showing_the_jump(0.5, 0.5));
        assert!(showing_the_jump(0.48, 0.5), "a normal move on top of the bonus");
        assert!(showing_the_jump(0.34, 0.3333));
        assert!(!showing_the_jump(1.0, 0.5), "no jump: already adjusted");
        assert!(!showing_the_jump(0.9, 0.5), "a 10% move is not a 50% bonus");
        assert!(!showing_the_jump(0.5, 0.9), "a 50% fall is not a 1:10 bonus");
    }

    #[test]
    fn factors_that_cannot_be_right_are_not_applied() {
        for bad in [1.0, 0.0, -2.0, f64::NAN, f64::INFINITY, 0.001, 5000.0] {
            assert!(!adjusts(bad), "{bad} must not be applied");
        }
        assert!(adjusts(0.5) && adjusts(0.3333) && adjusts(10.0));
    }

    #[test]
    fn labels_read_like_what_a_person_would_say() {
        assert_eq!(label(&action(day(2026, 1, 1), Kind::Split, "x", 0.1)), "Split 1:10");
        assert_eq!(label(&action(day(2026, 1, 1), Kind::Split, "x", 0.5)), "Split 1:2");
        assert_eq!(label(&action(day(2026, 1, 1), Kind::Bonus, "", 0.3333)), "Bonus 1:3");
        assert_eq!(label(&action(day(2026, 1, 1), Kind::Bonus, "Bonus 2:1", 0.3333)), "Bonus 2:1");
        assert_eq!(label(&action(day(2026, 1, 1), Kind::Rights, "Rights 1:1", 1.0)), "Rights issue");
    }

    /// A real answer, trimmed: NSE's reply for AASTHA on 3 Oct 2026.
    const AASTHA: &str = r#"{"symbol":"AASTHA","from":"2026-09-01","to":"2026-09-30","count":2,"actions":[
        {"exDate":"2026-09-09","actionType":"DIVIDEND","purpose":"Dividend - Re 0.10 Per Share","adjustmentFactor":1.0},
        {"exDate":"2026-09-28","actionType":"BONUS","purpose":"Bonus 1:1","adjustmentFactor":0.5}]}"#;

    #[test]
    fn nses_answer_keeps_the_bonus_and_drops_the_dividend() {
        let actions = parse_actions(AASTHA).unwrap();
        assert_eq!(actions, vec![action(day(2026, 9, 28), Kind::Bonus, "Bonus 1:1", 0.5)]);
    }

    #[test]
    fn demergers_and_rights_issues_are_kept_for_flagging() {
        let text = r#"{"actions":[
            {"exDate":"2026-04-30","actionType":"OTHER","purpose":"Demerger","adjustmentFactor":1.0},
            {"exDate":"2026-03-20","actionType":"OTHER","purpose":"Rights 1:1 @ Premium Rs 0/-","adjustmentFactor":1.0},
            {"exDate":"2025-11-14","actionType":"OTHER","purpose":"Buy Back","adjustmentFactor":1.0},
            {"exDate":"2024-07-26","actionType":"OTHER","purpose":"Annual General Meeting","adjustmentFactor":1.0}]}"#;
        let kinds: Vec<Kind> = parse_actions(text).unwrap().iter().map(|a| a.kind).collect();
        assert_eq!(kinds, vec![Kind::Rights, Kind::Demerger], "oldest first, buy-back and AGM dropped");
    }

    #[test]
    fn an_answer_with_gaps_in_it_does_not_break_the_rest() {
        let text = r#"{"actions":[
            {"exDate":"not a date","actionType":"BONUS","purpose":"Bonus 1:1","adjustmentFactor":0.5},
            {"exDate":"2026-09-28","actionType":null,"purpose":null,"adjustmentFactor":0.25},
            {"exDate":"2026-09-28","actionType":"BONUS","purpose":"Bonus 1:1"}]}"#;
        let actions = parse_actions(text).unwrap();
        // The bad date is skipped; an unnamed action with a factor is kept as
        // "other"; a bonus with no factor is kept too but, at 1.0, does nothing.
        let kinds: Vec<(Kind, f64)> = actions.iter().map(|a| (a.kind, a.factor)).collect();
        assert_eq!(kinds, vec![(Kind::Other, 0.25), (Kind::Bonus, 1.0)]);
        assert!(!adjusts(actions[1].factor));
        assert_eq!(parse_actions(r#"{"symbol":"X","count":0,"actions":[]}"#).unwrap(), vec![]);
        assert!(parse_actions("this is not json").is_err());
    }

    #[test]
    fn an_mcp_answer_is_read_whether_it_is_json_or_an_event() {
        let json = r#"{"jsonrpc":"2.0","id":2,"result":{"content":[{"type":"text","text":"{}"}]}}"#;
        let sse = format!("event:message\ndata:{json}\n\n");
        assert_eq!(parse_message(json).unwrap(), parse_message(&sse).unwrap());
        assert!(parse_message("<html>blocked</html>").is_err());
    }

    #[test]
    fn symbols_with_an_ampersand_are_sent_encoded() {
        assert_eq!(wire_symbol("M&M"), "M%26M");
        assert_eq!(wire_symbol(" bajaj-auto "), "BAJAJ-AUTO");
        assert_eq!(wire_symbol("TCS"), "TCS");
    }

    #[test]
    fn kinds_survive_the_trip_through_the_database_text() {
        for kind in [Kind::Split, Kind::Bonus, Kind::Demerger, Kind::Rights, Kind::Other] {
            assert_eq!(Kind::parse(kind.as_str()), kind);
        }
        assert_eq!(Kind::parse("something new"), Kind::Other);
    }

    /// Talks to NSE for real, so it is not part of a normal run:
    /// `cargo test -p spider_charts live_nse -- --ignored`
    #[tokio::test]
    #[ignore = "needs the internet and NSE's service to be up"]
    async fn live_nse_lists_the_aastha_bonus_and_the_ampersand_symbol() {
        let nse = NseMcp::new(reqwest::Client::new());
        let aastha = nse.actions("AASTHA").await.unwrap();
        assert!(aastha.iter().any(|a| a.ex_date == day(2026, 9, 28) && (a.factor - 0.5).abs() < 1e-9));
        let mm = nse.actions("M&M").await.unwrap();
        // M&M pays dividends, which are dropped, so what matters is that the
        // call itself worked for a symbol with an ampersand.
        assert!(mm.iter().all(|a| a.factor > 0.0));
    }
}
