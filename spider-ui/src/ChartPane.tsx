/** The chart half of the split view: price, volume and the scan markers.
 *
 *  The chart instance is created once and then *fed* — switching stocks calls
 *  `setData`, never a teardown. Recreating it per selection is what makes an
 *  arrow-key walk through a list feel heavy, and walking the list quickly is
 *  the whole point of this pane. */

import { Fragment, useEffect, useLayoutEffect, useRef, useState } from "react";
import {
  CandlestickSeries,
  ColorType,
  CrosshairMode,
  HistogramSeries,
  LineSeries,
  LineStyle,
  createChart,
  createSeriesMarkers,
  type IChartApi,
  type ISeriesApi,
  type ISeriesMarkersPluginApi,
  type Time,
} from "lightweight-charts";
import type { Bar, BarPoint, ChartData, CupShape, InsideBarShape } from "./api";

/** Bars visible when a stock is opened. Enough to see the base a cup formed
 *  over without the candles turning into hairlines. */
const DEFAULT_BARS = 150;

/** Empty bar-widths kept after the newest candle. Price glued to the axis is
 *  hard to read and leaves nowhere to project the next move. */
const RIGHT_OFFSET = 14;

const UP = "#2ebd85";
const DOWN = "#f6465d";

/** A short tag and colour per pattern, so two setups on one chart are never
 *  mistaken for each other. Keyed by the label the engine sends; a pattern not
 *  listed here falls back to the plain up/down colour with no tag. */
const IB = "#4c9aff";
const SOLDIERS = "#a78bfa";
const PATTERN_STYLE: Record<string, { tag: string; color: string }> = {
  "Cup n Handle": { tag: "C&H", color: UP },
  "Inside Bar Breakout": { tag: "IB", color: IB },
  "Three White Soldiers": { tag: "3WS", color: SOLDIERS },
};

/** The line series and label layers that draw one Three White Soldiers setup. */
interface SoldiersLayers {
  steps: ISeriesApi<"Line">;
  start: ISeriesApi<"Line">;
  stepLabels: ISeriesMarkersPluginApi<Time>;
  startLabels: ISeriesMarkersPluginApi<Time>;
}

/** Inside bar drawing colour for the mother bar's range; the inside bar's own
 *  lines use the IB colour. */
const MOTHER = "#f59e0b";

/** The line series and label layers that draw one inside bar breakout. */
interface InsideBarLayers {
  motherHigh: ISeriesApi<"Line">;
  motherLow: ISeriesApi<"Line">;
  insideHigh: ISeriesApi<"Line">;
  insideLow: ISeriesApi<"Line">;
  labels: ISeriesMarkersPluginApi<Time>[];
}

/** Cup drawing colours: the bowl, the neckline it broke, and the handle. */
const CUP = "#facc15";
const NECK = "#c9d1d9";
const HANDLE = "#22d3ee";

/** The line series and label layers that draw one cup. */
interface CupLayers {
  bowl: ISeriesApi<"Line">;
  neck: ISeriesApi<"Line">;
  handle: ISeriesApi<"Line">;
  bowlLabels: ISeriesMarkersPluginApi<Time>;
  neckLabels: ISeriesMarkersPluginApi<Time>;
  handleLabels: ISeriesMarkersPluginApi<Time>;
}

function fmt(n: number, dp = 2): string {
  return n.toLocaleString("en-IN", { minimumFractionDigits: dp, maximumFractionDigits: dp });
}

function fmtVol(v: number): string {
  if (v >= 1e7) return (v / 1e7).toFixed(2) + " Cr";
  if (v >= 1e5) return (v / 1e5).toFixed(2) + " L";
  if (v >= 1e3) return (v / 1e3).toFixed(1) + " K";
  return String(Math.round(v));
}

