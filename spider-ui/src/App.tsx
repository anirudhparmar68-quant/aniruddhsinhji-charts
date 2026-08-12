/** Split view: the stock list on the left, its chart on the right, both live at
 *  the same time. Selection is driven by the keyboard first — arrow keys walk
 *  the list and the chart follows — because scanning a night's hits is a
 *  hundred selections, and a hundred round trips to the mouse is the difference
 *  between a tool and a chore. */

import { useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState } from "react";
import ChartPane from "./ChartPane";
import SettingsSheet from "./SettingsSheet";
import {
  exportCsv,
  getChart,
  getScanner,
  getStatus,
  getUniverse,
  onEngine,
  runJob,
  type ChartData,
  type Job,
  type ScanHit,
  type Status,
  type Stock,
} from "./api";

const ROW_H = 46;
/** Rows rendered above and below the viewport, so a fast scroll never shows a
 *  blank band before React catches up. */
const OVERSCAN = 6;

type View = "scan" | "all";

type SortKey = "symbol" | "score" | "change" | "volume" | "rsi" | "date" | "close";

/** Which sorts each tab offers, in the order they are shown. `symbol` leads
 *  because alphabetical is the order this app was asked for from the start. */
const SORTS: Record<View, { key: SortKey; label: string }[]> = {
  scan: [
    { key: "symbol", label: "A–Z" },
    { key: "score", label: "Score" },
    { key: "change", label: "Chg%" },
    { key: "volume", label: "Vol×" },
    { key: "rsi", label: "RSI" },
    { key: "date", label: "Date" },
  ],
  all: [
    { key: "symbol", label: "A–Z" },
    { key: "change", label: "Chg%" },
    { key: "close", label: "Last" },
  ],
};

/** Descending is the useful default for every measure except the alphabet. */
const defaultDesc = (k: SortKey) => k !== "symbol";

const WINDOWS: { label: string; days: number | undefined }[] = [
  { label: "Latest", days: 0 },
  { label: "3D", days: 3 },
  { label: "1W", days: 7 },
  { label: "1M", days: 30 },
  { label: "All", days: undefined },
];

/** One line in the list, whichever tab produced it. */
interface Row {
  key: string;
  symbol: string;
  name: string;
  exchange: string;
  close: number;
  changePct: number;
  hit: boolean;
  stale: boolean;
  date?: string;
  score?: number;
  /// Session volume over its 50-day average, and RSI — the two numbers a
  /// breakout list is actually triaged on. Only carried for scanner rows.
  volumeRatio?: number;
  rsi?: number;
}

const fmt = (n: number, dp = 2) =>
  n.toLocaleString("en-IN", { minimumFractionDigits: dp, maximumFractionDigits: dp });

/** Quote only where a reader would otherwise get the wrong columns. Excel and
 *  every CSV parser accept a doubled `""` inside a quoted field. */
