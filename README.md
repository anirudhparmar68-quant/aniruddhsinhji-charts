# Spider Charts

A self-hosted, end-of-day charting and scanning desktop app for NSE + BSE
equities. Rust engine, local SQLite, no server, no account, nothing leaves the
machine.

Built for **overnight/positional** work only — there is no intraday mode by design.

Two front ends sit on one engine:

| | | |
|---|---|---|
| **`spider-ui`** | Tauri + React desktop app | the one to use |
| **`spider_charts`** | the original egui app | still builds, kept as a fallback |
| **`spider_charts --tail`** | headless nightly update | what Task Scheduler runs |

All three read the same `data/spider.db`, and only one may write at a time —
see *Two processes, one database* below.

---

## What it does

- **Universe** — driven by **your own screener export**, `data/universe_symbols.csv`.
  Drop in the result of a "Market Cap > 100 cr" screen and that is the universe;
  the exchange-tier and ISIN rules below then decide which listing of each name
  to use. Listed **alphabetically**. Currently **2,115** stocks matched out of
  the 2,161 symbols in the export — the rest are not in Upstox's instrument dump
  at all, which was verified by hand against 79,917 NSE and 26,536 BSE entries.
  Delete the file and the app falls back to the market-cap and turnover rules.
- **Data** — daily OHLCV in SQLite, from two sources on purpose: **Upstox** for
  building history, and the exchanges' own **NSE + BSE bhavcopy** for the recent
  tail. See below for why.
- **The scan** — exactly one: **your Chartink "cup and handle breakout"**, ported
  condition for condition (see below). It runs over the whole universe on every
  update.

  > Two larger families were built, tested, and then **removed on request**: 75
  > candlestick recognisers, and 24 geometric chart structures (cups, double and
  > triple tops/bottoms, head and shoulders, triangles, wedges, flags, pennants,
  > Darvas boxes, rounding bottoms, VCP, base breakouts). Both are the kind of
  > pattern where "did it really form?" is a matter of opinion. They live in git
  > history, so bringing any of them back is a revert away.
- **Scanner** — the universe-wide list of what fired, opening on **the latest
  candle**, because that is the question this tool exists to answer. It sits
  beside the chart, not in front of it, and the arrow keys walk it.
- **Chart** — candlesticks with a volume pane, scroll-to-zoom, drag-to-pan, a
  snapping crosshair with a floating OHLC readout, and a marker on the bar the
  scan fired on. Space is always kept to the right of the newest candle.

### The thing it does that a screener does not

A Chartink scan gives you a list. This gives you the list *and* the chart at the
same time, over your own 100 cr+ universe, from data you hold on disk — so
checking twenty names is twenty keypresses, not twenty page loads.

---

## Running it

The desktop app (the one to use):

```bash
npm run tauri dev --prefix spider-ui
```

To produce a standalone `.exe` and an installer:

```bash
npm run tauri build --prefix spider-ui
```

The old egui app still builds, unchanged:

```bash
cargo run --release
```

### The desktop app

Two panes, always both on screen: the list on the left, its chart on the right.

- **Scanner** tab lists what the scan fired on, one line per stock, over a
  window you choose (latest session, 3 days, a week, a month, everything).
- **All stocks** tab is the full universe, alphabetically, with a `Setup` badge
  on the ones that fired.

| Key | |
|---|---|
| `↑` `↓` `←` `→` | previous / next stock — the chart follows immediately |
| `PgUp` `PgDn` | jump ten |
| `Home` `End` | first / last |
| `/` or `Ctrl+K` | focus search (arrows keep working while you type) |
| `Esc` | clear search |

Zoom with the wheel or the `+` / `−` buttons, drag to pan, `Reset` returns to the
newest candles. There are deliberately **no drawing tools, no indicator menu and
no timeframe switcher** — this is an end-of-day breakout list, and everything
that is not price, volume or the signal was left out.