export default function ChartPane({ data }: { data: ChartData | null }) {
  const hostRef = useRef<HTMLDivElement>(null);
  const legendRef = useRef<HTMLDivElement>(null);
  const chartRef = useRef<IChartApi | null>(null);
  const priceRef = useRef<ISeriesApi<"Candlestick"> | null>(null);
  const volRef = useRef<ISeriesApi<"Histogram"> | null>(null);
  const markersRef = useRef<ISeriesMarkersPluginApi<Time> | null>(null);
  const cupRef = useRef<CupLayers | null>(null);
  const ibRef = useRef<InsideBarLayers | null>(null);
  const soldiersRef = useRef<SoldiersLayers | null>(null);
  // Kept outside React state: the crosshair handler reads it on every mouse
  // move, and a re-render per pixel is exactly the jank this pane avoids.
  const barsRef = useRef<Bar[]>([]);
  /// Which stock the viewport was last framed for. Refreshing the *same* stock
  /// must not move it — see the note in the feed effect.
  const framedKey = useRef<string | null>(null);
  /// Bar the crosshair is currently over, so a data refresh repaints the bar
  /// being pointed at rather than snapping the readout to the newest one.
  const hovered = useRef<number | null>(null);
  /// Candle the user clicked, tagged with the stock it belongs to so switching
  /// stocks drops it without an extra effect. Null shows the newest signal.
  const [clicked, setClicked] = useState<{ key: string; time: string } | null>(null);
  /// Pattern drawings are opt-in. Off, the chart is the plain candles and
  /// markers it always was. The choice is remembered on this computer.
  const [drawings, setDrawings] = useState(() => {
    try {
      return localStorage.getItem("drawings") === "on";
    } catch {
      return false;
    }
  });
  useEffect(() => {
    try {
      localStorage.setItem("drawings", drawings ? "on" : "off");
    } catch {
      // Storage unavailable: the switch still works, it just is not remembered.
    }
  }, [drawings]);

  // -- create once ---------------------------------------------------------
  useLayoutEffect(() => {
    const host = hostRef.current;
    if (!host) return;

    const chart = createChart(host, {
      // Sized explicitly rather than with `autoSize`, which waits for a
      // ResizeObserver callback: if that first callback is late or never
      // arrives the chart sits at its default 300×150 and paints nothing.
      // Measuring the host up front means the first frame is already correct.
      width: host.clientWidth,
      height: host.clientHeight,
      layout: {
        background: { type: ColorType.Solid, color: "#0b0e13" },
        textColor: "#7d8899",
        fontFamily: '"JetBrains Mono", "Cascadia Mono", Consolas, monospace',
        fontSize: 11,
        attributionLogo: false,
        panes: { separatorColor: "#212a36", separatorHoverColor: "#2b3542" },
      },
      grid: {
        vertLines: { color: "#141a22" },
        horzLines: { color: "#141a22" },
      },
      crosshair: {
        mode: CrosshairMode.Normal,
        vertLine: { color: "#4a5566", width: 1, style: LineStyle.Dashed, labelBackgroundColor: "#2b3542" },
        horzLine: { color: "#4a5566", width: 1, style: LineStyle.Dashed, labelBackgroundColor: "#2b3542" },
      },
      rightPriceScale: {
        borderColor: "#212a36",
        scaleMargins: { top: 0.1, bottom: 0.08 },
      },
      timeScale: {
        borderColor: "#212a36",
        rightOffset: RIGHT_OFFSET,
        barSpacing: 8,
        minBarSpacing: 0.6,
        fixLeftEdge: true,
        // Without this, every window resize silently rescales the bars and the
        // chart you were reading is not the chart you get back.
        lockVisibleTimeRangeOnResize: true,
      },
      // A chart-level formatter reaches every price scale in every pane, which
      // is only safe because volume is an overlay with no axis of its own.
      //
      // All three arrangements were measured on the running app. With volume
      // sharing the axis it cost 112px; moving this function onto the series as
      // `priceFormat: { type: "custom" }` made it worse at 176px, because the
      // library reserves space differently when it cannot infer the format.
      // Volume on an overlay, formatting here: 60px, or 72px for a four-figure
      // stock. That is 40–50px of candles back on every chart.
      localization: { locale: "en-IN", priceFormatter: (p: number) => fmt(p) },
      handleScroll: { mouseWheel: true, pressedMouseMove: true, horzTouchDrag: true, vertTouchDrag: false },
      handleScale: { mouseWheel: true, pinch: true, axisPressedMouseMove: true, axisDoubleClickReset: true },
    });

    const price = chart.addSeries(CandlestickSeries, {
      upColor: UP,
      downColor: DOWN,
      wickUpColor: UP,
      wickDownColor: DOWN,
      borderVisible: false,
      priceLineColor: "#5b6879",
      priceLineStyle: LineStyle.Dotted,
      priceLineWidth: 1,
      lastValueVisible: true,
    });

    // Volume lives in its own pane rather than overlaid on price: overlaying
    // costs a slice of the price range on every chart to serve a number that is
    // only ever read comparatively.
    // A named scale id makes this an *overlay*, which is never drawn as an
    // axis. That is the point: every pane shares one axis width — the widest
    // label anywhere wins — and raw NSE share counts are very wide numbers,
    // which was costing the candles about a hundred pixels on every chart.
    // Nothing is lost. Volume is read comparatively from the bars, and the
    // exact figure is already in the legend in Indian units ("Vol 1.52 Cr"),
    // which beats "1,52,00,000.00" pinned to an axis.
    //
    // Done through a series option rather than by reaching for the pane's
    // scale afterwards: that call throws here, and because it sat in the middle
    // of this effect it took the rest of the setup down with it — leaving a
    // chart that had data and drew nothing.
    const vol = chart.addSeries(
      HistogramSeries,
      {
        priceScaleId: "volume",
        priceFormat: { type: "volume" },
        priceLineVisible: false,
        lastValueVisible: false,
      },
      1,
    );

    const panes = chart.panes();
    if (panes.length > 1) panes[1].setHeight(110);

    // The cup drawing rides on three plain line series in the price pane. They
    // carry no price line, axis label or crosshair dot, so they read as a
    // drawing rather than as data.
    const overlay = { priceLineVisible: false, lastValueVisible: false, crosshairMarkerVisible: false };
    const bowl = chart.addSeries(LineSeries, { ...overlay, color: CUP, lineWidth: 2 });
    const neck = chart.addSeries(LineSeries, { ...overlay, color: NECK, lineWidth: 1, lineStyle: LineStyle.Dashed });
    const handle = chart.addSeries(LineSeries, { ...overlay, color: HANDLE, lineWidth: 2 });
    cupRef.current = {
      bowl,
      neck,
      handle,
      bowlLabels: createSeriesMarkers(bowl, []),
      neckLabels: createSeriesMarkers(neck, []),
      handleLabels: createSeriesMarkers(handle, []),
    };

    const motherHigh = chart.addSeries(LineSeries, { ...overlay, color: MOTHER, lineWidth: 2 });
    const motherLow = chart.addSeries(LineSeries, { ...overlay, color: MOTHER, lineWidth: 2 });
    const insideHigh = chart.addSeries(LineSeries, { ...overlay, color: IB, lineWidth: 2, lineStyle: LineStyle.Dashed });
    const insideLow = chart.addSeries(LineSeries, { ...overlay, color: IB, lineWidth: 1, lineStyle: LineStyle.Dotted });
    ibRef.current = {
      motherHigh,
      motherLow,
      insideHigh,
      insideLow,
      labels: [motherHigh, motherLow, insideHigh, insideLow].map((s) => createSeriesMarkers(s, [])),
    };

    const steps = chart.addSeries(LineSeries, { ...overlay, color: SOLDIERS, lineWidth: 2 });
    const start = chart.addSeries(LineSeries, { ...overlay, color: SOLDIERS, lineWidth: 1, lineStyle: LineStyle.Dashed });
    soldiersRef.current = {
      steps,
      start,
      stepLabels: createSeriesMarkers(steps, []),
      startLabels: createSeriesMarkers(start, []),
    };

    chart.subscribeCrosshairMove((param) => {
      const bars = barsRef.current;
      if (!bars.length) return;
      // Matched by logical index, not by `param.time`: the library is free to
      // normalise the time value it hands back, and a string compare that
      // silently stops matching would leave the readout frozen on stale prices.
      // Data index and logical index coincide because the series is one
      // contiguous array. Hovering the empty gap past the last candle falls
      // back to the newest bar rather than blanking the readout.
      const i = typeof param.logical === "number" ? Math.round(param.logical) : bars.length - 1;
      const bar = i >= 0 && i < bars.length ? i : bars.length - 1;
      hovered.current = typeof param.logical === "number" ? bar : null;
      paintLegend(legendRef.current, bar, bars);
    });

    // Clicking a candle pins the signal card to that candle. Clicking the empty
    // gap past the newest candle un-pins it, back to the newest signal.
    chart.subscribeClick((param) => {
      const bars = barsRef.current;
      const key = framedKey.current;
      if (!bars.length || key === null || typeof param.logical !== "number") return;
      const i = Math.round(param.logical);
      setClicked(i >= 0 && i < bars.length ? { key, time: bars[i].time } : null);
    });

    // Follows the window and the splitter. `contentRect` is used rather than
    // the device-pixel box so a fractional layout width still reports.
    const ro = new ResizeObserver(([entry]) => {
      const { width, height } = entry.contentRect;
      if (width > 0 && height > 0) chart.resize(width, height);
    });
    ro.observe(host);

    chartRef.current = chart;
    priceRef.current = price;
    volRef.current = vol;
    markersRef.current = createSeriesMarkers(price, []);

    return () => {
      ro.disconnect();
      chart.remove();
      chartRef.current = null;
      priceRef.current = null;
      volRef.current = null;
      markersRef.current = null;
      cupRef.current = null;
      ibRef.current = null;
      soldiersRef.current = null;
    };
  }, []);

  // -- feed on every selection change --------------------------------------
  useEffect(() => {
    const chart = chartRef.current;
    const price = priceRef.current;
    const vol = volRef.current;
    if (!chart || !price || !vol) return;

    const bars = data?.candles ?? [];
    barsRef.current = bars;

    price.setData(
      bars.map((b) => ({
        time: b.time as unknown as Time,
        open: b.open,
        high: b.high,
        low: b.low,
        close: b.close,
      })),
    );
    vol.setData(
      bars.map((b) => ({
        time: b.time as unknown as Time,
        value: b.volume,
        // Tinted by the day's own direction, and translucent so the histogram
        // never competes with price for attention.
        color: b.close >= b.open ? "rgba(46,189,133,0.42)" : "rgba(246,70,93,0.42)",
      })),
    );

    if (bars.length) {
      // Frame the viewport only when the stock itself changed. The same stock
      // is refetched whenever the engine publishes — every batch of a sync —
      // and resetting the range on those would yank a chart the user had just
      // zoomed into back to the default, repeatedly, while they were reading
      // it. `setData` on its own leaves the viewport where it is.
      const switched = framedKey.current !== (data?.key ?? null);
      if (switched) {
        framedKey.current = data?.key ?? null;
        hovered.current = null;
        // Anchor to the newest candle with the right-hand gap intact, rather
        // than fitContent(), which would squeeze 400 sessions into the pane.
        chart.timeScale().setVisibleLogicalRange({
          from: Math.max(0, bars.length - DEFAULT_BARS),
          to: bars.length + RIGHT_OFFSET,
        });
      }
      // Repaint whatever the crosshair is actually over. Forcing the newest bar
      // here made the readout contradict the crosshair whenever the same
      // stock's data was refreshed under a resting cursor.
      const at = !switched && hovered.current !== null ? Math.min(hovered.current, bars.length - 1) : bars.length - 1;
      paintLegend(legendRef.current, at, bars);
    } else {
      framedKey.current = data?.key ?? null;
      hovered.current = null;
      paintLegend(legendRef.current, -1, bars);
    }
  }, [data]);

  // -- markers and drawings -------------------------------------------------
  // Kept apart from the feed so flipping the Drawings switch only redraws the
  // overlays and never re-sends the candles.
  useEffect(() => {
    const bars = data?.candles ?? [];

    // Scan signals point at the candle, corporate events sit above it. Both go in
    // one list, which the chart wants in ascending time order.
    const signalMarkers = (data?.markers ?? []).map((m) => {
      const style = PATTERN_STYLE[m.pattern];
      return {
        time: m.time as unknown as Time,
        position: m.direction === "Bearish" ? ("aboveBar" as const) : ("belowBar" as const),
        color: style?.color ?? (m.direction === "Bearish" ? DOWN : UP),
        shape: m.direction === "Bearish" ? ("arrowDown" as const) : ("arrowUp" as const),
        text: `${style ? `${style.tag} ` : ""}${(m.score * 100).toFixed(0)}%`,
        size: 1,
      };
    });
    const eventMarkers = (data?.events ?? []).map((e) => ({
      time: e.time as unknown as Time,
      position: "aboveBar" as const,
      // Amber when the earlier prices were corrected, grey when NSE gives no
      // ratio (a demerger) and the cliff is only pointed out.
      color: e.adjusted ? "#d6a03c" : "#8b97a7",
      shape: "square" as const,
      text: e.adjusted ? e.label : `${e.label} (not adjusted)`,
      size: 1,
    }));
    // Today's drawings share one breakout label, set on the candles so it sits
    // above the candle rather than across it. Both setups can fire on the same
    // bar, and two labels there would print on top of each other. With the
    // Drawings switch off, none of this is drawn.
    const ib = drawings ? (data?.insideBar ?? null) : null;
    const cup = drawings ? (data?.cup ?? null) : null;
    const soldiers = drawings && data?.soldiers?.bars.length === 3 ? data.soldiers.bars : null;
    const breakoutTime = ib?.breakout.time ?? cup?.breakout.time;
    const breakoutMarkers = breakoutTime
      ? [
          {
            time: breakoutTime as unknown as Time,
            position: "aboveBar" as const,
            color: UP,
            shape: "arrowUp" as const,
            text: ib
              ? `Breakout +${(((ib.breakout.close - ib.inside.high) / ib.inside.high) * 100).toFixed(1)}%`
              : "Breakout",
            size: 1,
          },
        ]
      : [];
    // Three White Soldiers is not a breakout, so it gets its own label for the
    // three-day advance. Markers on the same bar and side stack, so it sits
    // above any breakout label rather than over it.
    const soldiersMarkers = soldiers
      ? [
          {
            time: soldiers[2].time as unknown as Time,
            position: "aboveBar" as const,
            color: SOLDIERS,
            shape: "arrowUp" as const,
            text: `3WS +${(((soldiers[2].close - soldiers[0].open) / soldiers[0].open) * 100).toFixed(1)}%`,
            size: 1,
          },
        ]
      : [];
    markersRef.current?.setMarkers(
      [...signalMarkers, ...eventMarkers, ...breakoutMarkers, ...soldiersMarkers].sort((a, b) =>
        String(a.time).localeCompare(String(b.time)),
      ),
    );
    if (cupRef.current) drawCup(cupRef.current, cup, bars);
    if (ibRef.current) drawInsideBar(ibRef.current, ib);
    if (soldiersRef.current) drawSoldiers(soldiersRef.current, soldiers);
  }, [data, drawings]);

  // -- zoom ----------------------------------------------------------------
  const zoom = (factor: number) => {
    const ts = chartRef.current?.timeScale();
    if (!ts) return;
    const range = ts.getVisibleLogicalRange();
    if (!range) return;
    const span = range.to - range.from;
    const next = Math.min(Math.max(span * factor, 12), barsRef.current.length + RIGHT_OFFSET + 40);
    // Zoom about the right edge, not the middle: on an end-of-day chart the
    // newest candle is the one being looked at, and it should stay put.
    ts.setVisibleLogicalRange({ from: range.to - next, to: range.to });
  };

  const reset = () => {
    setClicked(null);
    const ts = chartRef.current?.timeScale();
    const n = barsRef.current.length;
    if (!ts || !n) return;
    ts.setVisibleLogicalRange({ from: Math.max(0, n - DEFAULT_BARS), to: n + RIGHT_OFFSET });
  };

  const last = data?.candles.at(-1);
  const prev = data?.candles.at(-2);
  const chg = last && prev && prev.close > 0 ? ((last.close - prev.close) / prev.close) * 100 : 0;
  const dir = chg > 0 ? "up" : chg < 0 ? "down" : "flat";
  // With no candle clicked, the newest signal is shown whether or not it landed
  // on the newest candle — a stock opened from the "1 month" window would
  // otherwise show no card at all, which reads as "nothing fired here". A
  // clicked candle shows its own signals instead, or says it has none.
  const pinned = clicked && data && clicked.key === data.key ? clicked.time : null;
  const newest = data?.markers.at(-1);
  const signals = pinned ? (data?.markers ?? []).filter((m) => m.time === pinned) : newest ? [newest] : [];
  const cardTime = pinned ?? newest?.time;
  const cardIsOld = !!cardTime && !!last && cardTime !== last.time;
  const cardColor = signals.length ? PATTERN_STYLE[signals[0].pattern]?.color : undefined;

  return (
    <section className="chart-pane">
      <header className="chart-head">
        <div className="chart-title">
          <h1>{data?.symbol ?? "—"}</h1>
          {data && <span className={`badge ${data.exchange.toLowerCase()}`}>{data.exchange}</span>}
          <span className="co">{data?.name ?? ""}</span>
        </div>

        {last && (
          <div className={`last-price ${dir}`}>
            <span className="p">{fmt(last.close)}</span>
            <span className="d">
              {chg >= 0 ? "+" : ""}
              {fmt(chg)}%
            </span>
          </div>
        )}

        <div className="spacer" />

        <div className="chart-actions">
          <button
            className={drawings ? "on" : ""}
            onClick={() => setDrawings((d) => !d)}
            title="Draw today's Cup n Handle, Inside Bar and Three White Soldiers setups on the chart"
          >
            Drawings {drawings ? "On" : "Off"}
          </button>
          <button onClick={() => zoom(0.75)} title="Zoom in">+</button>
          <button onClick={() => zoom(1.35)} title="Zoom out">−</button>
          <button onClick={reset} title="Back to the latest candles">Reset</button>
        </div>
      </header>

      <div className="chart-body">
        <div className="chart-host" ref={hostRef} />
        <div className="legend" ref={legendRef} />

        {(signals.length > 0 || pinned) && (
          <div className={`signal ${cardIsOld ? "old" : ""}`} style={{ borderLeftColor: cardColor }}>
            {signals.map((s) => (
              <Fragment key={`${s.time}-${s.pattern}`}>
                <span className="t" style={{ color: PATTERN_STYLE[s.pattern]?.color }}>
                  {s.pattern} · {(s.score * 100).toFixed(0)}%
                </span>
                <span className="d">
                  {cardIsOld && <b>{s.time} · </b>}
                  {s.detail}
                </span>
              </Fragment>
            ))}
            {signals.length === 0 && (
              <span className="d">
                <b>{pinned} · </b>no pattern on this candle
              </span>
            )}
          </div>
        )}

        {!data && (
          <div className="chart-blank">
            Pick a stock from the list
            <br />
            <kbd>↑</kbd> <kbd>↓</kbd> <kbd>←</kbd> <kbd>→</kbd> to walk through it
          </div>
        )}
      </div>
    </section>
  );
}

