mod auth;
mod config;

use anyhow::Result;
use rspotify::prelude::*;

#[tokio::main]
async fn main() -> Result<()> {
    println!();
    println!("  ╔═══════════════════════════╗");
    println!("  ║         spotify--         ║");
    println!("  ╠═══════════════════════════╣");
    println!("  ║   Minimal Spotify Client  ║");
    println!("  ╚═══════════════════════════╝");

    // ── Step 1: Ensure API keys are configured ───────────────────────
    let app_config = config::AppConfig::ensure_configured()?;

    // ── Step 2: Authenticate with Spotify ────────────────────────────
    let spotify = auth::authenticate(&app_config).await?;

    // ── Verify: print the authenticated user ─────────────────────────
    let user = spotify.current_user().await?;
    println!();
    println!(
        "  Logged in as: {}",
        user.display_name.as_deref().unwrap_or("(unknown)")
    );
    println!("  Account ID:   {}", user.id);
    println!();

    Ok(())
}
