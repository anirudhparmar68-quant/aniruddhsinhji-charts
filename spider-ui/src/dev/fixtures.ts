/** Synthetic data for `npm run dev` in a plain browser.
 *
 *  The Rust commands only exist inside the Tauri window, so opening the Vite
 *  server directly would otherwise render an empty shell — useless for working
 *  on layout, spacing or colour. Seeded, so the preview looks the same every
 *  reload and a visual change is always a change you made.
 *
 *  Vite replaces `import.meta.env.DEV` with `false` in a production build, so
 *  the dynamic import that reaches this file is dropped from the bundle. */

import type { ChartData, ScanHit, Settings, Status, Stock } from "../api";

const NAMES: [string, string, string][] = [
  ["ABB", "ABB India Ltd", "NSE"],
  ["ADANIENT", "Adani Enterprises Ltd", "NSE"],
  ["ASIANPAINT", "Asian Paints Ltd", "NSE"],
  ["BAJFINANCE", "Bajaj Finance Ltd", "NSE"],
  ["BALKRISIND", "Balkrishna Industries Ltd", "NSE"],
  ["BHARTIARTL", "Bharti Airtel Ltd", "NSE"],
  ["CIPLA", "Cipla Ltd", "NSE"],
  ["COFORGE", "Coforge Ltd", "NSE"],
  ["DIVISLAB", "Divi's Laboratories Ltd", "NSE"],
  ["DRREDDY", "Dr Reddys Laboratories Ltd", "NSE"],
  ["EICHERMOT", "Eicher Motors Ltd", "NSE"],
  ["FINEORG", "Fine Organic Industries Ltd", "BSE"],
  ["GRASIM", "Grasim Industries Ltd", "NSE"],
  ["HAL", "Hindustan Aeronautics Ltd", "NSE"],
  ["HDFCBANK", "HDFC Bank Ltd", "NSE"],
  ["ICICIBANK", "ICICI Bank Ltd", "NSE"],
  ["INFY", "Infosys Ltd", "NSE"],
  ["JSWSTEEL", "JSW Steel Ltd", "NSE"],
  ["KAJARIACER", "Kajaria Ceramics Ltd", "BSE"],
  ["LTIM", "LTIMindtree Ltd", "NSE"],
  ["MARUTI", "Maruti Suzuki India Ltd", "NSE"],
  ["NESTLEIND", "Nestle India Ltd", "NSE"],
  ["OBEROIRLTY", "Oberoi Realty Ltd", "NSE"],
  ["PERSISTENT", "Persistent Systems Ltd", "NSE"],
  ["POLYCAB", "Polycab India Ltd", "NSE"],
  ["RELIANCE", "Reliance Industries Ltd", "NSE"],
  ["SBIN", "State Bank of India", "NSE"],
  ["SUPREMEIND", "Supreme Industries Ltd", "NSE"],
  ["TATAMOTORS", "Tata Motors Ltd", "NSE"],
  ["TCS", "Tata Consultancy Services Ltd", "NSE"],
  ["TITAN", "Titan Company Ltd", "NSE"],
  ["TRENT", "Trent Ltd", "NSE"],
  ["ULTRACEMCO", "UltraTech Cement Ltd", "NSE"],
  ["VBL", "Varun Beverages Ltd", "NSE"],
  ["WIPRO", "Wipro Ltd", "NSE"],
  ["ZYDUSLIFE", "Zydus Lifesciences Ltd", "NSE"],
];

/** Deterministic LCG — `Math.random` would reshuffle the preview every reload. */
function rng(seed: number) {
  let s = seed >>> 0;
  return () => {
    s = (s * 1664525 + 1013904223) >>> 0;
    return s / 4294967296;
  };
}

const SESSIONS = 320;
const LATEST = "2026-08-12";

/** Trading days counted backwards from the latest session, weekends skipped. */
function sessionDates(n: number): string[] {
  const out: string[] = [];
  const d = new Date(LATEST + "T00:00:00Z");
  while (out.length < n) {
    const dow = d.getUTCDay();
    if (dow !== 0 && dow !== 6) out.push(d.toISOString().slice(0, 10));
    d.setUTCDate(d.getUTCDate() - 1);
  }
  return out.reverse();
}

const DATES = sessionDates(SESSIONS);