/** Draw a Cup n Handle breakout, or clear the drawing when there is none.
 *
 *  The bowl is two half-cosine curves meeting flat at the bottom, so it passes
 *  exactly through the left rim, bottom and right rim closes the engine chose.
 *  The neckline and handle are straight lines between real closes. */
function drawCup(layers: CupLayers, cup: CupShape | null, bars: Bar[]) {
  const clear = () => {
    for (const s of [layers.bowl, layers.neck, layers.handle]) s.setData([]);
    for (const m of [layers.bowlLabels, layers.neckLabels, layers.handleLabels]) m.setMarkers([]);
  };
  if (!cup) return clear();

  const at = new Map(bars.map((b, k) => [b.time, k]));
  const l = at.get(cup.leftRim.time);
  const m = at.get(cup.bottom.time);
  const r = at.get(cup.rightRim.time);
  const e = at.get(cup.breakout.time);
  if (l === undefined || m === undefined || r === undefined || e === undefined) return clear();

  const t = (k: number) => bars[k].time as unknown as Time;
  // Eases from `from` to `to` with a flat slope at both ends.
  const ease = (from: number, to: number, x: number) => to + (from - to) * ((1 + Math.cos(Math.PI * x)) / 2);
  const bowl: { time: Time; value: number }[] = [];
  for (let k = l; k <= r; k++) {
    const value =
      k <= m
        ? ease(cup.leftRim.price, cup.bottom.price, (k - l) / Math.max(m - l, 1))
        : ease(cup.bottom.price, cup.rightRim.price, (k - m) / Math.max(r - m, 1));
    bowl.push({ time: t(k), value });
  }
  layers.bowl.setData(bowl);

  const neck: { time: Time; value: number }[] = [];
  for (let k = l; k <= e; k++) neck.push({ time: t(k), value: cup.neckline });
  layers.neck.setData(neck);

  const handle = [cup.rightRim, ...(cup.handleLow ? [cup.handleLow] : []), cup.breakout];
  layers.handle.setData(handle.map((p) => ({ time: p.time as unknown as Time, value: p.price })));

  const label = (time: string, position: "aboveBar" | "belowBar", color: string, text: string) => ({
    time: time as unknown as Time,
    position,
    color,
    shape: "circle" as const,
    text,
    size: 0.5,
  });
  layers.bowlLabels.setMarkers([
    label(cup.leftRim.time, "aboveBar", CUP, `Left rim ${fmt(cup.leftRim.price)}`),
    label(cup.bottom.time, "belowBar", CUP, `Cup bottom ${fmt(cup.bottom.price)}`),
    label(cup.rightRim.time, "aboveBar", CUP, `Right rim ${fmt(cup.rightRim.price)}`),
  ]);
  layers.neckLabels.setMarkers([
    label(bars[Math.round((l + r) / 2)].time, "aboveBar", NECK, `Neckline ${fmt(cup.neckline)} (30-day closing high)`),
  ]);
  layers.handleLabels.setMarkers(
    cup.handleLow ? [label(cup.handleLow.time, "belowBar", HANDLE, `Handle ${fmt(cup.handleLow.price)}`)] : [],
  );
}