The splitter between the panes drags, and the sort strip above the list doubles
as its column legend: `A–Z` (the default, as asked for from the start), `Score`,
`Chg%`, `Vol×`, `RSI`, `Date`. Click an active one again to flip the direction.
Ties always fall back to the alphabet so a refresh never reshuffles the list
under you.

**⭳** writes exactly what is listed — same tab, same filter, same sort, same
columns — to `data/scanner_export.csv` or `data/universe_export.csv`. Numbers go
out unformatted, so the file is arithmetic rather than text that looks like it.

Everything that touches the network sits under **More**: full sync, universe
refresh, history download, Upstox login. Only **Update** is needed day to day,
and it needs no login.

### First data load

1. Credentials are already in `.env`.
   The redirect URI must match the one registered on your Upstox app.

2. Press **Full sync**. That does, in order:
   - Upstox login (opens a browser once per day; the token is cached until 03:30 IST)
   - instrument master download for NSE + BSE
   - the universe decision (your export, or market cap and turnover)
   - daily history for every qualifying stock
   - the last 10 sessions re-taken from bhavcopy, which repairs any bad bars
   - a scan over everything

The first backfill downloads roughly 3,000–3,500 symbols. Subsequent runs only
fetch the missing sessions and take well under a minute.

---

## Checking the universe without logging in

```bash
spider_charts.exe --check-universe
```

Downloads the instrument master — public, so no access token is involved and
nothing touches your broker account — then prints how many stocks were found,
how many matched the export, and how many clear the floor. Useful for
sanity-checking the filter before committing to a full backfill.

---

## Nightly automation

The same binary runs headless:

```bash
spider_charts.exe --sync
```

Point Windows Task Scheduler at that with a trigger around 19:00 IST on weekdays.
Release builds are a windowed app with no console, so a scheduled run would
otherwise be silent — every line is appended to **`data/sync.log`** as well,
which is what you want for an unattended job anyway.

It exits when the scan finishes, so the app is ready with fresh results the
moment you open it. Opening the app also re-scans what is already on disk, so
the scanner is never empty on launch.

---

## How the universe is built

**Exchange tier comes first**, because it does most of the work for free.

NSE reports an instrument type, so its cash equities are simply
`instrument_type == "EQ"`. **BSE does not** — it reports its *group code* in the
same field, which means the obvious NSE rule silently drops every BSE row. The
groups matter a great deal. Measured against a live dump of 4,835 BSE
equity-series scrips:

| BSE group | Count | Not also on NSE |
|---|---:|---:|
| A (mainboard) | 697 | **1** |
| B (mainboard) | 1,372 | **80** |
| T, X, XT, Z, M, MT, MS, P, TS, ZP | 2,766 | 2,766 |

Group A is 99.9% dual-listed and B is 94%, while the surveillance, illiquid and
SME tiers are *entirely* BSE-only — which is exactly where the untradeable
scrips live. So BSE contributes only **81 names** the NSE list does not already
have, and taking groups A+B avoids the other 2,766 without needing any
market-cap data at all. Adjust `bse_groups` in settings if you want more.

A second filter separates a company's shares from its own bonds: BSE lists both
in the same cash segment, and its debt rows (6,522 in group F, 1,125 in G)
outnumber its equities. Indian ISINs encode the security type in characters 8–9
— `01` is an equity share, `07`/`08`/`09` are debt — so that is what the app tests.

### Market cap

There is no dependable free bulk source keyed by symbol. AMFI's categorisation
page has moved and returns 404; BSE's per-scrip API carries EPS, P/E and face
value but not market cap. Rather than ship something that quietly breaks, the app
is explicit about it:

- Drop a screener export into **`data/mcap_overrides.csv`** and the ₹ floor
  applies immediately. A `symbol` column plus any market-cap column is enough —
  ISIN is optional, extra columns are ignored, `#` lines are comments. Running
  a "Market Cap > 100" screen and pasting the result is the whole job.
- Set `mcap_source_url` in settings if you find a page hosting a bulk workbook;
  the app scrapes it for the first market-cap `.xls`/`.xlsx` link so a versioned
  filename keeps working.

