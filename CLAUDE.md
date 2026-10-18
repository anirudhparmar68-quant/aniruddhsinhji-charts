# CLAUDE.md

Guidance for Claude Code when working in this repository.

## Language

- The owner writes to you in Gujarati, often mixed with Hindi or English.
- Reply in Gujarati written in **English (Latin) letters**, for example "Haa, hu aa kari sakish." Do not use Gujarati script (ગુજરાતી લિપિ) in replies.
- Code, identifiers, comments in code, commit messages, and file names stay in English.
- If the owner writes in pure English, reply in English.

## What this project is

Spider Charts: a Windows app that shows end-of-day NSE and BSE charts and lists stocks that match scan rules. It has two front-ends that share one Rust crate:

- **Desktop app (Tauri + React):** `spider-ui/`. The Rust side is `spider-ui/src-tauri/`. This is the app the owner installs.
- **Classic egui app:** `src/ui/app.rs`, launched with `Run.bat --old`.

The shared engine lives in `src/` (`patterns/`, `sync.rs`, `store.rs`, `config.rs`, and so on). Both front-ends depend on it.

This is a market-data tool for looking, not advice. Do not add buy/sell recommendations to the UI or docs.

## Commands

- Tests for the scan rules: `cargo test patterns`
- Egui app, debug: `cargo run`
- Desktop app, dev: `npm run tauri dev --prefix spider-ui`
- Desktop app, release build: `npm run tauri build --prefix spider-ui`
- Live network test (ignored by default): `cargo test -p spider_charts live_nse -- --ignored`

The first Tauri release build takes 30 to 45 minutes. Later builds are much faster. Warn before starting one.

## Scanner rules (Chart patterns)

Each rule is a Chartink-style checklist of conditions. There are currently two:

- `ChartinkCupBreakout`, labelled **"Cup n Handle"**
- `ChartinkInsideBarBreakout`, labelled **"Inside Bar Breakout"**

To add a new rule, change all of these together:

1. `src/patterns/types.rs`: add a variant to the `patterns!` catalogue, with its label.
2. `src/patterns/scans.rs`: write the function. Use `Detection::new(...)` and `with_detail(...)`.
3. `src/patterns/mod.rs`: call the function from `detect_all`.
4. Tests: update the catalogue tests in `types.rs` and `mod.rs`, and add rule tests in `scans.rs` (one that fires, one that doesn't).
5. `src/ui/app.rs`: add a collapsing section under "Patterns" in the settings screen.
6. `README.md`: add the conditions under "The chart patterns that stayed".

Rules for these changes:

- **Never rename a variant's key** (for example `ChartinkCupBreakout`). The key is stored in saved data. Changing only the human label is safe.
- Keep the conditions exactly as the owner gives them. Do not add extra filters unless asked.
- Scans run on daily bars for the cash segment, unless the owner says otherwise.

## Installed app

The installed app is a compiled snapshot. Editing source does not change it. A new rule reaches the installed app only after a rebuild, and the new `spider-ui.exe` must be copied over the installed one (back up the old exe first).

## Git

- Do not commit or push unless the owner asks.
- Follow the attribution lines the system gives for commits and PRs.

## Conventions

- Match the existing style: short doc comments, `//!` module docs, and thresholds as named fields with defaults.
- Keep changes small and focused. Do not refactor unrelated code.
- Run `cargo test patterns` after any change in `src/patterns/`.