/** Draw today's Inside Bar Breakout, or clear the drawing when there is none.
 *
 *  The mother bar's high and low run from the mother bar to today, so the range
 *  the inside bar sat in stays visible. The inside bar's high is the breakout
 *  level and its low the other side of the squeeze. Lines stop at today's
 *  candle: nothing is projected forward. The breakout arrow is set with the
 *  candle markers. */
function drawInsideBar(layers: InsideBarLayers, ib: InsideBarShape | null) {
  const lines = [layers.motherHigh, layers.motherLow, layers.insideHigh, layers.insideLow];
  if (!ib) {
    for (const s of lines) s.setData([]);
    for (const m of layers.labels) m.setMarkers([]);
    return;
  }
  const t = (time: string) => time as unknown as Time;
  const flat = (times: string[], value: number) => times.map((time) => ({ time: t(time), value }));
  const all = [ib.mother.time, ib.inside.time, ib.breakout.time];
  const fromInside = [ib.inside.time, ib.breakout.time];
  layers.motherHigh.setData(flat(all, ib.mother.high));
  layers.motherLow.setData(flat(all, ib.mother.low));
  layers.insideHigh.setData(flat(fromInside, ib.inside.high));
  layers.insideLow.setData(flat(fromInside, ib.inside.low));

  const label = (time: string, position: "aboveBar" | "belowBar", color: string, text: string) => ({
    time: t(time),
    position,
    color,
    shape: "circle" as const,
    text,
    size: 0.5,
  });
  const [motherHigh, motherLow, insideHigh, insideLow] = layers.labels;
  motherHigh.setMarkers([label(ib.mother.time, "aboveBar", MOTHER, `Mother H ${fmt(ib.mother.high)}`)]);
  motherLow.setMarkers([label(ib.mother.time, "belowBar", MOTHER, `Mother L ${fmt(ib.mother.low)}`)]);
  // The breakout arrow itself is drawn on the candles, with the cup's.
  insideHigh.setMarkers([label(ib.breakout.time, "belowBar", IB, `Breakout level ${fmt(ib.inside.high)}`)]);
  insideLow.setMarkers([label(ib.inside.time, "belowBar", IB, `Inside L ${fmt(ib.inside.low)}`)]);
}

