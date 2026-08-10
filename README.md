# Spider Charts

A self-hosted, end-of-day charting and pattern-recognition desktop app for NSE + BSE
equities. Single Rust binary, local SQLite, no server, no browser.

Built for **overnight/positional** work only — there is no intraday mode by design.

---

## What it does

- **Universe** — NSE cash equities plus BSE mainboard groups A and B,
  de-duplicated by ISIN (NSE preferred for dual-listed names), then filtered on
  price and traded value. Listed **alphabetically**. Currently 2,161 stocks:
  2,080 from NSE and 81 that trade only on BSE.
- **Data** — daily OHLCV in SQLite, from two sources on purpose: **Upstox** for
  building history, and the exchanges' own **NSE + BSE bhavcopy** for the recent
  tail. See below for why.
- **Patterns** — every detection runs automatically over the whole universe:
  - **75 candlestick patterns** (the full classical set: dojis, hammers,
    engulfing, harami, stars, soldiers/crows, kickers, tasuki gaps, three
    methods, breakaways, hikkake, …)
  - **Your Chartink "cup and handle breakout" scan**, ported condition for
    condition (see below). This is the only non-candlestick pattern in the app.

  > Geometric chart-structure recognisers — cup shapes, double and triple
  > tops/bottoms, head and shoulders, triangles, wedges, flags, pennants, Darvas
  > boxes, rounding bottoms, VCP, base breakouts — were built, tested, and then
  > **removed on request**. They are the family where "did it really form?" is a
  > matter of opinion. They live in git history (`git log`), so bringing any of
  > them back is a revert away.
- **Scanner** — a universe-wide table that opens on **the latest candle**,
  because that is the question this tool exists to answer. Sortable by
  conviction, score, change, volume or RSI; filterable by pattern, family,
  direction and score; exportable to CSV. Click a symbol to jump to its chart.
- **Confluence** — the differentiator. Each stock gets a signed conviction score
  for the newest session built from how many independent patterns agree, how
  strongly, and how many there are. Four patterns agreeing at 75% outranks one
  lonely 95% signal, and two patterns in opposite directions cancel. Neither
  TradingView's screener nor Spider will tell you that.
- **Chart** — candlesticks with volume, scroll-to-zoom, drag-to-pan, a snapping
  crosshair with date and price tags, pattern markers on the completing bar, and
  boxes outlining multi-week structures. Space is always kept to the right of
  the newest candle. Optional 20/50/200 moving averages, off by default.

### The one thing it does better than anything else

> *"What printed on tonight's candle, across every NSE and BSE stock worth
> trading, and how much do the patterns agree?"*

TradingView's screener does not recognise candlestick or chart patterns across a
universe. Spider recognises patterns one chart at a time. This answers it for
2,161 stocks in a few seconds, with your own Chartink scan running alongside 99
classical patterns.

---

## First run

1. Credentials are already in `.env`.
   The redirect URI must match the one registered on your Upstox app.

2. Build and launch:

```bash
cargo run --release
```

3. Press **Full sync**. That does, in order:
   - Upstox login (opens a browser once per day; the token is cached until 03:30 IST)
   - instrument master download for NSE + BSE
   - market caps from AMFI
   - daily history for every qualifying stock
   - a pattern scan over everything

The first backfill downloads roughly 3,000–3,500 symbols. Subsequent runs only
fetch the missing sessions and take well under a minute.

---

## Checking the universe without logging in

```bash
spider_charts.exe --check-universe
```

Downloads the instrument master and the AMFI workbook — both public, so no
access token is involved and nothing touches your broker account — then prints
how many stocks were found, how many market caps resolved, and how many clear
the floor. Useful for sanity-checking the filter before committing to a
full backfill.

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

Its thresholds are tunable in **Settings**. Candlestick rules use fixed
thresholds, because there is little honest disagreement about what an engulfing
bar is.

---

## Settings

All tunables live in the **Settings** tab and persist to `data/settings.json`:

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

Keyboard: `↑`/`↓` walk the stock list, `←`/`→` pan the chart, `+`/`−` zoom,
`Home`/`End` jump, `Ctrl+K` focuses search.

> The history window is 400 calendar days rather than 365 so that a full year of
> *trading* sessions is available for the longest structures (a 200-bar cup plus
> its handle needs the buffer).

---

## Layout

```
src/
  config.rs        paths, .env secrets, persisted settings
  model.rs         Candle / Instrument / Exchange + candle geometry
  ta.rs            SMA, EMA, ATR, RSI, trend context, zig-zag pivots, line fitting
  store.rs         SQLite schema and queries
  universe.rs      market-cap resolution and the inclusion rules
  sync.rs          background worker: login → universe → backfill → scan
  upstox/
    auth.rs        OAuth, local redirect catcher, token cache
    instruments.rs NSE/BSE instrument master, ETF filtering, ISIN de-duplication
    history.rs     V3 daily candles with retry/backoff
  patterns/
    types.rs       the pattern catalogue
    candlesticks.rs  75 candlestick recognisers
    breakout.rs      24 chart structures
  ui/
    app.rs         sidebar, chart tab, scanner, settings
    chart.rs       the candlestick renderer
```

Run the test suite with:

```bash
cargo test
```

The pattern engines are tested against synthetic series — a hammer must not fire
without a prior downtrend, a V-bottom must not match as a cup, and every
detection's bar indices must stay inside the series.
