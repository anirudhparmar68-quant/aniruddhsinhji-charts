/** Typed wrapper over the Rust commands in `src-tauri/src/lib.rs`.
 *  These interfaces mirror the `#[derive(Serialize)]` DTOs one-for-one; if a
 *  field is added there it belongs here too. */

import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";

export interface Status {
  busy: boolean;
  message: string;
  lastSync: string | null;
  latestSession: string | null;
  universeCount: number;
  totalInstruments: number;
  hitCount: number;
  staleCount: number;
  progressDone: number;
  progressTotal: number;
  progressLabel: string;
  error: string | null;
  /** Folder holding this copy's `data/` and `.env`. */
  dataDir: string;
  /** The startup read of the database has finished. */
  loaded: boolean;
}

export interface Stock {
  key: string;
  symbol: string;
  name: string;
  exchange: string;
  mcapCr: number | null;
  close: number;
  changePct: number;
  volumeRatio: number;
  rsi: number;
  hit: boolean;
  score: number;
  bars: number;
  stale: boolean;
}

export interface Bar {
  time: string;
  open: number;
  high: number;
  low: number;
  close: number;
  volume: number;
}

export interface Marker {
  time: string;
  startTime: string;
  pattern: string;
  direction: string;
  score: number;
  detail: string;
}

/** A split, bonus or other corporate event drawn on a chart. */
export interface CorpEvent {
  time: string;
  label: string;
  /** Earlier prices were corrected for it. False when it is only flagged. */
  adjusted: boolean;
}

export interface ChartData {
  key: string;
  symbol: string;
  name: string;
  exchange: string;
  candles: Bar[];
  markers: Marker[];
  events: CorpEvent[];
}

export interface ScanHit {
  key: string;
  symbol: string;
  name: string;
  exchange: string;
  date: string;
  pattern: string;
  direction: string;
  score: number;
  detail: string;
  close: number;
  changePct: number;
  volumeRatio: number;
  rsi: number;
}

/** Mirrors `config::Settings`. Plain serde, so the keys are snake_case — they
 *  are the same keys you see in `data/settings.json`. */
export interface CupParams {
  breakout_lookback: number;
  depth_window: number;
  base_window: number;
  depth_ratio: number;
  trend_sma: number;
  volume_fast_sma: number;
  volume_slow_sma: number;
  rsi_period: number;
  rsi_min: number;
  adx_period: number;
  adx_min: number;
  volume_multiple: number;
  volume_sma: number;
}

export interface Settings {
  min_mcap_cr: number;
  history_days: number;
  max_concurrency: number;
  requests_per_second: number;
  include_unknown_mcap: boolean;
  min_median_turnover_cr: number;
  mcap_source_url: string;
  min_price: number;
  scan_lookback: number;
  min_pattern_score: number;
  prefer_nse: boolean;
  adjust_corporate_actions: boolean;
  bse_groups: string[];
  patterns: { chartink: CupParams };
}

export type Job = "tail" | "rescan" | "universe" | "backfill" | "full" | "reload" | "login";

/** The Rust commands only exist inside the Tauri window. Opening the Vite
 *  server in a browser is a normal thing to do while working on the UI, so that
 *  case falls back to fixtures instead of throwing on every call. */
const inTauri = typeof window !== "undefined" && "__TAURI_INTERNALS__" in window;
const preview = import.meta.env.DEV && !inTauri;
const fixtures = preview ? import("./dev/fixtures") : null;

// One line, once, saying where the data is coming from. Without it a browser
// tab showing an empty app is indistinguishable from an engine with no data.
console.info(
  `[spider] ${inTauri ? "engine attached" : preview ? "browser preview — synthetic data" : "no engine and no fixtures: every call will fail"}`,
);

export const getStatus = (): Promise<Status> =>
  fixtures ? fixtures.then((f) => f.status()) : invoke<Status>("get_status");

export const getUniverse = (): Promise<Stock[]> =>
  fixtures ? fixtures.then((f) => f.universe()) : invoke<Stock[]>("get_universe");

export const getChart = (key: string): Promise<ChartData | null> =>
  fixtures ? fixtures.then((f) => f.chart(key)) : invoke<ChartData | null>("get_chart", { key });

export const getScanner = (days?: number): Promise<ScanHit[]> =>
  fixtures ? fixtures.then((f) => f.scanner()) : invoke<ScanHit[]>("get_scanner", { days: days ?? null });

export const runJob = (name: Job): Promise<void> =>
  fixtures ? Promise.resolve() : invoke<void>("run_job", { name });

export const getSettings = (): Promise<Settings> =>
  fixtures ? fixtures.then((f) => f.settings()) : invoke<Settings>("get_settings");

export const defaultSettings = (): Promise<Settings> =>
  fixtures ? fixtures.then((f) => f.settings()) : invoke<Settings>("default_settings");

export const saveSettings = (settings: Settings, rescan: boolean): Promise<void> =>
  fixtures ? Promise.resolve() : invoke<void>("save_settings", { settings, rescan });

/** Returns the full path the file was written to. */
export const exportCsv = (name: string, contents: string): Promise<string> =>
  fixtures
    ? Promise.resolve(`(preview) ${name}.csv — ${contents.split("\n").length - 1} rows`)
    : invoke<string>("export_csv", { name, contents });

/** Opens the app's folder in Explorer. */
export const openDataFolder = (): Promise<void> =>
  fixtures ? Promise.resolve() : invoke<void>("open_data_folder");

/** Subscribe to every engine event. Returns the unlisten function. */
export async function onEngine(fn: (topic: string) => void): Promise<() => void> {
  if (!inTauri) return () => {};
  const offs = await Promise.all(
    ["engine:status", "engine:data", "engine:scanner"].map((t) =>
      listen(t, () => fn(t)),
    ),
  );
  return () => offs.forEach((off) => off());
}