/** Draw today's Three White Soldiers, or clear the drawing when there is none.
 *
 *  A step line joins the three closes, each numbered, and a dashed line holds
 *  the first bar's open across all three, so the climb is read from where it
 *  started. The advance label is set with the candle markers. */
function drawSoldiers(layers: SoldiersLayers, bars: BarPoint[] | null) {
  if (!bars) {
    layers.steps.setData([]);
    layers.start.setData([]);
    layers.stepLabels.setMarkers([]);
    layers.startLabels.setMarkers([]);
    return;
  }
  const t = (time: string) => time as unknown as Time;
  layers.steps.setData(bars.map((b) => ({ time: t(b.time), value: b.close })));
  layers.start.setData(bars.map((b) => ({ time: t(b.time), value: bars[0].open })));
  layers.stepLabels.setMarkers(
    bars.map((b, k) => ({
      time: t(b.time),
      position: "aboveBar" as const,
      color: SOLDIERS,
      shape: "circle" as const,
      text: String(k + 1),
      size: 0.5,
    })),
  );
  layers.startLabels.setMarkers([
    {
      time: t(bars[0].time),
      position: "belowBar" as const,
      color: SOLDIERS,
      shape: "circle" as const,
      text: `Start ${fmt(bars[0].open)}`,
      size: 0.5,
    },
  ]);
}

/** Written straight to the DOM — this runs on every crosshair pixel. */
function paintLegend(el: HTMLDivElement | null, i: number, bars: Bar[]) {
  if (!el) return;
  const bar = bars[i];
  if (!bar) {
    el.innerHTML = "";
    return;
  }
  const prev = i > 0 ? bars[i - 1] : undefined;
  const chg = prev && prev.close > 0 ? ((bar.close - prev.close) / prev.close) * 100 : 0;
  const cls = bar.close >= bar.open ? "up" : "down";
  el.innerHTML =
    `<span class="date">${bar.time}</span>` +
    `<span><i class="k">O</i><b class="${cls}">${fmt(bar.open)}</b></span>` +
    `<span><i class="k">H</i><b class="${cls}">${fmt(bar.high)}</b></span>` +
    `<span><i class="k">L</i><b class="${cls}">${fmt(bar.low)}</b></span>` +
    `<span><i class="k">C</i><b class="${cls}">${fmt(bar.close)}</b></span>` +
    `<span class="${chg >= 0 ? "up" : "down"}">${chg >= 0 ? "+" : ""}${fmt(chg)}%</span>` +
    `<span><i class="k">Vol</i>${fmtVol(bar.volume)}</span>`;
}