Anything unresolved is **not silently dropped**. It is kept with an unknown
market cap and judged on **median daily traded value** (default ₹0.25 crore/day)
plus a ₹5 minimum price, and the top bar always shows how many names took that
path.

Run `--check-universe` at any time to see exactly where the numbers stand.

---

## The one chart pattern that stayed

**`Cup & Handle Breakout (Chartink)`** — a faithful port of the screener:

```text
Close crossed above Max(30, 1 day ago Close)     fresh 30-day closing-high breakout
Min(20, Close)  <  Max(60, Close) × 0.85         at least a 15% dip in the recent past
Close           >  Sma(Close, 50)
Sma(Volume, 5)  >  Sma(Volume, 50)
Rsi(14)         >  55
Adx(14)         >  20
Volume          >  1.5 × Sma(Volume, 20)
```

Worth being clear about: it does **not** test cup shape at all — no roundness, no
handle, no rim symmetry. It is a momentum base-breakout checklist. That is a
feature, not a flaw, and it is precisely why this one survived: it sidesteps the
subjectivity of cup geometry by using conditions nobody can argue about.

Every threshold in that block is tunable — they live under `patterns` in
`data/settings.json`, so tightening the scan does not need a rebuild.

One detail worth keeping: *crossed above* is implemented as "above today **and
not** above yesterday", not merely "is above". A stock that broke out a week ago
and has held the level is not a fresh breakout, and treating it as one would fill
the list with names whose move already happened.

---

## Settings

Tunables persist to `data/settings.json`. The egui app has a Settings tab for
them; the React app does not yet, so change them there or edit the file:

| Setting | Default | Notes |
|---|---|---|
| Minimum market cap | ₹100 cr | Applied after a universe refresh |
| Keep unresolved market caps | on | Falls back to the turnover test |
| Minimum median turnover | ₹0.25 cr/day | Only used for unresolved names |
| Minimum price | ₹5 | Penny-stock guard |
| History to keep | 400 days | ~1 trading year plus buffer for pattern warm-up |
| Parallel downloads | 8 | How many requests may be in flight |
| Requests per second | 4 | **Global** rate cap — see below |
| Minimum pattern score | 0.5 | Raise to cut noise in the scanner |

In the desktop app they live behind the **⚙** button, and the scan block is laid
out as the Chartink scan itself — the numbers are editable in place inside the
conditions they belong to, because a field called "depth ratio" means nothing on
its own while `Min(20, Close) < Max(60, Close) × 0.85` explains itself.

Saving always persists and hands the worker its new copy. It offers **Save and
rescan** only when something that changes detection actually changed; a download
setting does nothing until the next sync, and rescanning for it would be thirty
wasted seconds.

### About the old Today tab

The egui app has a third tab that rolls the newest candle up per stock, with
bull/bear counts and a conviction score. With exactly one pattern in the
catalogue — and that pattern always bullish — every one of those columns is a
constant: bull is always 1, bear always 0, conviction always a fixed multiple of
the score. It was not ported as a separate tab because it would have been the
Scanner tab with three columns that never vary. **Scanner + "Latest" is the
Today tab**, and it carries the price context the roll-up existed to show.

If a second pattern family is ever added back, the roll-up earns its place again
and the conviction code in `sync.rs` is still there, untouched.

### Why the newest session comes from bhavcopy, not Upstox

Upstox is fine for bulk history and useless for the tail. Measured on RELIANCE,
within the same minute, against the same endpoint:

| window requested | newest bar returned |
|---|---|
| 5 days | 07 Aug ❌ |
| 10 days | 10 Aug ✅ |
| 30 days | 10 Aug ✅ |
| 400 days | 07 Aug ❌ |

It is not deterministic and not per-instrument — a 45-day window returned the
newest bar for INFY and a stale one for four other stocks in the same sweep. The
practical effect was ugly: a sync would report *"2161 downloaded, 0 failed"* while
1,368 stocks silently sat a session behind, and every "latest candle" view then
quietly excluded them.