const cell = (v: string | number) => {
  const s = String(v);
  return /[",\n\r]/.test(s) ? `"${s.replace(/"/g, '""')}"` : s;
};

/** The export is the list on screen — same tab, same filter, same order.
 *  Numbers are written unformatted (no thousands separators, no % sign) so the
 *  file is arithmetic, not text that merely looks like it. */
function toCsv(rows: Row[], view: View): string {
  const head =
    view === "scan"
      ? ["Symbol", "Name", "Exchange", "Date", "Close", "ChangePct", "VolumeRatio", "RSI", "Score"]
      : ["Symbol", "Name", "Exchange", "Close", "ChangePct", "Setup", "Traded"];

  const line = (r: Row) =>
    view === "scan"
      ? [
          r.symbol, r.name, r.exchange, r.date ?? "",
          r.close.toFixed(2), r.changePct.toFixed(2),
          (r.volumeRatio ?? 0).toFixed(2), (r.rsi ?? 0).toFixed(1), (r.score ?? 0).toFixed(3),
        ]
      : [
          r.symbol, r.name, r.exchange,
          r.close.toFixed(2), r.changePct.toFixed(2),
          r.hit ? "yes" : "no", r.stale ? "no" : "yes",
        ];

  return [head, ...rows.map(line)].map((cols) => cols.map(cell).join(",")).join("\r\n") + "\r\n";
}

/** Layout choices survive a restart. Deliberately only the layout — restoring a
 *  selected stock that has since dropped out of the scanner would land you on a
 *  chart you did not ask for. */
function remembered<T>(key: string, fallback: T): T {
  try {
    const raw = localStorage.getItem(key);
    return raw === null ? fallback : (JSON.parse(raw) as T);
  } catch {
    return fallback;
  }
}

export default function App() {
  // Restored values are validated, never trusted: these lists have been tuned
  // before, and a saved index or key that no longer exists would throw on the
  // very first render with nothing on screen to explain it.
  const [view, setView] = useState<View>(() => {
    const v = remembered<View>("view", "scan");
    return v === "scan" || v === "all" ? v : "scan";
  });
  const [windowIdx, setWindowIdx] = useState(() => {
    const i = remembered("window", 0);
    return Number.isInteger(i) && i >= 0 && i < WINDOWS.length ? i : 0;
  });
  const [universe, setUniverse] = useState<Stock[]>([]);
  const [hits, setHits] = useState<ScanHit[]>([]);
  const [status, setStatus] = useState<Status | null>(null);
  const [query, setQuery] = useState("");
  const [selected, setSelected] = useState<string | null>(null);
  const [chart, setChart] = useState<ChartData | null>(null);
  const [listW, setListW] = useState(() => remembered("listWidth", 330));
  const [sortKey, setSortKey] = useState<SortKey>(() => {
    const k = remembered<SortKey>("sortKey", "symbol");
    const known = new Set([...SORTS.scan, ...SORTS.all].map((s) => s.key));
    return known.has(k) ? k : "symbol";
  });
  const [sortDesc, setSortDesc] = useState(() => remembered<boolean>("sortDesc", false) === true);
  const [settingsOpen, setSettingsOpen] = useState(false);
  const [menuOpen, setMenuOpen] = useState(false);
  /** Transient line in the status bar for things the engine never hears about,
   *  like where an export was written. */
  const [flash, setFlash] = useState<string | null>(null);
  const flashTimer = useRef<ReturnType<typeof setTimeout> | undefined>(undefined);

  /** Show a note for long enough to read a file path off it.
   *
   *  It used to be cleared by the next engine event, which meant exporting
   *  during a sync wrote the file and then replaced the path with a progress
   *  line before it could be read. Engine *errors* still take precedence in the
   *  status bar; routine chatter no longer does. */
  const note = useCallback((message: string | null) => {
    clearTimeout(flashTimer.current);
    setFlash(message);
    if (message) flashTimer.current = setTimeout(() => setFlash(null), 12000);
  }, []);

  useEffect(() => () => clearTimeout(flashTimer.current), []);

  useEffect(() => {
    try {
      localStorage.setItem("view", JSON.stringify(view));
      localStorage.setItem("window", JSON.stringify(windowIdx));
      localStorage.setItem("listWidth", JSON.stringify(listW));
      localStorage.setItem("sortKey", JSON.stringify(sortKey));
      localStorage.setItem("sortDesc", JSON.stringify(sortDesc));
    } catch {
      // Private mode or a full quota: the app works fine without persistence.
    }
  }, [view, windowIdx, listW, sortKey, sortDesc]);

  const rowsRef = useRef<HTMLDivElement>(null);
  const searchRef = useRef<HTMLInputElement>(null);
  const [scrollTop, setScrollTop] = useState(0);
  const [viewportH, setViewportH] = useState(600);

  // -- data ----------------------------------------------------------------
  // A swallowed failure here is the worst outcome available: an empty list and
  // an empty chart look exactly like an engine that has no data yet, so the one
  // thing that would explain it never reaches the screen. Every call says so.
  const failed = useCallback((what: string) => (e: unknown) => {
    note(`Could not load ${what}: ${e}`);
  }, []);

  const refreshStatus = useCallback(() => {
    getStatus().then(setStatus).catch(failed("the status"));
  }, [failed]);

  const refreshData = useCallback(() => {
    getUniverse().then(setUniverse).catch(failed("the stock list"));
  }, [failed]);

  const refreshHits = useCallback(() => {
    getScanner(WINDOWS[windowIdx].days).then(setHits).catch(failed("the scan results"));
  }, [windowIdx, failed]);

  useEffect(() => {
    refreshStatus();
    refreshData();
    refreshHits();

    // A backfill emits DataChanged per batch. Refetching the whole universe on
    // each one would spend the sync stuttering, so the bursts are coalesced —
    // the list only has to be right once the flurry stops.
    let pending: ReturnType<typeof setTimeout> | undefined;
    const off = onEngine((topic) => {
      // A real engine message supersedes whatever local note was showing.
      refreshStatus();
      if (topic === "engine:status") return;
      clearTimeout(pending);
      pending = setTimeout(() => {
        refreshData();
        refreshHits();
      }, 300);
    });
    return () => {
      clearTimeout(pending);
      off.then((f) => f());
    };
  }, [refreshStatus, refreshData, refreshHits]);

  useEffect(refreshHits, [refreshHits]);

  // The engine reports progress on a channel, but a long backfill can be quiet
  // for a while; a slow tick keeps the status bar honest without polling hard.
  useEffect(() => {
    if (!status?.busy) return;
    const t = setInterval(refreshStatus, 1200);
    return () => clearInterval(t);
  }, [status?.busy, refreshStatus]);

  // -- rows ----------------------------------------------------------------
  /** Score and RSI mean nothing on the All stocks tab, so a sort carried over
   *  from the scanner falls back to the alphabet rather than sorting by zero.
   *  The direction has to fall back with it: descending is right for a score
   *  and wrong for the alphabet, and All stocks opening at Z–A is disorienting. */
  const sortOffered = SORTS[view].some((s) => s.key === sortKey);
  const effSort: SortKey = sortOffered ? sortKey : "symbol";
  const effDesc = sortOffered ? sortDesc : defaultDesc("symbol");

  const rows: Row[] = useMemo(() => {
    let base: Row[];
    if (view === "all") {
      base = universe.map((s) => ({
        key: s.key,
        symbol: s.symbol,
        name: s.name,
        exchange: s.exchange,
        close: s.close,
        changePct: s.changePct,
        hit: s.hit,
        stale: s.stale,
        score: s.score,
      }));
    } else {
      // One line per stock: a stock that fired on three days in the window is
      // still one thing to look at, and its chart shows every marker anyway.
      const newest = new Map<string, ScanHit>();
      for (const h of hits) {
        const seen = newest.get(h.key);
        if (!seen || h.date > seen.date) newest.set(h.key, h);
      }
      base = [...newest.values()].map((h) => ({
        key: h.key,
        symbol: h.symbol,
        name: h.name,
        exchange: h.exchange,
        close: h.close,
        changePct: h.changePct,
        hit: true,
        stale: false,
        date: h.date,
        score: h.score,
        volumeRatio: h.volumeRatio,
        rsi: h.rsi,
      }));
    }

    const q = query.trim().toUpperCase();
    if (q) base = base.filter((r) => r.symbol.includes(q) || r.name.toUpperCase().includes(q));

    const of = (r: Row): number | string => {
      switch (effSort) {
        case "score": return r.score ?? 0;
        case "change": return r.changePct;
        case "volume": return r.volumeRatio ?? 0;
        case "rsi": return r.rsi ?? 0;
        case "close": return r.close;
        case "date": return r.date ?? "";
        default: return r.symbol;
      }
    };
    base.sort((a, b) => {
      const x = of(a);
      const y = of(b);
      const c = typeof x === "string" ? x.localeCompare(y as string) : x - (y as number);
      // Ties fall back to the alphabet so the order never shuffles between
      // refreshes — a list that reorders under you loses your place.
      return (effDesc ? -c : c) || a.symbol.localeCompare(b.symbol);
    });
    return base;
  }, [view, universe, hits, query, effSort, effDesc]);

  /** Distinct stocks in the current scanner window, before any search. */
  const scanCount = useMemo(() => new Set(hits.map((h) => h.key)).size, [hits]);

  const index = useMemo(
    () => (selected ? rows.findIndex((r) => r.key === selected) : -1),
    [rows, selected],
  );

  // Keep a selection alive across tab and filter changes: landing on an empty
  // chart because a row moved is the fastest way to lose your place.
  useEffect(() => {
    if (!rows.length) return;
    if (index === -1) setSelected(rows[0].key);
  }, [rows, index]);

  useEffect(() => {
    if (!selected) {
      setChart(null);
      return;
    }
    let live = true;
    getChart(selected)
      .then((c) => {
        if (live) setChart(c);
      })
      .catch((e) => {
        if (live) note(`Could not load that chart: ${e}`);
      });
    return () => {
      live = false;
    };
  }, [selected, universe]);

  // -- keyboard ------------------------------------------------------------
  // The rows the keyboard is walking, mirrored into a ref.
  //
  // `move` must not close over `rows`/`index`, because several keydowns can
  // land inside one React render: with a captured index, twenty-five rapid
  // presses all compute "current + 1" and the list advances a single row.
  // Reading the live list from a ref and deriving the position inside the
  // updater makes each press see the previous one.
  const rowsData = useRef<Row[]>(rows);
  const pendingScroll = useRef<number | null>(null);
  useEffect(() => {
    rowsData.current = rows;
  }, [rows]);

  const move = useCallback((delta: number) => {
    setSelected((prev) => {
      const list = rowsData.current;
      if (!list.length) return prev;
      const from = Math.max(0, list.findIndex((r) => r.key === prev));
      const next = Math.min(Math.max(from + delta, 0), list.length - 1);
      const key = list[next].key;
      // Only arm the scroll when the selection actually moves. Pressing Down on
      // the last row changes nothing, so React skips the render and the effect
      // never runs — leaving a stale index armed that would then yank the list
      // away from the next row the user clicked.
      if (key !== prev) pendingScroll.current = next;
      return key;
    });
  }, []);

  // Scrolling is a DOM effect, so it happens after the render rather than
  // inside the state updater.
  useEffect(() => {
    if (pendingScroll.current === null) return;
    scrollRowIntoView(rowsRef.current, pendingScroll.current);
    pendingScroll.current = null;
  }, [selected]);

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      // The settings sheet is modal: keys must not walk the list behind it.
      if (settingsOpen) return;

      const el = e.target as HTMLElement | null;
      // A focused dropdown owns its own arrow keys; stealing them would change
      // the window *and* jump the selection on one keypress.
      if (el?.tagName === "SELECT") return;

      const typing = el && (el.tagName === "INPUT" || el.tagName === "TEXTAREA");

      if (typing) {
        // Arrows still walk the list while the search box has focus — you
        // filter, then step through what is left without reaching for the mouse.
        if (e.key === "Escape") {
          setQuery("");
          (el as HTMLInputElement).blur();
          e.preventDefault();
        }
        if (e.key === "Enter") {
          // Leave the search box, keep the filter. Handled here rather than in
          // the switch below so a focused toolbar button keeps its own Enter.
          (el as HTMLInputElement).blur();
          e.preventDefault();
          return;
        }
        if (e.key !== "ArrowUp" && e.key !== "ArrowDown") return;
      }

      switch (e.key) {
        case "ArrowUp":
        case "ArrowLeft":
          move(-1);
          break;
        case "ArrowDown":
        case "ArrowRight":
          move(1);
          break;
        case "PageUp":
          move(-10);
          break;
        case "PageDown":
          move(10);
          break;
        case "Home":
          move(-rows.length);
          break;
        case "End":
          move(rows.length);
          break;
        case "/":
          searchRef.current?.focus();
          searchRef.current?.select();
          break;
        case "k":
        case "K":
          if (!e.ctrlKey) return;
          searchRef.current?.focus();
          searchRef.current?.select();
          break;
        default:
          return;
      }
      e.preventDefault();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [move, rows.length, settingsOpen]);

  // -- splitter ------------------------------------------------------------
  const dragRef = useRef(false);
  useEffect(() => {
    const onMove = (e: MouseEvent) => {
      if (!dragRef.current) return;
      setListW(Math.min(Math.max(e.clientX, 240), 560));
    };
    const onUp = () => {
      dragRef.current = false;
      document.body.style.cursor = "";
    };
    window.addEventListener("mousemove", onMove);
    window.addEventListener("mouseup", onUp);
    return () => {
      window.removeEventListener("mousemove", onMove);
      window.removeEventListener("mouseup", onUp);
    };
  }, []);

  // -- virtual window ------------------------------------------------------
  // Measured after every render rather than only at mount. The pane is short
  // while the list is empty and grows once rows arrive, and a height captured
  // at mount would leave the window rendering a band too small — the selected
  // row then vanishes from the DOM as soon as the keyboard walks past it.
  // A ResizeObserver alone is not enough: it needs the page to be painting.
  useLayoutEffect(() => {
    const h = rowsRef.current?.clientHeight ?? 0;
    if (h > 0 && h !== viewportH) setViewportH(h);
  });

  useEffect(() => {
    const el = rowsRef.current;
    if (!el) return;
    const ro = new ResizeObserver(() => setViewportH(el.clientHeight));
    ro.observe(el);
    return () => ro.disconnect();
  }, []);

  // Switching to a shorter list leaves the scroller at an offset the new list
  // cannot reach; the browser corrects it, but only on the next scroll event.
  // Clamping here means the frame in between is not a blank band.
  const maxTop = Math.max(0, rows.length * ROW_H - viewportH);
  const top = Math.min(scrollTop, maxTop);
  const first = Math.min(Math.max(0, Math.floor(top / ROW_H) - OVERSCAN), Math.max(0, rows.length - 1));
  const last = Math.min(rows.length, Math.ceil((top + viewportH) / ROW_H) + OVERSCAN);
  const slice = rows.slice(first, last);

  // A list that changes identity starts at the top. Carrying the old offset
  // over leaves the auto-selected first row scrolled out of sight.
  useEffect(() => {
    if (rowsRef.current) rowsRef.current.scrollTop = 0;
    setScrollTop(0);
  }, [view, windowIdx, query]);

  const busy = status?.busy ?? false;
  const job = (name: Job) => () => {
    setMenuOpen(false);
    note(null);
    runJob(name).catch((e) => note(String(e)));
    refreshStatus();
  };

  const exportRows = async () => {
    if (!rows.length) {
      note("Nothing to export — the list is empty.");
      return;
    }
    const name = view === "scan" ? "scanner_export" : "universe_export";
    try {
      note(`Wrote ${rows.length} rows to ${await exportCsv(name, toCsv(rows, view))}`);
    } catch (e) {
      note(`Export failed: ${e}`);
    }
  };

  return (
    <div className="app" style={{ ["--list-w" as string]: `${listW}px` }}>
      <header className="topbar">
        <div className="brand">
          <b>Spider Charts</b>
          <span>NSE · BSE end of day</span>
        </div>

        <div className="session">
          Session <b>{status?.latestSession ?? "—"}</b>
        </div>

        <div className="spacer" />

        {status && status.staleCount > 0 && (
          <span className="stale-note" title="These stocks have no bar on the newest session — a bhavcopy only lists scrips that traded, so they simply did not trade. Nothing to fix.">
            {status.staleCount} didn’t trade
          </span>
        )}

        <button onClick={job("tail")} disabled={busy} className="primary" title="Pull the newest sessions from the NSE/BSE bhavcopy. Needs no login.">
          Update
        </button>
        <button onClick={job("rescan")} disabled={busy} title="Re-run the scan over stored history">
          Rescan
        </button>

        <div className="menu-wrap">
          <button onClick={() => setMenuOpen((o) => !o)} disabled={busy} title="Everything else">
            More ▾
          </button>
          {menuOpen && (
            <>
              <div className="menu-catch" onClick={() => setMenuOpen(false)} />
              <div className="menu">
                <button onClick={job("full")}>
                  Full sync<i>Login, universe, history, then scan</i>
                </button>
                <button onClick={job("universe")}>
                  Refresh universe<i>Re-read the instrument master and your export</i>
                </button>
                <button onClick={job("backfill")}>
                  Download history<i>Fetch whatever sessions are missing</i>
                </button>
                <button onClick={job("login")}>
                  Upstox login<i>Opens a browser. Only needed for history.</i>
                </button>
                <div className="menu-sep" />
                <button onClick={job("reload")}>
                  Reload from disk<i>Re-read the database, no network</i>
                </button>
              </div>
            </>
          )}
        </div>

        <button className="ghost icon-btn" onClick={() => setSettingsOpen(true)} title="Settings">
          ⚙
        </button>
      </header>

      <div className="body">
        <aside className="list-pane">
          <div className="tabs">
            {/* Both counts are unfiltered totals, so a search never makes it
                look as though the other tab emptied out. */}
            <button className={`tab ${view === "scan" ? "on" : ""}`} onClick={() => setView("scan")}>
              Scanner <span className="count">{scanCount}</span>
            </button>
            <button className={`tab ${view === "all" ? "on" : ""}`} onClick={() => setView("all")}>
              All stocks <span className="count">{universe.length || (status?.universeCount ?? "")}</span>
            </button>
          </div>

          <div className="list-tools">
            <div className="search">
              <span className="icon">⌕</span>
              <input
                ref={searchRef}
                value={query}
                placeholder="Search symbol or company"
                onChange={(e) => setQuery(e.target.value)}
              />
              {!query && <kbd>/</kbd>}
            </div>
            {view === "scan" && (
              <select
                className="win"
                value={windowIdx}
                onChange={(e) => setWindowIdx(Number(e.target.value))}
                title="How far back the scanner looks"
              >
                {WINDOWS.map((w, i) => (
                  <option key={w.label} value={i}>
                    {w.label}
                  </option>
                ))}
              </select>
            )}
            <button
              className="ghost icon-btn"
              onClick={exportRows}
              title="Write these rows to a CSV in data/ — exactly what is listed, in this order"
            >
              ⭳
            </button>
          </div>

          <div className="sortbar">
            {SORTS[view].map((s) => (
              <button
                key={s.key}
                className={`sort ${effSort === s.key ? "on" : ""}`}
                onClick={() => {
                  // Compared against the *stored* key, not the displayed one.
                  // On All stocks with "Score" still stored, A–Z is highlighted
                  // by the fallback; treating a click on it as "toggle" would
                  // silently flip the scanner's direction on the other tab.
                  if (sortKey === s.key) setSortDesc((d) => !d);
                  else {
                    setSortKey(s.key);
                    setSortDesc(defaultDesc(s.key));
                  }
                }}
                title={`Sort by ${s.label}`}
              >
                {s.label}
                {effSort === s.key && <i>{effDesc ? "▾" : "▴"}</i>}
              </button>
            ))}
          </div>

          <div className="rows" ref={rowsRef} onScroll={(e) => setScrollTop(e.currentTarget.scrollTop)}>
            {rows.length === 0 ? (
              <div className="empty">
                {view === "scan"
                  ? "No breakouts in this window."
                  : universe.length === 0
                    ? "No stocks loaded yet."
                    : "Nothing matches that search."}
              </div>
            ) : (
              <div style={{ height: rows.length * ROW_H, position: "relative" }}>
                <div style={{ transform: `translateY(${first * ROW_H}px)` }}>
                  {slice.map((r) => (
                    <div
                      key={r.key}
                      className={`row ${r.key === selected ? "sel" : ""}`}
                      onClick={() => setSelected(r.key)}
                      title={r.name}
                    >
                      <div className="row-main">
                        <div className="row-sym">
                          {r.symbol}
                          <span className={`badge ${r.exchange.toLowerCase()}`}>{r.exchange}</span>
                          {view === "all" && r.hit && <span className="badge hit">Setup</span>}
                          {r.stale && <span className="badge stale">No trade</span>}
                        </div>
                        {/* Scanner rows show what the setup is made of; the
                            company name is one hover away and is spelled out in
                            full in the chart header the moment a row is picked. */}
                        <div className="row-name">
                          {view === "scan" ? (
                            <span className="stats num">
                              {fmt(r.volumeRatio ?? 0, 1)}× vol · RSI {fmt(r.rsi ?? 0, 0)}
                              {windowIdx !== 0 && r.date ? ` · ${r.date}` : ""}
                            </span>
                          ) : (
                            r.name
                          )}
                        </div>
                      </div>
                      <div className="row-right">
                        <div className="row-close num">{r.close ? fmt(r.close) : "—"}</div>
                        <div className={`row-chg num ${r.changePct > 0 ? "up" : r.changePct < 0 ? "down" : "flat"}`}>
                          {r.changePct >= 0 ? "+" : ""}
                          {fmt(r.changePct)}%
                        </div>
                      </div>
                    </div>
                  ))}
                </div>
              </div>
            )}
          </div>
        </aside>

        <div
          className="splitter"
          style={{ left: listW }}
          onMouseDown={() => {
            dragRef.current = true;
            document.body.style.cursor = "col-resize";
          }}
        />

        <ChartPane data={chart} />
      </div>

      {settingsOpen && (
        <SettingsSheet onClose={() => setSettingsOpen(false)} onSaved={note} />
      )}

      <footer className="statusbar">
        <span className={`dot ${busy ? "busy" : ""}`} />
        {/* An engine error always wins: it is the one message that means
            something is wrong. A local note beats routine chatter. */}
        <span className={`msg ${status?.error ? "err" : ""}`}>
          {status?.error ?? flash ?? status?.message ?? "Ready"}
        </span>
        {busy && status && status.progressTotal > 0 && (
          <>
            <span className="bar">
              <i style={{ width: `${(status.progressDone / status.progressTotal) * 100}%` }} />
            </span>
            <span className="num">
              {status.progressDone}/{status.progressTotal}
            </span>
          </>
        )}
        <span className="spacer" />
        <span>
          {rows.length} shown · {status?.universeCount ?? 0} in universe · {status?.hitCount ?? 0} setups
        </span>
        {status?.lastSync && <span>Synced {status.lastSync}</span>}
      </footer>
    </div>
  );
}

/** Keep the keyboard selection on screen without scrolling when it already is. */
function scrollRowIntoView(el: HTMLDivElement | null, i: number) {
  if (!el) return;
  const top = i * ROW_H;
  const bottom = top + ROW_H;
  if (top < el.scrollTop) el.scrollTop = top;
  else if (bottom > el.scrollTop + el.clientHeight) el.scrollTop = bottom - el.clientHeight;
}
