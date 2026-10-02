# spider-ui

The desktop front end for Spider Charts: Tauri 2 + React 19 + TypeScript, charts by
[TradingView Lightweight Charts](https://github.com/tradingview/lightweight-charts).

It holds no trading logic. The Rust engine in the repository root owns the data,
the universe and the scan; this folder is the window over it. See the main
[README](../README.md) for what the app does and how to run it.

```bash
npm install
npm run tauri dev      # development window with hot reload
npm run tauri build    # release exe + installer, under ../target/release
```

`npm run dev` on its own opens the UI in a browser with synthetic data
(`src/dev/fixtures.ts`), which is handy for working on layout without the engine.