function series(seed: number, base: number) {
  const r = rng(seed);
  const bars = [];
  let price = base;
  for (let i = 0; i < SESSIONS; i++) {
    // A cup-shaped drift over the last 90 bars, so the chart shows something
    // the scan would plausibly have fired on.
    const t = (i - (SESSIONS - 90)) / 90;
    const cup = i > SESSIONS - 90 ? -Math.sin(t * Math.PI) * base * 0.16 : 0;
    price = price * (1 + (r() - 0.49) * 0.022);
    const mid = price + cup;
    const open = mid * (1 + (r() - 0.5) * 0.008);
    const close = mid * (1 + (r() - 0.5) * 0.012);
    const high = Math.max(open, close) * (1 + r() * 0.009);
    const low = Math.min(open, close) * (1 - r() * 0.009);
    bars.push({
      time: DATES[i],
      open: +open.toFixed(2),
      high: +high.toFixed(2),
      low: +low.toFixed(2),
      close: +close.toFixed(2),
      volume: Math.round((0.6 + r() * 1.6) * 400000 * (i > SESSIONS - 3 ? 2.4 : 1)),
    });
  }
  return bars;
}

const STOCKS: Stock[] = NAMES.map(([symbol, name, exchange], i) => {
  const bars = series(i * 977 + 13, 180 + ((i * 137) % 3200));
  const last = bars[bars.length - 1];
  const prev = bars[bars.length - 2];
  const hit = i % 4 === 0;
  return {
    key: `${exchange}_EQ|FIXTURE${String(i).padStart(6, "0")}`,
    symbol,
    name,
    exchange,
    mcapCr: 1200 + i * 830,
    close: last.close,
    changePct: ((last.close - prev.close) / prev.close) * 100,
    volumeRatio: hit ? 1.6 + (i % 5) * 0.35 : 0,
    rsi: hit ? 58 + (i % 7) : 0,
    hit,
    score: hit ? 0.62 + (i % 5) * 0.06 : 0,
    bars: bars.length,
    stale: i % 17 === 0,
  };
});

const BARS = new Map(STOCKS.map((s, i) => [s.key, series(i * 977 + 13, 180 + ((i * 137) % 3200))]));

export const status = (): Status => ({
  busy: false,
  message: "Browser preview — synthetic data, engine not attached",
  lastSync: "2026-08-12 19:34",
  latestSession: LATEST,
  universeCount: STOCKS.length,
  totalInstruments: STOCKS.length,
  hitCount: STOCKS.filter((s) => s.hit).length,
  staleCount: STOCKS.filter((s) => s.stale).length,
  progressDone: 0,
  progressTotal: 0,
  progressLabel: "",
  error: null,
});

export const universe = (): Stock[] => STOCKS;

/** The engine's own defaults, so the preview shows the real form. */
export const settings = (): Settings => ({
  min_mcap_cr: 100,
  history_days: 400,
  max_concurrency: 8,
  requests_per_second: 4,
  include_unknown_mcap: true,
  min_median_turnover_cr: 0.25,
  mcap_source_url: "",
  min_price: 5,
  scan_lookback: 250,
  min_pattern_score: 0.5,
  prefer_nse: true,
  bse_groups: ["A", "B"],
  patterns: {
    chartink: {
      breakout_lookback: 30,
      depth_window: 20,
      base_window: 60,
      depth_ratio: 0.85,
      trend_sma: 50,
      volume_fast_sma: 5,
      volume_slow_sma: 50,
      rsi_period: 14,
      rsi_min: 55,
      adx_period: 14,
      adx_min: 20,
      volume_multiple: 1.5,
      volume_sma: 20,
    },
  },
});

export const chart = (key: string): ChartData | null => {
  const s = STOCKS.find((x) => x.key === key);
  const candles = BARS.get(key);
  if (!s || !candles) return null;
  return {
    key,
    symbol: s.symbol,
    name: s.name,
    exchange: s.exchange,
    candles,
    markers: s.hit
      ? [
          {
            time: candles[candles.length - 1].time,
            startTime: candles[candles.length - 70].time,
            pattern: "Cup & Handle Breakout (Chartink)",
            direction: "Bullish",
            score: s.score,
            detail: `closed above the 30-day high on ${s.volumeRatio.toFixed(1)}× volume, RSI ${s.rsi.toFixed(0)}`,
          },
        ]
      : [],
  };
};

export const scanner = (): ScanHit[] =>
  STOCKS.filter((s) => s.hit).map((s) => ({
    key: s.key,
    symbol: s.symbol,
    name: s.name,
    exchange: s.exchange,
    date: LATEST,
    pattern: "Cup & Handle Breakout (Chartink)",
    direction: "Bullish",
    score: s.score,
    detail: "closed above the 30-day high",
    close: s.close,
    changePct: s.changePct,
    volumeRatio: s.volumeRatio,
    rsi: s.rsi,
  }));
