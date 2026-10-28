//! BridgeCraft — clean-room asset browser in Rust (egui/eframe frontend).

mod app;

use anyhow::Context;

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    let root = std::env::args().nth(1);

    // Thumbnail cache lives next to the CWD; the worker thread owns it.
    let cache_path = std::env::current_dir()
        .ok()
        .map(|d| d.join(".bridgecraft-thumbs.db"));

    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_inner_size([1280.0, 800.0])
            .with_title("BridgeCraft"),
        ..Default::default()
    };

    eframe::run_native(
        "BridgeCraft",
        options,
        Box::new(|cc| {
            Box::new(app::BridgeApp::new(cc, root, cache_path))
        }),
    )
    .map_err(|e| anyhow::anyhow!("eframe exited with an error: {e}"))?;
    Ok(())
}
