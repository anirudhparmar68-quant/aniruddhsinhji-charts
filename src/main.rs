// Release builds are a GUI app, not a console one: otherwise every launch also
// opens a black console window that steals focus and sits behind the chart.
// Debug builds keep the console so `cargo run` still shows logs.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! Spider Charts — a self-hosted end-of-day charting and pattern scanner for
//! NSE + BSE equities.
//!
//! Two ways to run it:
//!   spider_charts.exe            desktop app
//!   spider_charts.exe --sync     headless full sync, for a nightly scheduled task

mod bhavcopy;
mod config;
mod model;
mod patterns;
mod store;
mod sync;
mod ta;
mod ui;
mod universe;
mod upstox;

use anyhow::Result;

fn main() -> Result<()> {
    // eframe reports renderer and windowing problems through `log`. Without a
    // logger installed those messages go nowhere — which is exactly why the
    // first wgpu crash produced no output at all. Set RUST_LOG for more.
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .format_timestamp(None)
        .init();

    let headless = std::env::args().any(|a| a == "--sync" || a == "-s");
    let settings = config::Settings::load();
    // Materialise data/ and a settings file on first run so the paths are
    // discoverable before anything else happens.
    let _ = settings.save();

    if headless {
        let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
        return runtime.block_on(sync::run_headless(settings));
    }

    // Daily catch-up straight from the exchanges. No broker token involved, so
    // this is the one a scheduled job should run.
    if std::env::args().any(|a| a == "--tail" || a == "-t") {
        let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
        return runtime.block_on(sync::run_tail(settings));
    }

    // Read-only diagnostic: builds the universe from public sources only.
    if std::env::args().any(|a| a == "--check-universe") {
        let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
        return runtime.block_on(sync::run_universe_check(settings));
    }

    let options = eframe::NativeOptions {
        // OpenGL rather than eframe's default wgpu backend: wgpu dies during
        // adapter initialisation here (access violation, no Rust panic), and
        // this app draws flat 2D shapes that OpenGL handles perfectly well.
        renderer: eframe::Renderer::Glow,
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1440.0, 900.0])
            .with_min_inner_size([900.0, 600.0])
            .with_title("Spider Charts — NSE + BSE end-of-day scanner"),
        ..Default::default()
    };

    eframe::run_native(
        "spider-charts",
        options,
        Box::new(|cc| {
            // `set_visuals` only restyles the *current* theme. The preference
            // still says "follow the system", which overwrites those visuals on
            // the very next frame — so a dark call there silently renders light.
            // `set_theme` is what actually pins it.
            cc.egui_ctx.set_theme(egui::ThemePreference::Dark);
            Ok(Box::new(ui::SpiderApp::new(settings)))
        }),
    )
    .map_err(|e| anyhow::anyhow!("could not start the window: {e}"))
}
