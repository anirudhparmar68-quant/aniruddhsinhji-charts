/** The chart half of the split view: price, volume and the scan markers.
 *
 *  The chart instance is created once and then *fed* — switching stocks calls
 *  `setData`, never a teardown. Recreating it per selection is what makes an
 *  arrow-key walk through a list feel heavy, and walking the list quickly is
 *  the whole point of this pane. */

import { useEffect, useLayoutEffect, useRef } from "react";
import {
  CandlestickSeries,
  ColorType,
  CrosshairMode,
  HistogramSeries,
  LineStyle,
  createChart,
  createSeriesMarkers,
  type IChartApi,
  type ISeriesApi,
  type ISeriesMarkersPluginApi,
  type Time,
} from "lightweight-charts";
import type { Bar, ChartData } from "./api";

/** Bars visible when a stock is opened. Enough to see the base a cup formed
 *  over without the candles turning into hairlines. */
const DEFAULT_BARS = 150;

/** Empty bar-widths kept after the newest candle. Price glued to the axis is
 *  hard to read and leaves nowhere to project the next move. */
const RIGHT_OFFSET = 14;

const UP = "#2ebd85";
const DOWN = "#f6465d";

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
  // Kept outside React state: the crosshair handler reads it on every mouse
  // move, and a re-render per pixel is exactly the jank this pane avoids.
  const barsRef = useRef<Bar[]>([]);
  /// Which stock the viewport was last framed for. Refreshing the *same* stock
  /// must not move it — see the note in the feed effect.
  const framedKey = useRef<string | null>(null);
  /// Bar the crosshair is currently over, so a data refresh repaints the bar
  /// being pointed at rather than snapping the readout to the newest one.
  const hovered = useRef<number | null>(null);

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

    markersRef.current?.setMarkers(
      (data?.markers ?? []).map((m) => ({
        time: m.time as unknown as Time,
        position: m.direction === "Bearish" ? "aboveBar" : "belowBar",
        color: m.direction === "Bearish" ? DOWN : UP,
        shape: m.direction === "Bearish" ? "arrowDown" : "arrowUp",
        text: `${(m.score * 100).toFixed(0)}%`,
        size: 1,
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
    const ts = chartRef.current?.timeScale();
    const n = barsRef.current.length;
    if (!ts || !n) return;
    ts.setVisibleLogicalRange({ from: Math.max(0, n - DEFAULT_BARS), to: n + RIGHT_OFFSET });
  };

  const last = data?.candles.at(-1);
  const prev = data?.candles.at(-2);
  const chg = last && prev && prev.close > 0 ? ((last.close - prev.close) / prev.close) * 100 : 0;
  const dir = chg > 0 ? "up" : chg < 0 ? "down" : "flat";
  // The newest signal is shown whether or not it landed on the newest candle —
  // a stock opened from the "1 month" window would otherwise show no card at
  // all, which reads as "nothing fired here". The date says which bar it was.
  const signal = data?.markers.at(-1);
  const signalIsOld = !!signal && !!last && signal.time !== last.time;

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
          <button onClick={() => zoom(0.75)} title="Zoom in">+</button>
          <button onClick={() => zoom(1.35)} title="Zoom out">−</button>
          <button onClick={reset} title="Back to the latest candles">Reset</button>
        </div>
      </header>

      <div className="chart-body">
        <div className="chart-host" ref={hostRef} />
        <div className="legend" ref={legendRef} />

        {signal && (
          <div className={`signal ${signalIsOld ? "old" : ""}`}>
            <span className="t">
              {signal.pattern} · {(signal.score * 100).toFixed(0)}%
            </span>
            <span className="d">
              {signalIsOld && <b>{signal.time} · </b>}
              {signal.detail}
            </span>
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