So the newest sessions are taken from the exchanges instead. A bhavcopy is one
file per exchange per session covering every scrip that traded — nothing to
cache wrong, no rate limit, no per-symbol requests — and both NSE and BSE publish
the same UDiFF layout carrying **ISIN**, which is exactly what this app keys
instruments on, so matching is exact rather than by name.

Every backfill therefore ends by re-taking the last 10 calendar days from
bhavcopy and upserting them, which also repairs any bad bars written earlier. A
row whose `TradDt` does not match the session being requested is rejected, so a
mislabelled or cached file cannot be written under the wrong date.

What this does **not** fix: running a sync before the exchange has published.
Run it after **19:00 IST**. If anything is still behind, the toolbar says
`⚠ N stocks a session behind` rather than pretending everything is current.

### Why there is a rate cap as well as a concurrency limit

Upstox enforces per-second, per-minute **and per-30-minute** quotas. Bounded
concurrency only respects the first of those: 2,161 requests will blow through a
per-30-minute quota no matter how few run at a time. The first build learned this
the hard way — it downloaded 505 stocks, got throttled, and then failed the
remaining 1,656 in seconds because its retry budget was about five seconds
against a thirty-minute window. Worse, it still reported "Sync complete".

Now every request passes a global pacer, retries six times with backoff out to
two minutes, honours `Retry-After`, treats Cloudflare's 403 as throttling rather
than as failure, and slows the *whole fleet* when the server pushes back. And a
sync that leaves anything behind says so, with the count and what to do about it.

### Two processes, one database

The nightly job fires at 19:30 whether or not a window is open, so the app and
the scheduled task running together is the normal case, not a rare race. When
they first did, both stalled: each held part of what the other needed and both
sat at zero CPU until one was killed.

SQLite's own busy handling does not solve this. It serialises *statements*, while
a backfill or a scan is thousands of transactions that together mean "I am
rewriting the database". `data/writer.lock` expresses that larger unit: whoever
holds it is the writer, a lock left by a crash is reclaimed after 30 minutes, and
a process that cannot get it says so rather than forcing its way in.

Reading is never blocked, so opening the app during a sync is always safe.

> The history window is 400 calendar days rather than 365 so that a full year of
> *trading* sessions is available for the longest structures (a 200-bar cup plus
> its handle needs the buffer).

---

## Layout

```
src/                       the engine — a library, so both front ends share it
  lib.rs                   what the front ends may use
  config.rs                paths, .env secrets, persisted settings
  model.rs                 Candle / Instrument / Exchange + candle geometry
  ta.rs                    SMA, EMA, ATR, RSI and friends
  store.rs                 SQLite schema and queries
  universe.rs              the allowlist and the inclusion rules
  bhavcopy.rs              NSE + BSE daily files, the authoritative tail
  writelock.rs             one writer at a time, across processes
  sync.rs                  background worker: login → universe → backfill → scan
  upstox/
    auth.rs                OAuth, local redirect catcher, token cache
    instruments.rs         instrument master, ETF filtering, ISIN de-duplication
    history.rs             V3 daily candles with retry/backoff
  patterns/
    types.rs               the pattern catalogue
    scans.rs               the Chartink cup-breakout port
  main.rs, ui/             the egui app — the only part not in the library

spider-ui/                 the Tauri + React desktop app
  src/
    api.ts                 typed wrapper over the Rust commands
    App.tsx                split view, list, keyboard navigation
    ChartPane.tsx          the chart, on lightweight-charts
    styles.css             the theme
    dev/fixtures.ts        synthetic data for `npm run dev` in a browser
  src-tauri/src/lib.rs     the bridge: engine types → JSON, commands → worker
```

The bridge holds no rules of its own. Anything about what a pattern *is*, or
which stock belongs in the universe, stays in the engine where it is
unit-tested and shared with the headless job.

Run the test suite with:

```bash
cargo test
```

The pattern engines are tested against synthetic series — a hammer must not fire
without a prior downtrend, a V-bottom must not match as a cup, and every
detection's bar indices must stay inside the series.
