//! BridgeCraft — clean-room asset browser in Rust.
//! GUI shell is a placeholder until the Phase 0 framework spike concludes.

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    tracing::info!("BridgeCraft starting — GUI placeholder (Phase 0)");

    let root = std::env::args()
        .nth(1)
        .unwrap_or_else(|| ".".to_string());
    let entries = bc_core::scan(std::path::Path::new(&root))?;
    tracing::info!("scanned {}: {} entries", root, entries.len());

    Ok(())
}
